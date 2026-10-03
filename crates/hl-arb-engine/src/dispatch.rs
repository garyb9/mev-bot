//! Concrete strategy dispatcher (SPEC-0010 §8–§12, §15/§16, E-13 wiring).
//!
//! [`StrategyDispatcher`] is the engine-side half of "unblock E-13": it fills
//! the E-3 [`crate::run::Dispatcher`] seam with the real v2 strategy path,
//! integrating the E-5 order manager, the E-8 account stream state, the E-9
//! risk gate, the E-6 build/batch/send pipeline, and the E-10 latency recorder.
//!
//! It is **synchronous and non-blocking**: no I/O, no locks held across a call,
//! no `.await`. The engine thread owns it and the loop owns the market state,
//! which the loop hands in read-only through the `*_state` trait methods.
//!
//! ## Per-event flow
//!
//! 1. The loop drains an account/control update first, then market updates, then
//!    timers, then dispatches each dirty coin once (§9).
//! 2. For a dirty coin the dispatcher calls every strategy routed to it, with a
//!    read-only [`Ctx`] and one reusable [`Actions`] buffer, and then runs the
//!    emitted actions through the pipeline below.
//! 3. The pipeline gates each action with the risk gate, assigns/records cloids,
//!    inserts `PendingNew` orders, plans the batch (cancels first, then one bulk
//!    place), and hands unsigned posts to the exec backend. Backpressure trips
//!    the `exec_backpressure` breaker and marks the batch rejected (fail closed).
//! 4. Account events update the order manager, deliver [`OrderEvent`]s to the
//!    owning strategy, and pipeline the resulting actions.
//!
//! ## Notes / deliberate gaps
//!
//! - **Signing** stays off the engine thread (E-6): the engine hands unsigned
//!   posts to exec, so `t_signed` marks the end of build and `t_written`/`t_ack`
//!   stay unset. The `hl_engine_sign_seconds` span is therefore build time, and
//!   `hl_engine_handoff_seconds`/`hl_tick_to_order_seconds` are not recorded by
//!   the engine (they need the exec writer's stamps).
//! - **`Modify`/`PlaceGroup`** continue to be dropped by the E-6 builder; the
//!   dispatcher forwards them and records the drop.
//! - **Spot fills** do not update [`AccountState`]: spot balances are keyed by
//!   token symbol, and a fill carries a coin, not a token. Spot positions are
//!   corrected on the next `Reconcile`. Perp fills update the position directly.
//! - **Account-stream gaps** have no `AccountUpdate` variant; call
//!   [`StrategyDispatcher::on_gap`] from the reconnect path.
//! - **Gate counters** (`hl_strategy_intents_total` / per-reason `gates`) are
//!   not incremented: [`crate::instrument::LatencyRecorder`] has no intent
//!   counters yet, and adding them is a follow-up once the metric names exist.
//! - **`Control::ReloadLimits`** is a no-op here: re-reading the config and
//!   mutating the gate is the caller's job (`RiskGate::limits_mut`).
//! - **Strategy timers** are routed by [`StrategyDispatcher::register_timer`];
//!   scheduling them on the loop's heap is the caller's job (the v2 loop has no
//!   interest-driven scheduler yet).
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use hl_arb_strategy::StrategyId;
use rust_decimal::Decimal;
use smallvec::SmallVec;

use crate::builder::AssetTable;
use crate::clock::SharedClock;
use crate::exec::{ExecBackend, ReqIds, UnsignedPost};
use crate::instrument::LatencyRecorder;
use crate::journal::ActionSink;
use crate::orders::{CloidAssigner, OrderManager};
use crate::paper_exec::PaperExec;
use crate::risk::RiskGate;
use crate::routes::{Interests as RouteInterests, Routes, Stream as RouteStream};
use crate::run::Dispatcher;
use crate::state::{AccountState, AccountStreamState, EngineState, MarketSlot};
use crate::strategy::{Actions, Interests, Strategy, Stream};
use crate::timers::TimerId as HeapTimerId;
use crate::types::{AccountUpdate, Cloid, CoinId, CoinRegistry, Px, Side, Stamp};

mod events;
mod pipeline;
mod setup;
/// The dispatcher's runtime knobs.
#[derive(Debug, Clone)]
pub struct DispatcherConfig {
    /// Default `max_slippage_bps` for aggressive (`limit_px: None`) orders
    /// (SPEC-0010 §12; strategies may override per intent).
    pub max_slippage_bps: Decimal,
}

impl Default for DispatcherConfig {
    fn default() -> Self {
        Self {
            max_slippage_bps: Decimal::from(10),
        }
    }
}

/// `ExecBackend` for the boxed backend the dispatcher owns.
///
/// The E-6 helpers are generic over `B: ExecBackend` (sized), so the boxed
/// trait object needs this forwarding impl to be passed to [`dispatch`].
impl ExecBackend for Box<dyn ExecBackend + Send> {
    fn try_send(&mut self, post: UnsignedPost) -> bool {
        (**self).try_send(post)
    }
}

/// The concrete strategy dispatcher (SPEC-0010 E-13 wiring).
pub struct StrategyDispatcher {
    strategies: Vec<Box<dyn Strategy>>,
    /// Each strategy's declared interests, for cheap per-coin routing checks.
    strategy_interests: Vec<Interests>,
    /// The aggregate interests the loop builds its own [`Routes`] from.
    route_interests: Vec<RouteInterests>,
    /// Per-coin strategy indices, in registration order.
    routes: Routes,
    orders: OrderManager,
    account: AccountState,
    stream: AccountStreamState,
    risk: RiskGate,
    rate: crate::risk::RateBudget,
    table: AssetTable,
    registry: CoinRegistry,
    assigner: CloidAssigner,
    req_ids: ReqIds,
    exec: Option<Box<dyn ExecBackend + Send>>,
    /// Paper backend for `simulate`: fills on the engine thread, no I/O.
    paper: Option<PaperExec>,
    /// Account updates emitted by the paper backend, applied on the next tick.
    paper_updates: Vec<AccountUpdate>,
    recorder: LatencyRecorder,
    /// Reusable action buffer; strategies push into it and the pipeline drains it.
    actions: Actions,
    /// The strategy that placed each cloid, for `on_order` delivery.
    owners: BTreeMap<Cloid, StrategyId>,
    /// Strategy id → registration index.
    strategy_by_id: BTreeMap<StrategyId, usize>,
    /// Strategies paused by `Control::Pause`.
    paused: Vec<bool>,
    /// Whether dispatch is halted (kill switch or account-stream gap).
    halted: bool,
    /// Heap timer → (strategy index, strategy-local timer id).
    timer_owners: BTreeMap<HeapTimerId, (usize, crate::strategy::TimerId)>,
    /// Request id → cloids, kept so an `Error` post ack can resolve them
    /// (`OrderManager::on_post_ack` only handles per-order statuses).
    req_cloids: BTreeMap<u64, SmallVec<[Cloid; 8]>>,
    /// De-duplication of fills by venue `tid`, with the first-connect snapshot
    /// skip (SPEC-0002 H-3, SPEC-0010 E-8).
    fills: FillTracker,
    /// `oid → (cloid, owner)` for terminal orders pruned from the manager, so a
    /// late fill still reaches its owning strategy. Bounded FIFO.
    recent_routes: BTreeMap<u64, (Cloid, StrategyId)>,
    /// Shared market-drop counter the producer can increment.
    market_drops: Arc<AtomicU64>,
    /// Drops already folded into the recorder.
    drops_seen: u64,
    /// Shared resting-order count, refreshed once per iteration for the
    /// dead-man switch (read without touching the order path).
    resting_orders: Arc<AtomicUsize>,
    /// The engine's time source (SPEC-0010 §13): live wall clock, or the replay
    /// clock the driver advances per event.
    clock: SharedClock,
    /// Optional action journal for `simulate`/`replay` (SPEC-0010 §14); `None`
    /// in live, so nothing extra runs on the hot path.
    journal: Option<Box<dyn ActionSink + Send>>,
    config: DispatcherConfig,
}

impl Dispatcher for StrategyDispatcher {
    fn interests(&self) -> Vec<RouteInterests> {
        self.route_interests.clone()
    }

    fn on_coin_state(&mut self, coin: CoinId, stamp: Stamp, state: &EngineState) {
        self.dispatch_coin(coin, stamp, state);
    }

    fn on_timer_state(&mut self, id: HeapTimerId, stamp: Stamp, state: &EngineState) {
        self.dispatch_timer(id, stamp, state);
    }

    fn on_timer_registered(
        &mut self,
        heap: HeapTimerId,
        strategy_index: usize,
        strategy_timer: crate::strategy::TimerId,
    ) {
        self.register_timer(heap, strategy_index, strategy_timer);
    }

    fn on_account_state(&mut self, update: &AccountUpdate, state: &EngineState) {
        self.apply_account(update, state);
    }

    fn record_iteration(&mut self, iteration_ns: u64, events: usize) {
        self.recorder.record_iteration(iteration_ns, events);
        self.sync_market_drops();
        // Keep the order map bounded: drop terminal orders once their delivery
        // is done. The scan runs only when terminal orders exist. Before
        // pruning, remember each terminal order's `oid → owner` so a fill that
        // arrives after the terminal update and after pruning is still routed.
        if self.orders.terminal_count() > 0 {
            for order in self.orders.iter() {
                if order.state.is_terminal()
                    && let Some(oid) = order.oid
                {
                    self.recent_routes
                        .insert(oid, (order.cloid, order.strategy.clone()));
                }
            }
            self.orders.prune_terminal();
            self.owners
                .retain(|cloid, _| self.orders.get(*cloid).is_some());
            while self.recent_routes.len() > MAX_RECENT_ROUTES {
                let oldest = self.recent_routes.keys().next().copied();
                match oldest {
                    Some(oid) => {
                        self.recent_routes.remove(&oid);
                    }
                    None => break,
                }
            }
        }
        self.resting_orders
            .store(self.orders.resting_count(), Ordering::Relaxed);
    }
}

/// Convert a strategy's [`crate::strategy::Interests`] to the loop's route form.
fn to_route_interests(interests: &Interests) -> RouteInterests {
    let mut coins: Vec<CoinId> = Vec::new();
    let mut streams: Vec<RouteStream> = Vec::new();
    for (coin, stream) in &interests.coins {
        if !coins.contains(coin) {
            coins.push(*coin);
        }
        let route_stream = route_stream(*stream);
        if !streams.contains(&route_stream) {
            streams.push(route_stream);
        }
    }
    RouteInterests {
        coins,
        streams,
        lossless_trades: false,
        timers_ms: interests.timers_ms.clone(),
    }
}

fn route_stream(stream: Stream) -> RouteStream {
    match stream {
        Stream::Bbo => RouteStream::Bbo,
        Stream::Book => RouteStream::Book,
        Stream::Trades => RouteStream::Trades,
        Stream::Ctx => RouteStream::Ctx,
    }
}

/// Whether a strategy's declared interests include `coin` (any stream).
fn wants_coin(interests: &Interests, coin: CoinId) -> bool {
    interests.coins.iter().any(|(c, _)| *c == coin)
}

/// Convert the engine-side direction to the strategy-side direction used by
/// [`OrderEvent`].
fn strat_side(side: Side) -> hl_arb_strategy::Side {
    if matches!(side, Side::Buy) {
        hl_arb_strategy::Side::Buy
    } else {
        hl_arb_strategy::Side::Sell
    }
}

/// The journal's lowercase rendering of a strategy side.
fn strat_side_str(side: hl_arb_strategy::Side) -> &'static str {
    if side.is_buy() { "buy" } else { "sell" }
}

/// The journal's lowercase rendering of a venue side.
fn venue_side_str(side: Side) -> &'static str {
    if matches!(side, Side::Buy) {
        "buy"
    } else {
        "sell"
    }
}

/// The freshest market stamp for a slot, in monotonic ns.
fn slot_mono(slot: Option<&MarketSlot>) -> Option<u64> {
    let slot = slot?;
    let mut best: Option<u64> = None;
    let mut fold = |stamp: &Stamp| {
        if stamp.mono_ns > 0 {
            best = Some(best.map_or(stamp.mono_ns, |b| b.max(stamp.mono_ns)));
        }
    };
    if let Some((_, _, stamp)) = &slot.bbo {
        fold(stamp);
    }
    if let Some((_, stamp)) = &slot.book {
        fold(stamp);
    }
    if let Some((_, stamp)) = &slot.ctx {
        fold(stamp);
    }
    best
}

/// The event time carried by an account update (`Control` carries none).
fn account_stamp(update: &AccountUpdate) -> Stamp {
    match update {
        AccountUpdate::OrderUpdate { stamp, .. }
        | AccountUpdate::Fill { stamp, .. }
        | AccountUpdate::Fills { stamp, .. }
        | AccountUpdate::ResolveUnknown { stamp, .. }
        | AccountUpdate::UnknownExpired { stamp, .. }
        | AccountUpdate::PostAck { stamp, .. }
        | AccountUpdate::Reconcile { stamp, .. }
        | AccountUpdate::Funding { stamp, .. } => *stamp,
        AccountUpdate::Control(_) => Stamp::default(),
    }
}

/// Mid from a slot's freshest touch, or `None`.
fn mid(slot: &MarketSlot) -> Option<Px> {
    let bid = slot.best_bid()?.px;
    let ask = slot.best_ask()?.px;
    Some((bid + ask) / Decimal::TWO)
}

/// Holds a full `userFills` snapshot (the venue returns at most 2000) plus a
/// wide margin, so a reconnect snapshot's tids are all de-duplicated.
const MAX_SEEN_FILLS: usize = 8_192;

/// Cap on the recent-route cache for late fills of pruned terminal orders.
const MAX_RECENT_ROUTES: usize = 4_096;

/// A bounded, FIFO set of fill `tid`s.
///
/// `first_snapshot` distinguishes the first-connect `userFills` snapshot (whose
/// tids are recorded but not applied, since they are already in the starting
/// position) from a reconnect snapshot (whose unseen tids are the fills missed
/// during the gap, and are applied).
#[derive(Debug)]
struct FillTracker {
    seen: HashSet<u64>,
    order: VecDeque<u64>,
    first_snapshot: bool,
}

impl FillTracker {
    fn new() -> Self {
        Self {
            seen: HashSet::new(),
            order: VecDeque::new(),
            first_snapshot: false,
        }
    }

    fn insert(&mut self, tid: u64) {
        if self.seen.insert(tid) {
            self.order.push_back(tid);
            while self.order.len() > MAX_SEEN_FILLS {
                if let Some(old) = self.order.pop_front() {
                    self.seen.remove(&old);
                }
            }
        }
    }

    /// Record a tid without asking whether it is new (first snapshot).
    fn record(&mut self, tid: u64) {
        self.insert(tid);
    }

    /// Mark a tid seen; `true` means it is new and should be applied.
    fn observe(&mut self, tid: u64) -> bool {
        if self.seen.contains(&tid) {
            return false;
        }
        self.insert(tid);
        true
    }

    /// Observe a live (non-snapshot) fill. A live fill also ends the
    /// first-connect phase, so a later reconnect snapshot is applied.
    fn observe_live(&mut self, tid: u64) -> bool {
        self.first_snapshot = true;
        self.observe(tid)
    }
}

#[cfg(test)]
mod tests;
