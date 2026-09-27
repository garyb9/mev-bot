//! Exchange client: signing envelope, transports, and typed responses
//! (SPEC-0002 §8–§9).
//!
//! Reads use [`InfoApi`]; writes go through [`ExchangeApi`]. Two transports sit
//! behind the trait — WebSocket `post` ([`crate::ws_exchange::WsExchange`], the
//! default) and REST `POST /exchange` ([`HttpExchange`], the fallback). This
//! module owns the shared signed-envelope/response types and the [`WriteCore`]
//! (gating, signing, nonce sequencing) both transports build on.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use mev_core::{
    config::{Mode, Network},
    db::Db,
    error::{Error, Result},
};

use crate::nonce::{NonceManager, now_ms};
use crate::order::{Action, CancelByCloidWire, CancelWire, OrderWire};
use crate::signing::{AgentSigner, Signature};

/// Hyperliquid success status string.
pub const STATUS_OK: &str = "ok";
/// Hyperliquid error status string.
pub const STATUS_ERR: &str = "err";

/// Top-level `/exchange` response (`{"status":"ok"|"err",...}`).
#[derive(Debug, Clone, Deserialize)]
pub struct ExchangeResponse {
    /// `ok` or `err`.
    pub status: String,
    /// Present when `status == "err"`; may be a bare string or
    /// `{"status":"err","response":"..."}`.
    #[serde(default)]
    pub response: Option<Value>,
}

impl ExchangeResponse {
    /// Whether the venue accepted the action envelope.
    pub fn is_ok(&self) -> bool {
        self.status == STATUS_OK
    }

    /// Extract a human-readable reason from an error response.
    pub fn error_message(&self) -> Option<String> {
        match self.response.as_ref()? {
            Value::String(message) => Some(message.clone()),
            Value::Object(map) => map
                .get("response")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| map.get("status").and_then(Value::as_str).map(str::to_owned)),
            _ => None,
        }
    }
}

/// Per-order outcome status strings returned by the venue (SPEC-0002 §9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderStatus {
    /// Order accepted and resting on the book.
    Resting,
    /// Order fully filled immediately.
    Filled,
    /// Order was rejected; carries the venue's status string.
    Rejected(RejectReason),
    /// Any other status string, preserved verbatim.
    Other(String),
}

impl OrderStatus {
    /// Parse a per-order `status` string into a typed status.
    pub fn parse(status: &str) -> Self {
        match status {
            "resting" => OrderStatus::Resting,
            "filled" => OrderStatus::Filled,
            other => match RejectReason::parse(other) {
                Some(reason) => OrderStatus::Rejected(reason),
                None => OrderStatus::Other(other.to_string()),
            },
        }
    }

    /// Stable label for metrics.
    pub fn label(&self) -> String {
        match self {
            OrderStatus::Resting => "resting".to_string(),
            OrderStatus::Filled => "filled".to_string(),
            OrderStatus::Rejected(reason) => reason.as_str().to_string(),
            OrderStatus::Other(other) => other.clone(),
        }
    }
}

/// Typed rejection reasons (subset of the venue's status vocabulary).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// Post-only price would have crossed the book.
    BadAloPxRejected,
    /// IOC could not execute and was cancelled.
    IocCancelRejected,
    /// Insufficient perp margin.
    PerpMarginRejected,
    /// Notional below the venue minimum.
    MinTradeNtlRejected,
    /// Price not aligned to the tick.
    TickRejected,
    /// Reduce-only order would increase exposure.
    ReduceOnlyRejected,
    /// Oracle price unavailable/insufficient for the market.
    OracleRejected,
    /// Reduce-only order had no matching position.
    ReduceOnlyNoPosition,
    /// Any other status string.
    Unknown,
}

impl RejectReason {
    /// Parse a status string, or `None` when it is not a known reject.
    pub fn parse(status: &str) -> Option<Self> {
        Some(match status {
            "badAloPxRejected" => RejectReason::BadAloPxRejected,
            "iocCancelRejected" => RejectReason::IocCancelRejected,
            "perpMarginRejected" => RejectReason::PerpMarginRejected,
            "minTradeNtlRejected" => RejectReason::MinTradeNtlRejected,
            "tickRejected" => RejectReason::TickRejected,
            "reduceOnlyRejected" => RejectReason::ReduceOnlyRejected,
            "oracleRejected" => RejectReason::OracleRejected,
            "reduceOnlyNoPosition" => RejectReason::ReduceOnlyNoPosition,
            "unknownRejected" | "unknown" => RejectReason::Unknown,
            _ => return None,
        })
    }

    /// Stable label for metrics.
    pub const fn as_str(self) -> &'static str {
        match self {
            RejectReason::BadAloPxRejected => "badAloPxRejected",
            RejectReason::IocCancelRejected => "iocCancelRejected",
            RejectReason::PerpMarginRejected => "perpMarginRejected",
            RejectReason::MinTradeNtlRejected => "minTradeNtlRejected",
            RejectReason::TickRejected => "tickRejected",
            RejectReason::ReduceOnlyRejected => "reduceOnlyRejected",
            RejectReason::OracleRejected => "oracleRejected",
            RejectReason::ReduceOnlyNoPosition => "reduceOnlyNoPosition",
            RejectReason::Unknown => "unknownRejected",
        }
    }
}

/// Result of a submitted order action.
#[derive(Debug, Clone)]
pub struct OrderResponse {
    /// Per-order statuses, in request order.
    pub statuses: Vec<OrderStatus>,
    /// Per-order venue `oid`s, in request order (`None` when the reply carried
    /// none, e.g. a string status or an error). Used to map later fills to
    /// their orders (SPEC-0002 H-2/H-3).
    pub oids: Vec<Option<u64>>,
}

impl OrderResponse {
    /// Parse the `response.data.statuses` array from an `order` action result.
    pub fn from_value(value: &Value) -> Result<Self> {
        let entries = value
            .get("data")
            .and_then(|d| d.get("statuses"))
            .and_then(Value::as_array)
            .ok_or_else(|| Error::Decode("order response missing data.statuses".into()))?;

        let mut statuses = Vec::with_capacity(entries.len());
        let mut oids = Vec::with_capacity(entries.len());
        for entry in entries {
            match entry {
                Value::String(s) => {
                    statuses.push(OrderStatus::parse(s));
                    oids.push(None);
                }
                Value::Object(map) => {
                    let oid = map
                        .get("resting")
                        .and_then(|resting| resting.get("oid"))
                        .or_else(|| map.get("filled").and_then(|filled| filled.get("oid")))
                        .and_then(Value::as_u64);
                    let status = if map.contains_key("resting") {
                        OrderStatus::Resting
                    } else if map.get("filled").is_some() {
                        OrderStatus::Filled
                    } else if let Some(error) = map.get("error").and_then(Value::as_str) {
                        OrderStatus::parse(error)
                    } else {
                        OrderStatus::Other(entry.to_string())
                    };
                    statuses.push(status);
                    oids.push(oid);
                }
                _ => {
                    statuses.push(OrderStatus::Other(entry.to_string()));
                    oids.push(None);
                }
            }
        }

        Ok(OrderResponse { statuses, oids })
    }
}

/// Result of a submit (order/cancel/modify/scheduleCancel/leverage).
#[derive(Debug, Clone)]
pub struct ActionResponse {
    /// Raw `response` payload from the venue.
    pub value: Value,
}

impl ActionResponse {
    /// Parse per-order statuses when this is an `order` action result.
    pub fn order_response(&self) -> Result<OrderResponse> {
        OrderResponse::from_value(&self.value)
    }
}

/// Write API for HyperCore actions (SPEC-0002 §8).
///
/// [`submit`](Self::submit) is the single primitive: every transport signs and
/// sends one [`Action`]. The remaining methods are thin, typed wrappers over the
/// action catalog so callers never hand-build wire structs.
#[async_trait]
pub trait ExchangeApi: Send + Sync {
    /// Submit a signed action envelope.
    async fn submit(&self, action: &Action) -> Result<ActionResponse>;

    /// Place one or more orders and parse the per-order statuses.
    async fn place(&self, orders: Vec<OrderWire>) -> Result<OrderResponse> {
        self.submit(&Action::order(orders)).await?.order_response()
    }

    /// Cancel orders by venue order id.
    async fn cancel(&self, cancels: Vec<CancelWire>) -> Result<ActionResponse> {
        self.submit(&Action::Cancel { cancels }).await
    }

    /// Cancel orders by client order id.
    async fn cancel_by_cloid(&self, cancels: Vec<CancelByCloidWire>) -> Result<ActionResponse> {
        self.submit(&Action::CancelByCloid { cancels }).await
    }

    /// Arm (`Some(at)`) or disarm (`None`) the dead-man's switch.
    async fn schedule_cancel(&self, at_ms: Option<u64>) -> Result<ActionResponse> {
        self.submit(&Action::ScheduleCancel { time: at_ms }).await
    }

    /// Set cross/isolated leverage for an asset.
    async fn update_leverage(
        &self,
        asset: u32,
        is_cross: bool,
        leverage: u32,
    ) -> Result<ActionResponse> {
        self.submit(&Action::UpdateLeverage {
            asset,
            is_cross,
            leverage,
        })
        .await
    }
}

/// A signed, prepared action: either withheld (`simulate`) or ready to send.
#[derive(Debug, Clone)]
pub enum Prepared {
    /// Signed but must not be sent (`simulate` mode).
    DryRun(Box<ExchangeRequest>),
    /// Signed, nonce persisted, ready to send (`live` mode).
    Send(Box<ExchangeRequest>),
}

/// Shared write path used by every transport: mode gating, EIP-712 signing,
/// nonce sequencing, and durable high-water-mark persistence (SPEC-0002 §5).
pub struct WriteCore {
    signer: Option<AgentSigner>,
    nonce: tokio::sync::Mutex<NonceManager>,
    db: Option<Arc<Mutex<Db>>>,
    gate: WriteGate,
    expires_after: Option<u64>,
    vault_address: Option<String>,
}

impl WriteCore {
    /// Create a write core for `mode`; a signer is required unless blocked.
    pub fn new(mode: Mode, signer: Option<AgentSigner>) -> Result<Self> {
        let gate = WriteGate::from(mode);
        if gate != WriteGate::Blocked && signer.is_none() {
            return Err(Error::Config(format!(
                "{} mode requires an agent signer",
                match mode {
                    Mode::Observe => "observe",
                    Mode::Simulate => "simulate",
                    Mode::Live => "live",
                }
            )));
        }
        Ok(Self {
            signer,
            nonce: tokio::sync::Mutex::new(NonceManager::new()),
            db: None,
            gate,
            expires_after: None,
            vault_address: None,
        })
    }

    /// Attach a durable nonce store and restore the persisted high-water mark.
    pub fn with_nonce_db(mut self, db: Arc<Mutex<Db>>) -> Result<Self> {
        let restored = {
            let guard = db
                .lock()
                .map_err(|_| Error::Config("db lock poisoned".into()))?;
            guard.nonce_last()?
        };
        let mut manager = NonceManager::new();
        if let Some(last) = restored {
            manager = NonceManager::restore(last);
        }
        self.nonce = tokio::sync::Mutex::new(manager);
        self.db = Some(db);
        Ok(self)
    }

    /// The effective write gate.
    pub fn gate(&self) -> WriteGate {
        self.gate
    }

    /// Set the optional `expiresAfter` field applied to every action.
    pub fn with_expires_after(mut self, expires_after: Option<u64>) -> Self {
        self.expires_after = expires_after;
        self
    }

    /// Set the optional vault address applied to every action.
    pub fn with_vault_address(mut self, vault_address: Option<String>) -> Self {
        self.vault_address = vault_address;
        self
    }

    /// Restore the persisted nonce high-water mark.
    pub async fn restore_nonce(&self, last: u64) {
        *self.nonce.lock().await = NonceManager::restore(last);
    }

    /// The current nonce high-water mark (for persistence).
    pub async fn last_nonce(&self) -> u64 {
        self.nonce.lock().await.last()
    }

    /// Resync the nonce after a stale/duplicate/recent-window rejection.
    pub async fn heal_nonce(&self) -> u64 {
        let nonce = self.nonce.lock().await.on_reject(now_ms());
        let _ = self.persist_nonce(nonce);
        nonce
    }

    /// Write the nonce high-water mark to the durable store, if attached.
    fn persist_nonce(&self, nonce: u64) -> Result<()> {
        if let Some(db) = &self.db {
            let guard = db
                .lock()
                .map_err(|_| Error::Config("db lock poisoned".into()))?;
            guard.set_nonce_last(nonce)?;
        }
        Ok(())
    }

    /// Sign the next envelope for `action`, gated and persisted for `live`.
    pub async fn prepare(&self, action: &Action) -> Result<Prepared> {
        if self.gate == WriteGate::Blocked {
            return Err(Error::Config(
                "observe mode cannot sign or submit actions".into(),
            ));
        }
        let signer = self
            .signer
            .as_ref()
            .ok_or_else(|| Error::Config("no agent signer is configured".into()))?;
        let nonce = self.nonce.lock().await.next(now_ms());
        let request = build_request(
            action,
            signer,
            nonce,
            self.vault_address.clone(),
            self.expires_after,
        )?;
        if self.gate == WriteGate::DryRun {
            return Ok(Prepared::DryRun(Box::new(request)));
        }
        // Persist before sending so a crash cannot reuse this nonce.
        self.persist_nonce(nonce)?;
        Ok(Prepared::Send(Box::new(request)))
    }
}

/// A signed `/exchange` request payload.
///
/// The action is stored (not converted to a `Value`) so that its JSON field
/// order matches the msgpack order used for the hash: the venue re-encodes the
/// action it receives to verify the signature, and that encoding is
/// order-sensitive.
#[derive(Debug, Clone, Serialize)]
pub struct ExchangeRequest {
    /// The action (serialized inline).
    pub action: Action,
    /// Nonce (ms timestamp).
    pub nonce: u64,
    /// Signature over the action.
    pub signature: Signature,
    /// Optional vault address.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vault_address: Option<String>,
    /// Optional expiry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_after: Option<u64>,
}

/// Build the signed request envelope for an L1 action.
pub fn build_request(
    action: &Action,
    signer: &AgentSigner,
    nonce: u64,
    vault_address: Option<String>,
    expires_after: Option<u64>,
) -> Result<ExchangeRequest> {
    let vault = match &vault_address {
        Some(addr) => Some(
            addr.parse()
                .map_err(|e| Error::Config(format!("invalid vault address `{addr}`: {e}")))?,
        ),
        None => None,
    };
    let signature = signer.sign_l1(action, nonce, vault, expires_after)?;
    Ok(ExchangeRequest {
        action: action.clone(),
        nonce,
        signature,
        vault_address,
        expires_after,
    })
}

/// Execution mode gate: `observe` must never write to `/exchange`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteGate {
    /// Writes are disabled entirely (observe).
    Blocked,
    /// Writes are built and signed but not sent (simulate).
    DryRun,
    /// Writes are sent (live).
    Open,
}

impl From<Mode> for WriteGate {
    fn from(mode: Mode) -> Self {
        match mode {
            Mode::Observe => WriteGate::Blocked,
            Mode::Simulate => WriteGate::DryRun,
            Mode::Live => WriteGate::Open,
        }
    }
}

/// REST `POST /exchange` transport implementing [`ExchangeApi`].
pub struct HttpExchange {
    client: reqwest::Client,
    base_url: String,
    core: WriteCore,
}

impl HttpExchange {
    /// Create a client for the given network and mode.
    ///
    /// `signer` is required for `simulate`/`live`; it may be `None` in
    /// `observe`, where no writes are possible.
    pub fn new(network: Network, mode: Mode, signer: Option<AgentSigner>) -> Result<Self> {
        Self::with_base_url(network.rest_url(), mode, signer)
    }

    /// Create a client against an explicit base URL (tests).
    pub fn with_base_url(
        base_url: impl Into<String>,
        mode: Mode,
        signer: Option<AgentSigner>,
    ) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::new(),
            base_url: base_url.into(),
            core: WriteCore::new(mode, signer)?,
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

    /// Build, sign, and (if allowed) POST an action.
    async fn send(&self, action: &Action) -> Result<ActionResponse> {
        let request = match self.core.prepare(action).await? {
            Prepared::DryRun(request) => {
                return Ok(ActionResponse {
                    value: json!({ "status": "simulated", "request": request }),
                });
            }
            Prepared::Send(request) => request,
        };

        let url = format!("{}/exchange", self.base_url.trim_end_matches('/'));
        let resp = self
            .client
            .post(&url)
            .json(&request)
            .send()
            .await
            .map_err(|e| Error::Http(e.to_string()))?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(Error::Http(format!("POST /exchange -> {status}: {text}")));
        }

        // The response arrived but could not be parsed: reconcile, don't reject.
        let response: ExchangeResponse = resp
            .json()
            .await
            .map_err(|e| Error::UnknownOutcome(format!("undecodable post reply: {e}")))?;

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
}

#[async_trait]
impl ExchangeApi for HttpExchange {
    async fn submit(&self, action: &Action) -> Result<ActionResponse> {
        self.send(action).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::order::{Action, Grouping, Tif, limit_order};
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    const KEY: &str = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";

    fn simple_action() -> Action {
        Action::Order {
            orders: vec![limit_order(0, true, "50000", "0.1", Tif::Gtc, false, None)],
            grouping: Grouping::Na,
        }
    }

    fn signer() -> AgentSigner {
        AgentSigner::from_hex(KEY, true).unwrap()
    }

    #[tokio::test]
    async fn observe_blocks_without_sending() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/exchange"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"status":"ok"}"#))
            .expect(0)
            .mount(&server)
            .await;

        let exchange = HttpExchange::with_base_url(server.uri(), Mode::Observe, None).unwrap();
        assert_eq!(exchange.gate(), WriteGate::Blocked);
        let err = exchange.submit(&simple_action()).await.unwrap_err();
        assert!(matches!(err, Error::Config(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn simulate_signs_but_never_posts() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/exchange"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"status":"ok"}"#))
            .expect(0)
            .mount(&server)
            .await;

        let exchange =
            HttpExchange::with_base_url(server.uri(), Mode::Simulate, Some(signer())).unwrap();
        assert_eq!(exchange.gate(), WriteGate::DryRun);
        let response = exchange.submit(&simple_action()).await.unwrap();
        assert_eq!(response.value["status"], "simulated");
        assert!(response.value["request"]["signature"]["r"].is_string());
    }

    #[tokio::test]
    async fn live_posts_and_parses_order_statuses() {
        let server = MockServer::start().await;
        let body = r#"{"status":"ok","response":{"type":"order","data":{"statuses":["resting"]}}}"#;
        Mock::given(method("POST"))
            .and(path("/exchange"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .expect(1)
            .mount(&server)
            .await;

        let exchange =
            HttpExchange::with_base_url(server.uri(), Mode::Live, Some(signer())).unwrap();
        let response = exchange.submit(&simple_action()).await.unwrap();
        assert_eq!(
            response.order_response().unwrap().statuses,
            vec![OrderStatus::Resting]
        );
    }

    #[tokio::test]
    async fn live_maps_error_status_to_typed_error() {
        let server = MockServer::start().await;
        let body = r#"{"status":"err","response":"Must deposit before trading."}"#;
        Mock::given(method("POST"))
            .and(path("/exchange"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        let exchange =
            HttpExchange::with_base_url(server.uri(), Mode::Live, Some(signer())).unwrap();
        let err = exchange.submit(&simple_action()).await.unwrap_err();
        match err {
            Error::Exchange(message) => assert!(message.contains("Must deposit")),
            other => panic!("expected exchange error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn nonce_is_monotonic_across_submits() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/exchange"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"status":"ok"}"#))
            .mount(&server)
            .await;
        let exchange =
            HttpExchange::with_base_url(server.uri(), Mode::Live, Some(signer())).unwrap();

        let before = exchange.last_nonce().await;
        exchange.submit(&simple_action()).await.unwrap();
        let first = exchange.last_nonce().await;
        exchange.submit(&simple_action()).await.unwrap();
        let second = exchange.last_nonce().await;
        assert!(
            first > before && second > first,
            "{before} {first} {second}"
        );
    }

    #[test]
    fn parses_per_order_rejections() {
        let value = json!({
            "data": { "statuses": [
                {"resting": {"oid": 1}},
                "filled",
                {"error": "tickRejected"},
                "someUnknownStatus"
            ]}
        });
        let response = OrderResponse::from_value(&value).unwrap();
        assert_eq!(
            response.statuses,
            vec![
                OrderStatus::Resting,
                OrderStatus::Filled,
                OrderStatus::Rejected(RejectReason::TickRejected),
                OrderStatus::Other("someUnknownStatus".into()),
            ]
        );
    }

    #[test]
    fn envelope_preserves_action_field_order() {
        // The venue re-encodes the received action to verify the hash, so the
        // JSON field order must match the msgpack order.
        let action = simple_action();
        let request = build_request(&action, &signer(), 1, None, None).unwrap();
        let json = serde_json::to_string(&request).unwrap();
        let action_json = json
            .split("\"action\":")
            .nth(1)
            .and_then(|rest| rest.find(",\"nonce\"").map(|end| &rest[..end]))
            .unwrap();
        assert_eq!(
            action_json,
            r#"{"type":"order","orders":[{"a":0,"b":true,"p":"50000","s":"0.1","r":false,"t":{"limit":{"tif":"Gtc"}}}],"grouping":"na"}"#
        );
    }

    #[tokio::test]
    async fn nonce_persists_across_instances() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/exchange"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"status":"ok"}"#))
            .mount(&server)
            .await;

        let db = Arc::new(Mutex::new(Db::open_in_memory().unwrap()));
        let exchange = HttpExchange::with_base_url(server.uri(), Mode::Live, Some(signer()))
            .unwrap()
            .with_nonce_db(db.clone())
            .unwrap();
        exchange.submit(&simple_action()).await.unwrap();
        let first = exchange.last_nonce().await;
        assert_eq!(db.lock().unwrap().nonce_last().unwrap(), Some(first));

        // A fresh instance restores the persisted high-water mark and never
        // regresses.
        let restarted = HttpExchange::with_base_url(server.uri(), Mode::Live, Some(signer()))
            .unwrap()
            .with_nonce_db(db.clone())
            .unwrap();
        assert_eq!(restarted.last_nonce().await, first);
        restarted.submit(&simple_action()).await.unwrap();
        assert!(restarted.last_nonce().await > first);
    }

    #[tokio::test]
    async fn trait_wrappers_build_the_right_actions() {
        use crate::order::{CancelWire, OrderWire};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/exchange"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(
                    r#"{"status":"ok","response":{"data":{"statuses":["resting"]}}}"#,
                ),
            )
            .mount(&server)
            .await;

        let exchange =
            HttpExchange::with_base_url(server.uri(), Mode::Live, Some(signer())).unwrap();
        let orders = vec![OrderWire {
            a: 0,
            b: true,
            p: "50000".into(),
            s: "0.1".into(),
            r: false,
            t: crate::order::OrderType::limit(Tif::Gtc),
            c: None,
        }];
        assert_eq!(
            exchange.place(orders).await.unwrap().statuses,
            vec![OrderStatus::Resting]
        );
        exchange
            .cancel(vec![CancelWire { a: 0, o: 7 }])
            .await
            .unwrap();
        exchange
            .schedule_cancel(Some(1_700_000_000_000))
            .await
            .unwrap();
        exchange.schedule_cancel(None).await.unwrap();
    }

    #[test]
    fn simulate_and_live_require_a_signer() {
        assert!(HttpExchange::with_base_url("http://x", Mode::Simulate, None).is_err());
        assert!(HttpExchange::with_base_url("http://x", Mode::Live, None).is_err());
        assert!(HttpExchange::with_base_url("http://x", Mode::Observe, None).is_ok());
    }
}
