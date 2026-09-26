//! The pluggable [`Strategy`] trait and its decision context (SPEC-0003 §4, §8).

use std::time::Duration;

use async_trait::async_trait;
use mev_core::error::Result;
use mev_hl_client::ws::Subscription;

use crate::action::Action;
use crate::event::FillEvent;
use crate::id::StrategyId;
use crate::view::{AccountView, MarketView};

/// What caused the current decision cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trigger {
    /// An L2 book update for `coin`.
    Book {
        /// Coin that changed.
        coin: String,
    },
    /// An asset-context update for `coin`.
    Ctx {
        /// Coin that changed.
        coin: String,
    },
    /// A trade print for `coin`.
    Trade {
        /// Coin that changed.
        coin: String,
    },
    /// An all-mids snapshot.
    Mids,
    /// Account state changed.
    Account,
    /// A fill arrived for `coin`.
    Fill {
        /// Coin that filled.
        coin: String,
    },
    /// A strategy timer fired.
    Timer,
}

/// Everything a strategy may read during one decision cycle.
///
/// Views are borrowed and immutable; timestamp and trigger are event-time so
/// replay is deterministic. Strategies own their own [`crate::event::DeterministicRng`].
pub struct StrategyContext<'a> {
    /// Event time in milliseconds.
    pub now_ms: u64,
    /// What triggered this cycle.
    pub trigger: Trigger,
    /// Current market snapshot.
    pub market: &'a MarketView,
    /// Current account snapshot.
    pub account: &'a AccountView,
}

/// A pluggable trading strategy.
///
/// Strategies turn views into [`OrderIntent`]s. They never sign, submit, or
/// hold portfolio risk; risk (SPEC-0004) gates their intents before execution.
#[async_trait]
pub trait Strategy: Send + Sync {
    /// Stable identifier used in metrics, persistence, and config.
    fn id(&self) -> StrategyId;

    /// Market subscriptions the engine must maintain for this strategy.
    fn subscriptions(&self) -> Vec<Subscription> {
        Vec::new()
    }

    /// Timers the engine should fire for this strategy.
    fn timers(&self) -> Vec<Duration> {
        Vec::new()
    }

    /// React to an event, returning orders to place and/or cancel.
    async fn on_event(&mut self, ctx: &StrategyContext<'_>) -> Result<Vec<Action>>;

    /// Observe a fill (own or simulated).
    async fn on_fill(&mut self, _fill: &FillEvent) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::{AccountView, MarketView};

    struct Noop;

    #[async_trait]
    impl Strategy for Noop {
        fn id(&self) -> StrategyId {
            StrategyId::from("noop")
        }

        async fn on_event(&mut self, _ctx: &StrategyContext<'_>) -> Result<Vec<Action>> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn default_methods_are_empty() {
        let mut strategy = Noop;
        assert_eq!(strategy.id().as_str(), "noop");
        assert!(strategy.subscriptions().is_empty());
        assert!(strategy.timers().is_empty());

        let market = MarketView::new();
        let account = AccountView::default();
        let ctx = StrategyContext {
            now_ms: 0,
            trigger: Trigger::Timer,
            market: &market,
            account: &account,
        };
        assert!(strategy.on_event(&ctx).await.unwrap().is_empty());
    }
}
