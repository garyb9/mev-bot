//! WebSocket `post` transport (SPEC-0002 §8, default).
//!
//! Anything postable over HTTP is postable here: the signed envelope travels as
//! `{"method":"post","id":N,"request":{"type":"action","payload":{...}}}` and
//! the venue replies on `{"channel":"post","data":{"id":N,"response":{...}}}`.
//!
//! Posts are **concurrent** (SPEC-0002 H-1): one task owns the socket and
//! drains an outbound channel; a reader task correlates each reply by its `id`
//! into a pending map of one-shot channels, so many orders can be in flight at
//! once and a slow reply never serializes the others. In-flight posts are
//! capped at [`MAX_IN_FLIGHT`] and each request has a timeout that resolves to
//! [`Error::UnknownOutcome`]; a dropped socket also fails every pending request
//! with `UnknownOutcome` so the caller reconciles instead of resending.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use mev_core::{
    config::{Mode, Network},
    db::Db,
    error::{Error, Result},
};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

use crate::exchange::{
    ActionResponse, ExchangeApi, ExchangeRequest, Prepared, ReplyHandle, WriteCore, WriteGate,
    parse_post_reply,
};
use crate::order::Action;
use crate::raw_ws::set_tcp_nodelay;
use crate::ws::ensure_crypto_provider;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Venue limit on simultaneous in-flight WS posts (SPEC-0002 §10).
pub const MAX_IN_FLIGHT: usize = 100;
/// Default per-request reply timeout (SPEC-0002 H-1).
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// Application keepalive ping interval.
const PING_INTERVAL: Duration = Duration::from_secs(30);

/// Outgoing `post` frame. `payload` is a typed [`ExchangeRequest`] (not a
/// `Value`) so the action's JSON field order matches its msgpack hash.
#[derive(Serialize)]
struct PostFrame<'a> {
    method: &'static str,
    id: u64,
    request: PostBody<'a>,
}

#[derive(Serialize)]
struct PostBody<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    payload: &'a ExchangeRequest,
}

/// A reply routed back from the reader task; `None` means the socket was lost.
type PendingMap = Arc<Mutex<HashMap<u64, oneshot::Sender<Option<Reply>>>>>;

/// A venue reply plus the instant it was decoded, so `hl_submit_ack_seconds`
/// measures the transport and not when the exec writer happened to poll the
/// handle (SPEC-0002 H-7).
type Reply = (Value, Instant);

/// One outbound frame, the monotonic read time of the market frame it was
/// produced for, and a one-shot that resolves with the instant the socket write
/// completed. `recv_mono_ns` lets the socket task stamp
/// `hl_tick_to_order_seconds` after the write; `written_tx` lets the reply timer
/// start at the write rather than before dial/enqueue (SPEC-0002 H-7). `0`
/// means the caller did not provide a read time.
struct Outbound {
    message: Message,
    recv_mono_ns: u64,
    written_tx: oneshot::Sender<Instant>,
}

/// WebSocket `post` transport implementing [`ExchangeApi`].
pub struct WsExchange {
    url: String,
    core: WriteCore,
    connection: tokio::sync::Mutex<Option<Connection>>,
    next_id: AtomicU64,
    in_flight: Arc<Semaphore>,
    request_timeout: Duration,
    /// Cached `hl_submit_ack_seconds{transport="ws"}` handle (SPEC-0002 H-7).
    submit_histogram: metrics::Histogram,
    /// Cached `hl_tick_to_order_seconds` handle, handed to the socket task
    /// (SPEC-0002 H-7).
    tick_to_order: metrics::Histogram,
    /// Cached `hl_tick_to_order_skipped_total{reason="unset"}` counter.
    tick_skipped_unset: metrics::Counter,
    /// Cached `hl_tick_to_order_skipped_total{reason="clock"}` counter.
    tick_skipped_clock: metrics::Counter,
}

/// One live socket: the writer task, the reader task, and the pending map.
struct Connection {
    tx: mpsc::Sender<Outbound>,
    pending: PendingMap,
    task: JoinHandle<()>,
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl WsExchange {
    /// Create a client for the given network and mode.
    pub fn new(
        network: Network,
        mode: Mode,
        signer: Option<crate::signing::AgentSigner>,
    ) -> Result<Self> {
        Self::with_url(network.ws_url(), mode, signer)
    }

    /// Create a client against an explicit URL (tests).
    pub fn with_url(
        url: impl Into<String>,
        mode: Mode,
        signer: Option<crate::signing::AgentSigner>,
    ) -> Result<Self> {
        Ok(Self {
            url: url.into(),
            core: WriteCore::new(mode, signer)?,
            connection: tokio::sync::Mutex::new(None),
            next_id: AtomicU64::new(1),
            in_flight: Arc::new(Semaphore::new(MAX_IN_FLIGHT)),
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            submit_histogram: metrics::histogram!(
                mev_metrics::names::SUBMIT_ACK_SECONDS,
                "transport" => "ws"
            ),
            tick_to_order: metrics::histogram!(mev_metrics::names::TICK_TO_ORDER_SECONDS),
            tick_skipped_unset: metrics::counter!(
                mev_metrics::names::TICK_TO_ORDER_SKIPPED_TOTAL,
                "reason" => mev_metrics::names::TICK_TO_ORDER_SKIP_UNSET
            ),
            tick_skipped_clock: metrics::counter!(
                mev_metrics::names::TICK_TO_ORDER_SKIPPED_TOTAL,
                "reason" => mev_metrics::names::TICK_TO_ORDER_SKIP_CLOCK
            ),
        })
    }

    /// Override the per-request timeout (default 5 s).
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// Attach a durable nonce store and restore the persisted high-water mark.
    pub fn with_nonce_db(mut self, db: Arc<Mutex<Db>>) -> Result<Self> {
        self.core = self.core.with_nonce_db(db)?;
        Ok(self)
    }

    /// The effective write gate.
    pub fn gate(&self) -> WriteGate {
        self.core.gate()
    }

    /// Set the optional `expiresAfter` field applied to every action.
    pub fn with_expires_after(mut self, expires_after: Option<u64>) -> Self {
        self.core = self.core.with_expires_after(expires_after);
        self
    }

    /// Set the optional vault address applied to every action.
    pub fn with_vault_address(mut self, vault_address: Option<String>) -> Self {
        self.core = self.core.with_vault_address(vault_address);
        self
    }

    /// Restore the persisted nonce high-water mark (operator path).
    pub async fn restore_nonce(&self, last: u64) -> Result<()> {
        self.core.restore_nonce(last).await
    }

    /// Reset a corrupt persisted nonce (operator path).
    pub async fn reset_nonce(&self) -> Result<()> {
        self.core.reset_nonce().await
    }

    /// The current nonce high-water mark (for persistence).
    pub async fn last_nonce(&self) -> u64 {
        self.core.last_nonce().await
    }

    /// Resync the nonce after a stale/duplicate/recent-window rejection.
    pub async fn heal_nonce(&self) -> u64 {
        self.core.heal_nonce().await
    }

    async fn dial(url: &str) -> Result<Socket> {
        ensure_crypto_provider();
        let (socket, _resp) = connect_async(url)
            .await
            .map_err(|e| Error::NotSent(format!("websocket dial failed: {e}")))?;
        set_tcp_nodelay(socket.get_ref())?;
        Ok(socket)
    }

    /// Open the exec connection now if it is not already open (SPEC-0010 §12:
    /// connections are warmed at startup, never on demand). Does nothing else.
    pub async fn warm(&self) -> Result<()> {
        self.ensure_connection().await.map(|_| ())
    }

    /// Whether a live connection is currently held, without blocking on a dial
    /// in progress (cheap, best-effort check).
    pub fn is_connected(&self) -> bool {
        self.connection
            .try_lock()
            .map(|guard| guard.is_some())
            .unwrap_or(false)
    }

    /// Return the outbound sender, dialing and spawning the connection task on
    /// first use. Shared by [`Self::warm`] and [`Self::register_pending`].
    async fn ensure_connection(&self) -> Result<mpsc::Sender<Outbound>> {
        let mut guard = self.connection.lock().await;
        if guard.is_none() {
            let socket = Self::dial(&self.url).await?;
            let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
            let (send_tx, rx) = mpsc::channel::<Outbound>(MAX_IN_FLIGHT);
            let task = tokio::spawn(Self::connection_task(
                socket,
                rx,
                pending.clone(),
                self.tick_to_order.clone(),
                self.tick_skipped_unset.clone(),
                self.tick_skipped_clock.clone(),
            ));
            *guard = Some(Connection {
                tx: send_tx,
                pending,
                task,
            });
        }
        Ok(guard.as_ref().expect("connection present").tx.clone())
    }

    /// Serialize and enqueue `request`, returning the reply waiter.
    ///
    /// The frame is handed to the socket-owning task before this returns, so
    /// calls in sequence preserve write order (SPEC-0010 §12). The caller
    /// awaits the receiver separately, keeping only the wait concurrent.
    /// `recv_mono_ns` is the market frame's monotonic read time (SPEC-0002 H-7).
    /// The returned write-time receiver resolves with the instant the socket
    /// write completed, where `hl_submit_ack_seconds` starts.
    async fn post_enqueue(
        &self,
        request: &ExchangeRequest,
        recv_mono_ns: u64,
    ) -> Result<(
        u64,
        PendingMap,
        oneshot::Receiver<Option<Reply>>,
        oneshot::Receiver<Instant>,
    )> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let frame = serde_json::to_string(&PostFrame {
            method: "post",
            id,
            request: PostBody {
                kind: "action",
                payload: request,
            },
        })
        .map_err(|e| Error::NotSent(format!("request serialisation failed: {e}")))?;

        let (tx, rx) = oneshot::channel();
        let (written_tx, written_rx) = oneshot::channel();
        let (send_tx, pending) = self.register_pending(id, tx).await?;
        let outbound = Outbound {
            message: Message::Text(frame.into()),
            recv_mono_ns,
            written_tx,
        };
        if send_tx.send(outbound).await.is_err() {
            self.remove_pending(id);
            // The frame never entered the writer queue, so it was not sent.
            return Err(Error::NotSent("websocket send failed".into()));
        }
        Ok((id, pending, rx, written_rx))
    }

    /// Sign, enqueue, and return a handle for the reply, carrying no market
    /// read time (see [`Self::enqueue_split_timed`]).
    async fn enqueue_split(&self, action: &Action) -> Result<ReplyHandle> {
        self.enqueue_split_timed(action, 0).await
    }

    /// Sign, enqueue, and return a handle for the reply (the split exec API).
    ///
    /// The in-flight permit is moved into the reply future, so it is released
    /// when the reply resolves (or is abandoned), not when the frame is queued.
    /// `recv_mono_ns` travels with the frame so the socket task can stamp
    /// `hl_tick_to_order_seconds` after the write (SPEC-0002 H-7).
    async fn enqueue_split_timed(&self, action: &Action, recv_mono_ns: u64) -> Result<ReplyHandle> {
        let request = match self.core.prepare(action).await? {
            Prepared::DryRun(request) => {
                return Ok(ReplyHandle::ready(Ok(ActionResponse {
                    value: json!({ "status": "simulated", "request": request }),
                })));
            }
            Prepared::Send(request) => request,
        };

        // Cap simultaneous posts at the venue limit. Held until the reply.
        let permit = self
            .in_flight
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| Error::NotSent("websocket closed before send".into()))?;

        let (id, pending, rx, written_rx) = self.post_enqueue(&request, recv_mono_ns).await?;
        let histogram = self.submit_histogram.clone();
        let timeout = self.request_timeout;
        Ok(ReplyHandle::new(async move {
            let _permit = permit;
            let value = match tokio::time::timeout(timeout, rx).await {
                Ok(Ok(Some((response, ack_at)))) => {
                    // Submit-to-ack starts at the socket write, not at the
                    // enqueue: the write instant comes back from the socket
                    // task, so a cold dial or a busy writer channel is excluded
                    // (SPEC-0002 H-7, `transport="ws"`). The ack instant itself
                    // is stamped by the reader, so a delayed poll cannot inflate
                    // it. A timeout is not an ack.
                    if let Ok(written_at) = written_rx.await {
                        histogram
                            .record(ack_at.saturating_duration_since(written_at).as_secs_f64());
                    }
                    response
                }
                Ok(Ok(None)) => {
                    return Err(Error::UnknownOutcome(
                        "websocket dropped before reply".into(),
                    ));
                }
                Ok(Err(_)) => {
                    return Err(Error::UnknownOutcome(
                        "websocket reply channel closed".into(),
                    ));
                }
                Err(_) => {
                    pending
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .remove(&id);
                    return Err(Error::UnknownOutcome(format!(
                        "no reply within {timeout:?}"
                    )));
                }
            };
            parse_post_reply(value)
        }))
    }

    /// Register the waiter for `id`, dialing lazily if there is no connection.
    ///
    /// Returns the socket sender and a clone of the pending map so a timed-out
    /// await can drop its own waiter.
    async fn register_pending(
        &self,
        id: u64,
        tx: oneshot::Sender<Option<Reply>>,
    ) -> Result<(mpsc::Sender<Outbound>, PendingMap)> {
        let send_tx = self.ensure_connection().await?;
        let guard = self.connection.lock().await;
        if let Some(conn) = guard.as_ref() {
            conn.pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(id, tx);
        }
        let pending = guard
            .as_ref()
            .map(|conn| conn.pending.clone())
            .unwrap_or_else(|| Arc::new(Mutex::new(HashMap::new())));
        Ok((send_tx, pending))
    }

    /// Drop a waiter we no longer care about (e.g. after a timeout).
    fn remove_pending(&self, id: u64) {
        if let Ok(guard) = self.connection.try_lock()
            && let Some(conn) = guard.as_ref()
        {
            conn.pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&id);
        }
    }

    /// Own the socket: write outbound frames, read replies, answer pings, and
    /// on socket loss fail every pending request with `None` (UnknownOutcome).
    async fn connection_task(
        mut socket: Socket,
        mut rx: mpsc::Receiver<Outbound>,
        pending: PendingMap,
        tick_to_order: metrics::Histogram,
        skipped_unset: metrics::Counter,
        skipped_clock: metrics::Counter,
    ) {
        let mut ping = tokio::time::interval(PING_INTERVAL);
        ping.tick().await;
        loop {
            tokio::select! {
                outbound = rx.recv() => {
                    let Some(Outbound { message, recv_mono_ns, written_tx }) = outbound else { break };
                    if socket.send(message).await.is_err() {
                        break;
                    }
                    // Tell the submit-ack timer when the write completed, and
                    // stamp the headline span from the market frame's read time
                    // (SPEC-0002 H-7). An unset read time or a stamp not before
                    // now is counted with its reason rather than recorded.
                    let now = Instant::now();
                    let _ = written_tx.send(now);
                    if recv_mono_ns != 0 {
                        let elapsed = crate::raw_ws::mono_ns().saturating_sub(recv_mono_ns);
                        if elapsed > 0 {
                            tick_to_order.record(elapsed as f64 / 1e9);
                        } else {
                            skipped_clock.increment(1);
                        }
                    } else {
                        skipped_unset.increment(1);
                    }
                }
                _ = ping.tick() => {
                    if socket.send(Message::Ping(Vec::new().into())).await.is_err() {
                        break;
                    }
                }
                inbound = socket.next() => {
                    let Some(Ok(message)) = inbound else { break };
                    match message {
                        Message::Text(text) => {
                            let Ok(frame) = serde_json::from_str::<Value>(&text) else { continue };
                            if frame.get("channel").and_then(Value::as_str) != Some("post") {
                                continue;
                            }
                            let data = &frame["data"];
                            let Some(id) = data.get("id").and_then(Value::as_u64) else { continue };
                            let response = data.get("response").cloned().unwrap_or(Value::Null);
                            let sender = pending
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner())
                                .remove(&id);
                            if let Some(sender) = sender {
                                // Stamp the ack where it actually arrives, not
                                // when the exec writer next polls (SPEC-0002 H-7).
                                let _ = sender.send(Some((response, Instant::now())));
                            }
                        }
                        Message::Ping(payload) => {
                            if socket.send(Message::Pong(payload)).await.is_err() {
                                break;
                            }
                        }
                        Message::Close(_) => break,
                        Message::Pong(_) | Message::Binary(_) | Message::Frame(_) => {}
                    }
                }
            }
        }
        // Socket gone: fail every waiter.
        let waiters: Vec<_> = pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .drain()
            .map(|(_, sender)| sender)
            .collect();
        for sender in waiters {
            let _ = sender.send(None);
        }
    }

    /// Build, sign, enqueue, and await one action's reply.
    async fn send(&self, action: &Action) -> Result<ActionResponse> {
        self.enqueue_split(action).await?.wait().await
    }
}

#[async_trait]
impl ExchangeApi for WsExchange {
    async fn submit(&self, action: &Action) -> Result<ActionResponse> {
        self.send(action).await
    }

    async fn enqueue(&self, action: &Action) -> Result<ReplyHandle> {
        self.enqueue_split(action).await
    }

    async fn enqueue_timed(&self, action: &Action, recv_mono_ns: u64) -> Result<ReplyHandle> {
        self.enqueue_split_timed(action, recv_mono_ns).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::order::{Grouping, limit_order};
    use crate::signing::AgentSigner;
    use crate::test_metrics::{counter_value, histogram_samples, histogram_values};
    use metrics_util::debugging::DebuggingRecorder;
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;

    const KEY: &str = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";

    fn signer() -> AgentSigner {
        AgentSigner::from_hex(KEY, true).unwrap()
    }

    fn action() -> Action {
        Action::Order {
            orders: vec![limit_order(
                0,
                true,
                "50000",
                "0.1",
                crate::order::Tif::Gtc,
                false,
                None,
            )],
            grouping: Grouping::Na,
        }
    }

    /// Start a one-connection mock venue that replies to each `post` with
    /// `reply(request)` wrapped in a `post` channel envelope.
    async fn mock_venue<F>(reply: F) -> String
    where
        F: Fn(&Value) -> Value + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            while let Some(Ok(message)) = ws.next().await {
                if let Message::Text(text) = message {
                    let request: Value = serde_json::from_str(&text).unwrap();
                    let id = request["id"].as_u64().unwrap();
                    let frame = json!({
                        "channel": "post",
                        "data": { "id": id, "response": reply(&request) },
                    })
                    .to_string();
                    ws.send(Message::Text(frame.into())).await.unwrap();
                }
            }
        });
        format!("ws://{addr}")
    }

    /// A mock venue whose WebSocket handshake is delayed by `delay` after the
    /// TCP accept, so a cold dial is slow; posts are then answered promptly.
    async fn slow_handshake_venue(delay: Duration) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            tokio::time::sleep(delay).await;
            let mut ws = accept_async(stream).await.unwrap();
            while let Some(Ok(message)) = ws.next().await {
                if let Message::Text(text) = message {
                    let request: Value = serde_json::from_str(&text).unwrap();
                    let id = request["id"].as_u64().unwrap();
                    let frame = json!({
                        "channel": "post",
                        "data": {
                            "id": id,
                            "response": {
                                "type": "action",
                                "payload": {"status": "ok", "response": {"n": 1}},
                            },
                        },
                    })
                    .to_string();
                    ws.send(Message::Text(frame.into())).await.unwrap();
                }
            }
        });
        format!("ws://{addr}")
    }

    /// A mock venue that holds every post until its client socket is dropped,
    /// then drops the socket without replying. Replies are shuffled: the `id`
    /// echoed back is a marker so the caller can prove each response is routed
    /// to the right request.
    async fn black_hole_venue() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            // Read and discard; hold the socket until the peer closes.
            let mut ws = accept_async(stream).await.unwrap();
            while let Some(Ok(_)) = ws.next().await {}
        });
        format!("ws://{addr}")
    }

    /// A mock venue that answers each request with the request's own `id`
    /// echoed back, after a per-request delay, and shuffles the order.
    async fn echo_venue(delay_ms: u64) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            let mut pending: Vec<(u64, u64)> = Vec::new(); // (id, reply_at_ms)
            let start = std::time::Instant::now();
            loop {
                let next_reply = pending.iter().map(|(_, at)| *at).min().unwrap_or(u64::MAX);
                let now = start.elapsed().as_millis() as u64;
                let wait = next_reply.saturating_sub(now);
                tokio::select! {
                    message = ws.next() => {
                        let Some(Ok(Message::Text(text))) = message else { break };
                        let request: Value = serde_json::from_str(&text).unwrap();
                        let id = request["id"].as_u64().unwrap();
                        pending.push((id, now + delay_ms));
                    }
                    _ = tokio::time::sleep(Duration::from_millis(wait.clamp(1, 50))), if !pending.is_empty() => {
                        let now = start.elapsed().as_millis() as u64;
                        let due: Vec<u64> = pending
                            .iter()
                            .filter(|(_, at)| *at <= now)
                            .map(|(id, _)| *id)
                            .collect();
                        pending.retain(|(_, at)| *at > now);
                        for id in due {
                            let frame = json!({
                                "channel": "post",
                                "data": {
                                    "id": id,
                                    "response": {
                                        "type": "action",
                                        "payload": {
                                            "status": "ok",
                                            "response": {"echo": id},
                                        },
                                    },
                                },
                            })
                            .to_string();
                            ws.send(Message::Text(frame.into())).await.unwrap();
                        }
                    }
                }
            }
        });
        format!("ws://{addr}")
    }

    #[tokio::test]
    async fn posts_action_and_parses_order_statuses() {
        let url = mock_venue(|request| {
            assert_eq!(request["method"], "post");
            assert_eq!(request["request"]["type"], "action");
            assert!(request["request"]["payload"]["signature"]["r"].is_string());
            json!({
                "type": "action",
                "payload": {
                    "status": "ok",
                    "response": {"data": {"statuses": ["resting"]}},
                },
            })
        })
        .await;

        let exchange = WsExchange::with_url(url, Mode::Live, Some(signer())).unwrap();
        let response = exchange.submit(&action()).await.unwrap();
        assert_eq!(
            response.order_response().unwrap().statuses,
            vec![crate::exchange::OrderStatus::Resting]
        );
    }

    #[tokio::test]
    async fn maps_action_error_status() {
        let url = mock_venue(|_| {
            json!({
                "type": "action",
                "payload": {"status": "err", "response": "Must deposit before trading."},
            })
        })
        .await;

        let exchange = WsExchange::with_url(url, Mode::Live, Some(signer())).unwrap();
        match exchange.submit(&action()).await.unwrap_err() {
            Error::Exchange(message) => assert!(message.contains("Must deposit")),
            other => panic!("expected exchange error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn maps_transport_error_payload() {
        let url = mock_venue(|_| json!({"type": "error", "payload": "rate limited"})).await;
        let exchange = WsExchange::with_url(url, Mode::Live, Some(signer())).unwrap();
        match exchange.submit(&action()).await.unwrap_err() {
            Error::Exchange(message) => assert!(message.contains("rate limited")),
            other => panic!("expected exchange error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn correlates_ids_across_sequential_posts() {
        let url = mock_venue(
            |_| json!({"type": "action", "payload": {"status": "ok", "response": {"n": 1}}}),
        )
        .await;
        let exchange = WsExchange::with_url(url, Mode::Live, Some(signer())).unwrap();
        for _ in 0..3 {
            exchange.submit(&action()).await.unwrap();
        }
    }

    #[tokio::test]
    async fn fifty_concurrent_posts_each_get_their_own_reply() {
        let url = echo_venue(20).await;
        let exchange = Arc::new(
            WsExchange::with_url(url, Mode::Live, Some(signer()))
                .unwrap()
                .with_request_timeout(Duration::from_secs(5)),
        );

        let mut tasks = Vec::new();
        for _ in 0..50 {
            let exchange = exchange.clone();
            tasks.push(tokio::spawn(async move {
                let response = exchange.submit(&action()).await.unwrap();
                response.value["echo"].as_u64().unwrap()
            }));
        }
        let mut ids = Vec::new();
        for task in tasks {
            ids.push(task.await.unwrap());
        }
        assert_eq!(ids.len(), 50);
        // Every reply is unique, i.e. no two callers got the same response.
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 50, "each caller must get its own reply");
    }

    #[tokio::test]
    async fn dropped_socket_fails_pending_with_unknown_outcome() {
        // No server: connect fails, so register_pending dials and errors before
        // creating a waiter; use a server that accepts then goes silent and
        // closes, exercising the reader's fail-all path.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let ws = accept_async(stream).await.unwrap();
            // Drop the socket immediately after the handshake.
            drop(ws);
        });
        let exchange = WsExchange::with_url(format!("ws://{addr}"), Mode::Live, Some(signer()))
            .unwrap()
            .with_request_timeout(Duration::from_secs(2));
        match exchange.submit(&action()).await.unwrap_err() {
            Error::UnknownOutcome(message) => assert!(!message.is_empty()),
            other => panic!("expected UnknownOutcome, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn timeout_returns_unknown_outcome() {
        let url = black_hole_venue().await;
        let exchange = WsExchange::with_url(url, Mode::Live, Some(signer()))
            .unwrap()
            .with_request_timeout(Duration::from_millis(150));
        match exchange.submit(&action()).await.unwrap_err() {
            Error::UnknownOutcome(message) => assert!(message.contains("no reply"), "{message}"),
            other => panic!("expected UnknownOutcome, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_slow_post_does_not_block_a_fast_one() {
        // The first post is parked; the second must still get a reply well
        // before the first returns. This is the H-1 property that a dead-man
        // refresh can no longer serialize order sends.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            let mut ids = Vec::new();
            while let Some(Ok(Message::Text(text))) = ws.next().await {
                let request: Value = serde_json::from_str(&text).unwrap();
                ids.push(request["id"].as_u64().unwrap());
                if ids.len() == 2 {
                    // Park the first, release the second immediately.
                    let parked = ids[0];
                    let fast = ids[1];
                    let frame = json!({
                        "channel": "post",
                        "data": { "id": fast, "response": {
                            "type": "action", "payload": {"status": "ok", "response": {"echo": fast}},
                        }},
                    })
                    .to_string();
                    ws.send(Message::Text(frame.into())).await.unwrap();
                    tokio::time::sleep(Duration::from_millis(400)).await;
                    let frame = json!({
                        "channel": "post",
                        "data": { "id": parked, "response": {
                            "type": "action", "payload": {"status": "ok", "response": {"echo": parked}},
                        }},
                    })
                    .to_string();
                    ws.send(Message::Text(frame.into())).await.unwrap();
                    break;
                }
            }
        });
        let exchange = Arc::new(
            WsExchange::with_url(format!("ws://{addr}"), Mode::Live, Some(signer()))
                .unwrap()
                .with_request_timeout(Duration::from_secs(5)),
        );

        let slow = {
            let exchange = exchange.clone();
            tokio::spawn(async move { exchange.submit(&action()).await })
        };
        // Let the first post reach the venue before the second.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let started = std::time::Instant::now();
        let fast = exchange.submit(&action()).await.unwrap();
        assert!(fast.value["echo"].is_number());
        assert!(
            started.elapsed() < Duration::from_millis(300),
            "fast post waited {:?}",
            started.elapsed()
        );
        // The parked post still resolves.
        assert!(slow.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn in_flight_cap_is_enforced() {
        // A permit-limited semaphore of MAX_IN_FLIGHT means the 101st post
        // waits; use a small timeout so it still resolves.
        let url = black_hole_venue().await;
        let exchange = Arc::new(
            WsExchange::with_url(url, Mode::Live, Some(signer()))
                .unwrap()
                .with_request_timeout(Duration::from_millis(50)),
        );
        let mut tasks = Vec::new();
        for _ in 0..(MAX_IN_FLIGHT + 5) {
            let exchange = exchange.clone();
            tasks.push(tokio::spawn(
                async move { exchange.submit(&action()).await },
            ));
        }
        let mut unknown = 0;
        for task in tasks {
            if matches!(task.await.unwrap(), Err(Error::UnknownOutcome(_))) {
                unknown += 1;
            }
        }
        assert_eq!(unknown, MAX_IN_FLIGHT + 5);
    }

    #[tokio::test]
    async fn observe_never_dials() {
        // No server on this port: observe must fail before any connection.
        let exchange = WsExchange::with_url("ws://127.0.0.1:1", Mode::Observe, None).unwrap();
        assert_eq!(exchange.gate(), WriteGate::Blocked);
        assert!(matches!(
            exchange.submit(&action()).await.unwrap_err(),
            Error::Config(_)
        ));
    }

    #[tokio::test]
    async fn simulate_signs_but_never_dials() {
        let exchange =
            WsExchange::with_url("ws://127.0.0.1:1", Mode::Simulate, Some(signer())).unwrap();
        assert_eq!(exchange.gate(), WriteGate::DryRun);
        let response = exchange.submit(&action()).await.unwrap();
        assert_eq!(response.value["status"], "simulated");
        assert!(response.value["request"]["signature"]["r"].is_string());
    }

    #[tokio::test]
    async fn nonce_persists_across_instances() {
        let url = mock_venue(
            |_| json!({"type": "action", "payload": {"status": "ok", "response": null}}),
        )
        .await;
        let db = Arc::new(Mutex::new(Db::open_in_memory().unwrap()));
        let exchange = WsExchange::with_url(url.clone(), Mode::Live, Some(signer()))
            .unwrap()
            .with_nonce_db(db.clone())
            .unwrap();
        exchange.submit(&action()).await.unwrap();
        let first = exchange.last_nonce().await;
        // The durable value is the write-ahead lease, so it covers the send.
        let persisted = db.lock().unwrap().nonce_last().unwrap().unwrap();
        assert!(persisted > first, "{persisted} must cover {first}");

        let restarted = WsExchange::with_url(url, Mode::Live, Some(signer()))
            .unwrap()
            .with_nonce_db(db.clone())
            .unwrap();
        assert!(restarted.last_nonce().await >= persisted);
    }

    #[tokio::test]
    async fn warm_opens_the_socket_eagerly() {
        let url = mock_venue(
            |_| json!({"type": "action", "payload": {"status": "ok", "response": null}}),
        )
        .await;
        let exchange = WsExchange::with_url(url, Mode::Live, Some(signer())).unwrap();
        assert!(!exchange.is_connected());
        exchange.warm().await.unwrap();
        assert!(exchange.is_connected());
        // The warmed socket is the one the first order uses.
        exchange.submit(&action()).await.unwrap();
    }

    #[tokio::test]
    async fn warm_fails_fast_on_refused_connection() {
        // No listener on port 1: the dial is refused, not hung.
        let exchange =
            WsExchange::with_url("ws://127.0.0.1:1", Mode::Live, Some(signer())).unwrap();
        match tokio::time::timeout(Duration::from_secs(5), exchange.warm()).await {
            Ok(Err(Error::NotSent(_))) => {}
            Ok(other) => panic!("expected NotSent error, got {other:?}"),
            Err(_) => panic!("warm hung instead of failing fast"),
        }
        assert!(!exchange.is_connected());
    }

    #[tokio::test]
    async fn submit_records_the_submit_ack_histogram() {
        let url = mock_venue(
            |_| json!({"type": "action", "payload": {"status": "ok", "response": {"n": 1}}}),
        )
        .await;
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        // Construct inside the local recorder so the cached handle binds to it.
        let exchange = metrics::with_local_recorder(&recorder, || {
            WsExchange::with_url(url, Mode::Live, Some(signer())).unwrap()
        });
        exchange.submit(&action()).await.unwrap();
        assert_eq!(
            histogram_samples(
                snapshotter.snapshot(),
                mev_metrics::names::SUBMIT_ACK_SECONDS,
                Some(("transport", "ws")),
            ),
            1,
        );
    }

    #[tokio::test]
    async fn socket_write_records_the_tick_to_order_histogram() {
        let url = mock_venue(
            |_| json!({"type": "action", "payload": {"status": "ok", "response": {"n": 1}}}),
        )
        .await;
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let exchange = metrics::with_local_recorder(&recorder, || {
            WsExchange::with_url(url, Mode::Live, Some(signer())).unwrap()
        });

        let recv_mono_ns = crate::raw_ws::mono_ns();
        exchange
            .enqueue_timed(&action(), recv_mono_ns)
            .await
            .unwrap()
            .wait()
            .await
            .unwrap();

        assert_eq!(
            histogram_samples(
                snapshotter.snapshot(),
                mev_metrics::names::TICK_TO_ORDER_SECONDS,
                None,
            ),
            1,
            "a frame written to the socket must be timed end to end"
        );
    }

    #[tokio::test]
    async fn skipped_tick_to_order_samples_are_counted_by_reason() {
        let url = mock_venue(
            |_| json!({"type": "action", "payload": {"status": "ok", "response": {"n": 1}}}),
        )
        .await;
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let exchange = metrics::with_local_recorder(&recorder, || {
            WsExchange::with_url(url, Mode::Live, Some(signer())).unwrap()
        });

        // `submit` carries no read time, so the frame cannot be timed.
        exchange.submit(&action()).await.unwrap();
        // A stamp in the future cannot be before "now": a foreign clock.
        exchange
            .enqueue_timed(&action(), crate::raw_ws::mono_ns() + 1_000_000_000)
            .await
            .unwrap()
            .wait()
            .await
            .unwrap();

        let snapshot = snapshotter.snapshot();
        assert_eq!(
            counter_value(
                snapshot,
                mev_metrics::names::TICK_TO_ORDER_SKIPPED_TOTAL,
                Some(("reason", mev_metrics::names::TICK_TO_ORDER_SKIP_UNSET)),
            ),
            1,
        );
        assert_eq!(
            counter_value(
                snapshotter.snapshot(),
                mev_metrics::names::TICK_TO_ORDER_SKIPPED_TOTAL,
                Some(("reason", mev_metrics::names::TICK_TO_ORDER_SKIP_CLOCK)),
            ),
            1,
        );
    }

    #[tokio::test]
    async fn delayed_dial_does_not_inflate_submit_ack() {
        // The handshake stalls for `delay`; the submit-ack clock must start at
        // the socket write, after the dial (SPEC-0002 H-7).
        let delay = Duration::from_millis(300);
        let url = slow_handshake_venue(delay).await;
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let exchange = metrics::with_local_recorder(&recorder, || {
            WsExchange::with_url(url, Mode::Live, Some(signer())).unwrap()
        });

        exchange.submit(&action()).await.unwrap();

        let values = histogram_values(
            snapshotter.snapshot(),
            mev_metrics::names::SUBMIT_ACK_SECONDS,
            Some(("transport", "ws")),
        );
        let max = values.iter().copied().fold(0.0_f64, f64::max);
        assert!(
            max < delay.as_secs_f64() / 2.0,
            "submit_ack {max}s included the {delay:?} cold dial"
        );
    }

    #[tokio::test]
    async fn ack_time_is_stamped_by_the_reader_not_the_poller() {
        let url = mock_venue(
            |_| json!({"type": "action", "payload": {"status": "ok", "response": {"n": 1}}}),
        )
        .await;
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let exchange = metrics::with_local_recorder(&recorder, || {
            WsExchange::with_url(url, Mode::Live, Some(signer())).unwrap()
        });

        let poll_delay = Duration::from_millis(300);
        let handle = exchange.enqueue_timed(&action(), 0).await.unwrap();
        // The venue replies promptly, but the handle is only polled much later.
        tokio::time::sleep(poll_delay).await;
        handle.wait().await.unwrap();

        let values = histogram_values(
            snapshotter.snapshot(),
            mev_metrics::names::SUBMIT_ACK_SECONDS,
            Some(("transport", "ws")),
        );
        let max = values.iter().copied().fold(0.0_f64, f64::max);
        assert!(
            max < poll_delay.as_secs_f64() / 2.0,
            "submit_ack {max}s was inflated by the {poll_delay:?} delayed poll"
        );
    }
}
