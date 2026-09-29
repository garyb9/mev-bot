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
/// Minimum backoff after jitter, so a host that accepts then immediately closes
/// cannot be retried in a busy loop (full jitter alone can roll ~0 ms).
pub const BACKOFF_MIN: Duration = Duration::from_millis(100);
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
        /// Process-monotonic nanoseconds ([`mono_ns`]) when the drop was
        /// **detected**, before the reconnect. Callers use it to bracket the
        /// real outage (the `Gap` is returned only after the reconnect has
        /// completed, so `mono_ns()` at return would understate the downtime).
        /// `0` when the gap is a clean shutdown.
        disconnect_ns: u64,
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
            attempt: 1,
            metrics_src: src,
            pending_opened: false,
        };
        conn.resubscribe().await?;
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
    ///
    /// The attempt counter is reset only after a **healthy** connection: at
    /// entry, if the previous connection had been up for [`HEALTHY_AFTER`],
    /// backoff restarts at the minimum. Otherwise it keeps growing, so a host
    /// that accepts and immediately drops cannot pin the backoff low.
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

            let socket = match dial(&self.protocol.url()).await {
                Ok(socket) => socket,
                Err(err) => {
                    tracing::warn!(attempt = self.attempt, error = %err, "websocket reconnect failed");
                    continue;
                }
            };
            self.socket = socket;
            let now = Instant::now();
            self.last_data = now;
            self.opened_at = now;
            // A resubscribe send failure means this connection is unusable:
            // treat it as another failed attempt and keep backing off, instead
            // of terminating the stream.
            if let Err(err) = self.resubscribe().await {
                tracing::warn!(
                    attempt = self.attempt,
                    error = %err,
                    "websocket resubscribe after reconnect failed"
                );
                continue;
            }
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
        loop {
            tokio::select! {
                _ = self.shutdown.notified() => {
                    return Ok(RawEvent::Gap {
                        reason: "shutdown".into(),
                        detail: "cancellation requested".into(),
                        disconnect_ns: 0,
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
    ///
    /// The `Gap` carries the [`mono_ns`] timestamp taken when the drop was
    /// detected, **before** the (possibly long) reconnect, so callers can
    /// bracket the real outage.
    async fn handle_gap(&mut self, reason: &str, detail: &str) -> Result<Option<RawEvent>> {
        metrics::counter!(
            names::WS_RECONNECTS,
            "src" => self.metrics_src,
            "reason" => reason.to_string(),
        )
        .increment(1);
        let disconnect_ns = mono_ns();
        let event = RawEvent::Gap {
            reason: reason.to_string(),
            detail: detail.to_string(),
            disconnect_ns,
        };
        self.reconnect().await?;
        self.pending_opened = true;
        tracing::debug!(reason, detail, "websocket gap; reconnected");
        Ok(Some(event))
    }
}

/// Full-jitter exponential backoff for `attempt` (1-based): a uniform sleep in
/// `[BACKOFF_MIN, min(base * 2^(attempt-1), max)]`.
///
/// The lower bound stops a host that accepts then immediately closes from being
/// retried in a tight loop (full jitter alone can roll ~0 ns).
fn jittered_backoff(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(6);
    let ceiling = BACKOFF_BASE.saturating_mul(1 << shift).min(BACKOFF_MAX);
    let floor = BACKOFF_MIN.min(ceiling);
    let span = ceiling.saturating_sub(floor);
    if span.is_zero() {
        return floor;
    }
    let width = span.as_nanos().min(u64::MAX as u128) as u64;
    let roll = pseudo_random();
    floor + Duration::from_nanos(roll % (width + 1))
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

async fn dial(url: &str) -> Result<Socket> {
    ensure_crypto_provider();
    let (socket, _resp) = connect_async(url)
        .await
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

        let conn = RawWsConn::connect_with(
            Box::new(KeepaliveProtocol {
                url: format!("ws://{addr}"),
            }),
            vec![],
            // Far-future watchdog isolates the ping cadence from idle logic.
            Duration::from_secs(3_600),
            Duration::from_secs(20),
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

    /// `Gap` carries the detection time, so the measured outage covers the
    /// reconnect even though the event is returned afterwards.
    #[tokio::test(start_paused = true)]
    async fn gap_carries_the_disconnect_time() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            // First connection: send a frame, close. Second: hold open.
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            ws.send(Message::Text("hello".into())).await.unwrap();
            ws.close(None).await.unwrap();
            drop(ws);
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            while let Some(Ok(_)) = ws.next().await {}
        });
        let mut conn = RawWsConn::connect_with(
            protocol(format!("ws://{addr}")),
            vec![],
            Duration::from_secs(3_600),
            Duration::from_secs(30),
        )
        .await
        .unwrap();
        // Consume the hello, then the gap.
        let _ = conn.next().await.unwrap();
        let before = mono_ns();
        let event = conn.next().await.unwrap();
        let after = mono_ns();
        match event {
            RawEvent::Gap {
                reason,
                disconnect_ns,
                ..
            } => {
                assert_eq!(reason, "closed");
                assert!(
                    disconnect_ns >= before && disconnect_ns <= after,
                    "disconnect_ns {disconnect_ns} not in [{before}, {after}]"
                );
            }
            other => panic!("expected a gap, got {other:?}"),
        }
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

        let mut conn = RawWsConn::connect_with(
            protocol(format!("ws://{addr}")),
            vec!["sub1".to_string()],
            Duration::from_secs(3_600),
            Duration::from_secs(30),
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
