//! Typed exchange wire responses and the reply handle (SPEC-0002 §9).

use std::future::Future;
use std::pin::Pin;

use serde::Deserialize;
use serde_json::Value;

use hl_arb_core::error::{Error, Result};

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

/// Parse a raw `post` reply into an [`ActionResponse`] (or a typed error).
///
/// Shared by the transports so the split enqueue/await path and the one-shot
/// path classify replies the same way.
pub fn parse_post_reply(reply: Value) -> Result<ActionResponse> {
    match reply.get("type").and_then(Value::as_str) {
        Some("action") => {
            let payload = reply.get("payload").cloned().unwrap_or(Value::Null);
            // The reply was received but could not be parsed: the order's
            // outcome is ambiguous, so reconcile rather than reject.
            let response: ExchangeResponse = serde_json::from_value(payload)
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
        Some("error") => {
            let message = reply
                .get("payload")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            Err(Error::Exchange(message))
        }
        _ => Err(Error::UnknownOutcome(format!(
            "unexpected post reply: {reply}"
        ))),
    }
}

/// A type-erased handle to one signed action's reply.
///
/// Enqueueing an action (signing + writing the frame) happens before the handle
/// is returned, so a caller can enqueue in order and then await the replies
/// concurrently (SPEC-0002 H-1, SPEC-0010 §12). Dropping the handle simply
/// abandons the wait.
pub struct ReplyHandle {
    inner: Pin<Box<dyn Future<Output = Result<ActionResponse>> + Send>>,
}

impl ReplyHandle {
    /// Wrap an arbitrary reply future.
    pub fn new(fut: impl Future<Output = Result<ActionResponse>> + Send + 'static) -> Self {
        Self {
            inner: Box::pin(fut),
        }
    }

    /// A handle already resolved to an outcome (dry-run, HTTP fallback).
    pub fn ready(result: Result<ActionResponse>) -> Self {
        Self::new(async move { result })
    }

    /// Await the reply.
    pub async fn wait(self) -> Result<ActionResponse> {
        self.inner.await
    }
}

impl std::fmt::Debug for ReplyHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ReplyHandle { .. }")
    }
}
