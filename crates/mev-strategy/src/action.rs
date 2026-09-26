//! What a strategy asks the engine to do this cycle (SPEC-0003 §4).

use serde::{Deserialize, Serialize};

use crate::id::StrategyId;
use crate::intent::OrderIntent;

/// A request to cancel a specific resting order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelIntent {
    /// The strategy that owns the order.
    pub strategy: StrategyId,
    /// Coin.
    pub coin: String,
    /// Client order id to cancel, when known.
    pub cloid: Option<String>,
    /// Exchange order id to cancel, when known.
    pub oid: Option<u64>,
}

/// A strategy decision: place an order or cancel one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    /// Place a new order.
    Place(OrderIntent),
    /// Cancel a resting order.
    Cancel(CancelIntent),
}

impl Action {
    /// The owning strategy.
    pub fn strategy(&self) -> &StrategyId {
        match self {
            Action::Place(intent) => &intent.strategy,
            Action::Cancel(cancel) => &cancel.strategy,
        }
    }
}

impl From<OrderIntent> for Action {
    fn from(intent: OrderIntent) -> Self {
        Action::Place(intent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intent::{Side, TimeInForce};
    use rust_decimal::Decimal;

    fn intent() -> OrderIntent {
        OrderIntent {
            strategy: StrategyId::from("mm"),
            coin: "BTC".into(),
            side: Side::Buy,
            limit_px: Some(Decimal::ONE),
            size: Decimal::ONE,
            tif: TimeInForce::Alo,
            reduce_only: false,
            rationale: "q".into(),
            cloid: Some("0x01".into()),
            signal_ms: 0,
            decision_ms: 0,
        }
    }

    #[test]
    fn strategy_accessor_and_from() {
        let action: Action = intent().into();
        assert_eq!(action.strategy().as_str(), "mm");
        let cancel = Action::Cancel(CancelIntent {
            strategy: StrategyId::from("mm"),
            coin: "BTC".into(),
            cloid: Some("0x01".into()),
            oid: None,
        });
        assert!(matches!(cancel, Action::Cancel(_)));
        let json = serde_json::to_string(&action).unwrap();
        assert!(json.contains("\"kind\":\"place\""));
    }
}
