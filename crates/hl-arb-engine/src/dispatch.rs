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

use hl_arb_client::RejectReason;
use hl_arb_risk::Decision;
use hl_arb_strategy::{OrderIntent, StrategyId};
use rust_decimal::Decimal;
use smallvec::SmallVec;

use crate::builder::{AssetTable, plan_iteration};
use crate::clock::{LiveClock, SharedClock};
use crate::exec::{ExecBackend, ReqIds, UnsignedPost, apply_post_ack, dispatch};
use crate::instrument::{LatencyRecorder, Metric, Stamps};
use crate::journal::{ActionSink, JournalEntry};
use crate::orders::{CloidAssigner, LiveOrder, OrderManager, OrderState};
use crate::paper_exec::{PaperExec, paper_cancels_from_post, paper_orders_from_post};
use crate::risk::{RiskCtx, RiskGate, cancel_all_cloids};
use crate::routes::{Interests as RouteInterests, Routes, Stream as RouteStream};
use crate::run::Dispatcher;
use crate::state::{AccountState, AccountStreamState, EngineState, MarketSlot};
use crate::strategy::{
    Action, Actions, Ctx, Interests, OrderEvent, OrderEventKind, Strategy, Stream,
};
use crate::timers::TimerId as HeapTimerId;
use crate::types::{
    AccountUpdate, Cloid, CoinId, CoinRegistry, Control, PostResult, Px, Side, Stamp, Sz,
};

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

impl StrategyDispatcher {
    /// Build a dispatcher from strategies and the engine-owned building blocks.
    ///
    /// `registry` fixes the interned coin universe (and therefore the order and
    /// account sizing). `exec` is `None` in `observe` mode.
    pub fn new(
        strategies: Vec<Box<dyn Strategy>>,
        registry: CoinRegistry,
        table: AssetTable,
        risk: RiskGate,
        exec: Option<Box<dyn ExecBackend + Send>>,
        config: DispatcherConfig,
    ) -> Self {
        let coin_count = registry.len();
        let strategy_interests: Vec<Interests> = strategies
            .iter()
            .map(|strategy| strategy.interests())
            .collect();
        let route_interests: Vec<RouteInterests> =
            strategy_interests.iter().map(to_route_interests).collect();
        let routes = Routes::build(coin_count, &route_interests);

        let mut strategy_by_id: BTreeMap<StrategyId, usize> = BTreeMap::new();
        let mut paused = vec![false; strategies.len()];
        for (index, strategy) in strategies.iter().enumerate() {
            strategy_by_id.insert(strategy.id(), index);
        }
        paused.shrink_to_fit();

        Self {
            strategies,
            strategy_interests,
            route_interests,
            routes,
            orders: OrderManager::new(coin_count),
            account: AccountState::new(coin_count),
            stream: AccountStreamState::new(),
            risk,
            rate: crate::risk::RateBudget::new(crate::risk::RateBudgetConfig::default()),
            table,
            registry,
            assigner: CloidAssigner::new(),
            req_ids: ReqIds::new(),
            exec,
            paper: None,
            paper_updates: Vec::new(),
            recorder: LatencyRecorder::new(),
            actions: Actions::new(),
            owners: BTreeMap::new(),
            strategy_by_id,
            paused,
            halted: false,
            timer_owners: BTreeMap::new(),
            req_cloids: BTreeMap::new(),
            fills: FillTracker::new(),
            recent_routes: BTreeMap::new(),
            market_drops: Arc::new(AtomicU64::new(0)),
            drops_seen: 0,
            resting_orders: Arc::new(AtomicUsize::new(0)),
            clock: Arc::new(LiveClock::new()),
            journal: None,
            config,
        }
    }

    /// Use an explicit clock (replay); defaults to [`LiveClock`].
    pub fn with_clock(mut self, clock: SharedClock) -> Self {
        self.clock = clock;
        self
    }

    /// Record every approved action and fill into `sink` (SPEC-0010 §14).
    pub fn with_journal(mut self, sink: Box<dyn ActionSink + Send>) -> Self {
        self.journal = Some(sink);
        self
    }

    /// Pin the cloid prefix so simulate/replay are byte-identical (SPEC-0010
    /// G-6). Live must keep the random default.
    pub fn with_cloid_prefix(mut self, prefix: u64) -> Self {
        self.assigner = CloidAssigner::with_prefix(prefix);
        self
    }

    /// The engine clock (introspection / driver sharing).
    pub fn clock(&self) -> &SharedClock {
        &self.clock
    }

    /// The account state (positions, margin) the dispatcher is tracking.
    pub fn account(&self) -> &AccountState {
        &self.account
    }

    /// The order manager (read-only introspection).
    pub fn orders(&self) -> &OrderManager {
        &self.orders
    }

    /// The risk gate (mostly for tests and diagnostics).
    pub fn risk(&self) -> &RiskGate {
        &self.risk
    }

    /// Replace the exec backend (`None` for `observe`).
    pub fn set_exec(&mut self, exec: Option<Box<dyn ExecBackend + Send>>) {
        self.exec = exec;
    }

    /// Attach the paper backend used by `simulate`.
    ///
    /// The paper backend fills synchronously on the engine thread against the
    /// same market state, emitting account updates that are folded back into the
    /// order manager; it is mutually exclusive with `exec` (an iteration that
    /// has paper never touches the live exec channel).
    pub fn with_paper(mut self, paper: PaperExec) -> Self {
        self.paper = Some(paper);
        self
    }

    /// A shared resting-order count refreshed once per loop iteration.
    ///
    /// The dead-man switch reads it off the order path; the engine stores it
    /// (relaxed) in [`Self::record_iteration`], so reading never blocks an order.
    pub fn resting_order_counter(&self) -> Arc<AtomicUsize> {
        self.resting_orders.clone()
    }

    /// Set or clear the kill switch and the dispatch halt.
    pub fn set_kill(&mut self, active: bool) {
        if active {
            let _ = self.risk.kill().set();
            self.halted = true;
        } else {
            let _ = self.risk.kill().clear();
            self.halted = false;
        }
    }

    /// Record an account-stream gap (SPEC-0010 §15/§16): halt new places until a
    /// clean reconcile resumes them.
    pub fn on_gap(&mut self) {
        self.stream.on_gap();
    }

    /// Apply a reconcile outcome (SPEC-0010 §15): fix account value/margin and
    /// resume places only if no `Unknown` orders or drift remain.
    ///
    /// The diff itself is the caller's reconciler (`Reconciler::diff` /
    /// `apply_drift`); this records the outcome and the stream halt state.
    pub fn apply_reconcile(
        &mut self,
        snapshot: &crate::types::AccountSnapshot,
        has_unknown: bool,
        drift_remaining: bool,
    ) {
        self.account.account_value = snapshot.account_value;
        self.account.margin_used = snapshot.margin_used;
        let _ = self.stream.on_clean_reconcile(has_unknown, drift_remaining);
    }

    /// Register a scheduled heap timer's owning strategy (SPEC-0010 §8).
    pub fn register_timer(
        &mut self,
        id: HeapTimerId,
        strategy_index: usize,
        strategy_timer: crate::strategy::TimerId,
    ) {
        self.timer_owners
            .insert(id, (strategy_index, strategy_timer));
    }

    /// The latency recorder (metrics export).
    pub fn recorder(&self) -> &LatencyRecorder {
        &self.recorder
    }

    /// Mutable access to the latency recorder.
    pub fn recorder_mut(&mut self) -> &mut LatencyRecorder {
        &mut self.recorder
    }

    /// Export the recorder's window and reset it.
    pub fn flush(&mut self) -> Vec<(&'static str, Metric)> {
        self.sync_market_drops();
        self.recorder.flush()
    }

    /// A shared counter producers increment when a market update is dropped on a
    /// full channel; folded into `hl_engine_market_drops_total` each iteration.
    pub fn market_drop_counter(&self) -> Arc<AtomicU64> {
        self.market_drops.clone()
    }

    /// Fold the atomic market-drop counter delta into the recorder.
    fn sync_market_drops(&mut self) {
        let total = self.market_drops.load(Ordering::Relaxed);
        let delta = total.saturating_sub(self.drops_seen);
        for _ in 0..delta {
            self.recorder.record_market_drop();
        }
        self.drops_seen = total;
    }

    fn dispatch_coin(&mut self, coin: CoinId, stamp: Stamp, state: &EngineState) {
        // Apply any paper updates queued by a prior iteration (notably the
        // kill-switch cancels) even while halted, so `simulate` reflects the
        // cancellation within one iteration.
        self.tick_paper(state);
        if self.halted {
            return;
        }
        let indices: SmallVec<[usize; 8]> = self.routes.for_coin(coin).iter().copied().collect();
        if indices.is_empty() {
            return;
        }
        // Event time of the update that marked this coin dirty (SPEC-0010 §8):
        // fall back to the slot's freshest stamp if the caller passed none.
        let t_recv = if stamp.mono_ns > 0 {
            stamp.mono_ns
        } else {
            slot_mono(state.slot(coin)).unwrap_or(0)
        };
        for index in indices {
            if self.paused.get(index).copied().unwrap_or(false) {
                continue;
            }
            if !wants_coin(&self.strategy_interests[index], coin) {
                continue;
            }
            let ctx = Ctx {
                now: stamp,
                markets: state.slots(),
                account: &self.account,
                registry: &self.registry,
            };
            self.strategies[index].on_market(coin, &ctx, &mut self.actions);
        }
        let t_decided = self.clock.mono_ns();
        self.run_actions(stamp, state, t_recv, t_decided);
        self.tick_paper(state);
    }

    fn dispatch_timer(&mut self, id: HeapTimerId, stamp: Stamp, state: &EngineState) {
        self.tick_paper(state);
        if self.halted {
            return;
        }
        let Some(&(index, timer)) = self.timer_owners.get(&id) else {
            return;
        };
        if self.paused.get(index).copied().unwrap_or(false) {
            return;
        }
        let ctx = Ctx {
            now: stamp,
            markets: state.slots(),
            account: &self.account,
            registry: &self.registry,
        };
        self.strategies[index].on_timer(timer, &ctx, &mut self.actions);
        let t_decided = self.clock.mono_ns();
        self.run_actions(stamp, state, 0, t_decided);
        self.tick_paper(state);
    }

    fn apply_account(&mut self, update: &AccountUpdate, state: &EngineState) {
        self.stream.on_account_update(update);
        let stamp = account_stamp(update);
        let t_recv = stamp.mono_ns;

        match update {
            AccountUpdate::Control(control) => self.handle_control(control),
            AccountUpdate::OrderUpdate {
                cloid,
                oid,
                status,
                filled_sz,
                avg_px,
                ..
            } => {
                self.orders
                    .on_order_update(*cloid, *status, *filled_sz, *avg_px);
                self.orders.record_oid(*oid, *cloid);
                let event = self.orders.get(*cloid).map(|order| {
                    (
                        order.strategy.clone(),
                        OrderEvent {
                            stamp,
                            cloid: Some(*cloid),
                            oid: *oid,
                            coin: order.coin,
                            side: strat_side(order.side),
                            px: *avg_px,
                            sz: Decimal::ZERO,
                            fee: Decimal::ZERO,
                            maker: false,
                            reduce_only: order.reduce_only,
                            kind: OrderEventKind::Status(*status),
                        },
                    )
                });
                if let Some((owner, event)) = event {
                    self.deliver_to(&owner, &event, state);
                }
            }
            AccountUpdate::Fill {
                cloid,
                oid,
                tid,
                coin,
                side,
                px,
                sz,
                fee,
                ..
            } => {
                // Live fills skip tids already seen (a reconnect snapshot may
                // re-deliver them).
                if self.fills.observe_live(*tid) {
                    self.apply_fill(*cloid, *oid, *coin, *side, *px, *sz, *fee, stamp, state);
                }
            }
            AccountUpdate::Fills { fills, stamp } => {
                if self.fills.first_snapshot {
                    // A reconnect snapshot applies only the tids missed during
                    // the gap (the resync H-3 asks for).
                    for fill in fills {
                        if self.fills.observe(fill.tid) {
                            self.apply_fill(
                                fill.cloid, fill.oid, fill.coin, fill.side, fill.px, fill.sz,
                                fill.fee, *stamp, state,
                            );
                        }
                    }
                } else {
                    // The first-connect snapshot is already in the starting
                    // position: record its tids without applying them.
                    for fill in fills {
                        self.fills.record(fill.tid);
                    }
                    self.fills.first_snapshot = true;
                }
            }
            AccountUpdate::ResolveUnknown { cloid, status, .. } => {
                // Apply the venue's answer only while the order is still
                // `Unknown`: a stale answer after a newer stream update must not
                // move the order backwards (SPEC-0002 H-2).
                if self
                    .orders
                    .get(*cloid)
                    .is_some_and(|order| order.state.is_unknown())
                {
                    let _ = crate::reconcile::resolve_unknown(&mut self.orders, *cloid, status);
                }
            }
            AccountUpdate::UnknownExpired { cloid, .. } => {
                // The bounded `orderStatus` retries never resolved the order and
                // it can no longer land: resolve it as not placed.
                if self
                    .orders
                    .get(*cloid)
                    .is_some_and(|order| order.state.is_unknown())
                {
                    self.orders
                        .set_state(*cloid, OrderState::Rejected(RejectReason::Unknown));
                }
            }
            AccountUpdate::PostAck { req_id, result, .. } => {
                self.handle_post_ack(*req_id, result, stamp, state);
            }
            AccountUpdate::Reconcile { snapshot, .. } => {
                self.account.account_value = snapshot.account_value;
                self.account.margin_used = snapshot.margin_used;
            }
            AccountUpdate::Funding { usdc, .. } => {
                // Funding is realized PnL; it moves account value, not position.
                // If a future venue reports otherwise, defer to the reconcile.
                self.account.account_value += *usdc;
            }
        }

        // SPEC-0010 §16: resume new places once every `Unknown` order has been
        // reconciled. Only the self-resolving `exec_error` breaker is cleared
        // here; `exec_backpressure` and operator breakers stay tripped.
        if !self.orders.has_unknown() {
            self.risk.breakers_mut().clear_label("exec_error");
        }

        let t_decided = self.clock.mono_ns();
        self.run_actions(stamp, state, t_recv, t_decided);
    }

    /// Apply one already-deduplicated fill: move the position, advance the
    /// order's filled size, and deliver a fill event to the owning strategy.
    ///
    /// A fill is mapped to its order by `cloid` (when the wire carried one),
    /// else by the `oid → cloid` index, else by the recent-route cache for an
    /// order pruned after it filled. A fill that arrives after the order's
    /// terminal update and after pruning still reaches the strategy.
    #[allow(clippy::too_many_arguments)]
    fn apply_fill(
        &mut self,
        cloid: Option<Cloid>,
        oid: u64,
        coin: CoinId,
        side: Side,
        px: Px,
        sz: Sz,
        fee: Px,
        stamp: Stamp,
        state: &EngineState,
    ) {
        let cloid = cloid
            .or_else(|| self.orders.cloid_for_oid(oid))
            .or_else(|| self.recent_routes.get(&oid).map(|(cloid, _)| *cloid));

        if self.journal.is_some() {
            let entry = JournalEntry::Fill {
                cloid: cloid.map(|cloid| cloid.to_hex()),
                coin: self.registry.coin(coin).unwrap_or("").to_string(),
                side: venue_side_str(side).to_string(),
                px,
                sz,
                fee,
            };
            if let Some(sink) = self.journal.as_mut() {
                sink.record(&entry);
            }
        }

        // A perp fill moves the position directly; a spot fill is left to the
        // reconciler (balances are keyed by token, not coin).
        if !self.is_spot(coin) {
            let signed = if matches!(side, Side::Buy) { sz } else { -sz };
            let current = self.account.position_szi(coin);
            self.account.set_position_szi(coin, current + signed);
        }
        if let Some(cloid) = cloid {
            self.orders.on_fill(Some(cloid), sz);
        }

        let route = cloid
            .and_then(|cloid| {
                self.orders
                    .get(cloid)
                    .map(|order| (cloid, order.strategy.clone(), order.reduce_only))
            })
            .or_else(|| {
                self.recent_routes
                    .get(&oid)
                    .map(|(cloid, owner)| (*cloid, owner.clone(), false))
            });
        if let Some((cloid, owner, reduce_only)) = route {
            let event = OrderEvent {
                stamp,
                cloid: Some(cloid),
                oid,
                coin,
                side: strat_side(side),
                px,
                sz,
                fee,
                maker: false,
                reduce_only,
                kind: OrderEventKind::Fill,
            };
            self.deliver_to(&owner, &event, state);
        }
    }

    fn handle_control(&mut self, control: &Control) {
        match control {
            Control::KillSwitch => {
                let _ = self.risk.kill().set();
                self.halted = true;
                for cloid in cancel_all_cloids(&self.orders) {
                    self.actions.cancel(cloid);
                }
            }
            Control::Resume => {
                let _ = self.risk.kill().clear();
                self.halted = false;
            }
            Control::Pause { strategy } => {
                let id = StrategyId::from(strategy.clone());
                if let Some(&index) = self.strategy_by_id.get(&id) {
                    self.paused[index] = true;
                }
            }
            Control::ReloadLimits => {
                // Re-reading config and mutating the gate is the caller's job.
            }
        }
    }

    fn handle_post_ack(
        &mut self,
        req_id: u64,
        result: &PostResult,
        stamp: Stamp,
        state: &EngineState,
    ) {
        match result {
            PostResult::Statuses(acks) => {
                let cloids = self.req_cloids.remove(&req_id);
                apply_post_ack(&mut self.orders, req_id, result);
                let Some(cloids) = cloids else {
                    return;
                };
                for (cloid, ack) in cloids.iter().zip(acks.iter()) {
                    let event = self.orders.get(*cloid).map(|order| {
                        (
                            order.strategy.clone(),
                            OrderEvent {
                                stamp,
                                cloid: Some(*cloid),
                                oid: ack.oid.unwrap_or(0),
                                coin: order.coin,
                                side: strat_side(order.side),
                                px: Decimal::ZERO,
                                sz: Decimal::ZERO,
                                fee: Decimal::ZERO,
                                maker: false,
                                reduce_only: order.reduce_only,
                                kind: OrderEventKind::Status(ack.status),
                            },
                        )
                    });
                    if let Some((owner, event)) = event {
                        self.deliver_to(&owner, &event, state);
                    }
                }
            }
            PostResult::Rejected(reason) => {
                // The venue said no (or the post was never sent): the orders are
                // terminal `Rejected`, not `Unknown`, so the breaker clears.
                tracing::debug!(
                    req_id,
                    %reason,
                    "post rejected before/at the venue; orders terminal"
                );
                self.reject_requests(&[req_id]);
            }
            PostResult::Error(reason) => {
                // A lost reply does not mean the order was rejected: it may be
                // resting. Mark it `Unknown` (fail closed), trip the breaker to
                // halt new places, and let the exec layer reconcile it by cloid
                // via `orderStatus` (SPEC-0010 §10/§16, SPEC-0002 H-2). An
                // unknown order still counts for exposure and the dead-man
                // switch, and blocks new non-reduce-only places on its coin.
                self.risk.breakers_mut().trip("exec_error");
                tracing::warn!(
                    req_id,
                    %reason,
                    "post ack error: orders marked Unknown, breaker tripped"
                );
                let Some(cloids) = self.req_cloids.remove(&req_id) else {
                    return;
                };
                for cloid in &cloids {
                    self.orders.set_state(*cloid, OrderState::Unknown);
                }
            }
        }
    }

    fn deliver_to(&mut self, owner: &StrategyId, event: &OrderEvent, state: &EngineState) {
        if self.halted {
            return;
        }
        let Some(&index) = self.strategy_by_id.get(owner) else {
            return;
        };
        if self.paused.get(index).copied().unwrap_or(false) {
            return;
        }
        let ctx = Ctx {
            now: event.stamp,
            markets: state.slots(),
            account: &self.account,
            registry: &self.registry,
        };
        self.strategies[index].on_order(event, &ctx, &mut self.actions);
    }

    fn is_spot(&self, coin: CoinId) -> bool {
        self.table.meta(coin).is_some_and(|meta| meta.is_spot)
    }

    /// Gate the buffered actions, assign cloids, insert live orders, plan the
    /// batch, and dispatch it (SPEC-0010 §10–§12).
    fn run_actions(&mut self, stamp: Stamp, state: &EngineState, t_recv: u64, t_decided: u64) {
        if self.actions.is_empty() {
            return;
        }
        let actions = self.actions.take();
        self.actions.clear();

        let now_ms = self.clock.now_ms();
        let mut approved: SmallVec<[Action; 16]> = SmallVec::new();
        for mut action in actions {
            if self.gate_action(&mut action, state, now_ms) {
                approved.push(action);
            }
        }

        let t_risked = self.clock.mono_ns();
        if approved.is_empty() {
            return;
        }
        self.record_journal(&approved);
        self.plan_and_dispatch(&approved, state, t_recv, stamp.mono_ns, t_decided, t_risked);
    }

    /// Append the risk-approved actions to the journal, if one is attached
    /// (SPEC-0010 §14). Entries are built before the sink borrow to keep the
    /// order-manager lookup separate.
    fn record_journal(&mut self, approved: &[Action]) {
        if self.journal.is_none() {
            return;
        }
        let mut entries: SmallVec<[JournalEntry; 16]> = SmallVec::new();
        for action in approved {
            match action {
                Action::Place(intent) => entries.push(JournalEntry::Place {
                    cloid: intent.cloid.clone().unwrap_or_default(),
                    coin: intent.coin.clone(),
                    side: strat_side_str(intent.side).to_string(),
                    limit_px: intent.limit_px,
                    size: intent.size,
                }),
                Action::Cancel { cloid } => entries.push(JournalEntry::Cancel {
                    cloid: cloid.to_hex(),
                }),
                Action::Modify { cloid, px, sz } => entries.push(JournalEntry::Modify {
                    cloid: cloid.to_hex(),
                    px: *px,
                    sz: *sz,
                }),
                Action::PlaceGroup(_) => {}
            }
        }
        if let Some(sink) = self.journal.as_mut() {
            for entry in &entries {
                sink.record(entry);
            }
        }
    }

    /// Gate one action. Returns whether it should proceed to the builder.
    ///
    /// The check is done before any mutable borrow of `action`, so a resized
    /// place can then be rewritten in place.
    fn gate_action(&mut self, action: &mut Action, state: &EngineState, now_ms: u64) -> bool {
        // The stream halt only blocks risk-increasing places.
        if let Action::Place(intent) = action
            && self.stream.places_halted()
            && !intent.reduce_only
        {
            return false;
        }

        // Resolve the coin without a live mutable borrow of `action`.
        let coin = match action {
            Action::Place(intent) => self.registry.id(&intent.coin),
            Action::Cancel { cloid } => self.orders.get(*cloid).map(|order| order.coin),
            Action::Modify { cloid, .. } => self.orders.get(*cloid).map(|order| order.coin),
            Action::PlaceGroup(_) => return false,
        };
        let Some(coin) = coin else {
            // Unknown coin/cloid: cancels/modifies reduce risk and are left for
            // the builder to drop; an unknown place is dropped here.
            return !matches!(action, Action::Place(_));
        };
        let (Some(meta), Some(slot)) = (self.table.meta(coin), state.slot(coin)) else {
            return !matches!(action, Action::Place(_));
        };

        let decision = {
            let ctx = RiskCtx {
                coin,
                orders: &self.orders,
                account: &self.account,
                slot,
                meta: &meta,
                rate: &self.rate,
                now_ms,
            };
            self.risk.check(action, &ctx)
        };

        match (action, decision) {
            (Action::Place(_), Decision::Reject(_)) => false,
            (Action::Place(intent), Decision::Approve) => {
                self.finish_place(coin, intent, slot);
                true
            }
            (Action::Place(intent), Decision::Resize(size)) => {
                intent.size = size;
                self.finish_place(coin, intent, slot);
                true
            }
            (Action::Cancel { .. } | Action::Modify { .. }, decision) => decision.is_allowed(),
            (Action::PlaceGroup(_), _) => false,
        }
    }

    /// Assign a cloid, record the owner, and insert a `PendingNew` live order.
    fn finish_place(&mut self, coin: CoinId, intent: &mut OrderIntent, slot: &MarketSlot) {
        let cloid = intent
            .cloid
            .as_deref()
            .and_then(Cloid::from_hex)
            .unwrap_or_else(|| self.assigner.next());
        intent.cloid = Some(cloid.to_hex());
        let px = intent
            .limit_px
            .or_else(|| mid(slot))
            .unwrap_or(Decimal::ZERO);
        let order = LiveOrder {
            cloid,
            coin,
            side: if intent.side.is_buy() {
                Side::Buy
            } else {
                Side::Sell
            },
            px,
            sz: intent.size,
            filled_sz: Decimal::ZERO,
            reduce_only: intent.reduce_only,
            strategy: intent.strategy.clone(),
            state: OrderState::PendingNew,
            req_id: None,
            oid: None,
        };
        self.owners.insert(cloid, intent.strategy.clone());
        self.orders.insert(order);
    }

    #[allow(clippy::too_many_arguments)]
    fn plan_and_dispatch(
        &mut self,
        approved: &[Action],
        state: &EngineState,
        t_recv: u64,
        t_dequeued: u64,
        t_decided: u64,
        t_risked: u64,
    ) {
        let touch = |coin: CoinId, is_buy: bool| -> Option<Px> {
            let slot = state.slot(coin)?;
            // An aggressive order only needs the side it crosses: ask for a
            // buy, bid for a sell. The other side being empty is not a reason
            // to drop it.
            let level = if is_buy {
                slot.best_ask()
            } else {
                slot.best_bid()
            }?;
            Some(level.px)
        };
        let mut batch = plan_iteration(
            approved,
            &self.registry,
            &self.table,
            &self.orders,
            &self.assigner,
            &touch,
            self.config.max_slippage_bps,
            &mut self.req_ids,
        );
        // Carry the market frame's monotonic read time so the live transport
        // can stamp the end-to-end `hl_tick_to_order_seconds` after the socket
        // write (SPEC-0002 H-7).
        for post in &mut batch.posts {
            post.recv_mono_ns = t_recv;
        }

        // A built batch may drop places (bad size, min notional, missing asset).
        // Remove their provisional live orders so exposure is not stranded.
        for (cloid, _reason) in &batch.dropped {
            self.orders.remove(*cloid);
            self.owners.remove(cloid);
        }

        let batch_reqs: SmallVec<[u64; 2]> = batch.posts.iter().map(|post| post.req_id).collect();
        let paper_mode = self.exec.is_none() && self.paper.is_some();
        if !paper_mode {
            for post in &batch.posts {
                self.req_cloids.insert(post.req_id, post.cloids.clone());
            }
        }

        let t_signed = self.clock.mono_ns();
        if let Some(exec) = self.exec.as_mut() {
            if dispatch(batch, exec, &mut self.orders).is_err() {
                // Outbound channel full or exec down: fail closed. Rare, so
                // a WARN is allowed (never per-event at INFO; §4).
                let _ = self.risk.breakers_mut().trip("exec_backpressure");
                tracing::warn!(
                    drop = batch_reqs.len(),
                    "exec backpressure: batch rejected, breaker tripped"
                );
                self.reject_requests(&batch_reqs);
            }
        } else if paper_mode {
            // `simulate`: the paper backend fills synchronously; its account
            // updates are applied on this iteration's paper tick.
            let updates = self.feed_paper(&batch, self.clock.now_ms());
            self.paper_updates.extend(updates);
        } else {
            // No backend (`observe`): never leave orders pending. Fail closed.
            self.reject_requests(&batch_reqs);
        }
        let t_handoff = self.clock.mono_ns();

        // `t_written`/`t_ack` come from the exec layer and are unset here, so the
        // headline tick-to-order and handoff histograms are not recorded until
        // the exec writer reports; the engine-side decide/risk/sign spans are.
        self.recorder.record(&Stamps {
            t_recv,
            t_decoded: 0,
            t_dequeued,
            t_decided,
            t_risked,
            t_signed,
            t_handoff,
            t_written: 0,
            t_ack: 0,
        });
    }

    /// Feed one built batch to the paper backend, returning its immediate
    /// account updates (cancel acknowledgements; places are queued for latency).
    fn feed_paper(
        &mut self,
        batch: &crate::builder::BuiltBatch,
        now_ms: u64,
    ) -> Vec<AccountUpdate> {
        let Some(paper) = self.paper.as_mut() else {
            return Vec::new();
        };
        let mut updates = Vec::new();
        for post in &batch.posts {
            match &post.action {
                hl_arb_client::Action::Order { .. } => {
                    let orders = paper_orders_from_post(post, &self.registry, &self.table);
                    updates.extend(paper.submit(&orders, &self.registry, &[], now_ms));
                }
                hl_arb_client::Action::CancelByCloid { .. } => {
                    let cancels = paper_cancels_from_post(post);
                    updates.extend(paper.cancel(&cancels, now_ms));
                }
                _ => {}
            }
        }
        updates
    }

    /// Advance the paper backend against the current books and fold its account
    /// updates (fills, status changes) back through [`Self::apply_account`].
    fn tick_paper(&mut self, state: &EngineState) {
        if self.paper.is_none() {
            return;
        }
        let now_ms = self.clock.now_ms();
        let mut updates = std::mem::take(&mut self.paper_updates);
        if let Some(paper) = self.paper.as_mut() {
            updates.extend(paper.on_market(&self.registry, state.slots(), now_ms));
        }
        for update in &updates {
            self.apply_account(update, state);
        }
    }

    /// Mark every `PendingNew` order from these requests rejected (fail closed).
    fn reject_requests(&mut self, reqs: &[u64]) {
        for req_id in reqs {
            let Some(cloids) = self.req_cloids.remove(req_id) else {
                continue;
            };
            for cloid in cloids {
                if self
                    .orders
                    .get(cloid)
                    .is_some_and(|order| order.state == OrderState::PendingNew)
                {
                    self.orders
                        .set_state(cloid, OrderState::Rejected(RejectReason::Unknown));
                }
            }
        }
    }
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
