//! Delta-neutral funding/basis strategy (SPEC-0003 §6).
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

use std::time::Duration;

use async_trait::async_trait;
use mev_core::error::Result;
use mev_hl_client::ws::Subscription;
use rust_decimal::Decimal;

use crate::action::Action;
use crate::cost::{CostModel, FeeRates};
use crate::event::FillEvent;
use crate::id::StrategyId;
use crate::intent::{OrderIntent, Side, TimeInForce};
use crate::strategy::{Strategy, StrategyContext};
use crate::view::MarketView;

/// Stable strategy id.
pub const ID: &str = "funding_basis";

const MS_PER_HOUR: u64 = 3_600_000;

fn places(intents: Vec<OrderIntent>) -> Vec<Action> {
    intents.into_iter().map(Action::Place).collect()
}

/// Configuration for [`FundingBasis`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FundingConfig {
    /// Perp coin to short.
    pub perp_coin: String,
    /// Spot coin to buy (canonical, e.g. `@1`).
    pub spot_coin: String,
    /// Spot token used to read balances for drift checks (e.g. `UBTC`).
    pub spot_token: String,
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
    pub fn edge_bps(&self, market: &MarketView) -> Option<Decimal> {
        let perp_px = market.mid(&self.config.perp_coin)?;
        let spot_px = market.mid(&self.config.spot_coin)?;
        let rate = market.funding(&self.config.perp_coin)?;
        if perp_px <= Decimal::ZERO
            || spot_px <= Decimal::ZERO
            || self.config.target_notional <= Decimal::ZERO
        {
            return None;
        }

        let perp_size = self.config.target_notional / perp_px;
        let spot_size = self.config.target_notional / spot_px;
        let spot_book = market.book(&self.config.spot_coin)?;
        let perp_book = market.book(&self.config.perp_coin)?;

        let slippage = self.cost.slippage_bps(spot_book, Side::Buy, spot_size)?
            + self.cost.slippage_bps(perp_book, Side::Sell, perp_size)?
            + self.cost.slippage_bps(spot_book, Side::Sell, spot_size)?
            + self.cost.slippage_bps(perp_book, Side::Buy, perp_size)?;
        let fees = self.cost.round_trip_fee_bps(self.spot_fees, self.maker)
            + self.cost.round_trip_fee_bps(self.perp_fees, self.maker);
        let gross = self.cost.funding_bps(rate, self.config.horizon_hours);
        Some(self.cost.net_edge_bps(gross, fees + slippage))
    }

    #[allow(clippy::too_many_arguments)]
    fn leg_intent(
        &self,
        coin: &str,
        side: Side,
        size: Decimal,
        limit_px: Decimal,
        reduce_only: bool,
        now_ms: u64,
        rationale: String,
    ) -> OrderIntent {
        OrderIntent {
            strategy: StrategyId::from(ID),
            coin: coin.to_string(),
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
    fn limit_px(market: &MarketView, coin: &str, side: Side, maker: bool) -> Option<Decimal> {
        let book = market.book(coin)?;
        match (maker, side) {
            (false, Side::Buy) | (true, Side::Sell) => book.best_ask().map(|(px, _)| px),
            (false, Side::Sell) | (true, Side::Buy) => book.best_bid().map(|(px, _)| px),
        }
    }

    fn entry_intents(&self, market: &MarketView, now_ms: u64) -> Option<Vec<OrderIntent>> {
        let (perp_px, spot_px) = (
            market.mid(&self.config.perp_coin)?,
            market.mid(&self.config.spot_coin)?,
        );
        let perp_size = self.config.target_notional / perp_px;
        let spot_size = self.config.target_notional / spot_px;
        let spot_px = Self::limit_px(market, &self.config.spot_coin, Side::Buy, self.maker)?;
        let perp_px = Self::limit_px(market, &self.config.perp_coin, Side::Sell, self.maker)?;
        Some(vec![
            self.leg_intent(
                &self.config.spot_coin,
                Side::Buy,
                spot_size,
                spot_px,
                false,
                now_ms,
                format!("funding entry: buy {} spot", self.config.spot_token),
            ),
            self.leg_intent(
                &self.config.perp_coin,
                Side::Sell,
                perp_size,
                perp_px,
                false,
                now_ms,
                format!("funding entry: short {} perp", self.config.perp_coin),
            ),
        ])
    }

    fn exit_intents(
        &self,
        market: &MarketView,
        now_ms: u64,
        account: &crate::view::AccountView,
    ) -> Vec<OrderIntent> {
        let mut intents = Vec::new();
        let perp_short = account
            .position_szi(&self.config.perp_coin)
            .min(Decimal::ZERO)
            .abs();
        if perp_short > Decimal::ZERO
            && let Some(px) = Self::limit_px(market, &self.config.perp_coin, Side::Buy, self.maker)
        {
            intents.push(self.leg_intent(
                &self.config.perp_coin,
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
            && let Some(px) = Self::limit_px(market, &self.config.spot_coin, Side::Sell, self.maker)
        {
            intents.push(self.leg_intent(
                &self.config.spot_coin,
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
        market: &MarketView,
        account: &crate::view::AccountView,
        now_ms: u64,
    ) -> Option<OrderIntent> {
        if self.config.rebalance_drift_bps <= Decimal::ZERO {
            return None;
        }
        let perp_px = market.mid(&self.config.perp_coin)?;
        let perp_abs = account.position_szi(&self.config.perp_coin).abs();
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
            let px = Self::limit_px(market, &self.config.spot_coin, Side::Sell, self.maker)?;
            Some(self.leg_intent(
                &self.config.spot_coin,
                Side::Sell,
                sell,
                px,
                false,
                now_ms,
                "funding rebalance: trim spot leg".to_string(),
            ))
        } else {
            let buy = perp_abs - spot;
            let px = Self::limit_px(market, &self.config.spot_coin, Side::Buy, self.maker)?;
            Some(self.leg_intent(
                &self.config.spot_coin,
                Side::Buy,
                buy,
                px,
                false,
                now_ms,
                "funding rebalance: top up spot leg".to_string(),
            ))
        }
    }
}

#[async_trait]
impl Strategy for FundingBasis {
    fn id(&self) -> StrategyId {
        StrategyId::from(ID)
    }

    fn subscriptions(&self) -> Vec<Subscription> {
        vec![
            Subscription::L2Book {
                coin: self.config.perp_coin.clone(),
            },
            Subscription::ActiveAssetCtx {
                coin: self.config.perp_coin.clone(),
            },
            Subscription::L2Book {
                coin: self.config.spot_coin.clone(),
            },
        ]
    }

    fn timers(&self) -> Vec<Duration> {
        vec![Duration::from_secs(1)]
    }

    async fn on_event(&mut self, ctx: &StrategyContext<'_>) -> Result<Vec<Action>> {
        let now_ms = ctx.now_ms;
        match self.state {
            State::Flat => {
                if self
                    .edge_bps(ctx.market)
                    .is_some_and(|edge| edge > Decimal::ZERO)
                    && let Some(intents) = self.entry_intents(ctx.market, now_ms)
                {
                    self.state = State::Entering;
                    self.entry_fills = 0;
                    return Ok(places(intents));
                }
                Ok(Vec::new())
            }
            State::Entering | State::Exiting => Ok(Vec::new()),
            State::Hedged => {
                let rate_bps = ctx
                    .market
                    .funding(&self.config.perp_coin)
                    .map(|rate| rate * Decimal::from(10_000))
                    .unwrap_or(Decimal::ZERO);
                if self.settlement_below_threshold(rate_bps, now_ms) {
                    let intents = self.exit_intents(ctx.market, now_ms, ctx.account);
                    if intents.is_empty() {
                        // Nothing to close (e.g. paper account not wired); reset.
                        self.state = State::Flat;
                        self.below_streak = 0;
                        return Ok(Vec::new());
                    }
                    self.state = State::Exiting;
                    self.exit_fills = 0;
                    return Ok(places(intents));
                }
                Ok(self
                    .rebalance_intent(ctx.market, ctx.account, now_ms)
                    .map(Action::Place)
                    .into_iter()
                    .collect())
            }
        }
    }

    async fn on_fill(&mut self, _fill: &FillEvent) -> Result<()> {
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
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use mev_hl_client::types::AssetCtx;

    use super::*;
    use crate::view::{AccountView, BookView, PositionView};

    fn ds(value: &str) -> Decimal {
        Decimal::from_str(value).unwrap()
    }

    fn ctx_of<'a>(
        market: &'a MarketView,
        account: &'a AccountView,
        now_ms: u64,
    ) -> StrategyContext<'a> {
        StrategyContext {
            now_ms,
            trigger: crate::strategy::Trigger::Timer,
            market,
            account,
        }
    }

    fn book(bid: &str, ask: &str, sz: &str) -> BookView {
        BookView {
            bids: vec![(ds(bid), ds(sz))],
            asks: vec![(ds(ask), ds(sz))],
            sz_decimals: 3,
            time: 0,
        }
    }

    fn market_with(funding: &str, spot_bid: &str, spot_ask: &str) -> MarketView {
        let mut market = MarketView::new();
        market.insert_book("BTC", book("59990", "60010", "100"));
        market.insert_book("@1", book(spot_bid, spot_ask, "100"));
        market.insert_ctx(
            "BTC",
            AssetCtx {
                funding: ds(funding),
                open_interest: ds("1"),
                prev_day_px: ds("60000"),
                day_ntl_vlm: ds("1000000"),
                premium: None,
                oracle_px: ds("60000"),
                mark_px: ds("60000"),
                mid_px: None,
            },
        );
        market
    }

    fn places_of(actions: Vec<Action>) -> Vec<OrderIntent> {
        actions
            .into_iter()
            .filter_map(|action| match action {
                Action::Place(intent) => Some(intent),
                Action::Cancel(_) => None,
            })
            .collect()
    }

    fn config() -> FundingConfig {
        FundingConfig {
            perp_coin: "BTC".into(),
            spot_coin: "@1".into(),
            spot_token: "UBTC".into(),
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

    #[test]
    fn subscription_and_timer_shape() {
        let strategy = strategy();
        assert_eq!(strategy.id().as_str(), "funding_basis");
        assert_eq!(strategy.subscriptions().len(), 3);
        assert_eq!(strategy.timers(), vec![Duration::from_secs(1)]);
    }

    #[test]
    fn positive_edge_for_rich_funding() {
        let strategy = strategy();
        // 10 bps/hour over 24h = 240 bps gross; costs ~23 bps + slippage.
        let market = market_with("0.001", "59990", "60010");
        assert!(strategy.edge_bps(&market).unwrap() > Decimal::ZERO);
    }

    #[test]
    fn negative_edge_for_thin_funding() {
        let strategy = strategy();
        let market = market_with("0.0000001", "59990", "60010");
        assert!(strategy.edge_bps(&market).unwrap() < Decimal::ZERO);
    }

    #[test]
    fn edge_requires_both_books_and_funding() {
        let strategy = strategy();

        // Spot book missing.
        let mut no_spot = MarketView::new();
        no_spot.insert_book("BTC", book("59990", "60010", "100"));
        assert!(strategy.edge_bps(&no_spot).is_none());

        // Funding context missing.
        let mut no_funding = MarketView::new();
        no_funding.insert_book("BTC", book("59990", "60010", "100"));
        no_funding.insert_book("@1", book("59990", "60010", "100"));
        assert!(strategy.edge_bps(&no_funding).is_none());
    }

    #[tokio::test]
    async fn enters_when_edge_is_positive() {
        let mut strategy = strategy();
        let market = market_with("0.001", "59990", "60010");
        let account = AccountView::default();
        let intents = places_of(
            strategy
                .on_event(&ctx_of(&market, &account, 0))
                .await
                .unwrap(),
        );
        assert_eq!(intents.len(), 2);
        assert_eq!(strategy.state(), "entering");
        assert!(intents[0].coin == "@1" && intents[0].side == Side::Buy);
        assert!(intents[1].coin == "BTC" && intents[1].side == Side::Sell);
        assert!(!intents[0].reduce_only);
    }

    #[tokio::test]
    async fn stays_flat_when_edge_is_negative() {
        let mut strategy = strategy();
        let market = market_with("0.0000001", "59990", "60010");
        let account = AccountView::default();
        let intents = strategy
            .on_event(&ctx_of(&market, &account, 0))
            .await
            .unwrap();
        assert!(intents.is_empty());
        assert_eq!(strategy.state(), "flat");
    }

    #[tokio::test]
    async fn becomes_hedged_after_both_legs_fill() {
        let mut strategy = strategy();
        let market = market_with("0.001", "59990", "60010");
        let account = AccountView::default();
        let _ = strategy
            .on_event(&ctx_of(&market, &account, 0))
            .await
            .unwrap();
        let fill = FillEvent {
            strategy: Some(StrategyId::from(ID)),
            coin: "@1".into(),
            side: Side::Buy,
            px: ds("60010"),
            sz: ds("0.16"),
            fee: ds("0"),
            maker: false,
            reduce_only: false,
        };
        strategy.on_fill(&fill).await.unwrap();
        assert_eq!(strategy.state(), "entering");
        strategy.on_fill(&fill).await.unwrap();
        assert_eq!(strategy.state(), "hedged");
    }

    #[tokio::test]
    async fn exits_after_consecutive_hours_below_threshold() {
        let mut strategy = strategy();
        // Enter with rich funding.
        let rich = market_with("0.001", "59990", "60010");
        let account = AccountView::default();
        let _ = strategy
            .on_event(&ctx_of(&rich, &account, 0))
            .await
            .unwrap();
        strategy.on_fill(&fill_event()).await.unwrap();
        strategy.on_fill(&fill_event()).await.unwrap();
        assert_eq!(strategy.state(), "hedged");

        // Now funding collapses; account holds the legs so exit can close them.
        let mut account = AccountView::default();
        account.positions.insert(
            "BTC".into(),
            PositionView {
                coin: "BTC".into(),
                szi: ds("-0.16"),
                ..Default::default()
            },
        );
        account.spot.insert("UBTC".into(), ds("0.16"));
        let flat = market_with("0", "59990", "60010");

        // Hour 0: streak 1 (< 2), no exit yet.
        let intents = strategy
            .on_event(&ctx_of(&flat, &account, MS_PER_HOUR))
            .await
            .unwrap();
        assert!(intents.is_empty());
        assert_eq!(strategy.state(), "hedged");

        // Hour 1: streak 2 => exit both legs.
        let intents = places_of(
            strategy
                .on_event(&ctx_of(&flat, &account, 2 * MS_PER_HOUR))
                .await
                .unwrap(),
        );
        assert_eq!(intents.len(), 2);
        assert_eq!(strategy.state(), "exiting");
        let perp = intents.iter().find(|i| i.coin == "BTC").unwrap();
        assert!(perp.reduce_only);
        let spot = intents.iter().find(|i| i.coin == "@1").unwrap();
        assert!(!spot.reduce_only);
    }

    #[tokio::test]
    async fn rebalances_spot_drift_while_hedged() {
        let mut strategy = strategy();
        let rich = market_with("0.001", "59990", "60010");
        let account = AccountView::default();
        let _ = strategy
            .on_event(&ctx_of(&rich, &account, 0))
            .await
            .unwrap();
        strategy.on_fill(&fill_event()).await.unwrap();
        strategy.on_fill(&fill_event()).await.unwrap();

        // Spot (0.20) is larger than the perp short (0.16): drift > 1% notional.
        let mut account = AccountView::default();
        account.positions.insert(
            "BTC".into(),
            PositionView {
                coin: "BTC".into(),
                szi: ds("-0.16"),
                ..Default::default()
            },
        );
        account.spot.insert("UBTC".into(), ds("0.20"));
        // Keep funding rich so no exit.
        let intents = places_of(
            strategy
                .on_event(&ctx_of(&rich, &account, 0))
                .await
                .unwrap(),
        );
        assert_eq!(intents.len(), 1);
        assert_eq!(intents[0].coin, "@1");
        assert_eq!(intents[0].side, Side::Sell);
    }

    fn fill_event() -> FillEvent {
        FillEvent {
            strategy: Some(StrategyId::from(ID)),
            coin: "@1".into(),
            side: Side::Buy,
            px: ds("60010"),
            sz: ds("0.16"),
            fee: ds("0"),
            maker: false,
            reduce_only: false,
        }
    }
}
