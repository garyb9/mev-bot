//! Binance and Bybit reference-venue sources (SPEC-0008 §9, task R-8).
//!
//! Records the public top-of-book streams of three reference venues as raw
//! frames, so studies O5 (cross-venue lead-lag) and O1/O3 (fair-value
//! references) can use them:
//!
//! | `src` | Stream | Subscribe |
//! |---|---|---|
//! | `binance-usdm` | Binance USDⓈ-M futures `bookTicker` | streams encoded in the URL |
//! | `binance-spot` | Binance spot `bookTicker` | streams encoded in the URL |
//! | `bybit-linear` | Bybit v5 linear `orderbook.1` | `{"op":"subscribe","args":[…]}` |
//!
//! Endpoint URLs, payload fields, and keepalive rules are the V-2-verified
//! facts in SPEC-0008 §9/§15 (2026-09-28): symbols are lowercased for the
//! Binance URLs and uppercased for Bybit; Bybit needs an app-level
//! `{"op":"ping"}` every 20 s; Binance only relies on tungstenite's automatic
//! pong replies to the server's ping frames.
//!
//! Everything here is public and unauthenticated: no keys, no auth, no env
//! secrets. The recorder never places orders (§3).
//!
//! Reconnect, jittered backoff, the silence watchdog, and resubscribe come from
//! [`hl_arb_client::raw_ws::RawWsConn`]; the protocols here only differ in the
//! URL, the subscribe message, and the keepalive.

use std::sync::Arc;
use std::time::{Duration, Instant};

use hl_arb_client::raw_ws::ReconnectPolicy;
use hl_arb_client::{Protocol, RawEvent, RawWsConn, raw_ws};
use serde_json::{Value, json};
use tokio::sync::Notify;
use tracing::{debug, warn};

use crate::envelope::{Envelope, EnvelopeClock};
use crate::sources::hl_rest::EnvelopeSink;

/// Binance USDⓈ-M futures WebSocket base (SPEC-0008 §9, V-2).
pub const BINANCE_USDM_BASE: &str = "wss://fstream.binance.com";
/// Binance spot WebSocket base (SPEC-0008 §9, V-2).
pub const BINANCE_SPOT_BASE: &str = "wss://stream.binance.com:9443";
/// Bybit v5 linear WebSocket base (SPEC-0008 §9, V-2).
pub const BYBIT_LINEAR_BASE: &str = "wss://stream.bybit.com";

/// Bybit keepalive cadence (SPEC-0008 §9, V-2).
pub const BYBIT_PING_INTERVAL: Duration = Duration::from_secs(20);
/// Default silence window for the CEX watchdog.
pub const CEX_WATCHDOG: Duration = raw_ws::DEFAULT_WATCHDOG;
/// Reconnect policy for the Binance sources (BNR-1, fix A).
///
/// Binance connections over our long lossy path usually live 10 to 40 s, so
/// with the default policy (healthy only after 60 s) the attempt counter never
/// reset and backoff climbed to 30 s: one measured hour lost 37% of its time
/// in 98 gaps with a 12.4 s median. A 5 s healthy threshold makes a normal
/// 10 to 40 s connection reset the counter, so retries draw from the short end
/// of the schedule and gaps stay near a second. The 10 s cap only matters for
/// a persistent failure (maintenance, a 429/418 at the handshake): it holds
/// the worst case near 50 dials per 5 minutes per source, well under Binance's
/// documented 300 connection attempts per 5 minutes per IP for spot. Bybit
/// keeps the default policy.
pub const BINANCE_RECONNECT: ReconnectPolicy = ReconnectPolicy {
    base: Duration::from_millis(250),
    max: Duration::from_secs(10),
    healthy_after: Duration::from_secs(5),
};

/// The reconnect policy for a venue. The match is exhaustive on purpose: a new
/// [`CexKind`] must pick a policy instead of silently inheriting Binance's.
pub fn reconnect_policy_for(kind: CexKind) -> ReconnectPolicy {
    match kind {
        CexKind::BinanceUsdm | CexKind::BinanceSpot => BINANCE_RECONNECT,
        CexKind::BybitLinear => ReconnectPolicy::default(),
    }
}

/// Delay before retrying a failed initial CEX dial.
pub const CEX_CONNECT_RETRY: Duration = Duration::from_secs(3);

/// A single subscription token handed to [`RawWsConn`] for Bybit. The protocol
/// ignores the token and sends one combined subscribe message, but the token
/// makes `RawWsConn` send (and resend on reconnect) exactly once.
const SUBSCRIBE_ONCE: &str = "bybit-subscribe";

/// Which reference venue a [`CexSource`] records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CexKind {
    /// Binance USDⓈ-M futures `bookTicker`.
    BinanceUsdm,
    /// Binance spot `bookTicker`.
    BinanceSpot,
    /// Bybit v5 linear `orderbook.1`.
    BybitLinear,
}

impl CexKind {
    /// The envelope `src`/`conn` id (SPEC-0008 §5.4).
    pub fn src(self) -> &'static str {
        match self {
            CexKind::BinanceUsdm => "binance-usdm",
            CexKind::BinanceSpot => "binance-spot",
            CexKind::BybitLinear => "bybit-linear",
        }
    }

    /// The metrics label for [`RawWsConn`].
    pub fn name(self) -> &'static str {
        self.src()
    }

    /// The default public WebSocket base URL (SPEC-0008 §9, V-2).
    pub fn base_url(self) -> &'static str {
        match self {
            CexKind::BinanceUsdm => BINANCE_USDM_BASE,
            CexKind::BinanceSpot => BINANCE_SPOT_BASE,
            CexKind::BybitLinear => BYBIT_LINEAR_BASE,
        }
    }
}

/// The Binance combined-stream protocol (`usdm` or `spot`).
///
/// Binance encodes the subscription in the dial URL:
/// `{base}/stream?streams=btcusdt@bookTicker/ethusdt@bookTicker/…` (lowercased
/// symbols, SPEC-0008 §9). There is no per-symbol subscribe frame to send, so
/// [`CexSource`] passes an empty subscription list and re-dials with the same
/// streams on reconnect.
#[derive(Debug, Clone)]
pub struct BinanceProtocol {
    base_url: String,
    spot: bool,
    streams: Vec<String>,
}

impl BinanceProtocol {
    /// Build a protocol from the venue kind and its configured symbols.
    ///
    /// `kind` selects the USDⓈ-M vs spot endpoint; a non-Binance kind falls
    /// back to the USDⓈ-M endpoint.
    pub fn new(kind: CexKind, symbols: &[String]) -> Self {
        Self::with_base_url(kind.base_url(), kind == CexKind::BinanceSpot, symbols)
    }

    /// Build a protocol against an explicit base URL (tests point this at a
    /// local mock). `spot` picks the spot endpoint semantics.
    pub fn with_base_url(base_url: impl Into<String>, spot: bool, symbols: &[String]) -> Self {
        let streams = symbols
            .iter()
            .map(|symbol| format!("{}@bookTicker", symbol.to_ascii_lowercase()))
            .collect();
        Self {
            base_url: base_url.into(),
            spot,
            streams,
        }
    }

    /// The combined-stream entries (`btcusdt@bookTicker`, …).
    pub fn streams(&self) -> &[String] {
        &self.streams
    }
}

impl Protocol for BinanceProtocol {
    fn name(&self) -> &'static str {
        if self.spot {
            CexKind::BinanceSpot.name()
        } else {
            CexKind::BinanceUsdm.name()
        }
    }

    fn url(&self) -> String {
        format!(
            "{}/stream?streams={}",
            self.base_url.trim_end_matches('/'),
            self.streams.join("/")
        )
    }

    fn subscribe_frame(&self, _sub: &str) -> String {
        // Not used: Binance subscriptions live in the URL. Kept as a
        // well-formed combined SUBSCRIBE fallback rather than an empty frame.
        let params: Vec<String> = self
            .streams
            .iter()
            .map(|stream| format!("\"{stream}\""))
            .collect();
        format!(
            r#"{{"method":"SUBSCRIBE","params":[{}],"id":1}}"#,
            params.join(",")
        )
    }
}

/// The Bybit v5 linear protocol (`orderbook.1`).
///
/// Bybit subscribes with an application message
/// `{"op":"subscribe","args":["orderbook.1.BTCUSDT",…]}` (uppercased symbols)
/// and must send `{"op":"ping"}` every 20 s (SPEC-0008 §9, V-2).
#[derive(Debug, Clone)]
pub struct BybitProtocol {
    base_url: String,
    args: Vec<String>,
}

impl BybitProtocol {
    /// Build a protocol from the configured symbols.
    pub fn new(symbols: &[String]) -> Self {
        Self::with_base_url(BYBIT_LINEAR_BASE, symbols)
    }

    /// Build a protocol against an explicit base URL (tests point this at a
    /// local mock).
    pub fn with_base_url(base_url: impl Into<String>, symbols: &[String]) -> Self {
        let args = symbols
            .iter()
            .map(|symbol| format!("orderbook.1.{}", symbol.to_ascii_uppercase()))
            .collect();
        Self {
            base_url: base_url.into(),
            args,
        }
    }

    /// The Bybit topic args (`orderbook.1.BTCUSDT`, …).
    pub fn args(&self) -> &[String] {
        &self.args
    }
}

impl Protocol for BybitProtocol {
    fn name(&self) -> &'static str {
        CexKind::BybitLinear.name()
    }

    fn url(&self) -> String {
        format!("{}/v5/public/linear", self.base_url.trim_end_matches('/'))
    }

    fn subscribe_frame(&self, _sub: &str) -> String {
        let args: Vec<String> = self.args.iter().map(|arg| format!("\"{arg}\"")).collect();
        format!(r#"{{"op":"subscribe","args":[{}]}}"#, args.join(","))
    }

    fn keepalive_frame(&self) -> Option<String> {
        Some(r#"{"op":"ping"}"#.to_string())
    }
}

/// Configuration for one [`CexSource`] (SPEC-0008 §9).
#[derive(Debug, Clone)]
pub struct CexConfig {
    /// Which reference venue.
    pub kind: CexKind,
    /// Configured symbols (uppercase, e.g. `BTCUSDT`).
    pub symbols: Vec<String>,
    /// WebSocket base URL; defaults to the venue's public endpoint.
    pub base_url: String,
    /// Silence watchdog window.
    pub watchdog: Duration,
    /// Application keepalive interval (only Bybit uses it).
    pub ping_interval: Duration,
}

impl CexConfig {
    /// Build a config for `kind` with the default public base URL.
    pub fn new(kind: CexKind, symbols: Vec<String>) -> Self {
        let ping_interval = match kind {
            CexKind::BybitLinear => BYBIT_PING_INTERVAL,
            _ => raw_ws::DEFAULT_PING_INTERVAL,
        };
        Self {
            kind,
            symbols,
            base_url: kind.base_url().to_string(),
            watchdog: CEX_WATCHDOG,
            ping_interval,
        }
    }
}

/// Records one reference venue over a reconnecting [`RawWsConn`].
///
/// Writes `conn_open`/`sub`/`frame`/`frame_bin`/`gap_start`/`gap_end` envelopes
/// under `src = kind.src()` and `conn = kind.src()`, mirroring the Hyperliquid
/// path in `hl-arb-bot/src/record.rs`. Reconnects and keepalives are handled by
/// [`RawWsConn`], so a server close produces a `gap_start`, a jittered backoff,
/// and a `gap_end` after the resubscribe.
pub struct CexSource {
    config: CexConfig,
    sink: Arc<dyn EnvelopeSink>,
    clock: Arc<dyn EnvelopeClock>,
    subscriptions: Vec<String>,
    sub_meta: Value,
    seq: u64,
    src: &'static str,
    conn: &'static str,
    /// Envelopes dropped since the last rate-limited warning.
    dropped_since_warn: u64,
    /// When the dropped-envelope warning was last emitted.
    last_drop_warn: Instant,
}

impl CexSource {
    /// Build a source that writes envelopes to `sink` and stamps them with
    /// `clock`.
    pub fn new(
        config: CexConfig,
        sink: Arc<dyn EnvelopeSink>,
        clock: Arc<dyn EnvelopeClock>,
    ) -> Self {
        let (subscriptions, sub_meta) = match config.kind {
            CexKind::BybitLinear => {
                let protocol = BybitProtocol::with_base_url(&config.base_url, &config.symbols);
                let args: Vec<Value> = protocol
                    .args()
                    .iter()
                    .map(|arg| Value::String(arg.clone()))
                    .collect();
                (
                    vec![SUBSCRIBE_ONCE.to_string()],
                    json!({ "op": "subscribe", "args": args }),
                )
            }
            kind => {
                let protocol = BinanceProtocol::with_base_url(
                    &config.base_url,
                    kind == CexKind::BinanceSpot,
                    &config.symbols,
                );
                let streams: Vec<Value> = protocol
                    .streams()
                    .iter()
                    .map(|stream| Value::String(stream.clone()))
                    .collect();
                (Vec::new(), json!({ "streams": streams }))
            }
        };
        let src = config.kind.src();
        Self {
            config,
            sink,
            clock,
            subscriptions,
            sub_meta,
            seq: 0,
            src,
            conn: src,
            dropped_since_warn: 0,
            last_drop_warn: Instant::now(),
        }
    }

    /// The `src`/`conn` id this source records.
    pub fn src(&self) -> &'static str {
        self.src
    }

    /// A fresh protocol for the next dial (a [`RawWsConn`] owns the one it is
    /// given).
    fn protocol(&self) -> Box<dyn Protocol> {
        match self.config.kind {
            CexKind::BybitLinear => Box::new(BybitProtocol::with_base_url(
                &self.config.base_url,
                &self.config.symbols,
            )),
            kind => Box::new(BinanceProtocol::with_base_url(
                &self.config.base_url,
                kind == CexKind::BinanceSpot,
                &self.config.symbols,
            )),
        }
    }

    /// Binance sources reconnect fast ([`BINANCE_RECONNECT`]); Bybit keeps the
    /// default policy.
    fn reconnect_policy(&self) -> ReconnectPolicy {
        reconnect_policy_for(self.config.kind)
    }

    /// The URL the next dial uses (also recorded in `conn_open.meta.url`).
    pub fn url(&self) -> String {
        self.protocol().url()
    }

    /// Run until `shutdown` is notified.
    ///
    /// One connection is dialed; after that [`RawWsConn`] transparently
    /// reconnects on gaps. A failed initial dial is retried every
    /// [`CEX_CONNECT_RETRY`], emitting a single `gap_start` until the first
    /// successful open (a matching `gap_end` closes it).
    pub async fn run(mut self, shutdown: Arc<Notify>) {
        let url = self.url();
        // Wall-clock ns (envelope `t_ns`) at the start of the current outage.
        // Set when the drop is detected (initial-dial failure or the immediate
        // `RawEvent::Gap`, which now arrives before the reconnect), cleared by
        // `gap_end`. `gap_ms` is `gap_end.t_ns - gap_start.t_ns`, so it measures
        // the real outage.
        let mut gap_started: Option<i64> = None;

        let mut raw = loop {
            match RawWsConn::connect_with_policy(
                self.protocol(),
                self.subscriptions.clone(),
                self.config.watchdog,
                self.config.ping_interval,
                self.reconnect_policy(),
            )
            .await
            {
                Ok(raw) => break raw,
                Err(err) => {
                    warn!(
                        src = self.src,
                        error = %err,
                        "cex websocket connect failed; retrying"
                    );
                    if gap_started.is_none() {
                        let t_ns = self.clock.t_ns();
                        self.emit_gap_start(t_ns, raw_ws::mono_ns(), "error", &err.to_string());
                        gap_started = Some(t_ns);
                    }
                    tokio::select! {
                        _ = tokio::time::sleep(CEX_CONNECT_RETRY) => {}
                        _ = shutdown.notified() => {
                            debug!(src = self.src, "cex source stopped during initial dial");
                            return;
                        }
                    }
                }
            }
        };

        self.emit_conn_open(&url, 1);
        self.emit_subs();
        if let Some(started) = gap_started.take() {
            self.emit_gap_end(started);
        }

        loop {
            tokio::select! {
                _ = shutdown.notified() => {
                    let t_ns = self.clock.t_ns();
                    self.emit_gap_start(t_ns, raw_ws::mono_ns(), "shutdown", "shutdown requested");
                    break;
                }
                event = raw.next() => match event {
                    Ok(RawEvent::Text { text, .. }) => {
                        self.emit(Envelope::frame(&*self.clock, self.src, self.conn, self.seq, text));
                        self.seq += 1;
                    }
                    Ok(RawEvent::Binary { bytes, .. }) => {
                        let encoded = base64_encode(&bytes);
                        self.emit(Envelope::frame_bin(
                            &*self.clock,
                            self.src,
                            self.conn,
                            self.seq,
                            encoded,
                        ));
                        self.seq += 1;
                    }
                    Ok(RawEvent::Opened { attempt }) => {
                        self.emit_conn_open(&url, attempt);
                        self.emit_subs();
                        if let Some(started) = gap_started.take() {
                            self.emit_gap_end(started);
                        }
                    }
                    Ok(RawEvent::Gap { reason, detail, disconnect_ns, t_ns }) => {
                        // `t_ns`/`disconnect_ns` are the disconnect instant: the
                        // next `next()` performs the reconnect.
                        self.emit_gap_start(t_ns, disconnect_ns, &reason, &detail);
                        gap_started = Some(t_ns);
                    }
                    Err(err) => {
                        // `RawWsConn` only returns `Err` on shutdown; a broken
                        // connection is retried internally.
                        warn!(src = self.src, error = %err, "cex source stopped");
                        let t_ns = self.clock.t_ns();
                        self.emit_gap_start(t_ns, raw_ws::mono_ns(), "error", &err.to_string());
                        break;
                    }
                },
            }
        }
        debug!(src = self.src, "cex source stopped");
    }

    /// Send one envelope through the sink, rate-limiting dropped-envelope
    /// warnings to at most one per 10 s per stream with a suppressed count.
    fn emit(&mut self, env: Envelope) {
        if self.sink.send(env) {
            return;
        }
        self.dropped_since_warn += 1;
        if self.last_drop_warn.elapsed() >= Duration::from_secs(10) {
            warn!(
                src = self.src,
                conn = self.conn,
                dropped = self.dropped_since_warn,
                "cex envelopes dropped (suppressing further warnings for 10 s)"
            );
            self.dropped_since_warn = 0;
            self.last_drop_warn = Instant::now();
        }
    }

    fn emit_conn_open(&mut self, url: &str, attempt: u32) {
        let env = Envelope::conn_open(&*self.clock, self.src, self.conn, self.seq, url, attempt);
        self.emit(env);
        self.seq += 1;
    }

    fn emit_subs(&mut self) {
        let env = Envelope::sub(
            &*self.clock,
            self.src,
            self.conn,
            self.seq,
            self.sub_meta.clone(),
        );
        self.emit(env);
        self.seq += 1;
    }

    fn emit_gap_start(&mut self, t_ns: i64, mono_ns: u64, reason: &str, detail: &str) {
        let env =
            Envelope::gap_start_at(self.src, self.conn, self.seq, t_ns, mono_ns, reason, detail);
        self.emit(env);
        self.seq += 1;
    }

    fn emit_gap_end(&mut self, started_t_ns: i64) {
        let t_ns = self.clock.t_ns();
        let gap_ms = t_ns.saturating_sub(started_t_ns).max(0) as u64 / 1_000_000;
        let env = Envelope::gap_end_at(
            self.src,
            self.conn,
            self.seq,
            t_ns,
            raw_ws::mono_ns(),
            gap_ms,
        );
        self.emit(env);
        self.seq += 1;
    }
}

/// Encode bytes with the standard base64 alphabet (SPEC-0008 §5.1).
fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((triple >> 18) & 63) as usize] as char);
        out.push(TABLE[((triple >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((triple >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(triple & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use futures_util::{SinkExt, StreamExt};
    use tokio::net::TcpListener;
    use tokio::sync::mpsc;
    use tokio_tungstenite::accept_async;
    use tokio_tungstenite::tungstenite::Message;

    use super::*;
    use crate::envelope::{Kind, SystemEnvelopeClock};

    /// A sink that forwards envelopes to a channel so tests can await them.
    struct ChannelSink {
        tx: Mutex<mpsc::UnboundedSender<Envelope>>,
    }

    impl EnvelopeSink for ChannelSink {
        fn send(&self, env: Envelope) -> bool {
            match self.tx.lock() {
                Ok(tx) => tx.send(env).is_ok(),
                Err(_) => false,
            }
        }
    }

    #[test]
    fn binance_sources_reconnect_fast_and_bybit_keeps_the_default() {
        assert_eq!(
            reconnect_policy_for(CexKind::BinanceUsdm),
            BINANCE_RECONNECT
        );
        assert_eq!(
            reconnect_policy_for(CexKind::BinanceSpot),
            BINANCE_RECONNECT
        );
        assert_eq!(
            reconnect_policy_for(CexKind::BybitLinear),
            ReconnectPolicy::default()
        );
        // Guard the rate: the worst case stays far below Binance's documented
        // 300 connection attempts per 5 minutes per IP.
        assert!(BINANCE_RECONNECT.max >= Duration::from_secs(5));
    }

    fn channel_sink() -> (Arc<dyn EnvelopeSink>, mpsc::UnboundedReceiver<Envelope>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Arc::new(ChannelSink { tx: Mutex::new(tx) }), rx)
    }

    fn config(kind: CexKind, base_url: String, symbols: &[&str]) -> CexConfig {
        CexConfig {
            kind,
            symbols: symbols.iter().map(|s| s.to_string()).collect(),
            base_url,
            watchdog: Duration::from_secs(3600),
            ping_interval: match kind {
                CexKind::BybitLinear => BYBIT_PING_INTERVAL,
                _ => raw_ws::DEFAULT_PING_INTERVAL,
            },
        }
    }

    fn source(config: CexConfig) -> (CexSource, mpsc::UnboundedReceiver<Envelope>) {
        let (sink, rx) = channel_sink();
        let clock: Arc<dyn EnvelopeClock> = Arc::new(SystemEnvelopeClock::new());
        (CexSource::new(config, sink, clock), rx)
    }

    // -- Pure protocol facts (no network) --------------------------------

    #[test]
    fn binance_usdm_url_encodes_lowercased_streams() {
        let protocol = BinanceProtocol::new(
            CexKind::BinanceUsdm,
            &["BTCUSDT".to_string(), "ETHUSDT".to_string()],
        );
        assert_eq!(
            protocol.url(),
            "wss://fstream.binance.com/stream?streams=btcusdt@bookTicker/ethusdt@bookTicker"
        );
        assert_eq!(protocol.name(), "binance-usdm");
        assert_eq!(
            protocol.streams(),
            &[
                "btcusdt@bookTicker".to_string(),
                "ethusdt@bookTicker".to_string()
            ]
        );
    }

    #[test]
    fn binance_spot_uses_the_spot_endpoint() {
        let protocol = BinanceProtocol::new(CexKind::BinanceSpot, &["SOLUSDT".to_string()]);
        assert_eq!(
            protocol.url(),
            "wss://stream.binance.com:9443/stream?streams=solusdt@bookTicker"
        );
        assert_eq!(protocol.name(), "binance-spot");
    }

    #[test]
    fn bybit_url_and_subscribe_frame_match_the_spec() {
        let protocol = BybitProtocol::new(&["BTCUSDT".to_string(), "ETHUSDT".to_string()]);
        assert_eq!(protocol.url(), "wss://stream.bybit.com/v5/public/linear");
        assert_eq!(
            protocol.subscribe_frame(SUBSCRIBE_ONCE),
            r#"{"op":"subscribe","args":["orderbook.1.BTCUSDT","orderbook.1.ETHUSDT"]}"#
        );
        assert_eq!(
            protocol.keepalive_frame().as_deref(),
            Some(r#"{"op":"ping"}"#)
        );
        assert_eq!(protocol.name(), "bybit-linear");
    }

    #[test]
    fn base_urls_are_exactly_the_spec_endpoints() {
        assert_eq!(CexKind::BinanceUsdm.base_url(), "wss://fstream.binance.com");
        assert_eq!(
            CexKind::BinanceSpot.base_url(),
            "wss://stream.binance.com:9443"
        );
        assert_eq!(CexKind::BybitLinear.base_url(), "wss://stream.bybit.com");
    }

    #[test]
    fn bybit_defaults_to_a_20s_ping() {
        let config = CexConfig::new(CexKind::BybitLinear, vec!["BTCUSDT".to_string()]);
        assert_eq!(config.ping_interval, Duration::from_secs(20));
    }

    // -- Mock-server behaviour -------------------------------------------

    /// A mock WS server: accepts `rounds` connections; each round optionally
    /// reads one subscribe frame, reports it, sends one text frame, then closes
    /// every round but the last (which is held open).
    struct MockServer {
        addr: std::net::SocketAddr,
        subscriptions: mpsc::UnboundedReceiver<String>,
        connections: Arc<std::sync::atomic::AtomicUsize>,
    }

    async fn spawn_mock(
        expect_subscribe: bool,
        frame_text: &'static str,
        rounds: usize,
    ) -> MockServer {
        let connections = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (subs_tx, subs_rx) = mpsc::unbounded_channel();
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
        let addr = listener.local_addr().expect("addr");
        let connections_task = connections.clone();
        tokio::spawn(async move {
            for round in 0..rounds {
                let (stream, _) = listener.accept().await.expect("accept");
                connections_task.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut ws = accept_async(stream).await.expect("handshake");
                if expect_subscribe && let Some(Ok(Message::Text(text))) = ws.next().await {
                    let _ = subs_tx.send(text.to_string());
                }
                if ws.send(Message::Text(frame_text.into())).await.is_err() {
                    break;
                }
                if round + 1 == rounds {
                    // Hold the final connection open until the test ends.
                    while let Some(Ok(_)) = ws.next().await {}
                } else {
                    ws.close(None).await.ok();
                    drop(ws);
                }
            }
        });
        MockServer {
            addr,
            subscriptions: subs_rx,
            connections,
        }
    }

    async fn recv_until(rx: &mut mpsc::UnboundedReceiver<Envelope>, kind: Kind) -> Envelope {
        loop {
            let env = rx.recv().await.expect("envelope before channel close");
            if env.kind == kind {
                return env;
            }
        }
    }

    async fn wait_for_connections(server: &MockServer, n: usize) {
        for _ in 0..2_000 {
            if server.connections.load(std::sync::atomic::Ordering::SeqCst) >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("mock server did not see {n} connections");
    }

    /// A freshly connected source writes `conn_open` first, then `sub`.
    #[tokio::test]
    async fn first_envelopes_are_conn_open_then_sub() {
        let server = spawn_mock(false, r#"{"s":"BTCUSDT"}"#, 1).await;
        let (source, mut rx) = source(config(
            CexKind::BinanceSpot,
            format!("ws://{}", server.addr),
            &["BTCUSDT"],
        ));
        let shutdown = Arc::new(Notify::new());
        let handle = tokio::spawn(source.run(shutdown.clone()));

        let open = rx.recv().await.expect("conn_open");
        assert_eq!(open.kind, Kind::ConnOpen);
        assert_eq!(open.meta.unwrap()["attempt"], 1);
        let sub = rx.recv().await.expect("sub");
        assert_eq!(sub.kind, Kind::Sub);

        shutdown.notify_one();
        handle.await.unwrap();
    }

    /// Binance USDⓈ-M: frames are recorded unmodified with the right `src`.
    #[tokio::test]
    async fn binance_usdm_records_frames_with_the_right_src() {
        let server = spawn_mock(false, r#"{"e":"bookTicker","s":"BTCUSDT"}"#, 1).await;
        let (source, mut rx) = source(config(
            CexKind::BinanceUsdm,
            format!("ws://{}", server.addr),
            &["BTCUSDT", "ETHUSDT"],
        ));
        let shutdown = Arc::new(Notify::new());
        let handle = tokio::spawn(source.run(shutdown.clone()));

        let first = recv_until(&mut rx, Kind::Frame).await;
        assert_eq!(first.src, "binance-usdm");
        assert_eq!(first.conn, "binance-usdm");
        assert_eq!(
            first.raw.as_deref(),
            Some(r#"{"e":"bookTicker","s":"BTCUSDT"}"#)
        );

        shutdown.notify_one();
        handle.await.unwrap();
    }

    /// Binance spot: frames are recorded under its own `src`.
    #[tokio::test]
    async fn binance_spot_records_frames_with_the_right_src() {
        let server = spawn_mock(false, r#"{"u":1,"s":"BTCUSDT"}"#, 1).await;
        let (source, mut rx) = source(config(
            CexKind::BinanceSpot,
            format!("ws://{}", server.addr),
            &["BTCUSDT"],
        ));
        let shutdown = Arc::new(Notify::new());
        let handle = tokio::spawn(source.run(shutdown.clone()));

        let first = recv_until(&mut rx, Kind::Frame).await;
        assert_eq!(first.src, "binance-spot");
        assert_eq!(first.conn, "binance-spot");
        assert_eq!(first.raw.as_deref(), Some(r#"{"u":1,"s":"BTCUSDT"}"#));

        shutdown.notify_one();
        handle.await.unwrap();
    }

    /// Bybit: the combined subscribe message reaches the server, the `sub`
    /// envelope carries it, and frames are recorded under the right `src`.
    #[tokio::test]
    async fn bybit_records_frames_and_the_subscribe_message() {
        let mut server = spawn_mock(true, r#"{"topic":"orderbook.1.BTCUSDT"}"#, 1).await;
        let (source, mut rx) = source(config(
            CexKind::BybitLinear,
            format!("ws://{}", server.addr),
            &["BTCUSDT", "ETHUSDT"],
        ));
        let shutdown = Arc::new(Notify::new());
        let handle = tokio::spawn(source.run(shutdown.clone()));

        let received = server.subscriptions.recv().await.expect("subscribe frame");
        assert_eq!(
            received,
            r#"{"op":"subscribe","args":["orderbook.1.BTCUSDT","orderbook.1.ETHUSDT"]}"#
        );
        let sub = recv_until(&mut rx, Kind::Sub).await;
        assert_eq!(sub.src, "bybit-linear");
        assert_eq!(
            sub.meta.unwrap()["sub"],
            serde_json::json!({
                "op": "subscribe",
                "args": ["orderbook.1.BTCUSDT", "orderbook.1.ETHUSDT"],
            })
        );
        let frame = recv_until(&mut rx, Kind::Frame).await;
        assert_eq!(frame.src, "bybit-linear");
        assert_eq!(
            frame.raw.as_deref(),
            Some(r#"{"topic":"orderbook.1.BTCUSDT"}"#)
        );

        shutdown.notify_one();
        handle.await.unwrap();
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(&[0, 1, 2, 3]), "AAECAw==");
    }

    /// A binary frame is recorded as a `frame_bin` envelope with base64 `raw`.
    #[tokio::test]
    async fn binary_frames_are_recorded_as_base64() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            let _ = ws.next().await; // subscribe frame
            ws.send(Message::Binary(vec![0, 1, 2, 3].into())).await.ok();
            while let Some(Ok(_)) = ws.next().await {}
        });

        let (source, mut rx) = source(config(
            CexKind::BybitLinear,
            format!("ws://{addr}"),
            &["BTCUSDT"],
        ));
        let shutdown = Arc::new(Notify::new());
        let handle = tokio::spawn(source.run(shutdown.clone()));

        let env = recv_until(&mut rx, Kind::FrameBin).await;
        assert_eq!(env.src, "bybit-linear");
        assert_eq!(env.raw.as_deref(), Some("AAECAw=="));

        shutdown.notify_one();
        handle.await.unwrap();
    }

    /// Every venue records its `sub` envelope under its own `src`.
    #[tokio::test]
    async fn sub_src_for_each_venue() {
        let cases = [
            (CexKind::BinanceUsdm, false),
            (CexKind::BinanceSpot, false),
            (CexKind::BybitLinear, true),
        ];
        for (kind, expect_subscribe) in cases {
            let server = spawn_mock(expect_subscribe, r#"{"x":1}"#, 1).await;
            let (source, mut rx) =
                source(config(kind, format!("ws://{}", server.addr), &["BTCUSDT"]));
            let shutdown = Arc::new(Notify::new());
            let handle = tokio::spawn(source.run(shutdown.clone()));

            let sub = recv_until(&mut rx, Kind::Sub).await;
            assert_eq!(sub.src, kind.src(), "sub src for {kind:?}");
            assert_eq!(sub.conn, kind.src(), "sub conn for {kind:?}");

            shutdown.notify_one();
            handle.await.unwrap();
        }
    }

    /// A busy feed must not starve the Bybit 20 s heartbeat.
    #[tokio::test(start_paused = true)]
    async fn busy_feed_still_pings_every_20_seconds() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (ping_tx, mut ping_rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            let _ = ws.next().await; // subscribe frame
            let mut tick = tokio::time::interval(Duration::from_millis(100));
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        let frame = r#"{"topic":"orderbook.1.BTCUSDT"}"#;
                        if ws.send(Message::Text(frame.into())).await.is_err() {
                            break;
                        }
                    }
                    msg = ws.next() => match msg {
                        Some(Ok(Message::Text(text))) if text.contains("\"op\":\"ping\"") => {
                            let _ = ping_tx.send(tokio::time::Instant::now());
                        }
                        Some(Ok(_)) => {}
                        Some(Err(_)) | None => break,
                    },
                }
            }
        });

        let mut config = config(CexKind::BybitLinear, format!("ws://{addr}"), &["BTCUSDT"]);
        config.watchdog = Duration::from_secs(3600);
        let (source, _rx) = source(config);
        let shutdown = Arc::new(Notify::new());
        let handle = tokio::spawn(source.run(shutdown.clone()));

        // Advance ~62 s in 100 ms steps so the feed stays busy throughout.
        for _ in 0..620 {
            tokio::time::advance(Duration::from_millis(100)).await;
            tokio::task::yield_now().await;
        }

        let mut pings = 0;
        let mut times = Vec::new();
        while let Ok(time) = ping_rx.try_recv() {
            pings += 1;
            times.push(time);
        }
        assert!(
            pings >= 3,
            "expected >= 3 pings on a busy bybit feed, got {pings}"
        );
        for pair in times.windows(2) {
            let delta = pair[1].duration_since(pair[0]);
            assert!(
                delta >= Duration::from_secs(19) && delta <= Duration::from_secs(21),
                "busy-feed ping cadence {delta:?} outside 20s ± 1s"
            );
        }

        shutdown.notify_one();
        let _ = handle.await;
    }

    /// `gap_ms` covers the real outage, not just the post-reconnect slice.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn gap_ms_covers_the_real_downtime() {
        const DOWNTIME: Duration = Duration::from_millis(400);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            // Round 0: send a frame and close.
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            let _ = ws.next().await;
            ws.send(Message::Text(r#"{"topic":"orderbook.1.BTCUSDT"}"#.into()))
                .await
                .ok();
            ws.close(None).await.ok();
            drop(ws);
            // Stay down for a while before the next connection.
            tokio::time::sleep(DOWNTIME).await;
            // Round 1: accept and hold.
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            while let Some(Ok(_)) = ws.next().await {}
        });

        let mut config = config(CexKind::BybitLinear, format!("ws://{addr}"), &["BTCUSDT"]);
        config.watchdog = Duration::from_secs(3600);
        let (source, mut rx) = source(config);
        let shutdown = Arc::new(Notify::new());
        let handle = tokio::spawn(source.run(shutdown.clone()));

        let started = recv_until(&mut rx, Kind::GapStart).await;
        let ended = recv_until(&mut rx, Kind::GapEnd).await;
        let gap_ms = ended.meta.unwrap()["gap_ms"].as_u64().unwrap();
        assert!(
            gap_ms >= DOWNTIME.as_millis() as u64 * 3 / 4,
            "gap_ms {gap_ms} does not cover the {DOWNTIME:?} downtime"
        );
        // The gap_start is stamped at the disconnect, so the two recorded
        // wall-clock times differ by at least the real downtime.
        let recorded_ns = ended.t_ns.saturating_sub(started.t_ns).max(0) as u64;
        assert!(
            recorded_ns >= DOWNTIME.as_nanos() as u64 * 3 / 4,
            "recorded gap {recorded_ns} ns does not cover the {DOWNTIME:?} downtime"
        );
        assert_eq!(gap_ms, recorded_ns / 1_000_000, "gap_ms != t_ns difference");

        shutdown.notify_one();
        let _ = handle.await;
    }

    /// Bybit sends `{"op":"ping"}` every 20 s (SPEC-0008 §9, V-2).
    #[tokio::test(start_paused = true)]
    async fn bybit_pings_every_20_seconds() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (ping_tx, mut ping_rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            // Send one frame so the test can confirm the connection is live
            // (and the source is inside its read loop) before advancing time.
            ws.send(Message::Text(r#"{"topic":"orderbook.1.BTCUSDT"}"#.into()))
                .await
                .ok();
            while let Some(Ok(message)) = ws.next().await {
                if let Message::Text(text) = message
                    && text.contains("\"op\":\"ping\"")
                {
                    let _ = ping_tx.send(text.to_string());
                }
            }
        });

        let mut config = config(CexKind::BybitLinear, format!("ws://{addr}"), &["BTCUSDT"]);
        config.watchdog = Duration::from_secs(3600);
        let (source, mut rx) = source(config);
        let shutdown = Arc::new(Notify::new());
        let handle = tokio::spawn(source.run(shutdown.clone()));

        // Wait until the source has consumed the first frame: it is now in the
        // read loop with its ping interval armed.
        let _ = recv_until(&mut rx, Kind::Frame).await;

        // Advance 61 s of paused time in small steps, letting the tasks run.
        for _ in 0..61 {
            tokio::time::advance(Duration::from_secs(1)).await;
            tokio::task::yield_now().await;
        }

        // 20 s, 40 s, 60 s -> at least 3 pings in 61 s.
        let mut pings = 0;
        while ping_rx.try_recv().is_ok() {
            pings += 1;
        }
        assert!(pings >= 3, "expected >= 3 bybit pings, got {pings}");

        shutdown.notify_one();
        let _ = handle.await;
    }

    /// A server close produces a `gap_start`, a reconnect, and a resubscribe
    /// (a second `conn_open` + `sub` + `gap_end`).
    #[tokio::test]
    async fn bybit_reconnects_and_resubscribes_after_a_close() {
        let mut server = spawn_mock(true, r#"{"topic":"x"}"#, 2).await;
        let (source, mut rx) = source(config(
            CexKind::BybitLinear,
            format!("ws://{}", server.addr),
            &["BTCUSDT"],
        ));
        let shutdown = Arc::new(Notify::new());
        let handle = tokio::spawn(source.run(shutdown.clone()));

        let gap = recv_until(&mut rx, Kind::GapStart).await;
        assert_eq!(gap.meta.unwrap()["reason"], "closed");

        let reopen = recv_until(&mut rx, Kind::ConnOpen).await;
        assert!(reopen.meta.unwrap()["url"].is_string());
        let second_sub = recv_until(&mut rx, Kind::Sub).await;
        assert!(second_sub.seq > gap.seq, "sub must follow the gap");
        let _ = recv_until(&mut rx, Kind::GapEnd).await;

        // The server saw the subscribe on both connections.
        assert_eq!(
            server.subscriptions.recv().await.as_deref(),
            Some(r#"{"op":"subscribe","args":["orderbook.1.BTCUSDT"]}"#)
        );
        assert_eq!(
            server.subscriptions.recv().await.as_deref(),
            Some(r#"{"op":"subscribe","args":["orderbook.1.BTCUSDT"]}"#)
        );

        shutdown.notify_one();
        handle.await.unwrap();
    }

    /// Binance reconnects to the combined URL and re-emits its `sub` envelope
    /// (the URL carries the subscription).
    #[tokio::test]
    async fn binance_reconnects_and_reemits_subscription() {
        let server = spawn_mock(false, r#"{"s":"BTCUSDT"}"#, 2).await;
        let (source, mut rx) = source(config(
            CexKind::BinanceUsdm,
            format!("ws://{}", server.addr),
            &["BTCUSDT"],
        ));
        let shutdown = Arc::new(Notify::new());
        let handle = tokio::spawn(source.run(shutdown.clone()));

        let gap = recv_until(&mut rx, Kind::GapStart).await;
        assert_eq!(gap.meta.unwrap()["reason"], "closed");
        let _ = recv_until(&mut rx, Kind::ConnOpen).await;
        let second_sub = recv_until(&mut rx, Kind::Sub).await;
        assert!(second_sub.seq > gap.seq);
        assert!(recv_until(&mut rx, Kind::GapEnd).await.kind == Kind::GapEnd);
        wait_for_connections(&server, 2).await;

        shutdown.notify_one();
        handle.await.unwrap();
    }

    /// A failed initial dial emits a single `gap_start{error}` and keeps
    /// retrying (no second `gap_start` until a successful open).
    #[tokio::test(start_paused = true)]
    async fn initial_connect_failure_emits_one_gap_start() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let (source, mut rx) = source(config(
            CexKind::BinanceUsdm,
            format!("ws://{addr}"),
            &["BTCUSDT"],
        ));
        let shutdown = Arc::new(Notify::new());
        let handle = tokio::spawn(source.run(shutdown.clone()));

        let gap = recv_until(&mut rx, Kind::GapStart).await;
        assert_eq!(gap.meta.unwrap()["reason"], "error");
        assert_eq!(gap.src, "binance-usdm");

        // Advance past several 3 s retries; repeated failures must not add
        // another `gap_start` while the first is still open.
        for _ in 0..12 {
            tokio::time::advance(Duration::from_secs(1)).await;
            tokio::task::yield_now().await;
        }
        while let Ok(env) = rx.try_recv() {
            assert_ne!(
                env.kind,
                Kind::GapStart,
                "a second gap_start was emitted before gap_end"
            );
        }

        shutdown.notify_one();
        let _ = handle.await;
    }
}
