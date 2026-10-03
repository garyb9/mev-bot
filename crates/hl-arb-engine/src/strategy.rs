//! Strategy API v2: the synchronous trait the engine drives (SPEC-0010 §8, E-4).
//!
//! Strategies are pure and synchronous: no I/O, no clock reads, no locks. The
//! engine owns all trading state and calls a strategy only for the coins,
//! streams, and timers it declared in [`Strategy::interests`], passing a
//! read-only [`Ctx`] and a reusable [`Actions`] buffer to push decisions into.
//!
//! The trait lives here (not in `hl-arb-strategy`) because its context and actions
//! name engine types ([`CoinId`], [`Cloid`], [`MarketSlot`], [`AccountState`])
//! and `hl-arb-engine` already depends on `hl-arb-strategy`, so the reverse would be
//! a dependency cycle. The pure building blocks the strategies use (the
//! [`CostModel`], fee rates, views, deterministic RNG) stay in `hl-arb-strategy`.
//! See SPEC-0010 §23 Q-Layering.

use hl_arb_strategy::{CostModel, OrderIntent, Side, StrategyId};
use rust_decimal::Decimal;
use smallvec::SmallVec;

use crate::state::{AccountState, MarketSlot};
use crate::types::{Cloid, CoinId, CoinRegistry, Px, Stamp, Sz, VenueOrderStatus};

/// A strategy-local timer handle. The engine assigns these when a strategy
/// schedules a timer and returns the same id when it fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TimerId(pub u32);

/// A multi-leg bundle (SPEC-0011). Placeholder in E-4; the engine rejects it
/// until the executor lands, so strategies never emit one yet.
#[derive(Debug, Clone, PartialEq)]
pub struct GroupIntent {
    /// The legs, in submission order.
    pub legs: Vec<OrderIntent>,
    /// Whether the legs must fill together.
    pub all_or_none: bool,
}

/// A decision a strategy asks the engine to carry out.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Place a new order (the engine assigns a [`Cloid`] when absent).
    Place(OrderIntent),
    /// Cancel a resting order by its cloid.
    Cancel {
        /// The order to cancel.
        cloid: Cloid,
    },
    /// Modify a resting order (one venue action instead of cancel + place).
    Modify {
        /// The order to modify.
        cloid: Cloid,
        /// New limit price.
        px: Px,
        /// New size.
        sz: Sz,
    },
    /// Place a multi-leg group (SPEC-0011; off until the executor supports it).
    PlaceGroup(GroupIntent),
}

/// A reusable buffer of [`Action`]s, cleared by the engine each dispatch.
#[derive(Debug, Default)]
pub struct Actions {
    inner: SmallVec<[Action; 8]>,
}

impl Actions {
    /// An empty buffer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a place action.
    pub fn place(&mut self, intent: OrderIntent) {
        self.inner.push(Action::Place(intent));
    }

    /// Append a cancel action.
    pub fn cancel(&mut self, cloid: Cloid) {
        self.inner.push(Action::Cancel { cloid });
    }

    /// Append a modify action.
    pub fn modify(&mut self, cloid: Cloid, px: Px, sz: Sz) {
        self.inner.push(Action::Modify { cloid, px, sz });
    }

    /// Append a multi-leg group (SPEC-0011).
    pub fn place_group(&mut self, group: GroupIntent) {
        self.inner.push(Action::PlaceGroup(group));
    }

    /// Number of buffered actions.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Whether the buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Borrow the buffered actions.
    pub fn as_slice(&self) -> &[Action] {
        &self.inner
    }

    /// Take the buffered actions, leaving the buffer empty.
    pub fn take(&mut self) -> SmallVec<[Action; 8]> {
        std::mem::take(&mut self.inner)
    }

    /// Clear the buffer for reuse.
    pub fn clear(&mut self) {
        self.inner.clear();
    }
}

/// The kind of order event delivered to [`Strategy::on_order`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderEventKind {
    /// The venue reported a status change.
    Status(VenueOrderStatus),
    /// The order (or part of it) filled.
    Fill,
}

/// An update about one of the strategy's own orders (SPEC-0010 §8).
#[derive(Debug, Clone, PartialEq)]
pub struct OrderEvent {
    /// Event time.
    pub stamp: Stamp,
    /// Client order id, if the engine assigned one.
    pub cloid: Option<Cloid>,
    /// Venue order id.
    pub oid: u64,
    /// Coin.
    pub coin: CoinId,
    /// Side that rested.
    pub side: Side,
    /// Fill price (zero for a pure status change).
    pub px: Px,
    /// Fill size (zero for a pure status change).
    pub sz: Sz,
    /// Fee paid (zero unless a fill).
    pub fee: Px,
    /// Whether the fill was a maker.
    pub maker: bool,
    /// Whether the order was reduce-only.
    pub reduce_only: bool,
    /// What happened.
    pub kind: OrderEventKind,
}

impl OrderEvent {
    /// Whether this event reports a fill.
    pub fn is_fill(&self) -> bool {
        self.kind == OrderEventKind::Fill
    }
}

/// Read-only view of engine state handed to a strategy for one dispatch.
///
/// Timestamps are event-time so replay is deterministic. Strategies must not
/// mutate anything reachable from here.
pub struct Ctx<'a> {
    /// Event time of the triggering update.
    pub now: Stamp,
    /// Per-coin market state, indexed by [`CoinId`].
    pub markets: &'a [MarketSlot],
    /// Account state (positions, spot balances, margin).
    pub account: &'a AccountState,
    /// Bidirectional coin map, for names in rationales and config resolution.
    pub registry: &'a CoinRegistry,
}

impl<'a> Ctx<'a> {
    /// The canonical coin name for an id.
    pub fn coin_name(&self, coin: CoinId) -> &str {
        self.registry.coin(coin).unwrap_or("")
    }

    /// The market slot for a coin.
    pub fn slot(&self, coin: CoinId) -> Option<&MarketSlot> {
        self.markets.get(coin.index())
    }

    /// Best bid, considering the fresher of bbo and the book top.
    pub fn best_bid(&self, coin: CoinId) -> Option<Px> {
        self.slot(coin)?.best_bid().map(|level| level.px)
    }

    /// Best ask, considering the fresher of bbo and the book top.
    pub fn best_ask(&self, coin: CoinId) -> Option<Px> {
        self.slot(coin)?.best_ask().map(|level| level.px)
    }

    /// Mid price from the best bid/ask, if both are present.
    pub fn mid(&self, coin: CoinId) -> Option<Px> {
        let bid = self.best_bid(coin)?;
        let ask = self.best_ask(coin)?;
        Some((bid + ask) / Decimal::TWO)
    }

    /// The latest funding rate for a coin, if its context is known.
    pub fn funding(&self, coin: CoinId) -> Option<Decimal> {
        self.slot(coin)?.ctx.map(|(ctx, _)| ctx.funding)
    }

    /// Whether a coin's inputs are marked stale (feed gap or staleness).
    pub fn is_stale(&self, coin: CoinId) -> bool {
        self.slot(coin).is_none_or(|slot| slot.stale)
    }
}

/// The synchronous strategy interface (SPEC-0010 §8).
pub trait Strategy: Send {
    /// Stable identifier used in metrics, persistence, and config.
    fn id(&self) -> StrategyId;

    /// Timing/fee configuration this strategy is using (metrics, diagnostics).
    fn cost(&self) -> CostModel;

    /// Coins, streams, and timers this strategy reacts to. Called once at
    /// startup, so the engine can precompute dispatch routes.
    fn interests(&self) -> Interests;

    /// React to a market change on a coin this strategy is interested in.
    fn on_market(&mut self, coin: CoinId, ctx: &Ctx<'_>, out: &mut Actions);

    /// React to one of this strategy's own order updates or fills.
    fn on_order(&mut self, update: &OrderEvent, ctx: &Ctx<'_>, out: &mut Actions);

    /// React to one of this strategy's timers firing.
    fn on_timer(&mut self, _timer: TimerId, _ctx: &Ctx<'_>, _out: &mut Actions) {}
}

/// The streams a strategy wants routed to it (SPEC-0010 §8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Stream {
    /// Best bid/offer updates.
    Bbo,
    /// Full book snapshots.
    Book,
    /// Trade prints.
    Trades,
    /// Asset context (funding, mark, OI).
    Ctx,
}

/// What a strategy declared interest in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Interests {
    /// Per-coin streams.
    pub coins: Vec<(CoinId, Stream)>,
    /// Timer periods in milliseconds.
    pub timers_ms: Vec<u64>,
}

impl Interests {
    /// Interest in one stream for a set of coins.
    pub fn coins(coins: impl IntoIterator<Item = CoinId>, stream: Stream) -> Self {
        Self {
            coins: coins.into_iter().map(|coin| (coin, stream)).collect(),
            timers_ms: Vec::new(),
        }
    }

    /// Add a timer period in milliseconds.
    pub fn every_ms(mut self, ms: u64) -> Self {
        self.timers_ms.push(ms);
        self
    }

    /// The coins this strategy cares about (any stream), de-duplicated by first
    /// appearance.
    pub fn distinct_coins(&self) -> Vec<CoinId> {
        let mut out = Vec::new();
        for (coin, _) in &self.coins {
            if !out.contains(coin) {
                out.push(*coin);
            }
        }
        out
    }
}

/// A convenience constructor for the common "one coin, book + ctx" interest.
pub fn book_and_ctx(coins: impl IntoIterator<Item = CoinId>) -> Interests {
    let coins: Vec<CoinId> = coins.into_iter().collect();
    Interests {
        coins: coins
            .iter()
            .flat_map(|coin| [(*coin, Stream::Book), (*coin, Stream::Ctx)])
            .collect(),
        timers_ms: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> CoinRegistry {
        CoinRegistry::from_coins(&["BTC".into(), "@1".into()])
    }

    fn level(px: i64) -> crate::types::Level {
        crate::types::Level {
            px: Decimal::from(px),
            sz: Decimal::ONE,
            n: 1,
        }
    }

    #[test]
    fn actions_buffer_reuses_capacity() {
        let mut actions = Actions::new();
        assert!(actions.is_empty());
        actions.place(OrderIntent {
            strategy: StrategyId::from("t"),
            coin: "BTC".into(),
            side: Side::Buy,
            limit_px: Some(Decimal::ONE),
            size: Decimal::ONE,
            tif: hl_arb_strategy::TimeInForce::Alo,
            reduce_only: false,
            rationale: "x".into(),
            cloid: None,
            signal_ms: 0,
            decision_ms: 0,
        });
        actions.cancel(Cloid([0; 16]));
        actions.modify(Cloid([1; 16]), Decimal::ONE, Decimal::TWO);
        assert_eq!(actions.len(), 3);
        let taken = actions.take();
        assert_eq!(taken.len(), 3);
        assert!(actions.is_empty());
        assert!(actions.as_slice().is_empty());
        actions.place_group(GroupIntent {
            legs: Vec::new(),
            all_or_none: true,
        });
        assert_eq!(actions.len(), 1);
        actions.clear();
        assert!(actions.is_empty());
    }

    #[test]
    fn ctx_reads_best_prices_and_funding() {
        let slot = MarketSlot {
            bbo: Some((Some(level(100)), Some(level(101)), Stamp::default())),
            ctx: Some((
                crate::types::AssetCtxLite {
                    funding: Decimal::from(7),
                    ..Default::default()
                },
                Stamp::default(),
            )),
            ..Default::default()
        };
        let markets = vec![slot];
        let account = AccountState::default();
        let registry = registry();
        let ctx = Ctx {
            now: Stamp::default(),
            markets: &markets,
            account: &account,
            registry: &registry,
        };
        assert_eq!(ctx.best_bid(CoinId(0)), Some(Decimal::from(100)));
        assert_eq!(ctx.best_ask(CoinId(0)), Some(Decimal::from(101)));
        assert_eq!(
            ctx.mid(CoinId(0)),
            Some(Decimal::from(100) + Decimal::ONE / Decimal::TWO)
        );
        assert_eq!(ctx.funding(CoinId(0)), Some(Decimal::from(7)));
        assert_eq!(ctx.coin_name(CoinId(0)), "BTC");
        assert_eq!(ctx.coin_name(CoinId(9)), "");
        assert!(!ctx.is_stale(CoinId(0)));
        assert!(ctx.is_stale(CoinId(9)));
    }

    #[test]
    fn ctx_mid_is_none_when_a_touch_side_is_empty() {
        let markets = vec![MarketSlot {
            bbo: Some((None, Some(level(101)), Stamp::default())),
            ..Default::default()
        }];
        let account = AccountState::default();
        let registry = registry();
        let ctx = Ctx {
            now: Stamp::default(),
            markets: &markets,
            account: &account,
            registry: &registry,
        };
        assert_eq!(ctx.best_bid(CoinId(0)), None);
        assert_eq!(ctx.best_ask(CoinId(0)), Some(Decimal::from(101)));
        assert_eq!(ctx.mid(CoinId(0)), None);
    }

    #[test]
    fn interests_dedupe_coins_and_collect_timers() {
        let interests = book_and_ctx([CoinId(0), CoinId(1)]).every_ms(1_000);
        assert_eq!(interests.distinct_coins(), vec![CoinId(0), CoinId(1)]);
        assert_eq!(interests.timers_ms, vec![1_000]);
        let listed = Interests::coins([CoinId(0)], Stream::Bbo);
        assert_eq!(listed.distinct_coins(), vec![CoinId(0)]);
    }

    #[test]
    fn order_event_reports_fills() {
        let event = OrderEvent {
            stamp: Stamp::default(),
            cloid: None,
            oid: 1,
            coin: CoinId(0),
            side: Side::Buy,
            px: Decimal::ONE,
            sz: Decimal::ONE,
            fee: Decimal::ZERO,
            maker: false,
            reduce_only: false,
            kind: OrderEventKind::Fill,
        };
        assert!(event.is_fill());
        let status = OrderEvent {
            kind: OrderEventKind::Status(VenueOrderStatus::Resting),
            ..event.clone()
        };
        assert!(!status.is_fill());
    }
}
