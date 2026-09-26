//! Exchange-agnostic order intents (SPEC-0003 §4).

use mev_hl_client::order::Tif;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::id::StrategyId;

/// Which side of the book an intent is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    /// Buy / bid.
    Buy,
    /// Sell / ask.
    Sell,
}

impl Side {
    /// True for [`Side::Buy`].
    pub fn is_buy(self) -> bool {
        matches!(self, Side::Buy)
    }

    /// The opposite side.
    pub fn opposite(self) -> Self {
        match self {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        }
    }
}

/// Time in force for a proposed limit order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TimeInForce {
    /// Add-liquidity-only (post-only).
    Alo,
    /// Immediate-or-cancel.
    Ioc,
    /// Good-till-cancelled.
    Gtc,
}

impl From<TimeInForce> for Tif {
    fn from(value: TimeInForce) -> Self {
        match value {
            TimeInForce::Alo => Tif::Alo,
            TimeInForce::Ioc => Tif::Ioc,
            TimeInForce::Gtc => Tif::Gtc,
        }
    }
}

/// A fully-described, unsigned order proposal.
///
/// Intents are exchange-agnostic: strategies never build wire orders, sign, or
/// submit. Timestamps are event-time (ms) so replay is deterministic.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderIntent {
    /// The strategy that proposed this order.
    pub strategy: StrategyId,
    /// Canonical coin (e.g. `BTC`, `@107`, `xyz:TSLA`).
    pub coin: String,
    /// Direction.
    pub side: Side,
    /// Limit price, or `None` for an aggressive/market order.
    pub limit_px: Option<Decimal>,
    /// Size in base units.
    pub size: Decimal,
    /// Time in force.
    pub tif: TimeInForce,
    /// Reduce-only flag.
    pub reduce_only: bool,
    /// Human-readable justification, logged for audit.
    pub rationale: String,
    /// Event time the signal was observed (ms).
    pub signal_ms: u64,
    /// Event time the decision was made (ms).
    pub decision_ms: u64,
}

impl OrderIntent {
    /// True when this intent buys.
    pub fn is_buy(&self) -> bool {
        self.side.is_buy()
    }

    /// Notional using the limit price, or `reference_px` when there is none.
    pub fn notional(&self, reference_px: Decimal) -> Decimal {
        self.limit_px.unwrap_or(reference_px) * self.size
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent(side: Side, limit_px: Option<Decimal>, size: Decimal) -> OrderIntent {
        OrderIntent {
            strategy: StrategyId::from("test"),
            coin: "BTC".into(),
            side,
            limit_px,
            size,
            tif: TimeInForce::Alo,
            reduce_only: false,
            rationale: "test".into(),
            signal_ms: 1,
            decision_ms: 2,
        }
    }

    #[test]
    fn side_helpers() {
        assert!(Side::Buy.is_buy());
        assert!(!Side::Sell.is_buy());
        assert_eq!(Side::Buy.opposite(), Side::Sell);
        assert_eq!(Side::Sell.opposite(), Side::Buy);
    }

    #[test]
    fn notional_prefers_limit_price() {
        let with_limit = intent(Side::Buy, Some(Decimal::from(100)), Decimal::new(5, 1));
        assert_eq!(with_limit.notional(Decimal::from(90)), Decimal::from(50));

        let no_limit = intent(Side::Buy, None, Decimal::new(2, 0));
        assert_eq!(no_limit.notional(Decimal::from(90)), Decimal::from(180));
    }

    #[test]
    fn tif_maps_to_wire() {
        assert_eq!(Tif::from(TimeInForce::Alo), Tif::Alo);
        assert_eq!(Tif::from(TimeInForce::Ioc), Tif::Ioc);
        assert_eq!(Tif::from(TimeInForce::Gtc), Tif::Gtc);
    }
}
