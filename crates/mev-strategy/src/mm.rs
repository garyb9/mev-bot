//! Market-making strategy (SPEC-0003 §7).
//!
//! Quotes a maker ladder around the mid on a coin, skewing the reservation
//! price against accumulated inventory and capping the position. Quotes are
//! cancelled and replaced when the mid moves past a refresh threshold or when
//! volatility widens the spread beyond a pull threshold.

use std::time::Duration;

use async_trait::async_trait;
use mev_core::error::Result;
use mev_hl_client::ws::Subscription;
use rust_decimal::Decimal;

use crate::action::{Action, CancelIntent};
use crate::event::FillEvent;
use crate::id::StrategyId;
use crate::intent::{OrderIntent, Side, TimeInForce};
use crate::strategy::{Strategy, StrategyContext};

/// Stable strategy id.
pub const ID: &str = "market_making";

/// Configuration for [`MarketMaker`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MmConfig {
    /// Coin to quote.
    pub coin: String,
    /// Number of levels per side.
    pub levels: u32,
    /// Half-spread from the reservation price to the first level, in bps.
    pub half_spread_bps: Decimal,
    /// Spacing between successive levels, in bps.
    pub level_step_bps: Decimal,
    /// Size per level, in base units.
    pub size_per_level: Decimal,
    /// Absolute inventory cap, in base units.
    pub max_inventory: Decimal,
    /// Maximum reservation skew at full inventory, in bps.
    pub max_skew_bps: Decimal,
    /// Pull quotes when the book spread exceeds this, in bps.
    pub vol_pull_bps: Decimal,
    /// Replace quotes when the mid moves by at least this, in bps.
    pub refresh_bps: Decimal,
}

/// The market-making strategy.
pub struct MarketMaker {
    config: MmConfig,
    quote_seq: u64,
    active_cloids: Vec<String>,
    last_mid: Option<Decimal>,
}

impl MarketMaker {
    /// Build the strategy.
    pub fn new(config: MmConfig) -> Self {
        Self {
            config,
            quote_seq: 0,
            active_cloids: Vec::new(),
            last_mid: None,
        }
    }

    /// Number of resting quote levels currently tracked.
    pub fn active_quotes(&self) -> usize {
        self.active_cloids.len()
    }

    /// Deterministic client order id for a quote.
    fn cloid(coin: &str, seq: u64, level: u32, side: Side) -> String {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in coin.as_bytes() {
            hash ^= *byte as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        let side_bit = if side.is_buy() { 0 } else { 1 };
        format!(
            "0x{hash:016x}{:08x}{:08x}",
            seq as u32,
            (level << 1) | side_bit
        )
    }

    fn cancel_all(&mut self) -> Vec<Action> {
        let actions = self
            .active_cloids
            .drain(..)
            .map(|cloid| {
                Action::Cancel(CancelIntent {
                    strategy: StrategyId::from(ID),
                    coin: self.config.coin.clone(),
                    cloid: Some(cloid),
                    oid: None,
                })
            })
            .collect();
        self.last_mid = None;
        actions
    }

    /// Desired ladder as `(side, price)` pairs for the current inventory.
    fn ladder(&self, mid: Decimal, inventory: Decimal) -> Vec<(Side, Decimal)> {
        let max = self.config.max_inventory;
        let skew_bps = if max > Decimal::ZERO {
            (inventory / max * self.config.max_skew_bps)
                .clamp(-self.config.max_skew_bps, self.config.max_skew_bps)
        } else {
            Decimal::ZERO
        };
        let reservation = mid * (Decimal::ONE - skew_bps / Decimal::from(10_000));
        let can_buy = inventory < max;
        let can_sell = inventory > -max;

        let mut out = Vec::new();
        for level in 0..self.config.levels {
            let offset =
                self.config.half_spread_bps + self.config.level_step_bps * Decimal::from(level);
            let bid = reservation * (Decimal::ONE - offset / Decimal::from(10_000));
            let ask = reservation * (Decimal::ONE + offset / Decimal::from(10_000));
            if can_buy && bid > Decimal::ZERO {
                out.push((Side::Buy, bid));
            }
            if can_sell {
                out.push((Side::Sell, ask));
            }
        }
        out
    }

    fn quote_intent(
        &self,
        coin: &str,
        side: Side,
        px: Decimal,
        level: u32,
        seq: u64,
        now_ms: u64,
    ) -> OrderIntent {
        OrderIntent {
            strategy: StrategyId::from(ID),
            coin: coin.to_string(),
            side,
            limit_px: Some(px),
            size: self.config.size_per_level,
            tif: TimeInForce::Alo,
            reduce_only: false,
            rationale: format!(
                "mm quote L{level} {}",
                if side.is_buy() { "bid" } else { "ask" }
            ),
            cloid: Some(Self::cloid(coin, seq, level, side)),
            signal_ms: now_ms,
            decision_ms: now_ms,
        }
    }
}

#[async_trait]
impl Strategy for MarketMaker {
    fn id(&self) -> StrategyId {
        StrategyId::from(ID)
    }

    fn subscriptions(&self) -> Vec<Subscription> {
        vec![
            Subscription::L2Book {
                coin: self.config.coin.clone(),
            },
            Subscription::ActiveAssetCtx {
                coin: self.config.coin.clone(),
            },
        ]
    }

    fn timers(&self) -> Vec<Duration> {
        vec![Duration::from_secs(1)]
    }

    async fn on_event(&mut self, ctx: &StrategyContext<'_>) -> Result<Vec<Action>> {
        let now_ms = ctx.now_ms;
        let coin = self.config.coin.clone();
        let Some(book) = ctx.market.book(&coin) else {
            return Ok(self.cancel_all());
        };
        let Some(mid) = book.mid() else {
            return Ok(self.cancel_all());
        };

        // Volatility guard: widen/pull when the spread blows out.
        if let Some(spread) = book.spread() {
            let spread_bps = spread / mid * Decimal::from(10_000);
            if spread_bps > self.config.vol_pull_bps {
                return Ok(self.cancel_all());
            }
        }

        // Replace only when the mid has moved enough, or we have no quotes.
        if !self.active_cloids.is_empty()
            && let Some(last) = self.last_mid
        {
            let moved_bps = (mid - last).abs() / mid * Decimal::from(10_000);
            if moved_bps < self.config.refresh_bps {
                return Ok(Vec::new());
            }
        }

        let inventory = ctx.account.position_szi(&coin);
        let ladder = self.ladder(mid, inventory);

        let mut actions = self.cancel_all();
        self.quote_seq = self.quote_seq.wrapping_add(1);
        let seq = self.quote_seq;
        self.active_cloids.clear();
        for (level, (side, px)) in ladder.into_iter().enumerate() {
            let intent = self.quote_intent(&coin, side, px, level as u32, seq, now_ms);
            if let Some(cloid) = &intent.cloid {
                self.active_cloids.push(cloid.clone());
            }
            actions.push(Action::Place(intent));
        }
        self.last_mid = Some(mid);
        Ok(actions)
    }

    async fn on_fill(&mut self, _fill: &FillEvent) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;
    use crate::strategy::Trigger;
    use crate::view::{AccountView, BookView, MarketView, PositionView};

    fn ds(value: &str) -> Decimal {
        Decimal::from_str(value).unwrap()
    }

    fn market(bid: &str, ask: &str) -> MarketView {
        let mut market = MarketView::new();
        market.insert_book(
            "BTC",
            BookView {
                bids: vec![(ds(bid), ds("100"))],
                asks: vec![(ds(ask), ds("100"))],
                sz_decimals: 3,
                time: 0,
            },
        );
        market
    }

    fn config() -> MmConfig {
        MmConfig {
            coin: "BTC".into(),
            levels: 2,
            half_spread_bps: ds("5"),
            level_step_bps: ds("5"),
            size_per_level: ds("1"),
            max_inventory: ds("10"),
            max_skew_bps: ds("5"),
            vol_pull_bps: ds("50"),
            refresh_bps: ds("2"),
        }
    }

    fn account_with(inventory: &str) -> AccountView {
        let mut account = AccountView::default();
        if inventory != "0" {
            account.positions.insert(
                "BTC".into(),
                PositionView {
                    coin: "BTC".into(),
                    szi: ds(inventory),
                    ..Default::default()
                },
            );
        }
        account
    }

    async fn run_once(
        strategy: &mut MarketMaker,
        market: &MarketView,
        account: &AccountView,
    ) -> Vec<Action> {
        let ctx = StrategyContext {
            now_ms: 0,
            trigger: Trigger::Timer,
            market,
            account,
        };
        strategy.on_event(&ctx).await.unwrap()
    }

    fn places(actions: &[Action]) -> Vec<&OrderIntent> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Place(i) => Some(i),
                Action::Cancel(_) => None,
            })
            .collect()
    }

    fn cancels(actions: &[Action]) -> usize {
        actions
            .iter()
            .filter(|a| matches!(a, Action::Cancel(_)))
            .count()
    }

    #[tokio::test]
    async fn places_a_symmetric_ladder_when_flat() {
        let mut strategy = MarketMaker::new(config());
        let actions = run_once(&mut strategy, &market("100", "100"), &account_with("0")).await;
        let quotes = places(&actions);
        // 2 levels x 2 sides.
        assert_eq!(quotes.len(), 4);
        assert_eq!(cancels(&actions), 0);
        let bid = quotes.iter().find(|q| q.side == Side::Buy).unwrap();
        let ask = quotes.iter().find(|q| q.side == Side::Sell).unwrap();
        assert!(bid.limit_px.unwrap() < ds("100"));
        assert!(ask.limit_px.unwrap() > ds("100"));
        assert!(quotes.iter().all(|q| q.tif == TimeInForce::Alo));
        assert!(quotes.iter().all(|q| q.cloid.is_some()));
        assert_eq!(strategy.active_quotes(), 4);
    }

    #[tokio::test]
    async fn skews_quotes_down_when_long() {
        let mut strategy = MarketMaker::new(config());
        let flat = run_once(&mut strategy, &market("100", "100"), &account_with("0")).await;
        let flat_bid = places(&flat)
            .iter()
            .filter(|q| q.side == Side::Buy)
            .map(|q| q.limit_px.unwrap())
            .max()
            .unwrap();

        let mut strategy = MarketMaker::new(config());
        let long = run_once(&mut strategy, &market("100", "100"), &account_with("5")).await;
        let long_bid = places(&long)
            .iter()
            .filter(|q| q.side == Side::Buy)
            .map(|q| q.limit_px.unwrap())
            .max()
            .unwrap();
        // The reservation drops with long inventory, so bids sit lower.
        assert!(long_bid < flat_bid);
    }

    #[tokio::test]
    async fn stops_bidding_at_inventory_cap() {
        let mut strategy = MarketMaker::new(config());
        let actions = run_once(&mut strategy, &market("100", "100"), &account_with("11")).await;
        let quotes = places(&actions);
        assert!(quotes.iter().all(|q| q.side == Side::Sell));
    }

    #[tokio::test]
    async fn pulls_quotes_on_wide_spread() {
        let mut strategy = MarketMaker::new(config());
        // Seed some quotes, then widen the spread past the pull threshold.
        let _ = run_once(&mut strategy, &market("100", "100"), &account_with("0")).await;
        assert_eq!(strategy.active_quotes(), 4);
        let actions = run_once(&mut strategy, &market("100", "102"), &account_with("0")).await;
        assert_eq!(places(&actions).len(), 0);
        assert_eq!(cancels(&actions), 4);
        assert_eq!(strategy.active_quotes(), 0);
    }

    #[tokio::test]
    async fn does_not_refresh_when_mid_is_stable() {
        let mut strategy = MarketMaker::new(config());
        let _ = run_once(&mut strategy, &market("100", "100"), &account_with("0")).await;
        let actions = run_once(&mut strategy, &market("100", "100"), &account_with("0")).await;
        assert!(actions.is_empty());
    }

    #[tokio::test]
    async fn cancels_and_replaces_when_mid_moves() {
        let mut strategy = MarketMaker::new(config());
        let first = run_once(&mut strategy, &market("100", "100"), &account_with("0")).await;
        assert_eq!(cancels(&first), 0);

        // Mid moves from 100 to 101 (100 bps) > refresh threshold.
        let second = run_once(&mut strategy, &market("101", "101"), &account_with("0")).await;
        assert_eq!(cancels(&second), 4);
        assert_eq!(places(&second).len(), 4);
    }
}
