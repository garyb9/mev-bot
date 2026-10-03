//! Reusable raw WebSocket connection (SPEC-0008 §7.5, task R-3).
//!
//! [`RawWsConn`] owns reconnect, heartbeat, a silence watchdog, jittered
//! backoff, and cancellable shutdown, and yields [`RawEvent`]s. Protocol
//! specifics (URL, how to subscribe, how to keepalive) live behind [`Protocol`]
//! so Hyperliquid today and the recorder's Binance/Bybit sources later share one
//! reconnection implementation. [`crate::ws::WsMarketStream`] is built on top.

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use hl_arb_core::{
    config::Network,
    error::{Error, Result},
};
use hl_arb_metrics::names;
use tokio::net::TcpStream;
use tokio::sync::Notify;
use tokio::time::Instant;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

use crate::ws::ensure_crypto_provider;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Default silence window before the watchdog declares a dead connection.
pub const DEFAULT_WATCHDOG: Duration = Duration::from_secs(45);
/// Default application keepalive interval.
pub const DEFAULT_PING_INTERVAL: Duration = Duration::from_secs(30);
/// Base backoff before jitter.
pub const BACKOFF_BASE: Duration = Duration::from_millis(500);
/// Maximum backoff before jitter.
pub const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// Minimum backoff after jitter, so a host that accepts then immediately closes
/// cannot be retried in a busy loop (full jitter alone can roll ~0 ms).
pub const BACKOFF_MIN: Duration = Duration::from_millis(100);
/// A connection is considered healthy after this long, resetting backoff.
pub const HEALTHY_AFTER: Duration = Duration::from_secs(60);
/// Maximum time to complete a WebSocket handshake before a dial is abandoned
/// with a typed error. Without this a host that accepts TCP and then stalls the
/// handshake hangs the caller forever (PERF-004/SEC-001).
pub const DIAL_TIMEOUT: Duration = Duration::from_secs(3);

/// Per-connection reconnect policy: the backoff schedule, when a dropped
/// connection counts as healthy (resetting the attempt counter), and the
/// handshake deadline for its dials.
///
/// [`ReconnectPolicy::default`] is exactly the historical behaviour
/// ([`BACKOFF_BASE`], [`BACKOFF_MAX`], [`HEALTHY_AFTER`]); Hyperliquid and Bybit
/// use it. [`BACKOFF_MIN`] and the full-jitter formula are shared by all
/// policies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconnectPolicy {
    /// Backoff ceiling for the first retry, before doubling and jitter.
    pub base: Duration,
    /// Maximum backoff ceiling before jitter.
    pub max: Duration,
    /// A connection that lived at least this long resets the attempt counter
    /// when it drops.
    pub healthy_after: Duration,
    /// Maximum time to complete a WebSocket handshake before a dial is
    /// abandoned with a typed error. A host that accepts TCP and then stalls
    /// the handshake must not hang the caller (PERF-004/SEC-001). Tests that
    /// run under paused virtual time dialing a real socket set this far future
    /// so auto-advanced time can never trip it.
    pub dial_timeout: Duration,
}

impl Default for ReconnectPolicy {
    /// The historical policy: 500 ms base, 30 s max, healthy after 60 s, and
    /// the production [`DIAL_TIMEOUT`].
    fn default() -> Self {
        Self {
            base: BACKOFF_BASE,
            max: BACKOFF_MAX,
            healthy_after: HEALTHY_AFTER,
            dial_timeout: DIAL_TIMEOUT,
        }
    }
}

impl ReconnectPolicy {
    /// Whether a connection that lived `lived` before dropping resets the
    /// attempt counter.
    pub fn is_healthy(&self, lived: Duration) -> bool {
        lived >= self.healthy_after
    }

    /// Full-jitter exponential backoff for `attempt` (1-based): a uniform sleep
    /// in `[BACKOFF_MIN, min(base * 2^(attempt-1), max)]`.
    ///
    /// The lower bound stops a host that accepts then immediately closes from
    /// being retried in a tight loop (full jitter alone can roll ~0 ns).
    pub fn jittered_backoff(&self, attempt: u32) -> Duration {
        let shift = attempt.saturating_sub(1).min(6);
        let ceiling = self.base.saturating_mul(1 << shift).min(self.max);
        let floor = BACKOFF_MIN.min(ceiling);
        let span = ceiling.saturating_sub(floor);
        if span.is_zero() {
            return floor;
        }
        let width = span.as_nanos().min(u64::MAX as u128) as u64;
        let roll = pseudo_random();
        floor + Duration::from_nanos(roll % (width + 1))
    }
}

/// How a source connects and keeps itself alive (SPEC-0008 §7.5).
pub trait Protocol: Send + Sync {
    /// A short label for metrics (`hl`, `binance`, `bybit`, …).
    fn name(&self) -> &'static str;
    /// The WebSocket URL to dial.
    fn url(&self) -> String;
    /// Encode a subscription request as a text frame.
    fn subscribe_frame(&self, sub: &str) -> String;
    /// The keepalive text frame, if the protocol uses an app-level ping.
    fn keepalive_frame(&self) -> Option<String> {
        None
    }
}

/// Hyperliquid's raw protocol.
#[derive(Debug, Clone)]
pub struct HlProtocol {
    network: Network,
}

impl HlProtocol {
    /// A protocol for the given network.
    pub fn new(network: Network) -> Self {
        Self { network }
    }
}

impl Protocol for HlProtocol {
    fn name(&self) -> &'static str {
        "hl"
    }

    fn url(&self) -> String {
        self.network.ws_url().to_string()
    }

    fn subscribe_frame(&self, sub: &str) -> String {
        format!(r#"{{"method":"subscribe","subscription":{sub}}}"#)
    }

    fn keepalive_frame(&self) -> Option<String> {
        Some(r#"{"method":"ping"}"#.to_string())
    }
}

/// An event from the raw connection.
#[derive(Debug, Clone)]
pub enum RawEvent {
    /// A text frame.
    Text {
        /// Wall-clock time at read (ms).
        t_ns: i64,
        /// Monotonic time at read (ns).
        mono_ns: u64,
        /// The frame payload.
        text: String,
    },
    /// A binary frame.
    Binary {
        /// Wall-clock time at read (ms).
        t_ns: i64,
        /// Monotonic time at read (ns).
        mono_ns: u64,
        /// The frame payload.
        bytes: Vec<u8>,
    },
    /// The connection opened on the given attempt number (1 = first dial).
    Opened {
        /// Attempt counter.
        attempt: u32,
    },
    /// The feed is not healthy: a watchdog timeout, socket close, error, or
    /// shutdown.
    ///
    /// A `Gap` is returned **immediately** when the drop is detected, before
    /// any reconnect attempt, so callers can mark their state stale at the
    /// moment of the outage. The **next** call to [`RawWsConn::next`] performs
    /// the reconnect (with backoff and resubscribe retries) and yields
    /// [`RawEvent::Opened`] once the feed is up again.
    Gap {
        /// Why the gap happened (`watchdog`, `closed`, `error`, `shutdown`).
        reason: String,
        /// Human-readable detail.
        detail: String,
        /// Process-monotonic nanoseconds ([`mono_ns`]) when the drop was
        /// detected. `0` for a clean shutdown.
        disconnect_ns: u64,
        /// Wall-clock nanoseconds since the Unix epoch when the drop was
        /// detected. `0` for a clean shutdown.
        t_ns: i64,
    },
}

/// A raw, reconnecting WebSocket connection.
pub struct RawWsConn {
    protocol: Box<dyn Protocol>,
    socket: Socket,
    subscriptions: Vec<String>,
    watchdog: Duration,
    ping_interval: Duration,
    shutdown: Arc<Notify>,
    last_data: Instant,
    opened_at: Instant,
    /// Deadline of the next application keepalive, created once per connection
    /// and advanced only when a keepalive is actually sent. Keeping it on the
    /// connection (not rebuilt per `next()`) means a busy feed still gets its
    /// heartbeats instead of resetting the timer on every frame.
    next_ping: Instant,
    policy: ReconnectPolicy,
    attempt: u32,
    metrics_src: &'static str,
    /// Set when a `Gap` has been returned: the next `next()` reconnects first
    /// and then yields `Opened`.
    pending_reconnect: bool,
}

impl RawWsConn {
    /// Dial `protocol` and start streaming.
    pub async fn connect(protocol: Box<dyn Protocol>, subscriptions: Vec<String>) -> Result<Self> {
        Self::connect_with(
            protocol,
            subscriptions,
            DEFAULT_WATCHDOG,
            DEFAULT_PING_INTERVAL,
        )
        .await
    }

    /// Dial with explicit watchdog and keepalive timings.
    pub async fn connect_with(
        protocol: Box<dyn Protocol>,
        subscriptions: Vec<String>,
        watchdog: Duration,
        ping_interval: Duration,
    ) -> Result<Self> {
        Self::connect_with_policy(
            protocol,
            subscriptions,
            watchdog,
            ping_interval,
            ReconnectPolicy::default(),
        )
        .await
    }

    /// Dial with explicit timings and a custom [`ReconnectPolicy`].
    pub async fn connect_with_policy(
        protocol: Box<dyn Protocol>,
        subscriptions: Vec<String>,
        watchdog: Duration,
        ping_interval: Duration,
        policy: ReconnectPolicy,
    ) -> Result<Self> {
        let socket = dial_with_timeout(&protocol.url(), policy.dial_timeout).await?;
        let src = protocol.name();
        let now = Instant::now();
        let mut conn = Self {
            protocol,
            socket,
            subscriptions,
            watchdog,
            ping_interval,
            shutdown: Arc::new(Notify::new()),
            last_data: now,
            opened_at: now,
            next_ping: now + ping_interval,
            policy,
            attempt: 1,
            metrics_src: src,
            pending_reconnect: false,
        };
        send_subscriptions(&mut conn.socket, &*conn.protocol, &conn.subscriptions).await?;
        metrics::counter!(names::WS_RECONNECTS, "src" => src, "reason" => "open").increment(1);
        metrics::gauge!(names::WS_CONNECTED, "src" => src).set(1.0);
        Ok(conn)
    }

    /// A handle that, when signalled, stops the connection cleanly.
    pub fn shutdown_handle(&self) -> Arc<Notify> {
        self.shutdown.clone()
    }

    /// Milliseconds since the last inbound frame of any kind.
    pub fn idle_ms(&self) -> u128 {
        self.last_data.elapsed().as_millis()
    }

    /// Dial with full-jitter backoff until a connection opens or shutdown is
    /// requested. Returns `Ok(())` on reconnect and `Err` on shutdown.
    ///
    /// The attempt counter is reset only after a **healthy** connection: a
    /// drop that follows a run of at least the policy's `healthy_after` resets it in
    /// [`RawWsConn::gap_now`] (at the drop, before the first retry). Otherwise
    /// it keeps growing, so a host that accepts and immediately drops cannot
    /// pin the backoff low. Resetting at the drop — a single synchronous step —
    /// rather than here means a reconnect future that is cancelled and
    /// restarted does not re-run the reset and cannot pin the backoff low.
    ///
    /// Cancel safety: the new socket is built in locals and every field is
    /// committed in one synchronous step at the end, only after the full
    /// subscription set has been sent. If this future is dropped mid-attempt,
    /// `self` still points at the previous socket and `self.attempt` keeps its
    /// last increment, so the next call redials and resubscribes from scratch
    /// without ever exposing a partially-subscribed live socket.
    async fn reconnect(&mut self) -> Result<()> {
        metrics::gauge!(names::WS_CONNECTED, "src" => self.metrics_src).set(0.0);
        loop {
            self.attempt = self.attempt.saturating_add(1);
            let backoff = self.policy.jittered_backoff(self.attempt);
            tokio::select! {
                _ = self.shutdown.notified() => {
                    return Err(Error::Http("websocket shutdown requested".into()));
                }
                _ = tokio::time::sleep(backoff) => {}
            }

            let url = self.protocol.url();
            let mut socket = tokio::select! {
                _ = self.shutdown.notified() => {
                    return Err(Error::Http("websocket shutdown requested".into()));
                }
                result = dial_with_timeout(&url, self.policy.dial_timeout) => {
                    match result {
                        Ok(socket) => socket,
                        Err(err) => {
                            tracing::warn!(attempt = self.attempt, error = %err, "websocket reconnect failed");
                            continue;
                        }
                    }
                }
            };
            // A resubscribe send failure means this connection is unusable:
            // treat it as another failed attempt and keep backing off, instead
            // of terminating the stream. Send on the local socket so a cancel
            // in here drops the partial connection and leaves `self` untouched.
            if let Err(err) =
                send_subscriptions(&mut socket, &*self.protocol, &self.subscriptions).await
            {
                tracing::warn!(
                    attempt = self.attempt,
                    error = %err,
                    "websocket resubscribe after reconnect failed"
                );
                continue;
            }
            // The connection is fully usable now: commit it and every piece of
            // per-connection bookkeeping in one synchronous step.
            let now = Instant::now();
            self.socket = socket;
            self.last_data = now;
            self.opened_at = now;
            self.next_ping = now + self.ping_interval;
            metrics::counter!(
                names::WS_RECONNECTS,
                "src" => self.metrics_src,
                "reason" => "reconnect",
            )
            .increment(1);
            metrics::gauge!(names::WS_CONNECTED, "src" => self.metrics_src).set(1.0);
            tracing::debug!(attempt = self.attempt, "websocket reconnected");
            return Ok(());
        }
    }

    /// The next raw event.
    ///
    /// Yields [`RawEvent::Gap`] **immediately** when the feed breaks (before
    /// any reconnect attempt), then performs the reconnect on the following
    /// call and yields [`RawEvent::Opened`] once it is back. On shutdown it
    /// returns `Err`.
    ///
    /// Cancel safety: the reconnect branch clears `pending_reconnect` only
    /// after [`RawWsConn::reconnect`] returns `Ok`, so a dropped future leaves
    /// the reconnect pending. The next call retries the reconnect and still
    /// yields exactly one `Opened`, never a second `Gap` for the same outage.
    pub async fn next(&mut self) -> Result<RawEvent> {
        if self.pending_reconnect {
            // A `Gap` was returned by the previous call: reconnect now (with
            // backoff, resubscribe retries, and shutdown interruption). Clear
            // the flag only once the reconnect really succeeded.
            self.reconnect().await?;
            self.pending_reconnect = false;
            return Ok(RawEvent::Opened {
                attempt: self.attempt,
            });
        }
        loop {
            tokio::select! {
                _ = self.shutdown.notified() => {
                    metrics::gauge!(names::WS_CONNECTED, "src" => self.metrics_src).set(0.0);
                    return Ok(RawEvent::Gap {
                        reason: "shutdown".into(),
                        detail: "cancellation requested".into(),
                        disconnect_ns: 0,
                        t_ns: 0,
                    });
                }
                _ = tokio::time::sleep_until(self.next_ping) => {
                    // Advance the deadline only when the ping is actually sent
                    // (or skipped by a protocol without keepalive), so frames
                    // flowing in between do not reset the heartbeat.
                    self.next_ping = Instant::now() + self.ping_interval;
                    if let Some(frame) = self.protocol.keepalive_frame()
                        && self.socket.send(Message::Text(frame.into())).await.is_err()
                    {
                        return Ok(self.gap_now("error", "keepalive send failed"));
                    }
                }
                _ = tokio::time::sleep_until(self.last_data + self.watchdog) => {
                    let detail = format!("no inbound frame for {}s", self.watchdog.as_secs());
                    return Ok(self.gap_now("watchdog", &detail));
                }
                msg = self.socket.next() => {
                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            self.last_data = Instant::now();
                            return Ok(RawEvent::Text {
                                t_ns: now_ms(),
                                mono_ns: mono_ns(),
                                text: text.to_string(),
                            });
                        }
                        Some(Ok(Message::Binary(bytes))) => {
                            self.last_data = Instant::now();
                            return Ok(RawEvent::Binary {
                                t_ns: now_ms(),
                                mono_ns: mono_ns(),
                                bytes: bytes.to_vec(),
                            });
                        }
                        Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => {
                            // Any inbound frame, including pong, keeps the watchdog fed.
                            self.last_data = Instant::now();
                        }
                        Some(Ok(Message::Close(_))) => {
                            return Ok(self.gap_now("closed", "server closed"));
                        }
                        Some(Ok(Message::Frame(_))) => {}
                        Some(Err(err)) => {
                            return Ok(self.gap_now("error", &err.to_string()));
                        }
                        None => {
                            return Ok(self.gap_now("closed", "stream ended"));
                        }
                    }
                }
            }
        }
    }

    /// Announce a `Gap` at the moment the drop is detected and arrange for the
    /// next [`RawWsConn::next`] to reconnect. Carries both the monotonic and the
    /// wall-clock disconnect timestamps so callers can bracket the outage.
    fn gap_now(&mut self, reason: &str, detail: &str) -> RawEvent {
        metrics::counter!(
            names::WS_RECONNECTS,
            "src" => self.metrics_src,
            "reason" => reason.to_string(),
        )
        .increment(1);
        // The socket is already down; reflect it immediately rather than when
        // the reconnect starts on the next call.
        metrics::gauge!(names::WS_CONNECTED, "src" => self.metrics_src).set(0.0);
        // Reset the attempt counter after a healthy run, exactly once per
        // outage. Doing it here (synchronous, at the drop) instead of inside
        // `reconnect` means a cancelled-and-restarted reconnect cannot re-run
        // the reset and so cannot pin the backoff low.
        if self.policy.is_healthy(self.opened_at.elapsed()) {
            self.attempt = 0;
        }
        self.pending_reconnect = true;
        tracing::debug!(reason, detail, "websocket gap detected");
        RawEvent::Gap {
            reason: reason.to_string(),
            detail: detail.to_string(),
            disconnect_ns: mono_ns(),
            t_ns: now_ns(),
        }
    }
}

/// Backoff under the default policy (test helper for the historical bounds).
#[cfg(test)]
fn jittered_backoff(attempt: u32) -> Duration {
    ReconnectPolicy::default().jittered_backoff(attempt)
}

/// A cheap, non-cryptographic random source for jitter. Randomness here only
/// needs to spread reconnect storms, not be unpredictable.
fn pseudo_random() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(mono_ns());
    hasher.finish()
}

/// Enable `TCP_NODELAY` on the underlying TCP socket of a live WebSocket
/// connection (SPEC-0010 §12/§18: `TCP_NODELAY` is always on).
///
/// [`MaybeTlsStream::get_ref`] reaches the inner [`TcpStream`] for both the
/// plain and the rustls variants (and native-tls, if compiled in), so TLS
/// sockets are covered too, not only plain ones.
pub(crate) fn set_tcp_nodelay(stream: &MaybeTlsStream<TcpStream>) -> Result<()> {
    stream
        .get_ref()
        .set_nodelay(true)
        .map_err(|e| Error::Http(format!("failed to set TCP_NODELAY: {e}")))
}

/// Send the full subscription set over `socket` (SPEC-0008 §7.5).
///
/// Free rather than a method on [`RawWsConn`] so a reconnect can resubscribe a
/// **local** socket before committing it to the struct, which keeps
/// [`RawWsConn::next`] cancel-safe: dropping the future drops the partial
/// connection instead of leaving it live with only some subscriptions.
async fn send_subscriptions(
    socket: &mut Socket,
    protocol: &dyn Protocol,
    subscriptions: &[String],
) -> Result<()> {
    for sub in subscriptions {
        let frame = protocol.subscribe_frame(sub);
        socket
            .send(Message::Text(frame.into()))
            .await
            .map_err(|e| Error::Http(e.to_string()))?;
    }
    Ok(())
}

async fn dial_with_timeout(url: &str, timeout: Duration) -> Result<Socket> {
    ensure_crypto_provider();
    let (socket, _resp) = tokio::time::timeout(timeout, connect_async(url))
        .await
        .map_err(|_| Error::Http(format!("websocket dial timed out after {timeout:?}")))?
        .map_err(|e| Error::Http(e.to_string()))?;
    set_tcp_nodelay(socket.get_ref())?;
    Ok(socket)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Wall-clock nanoseconds since the Unix epoch, used to stamp gap disconnect
/// times so the recorder can bracket an outage in its envelope timeline.
fn now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

/// Process-global monotonic nanoseconds, used to stamp received frames and to
/// measure end-to-end latency (SPEC-0002 H-7).
pub fn mono_ns() -> u64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    let start = START.get_or_init(std::time::Instant::now);
    start.elapsed().as_nanos() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;

    #[test]
    fn default_policy_equals_the_historical_constants() {
        let p = ReconnectPolicy::default();
        assert_eq!(p.base, Duration::from_millis(500));
        assert_eq!(p.max, Duration::from_secs(30));
        assert_eq!(p.healthy_after, Duration::from_secs(60));
        assert_eq!(BACKOFF_MIN, Duration::from_millis(100));
        for attempt in 1..=20 {
            let d = p.jittered_backoff(attempt);
            assert!(d <= Duration::from_secs(30), "attempt {attempt}: {d:?}");
            assert!(d >= Duration::from_millis(100), "attempt {attempt}: {d:?}");
        }
    }

    #[test]
    fn fast_policy_backoff_stays_within_its_cap_and_the_floor() {
        let p = ReconnectPolicy {
            base: Duration::from_millis(250),
            max: Duration::from_secs(10),
            healthy_after: Duration::from_secs(5),
            dial_timeout: DIAL_TIMEOUT,
        };
        for attempt in 1..=50 {
            let d = p.jittered_backoff(attempt);
            assert!(d <= Duration::from_secs(10), "attempt {attempt}: {d:?}");
            assert!(d >= BACKOFF_MIN, "attempt {attempt}: {d:?}");
            // The ceiling doubles from `base`, so `base` is really used: an
            // implementation that ignored it would blow these bounds.
            let ceiling = p
                .base
                .saturating_mul(1 << attempt.saturating_sub(1).min(6))
                .min(p.max);
            assert!(d <= ceiling, "attempt {attempt}: {d:?} > {ceiling:?}");
        }
        for _ in 0..200 {
            assert!(p.jittered_backoff(1) <= Duration::from_millis(250));
            assert!(p.jittered_backoff(2) <= Duration::from_millis(500));
        }
    }

    #[test]
    fn healthy_reset_depends_on_the_policy_threshold() {
        let fast = ReconnectPolicy {
            healthy_after: Duration::from_secs(5),
            ..ReconnectPolicy::default()
        };
        assert!(fast.is_healthy(Duration::from_secs(6)));
        assert!(!fast.is_healthy(Duration::from_secs(4)));
        let default = ReconnectPolicy::default();
        assert!(!default.is_healthy(Duration::from_secs(6)));
        assert!(default.is_healthy(Duration::from_secs(60)));
    }

    #[test]
    fn backoff_is_capped_jittered_and_floored() {
        for attempt in 1..20 {
            let d = jittered_backoff(attempt);
            assert!(d <= BACKOFF_MAX, "attempt {attempt}: {d:?}");
            assert!(d >= BACKOFF_MIN, "attempt {attempt}: {d:?}");
        }
        // Many samples should not all be identical (jitter present).
        let samples: std::collections::BTreeSet<u64> = (0..50)
            .map(|_| jittered_backoff(4).as_nanos() as u64)
            .collect();
        assert!(samples.len() > 1, "jitter collapsed to one value");
    }

    #[test]
    fn hl_protocol_frames() {
        let protocol = HlProtocol::new(Network::Testnet);
        assert!(protocol.url().contains("testnet"));
        assert_eq!(
            protocol.subscribe_frame(r#"{"type":"l2Book","coin":"BTC"}"#),
            r#"{"method":"subscribe","subscription":{"type":"l2Book","coin":"BTC"}}"#
        );
        assert_eq!(
            protocol.keepalive_frame().as_deref(),
            Some(r#"{"method":"ping"}"#)
        );
    }

    /// A test protocol pointed at a fixed URL, with no keepalive.
    struct TestProtocol {
        url: String,
    }

    impl Protocol for TestProtocol {
        fn name(&self) -> &'static str {
            "test"
        }
        fn url(&self) -> String {
            self.url.clone()
        }
        fn subscribe_frame(&self, sub: &str) -> String {
            sub.to_string()
        }
    }

    fn protocol(url: String) -> Box<dyn Protocol> {
        Box::new(TestProtocol { url })
    }

    #[tokio::test]
    async fn reconnects_after_server_closes_and_resubscribes() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (subs_tx, mut subs_rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            // First connection: read the subscribe frame, reply with a text
            // frame, then close. Second connection: read the subscribe frame
            // and hold it open.
            for round in 0..2 {
                let (stream, _) = listener.accept().await.unwrap();
                let mut ws = accept_async(stream).await.unwrap();
                if let Some(Ok(Message::Text(text))) = ws.next().await {
                    let _ = subs_tx.send(text.to_string());
                }
                if round == 0 {
                    ws.send(Message::Text("hello".into())).await.unwrap();
                    ws.close(None).await.unwrap();
                    // Drop to force the client's `next` to observe close.
                    drop(ws);
                } else {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        });

        let mut conn = RawWsConn::connect_with(
            protocol(format!("ws://{addr}")),
            vec!["sub1".to_string()],
            Duration::from_millis(500),
            Duration::from_secs(30),
        )
        .await
        .unwrap();

        // First text frame.
        let first = conn.next().await.unwrap();
        assert!(matches!(first, RawEvent::Text { .. }), "{first:?}");
        // Then a close => Gap, then Opened on reconnect.
        let mut saw_gap = false;
        let mut saw_opened = false;
        for _ in 0..4 {
            match conn.next().await.unwrap() {
                RawEvent::Gap { reason, .. } => {
                    assert_eq!(reason, "closed");
                    saw_gap = true;
                }
                RawEvent::Opened { .. } => {
                    saw_opened = true;
                    break;
                }
                _ => {}
            }
        }
        assert!(saw_gap, "expected a Gap");
        assert!(saw_opened, "expected an Opened after reconnect");
        // Both connections resubscribed.
        assert_eq!(subs_rx.recv().await.as_deref(), Some("sub1"));
        assert_eq!(subs_rx.recv().await.as_deref(), Some("sub1"));
    }

    #[tokio::test]
    async fn watchdog_fires_on_silent_server() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            // Accept any number of connections, each silent forever, so a
            // reconnect during the test still completes its handshake.
            while let Ok((stream, _)) = listener.accept().await {
                let mut ws = accept_async(stream).await.unwrap();
                tokio::spawn(async move { while let Some(Ok(_)) = ws.next().await {} });
            }
        });
        let mut conn = RawWsConn::connect_with(
            protocol(format!("ws://{addr}")),
            vec![],
            Duration::from_millis(200),
            Duration::from_secs(30),
        )
        .await
        .unwrap();
        let event = conn.next().await.unwrap();
        match event {
            RawEvent::Gap { reason, .. } => assert_eq!(reason, "watchdog"),
            other => panic!("expected watchdog gap, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dial_times_out_on_a_stalled_handshake() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            // Accept TCP but never complete the WebSocket handshake.
            let (stream, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(30)).await;
            drop(stream);
        });

        let start = Instant::now();
        let err = match RawWsConn::connect_with_policy(
            protocol(format!("ws://{addr}")),
            vec![],
            Duration::from_secs(30),
            Duration::from_secs(30),
            ReconnectPolicy {
                dial_timeout: Duration::from_millis(100),
                ..ReconnectPolicy::default()
            },
        )
        .await
        {
            Err(err) => err,
            Ok(_) => panic!("a stalled handshake must not connect"),
        };
        assert!(matches!(err, Error::Http(_)), "got {err:?}");
        assert!(err.to_string().contains("timed out"), "{err}");
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "a stalled handshake must fail fast, took {:?}",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn shutdown_interrupts_a_stalled_reconnect_dial() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            // First connection: complete, then close to force a gap.
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            ws.close(None).await.unwrap();
            drop(ws);
            // Second connection: accept TCP but never finish the handshake.
            let (_stream, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(30)).await;
        });

        let mut conn = RawWsConn::connect_with_policy(
            protocol(format!("ws://{addr}")),
            vec![],
            Duration::from_secs(3_600),
            Duration::from_secs(30),
            // Tiny backoff so the reconnect reaches the dial immediately, and a
            // long dial timeout so only the shutdown signal can end the call.
            ReconnectPolicy {
                base: Duration::from_millis(10),
                max: Duration::from_millis(10),
                healthy_after: Duration::from_secs(60),
                dial_timeout: Duration::from_secs(30),
            },
        )
        .await
        .unwrap();

        let gap = conn.next().await.unwrap();
        assert!(matches!(gap, RawEvent::Gap { .. }), "{gap:?}");

        let shutdown = conn.shutdown_handle();
        let reconnect = tokio::spawn(async move { conn.next().await });
        // Let the reconnect reach the stalled dial before cancelling.
        tokio::time::sleep(Duration::from_millis(50)).await;
        shutdown.notify_one();

        let result = tokio::time::timeout(Duration::from_secs(2), reconnect).await;
        match result {
            Ok(Ok(Err(Error::Http(_)))) => {}
            other => panic!("shutdown did not interrupt the stalled dial: {other:?}"),
        }
    }

    #[tokio::test]
    async fn set_tcp_nodelay_enables_nodelay_on_loopback() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            // Hold the connection open while the client flips the flag.
            tokio::time::sleep(Duration::from_millis(250)).await;
            drop(stream);
        });

        let tcp = TcpStream::connect(addr).await.unwrap();
        tcp.set_nodelay(false).unwrap();
        assert!(!tcp.nodelay().unwrap(), "precondition: nodelay starts off");
        let stream = MaybeTlsStream::Plain(tcp);
        set_tcp_nodelay(&stream).unwrap();
        assert!(stream.get_ref().nodelay().unwrap());

        server.await.unwrap();
    }

    #[tokio::test]
    async fn cancellation_stops_a_reconnect_loop() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let mut ws = accept_async(stream).await.unwrap();
                tokio::spawn(async move { while let Some(Ok(_)) = ws.next().await {} });
            }
        });
        let mut conn = RawWsConn::connect_with(
            protocol(format!("ws://{addr}")),
            vec![],
            Duration::from_secs(30),
            Duration::from_secs(30),
        )
        .await
        .unwrap();
        let shutdown = conn.shutdown_handle();
        shutdown.notify_one();
        let event = conn.next().await.unwrap();
        assert!(
            matches!(event, RawEvent::Gap { ref reason, .. } if reason == "shutdown"),
            "expected shutdown gap, got {event:?}"
        );
    }

    /// A protocol with an application keepalive frame (`"ping"`).
    struct KeepaliveProtocol {
        url: String,
    }

    impl Protocol for KeepaliveProtocol {
        fn name(&self) -> &'static str {
            "keepalive-test"
        }
        fn url(&self) -> String {
            self.url.clone()
        }
        fn subscribe_frame(&self, sub: &str) -> String {
            sub.to_string()
        }
        fn keepalive_frame(&self) -> Option<String> {
            Some("ping".to_string())
        }
    }

    /// The heartbeat must keep its cadence while data flows: a frame every
    /// 100 ms must not reset the 20 s ping (the old per-`next()` interval did).
    #[tokio::test(start_paused = true)]
    async fn pings_are_not_starved_by_a_busy_feed() {
        use tokio::sync::mpsc;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (ping_tx, mut ping_rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            let mut tick = tokio::time::interval(Duration::from_millis(100));
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        if ws.send(Message::Text("frame".into())).await.is_err() {
                            break;
                        }
                    }
                    msg = ws.next() => match msg {
                        Some(Ok(Message::Text(text))) if text == "ping" => {
                            let _ = ping_tx.send(tokio::time::Instant::now());
                        }
                        Some(Ok(_)) => {}
                        Some(Err(_)) | None => break,
                    },
                }
            }
        });

        let conn = RawWsConn::connect_with_policy(
            Box::new(KeepaliveProtocol {
                url: format!("ws://{addr}"),
            }),
            vec![],
            // Far-future watchdog isolates the ping cadence from idle logic.
            Duration::from_secs(3_600),
            Duration::from_secs(20),
            // Paused virtual time would fire a wall-clock-sized dial deadline
            // before the real handshake finishes, so keep it far future here.
            ReconnectPolicy {
                dial_timeout: Duration::from_secs(3_600),
                ..ReconnectPolicy::default()
            },
        )
        .await
        .unwrap();

        // Drain frames for ~62 s of paused time without ever going idle.
        let mut conn = conn;
        let client = tokio::spawn(async move { while conn.next().await.is_ok() {} });
        for _ in 0..620 {
            tokio::time::advance(Duration::from_millis(100)).await;
            tokio::task::yield_now().await;
        }
        client.abort();

        let mut times = Vec::new();
        while let Ok(time) = ping_rx.try_recv() {
            times.push(time);
        }
        assert!(
            times.len() >= 3,
            "expected >= 3 pings on a busy feed, got {}",
            times.len()
        );
        for pair in times.windows(2) {
            let delta = pair[1].duration_since(pair[0]);
            assert!(
                delta >= Duration::from_secs(19) && delta <= Duration::from_secs(21),
                "ping cadence {delta:?} outside 20s ± 1s"
            );
        }
    }

    /// A drop is announced at once, before any reconnect: with the server
    /// refusing connections for 2 s of logical time, the `Gap` arrives without
    /// advancing the clock and `Opened` is the later call (SPEC-0008 RW-2).
    #[tokio::test(start_paused = true)]
    async fn gap_is_immediate_and_opened_after_the_reconnect() {
        const REFUSAL: Duration = Duration::from_secs(2);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            // First connection: one frame, then close.
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            ws.send(Message::Text("hello".into())).await.unwrap();
            ws.close(None).await.unwrap();
            drop(ws);
            // Refuse the next reconnect by delaying the handshake.
            let (stream, _) = listener.accept().await.unwrap();
            tokio::time::sleep(REFUSAL).await;
            let mut ws = accept_async(stream).await.unwrap();
            while let Some(Ok(_)) = ws.next().await {}
        });
        let mut conn = RawWsConn::connect_with_policy(
            protocol(format!("ws://{addr}")),
            vec![],
            Duration::from_secs(3_600),
            Duration::from_secs(3_600),
            // Paused time: keep the dial deadline far future.
            ReconnectPolicy {
                dial_timeout: Duration::from_secs(3_600),
                ..ReconnectPolicy::default()
            },
        )
        .await
        .unwrap();
        let _ = conn.next().await.unwrap(); // the hello

        // The drop is announced at the drop: no logical time may pass.
        let started = Instant::now();
        let gap = conn.next().await.unwrap();
        let gap_waited = started.elapsed();
        match gap {
            RawEvent::Gap { ref reason, .. } => assert_eq!(reason, "closed"),
            other => panic!("expected a gap, got {other:?}"),
        }
        assert_eq!(
            gap_waited,
            Duration::ZERO,
            "the gap waited {gap_waited:?}; it must be announced at the drop, not after the reconnect"
        );

        // The reconnect happens on the next call, after the refusal.
        let reopen_started = Instant::now();
        let opened = conn.next().await.unwrap();
        assert!(matches!(opened, RawEvent::Opened { .. }), "{opened:?}");
        assert!(
            reopen_started.elapsed() >= REFUSAL,
            "Opened arrived before the server let the reconnect through"
        );
    }

    /// `Gap` carries the disconnect wall-clock and monotonic times, taken at the
    /// instant the drop is detected, before any reconnect. This fails if the
    /// stamp is taken at the `next()` call (too early), after the reconnect
    /// (too late), or at the reconnect call.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn gap_carries_the_disconnect_time() {
        const DROP_DELAY: Duration = Duration::from_millis(500);
        const DOWNTIME: Duration = Duration::from_millis(500);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            // The connection stays up for DROP_DELAY, so a stamp taken at
            // `next()`-entry would predate the real drop by that much.
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            ws.send(Message::Text("hello".into())).await.unwrap();
            tokio::time::sleep(DROP_DELAY).await;
            ws.close(None).await.unwrap();
            drop(ws);
            let (stream, _) = listener.accept().await.unwrap();
            tokio::time::sleep(DOWNTIME).await;
            let mut ws = accept_async(stream).await.unwrap();
            while let Some(Ok(_)) = ws.next().await {}
        });
        let mut conn = RawWsConn::connect_with(
            protocol(format!("ws://{addr}")),
            vec![],
            Duration::from_secs(3_600),
            Duration::from_secs(3_600),
        )
        .await
        .unwrap();
        let _ = conn.next().await.unwrap(); // the hello

        // Stamp the monotonic clock at `next()`-entry: the drop is DROP_DELAY
        // later, so a stamp taken at entry would be too early.
        let entry_ns = mono_ns();
        let gap = conn.next().await.unwrap();
        let after_gap_ns = mono_ns();
        let RawEvent::Gap {
            reason,
            disconnect_ns,
            t_ns,
            ..
        } = gap
        else {
            panic!("expected a gap");
        };
        assert_eq!(reason, "closed");
        assert!(disconnect_ns > 0);
        assert!(
            disconnect_ns >= entry_ns + DROP_DELAY.as_nanos() as u64 / 2,
            "disconnect_ns {disconnect_ns} was stamped at the next() call, before the drop at entry+{DROP_DELAY:?}"
        );
        assert!(
            disconnect_ns <= after_gap_ns,
            "disconnect_ns was stamped after the gap was observed"
        );
        let wall_gap_ns = t_ns;
        assert!(wall_gap_ns > 0);

        // The reconnect completes at least DOWNTIME later; the stamp must
        // predate it and not be taken at the reconnect call.
        let reconnect_entry_ns = mono_ns();
        let opened = conn.next().await.unwrap();
        assert!(matches!(opened, RawEvent::Opened { .. }));
        let after_reconnect_ns = mono_ns();
        assert!(
            disconnect_ns < reconnect_entry_ns,
            "disconnect_ns was stamped at the reconnect call"
        );
        let measured = Duration::from_nanos(after_reconnect_ns.saturating_sub(disconnect_ns));
        assert!(
            measured >= DOWNTIME.mul_f64(0.5),
            "the gap did not cover the outage: {measured:?}"
        );
        let wall_measured = Duration::from_nanos((now_ns() - wall_gap_ns).max(0) as u64);
        assert!(
            wall_measured >= DOWNTIME.mul_f64(0.5),
            "gap t_ns was stamped too late: measured outage {wall_measured:?}"
        );
    }

    /// Cancelling `next()` mid-reconnect (as `run_ws_conn`'s 60 s tick does)
    /// must not produce a second `Gap` for the same outage, must not reset the
    /// backoff attempt counter, and must still yield exactly one `Opened` when
    /// the server returns (SPEC-0008 RW-3).
    #[tokio::test(start_paused = true)]
    async fn cancelled_reconnect_yields_one_gap_and_keeps_backoff() {
        const REFUSAL: Duration = Duration::from_secs(180);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let accepts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let accepts_task = accepts.clone();
        tokio::spawn(async move {
            // Round 0: normal connection, read the subscribe, close (the outage
            // begins).
            let (stream, _) = listener.accept().await.unwrap();
            accepts_task.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut ws = accept_async(stream).await.unwrap();
            let _ = ws.next().await;
            ws.close(None).await.ok();
            drop(ws);
            // Refuse every dial for REFUSAL of logical time, then keep serving
            // whatever the client dials next (a tick may cancel a dial mid
            // handshake, so do not bind the server to one socket).
            let deadline = tokio::time::Instant::now() + REFUSAL;
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                accepts_task.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if tokio::time::Instant::now() < deadline {
                    drop(stream);
                    continue;
                }
                if let Ok(mut ws) = accept_async(stream).await {
                    while let Some(Ok(_)) = ws.next().await {}
                }
            }
        });
        let mut conn = RawWsConn::connect_with_policy(
            protocol(format!("ws://{addr}")),
            vec!["s".to_string()],
            Duration::from_secs(3_600),
            Duration::from_secs(3_600),
            // Paused time: keep the dial deadline far future.
            ReconnectPolicy {
                dial_timeout: Duration::from_secs(3_600),
                ..ReconnectPolicy::default()
            },
        )
        .await
        .unwrap();

        // Drive the connection like run_ws_conn: a 60 s tick races next() and
        // drops the in-flight reconnect whenever it fires.
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        tick.tick().await; // consume the immediate tick
        let mut gaps = 0u32;
        let mut opened = None;
        for _ in 0..10_000 {
            tokio::select! {
                _ = tick.tick() => {}
                event = conn.next() => match event.unwrap() {
                    RawEvent::Gap { .. } => gaps += 1,
                    RawEvent::Opened { attempt } => {
                        opened = Some(attempt);
                        break;
                    }
                    _ => {}
                },
            }
        }
        let attempt = opened.expect("no Opened after the outage");
        assert_eq!(gaps, 1, "a cancelled reconnect emitted another Gap");
        let accepts = accepts.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            accepts >= 3,
            "expected several refused dials, got {accepts}"
        );
        assert!(
            attempt >= accepts as u32,
            "the backoff attempt counter was reset: attempt {attempt} < {accepts} dials"
        );
    }

    /// Cancelling `next()` between assigning the new socket and finishing the
    /// resubscribe must not leave a live socket with only some subscriptions:
    /// the next call redials and (re)subscribes the full set, then yields
    /// `Opened` (SPEC-0008 RW-3).
    #[tokio::test(start_paused = true)]
    async fn cancelled_resubscribe_leaves_no_partial_subscriptions() {
        use tokio::sync::Notify;
        use tokio::sync::mpsc;

        const N: usize = 1024;
        // ~4 KiB per subscription, so the reconnect's resubscribe cannot finish
        // while the server reads only the first frame: the writes fill the
        // socket buffers and the future parks, exactly the cancel window.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (first_sub_tx, mut first_sub_rx) = mpsc::unbounded_channel::<()>();
        let (full_tx, mut full_rx) = mpsc::unbounded_channel::<usize>();
        let round1_go = std::sync::Arc::new(Notify::new());
        let round1_go_task = round1_go.clone();
        tokio::spawn(async move {
            // Round 0: read the full subscribe set, then close.
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            for _ in 0..N {
                let _ = ws.next().await;
            }
            ws.close(None).await.ok();
            drop(ws);
            // Round 1: handshake, read exactly one subscribe frame, signal the
            // test to cancel, then stop reading and wait to be released.
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            if let Some(Ok(_)) = ws.next().await {
                let _ = first_sub_tx.send(());
            }
            round1_go_task.notified().await;
            drop(ws);
            // Round 2: the recovered connection must see the full set.
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            let mut count = 0usize;
            while let Some(Ok(Message::Text(_))) = ws.next().await {
                count += 1;
                if count == N {
                    let _ = full_tx.send(count);
                }
            }
        });

        let subs: Vec<String> = (0..N)
            .map(|i| format!("{i}:{}", "x".repeat(4 * 1024)))
            .collect();
        let mut conn = RawWsConn::connect_with_policy(
            protocol(format!("ws://{addr}")),
            subs,
            Duration::from_secs(3_600),
            Duration::from_secs(3_600),
            // Paused time: keep the dial deadline far future.
            ReconnectPolicy {
                dial_timeout: Duration::from_secs(3_600),
                ..ReconnectPolicy::default()
            },
        )
        .await
        .unwrap();

        // The initial close produces the outage that triggers the reconnect.
        let gap = conn.next().await.unwrap();
        assert!(matches!(gap, RawEvent::Gap { .. }), "{gap:?}");

        // Reconnect until the server has read one subscribe frame, then cancel.
        {
            let fut = conn.next();
            tokio::pin!(fut);
            tokio::select! {
                _ = first_sub_rx.recv() => {}
                event = &mut fut => panic!("reconnect completed before the cancel: {event:?}"),
            }
        }
        round1_go.notify_one();

        // Recovery: the client must redial and resubscribe the full set.
        let opened = conn.next().await.unwrap();
        assert!(matches!(opened, RawEvent::Opened { .. }), "{opened:?}");
        let count = full_rx.recv().await.expect("the full subscribe set");
        assert_eq!(count, N, "the recovered socket was missing subscriptions");
    }

    /// A host that accepts then resets every connection must not terminate the
    /// stream: the resubscribe failure is retried like any other failure.
    #[tokio::test(start_paused = true)]
    async fn resubscribe_failure_on_reconnect_retries_instead_of_dying() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let connections = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let connections_task = connections.clone();
        tokio::spawn(async move {
            let mut round = 0usize;
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                connections_task.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if round == 0 {
                    // Healthy first connection: complete the handshake, read the
                    // subscribe, then let the client see a clean close.
                    let mut ws = accept_async(stream).await.unwrap();
                    let _ = ws.next().await;
                    ws.close(None).await.ok();
                    drop(ws);
                } else if round == 1 {
                    // Reset the connection before the client's resubscribe send
                    // can succeed, forcing a send error.
                    let ws = accept_async(stream).await.unwrap();
                    #[allow(deprecated)]
                    ws.get_ref().set_linger(Some(Duration::ZERO)).ok();
                    // Drop immediately to emit an RST.
                } else {
                    // Finally accept and hold a working connection.
                    let mut ws = accept_async(stream).await.unwrap();
                    while let Some(Ok(_)) = ws.next().await {}
                }
                round += 1;
            }
        });

        let mut conn = RawWsConn::connect_with_policy(
            protocol(format!("ws://{addr}")),
            vec!["sub1".to_string()],
            Duration::from_secs(3_600),
            Duration::from_secs(30),
            // Paused time: keep the dial deadline far future.
            ReconnectPolicy {
                dial_timeout: Duration::from_secs(3_600),
                ..ReconnectPolicy::default()
            },
        )
        .await
        .unwrap();
        // Drive until we see Opened after the gap; Err would mean death.
        let mut saw_opened = false;
        for _ in 0..10 {
            match conn.next().await {
                Ok(RawEvent::Opened { .. }) => {
                    saw_opened = true;
                    break;
                }
                Ok(_) => {}
                Err(err) => panic!("stream ended instead of retrying: {err}"),
            }
        }
        assert!(saw_opened, "did not recover after resubscribe failures");
        assert!(
            connections.load(std::sync::atomic::Ordering::SeqCst) >= 3,
            "expected the client to keep dialing"
        );
    }
}
