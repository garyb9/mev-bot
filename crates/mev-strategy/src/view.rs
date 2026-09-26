//! Owned snapshots of market and account state handed to strategies
//! (SPEC-0003 §5).
//!
//! Views are plain owned data: the engine assembles them from live state each
//! decision cycle and passes them by reference. Because they carry no locks or
//! handles, the same code path runs in live, simulate, and deterministic replay.

use std::collections::BTreeMap;

use mev_hl_client::market::OrderBook;
use mev_hl_client::types::AssetCtx;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::cost::FeeRates;
use crate::intent::Side;

/// One side of a book walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Walk {
    /// Volume-weighted average price of the consumed levels.
    pub avg_px: Decimal,
    /// Size actually available (may be less than requested).
    pub filled: Decimal,
}

impl Walk {
    /// Whether the requested size was fully available.
    pub fn is_complete(&self, requested: Decimal) -> bool {
        self.filled >= requested
    }
}

/// An owned L2 book snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BookView {
    /// Bids, best (highest) first.
    pub bids: Vec<(Decimal, Decimal)>,
    /// Asks, best (lowest) first.
    pub asks: Vec<(Decimal, Decimal)>,
    /// Size decimals for the market, used by the sizer.
    pub sz_decimals: u32,
    /// Exchange timestamp in milliseconds.
    pub time: u64,
}

impl BookView {
    /// Build a view from the client's keyed book. Bids are emitted descending,
    /// asks ascending.
    pub fn from_order_book(book: &OrderBook, sz_decimals: u32) -> Self {
        Self {
            bids: book.bids.iter().rev().map(|(px, sz)| (*px, *sz)).collect(),
            asks: book.asks.iter().map(|(px, sz)| (*px, *sz)).collect(),
            sz_decimals,
            time: book.time,
        }
    }

    /// Best bid as `(price, size)`.
    pub fn best_bid(&self) -> Option<(Decimal, Decimal)> {
        self.bids.first().copied()
    }

    /// Best ask as `(price, size)`.
    pub fn best_ask(&self) -> Option<(Decimal, Decimal)> {
        self.asks.first().copied()
    }

    /// Mid price, if both sides are present.
    pub fn mid(&self) -> Option<Decimal> {
        Some((self.best_bid()?.0 + self.best_ask()?.0) / Decimal::TWO)
    }

    /// Absolute spread, if both sides are present.
    pub fn spread(&self) -> Option<Decimal> {
        Some(self.best_ask()?.0 - self.best_bid()?.0)
    }

    /// Walk the book consuming `size` against `side` (a buy lifts asks, a sell
    /// hits bids). Returns the volume-weighted average price and filled size.
    pub fn walk(&self, side: Side, size: Decimal) -> Walk {
        let levels = match side {
            Side::Buy => &self.asks,
            Side::Sell => &self.bids,
        };
        let mut remaining = size;
        let mut notional = Decimal::ZERO;
        let mut filled = Decimal::ZERO;
        for (px, avail) in levels {
            if remaining <= Decimal::ZERO {
                break;
            }
            let take = remaining.min(*avail);
            notional += *px * take;
            filled += take;
            remaining -= take;
        }
        let avg_px = if filled > Decimal::ZERO {
            notional / filled
        } else {
            Decimal::ZERO
        };
        Walk { avg_px, filled }
    }

    /// Notional resting within `bps` of the mid, per side.
    pub fn depth_notional(&self, side: Side, bps: Decimal) -> Decimal {
        let Some(mid) = self.mid() else {
            return Decimal::ZERO;
        };
        let band = mid * bps / Decimal::from(10_000);
        let levels = match side {
            Side::Buy => &self.bids,
            Side::Sell => &self.asks,
        };
        levels
            .iter()
            .filter(|(px, _)| match side {
                Side::Buy => mid - *px <= band,
                Side::Sell => *px - mid <= band,
            })
            .map(|(px, sz)| *px * *sz)
            .sum()
    }
}

/// An owned snapshot of market state for the watched coins.
#[derive(Debug, Clone, Default)]
pub struct MarketView {
    books: BTreeMap<String, BookView>,
    ctx: BTreeMap<String, AssetCtx>,
    mids: BTreeMap<String, Decimal>,
}

impl MarketView {
    /// An empty view.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace a coin's book.
    pub fn insert_book(&mut self, coin: impl Into<String>, book: BookView) {
        self.books.insert(coin.into(), book);
    }

    /// Insert or replace a coin's asset context.
    pub fn insert_ctx(&mut self, coin: impl Into<String>, ctx: AssetCtx) {
        self.ctx.insert(coin.into(), ctx);
    }

    /// Replace the all-mids snapshot.
    pub fn set_mids(&mut self, mids: BTreeMap<String, Decimal>) {
        self.mids = mids;
    }

    /// The book for a coin.
    pub fn book(&self, coin: &str) -> Option<&BookView> {
        self.books.get(coin)
    }

    /// The asset context for a coin.
    pub fn ctx(&self, coin: &str) -> Option<&AssetCtx> {
        self.ctx.get(coin)
    }

    /// Mid price: book mid when available, otherwise the `allMids` value.
    pub fn mid(&self, coin: &str) -> Option<Decimal> {
        if let Some(mid) = self.books.get(coin).and_then(BookView::mid) {
            return Some(mid);
        }
        self.mids.get(coin).copied()
    }

    /// Funding rate (hourly) for a coin, if its context is known.
    pub fn funding(&self, coin: &str) -> Option<Decimal> {
        self.ctx.get(coin).map(|ctx| ctx.funding)
    }

    /// Coins with a book.
    pub fn coins(&self) -> impl Iterator<Item = &str> {
        self.books.keys().map(String::as_str)
    }
}

/// A perp position, owned.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PositionView {
    /// Coin.
    pub coin: String,
    /// Signed size (positive long, negative short).
    pub szi: Decimal,
    /// Entry price, if any.
    pub entry_px: Option<Decimal>,
    /// Position value in USD.
    pub position_value: Decimal,
    /// Unrealized PnL.
    pub unrealized_pnl: Decimal,
    /// Margin used.
    pub margin_used: Decimal,
}

/// A resting order, owned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenOrderView {
    /// Coin.
    pub coin: String,
    /// Exchange order id.
    pub oid: Option<u64>,
    /// Client order id.
    pub cloid: Option<String>,
    /// Side.
    pub side: Side,
    /// Limit price.
    pub limit_px: Decimal,
    /// Remaining size.
    pub sz: Decimal,
    /// Reduce-only flag.
    pub reduce_only: bool,
}

/// An owned snapshot of the account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountView {
    /// Perp positions keyed by coin.
    pub positions: BTreeMap<String, PositionView>,
    /// Spot balances keyed by token symbol (total balance, not available).
    pub spot: BTreeMap<String, Decimal>,
    /// Resting orders.
    pub open_orders: Vec<OpenOrderView>,
    /// Effective fee rates for this account.
    pub fees: FeeRates,
    /// Perp account value.
    pub account_value: Decimal,
    /// Margin currently used.
    pub margin_used: Decimal,
    /// Withdrawable USDC.
    pub withdrawable: Decimal,
}

impl Default for AccountView {
    fn default() -> Self {
        Self {
            positions: BTreeMap::new(),
            spot: BTreeMap::new(),
            open_orders: Vec::new(),
            fees: FeeRates::PERP,
            account_value: Decimal::ZERO,
            margin_used: Decimal::ZERO,
            withdrawable: Decimal::ZERO,
        }
    }
}

impl AccountView {
    /// Signed position size for a coin (zero when flat).
    pub fn position_szi(&self, coin: &str) -> Decimal {
        self.positions.get(coin).map_or(Decimal::ZERO, |p| p.szi)
    }

    /// Spot balance for a token (zero when absent).
    pub fn spot_balance(&self, token: &str) -> Decimal {
        self.spot.get(token).copied().unwrap_or(Decimal::ZERO)
    }

    /// Whether any resting order is for `coin`.
    pub fn has_open_order(&self, coin: &str) -> bool {
        self.open_orders.iter().any(|o| o.coin == coin)
    }

    /// Margin utilization in bps of account value.
    pub fn margin_utilization_bps(&self) -> Decimal {
        if self.account_value <= Decimal::ZERO {
            return Decimal::ZERO;
        }
        self.margin_used / self.account_value * Decimal::from(10_000)
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    fn d(value: i64) -> Decimal {
        Decimal::from(value)
    }

    fn ds(value: &str) -> Decimal {
        Decimal::from_str(value).unwrap()
    }

    fn book() -> BookView {
        BookView {
            bids: vec![(d(100), d(2)), (d(99), d(5))],
            asks: vec![(d(101), d(1)), (d(102), d(4))],
            sz_decimals: 3,
            time: 0,
        }
    }

    #[test]
    fn touch_and_mid() {
        let b = book();
        assert_eq!(b.best_bid().unwrap(), (d(100), d(2)));
        assert_eq!(b.best_ask().unwrap(), (d(101), d(1)));
        assert_eq!(b.mid().unwrap(), ds("100.5"));
        assert_eq!(b.spread().unwrap(), d(1));
    }

    #[test]
    fn walk_buy_consumes_asks() {
        let b = book();
        let w = b.walk(Side::Buy, d(5));
        // 1 @ 101 + 4 @ 102 = 509 / 5
        assert_eq!(w.filled, d(5));
        assert!(w.is_complete(d(5)));
        assert_eq!(w.avg_px, ds("101.8"));
    }

    #[test]
    fn walk_partial_reports_true_fill() {
        let b = book();
        let w = b.walk(Side::Sell, d(10));
        // Only 2 + 5 = 7 available.
        assert_eq!(w.filled, d(7));
        assert!(!w.is_complete(d(10)));
    }

    #[test]
    fn walk_with_no_liquidity() {
        let b = BookView::default();
        let w = b.walk(Side::Buy, d(1));
        assert_eq!(w.filled, Decimal::ZERO);
        assert_eq!(w.avg_px, Decimal::ZERO);
    }

    #[test]
    fn depth_within_band() {
        let b = book();
        // Band is 1% of mid 100.5 = 1.005. Only the touch (0.5 away) qualifies,
        // not the second level (1.5 away).
        assert_eq!(b.depth_notional(Side::Buy, d(100)), d(200));
        assert_eq!(b.depth_notional(Side::Sell, d(100)), d(101));
    }

    #[test]
    fn account_helpers() {
        let mut account = AccountView::default();
        account.positions.insert(
            "BTC".into(),
            PositionView {
                coin: "BTC".into(),
                szi: ds("-0.5"),
                ..Default::default()
            },
        );
        account.spot.insert("USDC".into(), d(1000));
        account.account_value = d(10000);
        account.margin_used = d(2500);

        assert_eq!(account.position_szi("BTC"), ds("-0.5"));
        assert_eq!(account.position_szi("ETH"), Decimal::ZERO);
        assert_eq!(account.spot_balance("USDC"), d(1000));
        assert_eq!(account.margin_utilization_bps(), d(2500));
    }
}
