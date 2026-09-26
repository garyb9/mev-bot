//! Delta-neutral funding/basis strategy, v2 (SPEC-0003 §6, SPEC-0010 E-4).
//!
//! When perp funding is positive, shorts receive it. Holding spot long against
//! an equal-size perp short nets out price risk while collecting funding. The
//! strategy enters only when the funding projected over the holding horizon
//! clears the round-trip cost (fees + slippage) plus the cost model's buffer,
//! and exits after funding has stayed below a threshold for a set number of
//! hourly settlements.
//!
//! v1 trades the positive-funding direction only: Hyperliquid spot cannot be
//! shorted, so the negative-funding mirror image is out of scope (SPEC-0003 §15).
//!
//! Ported from `mev-strategy::funding` in E-4: same decision state machine, now
//! driven synchronously through [`Strategy`] with engine-interned coins.

use mev_strategy::{CostModel, FeeRates, OrderIntent, Side, StrategyId, TimeInForce};
use rust_decimal::Decimal;

use crate::state::AccountState;
use crate::strategy::{Actions, Ctx, Interests, OrderEvent, Strategy, Stream};
use crate::types::{CoinId, Px};

use super::book_view;

/// Stable strategy id.
pub const ID: &str = "funding_basis";

const MS_PER_HOUR: u64 = 3_600_000;

/// Configuration for [`FundingBasis`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FundingConfig {
    /// Interned perp coin to short.
    pub perp_coin: CoinId,
    /// Interned spot coin to buy.
    pub spot_coin: CoinId,
    /// Spot token used to read balances for drift checks (e.g. `UBTC`).
    pub spot_token: String,
    /// Size decimals for the spot book view.
    pub spot_sz_decimals: u32,
    /// Target notional per leg in USD.
    pub target_notional: Decimal,
    /// Holding horizon used to project funding, in hours.
    pub horizon_hours: Decimal,
    /// Funding (bps/hour) at or below which a settlement counts toward exit.
    pub exit_threshold_bps: Decimal,
    /// Consecutive hourly settlements below threshold before exiting.
    pub exit_after_hours: u32,
    /// Delta drift (bps of notional) that triggers a rebalance while hedged.
    pub rebalance_drift_bps: Decimal,
}

/// Decision state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Flat,
    Entering,
    Hedged,
    Exiting,
}

/// The funding/basis strategy.
pub struct FundingBasis {
    config: FundingConfig,
    cost: CostModel,
    perp_fees: FeeRates,
    spot_fees: FeeRates,
    maker: bool,
    state: State,
    entry_fills: u32,
    exit_fills: u32,
    last_hour: Option<u64>,
    below_streak: u32,
}

impl FundingBasis {
    /// Build the strategy.
    pub fn new(
        config: FundingConfig,
        cost: CostModel,
        perp_fees: FeeRates,
        spot_fees: FeeRates,
        maker: bool,
    ) -> Self {
        Self {
            config,
            cost,
            perp_fees,
            spot_fees,
            maker,
            state: State::Flat,
            entry_fills: 0,
            exit_fills: 0,
            last_hour: None,
            below_streak: 0,
        }
    }

    /// Current decision state (for tests/metrics).
    pub fn state(&self) -> &'static str {
        match self.state {
            State::Flat => "flat",
            State::Entering => "entering",
            State::Hedged => "hedged",
            State::Exiting => "exiting",
        }
    }

    /// Projected net edge in bps for the current market, or `None` when the
    /// books needed to price the trade are missing.
    pub fn edge_bps(&self, ctx: &Ctx<'_>) -> Option<Decimal> {
        let perp_px = ctx.mid(self.config.perp_coin)?;
        let spot_px = ctx.mid(self.config.spot_coin)?;
        let rate = ctx.funding(self.config.perp_coin)?;
        if perp_px <= Decimal::ZERO
            || spot_px <= Decimal::ZERO
            || self.config.target_notional <= Decimal::ZERO
        {
            return None;
        }

        let perp_size = self.config.target_notional / perp_px;
        let spot_size = self.config.target_notional / spot_px;
        let spot_book = self.book(ctx, self.config.spot_coin)?;
        let perp_book = self.book(ctx, self.config.perp_coin)?;

        let slippage = self.cost.slippage_bps(&spot_book, Side::Buy, spot_size)?
            + self.cost.slippage_bps(&perp_book, Side::Sell, perp_size)?
            + self.cost.slippage_bps(&spot_book, Side::Sell, spot_size)?
            + self.cost.slippage_bps(&perp_book, Side::Buy, perp_size)?;
        let fees = self.cost.round_trip_fee_bps(self.spot_fees, self.maker)
            + self.cost.round_trip_fee_bps(self.perp_fees, self.maker);
        let gross = self.cost.funding_bps(rate, self.config.horizon_hours);
        Some(self.cost.net_edge_bps(gross, fees + slippage))
    }

    fn book(&self, ctx: &Ctx<'_>, coin: CoinId) -> Option<mev_strategy::BookView> {
        let slot = ctx.slot(coin)?;
        let sz_decimals = if coin == self.config.spot_coin {
            self.config.spot_sz_decimals
        } else {
            // Perp size decimals are not needed for slippage math; the walk only
            // uses prices and sizes as-is.
            0
        };
        book_view(slot, sz_decimals)
    }

    #[allow(clippy::too_many_arguments)]
    fn leg_intent(
        &self,
        ctx: &Ctx<'_>,
        coin: CoinId,
        side: Side,
        size: Decimal,
        limit_px: Decimal,
        reduce_only: bool,
        now_ms: u64,
        rationale: String,
    ) -> OrderIntent {
        OrderIntent {
            strategy: StrategyId::from(ID),
            coin: ctx.coin_name(coin).to_string(),
            side,
            limit_px: Some(limit_px),
            size,
            tif: if self.maker {
                TimeInForce::Alo
            } else {
                TimeInForce::Ioc
            },
            reduce_only,
            rationale,
            cloid: None,
            signal_ms: now_ms,
            decision_ms: now_ms,
        }
    }

    /// Crossable (taker) or inside-spread (maker) limit price for a side.
    fn limit_px(ctx: &Ctx<'_>, coin: CoinId, side: Side, maker: bool) -> Option<Px> {
        match (maker, side) {
            (false, Side::Buy) | (true, Side::Sell) => ctx.best_ask(coin),
            (false, Side::Sell) | (true, Side::Buy) => ctx.best_bid(coin),
        }
    }

    fn entry_intents(&self, ctx: &Ctx<'_>, now_ms: u64) -> Option<Vec<OrderIntent>> {
        let perp_mid = ctx.mid(self.config.perp_coin)?;
        let spot_mid = ctx.mid(self.config.spot_coin)?;
        let perp_size = self.config.target_notional / perp_mid;
        let spot_size = self.config.target_notional / spot_mid;
        let spot_px = Self::limit_px(ctx, self.config.spot_coin, Side::Buy, self.maker)?;
        let perp_px = Self::limit_px(ctx, self.config.perp_coin, Side::Sell, self.maker)?;
        Some(vec![
            self.leg_intent(
                ctx,
                self.config.spot_coin,
                Side::Buy,
                spot_size,
                spot_px,
                false,
                now_ms,
                format!("funding entry: buy {} spot", self.config.spot_token),
            ),
            self.leg_intent(
                ctx,
                self.config.perp_coin,
                Side::Sell,
                perp_size,
                perp_px,
                false,
                now_ms,
                format!(
                    "funding entry: short {}",
                    ctx.coin_name(self.config.perp_coin)
                ),
            ),
        ])
    }

    fn exit_intents(&self, ctx: &Ctx<'_>, account: &AccountState, now_ms: u64) -> Vec<OrderIntent> {
        let mut intents = Vec::new();
        let perp_short = account
            .position_szi(self.config.perp_coin)
            .min(Decimal::ZERO)
            .abs();
        if perp_short > Decimal::ZERO
            && let Some(px) = Self::limit_px(ctx, self.config.perp_coin, Side::Buy, self.maker)
        {
            intents.push(self.leg_intent(
                ctx,
                self.config.perp_coin,
                Side::Buy,
                perp_short,
                px,
                true,
                now_ms,
                "funding exit: close perp short".to_string(),
            ));
        }
        let spot_long = account.spot_balance(&self.config.spot_token);
        if spot_long > Decimal::ZERO
            && let Some(px) = Self::limit_px(ctx, self.config.spot_coin, Side::Sell, self.maker)
        {
            intents.push(self.leg_intent(
                ctx,
                self.config.spot_coin,
                Side::Sell,
                spot_long,
                px,
                false,
                now_ms,
                "funding exit: sell spot".to_string(),
            ));
        }
        intents
    }

    /// Advance the below-threshold streak, returning true when it is time to
    /// exit. Only distinct hourly settlements advance the counter.
    fn settlement_below_threshold(&mut self, rate_bps: Decimal, now_ms: u64) -> bool {
        let hour = now_ms / MS_PER_HOUR;
        if self.last_hour == Some(hour) {
            return self.below_streak >= self.config.exit_after_hours;
        }
        self.last_hour = Some(hour);
        if rate_bps <= self.config.exit_threshold_bps {
            self.below_streak += 1;
        } else {
            self.below_streak = 0;
        }
        self.below_streak >= self.config.exit_after_hours
    }

    fn rebalance_intent(
        &self,
        ctx: &Ctx<'_>,
        account: &AccountState,
        now_ms: u64,
    ) -> Option<OrderIntent> {
        if self.config.rebalance_drift_bps <= Decimal::ZERO {
            return None;
        }
        let perp_px = ctx.mid(self.config.perp_coin)?;
        let perp_abs = account.position_szi(self.config.perp_coin).abs();
        let spot = account.spot_balance(&self.config.spot_token);
        if perp_abs <= Decimal::ZERO || spot <= Decimal::ZERO {
            return None;
        }
        let drift = (spot - perp_abs).abs() * perp_px;
        if drift
            <= self.config.target_notional * self.config.rebalance_drift_bps / Decimal::from(10_000)
        {
            return None;
        }
        if spot > perp_abs {
            let sell = spot - perp_abs;
            let px = Self::limit_px(ctx, self.config.spot_coin, Side::Sell, self.maker)?;
            Some(self.leg_intent(
                ctx,
                self.config.spot_coin,
                Side::Sell,
                sell,
                px,
                false,
                now_ms,
                "funding rebalance: trim spot leg".to_string(),
            ))
        } else {
            let buy = perp_abs - spot;
            let px = Self::limit_px(ctx, self.config.spot_coin, Side::Buy, self.maker)?;
            Some(self.leg_intent(
                ctx,
                self.config.spot_coin,
                Side::Buy,
                buy,
                px,
                false,
                now_ms,
                "funding rebalance: top up spot leg".to_string(),
            ))
        }
    }

    fn dispatch(&mut self, ctx: &Ctx<'_>, out: &mut Actions) {
        let now_ms = ctx.now.ts_exch_ms.max(stamp_ms(ctx));
        match self.state {
            State::Flat => {
                if self.edge_bps(ctx).is_some_and(|edge| edge > Decimal::ZERO)
                    && let Some(intents) = self.entry_intents(ctx, now_ms)
                    && !ctx.is_stale(self.config.perp_coin)
                    && !ctx.is_stale(self.config.spot_coin)
                {
                    self.state = State::Entering;
                    self.entry_fills = 0;
                    for intent in intents {
                        out.place(intent);
                    }
                }
            }
            State::Entering | State::Exiting => {}
            State::Hedged => {
                let rate_bps = ctx
                    .funding(self.config.perp_coin)
                    .map(|rate| rate * Decimal::from(10_000))
                    .unwrap_or(Decimal::ZERO);
                if self.settlement_below_threshold(rate_bps, now_ms) {
                    let intents = self.exit_intents(ctx, ctx.account, now_ms);
                    if intents.is_empty() {
                        // Nothing to close (e.g. paper account not wired); reset.
                        self.state = State::Flat;
                        self.below_streak = 0;
                        return;
                    }
                    self.state = State::Exiting;
                    self.exit_fills = 0;
                    for intent in intents {
                        out.place(intent);
                    }
                } else if let Some(intent) = self.rebalance_intent(ctx, ctx.account, now_ms) {
                    out.place(intent);
                }
            }
        }
    }
}

/// The event time in milliseconds since the Unix epoch, derived from the
/// receive wall clock (event-time so replay is deterministic; SPEC-0010 §6).
fn stamp_ms(ctx: &Ctx<'_>) -> u64 {
    u64::try_from(ctx.now.t_recv_ns / 1_000_000).unwrap_or(0)
}

impl Strategy for FundingBasis {
    fn id(&self) -> StrategyId {
        StrategyId::from(ID)
    }

    fn cost(&self) -> CostModel {
        self.cost
    }

    fn interests(&self) -> Interests {
        Interests {
            coins: vec![
                (self.config.perp_coin, Stream::Book),
                (self.config.perp_coin, Stream::Ctx),
                (self.config.spot_coin, Stream::Book),
            ],
            timers_ms: vec![1_000],
        }
    }

    fn on_market(&mut self, _coin: CoinId, ctx: &Ctx<'_>, out: &mut Actions) {
        self.dispatch(ctx, out);
    }

    fn on_order(&mut self, update: &OrderEvent, _ctx: &Ctx<'_>, _out: &mut Actions) {
        if !update.is_fill() {
            return;
        }
        match self.state {
            State::Entering => {
                self.entry_fills += 1;
                if self.entry_fills >= 2 {
                    self.state = State::Hedged;
                    self.last_hour = None;
                    self.below_streak = 0;
                }
            }
            State::Exiting => {
                self.exit_fills += 1;
                if self.exit_fills >= 2 {
                    self.state = State::Flat;
                }
            }
            State::Flat | State::Hedged => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use mev_strategy::FeeRates;

    use super::*;
    use crate::strategies::test_support::{account_with, ctx_with, market_btc_spot};
    use crate::strategy::Action;
    use crate::types::Stamp;

    fn ds(value: &str) -> Decimal {
        Decimal::from_str(value).unwrap()
    }

    fn config() -> FundingConfig {
        FundingConfig {
            perp_coin: CoinId(0),
            spot_coin: CoinId(1),
            spot_token: "UBTC".into(),
            spot_sz_decimals: 3,
            target_notional: ds("10000"),
            horizon_hours: ds("24"),
            exit_threshold_bps: Decimal::ZERO,
            exit_after_hours: 2,
            rebalance_drift_bps: ds("100"),
        }
    }

    fn strategy() -> FundingBasis {
        FundingBasis::new(
            config(),
            CostModel::new(ds("5")),
            FeeRates::PERP,
            FeeRates::SPOT,
            false,
        )
    }

    fn run(strategy: &mut FundingBasis, ctx: &Ctx<'_>) -> Vec<Action> {
        let mut actions = Actions::new();
        strategy.on_market(CoinId(0), ctx, &mut actions);
        actions.take().into_vec()
    }

    fn fill(coin: CoinId) -> OrderEvent {
        OrderEvent {
            stamp: Stamp::default(),
            cloid: None,
            oid: 1,
            coin,
            side: Side::Buy,
            px: ds("60010"),
            sz: ds("0.16"),
            fee: Decimal::ZERO,
            maker: false,
            reduce_only: false,
            kind: crate::strategy::OrderEventKind::Fill,
        }
    }

    fn places_of(actions: Vec<Action>) -> Vec<OrderIntent> {
        actions
            .into_iter()
            .filter_map(|action| match action {
                Action::Place(intent) => Some(intent),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn interests_cover_both_books_and_perp_ctx() {
        let interests = strategy().interests();
        assert!(interests.coins.contains(&(CoinId(0), Stream::Book)));
        assert!(interests.coins.contains(&(CoinId(0), Stream::Ctx)));
        assert!(interests.coins.contains(&(CoinId(1), Stream::Book)));
        assert_eq!(interests.timers_ms, vec![1_000]);
        assert_eq!(strategy().id().as_str(), "funding_basis");
    }

    #[test]
    fn positive_edge_for_rich_funding() {
        let strategy = strategy();
        let market = market_btc_spot("0.001", "59990", "60010");
        let account = AccountState::new(2);
        let ctx = ctx_with(&market, &account, 0);
        assert!(strategy.edge_bps(&ctx).unwrap() > Decimal::ZERO);
    }

    #[test]
    fn negative_edge_for_thin_funding() {
        let strategy = strategy();
        let market = market_btc_spot("0.0000001", "59990", "60010");
        let account = AccountState::new(2);
        let ctx = ctx_with(&market, &account, 0);
        assert!(strategy.edge_bps(&ctx).unwrap() < Decimal::ZERO);
    }

    #[test]
    fn enters_when_edge_is_positive() {
        let mut strategy = strategy();
        let market = market_btc_spot("0.001", "59990", "60010");
        let account = AccountState::new(2);
        let ctx = ctx_with(&market, &account, 0);
        let intents = places_of(run(&mut strategy, &ctx));
        assert_eq!(intents.len(), 2);
        assert_eq!(strategy.state(), "entering");
        assert!(intents[0].coin == "@1" && intents[0].side == Side::Buy);
        assert!(intents[1].coin == "BTC" && intents[1].side == Side::Sell);
        assert!(!intents[0].reduce_only);
    }

    #[test]
    fn stays_flat_when_edge_is_negative() {
        let mut strategy = strategy();
        let market = market_btc_spot("0.0000001", "59990", "60010");
        let account = AccountState::new(2);
        let ctx = ctx_with(&market, &account, 0);
        assert!(run(&mut strategy, &ctx).is_empty());
        assert_eq!(strategy.state(), "flat");
    }

    #[test]
    fn becomes_hedged_after_both_legs_fill() {
        let mut strategy = strategy();
        let market = market_btc_spot("0.001", "59990", "60010");
        let account = AccountState::new(2);
        let ctx = ctx_with(&market, &account, 0);
        let _ = run(&mut strategy, &ctx);
        let mut out = Actions::new();
        strategy.on_order(&fill(CoinId(1)), &ctx, &mut out);
        assert_eq!(strategy.state(), "entering");
        strategy.on_order(&fill(CoinId(1)), &ctx, &mut out);
        assert_eq!(strategy.state(), "hedged");
    }

    #[test]
    fn exits_after_consecutive_hours_below_threshold() {
        let mut strategy = strategy();
        let rich = market_btc_spot("0.001", "59990", "60010");
        let account = AccountState::new(2);
        let ctx = ctx_with(&rich, &account, 0);
        let _ = run(&mut strategy, &ctx);
        let mut out = Actions::new();
        strategy.on_order(&fill(CoinId(1)), &ctx, &mut out);
        strategy.on_order(&fill(CoinId(1)), &ctx, &mut out);
        assert_eq!(strategy.state(), "hedged");

        let mut account = AccountState::new(2);
        account.set_position_szi(CoinId(0), ds("-0.16"));
        account.spot.insert("UBTC".into(), ds("0.16"));
        let flat = market_btc_spot("0", "59990", "60010");

        // Hour 0: streak 1 (< 2), no exit yet.
        let ctx = ctx_with(&flat, &account, MS_PER_HOUR);
        assert!(run(&mut strategy, &ctx).is_empty());
        assert_eq!(strategy.state(), "hedged");

        // Hour 1: streak 2 => exit both legs.
        let ctx = ctx_with(&flat, &account, 2 * MS_PER_HOUR);
        let intents = places_of(run(&mut strategy, &ctx));
        assert_eq!(intents.len(), 2);
        assert_eq!(strategy.state(), "exiting");
        let perp = intents.iter().find(|i| i.coin == "BTC").unwrap();
        assert!(perp.reduce_only);
        let spot = intents.iter().find(|i| i.coin == "@1").unwrap();
        assert!(!spot.reduce_only);
    }

    #[test]
    fn rebalances_spot_drift_while_hedged() {
        let mut strategy = strategy();
        let rich = market_btc_spot("0.001", "59990", "60010");
        let account = AccountState::new(2);
        let ctx = ctx_with(&rich, &account, 0);
        let _ = run(&mut strategy, &ctx);
        let mut out = Actions::new();
        strategy.on_order(&fill(CoinId(1)), &ctx, &mut out);
        strategy.on_order(&fill(CoinId(1)), &ctx, &mut out);

        let mut account = AccountState::new(2);
        account.set_position_szi(CoinId(0), ds("-0.16"));
        account.spot.insert("UBTC".into(), ds("0.20"));
        let ctx = ctx_with(&rich, &account, 0);
        let intents = places_of(run(&mut strategy, &ctx));
        assert_eq!(intents.len(), 1);
        assert_eq!(intents[0].coin, "@1");
        assert_eq!(intents[0].side, Side::Sell);
    }

    #[test]
    fn stays_flat_on_stale_books() {
        let mut strategy = strategy();
        let mut market = market_btc_spot("0.001", "59990", "60010");
        market[0].stale = true;
        let account = AccountState::new(2);
        let ctx = ctx_with(&market, &account, 0);
        assert!(run(&mut strategy, &ctx).is_empty());
        assert_eq!(strategy.state(), "flat");
    }

    #[test]
    fn account_with_is_unused_placeholder() {
        // Keep the helper exercised so it stays in sync with AccountState.
        let account = account_with(CoinId(0), "1");
        assert_eq!(account.position_szi(CoinId(0)), Decimal::ONE);
    }
}
