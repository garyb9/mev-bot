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
use mev_core::{
    config::Network,
    error::{Error, Result},
};
use mev_metrics::names;
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
/// A connection is considered healthy after this long, resetting backoff.
pub const HEALTHY_AFTER: Duration = Duration::from_secs(60);

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
    /// shutdown. A reconnect follows `Gap` for everything but shutdown.
    Gap {
        /// Why the gap happened (`watchdog`, `closed`, `error`, `shutdown`).
        reason: String,
        /// Human-readable detail.
        detail: String,
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
    attempt: u32,
    metrics_src: &'static str,
    /// Set after a reconnect so the next `next()` yields `Opened`.
    pending_opened: bool,
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
        let socket = dial(&protocol.url()).await?;
        let src = protocol.name();
        let mut conn = Self {
            protocol,
            socket,
            subscriptions,
            watchdog,
            ping_interval,
            shutdown: Arc::new(Notify::new()),
            last_data: Instant::now(),
            opened_at: Instant::now(),
            attempt: 1,
            metrics_src: src,
            pending_opened: false,
        };
        conn.resubscribe().await?;
        metrics::counter!(names::WS_RECONNECTS, "src" => src, "reason" => "open").increment(1);
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

    async fn resubscribe(&mut self) -> Result<()> {
        let subs = self.subscriptions.clone();
        for sub in &subs {
            let frame = self.protocol.subscribe_frame(sub);
            self.socket
                .send(Message::Text(frame.into()))
                .await
                .map_err(|e| Error::Http(e.to_string()))?;
        }
        Ok(())
    }

    /// Dial with full-jitter backoff until a connection opens or shutdown is
    /// requested. Returns `Ok(())` on reconnect and `Err` on shutdown.
    async fn reconnect(&mut self) -> Result<()> {
        metrics::gauge!(names::WS_CONNECTED, "src" => self.metrics_src).set(0.0);
        // Reset the attempt counter after a healthy run.
        if self.opened_at.elapsed() >= HEALTHY_AFTER {
            self.attempt = 0;
        }
        loop {
            self.attempt = self.attempt.saturating_add(1);
            let backoff = jittered_backoff(self.attempt);
            tokio::select! {
                _ = self.shutdown.notified() => {
                    return Err(Error::Http("websocket shutdown requested".into()));
                }
                _ = tokio::time::sleep(backoff) => {}
            }

            match dial(&self.protocol.url()).await {
                Ok(socket) => {
                    self.socket = socket;
                    self.last_data = Instant::now();
                    self.opened_at = Instant::now();
                    self.resubscribe().await?;
                    metrics::counter!(
                        names::WS_RECONNECTS,
                        "src" => self.metrics_src,
                        "reason" => "reconnect",
                    )
                    .increment(1);
                    tracing::debug!(attempt = self.attempt, "websocket reconnected");
                    self.attempt = 1;
                    return Ok(());
                }
                Err(err) => {
                    tracing::warn!(attempt = self.attempt, error = %err, "websocket reconnect failed");
                }
            }
        }
    }

    /// The next raw event, transparently reconnecting after gaps.
    ///
    /// Yields [`RawEvent::Opened`] after a successful (re)connect and
    /// [`RawEvent::Gap`] when the feed breaks; on shutdown it returns `Err`.
    pub async fn next(&mut self) -> Result<RawEvent> {
        if self.pending_opened {
            self.pending_opened = false;
            return Ok(RawEvent::Opened {
                attempt: self.attempt,
            });
        }
        let mut ping =
            tokio::time::interval_at(Instant::now() + self.ping_interval, self.ping_interval);
        loop {
            tokio::select! {
                _ = self.shutdown.notified() => {
                    return Ok(RawEvent::Gap {
                        reason: "shutdown".into(),
                        detail: "cancellation requested".into(),
                    });
                }
                _ = ping.tick() => {
                    if let Some(frame) = self.protocol.keepalive_frame()
                        && self.socket.send(Message::Text(frame.into())).await.is_err()
                    {
                        if let Some(event) = self.handle_gap("error", "keepalive send failed").await? {
                            return Ok(event);
                        }
                        continue;
                    }
                }
                _ = tokio::time::sleep_until(self.last_data + self.watchdog) => {
                    if let Some(event) = self
                        .handle_gap(
                            "watchdog",
                            &format!("no inbound frame for {}s", self.watchdog.as_secs()),
                        )
                        .await?
                    {
                        return Ok(event);
                    }
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
                            if let Some(event) = self.handle_gap("closed", "server closed").await? {
                                return Ok(event);
                            }
                            continue;
                        }
                        Some(Ok(Message::Frame(_))) => {}
                        Some(Err(err)) => {
                            if let Some(event) = self.handle_gap("error", &err.to_string()).await? {
                                return Ok(event);
                            }
                            continue;
                        }
                        None => {
                            if let Some(event) = self.handle_gap("closed", "stream ended").await? {
                                return Ok(event);
                            }
                            continue;
                        }
                    }
                }
            }
        }
    }

    /// Emit a `Gap`, reconnect, and arrange for the next `next()` to yield
    /// `Opened`. On shutdown (cancellation during backoff) the gap is still
    /// returned and the caller sees the shutdown on its next call.
    async fn handle_gap(&mut self, reason: &str, detail: &str) -> Result<Option<RawEvent>> {
        metrics::counter!(
            names::WS_RECONNECTS,
            "src" => self.metrics_src,
            "reason" => reason.to_string(),
        )
        .increment(1);
        let event = RawEvent::Gap {
            reason: reason.to_string(),
            detail: detail.to_string(),
        };
        self.reconnect().await?;
        self.pending_opened = true;
        tracing::debug!(reason, detail, "websocket gap; reconnected");
        Ok(Some(event))
    }
}

/// Full-jitter exponential backoff for `attempt` (1-based): a uniform sleep in
/// `[0, min(base * 2^(attempt-1), max)]`.
fn jittered_backoff(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(6);
    let ceiling = BACKOFF_BASE.saturating_mul(1 << shift).min(BACKOFF_MAX);
    let width = ceiling.as_nanos().min(u64::MAX as u128) as u64;
    if width == 0 {
        return Duration::ZERO;
    }
    let roll = pseudo_random();
    Duration::from_nanos(roll % (width + 1))
}

/// A cheap, non-cryptographic random source for jitter. Randomness here only
/// needs to spread reconnect storms, not be unpredictable.
fn pseudo_random() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(mono_ns());
    hasher.finish()
}

async fn dial(url: &str) -> Result<Socket> {
    ensure_crypto_provider();
    let (socket, _resp) = connect_async(url)
        .await
        .map_err(|e| Error::Http(e.to_string()))?;
    Ok(socket)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn mono_ns() -> u64 {
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
    fn backoff_is_capped_and_jittered() {
        for attempt in 1..20 {
            let d = jittered_backoff(attempt);
            assert!(d <= BACKOFF_MAX, "attempt {attempt}: {d:?}");
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
}
