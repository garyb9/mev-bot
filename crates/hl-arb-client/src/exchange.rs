//! Exchange client: signing envelope, transports, and typed responses
//! (SPEC-0002 §8–§9).
//!
//! Reads use [`InfoApi`]; writes go through [`ExchangeApi`]. Two transports sit
//! behind the trait — WebSocket `post` ([`crate::ws_exchange::WsExchange`], the
//! default) and REST `POST /exchange` ([`HttpExchange`], the fallback). This
//! module owns the shared signed-envelope/response types and the [`WriteCore`]
//! (gating, signing, nonce sequencing) both transports build on.

use std::time::Duration;

use async_trait::async_trait;

use hl_arb_core::error::Result;

use crate::order::{Action, CancelByCloidWire, CancelWire, OrderWire};

mod http;
mod response;
mod write_core;

pub use http::{ExchangeRequest, HttpExchange, WriteGate, build_request};
pub use response::{
    ActionResponse, ExchangeResponse, OrderResponse, OrderStatus, RejectReason, ReplyHandle,
    STATUS_ERR, STATUS_OK, parse_post_reply,
};
pub use write_core::WriteCore;

/// Connect timeout for every HTTP client in this crate: a dial that cannot open
/// a TCP connection within this bound fails with a typed error (SEC-001).
pub const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// Total request timeout for every HTTP client in this crate: a server that
/// accepts but never replies cannot hang a caller (SEC-001).
pub const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Build the shared `reqwest` client used by every HTTP call in this crate.
///
/// Both timeouts are bounded, so a black-holed endpoint fails with a typed
/// error instead of hanging. Tests use [`http_client_with`] to shorten them.
pub fn http_client_with(connect_timeout: Duration, request_timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(connect_timeout)
        .timeout(request_timeout)
        .build()
        .expect("constructing the shared HTTP client")
}

/// The shared HTTP client with the production timeouts
/// ([`HTTP_CONNECT_TIMEOUT`], [`HTTP_REQUEST_TIMEOUT`]).
pub fn http_client() -> reqwest::Client {
    http_client_with(HTTP_CONNECT_TIMEOUT, HTTP_REQUEST_TIMEOUT)
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

    /// Eagerly establish the transport before the first order (SPEC-0010 §12:
    /// connections are warmed at startup, never dialed on the order hot path).
    ///
    /// The default is a no-op: REST has no persistent connection. WebSocket
    /// transports dial here, so a cold connection is not paid for out of the
    /// first order's latency budget.
    async fn warm(&self) -> Result<()> {
        Ok(())
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

#[cfg(test)]
mod tests;
