//! WebSocket `post` transport (SPEC-0002 §8, default).
//!
//! Anything postable over HTTP is postable here: the signed envelope travels as
//! `{"method":"post","id":N,"request":{"type":"action","payload":{...}}}` and
//! the venue replies on `{"channel":"post","data":{"id":N,"response":{...}}}`.
//! Posts are serialized on a single socket and correlated by `id`, so a late
//! reply for an earlier post is skipped rather than misattributed.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};

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
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

use crate::exchange::{
    ActionResponse, ExchangeApi, ExchangeRequest, ExchangeResponse, Prepared, WriteCore, WriteGate,
};
use crate::order::Action;
use crate::ws::ensure_crypto_provider;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

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

/// WebSocket `post` transport implementing [`ExchangeApi`].
pub struct WsExchange {
    url: String,
    core: WriteCore,
    socket: tokio::sync::Mutex<Option<Socket>>,
    next_id: AtomicU64,
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
            socket: tokio::sync::Mutex::new(None),
            next_id: AtomicU64::new(1),
        })
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

    /// Restore the persisted nonce high-water mark.
    pub async fn restore_nonce(&self, last: u64) {
        self.core.restore_nonce(last).await;
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
            .map_err(|e| Error::Http(e.to_string()))?;
        Ok(socket)
    }

    /// Send `request` and await the correlated `post` reply.
    async fn post(&self, request: &ExchangeRequest) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let frame = serde_json::to_string(&PostFrame {
            method: "post",
            id,
            request: PostBody {
                kind: "action",
                payload: request,
            },
        })
        .map_err(|e| Error::Decode(e.to_string()))?;

        let mut guard = self.socket.lock().await;
        if guard.is_none() {
            *guard = Some(Self::dial(&self.url).await?);
        }

        let sent = guard
            .as_mut()
            .expect("socket present")
            .send(Message::Text(frame.clone().into()))
            .await;
        if sent.is_err() {
            // The socket died between posts; redial once and resend the same
            // signed envelope (identical nonce, so at worst the venue reports a
            // duplicate, which the nonce manager heals).
            *guard = Some(Self::dial(&self.url).await?);
            guard
                .as_mut()
                .expect("socket present")
                .send(Message::Text(frame.into()))
                .await
                .map_err(|e| Error::Http(e.to_string()))?;
        }

        let socket = guard.as_mut().expect("socket present");
        Self::await_reply(socket, id).await
    }

    /// Read frames until the `post` reply for `id` arrives, answering pings.
    async fn await_reply(socket: &mut Socket, id: u64) -> Result<Value> {
        loop {
            let message = socket
                .next()
                .await
                .ok_or_else(|| Error::Http("websocket closed before reply".into()))?
                .map_err(|e| Error::Http(e.to_string()))?;
            match message {
                Message::Text(text) => {
                    let frame: Value =
                        serde_json::from_str(&text).map_err(|e| Error::Decode(e.to_string()))?;
                    match frame.get("channel").and_then(Value::as_str) {
                        Some("post") => {
                            let data = &frame["data"];
                            if data.get("id").and_then(Value::as_u64) == Some(id) {
                                return Ok(data.get("response").cloned().unwrap_or(Value::Null));
                            }
                        }
                        Some("error") => {
                            return Err(Error::Http(format!("websocket error: {frame}")));
                        }
                        _ => {}
                    }
                }
                Message::Ping(payload) => {
                    socket
                        .send(Message::Pong(payload))
                        .await
                        .map_err(|e| Error::Http(e.to_string()))?;
                }
                Message::Close(_) => {
                    return Err(Error::Http("websocket closed before reply".into()));
                }
                Message::Pong(_) | Message::Binary(_) | Message::Frame(_) => {}
            }
        }
    }

    /// Build, sign, and (if allowed) post an action.
    async fn send(&self, action: &Action) -> Result<ActionResponse> {
        let request = match self.core.prepare(action).await? {
            Prepared::DryRun(request) => {
                return Ok(ActionResponse {
                    value: json!({ "status": "simulated", "request": request }),
                });
            }
            Prepared::Send(request) => request,
        };

        // The payload is `{"type":"action","payload":{"status":...,"response":...}}`
        // or `{"type":"error","payload":"..."}`.
        let reply = self.post(&request).await?;
        match reply.get("type").and_then(Value::as_str) {
            Some("action") => {
                let payload = reply.get("payload").cloned().unwrap_or(Value::Null);
                let response: ExchangeResponse =
                    serde_json::from_value(payload).map_err(|e| Error::Decode(e.to_string()))?;
                if !response.is_ok() {
                    let message = response
                        .error_message()
                        .unwrap_or_else(|| "unknown".to_string());
                    return Err(Error::Exchange(message));
                }
                Ok(ActionResponse {
                    value: response.response.unwrap_or(Value::Null),
                })
            }
            Some("error") => {
                let message = reply
                    .get("payload")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string();
                Err(Error::Exchange(message))
            }
            _ => Err(Error::Decode(format!("unexpected post reply: {reply}"))),
        }
    }
}

#[async_trait]
impl ExchangeApi for WsExchange {
    async fn submit(&self, action: &Action) -> Result<ActionResponse> {
        self.send(action).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::order::{Grouping, limit_order};
    use crate::signing::AgentSigner;
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
    async fn serializes_posts_and_correlates_ids() {
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
        assert_eq!(db.lock().unwrap().nonce_last().unwrap(), Some(first));

        let restarted = WsExchange::with_url(url, Mode::Live, Some(signer()))
            .unwrap()
            .with_nonce_db(db.clone())
            .unwrap();
        assert_eq!(restarted.last_nonce().await, first);
    }
}
