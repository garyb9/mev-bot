//! Risk engine, portfolio state, and accounting.
//!
//! See SPEC-0004. The risk engine is the mandatory gate between strategy
//! [`mev_strategy::OrderIntent`]s and execution: every intent is approved,
//! resized, or rejected (fail-closed) before it can be submitted.
//!
//! This crate currently ships the minimal, fail-closed [`LimitRisk`] gate used
//! by M3. The full engine (drawdown/liquidation guard, reconciliation, PnL
//! attribution) lands with SPEC-0004.

pub mod halt;
pub mod limits;

pub use halt::TradingHalt;
pub use limits::{LimitRisk, Limits};

use mev_strategy::{AccountView, MarketView, OrderIntent};

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

impl Decision {
    /// Whether the decision permits submission.
    pub fn is_allowed(&self) -> bool {
        !matches!(self, Decision::Reject(_))
    }
}

/// Everything a risk check may read. Views are borrowed and immutable.
pub struct RiskContext<'a> {
    /// Current market snapshot.
    pub market: &'a MarketView,
    /// Current account snapshot.
    pub account: &'a AccountView,
    /// Decision time in milliseconds.
    pub now_ms: u64,
}

/// A pre-trade risk check. Implementations are synchronous and deterministic.
///
/// `check` takes `&mut self` so an implementation can account for intents it
/// already approved in the same decision cycle (pending exposure the account
/// snapshot does not yet reflect); see [`limits::LimitRisk::begin_cycle`].
pub trait RiskCheck: Send + Sync {
    /// Evaluate an intent against current limits and portfolio state.
    fn check(&mut self, intent: &OrderIntent, ctx: &RiskContext<'_>) -> Decision;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_predicate() {
        assert!(Decision::Approve.is_allowed());
        assert!(Decision::Resize(rust_decimal::Decimal::ONE).is_allowed());
        assert!(!Decision::Reject("no".into()).is_allowed());
    }
}
