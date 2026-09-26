//! Exchange client: signing envelope, transports, and typed responses
//! (SPEC-0002 §8–§9).
//!
//! Reads use [`InfoApi`]; writes go through [`ExchangeApi`]. Two transports sit
//! behind the trait — REST `POST /exchange` (implemented here) and a WebSocket
//! post (later milestone). The agent signer and nonce manager are injected and
//! never logged.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use mev_core::{
    config::{Mode, Network},
    error::{Error, Result},
};

use crate::nonce::{NonceManager, now_ms};
use crate::order::Action;
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
}

impl OrderResponse {
    /// Parse the `response.data.statuses` array from an `order` action result.
    pub fn from_value(value: &Value) -> Result<Self> {
        let entries = value
            .get("data")
            .and_then(|d| d.get("statuses"))
            .and_then(Value::as_array)
            .ok_or_else(|| Error::Decode("order response missing data.statuses".into()))?;

        let statuses = entries
            .iter()
            .map(|entry| match entry {
                Value::String(s) => OrderStatus::parse(s),
                Value::Object(map) => {
                    if map.contains_key("resting") {
                        OrderStatus::Resting
                    } else if map.get("filled").is_some() {
                        OrderStatus::Filled
                    } else if let Some(error) = map.get("error").and_then(Value::as_str) {
                        OrderStatus::parse(error)
                    } else {
                        OrderStatus::Other(entry.to_string())
                    }
                }
                _ => OrderStatus::Other(entry.to_string()),
            })
            .collect();

        Ok(OrderResponse { statuses })
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
#[async_trait]
pub trait ExchangeApi: Send + Sync {
    /// Submit a signed action envelope.
    async fn submit(&self, action: &Action) -> Result<ActionResponse>;
}

/// A signed `/exchange` request payload.
#[derive(Debug, Clone, Serialize)]
pub struct ExchangeRequest {
    /// The action (serialized inline).
    pub action: Value,
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
    let action_value = serde_json::to_value(action).map_err(|e| Error::Decode(e.to_string()))?;
    Ok(ExchangeRequest {
        action: action_value,
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
    signer: Option<AgentSigner>,
    nonce: tokio::sync::Mutex<NonceManager>,
    gate: WriteGate,
    expires_after: Option<u64>,
    vault_address: Option<String>,
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
            client: reqwest::Client::new(),
            base_url: base_url.into(),
            signer,
            nonce: tokio::sync::Mutex::new(NonceManager::new()),
            gate,
            expires_after: None,
            vault_address: None,
        })
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
        self.nonce.lock().await.on_reject(now_ms())
    }

    /// Build, sign, and (if allowed) POST an action.
    async fn send(&self, action: &Action) -> Result<ActionResponse> {
        let signer = self
            .signer
            .as_ref()
            .ok_or_else(|| Error::Config("observe mode cannot sign or submit actions".into()))?;
        let nonce = self.nonce.lock().await.next(now_ms());
        let request = build_request(
            action,
            signer,
            nonce,
            self.vault_address.clone(),
            self.expires_after,
        )?;

        if self.gate == WriteGate::DryRun {
            return Ok(ActionResponse {
                value: json!({ "status": "simulated", "request": request }),
            });
        }

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

        let response: ExchangeResponse = resp
            .json()
            .await
            .map_err(|e| Error::Decode(e.to_string()))?;

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
    fn simulate_and_live_require_a_signer() {
        assert!(HttpExchange::with_base_url("http://x", Mode::Simulate, None).is_err());
        assert!(HttpExchange::with_base_url("http://x", Mode::Live, None).is_err());
        assert!(HttpExchange::with_base_url("http://x", Mode::Observe, None).is_ok());
    }
}
