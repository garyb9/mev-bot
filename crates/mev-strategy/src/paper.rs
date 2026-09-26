//! Paper trading executor (SPEC-0003 §10).
//!
//! Simulates intent fills against the live or replayed book so `simulate` mode
//! produces fills, realized edge, and funding accrual without touching keys.
//! Marketable orders cross immediately at book prices (taker); resting orders
//! fill when the book trades through their limit (maker).

use std::collections::BTreeMap;

use rust_decimal::Decimal;

use crate::cost::FeeRates;
use crate::event::FillEvent;
use crate::id::StrategyId;
use crate::intent::{OrderIntent, Side, TimeInForce};
use crate::view::{AccountView, MarketView, OpenOrderView, PositionView};

/// How an instrument settles. Needed to route fills to spot balances or perp
/// positions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instrument {
    /// Whether this is a spot pair.
    pub is_spot: bool,
    /// Spot token symbol (empty for perps), e.g. `UBTC`.
    pub token: String,
}

impl Instrument {
    /// A perpetual instrument.
    pub fn perp() -> Self {
        Self {
            is_spot: false,
            token: String::new(),
        }
    }

    /// A spot instrument backed by `token`.
    pub fn spot(token: impl Into<String>) -> Self {
        Self {
            is_spot: true,
            token: token.into(),
        }
    }
}

struct RestingOrder {
    oid: u64,
    cloid: String,
    coin: String,
    instrument: Instrument,
    strategy: StrategyId,
    side: Side,
    limit_px: Decimal,
    remaining: Decimal,
    reduce_only: bool,
}

impl RestingOrder {
    fn view(&self) -> OpenOrderView {
        OpenOrderView {
            coin: self.coin.clone(),
            oid: Some(self.oid),
            cloid: Some(self.cloid.clone()),
            side: self.side,
            limit_px: self.limit_px,
            sz: self.remaining,
            reduce_only: self.reduce_only,
        }
    }
}

/// A simulated account and matching engine.
pub struct PaperExecutor {
    instruments: BTreeMap<String, Instrument>,
    account: AccountView,
    resting: Vec<RestingOrder>,
    perp_fees: FeeRates,
    spot_fees: FeeRates,
    next_oid: u64,
    fees_paid: Decimal,
}

impl PaperExecutor {
    /// Build an executor over the given instruments and starting account.
    pub fn new(
        instruments: BTreeMap<String, Instrument>,
        account: AccountView,
        perp_fees: FeeRates,
        spot_fees: FeeRates,
    ) -> Self {
        Self {
            instruments,
            account,
            resting: Vec::new(),
            perp_fees,
            spot_fees,
            next_oid: 1,
            fees_paid: Decimal::ZERO,
        }
    }

    /// The simulated account snapshot.
    pub fn account(&self) -> &AccountView {
        &self.account
    }

    /// The simulated account snapshot, mutably (for sync from live state).
    pub fn account_mut(&mut self) -> &mut AccountView {
        &mut self.account
    }

    /// Resting orders as fee/portfolio views.
    pub fn open_orders(&self) -> Vec<OpenOrderView> {
        self.resting.iter().map(RestingOrder::view).collect()
    }

    /// Total fees paid by the simulated account.
    pub fn fees_paid(&self) -> Decimal {
        self.fees_paid
    }

    fn instrument(&self, coin: &str) -> Instrument {
        self.instruments
            .get(coin)
            .cloned()
            .unwrap_or_else(Instrument::perp)
    }

    /// Submit an intent. Marketable orders fill (wholly or partly) at once and
    /// resting orders are recorded for later [`PaperExecutor::on_market`] calls.
    pub fn submit(
        &mut self,
        intent: &OrderIntent,
        market: &MarketView,
        _now_ms: u64,
    ) -> Vec<FillEvent> {
        let Some(book) = market.book(&intent.coin) else {
            return Vec::new();
        };
        let Some(limit_px) = intent.limit_px else {
            // Aggressive/market: treat the touch as the limit.
            let touch = match intent.side {
                Side::Buy => book.best_ask(),
                Side::Sell => book.best_bid(),
            };
            let Some((px, _)) = touch else {
                return Vec::new();
            };
            return self.fill_now(intent, px, intent.size, false);
        };

        let crosses = match intent.side {
            Side::Buy => book.best_ask().is_some_and(|(ask, _)| ask <= limit_px),
            Side::Sell => book.best_bid().is_some_and(|(bid, _)| bid >= limit_px),
        };

        if crosses {
            if intent.tif == TimeInForce::Alo {
                // Post-only that would cross is rejected by the venue: no fill.
                return Vec::new();
            }
            let walk = book.walk_bounded(intent.side, intent.size, limit_px);
            if walk.filled <= Decimal::ZERO {
                return Vec::new();
            }
            return self.fill_now(intent, walk.avg_px, walk.filled, false);
        }

        // Does not cross: rest it (Ioc would simply cancel unfilled).
        if intent.tif != TimeInForce::Ioc {
            let oid = self.next_oid;
            self.next_oid += 1;
            self.resting.push(RestingOrder {
                oid,
                cloid: cloid_for(oid),
                coin: intent.coin.clone(),
                instrument: self.instrument(&intent.coin),
                strategy: intent.strategy.clone(),
                side: intent.side,
                limit_px,
                remaining: intent.size,
                reduce_only: intent.reduce_only,
            });
        }
        Vec::new()
    }

    /// Advance resting orders against a new book, returning any maker fills.
    pub fn on_market(&mut self, market: &MarketView, _now_ms: u64) -> Vec<FillEvent> {
        let mut fills = Vec::new();
        let mut still_resting = Vec::with_capacity(self.resting.len());
        for mut order in std::mem::take(&mut self.resting) {
            let Some(book) = market.book(&order.coin) else {
                still_resting.push(order);
                continue;
            };
            let available: Option<Decimal> = match order.side {
                Side::Buy => book
                    .best_ask()
                    .filter(|(ask, _)| *ask <= order.limit_px)
                    .map(|_| depth_within(&book.asks, Side::Buy, order.limit_px)),
                Side::Sell => book
                    .best_bid()
                    .filter(|(bid, _)| *bid >= order.limit_px)
                    .map(|_| depth_within(&book.bids, Side::Sell, order.limit_px)),
            };
            let Some(available) = available else {
                still_resting.push(order);
                continue;
            };
            let fill_size = order.remaining.min(available);
            if fill_size <= Decimal::ZERO {
                still_resting.push(order);
                continue;
            }
            let fill = self.apply(
                &order.coin,
                &order.instrument,
                &order.strategy,
                order.side,
                fill_size,
                order.limit_px,
                true,
                order.reduce_only,
            );
            order.remaining -= fill_size;
            if order.remaining > Decimal::ZERO {
                still_resting.push(order);
            }
            if let Some(fill) = fill {
                fills.push(fill);
            }
        }
        self.resting = still_resting;
        fills
    }

    fn fill_now(
        &mut self,
        intent: &OrderIntent,
        px: Decimal,
        size: Decimal,
        maker: bool,
    ) -> Vec<FillEvent> {
        let instrument = self.instrument(&intent.coin);
        self.apply(
            &intent.coin,
            &instrument,
            &intent.strategy,
            intent.side,
            size,
            px,
            maker,
            intent.reduce_only,
        )
        .into_iter()
        .collect()
    }

    #[allow(clippy::too_many_arguments)]
    fn apply(
        &mut self,
        coin: &str,
        instrument: &Instrument,
        strategy: &StrategyId,
        side: Side,
        size: Decimal,
        px: Decimal,
        maker: bool,
        reduce_only: bool,
    ) -> Option<FillEvent> {
        let size = if reduce_only && !instrument.is_spot {
            let current = self.account.position_szi(coin);
            let closing = if side.is_buy() {
                (-current).max(Decimal::ZERO)
            } else {
                current.max(Decimal::ZERO)
            };
            closing.min(size)
        } else {
            size
        };
        if size <= Decimal::ZERO {
            return None;
        }
        let signed = if side.is_buy() { size } else { -size };

        if instrument.is_spot {
            let balance = self
                .account
                .spot
                .entry(instrument.token.clone())
                .or_insert(Decimal::ZERO);
            *balance = (*balance + signed).max(Decimal::ZERO);
        } else {
            let position = self
                .account
                .positions
                .entry(coin.to_string())
                .or_insert_with(|| PositionView {
                    coin: coin.to_string(),
                    ..Default::default()
                });
            let was_flat = position.szi.is_zero();
            position.szi += signed;
            position.position_value = position.szi.abs() * px;
            if was_flat || position.entry_px.is_none() {
                position.entry_px = Some(px);
            }
        }

        let rates = if instrument.is_spot {
            self.spot_fees
        } else {
            self.perp_fees
        };
        let fee = px * size * rates.rate(maker);
        self.account.account_value -= fee;
        self.fees_paid += fee;

        Some(FillEvent {
            strategy: Some(strategy.clone()),
            coin: coin.to_string(),
            side,
            px,
            sz: size,
            fee,
            maker,
            reduce_only,
        })
    }
}

fn depth_within(levels: &[(Decimal, Decimal)], side: Side, limit_px: Decimal) -> Decimal {
    levels
        .iter()
        .take_while(|(px, _)| match side {
            Side::Buy => *px <= limit_px,
            Side::Sell => *px >= limit_px,
        })
        .map(|(_, sz)| *sz)
        .sum()
}

/// Deterministic 16-byte client order id from an order number.
fn cloid_for(oid: u64) -> String {
    format!("0x{oid:032x}")
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;
    use crate::id::StrategyId;
    use crate::view::BookView;

    fn ds(value: &str) -> Decimal {
        Decimal::from_str(value).unwrap()
    }

    fn market() -> MarketView {
        let mut market = MarketView::new();
        market.insert_book(
            "@1",
            BookView {
                bids: vec![(ds("100"), ds("5"))],
                asks: vec![(ds("101"), ds("5"))],
                sz_decimals: 2,
                time: 0,
            },
        );
        market
    }

    fn executor() -> PaperExecutor {
        let mut instruments = BTreeMap::new();
        instruments.insert("@1".to_string(), Instrument::spot("UBTC"));
        let account = AccountView {
            account_value: ds("100000"),
            ..Default::default()
        };
        PaperExecutor::new(instruments, account, FeeRates::PERP, FeeRates::SPOT)
    }

    fn intent(side: Side, px: &str, size: &str, tif: TimeInForce) -> OrderIntent {
        OrderIntent {
            strategy: StrategyId::from("paper"),
            coin: "@1".into(),
            side,
            limit_px: Some(ds(px)),
            size: ds(size),
            tif,
            reduce_only: false,
            rationale: "test".into(),
            signal_ms: 0,
            decision_ms: 0,
        }
    }

    #[test]
    fn marketable_taker_fill_updates_spot() {
        let mut executor = executor();
        let market = market();
        // Buy at 101 (the ask) with IOC: taker fill at 101.
        let fills = executor.submit(&intent(Side::Buy, "101", "2", TimeInForce::Ioc), &market, 0);
        assert_eq!(fills.len(), 1);
        assert!(!fills[0].maker);
        assert_eq!(fills[0].px, ds("101"));
        assert_eq!(fills[0].sz, ds("2"));
        assert_eq!(executor.account().spot_balance("UBTC"), ds("2"));
        // Fee = 101 * 2 * 0.0007.
        assert_eq!(executor.fees_paid(), ds("0.1414"));
        assert!(executor.open_orders().is_empty());
    }

    #[test]
    fn post_only_rests_then_fills_as_maker() {
        let mut executor = executor();
        let market = market();
        // Post-only bid at 100 (below ask) rests.
        let fills = executor.submit(&intent(Side::Buy, "100", "2", TimeInForce::Alo), &market, 0);
        assert!(fills.is_empty());
        assert_eq!(executor.open_orders().len(), 1);

        // Book trades down to 100: the resting bid fills as maker.
        let mut lower = MarketView::new();
        lower.insert_book(
            "@1",
            BookView {
                bids: vec![(ds("99"), ds("5"))],
                asks: vec![(ds("100"), ds("5"))],
                sz_decimals: 2,
                time: 1,
            },
        );
        let fills = executor.on_market(&lower, 1);
        assert_eq!(fills.len(), 1);
        assert!(fills[0].maker);
        assert_eq!(fills[0].px, ds("100"));
        assert_eq!(executor.account().spot_balance("UBTC"), ds("2"));
        assert!(executor.open_orders().is_empty());
    }

    #[test]
    fn post_only_that_would_cross_is_rejected() {
        let mut executor = executor();
        let market = market();
        // Bid at 102 would cross the 101 ask: post-only rejects.
        let fills = executor.submit(&intent(Side::Buy, "102", "2", TimeInForce::Alo), &market, 0);
        assert!(fills.is_empty());
        assert!(executor.open_orders().is_empty());
    }

    #[test]
    fn ioc_partial_fill_leaves_nothing_resting() {
        let mut executor = executor();
        let market = market();
        // Ask only has 5; IOC wants 8 at 101.
        let fills = executor.submit(&intent(Side::Buy, "101", "8", TimeInForce::Ioc), &market, 0);
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].sz, ds("5"));
        assert!(executor.open_orders().is_empty());
    }

    #[test]
    fn reduce_only_perp_close_is_capped() {
        let mut instruments = BTreeMap::new();
        instruments.insert("BTC".to_string(), Instrument::perp());
        let mut account = AccountView {
            account_value: ds("10000"),
            ..Default::default()
        };
        account.positions.insert(
            "BTC".into(),
            PositionView {
                coin: "BTC".into(),
                szi: ds("-1"),
                ..Default::default()
            },
        );
        let mut executor = PaperExecutor::new(instruments, account, FeeRates::PERP, FeeRates::SPOT);

        let mut market = MarketView::new();
        market.insert_book(
            "BTC",
            BookView {
                bids: vec![(ds("100"), ds("100"))],
                asks: vec![(ds("101"), ds("100"))],
                sz_decimals: 3,
                time: 0,
            },
        );

        let mut close = intent(Side::Buy, "101", "5", TimeInForce::Ioc);
        close.coin = "BTC".into();
        close.reduce_only = true;
        let fills = executor.submit(&close, &market, 0);
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].sz, ds("1"), "capped at the open short");
        assert_eq!(executor.account().position_szi("BTC"), Decimal::ZERO);
    }
}
