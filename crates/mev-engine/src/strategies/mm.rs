//! Market-making strategy, v2 (SPEC-0003 §7, SPEC-0010 E-4).
//!
//! Quotes a maker ladder around the mid on a coin, skewing the reservation
//! price against accumulated inventory and capping the position. Quotes are
//! refreshed when the mid moves past a refresh threshold, or pulled when
//! volatility widens the spread beyond a pull threshold.
//!
//! Ported from `mev-strategy::mm` in E-4: same ladder math, now driven
//! synchronously through [`Strategy`] with engine-interned coins, and
//! re-quoting a stable ladder with [`Action::Modify`] (one venue action per
//! level) instead of cancel + place.

use rust_decimal::Decimal;

use mev_strategy::{CostModel, OrderIntent, Side, StrategyId, TimeInForce};

use crate::strategy::{Actions, Ctx, Interests, OrderEvent, Strategy, Stream};
use crate::types::{CoinId, Px, Sz};

use super::book_view;

/// Stable strategy id.
pub const ID: &str = "market_making";

/// Configuration for [`MarketMaker`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MmConfig {
    /// Interned coin to quote.
    pub coin: CoinId,
    /// Size decimals for the book view.
    pub sz_decimals: u32,
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

/// One resting quote the strategy owns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ActiveQuote {
    cloid: crate::types::Cloid,
    side: Side,
    px: Px,
    sz: Sz,
}

/// The market-making strategy.
pub struct MarketMaker {
    config: MmConfig,
    /// Stable per-quote cloids, keyed by `(side, level)`, so a re-quote can use
    /// `Modify` on the same order rather than cancel + place.
    cloids: Vec<crate::types::Cloid>,
    active: Vec<ActiveQuote>,
    last_mid: Option<Decimal>,
}

impl MarketMaker {
    /// Build the strategy.
    pub fn new(config: MmConfig) -> Self {
        let cloids = (0..config.levels)
            .flat_map(|level| {
                [Side::Buy, Side::Sell].map(|side| Self::cloid(config.coin, level, side))
            })
            .collect();
        Self {
            config,
            cloids,
            active: Vec::new(),
            last_mid: None,
        }
    }

    /// Number of resting quote levels currently tracked.
    pub fn active_quotes(&self) -> usize {
        self.active.len()
    }

    /// Deterministic client order id for a quote: stable for a
    /// `(coin, level, side)` so re-quotes can target the same order.
    fn cloid(coin: CoinId, level: u32, side: Side) -> crate::types::Cloid {
        let mut bytes = [0u8; 16];
        bytes[0..2].copy_from_slice(&coin.0.to_le_bytes());
        bytes[2..6].copy_from_slice(&level.to_le_bytes());
        bytes[6] = if side.is_buy() { 0 } else { 1 };
        crate::types::Cloid(bytes)
    }

    fn cancel_all(&mut self, out: &mut Actions) {
        for quote in self.active.drain(..) {
            out.cancel(quote.cloid);
        }
        self.last_mid = None;
    }

    /// Desired ladder as `(side, price)` pairs for the current inventory.
    fn ladder(&self, mid: Decimal, inventory: Decimal) -> Vec<(Side, Px)> {
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

    fn cloid_for(&self, side: Side, level: u32) -> crate::types::Cloid {
        let index = match side {
            Side::Buy => 0,
            Side::Sell => 1,
        };
        self.cloids
            .get((level as usize) * 2 + index)
            .copied()
            .unwrap_or(crate::types::Cloid([0xFF; 16]))
    }

    fn quote_intent(
        &self,
        ctx: &Ctx<'_>,
        side: Side,
        px: Px,
        level: u32,
        now_ms: u64,
        cloid: Option<crate::types::Cloid>,
    ) -> OrderIntent {
        OrderIntent {
            strategy: StrategyId::from(ID),
            coin: ctx.coin_name(self.config.coin).to_string(),
            side,
            limit_px: Some(px),
            size: self.config.size_per_level,
            tif: TimeInForce::Alo,
            reduce_only: false,
            rationale: format!(
                "mm quote L{level} {}",
                if side.is_buy() { "bid" } else { "ask" }
            ),
            cloid: cloid.map(crate::types::Cloid::to_hex),
            signal_ms: now_ms,
            decision_ms: now_ms,
        }
    }
}

/// The event time in milliseconds since the Unix epoch, from the receive clock.
fn stamp_ms(ctx: &Ctx<'_>) -> u64 {
    u64::try_from(ctx.now.t_recv_ns / 1_000_000).unwrap_or(0)
}

impl Strategy for MarketMaker {
    fn id(&self) -> StrategyId {
        StrategyId::from(ID)
    }

    fn cost(&self) -> CostModel {
        CostModel::default()
    }

    fn interests(&self) -> Interests {
        Interests {
            coins: vec![
                (self.config.coin, Stream::Book),
                (self.config.coin, Stream::Ctx),
            ],
            timers_ms: vec![1_000],
        }
    }

    fn on_market(&mut self, _coin: CoinId, ctx: &Ctx<'_>, out: &mut Actions) {
        let now_ms = stamp_ms(ctx);
        let Some(slot) = ctx.slot(self.config.coin) else {
            self.cancel_all(out);
            return;
        };
        let Some(book) = book_view(slot, self.config.sz_decimals) else {
            self.cancel_all(out);
            return;
        };
        let Some(mid) = book.mid() else {
            self.cancel_all(out);
            return;
        };

        // Volatility guard: widen/pull when the spread blows out.
        if let Some(spread) = book.spread() {
            let spread_bps = spread / mid * Decimal::from(10_000);
            if spread_bps > self.config.vol_pull_bps {
                self.cancel_all(out);
                return;
            }
        }

        // Replace only when the mid has moved enough, or we have no quotes.
        if !self.active.is_empty()
            && let Some(last) = self.last_mid
        {
            let moved_bps = (mid - last).abs() / mid * Decimal::from(10_000);
            if moved_bps < self.config.refresh_bps {
                return;
            }
        }

        let inventory = ctx.account.position_szi(self.config.coin);
        let ladder = self.ladder(mid, inventory);

        // Re-quote in place with `Modify` when the ladder shape is unchanged
        // (same sides in the same order, only prices moved); otherwise cancel
        // the old ladder and place a fresh one.
        let shape_unchanged = ladder.len() == self.active.len()
            && self
                .active
                .iter()
                .zip(ladder.iter())
                .all(|(quote, (side, _))| quote.side == *side);
        if shape_unchanged {
            for (quote, (_, px)) in self.active.iter_mut().zip(ladder.iter()) {
                quote.px = *px;
                out.modify(quote.cloid, *px, quote.sz);
            }
            self.last_mid = Some(mid);
            return;
        }

        self.cancel_all(out);
        self.active.clear();
        for (level, (side, px)) in ladder.into_iter().enumerate() {
            let cloid = self.cloid_for(side, level as u32);
            self.active.push(ActiveQuote {
                cloid,
                side,
                px,
                sz: self.config.size_per_level,
            });
            out.place(self.quote_intent(ctx, side, px, level as u32, now_ms, Some(cloid)));
        }
        self.last_mid = Some(mid);
    }

    fn on_order(&mut self, _update: &OrderEvent, _ctx: &Ctx<'_>, _out: &mut Actions) {}
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;
    use crate::strategies::test_support::{
        account_with, ctx_with_registry, market_one_bbo, registry,
    };
    use crate::strategy::Action;

    fn ds(value: &str) -> Decimal {
        Decimal::from_str(value).unwrap()
    }

    fn config() -> MmConfig {
        MmConfig {
            coin: CoinId(0),
            sz_decimals: 3,
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

    fn run_once(strategy: &mut MarketMaker, market: &[crate::state::MarketSlot]) -> Vec<Action> {
        let account = crate::state::AccountState::new(1);
        run_with(strategy, market, &account)
    }

    fn run_with(
        strategy: &mut MarketMaker,
        market: &[crate::state::MarketSlot],
        account: &crate::state::AccountState,
    ) -> Vec<Action> {
        let registry = registry();
        let ctx = ctx_with_registry(market, account, 0, &registry);
        let mut actions = Actions::new();
        strategy.on_market(CoinId(0), &ctx, &mut actions);
        actions.take().into_vec()
    }

    fn places(actions: &[Action]) -> Vec<&OrderIntent> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Place(i) => Some(i),
                _ => None,
            })
            .collect()
    }

    fn cancels(actions: &[Action]) -> usize {
        actions
            .iter()
            .filter(|a| matches!(a, Action::Cancel { .. }))
            .count()
    }

    fn modifies(actions: &[Action]) -> usize {
        actions
            .iter()
            .filter(|a| matches!(a, Action::Modify { .. }))
            .count()
    }

    #[test]
    fn places_a_symmetric_ladder_when_flat() {
        let mut strategy = MarketMaker::new(config());
        let actions = run_once(&mut strategy, &market_one_bbo("100", "100"));
        let quotes = places(&actions);
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

    #[test]
    fn skews_quotes_down_when_long() {
        let mut flat_strategy = MarketMaker::new(config());
        let flat = run_once(&mut flat_strategy, &market_one_bbo("100", "100"));
        let flat_bid = places(&flat)
            .iter()
            .filter(|q| q.side == Side::Buy)
            .map(|q| q.limit_px.unwrap())
            .max()
            .unwrap();

        let mut long_strategy = MarketMaker::new(config());
        let long = run_with(
            &mut long_strategy,
            &market_one_bbo("100", "100"),
            &account_with(CoinId(0), "5"),
        );
        let long_bid = places(&long)
            .iter()
            .filter(|q| q.side == Side::Buy)
            .map(|q| q.limit_px.unwrap())
            .max()
            .unwrap();
        assert!(long_bid < flat_bid);
    }

    #[test]
    fn stops_bidding_at_inventory_cap() {
        let mut strategy = MarketMaker::new(config());
        let actions = run_with(
            &mut strategy,
            &market_one_bbo("100", "100"),
            &account_with(CoinId(0), "11"),
        );
        let quotes = places(&actions);
        assert!(quotes.iter().all(|q| q.side == Side::Sell));
    }

    #[test]
    fn pulls_quotes_on_wide_spread() {
        let mut strategy = MarketMaker::new(config());
        let _ = run_once(&mut strategy, &market_one_bbo("100", "100"));
        assert_eq!(strategy.active_quotes(), 4);
        let actions = run_once(&mut strategy, &market_one_bbo("100", "102"));
        assert_eq!(places(&actions).len(), 0);
        assert_eq!(cancels(&actions), 4);
        assert_eq!(strategy.active_quotes(), 0);
    }

    #[test]
    fn does_not_refresh_when_mid_is_stable() {
        let mut strategy = MarketMaker::new(config());
        let _ = run_once(&mut strategy, &market_one_bbo("100", "100"));
        let actions = run_once(&mut strategy, &market_one_bbo("100", "100"));
        assert!(actions.is_empty());
    }

    #[test]
    fn modifies_in_place_when_mid_moves() {
        let mut strategy = MarketMaker::new(config());
        let first = run_once(&mut strategy, &market_one_bbo("100", "100"));
        assert_eq!(cancels(&first), 0);

        // Mid moves from 100 to 101 (100 bps) > refresh threshold; the ladder
        // shape is unchanged, so each level is modified in place (no cancels).
        let second = run_once(&mut strategy, &market_one_bbo("101", "101"));
        assert_eq!(cancels(&second), 0);
        assert_eq!(modifies(&second), 4);
        assert_eq!(places(&second).len(), 0);
        assert_eq!(strategy.active_quotes(), 4);
    }

    #[test]
    fn rebuilds_when_the_inventory_cap_removes_a_side() {
        let mut strategy = MarketMaker::new(config());
        let _ = run_once(&mut strategy, &market_one_bbo("100", "100"));
        assert_eq!(strategy.active_quotes(), 4);
        // Inventory at the cap: only asks remain, so the ladder shape changes
        // and the engine must cancel the flat ladder and rebuild.
        let actions = run_with(
            &mut strategy,
            &market_one_bbo("101", "101"),
            &account_with(CoinId(0), "11"),
        );
        assert_eq!(cancels(&actions), 4);
        assert_eq!(places(&actions).len(), 2);
    }
}
