//! Risk engine, portfolio state, and accounting.
//!
//! See SPEC-0004. The risk engine is the mandatory gate between strategy
//! [`mev_strategy::OrderIntent`]s and execution: every intent is approved,
//! resized, or rejected (fail-closed) before it can be submitted.

use mev_strategy::OrderIntent;

/// Outcome of a pre-trade risk check.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// The intent may proceed unchanged.
    Approve,
    /// The intent may proceed with a reduced size.
    Resize(rust_decimal::Decimal),
    /// The intent must not proceed; reason is logged and metered.
    Reject(String),
}

/// A pre-trade risk check. Implementations are synchronous and deterministic.
pub trait RiskCheck: Send + Sync {
    /// Evaluate an intent against current limits and portfolio state.
    fn check(&self, intent: &OrderIntent) -> Decision;
}
