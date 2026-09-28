//! Exchange client: signing envelope, transports, and typed responses
//! (SPEC-0002 §8–§9).
//!
//! Reads use [`InfoApi`]; writes go through [`ExchangeApi`]. Two transports sit
//! behind the trait — WebSocket `post` ([`crate::ws_exchange::WsExchange`], the
//! default) and REST `POST /exchange` ([`HttpExchange`], the fallback). This
//! module owns the shared signed-envelope/response types and the [`WriteCore`]
//! (gating, signing, nonce sequencing) both transports build on.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use mev_core::{
    clock::{Clock, SystemClock},
    config::{Mode, Network},
    db::{Db, writer::DbWriter},
    error::{Error, Result},
};

use crate::nonce::{
    DEFAULT_NONCE_LEASE_MS, NONCE_WARN_INTERVAL_MS, NONCE_WRITE_QUEUE, NonceLease, NonceManager,
    VENUE_MAX_FUTURE_MS,
};
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

/// Write API for HyperCore actions (SPEC-0002 §8).
///
/// [`submit`](Self::submit) is the single primitive: every transport signs and
/// sends one [`Action`]. The remaining methods are thin, typed wrappers over the
/// action catalog so callers never hand-build wire structs.
#[async_trait]
pub trait ExchangeApi: Send + Sync {
    /// Submit a signed action envelope.
    async fn submit(&self, action: &Action) -> Result<ActionResponse>;

    /// Sign and enqueue `action`, returning a handle that resolves its reply.
    ///
    /// The enqueue completes before this returns, so callers can enqueue in
    /// order (e.g. cancels before places) and await the replies concurrently
    /// (SPEC-0002 H-1). The default is the one-shot [`Self::submit`], which is
    /// correct but not split (used by the REST fallback).
    async fn enqueue(&self, action: &Action) -> Result<ReplyHandle> {
        let response = self.submit(action).await?;
        Ok(ReplyHandle::ready(Ok(response)))
    }

    /// Like [`Self::enqueue`], but carries the market frame's monotonic read
    /// time (`recv_mono_ns`) so a transport that owns the socket write can
    /// record `hl_tick_to_order_seconds` end to end (SPEC-0002 H-7). The
    /// default drops the hint and defers to [`Self::enqueue`].
    async fn enqueue_timed(&self, action: &Action, recv_mono_ns: u64) -> Result<ReplyHandle> {
        let _ = recv_mono_ns;
        self.enqueue(action).await
    }

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
    /// Signed, nonce covered by the durable lease, ready to send (`live` mode).
    Send(Box<ExchangeRequest>),
}

/// Shared write path used by every transport: mode gating, EIP-712 signing,
/// nonce sequencing, and write-behind lease persistence (SPEC-0002 §5, H-6).
///
/// Dropping a `WriteCore` with an attached nonce store drops its [`DbWriter`],
/// which drains the queue and joins the writer thread — a brief blocking wait
/// at shutdown.
pub struct WriteCore {
    signer: Option<AgentSigner>,
    nonce: tokio::sync::Mutex<NonceManager>,
    /// Coalesced write-behind nonce persistence; absent when no store is
    /// attached, in which case no nonce is persisted (tests, `observe`).
    nonce_store: Option<NonceLease>,
    /// Shared connection, kept only for the off-hot-path operator nonce reset
    /// (`reset_nonce`).
    nonce_db: Option<Arc<Mutex<Db>>>,
    /// Set when the persisted nonce was beyond the venue's future window at
    /// boot. `prepare` fails closed until an operator calls `reset_nonce`.
    nonce_corrupt: std::sync::atomic::AtomicBool,
    /// Time of the last rate-limited log for a runtime far-future refusal
    /// (SPEC-0002 H-6 follow-up). A stuck far-future value must not log per
    /// order.
    future_refusal_warn_ms: AtomicU64,
    clock: Arc<dyn Clock>,
    nonce_lease_ms: u64,
    gate: WriteGate,
    expires_after: Option<u64>,
    vault_address: Option<String>,
    /// Cached `hl_sign_seconds` handle (SPEC-0002 H-7). Pre-created so the hot
    /// statement does not format a label or build a key per sign.
    sign_histogram: metrics::Histogram,
    /// Cached runtime far-future refusal counter (SPEC-0002 H-6 follow-up):
    /// incremented per refused `prepare`, so no per-order key lookup.
    future_refusals: metrics::Counter,
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
            nonce_store: None,
            nonce_db: None,
            nonce_corrupt: std::sync::atomic::AtomicBool::new(false),
            future_refusal_warn_ms: AtomicU64::new(0),
            clock: Arc::new(SystemClock),
            nonce_lease_ms: DEFAULT_NONCE_LEASE_MS,
            gate,
            expires_after: None,
            vault_address: None,
            sign_histogram: metrics::histogram!(mev_metrics::names::SIGN_SECONDS),
            future_refusals: metrics::counter!(mev_metrics::names::NONCE_FUTURE_REFUSALS_TOTAL),
        })
    }

    /// Inject the clock used for nonce generation (tests).
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Override the write-ahead nonce lease (in nonce space). Values below 4 are
    /// clamped so the half-lease refresh threshold stays meaningful.
    pub fn with_nonce_lease(mut self, lease_ms: u64) -> Self {
        self.nonce_lease_ms = lease_ms.max(4);
        self
    }

    /// Attach a durable nonce store and restore the persisted high-water mark.
    ///
    /// The bot **never resumes below the persisted value**: the manager starts
    /// at `max(now, restored + 1)`. The restart prime is `max(restored, now +
    /// lease)`, so a crash that sends nothing does not ratchet the horizon; a
    /// fast restart with `restored >= now + lease` may refuse the first send
    /// until the urgent write-behind refresh lands (milliseconds).
    ///
    /// A persisted value beyond the venue's future window
    /// ([`VENUE_MAX_FUTURE_MS`]) cannot be one this bot sent: it is treated as
    /// corruption, counted in `hl_nonce_resume_corrupt_total`, logged at
    /// `error`, and the core **fails closed** until an operator calls
    /// [`Self::reset_nonce`] (SPEC-0002 H-6). It is never silently clamped.
    pub fn with_nonce_db(mut self, db: Arc<Mutex<Db>>) -> Result<Self> {
        let now = self.clock.now_ms();
        let (resumed, horizon, corrupt) = {
            let guard = db
                .lock()
                .map_err(|_| Error::Config("db lock poisoned".into()))?;
            let restored = guard.nonce_last()?.unwrap_or(0);
            let ceiling = now.saturating_add(VENUE_MAX_FUTURE_MS);
            let corrupt = restored > ceiling;
            if corrupt {
                metrics::counter!(mev_metrics::names::NONCE_RESUME_CORRUPT_TOTAL).increment(1);
                tracing::error!(
                    restored,
                    ceiling,
                    "persisted nonce is beyond the venue future window; refusing to send until an operator resets the nonce"
                );
            }
            let horizon = restored.max(now.saturating_add(self.nonce_lease_ms));
            if !corrupt {
                guard.set_nonce_last(horizon)?;
            }
            (restored, horizon, corrupt)
        };
        let writer = DbWriter::spawn_shared(db.clone(), NONCE_WRITE_QUEUE);
        let durable = writer.nonce_hwm();
        let lease = NonceLease::new(writer, horizon, self.nonce_lease_ms, durable, now);
        self.nonce = tokio::sync::Mutex::new(NonceManager::resume(resumed));
        self.nonce_store = Some(lease);
        self.nonce_db = Some(db);
        self.nonce_corrupt
            .store(corrupt, std::sync::atomic::Ordering::Release);
        Ok(self)
    }

    /// Test-only: attach a lease over an in-memory manager resumed from
    /// `resume_from`, bypassing SQLite.
    #[cfg(test)]
    pub(crate) fn with_test_nonce_store(mut self, lease: NonceLease, resume_from: u64) -> Self {
        self.nonce = tokio::sync::Mutex::new(NonceManager::resume(resume_from));
        self.nonce_store = Some(lease);
        self
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

    /// Restore a specific nonce high-water mark (explicit operator path).
    ///
    /// Clamped to never go below any known value (the in-memory last and the
    /// durable/requested marks), so it cannot force a reuse. The floor is read
    /// under the nonce lock in [`Self::rebase_nonce`], so a concurrent
    /// `prepare` cannot land between measuring the floor and rewriting it.
    /// Also clears a prior corruption fail-closed state and writes the new
    /// horizon synchronously so a restart sees it (SPEC-0002 H-6).
    pub async fn restore_nonce(&self, last: u64) -> Result<()> {
        self.rebase_nonce(last, false).await
    }

    /// Operator escape hatch for a corrupt persisted nonce: abandon history and
    /// resume from the wall clock, clearing the fail-closed state (SPEC-0002
    /// H-6). Intentionally allowed to lower the persisted value, so it is
    /// **gated on the boot corruption flag**: while that flag is clear, `prepare`
    /// may have handed out nonces and a reset could force a reuse.
    pub async fn reset_nonce(&self) -> Result<()> {
        if !self.nonce_corrupt.load(Ordering::Acquire) {
            return Err(Error::Config(
                "refusing to reset the nonce: it is not flagged corrupt, so nonces \
                 may already have been issued; restart to recover instead"
                    .into(),
            ));
        }
        let now = self.clock.now_ms();
        self.rebase_nonce(now, true).await
    }

    /// Write `requested` (and a lease ahead of it) to the durable store and
    /// rebase the in-memory manager and lease. The synchronous write is safe
    /// here: this is the explicit operator path, never the hot path.
    ///
    /// `allow_lower` is set only by the corrupt-value operator reset. On the
    /// restore path it is clear, so the floor — the in-memory last together
    /// with the durable and requested marks, read while holding the nonce lock
    /// that `prepare` reserves under — wins: a concurrent send can never be
    /// lowered underneath the rebase and handed out twice.
    ///
    /// The durable atomic is lowered under the same lock the writer thread takes
    /// to commit a nonce, so the database write and the atomic cannot be
    /// observed apart: no writer commit interleaves between them.
    async fn rebase_nonce(&self, requested: u64, allow_lower: bool) -> Result<()> {
        let now = self.clock.now_ms();
        // Replace the in-memory manager under its lock, and measure the floor
        // under the same lock so no `prepare` can reserve a nonce between.
        let mut manager = self.nonce.lock().await;
        let known = self
            .nonce_store
            .as_ref()
            .map_or(0, |store| store.durable().max(store.requested()));
        let floor = manager.last().max(known);
        let restore = if allow_lower {
            requested
        } else {
            requested.max(floor)
        };
        let horizon = restore.max(now.saturating_add(self.nonce_lease_ms));
        *manager = NonceManager::resume(restore);
        if let Some(db) = &self.nonce_db {
            let guard = db
                .lock()
                .map_err(|_| Error::Config("db lock poisoned".into()))?;
            guard.set_nonce_last(horizon)?;
            if let Some(store) = &self.nonce_store {
                store.rebase(horizon, now);
            }
        } else if let Some(store) = &self.nonce_store {
            store.rebase(horizon, now);
        }
        drop(manager);
        self.nonce_corrupt.store(false, Ordering::Release);
        Ok(())
    }

    /// Test-only: the confirmed durable and requested nonce horizons.
    #[cfg(test)]
    pub(crate) fn nonce_marks(&self) -> Option<(u64, u64)> {
        self.nonce_store
            .as_ref()
            .map(|store| (store.durable(), store.requested()))
    }

    /// The current nonce high-water mark (for persistence and tests).
    pub async fn last_nonce(&self) -> u64 {
        self.nonce.lock().await.last()
    }

    /// Resync the nonce after a stale/duplicate/recent-window rejection.
    ///
    /// Errors while the corrupt fail-closed state is set: the resync path would
    /// advance from a value known to be corrupt, so an operator reset is the only
    /// correct recovery.
    pub async fn heal_nonce(&self) -> Result<u64> {
        if self
            .nonce_corrupt
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(Error::NotSent(
                "persisted nonce is corrupt; an operator must reset it before resyncing".into(),
            ));
        }
        let now = self.clock.now_ms();
        let nonce = self.nonce.lock().await.on_reject(now);
        if let Some(store) = &self.nonce_store {
            store.force(nonce, now);
        }
        Ok(nonce)
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
        // Fail closed until an operator resets a corrupt persisted nonce.
        if self
            .nonce_corrupt
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(Error::NotSent(
                "persisted nonce is corrupt; an operator must reset the nonce".to_string(),
            ));
        }
        let now = self.clock.now_ms();
        let nonce = match self.nonce.lock().await.next(now) {
            Ok(nonce) => nonce,
            Err(err) => {
                // The next candidate is beyond the venue's future window
                // (corruption or a large backwards clock step). Fail closed;
                // the condition clears once the clock catches up. Counted
                // separately from boot corruption, and the error is
                // rate-limited so a stuck value cannot log per order.
                self.future_refusals.increment(1);
                if refusal_log_due(&self.future_refusal_warn_ms, now) {
                    tracing::error!(
                        now,
                        "next nonce is beyond the venue future window; refusing to send"
                    );
                }
                return Err(err);
            }
        };
        let started = Instant::now();
        let built = build_request(
            action,
            signer,
            nonce,
            self.vault_address.clone(),
            self.expires_after,
        );
        // Build + msgpack + EIP-712 sign (SPEC-0002 H-7). Recorded even on
        // failure: the stage ran.
        self.sign_histogram.record(started.elapsed().as_secs_f64());
        let request = built?;
        if self.gate == WriteGate::DryRun {
            return Ok(Prepared::DryRun(Box::new(request)));
        }
        // Off the hot path: at most an atomic read and a `Copy` enqueue. The
        // write-behind lease guarantees a crash cannot reuse this nonce.
        if let Some(store) = &self.nonce_store
            && let Err(err) = store.cover(nonce, now)
        {
            // The nonce is not covered, so it will not be sent: free it so a
            // stall does not burn through nonce space (cheap: the failure path
            // only, and a no-op if a later prepare advanced past it).
            self.nonce.lock().await.release(nonce);
            return Err(err);
        }
        Ok(Prepared::Send(Box::new(request)))
    }
}

/// Whether a runtime far-future refusal should be logged now, stamping
/// `last_ms` when it is. Rate-limited to one message per
/// [`NONCE_WARN_INTERVAL_MS`] (the same pattern as the write-behind warning) so
/// a stuck far-future value cannot flood the log on every `prepare`.
fn refusal_log_due(last_ms: &AtomicU64, now_ms: u64) -> bool {
    let last = last_ms.load(Ordering::Relaxed);
    now_ms >= last.saturating_add(NONCE_WARN_INTERVAL_MS)
        && last_ms
            .compare_exchange(last, now_ms, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
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
    /// Cached `hl_submit_ack_seconds{transport="rest"}` handle (SPEC-0002 H-7).
    submit_histogram: metrics::Histogram,
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
            submit_histogram: metrics::histogram!(
                mev_metrics::names::SUBMIT_ACK_SECONDS,
                "transport" => "rest"
            ),
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
    pub async fn heal_nonce(&self) -> Result<u64> {
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
        let started = Instant::now();
        let resp = self
            .client
            .post(&url)
            .json(&request)
            .send()
            .await
            .map_err(|e| Error::Http(e.to_string()))?;
        // Submit-to-ack: request written through the venue's response
        // (SPEC-0002 H-7, `transport="rest"`).
        self.submit_histogram
            .record(started.elapsed().as_secs_f64());

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
    use crate::test_metrics::{counter_value, histogram_samples};
    use metrics_util::debugging::DebuggingRecorder;
    use mev_core::clock::FixedClock;
    use mev_metrics::names;
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
        // The durable value is the write-ahead lease, so it covers what was sent.
        let persisted = db.lock().unwrap().nonce_last().unwrap().unwrap();
        assert!(persisted > first, "{persisted} must cover {first}");

        // A fresh instance restores the persisted high-water mark and never
        // regresses. The first send may be refused until the write-behind lease
        // refreshes (accepted); every sent nonce must exceed the previous one.
        let restarted = HttpExchange::with_base_url(server.uri(), Mode::Live, Some(signer()))
            .unwrap()
            .with_nonce_db(db.clone())
            .unwrap();
        let resumed = restarted.last_nonce().await;
        assert!(resumed >= first, "{resumed} must not be below {first}");
        let mut advanced = false;
        for _ in 0..50 {
            match restarted.submit(&simple_action()).await {
                Ok(_) => {
                    advanced = true;
                    break;
                }
                Err(Error::NotSent(_)) => {}
                Err(other) => panic!("unexpected error: {other:?}"),
            }
        }
        assert!(advanced, "the restarted instance must eventually send");
        assert!(restarted.last_nonce().await > resumed);
    }

    #[tokio::test]
    async fn write_behind_nonce_survives_a_crash_without_a_flush() {
        // A fixed clock: the burst runs faster than 1/ms, so sent nonces run
        // ahead of the wall clock and the clock never advances to catch up.
        let clock = Arc::new(FixedClock::new(1_000_000));
        let db = Arc::new(Mutex::new(Db::open_in_memory().unwrap()));

        let mut sent = Vec::new();
        let prime;
        {
            let core = WriteCore::new(Mode::Live, Some(signer()))
                .unwrap()
                .with_clock(clock.clone())
                .with_nonce_lease(100)
                .with_nonce_db(db.clone())
                .unwrap();
            prime = db.lock().unwrap().nonce_last().unwrap().unwrap();
            for _ in 0..40 {
                match core.prepare(&simple_action()).await.unwrap() {
                    Prepared::Send(request) => sent.push(request.nonce),
                    Prepared::DryRun(_) => panic!("live mode must send"),
                }
            }
            // The burst fit inside the startup lease, so nothing was enqueued
            // write-behind: the database still holds only the lease.
            assert_eq!(db.lock().unwrap().nonce_last().unwrap(), Some(prime));
            // "Crash": drop without a graceful flush. Crash safety must not
            // depend on the per-order nonces.
        }
        let max_sent = *sent.iter().max().unwrap();
        assert_eq!(sent.len(), 40);
        assert!(
            max_sent < prime,
            "{max_sent} must fit inside the lease {prime}"
        );

        let restarted = WriteCore::new(Mode::Live, Some(signer()))
            .unwrap()
            .with_clock(clock)
            .with_nonce_lease(100)
            .with_nonce_db(db.clone())
            .unwrap();
        // The restored lease covers every sent nonce, so the first send after a
        // fast restart may be refused until the urgent refresh lands (the
        // accepted liveness cost); every send that does go out must be higher.
        let mut advanced = 0;
        for _ in 0..200 {
            match restarted.prepare(&simple_action()).await {
                Ok(Prepared::Send(request)) => {
                    assert!(
                        request.nonce > max_sent,
                        "restarted nonce {} reuses or regresses below {max_sent}",
                        request.nonce
                    );
                    advanced += 1;
                    if advanced == 10 {
                        break;
                    }
                }
                Ok(Prepared::DryRun(_)) => panic!("live mode must send"),
                Err(Error::NotSent(_)) => {}
                Err(other) => panic!("unexpected error: {other:?}"),
            }
        }
        assert_eq!(advanced, 10, "the restarted instance must resume sending");
    }

    #[tokio::test]
    async fn lease_refresh_is_persisted_write_behind() {
        let lease_ms = 100u64;
        let restored = 1_000_000u64;
        let clock = Arc::new(FixedClock::new(restored));
        let horizon = restored.max(restored + lease_ms);
        let durable = Arc::new(AtomicU64::new(0));
        let (tx, rx) = std::sync::mpsc::sync_channel(64);
        let core = WriteCore::new(Mode::Live, Some(signer()))
            .unwrap()
            .with_clock(clock)
            .with_nonce_lease(lease_ms)
            .with_test_nonce_store(
                NonceLease::with_channel(tx, horizon, lease_ms, durable.clone(), restored),
                restored,
            );

        // Send enough to spend the half-lease: a refresh is scheduled
        // write-behind and the durable horizon advances past the prime.
        let mut sent = Vec::new();
        for _ in 0..80 {
            match core.prepare(&simple_action()).await.unwrap() {
                Prepared::Send(request) => sent.push(request.nonce),
                Prepared::DryRun(_) => panic!("live mode must send"),
            }
        }
        let max_sent = *sent.iter().max().unwrap();

        // The refresh went to the writer channel, not to SQLite (no writer
        // thread, no sleeps): the highest queued horizon covers every send.
        let mut requested = horizon;
        while let Ok(value) = rx.try_recv() {
            requested = requested.max(value);
        }
        assert!(
            requested > horizon,
            "a write-behind refresh must be enqueued"
        );
        assert!(requested > max_sent, "the refresh must cover the burst");
        // The writer commits it: the confirmed durable mark advances.
        durable.fetch_max(requested, Ordering::AcqRel);
        assert!(durable.load(Ordering::Acquire) > horizon);
    }

    #[tokio::test]
    async fn restore_nonce_never_lowers_below_the_last_issued() {
        // Interleaving, set up deterministically (no sleeps): a concurrent
        // `prepare` reserved `sent` and scheduled a write-behind refresh, but
        // the confirmed durable mark still trails it. The floor is measured
        // under the nonce lock, so a restore to 0 cannot lower the manager
        // below `sent` and hand it out twice.
        let lease_ms = 100u64;
        let restored = 1_000_000u64;
        let clock = Arc::new(FixedClock::new(restored));
        let horizon = restored + lease_ms;
        let durable = Arc::new(AtomicU64::new(horizon));
        let (tx, _rx) = std::sync::mpsc::sync_channel(64);
        let core = WriteCore::new(Mode::Live, Some(signer()))
            .unwrap()
            .with_clock(clock)
            .with_nonce_lease(lease_ms)
            .with_test_nonce_store(
                NonceLease::with_channel(tx, horizon, lease_ms, durable, restored),
                restored,
            );

        let sent = core.nonce.lock().await.next(restored).unwrap();
        // The write-behind horizon moved, but nothing has confirmed it yet.
        core.nonce_store
            .as_ref()
            .unwrap()
            .force(sent + lease_ms, restored);

        core.restore_nonce(0).await.unwrap();
        assert!(
            core.last_nonce().await >= sent,
            "restore lowered below the reserved nonce {sent}"
        );
        let (durable, requested) = core.nonce_marks().unwrap();
        assert!(durable >= sent && requested >= sent);
        let next = core.nonce.lock().await.next(restored).unwrap();
        assert!(next > sent, "next nonce {next} must exceed {sent}");
    }

    #[tokio::test]
    async fn restore_nonce_never_lowers_the_durable_mark() {
        // The durable mark is ahead of the in-memory manager (a committed
        // lease): a restore below it must clamp up to the mark.
        let lease_ms = 100u64;
        let restored = 1_000_000u64;
        let clock = Arc::new(FixedClock::new(restored));
        let horizon = restored + lease_ms;
        let (tx, _rx) = std::sync::mpsc::sync_channel(64);
        let durable = Arc::new(AtomicU64::new(horizon));
        let core = WriteCore::new(Mode::Live, Some(signer()))
            .unwrap()
            .with_clock(clock)
            .with_nonce_lease(lease_ms)
            .with_test_nonce_store(
                NonceLease::with_channel(tx, horizon, lease_ms, durable, restored),
                restored,
            );

        core.restore_nonce(0).await.unwrap();
        let (durable, requested) = core.nonce_marks().unwrap();
        assert!(durable >= horizon, "{durable} lowered below {horizon}");
        assert!(requested >= horizon, "{requested} lowered below {horizon}");
        assert!(core.last_nonce().await >= horizon);
    }

    #[test]
    fn future_refusal_log_is_rate_limited() {
        let last = AtomicU64::new(0);
        assert!(refusal_log_due(&last, 1_000));
        assert!(!refusal_log_due(&last, 1_000));
        assert!(!refusal_log_due(&last, 1_000 + NONCE_WARN_INTERVAL_MS - 1));
        assert!(refusal_log_due(&last, 1_000 + NONCE_WARN_INTERVAL_MS));
        assert!(!refusal_log_due(&last, 1_000 + NONCE_WARN_INTERVAL_MS));
    }

    #[tokio::test]
    async fn prepare_does_not_write_the_database_synchronously() {
        let db = Arc::new(Mutex::new(Db::open_in_memory().unwrap()));
        // A lease far larger than the test burst guarantees no refresh is due.
        let core = WriteCore::new(Mode::Live, Some(signer()))
            .unwrap()
            .with_nonce_lease(1_000_000)
            .with_nonce_db(db.clone())
            .unwrap();
        let prime = db.lock().unwrap().nonce_last().unwrap().unwrap();
        let before = core.last_nonce().await;

        for _ in 0..50 {
            assert!(matches!(
                core.prepare(&simple_action()).await.unwrap(),
                Prepared::Send(_)
            ));
        }

        // Nonces advanced in memory, but the durable value is still the startup
        // prime: persistence went to the writer channel, not the database.
        assert!(core.last_nonce().await > before);
        assert_eq!(db.lock().unwrap().nonce_last().unwrap(), Some(prime));
    }

    #[tokio::test]
    async fn uncommitted_refresh_is_not_trusted_on_restart() {
        use std::sync::atomic::{AtomicU64, Ordering};

        let clock = Arc::new(FixedClock::new(1_000_000));
        let horizon = 1_000_100u64;
        let lease_ms = 100u64;
        let durable = Arc::new(AtomicU64::new(0));
        let (tx, rx) = std::sync::mpsc::sync_channel(64);
        // The startup prime is on disk; the sink accepts refreshes but never
        // commits them, simulating a writer killed mid-write.
        let lease = NonceLease::with_channel(tx, horizon, lease_ms, durable.clone(), 1_000_000);
        let core = WriteCore::new(Mode::Live, Some(signer()))
            .unwrap()
            .with_clock(clock.clone())
            .with_test_nonce_store(lease, 1_000_000);

        let mut sent = Vec::new();
        for _ in 0..200 {
            match core.prepare(&simple_action()).await {
                Ok(Prepared::Send(request)) => sent.push(request.nonce),
                Ok(Prepared::DryRun(_)) => panic!("live mode must send"),
                Err(Error::NotSent(_)) => break,
                Err(other) => panic!("unexpected error: {other:?}"),
            }
        }
        assert!(rx.try_recv().is_ok(), "a refresh must have been enqueued");
        assert_eq!(
            durable.load(Ordering::Acquire),
            horizon,
            "a killed writer must not have advanced the durable mark"
        );
        let max_sent = *sent.iter().max().unwrap();
        assert!(max_sent <= horizon, "{max_sent} must not exceed {horizon}");

        // Restart from the value the writer actually made durable.
        let restart_horizon = horizon.saturating_add(1).max(1_000_000 + lease_ms);
        let durable2 = Arc::new(AtomicU64::new(0));
        let (tx2, rx2) = std::sync::mpsc::sync_channel(64);
        let restarted = WriteCore::new(Mode::Live, Some(signer()))
            .unwrap()
            .with_clock(clock)
            .with_test_nonce_store(
                NonceLease::with_channel(
                    tx2,
                    restart_horizon,
                    lease_ms,
                    durable2.clone(),
                    1_000_000,
                ),
                horizon,
            );
        for _ in 0..10 {
            match restarted.prepare(&simple_action()).await.unwrap() {
                Prepared::Send(request) => assert!(
                    request.nonce > max_sent,
                    "{} must exceed {max_sent}",
                    request.nonce
                ),
                Prepared::DryRun(_) => panic!("live mode must send"),
            }
            // Pretend the writer commits the queued refresh.
            if let Ok(committed) = rx2.try_recv() {
                durable2.fetch_max(committed, Ordering::AcqRel);
            }
        }
    }

    #[tokio::test]
    async fn prepare_refuses_and_counts_when_the_writer_is_gone() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let clock = Arc::new(FixedClock::new(1_000_000));
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        drop(rx); // writer gone: every enqueue fails
        let core = metrics::with_local_recorder(&recorder, || {
            let durable = Arc::new(std::sync::atomic::AtomicU64::new(0));
            WriteCore::new(Mode::Live, Some(signer()))
                .unwrap()
                .with_clock(clock)
                .with_test_nonce_store(
                    NonceLease::with_channel(tx, 1_000_100, 100, durable, 1_000_000),
                    1_000_000,
                )
        });

        let mut refused = false;
        for _ in 0..200 {
            match core.prepare(&simple_action()).await {
                Ok(Prepared::Send(_)) => {}
                Ok(Prepared::DryRun(_)) => panic!("live mode must send"),
                Err(Error::NotSent(_)) => {
                    refused = true;
                    break;
                }
                Err(other) => panic!("unexpected error: {other:?}"),
            }
        }
        assert!(refused, "an absent writer must fail closed");
        assert_eq!(
            counter_value(
                snapshotter.snapshot(),
                names::NONCE_LEASE_REFUSALS_TOTAL,
                None
            ),
            1
        );
        assert!(
            counter_value(
                snapshotter.snapshot(),
                names::NONCE_PERSIST_DROPPED_TOTAL,
                None
            ) >= 1
        );
    }

    #[tokio::test]
    async fn orders_resume_after_a_stalled_write_recovers() {
        use std::sync::atomic::{AtomicU64, Ordering};

        let clock = Arc::new(FixedClock::new(1_000_000));
        let durable = Arc::new(AtomicU64::new(0));
        let (tx, rx) = std::sync::mpsc::sync_channel::<u64>(1);
        let horizon = 1_000_100u64;
        let lease = NonceLease::with_channel(tx, horizon, 100, durable.clone(), 1_000_000);
        let core = WriteCore::new(Mode::Live, Some(signer()))
            .unwrap()
            .with_clock(clock)
            .with_test_nonce_store(lease, 1_000_000);

        let mut refused = false;
        for _ in 0..200 {
            match core.prepare(&simple_action()).await {
                Ok(Prepared::Send(_)) => {}
                Ok(Prepared::DryRun(_)) => panic!("live mode must send"),
                Err(Error::NotSent(_)) => {
                    refused = true;
                    break;
                }
                Err(other) => panic!("unexpected error: {other:?}"),
            }
        }
        assert!(refused, "a stalled writer must fail closed");

        // The writer recovers and commits the highest queued horizon.
        let mut committed = horizon;
        while let Ok(value) = rx.try_recv() {
            committed = committed.max(value);
        }
        assert!(committed > horizon, "a refresh must have been queued");
        durable.store(committed, Ordering::Release);

        match core.prepare(&simple_action()).await {
            Ok(Prepared::Send(_)) => {}
            other => panic!("orders should resume after the write recovers: {other:?}"),
        }
    }

    #[tokio::test]
    async fn corrupt_persisted_nonce_fails_closed_until_reset() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let clock = Arc::new(FixedClock::new(1_000_000));
        let db = Arc::new(Mutex::new(Db::open_in_memory().unwrap()));
        // Beyond the venue's future window: it cannot be one we sent.
        db.lock()
            .unwrap()
            .set_nonce_last(1_000_000 + VENUE_MAX_FUTURE_MS + 1)
            .unwrap();

        let core = metrics::with_local_recorder(&recorder, || {
            WriteCore::new(Mode::Live, Some(signer()))
                .unwrap()
                .with_clock(clock)
                .with_nonce_db(db.clone())
                .unwrap()
        });

        // Fail closed: a typed refusal, and the corruption is counted.
        assert!(matches!(
            core.prepare(&simple_action()).await,
            Err(Error::NotSent(_))
        ));
        assert_eq!(
            counter_value(
                snapshotter.snapshot(),
                names::NONCE_RESUME_CORRUPT_TOTAL,
                None
            ),
            1
        );

        // The explicit operator reset clears it and orders flow again.
        core.reset_nonce().await.unwrap();
        assert!(matches!(
            core.prepare(&simple_action()).await.unwrap(),
            Prepared::Send(_)
        ));
    }

    #[tokio::test]
    async fn runtime_future_refusal_uses_its_own_counter() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let now = 1_000_000u64;
        let clock = Arc::new(FixedClock::new(now));
        // A manager already beyond the venue future window: every `next` fails
        // closed until the clock catches up.
        let resume_from = now + VENUE_MAX_FUTURE_MS + 1;
        let (tx, _rx) = std::sync::mpsc::sync_channel(64);
        let core = metrics::with_local_recorder(&recorder, || {
            let durable = Arc::new(AtomicU64::new(resume_from));
            WriteCore::new(Mode::Live, Some(signer()))
                .unwrap()
                .with_clock(clock)
                .with_test_nonce_store(
                    NonceLease::with_channel(tx, resume_from, DEFAULT_NONCE_LEASE_MS, durable, now),
                    resume_from,
                )
        });

        for _ in 0..3 {
            assert!(matches!(
                core.prepare(&simple_action()).await,
                Err(Error::NotSent(_))
            ));
        }
        assert_eq!(
            counter_value(
                snapshotter.snapshot(),
                names::NONCE_FUTURE_REFUSALS_TOTAL,
                None
            ),
            3
        );
        assert_eq!(
            counter_value(
                snapshotter.snapshot(),
                names::NONCE_RESUME_CORRUPT_TOTAL,
                None
            ),
            0,
            "a runtime refusal must not reuse the boot corruption counter"
        );
    }

    #[tokio::test]
    async fn reset_nonce_is_refused_while_not_corrupt() {
        let clock = Arc::new(FixedClock::new(1_000_000));
        let db = Arc::new(Mutex::new(Db::open_in_memory().unwrap()));
        let core = WriteCore::new(Mode::Live, Some(signer()))
            .unwrap()
            .with_clock(clock)
            .with_nonce_db(db)
            .unwrap();
        // Not corrupt: nonces may already have been issued, so a reset (which
        // may lower the value) could force a reuse and must be refused.
        match core.reset_nonce().await {
            Err(Error::Config(message)) => {
                assert!(message.contains("not flagged corrupt"), "{message}")
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        // The refusal must not disturb the send path.
        assert!(matches!(
            core.prepare(&simple_action()).await.unwrap(),
            Prepared::Send(_)
        ));
    }

    #[tokio::test]
    async fn heal_nonce_is_refused_while_corrupt() {
        let clock = Arc::new(FixedClock::new(1_000_000));
        let db = Arc::new(Mutex::new(Db::open_in_memory().unwrap()));
        db.lock()
            .unwrap()
            .set_nonce_last(1_000_000 + VENUE_MAX_FUTURE_MS + 1)
            .unwrap();
        let core = WriteCore::new(Mode::Live, Some(signer()))
            .unwrap()
            .with_clock(clock)
            .with_nonce_db(db)
            .unwrap();
        match core.heal_nonce().await {
            Err(Error::NotSent(message)) => assert!(message.contains("corrupt"), "{message}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn reset_lowers_the_durable_and_requested_marks_together() {
        let clock = Arc::new(FixedClock::new(1_000_000));
        let db = Arc::new(Mutex::new(Db::open_in_memory().unwrap()));
        let corrupt = 1_000_000 + VENUE_MAX_FUTURE_MS + 1;
        db.lock().unwrap().set_nonce_last(corrupt).unwrap();
        let core = WriteCore::new(Mode::Live, Some(signer()))
            .unwrap()
            .with_clock(clock)
            .with_nonce_lease(100)
            .with_nonce_db(db.clone())
            .unwrap();
        // At boot the in-memory marks are the corrupt value and nothing was
        // sent, so the reset is allowed.
        assert_eq!(core.nonce_marks(), Some((corrupt, corrupt)));
        core.reset_nonce().await.unwrap();
        // The database, the durable atomic, and the requested horizon all move
        // together to the new lease: no stale higher request survives.
        assert_eq!(core.nonce_marks(), Some((1_000_100, 1_000_100)));
        assert_eq!(db.lock().unwrap().nonce_last().unwrap(), Some(1_000_100));
        assert!(matches!(
            core.prepare(&simple_action()).await.unwrap(),
            Prepared::Send(_)
        ));
    }

    #[tokio::test]
    async fn crash_loop_never_reuses_a_nonce() {
        use std::sync::atomic::AtomicU64;

        // Deterministic and fast: a test channel stands in for the writer, the
        // clock advances a lease per boot, and the write-behind is never
        // committed (the worst case: every write-behind write is lost in the
        // crash). The lease is small so the test does a handful of signs, not
        // thousands.
        let lease_ms = 4u64;
        let mut restored = 1_000_000u64;
        let mut sent: Vec<u64> = Vec::new();
        for run in 0..100u64 {
            let now = 1_000_000 + run * lease_ms;
            let clock = Arc::new(FixedClock::new(now));
            // The boot prime `max(restored, now + lease)` is written
            // synchronously and is what the next boot restores.
            let horizon = restored.max(now + lease_ms);
            let durable = Arc::new(AtomicU64::new(0));
            let (tx, _rx) = std::sync::mpsc::sync_channel(64);
            let core = WriteCore::new(Mode::Live, Some(signer()))
                .unwrap()
                .with_clock(clock.clone())
                .with_nonce_lease(lease_ms)
                .with_test_nonce_store(
                    NonceLease::with_channel(tx, horizon, lease_ms, durable, now),
                    restored,
                );
            // Enough attempts to spend the boot headroom and hit the
            // write-behind/refusal path at least once.
            for _ in 0..(lease_ms + 2) {
                match core.prepare(&simple_action()).await {
                    Ok(Prepared::Send(request)) => {
                        assert!(
                            sent.iter().all(|&previous| request.nonce > previous),
                            "nonce {} reused",
                            request.nonce
                        );
                        sent.push(request.nonce);
                    }
                    Ok(Prepared::DryRun(_)) => panic!("live mode must send"),
                    Err(Error::NotSent(_)) => {}
                    Err(other) => panic!("unexpected error: {other:?}"),
                }
            }
            // Crash: the next boot restores exactly the prime written above.
            restored = horizon;
        }
        assert!(!sent.is_empty(), "each boot must send from its prime");
    }

    #[tokio::test]
    async fn restart_after_a_committed_refresh_never_reuses() {
        use std::sync::atomic::{AtomicU64, Ordering};

        let clock = Arc::new(FixedClock::new(1_000_000));
        let lease_ms = 100u64;
        let restored = 1_000_000u64;
        let horizon = restored.max(1_000_000 + lease_ms);
        let durable = Arc::new(AtomicU64::new(0));
        let (tx, rx) = std::sync::mpsc::sync_channel(64);
        let mut sent = Vec::new();
        {
            let core = WriteCore::new(Mode::Live, Some(signer()))
                .unwrap()
                .with_clock(clock.clone())
                .with_nonce_lease(lease_ms)
                .with_test_nonce_store(
                    NonceLease::with_channel(tx, horizon, lease_ms, durable.clone(), 1_000_000),
                    restored,
                );
            for _ in 0..160 {
                match core.prepare(&simple_action()).await {
                    Ok(Prepared::Send(request)) => sent.push(request.nonce),
                    Ok(Prepared::DryRun(_)) => panic!("live mode must send"),
                    Err(Error::NotSent(_)) => {}
                    Err(other) => panic!("unexpected error: {other:?}"),
                }
            }
        }
        // Explicitly commit the highest queued refresh, as the writer thread
        // does after a successful SQLite write.
        let mut committed = horizon;
        while let Ok(value) = rx.try_recv() {
            committed = committed.max(value);
        }
        assert!(committed > horizon, "a refresh must have been enqueued");
        durable.fetch_max(committed, Ordering::AcqRel);
        let max_sent = *sent.iter().max().unwrap();
        assert!(
            committed > max_sent,
            "the committed horizon {committed} must cover every sent nonce, max {max_sent}"
        );

        // Restart in the same frozen millisecond, restoring the committed mark.
        let durable2 = Arc::new(AtomicU64::new(0));
        let (tx2, rx2) = std::sync::mpsc::sync_channel(64);
        let restarted = WriteCore::new(Mode::Live, Some(signer()))
            .unwrap()
            .with_clock(clock)
            .with_nonce_lease(lease_ms)
            .with_test_nonce_store(
                NonceLease::with_channel(
                    tx2,
                    committed.max(1_000_000 + lease_ms),
                    lease_ms,
                    durable2.clone(),
                    1_000_000,
                ),
                committed,
            );
        let mut next = Vec::new();
        for _ in 0..160 {
            match restarted.prepare(&simple_action()).await {
                Ok(Prepared::Send(request)) => next.push(request.nonce),
                Ok(Prepared::DryRun(_)) => panic!("live mode must send"),
                Err(Error::NotSent(_)) => {}
                Err(other) => panic!("unexpected error: {other:?}"),
            }
            // Explicitly commit any queued refresh (the writer thread's job).
            while let Ok(value) = rx2.try_recv() {
                durable2.fetch_max(value, Ordering::AcqRel);
            }
        }
        assert!(
            !next.is_empty(),
            "the restarted instance must resume sending"
        );
        for nonce in next {
            assert!(
                nonce > max_sent,
                "restart nonce {nonce} reused <= {max_sent}"
            );
        }
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

    #[tokio::test]
    async fn prepare_records_the_sign_histogram() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        // Construct inside the local recorder so the cached handle binds to it.
        let exchange = metrics::with_local_recorder(&recorder, || {
            HttpExchange::with_base_url("http://127.0.0.1:1", Mode::Simulate, Some(signer()))
                .unwrap()
        });
        // `simulate` signs (the `prepare` stage) but never posts.
        exchange.submit(&simple_action()).await.unwrap();
        assert_eq!(
            histogram_samples(
                snapshotter.snapshot(),
                mev_metrics::names::SIGN_SECONDS,
                None,
            ),
            1,
        );
    }

    #[tokio::test]
    async fn live_post_records_the_rest_submit_ack_histogram() {
        let server = MockServer::start().await;
        let body = r#"{"status":"ok","response":{"data":{"statuses":["resting"]}}}"#;
        Mock::given(method("POST"))
            .and(path("/exchange"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        // Construct inside the local recorder so the cached handle binds to it.
        let exchange = metrics::with_local_recorder(&recorder, || {
            HttpExchange::with_base_url(server.uri(), Mode::Live, Some(signer())).unwrap()
        });
        exchange.submit(&simple_action()).await.unwrap();
        assert_eq!(
            histogram_samples(
                snapshotter.snapshot(),
                mev_metrics::names::SUBMIT_ACK_SECONDS,
                Some(("transport", "rest")),
            ),
            1,
        );
    }
}
