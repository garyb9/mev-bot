//! Local market state and staleness tracking (SPEC-0001 §7).
//!
//! Backend-agnostic: it consumes [`StreamEvent`]s and is therefore independent
//! of which transport produced them.

use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    time::{Duration, Instant},
};

use rust_decimal::Decimal;

use crate::{
    types::{AllMids, AssetCtx, L2Book, Trade},
    ws::StreamEvent,
};

/// Feed identifier for books.
pub const FEED_BOOK: &str = "book";
/// Feed identifier for asset contexts.
pub const FEED_CTX: &str = "ctx";

const TRADES_CAPACITY: usize = 256;

/// A coin's L2 order book, keyed by price.
#[derive(Debug, Clone, Default)]
pub struct OrderBook {
    /// Bids, price → size.
    pub bids: BTreeMap<Decimal, Decimal>,
    /// Asks, price → size.
    pub asks: BTreeMap<Decimal, Decimal>,
    /// Exchange timestamp of the snapshot, in milliseconds.
    pub time: u64,
}

impl OrderBook {
    /// Build a book from a full `l2Book` snapshot.
    pub fn from_snapshot(snapshot: &L2Book) -> Self {
        let mut bids = BTreeMap::new();
        let mut asks = BTreeMap::new();
        for level in &snapshot.levels[0] {
            bids.insert(level.px, level.sz);
        }
        for level in &snapshot.levels[1] {
            asks.insert(level.px, level.sz);
        }
        Self {
            bids,
            asks,
            time: snapshot.time,
        }
    }

    /// Best bid as `(price, size)`.
    pub fn best_bid(&self) -> Option<(Decimal, Decimal)> {
        self.bids.iter().next_back().map(|(px, sz)| (*px, *sz))
    }

    /// Best ask as `(price, size)`.
    pub fn best_ask(&self) -> Option<(Decimal, Decimal)> {
        self.asks.iter().next().map(|(px, sz)| (*px, *sz))
    }

    /// Mid price, if both sides are present.
    pub fn mid(&self) -> Option<Decimal> {
        Some((self.best_bid()?.0 + self.best_ask()?.0) / Decimal::TWO)
    }

    /// Absolute spread, if both sides are present.
    pub fn spread(&self) -> Option<Decimal> {
        Some(self.best_ask()?.0 - self.best_bid()?.0)
    }
}

/// Per-feed freshness tolerances.
#[derive(Debug, Clone, Copy)]
pub struct Tolerance {
    /// Max age of an `l2Book` update before it counts as stale.
    pub book: Duration,
    /// Max age of an `activeAssetCtx` update before it counts as stale.
    pub ctx: Duration,
}

impl Default for Tolerance {
    fn default() -> Self {
        Self {
            book: Duration::from_secs(2),
            ctx: Duration::from_secs(60),
        }
    }
}

/// The age of one subscribed feed.
#[derive(Debug, Clone)]
pub struct FeedAge {
    /// Coin the feed belongs to.
    pub coin: String,
    /// Feed identifier ([`FEED_BOOK`] / [`FEED_CTX`]).
    pub feed: &'static str,
    /// Seconds since the last update, or infinity if never received.
    pub age_secs: f64,
}

impl FeedAge {
    /// Whether this age exceeds the tolerance for its feed.
    pub fn is_stale(&self, tolerance: &Tolerance) -> bool {
        let limit = match self.feed {
            FEED_BOOK => tolerance.book,
            _ => tolerance.ctx,
        };
        self.age_secs.is_infinite() || self.age_secs > limit.as_secs_f64()
    }
}

#[derive(Debug, Clone)]
struct Timed<T> {
    value: T,
    updated_at: Instant,
}

/// Rolling per-coin market state built from the stream.
#[derive(Debug)]
pub struct MarketState {
    books: HashMap<String, Timed<OrderBook>>,
    ctx: HashMap<String, Timed<AssetCtx>>,
    trades: HashMap<String, VecDeque<Trade>>,
    mids: Option<Timed<AllMids>>,
    expected_books: HashSet<String>,
    expected_ctx: HashSet<String>,
    tolerance: Tolerance,
}

impl Default for MarketState {
    fn default() -> Self {
        Self::new(Tolerance::default())
    }
}

impl MarketState {
    /// Create an empty state with the given tolerances.
    pub fn new(tolerance: Tolerance) -> Self {
        Self {
            books: HashMap::new(),
            ctx: HashMap::new(),
            trades: HashMap::new(),
            mids: None,
            expected_books: HashSet::new(),
            expected_ctx: HashSet::new(),
            tolerance,
        }
    }

    /// Mark a coin's book as required for readiness.
    pub fn expect_book(&mut self, coin: &str) {
        self.expected_books.insert(coin.to_owned());
    }

    /// Mark a coin's asset context as required for readiness.
    pub fn expect_ctx(&mut self, coin: &str) {
        self.expected_ctx.insert(coin.to_owned());
    }

    /// Apply an event at the current time.
    pub fn apply(&mut self, event: &StreamEvent) {
        self.apply_at(event, Instant::now());
    }

    /// Apply an event at an explicit time (used in tests for staleness).
    pub fn apply_at(&mut self, event: &StreamEvent, now: Instant) {
        match event {
            StreamEvent::Book(book) => {
                self.books.insert(
                    book.coin.clone(),
                    Timed {
                        value: OrderBook::from_snapshot(book),
                        updated_at: now,
                    },
                );
            }
            StreamEvent::AssetCtx(update) => {
                self.ctx.insert(
                    update.coin.clone(),
                    Timed {
                        value: update.ctx.clone(),
                        updated_at: now,
                    },
                );
            }
            StreamEvent::Mids(mids) => {
                self.mids = Some(Timed {
                    value: mids.clone(),
                    updated_at: now,
                });
            }
            StreamEvent::Trades(trades) => {
                for trade in trades {
                    let buffer = self.trades.entry(trade.coin.clone()).or_default();
                    if buffer.len() == TRADES_CAPACITY {
                        buffer.pop_front();
                    }
                    buffer.push_back(trade.clone());
                }
            }
            // Account channels (H-3) are not market state.
            StreamEvent::Bbo(_)
            | StreamEvent::OrderUpdates(_)
            | StreamEvent::UserFills(_)
            | StreamEvent::UserEvent(_) => {}
        }
    }

    /// Current book for a coin.
    pub fn book(&self, coin: &str) -> Option<&OrderBook> {
        self.books.get(coin).map(|entry| &entry.value)
    }

    /// Current asset context for a coin.
    pub fn ctx(&self, coin: &str) -> Option<&AssetCtx> {
        self.ctx.get(coin).map(|entry| &entry.value)
    }

    /// Recent trades for a coin.
    pub fn trades(&self, coin: &str) -> Option<&VecDeque<Trade>> {
        self.trades.get(coin)
    }

    /// Latest all-mids snapshot.
    pub fn mids(&self) -> Option<&AllMids> {
        self.mids.as_ref().map(|entry| &entry.value)
    }

    /// Ages of every subscribed feed, including those never received.
    pub fn ages(&self, now: Instant) -> Vec<FeedAge> {
        let mut out = Vec::with_capacity(self.expected_books.len() + self.expected_ctx.len());

        for coin in &self.expected_books {
            out.push(FeedAge {
                coin: coin.clone(),
                feed: FEED_BOOK,
                age_secs: self.books.get(coin).map_or(f64::INFINITY, |entry| {
                    now.saturating_duration_since(entry.updated_at)
                        .as_secs_f64()
                }),
            });
        }
        for coin in &self.expected_ctx {
            out.push(FeedAge {
                coin: coin.clone(),
                feed: FEED_CTX,
                age_secs: self.ctx.get(coin).map_or(f64::INFINITY, |entry| {
                    now.saturating_duration_since(entry.updated_at)
                        .as_secs_f64()
                }),
            });
        }
        out
    }

    /// Subscribed feeds whose age exceeds their tolerance.
    pub fn staleness(&self, now: Instant) -> Vec<FeedAge> {
        self.ages(now)
            .into_iter()
            .filter(|age| age.is_stale(&self.tolerance))
            .collect()
    }

    /// Ready when at least one book is expected and every subscribed feed is
    /// fresh.
    pub fn is_ready(&self, now: Instant) -> bool {
        !self.expected_books.is_empty() && self.staleness(now).is_empty()
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use rust_decimal::Decimal;
    use serde_json::json;

    use super::*;
    use crate::ws::decode;

    fn book_event(coin: &str) -> StreamEvent {
        let frame = json!({
            "channel": "l2Book",
            "data": {
                "coin": coin, "time": 1,
                "levels": [
                    [{"px": "100", "sz": "1.5", "n": 2}],
                    [{"px": "101", "sz": "2", "n": 3}]
                ]
            }
        })
        .to_string();
        decode(&frame).unwrap().unwrap()
    }

    fn ctx_event(coin: &str) -> StreamEvent {
        let frame = json!({
            "channel": "activeAssetCtx",
            "data": {"coin": coin, "ctx": {
                "funding": "0.00001", "openInterest": "1", "prevDayPx": "99",
                "dayNtlVlm": "10", "oraclePx": "100", "markPx": "100.5"
            }}
        })
        .to_string();
        decode(&frame).unwrap().unwrap()
    }

    #[test]
    fn book_snapshot_derives_touch() {
        let mut state = MarketState::default();
        state.apply(&book_event("BTC"));
        let book = state.book("BTC").unwrap();
        assert_eq!(book.best_bid().unwrap().0, Decimal::from(100));
        assert_eq!(book.best_ask().unwrap().0, Decimal::from(101));
        assert_eq!(book.mid().unwrap(), Decimal::new(1005, 1));
        assert_eq!(book.spread().unwrap(), Decimal::from(1));
    }

    #[test]
    fn readiness_requires_fresh_expected_feeds() {
        let t0 = Instant::now();
        let mut state = MarketState::default();
        state.expect_book("BTC");
        state.expect_ctx("BTC");

        assert!(!state.is_ready(t0));

        state.apply_at(&book_event("BTC"), t0);
        assert!(!state.is_ready(t0), "ctx still missing");

        state.apply_at(&ctx_event("BTC"), t0);
        assert!(state.is_ready(t0));

        // Book tolerance is 2s: 5s later the book is stale.
        let later = t0 + Duration::from_secs(5);
        assert!(!state.is_ready(later));
        let stale = state.staleness(later);
        assert!(stale.iter().any(|age| age.feed == FEED_BOOK));
        assert!(!stale.iter().any(|age| age.feed == FEED_CTX));
    }

    #[test]
    fn empty_watchlist_is_not_ready() {
        let state = MarketState::default();
        assert!(!state.is_ready(Instant::now()));
    }

    #[test]
    fn trades_ring_is_bounded() {
        let mut state = MarketState::default();
        for tid in 0..(TRADES_CAPACITY + 10) {
            let frame = json!({
                "channel": "trades",
                "data": [{
                    "coin": "BTC", "side": "B", "px": "100", "sz": "0.1",
                    "time": 1, "tid": tid
                }]
            })
            .to_string();
            state.apply(&decode(&frame).unwrap().unwrap());
        }
        let trades = state.trades("BTC").unwrap();
        assert_eq!(trades.len(), TRADES_CAPACITY);
        assert_eq!(trades.front().unwrap().tid, Some(10));
    }
}
