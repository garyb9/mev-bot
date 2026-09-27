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

use mev_core::clock::{Clock, SystemClock};
use mev_hl_client::RejectReason;
use mev_risk::Decision;
use mev_strategy::{OrderIntent, StrategyId};
use rust_decimal::Decimal;
use smallvec::SmallVec;

use crate::builder::{AssetTable, plan_iteration};
use crate::exec::{ExecBackend, ReqIds, UnsignedPost, apply_post_ack, dispatch};
use crate::instrument::{LatencyRecorder, Metric, Stamps};
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
            config,
        }
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
        if self.halted {
            return;
        }
        let indices: SmallVec<[usize; 8]> = self.routes.for_coin(coin).iter().copied().collect();
        if indices.is_empty() {
            return;
        }
        let t_recv = slot_mono(state.slot(coin)).unwrap_or(0);
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
        let t_decided = now_ns();
        self.run_actions(stamp, state, t_recv, t_decided);
        self.tick_paper(state);
    }

    fn dispatch_timer(&mut self, id: HeapTimerId, stamp: Stamp, state: &EngineState) {
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
        let t_decided = now_ns();
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

        let t_decided = now_ns();
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

        let now_ms = now_ms();
        let mut approved: SmallVec<[Action; 16]> = SmallVec::new();
        for mut action in actions {
            if self.gate_action(&mut action, state, now_ms) {
                approved.push(action);
            }
        }

        let t_risked = now_ns();
        if approved.is_empty() {
            return;
        }
        self.plan_and_dispatch(&approved, state, t_recv, stamp.mono_ns, t_decided, t_risked);
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
        let touch = |coin: CoinId| -> Option<(Px, Px)> {
            let slot = state.slot(coin)?;
            let bid = slot.best_bid()?.px;
            let ask = slot.best_ask()?.px;
            Some((bid, ask))
        };
        let batch = plan_iteration(
            approved,
            &self.registry,
            &self.table,
            &self.orders,
            &self.assigner,
            &touch,
            self.config.max_slippage_bps,
            &mut self.req_ids,
        );

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

        let t_signed = now_ns();
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
            let updates = self.feed_paper(&batch, now_ms());
            self.paper_updates.extend(updates);
        } else {
            // No backend (`observe`): never leave orders pending. Fail closed.
            self.reject_requests(&batch_reqs);
        }
        let t_handoff = now_ns();

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
                mev_hl_client::Action::Order { .. } => {
                    let orders = paper_orders_from_post(post, &self.registry, &self.table);
                    updates.extend(paper.submit(&orders, &self.registry, &[], now_ms));
                }
                mev_hl_client::Action::CancelByCloid { .. } => {
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
        let now_ms = now_ms();
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
fn strat_side(side: Side) -> mev_strategy::Side {
    if matches!(side, Side::Buy) {
        mev_strategy::Side::Buy
    } else {
        mev_strategy::Side::Sell
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

/// Engine monotonic time in nanoseconds, on the loop's millisecond base.
fn now_ns() -> u64 {
    SystemClock.now_ms().saturating_mul(1_000_000)
}

/// Wall-clock milliseconds, for the rate-budget refill.
fn now_ms() -> u64 {
    SystemClock.now_ms()
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
mod tests {
    use std::str::FromStr;
    use std::sync::Mutex;

    use mev_hl_client::types::{AssetMeta as WireAssetMeta, Meta};
    use mev_hl_client::{Action as VenueAction, AssetMap};
    use mev_strategy::{CostModel, TimeInForce};
    use rust_decimal::Decimal;

    use super::*;
    use crate::orders::CloidAssigner;
    use crate::risk::RiskGate;
    use crate::state::EngineState;
    use crate::strategy::TimerId as StratTimerId;
    use crate::types::{AccountSnapshot, Level, VenueOrderStatus};

    fn ds(value: &str) -> Decimal {
        Decimal::from_str(value).unwrap()
    }

    fn registry() -> CoinRegistry {
        CoinRegistry::from_coins(&["BTC".into(), "ETH".into()])
    }

    fn asset_map() -> AssetMap {
        let mut map = AssetMap::new();
        map.insert_perp_dex(
            None,
            None,
            &Meta {
                universe: vec![
                    WireAssetMeta {
                        name: "BTC".into(),
                        sz_decimals: 0,
                        max_leverage: 40,
                        is_delisted: false,
                        only_isolated: false,
                    },
                    WireAssetMeta {
                        name: "ETH".into(),
                        sz_decimals: 0,
                        max_leverage: 40,
                        is_delisted: false,
                        only_isolated: false,
                    },
                ],
            },
        );
        map
    }

    fn table() -> AssetTable {
        AssetTable::from_markets(&registry(), &asset_map())
    }

    fn level(px: &str) -> Level {
        Level {
            px: ds(px),
            sz: Decimal::ONE,
            n: 1,
        }
    }

    fn state_with(bid: &str, ask: &str) -> EngineState {
        let mut state = EngineState::new(2);
        let slot = state.slot_mut(CoinId(0)).unwrap();
        slot.bbo = Some((
            level(bid),
            level(ask),
            Stamp {
                mono_ns: 1_000,
                ..Default::default()
            },
        ));
        state
    }

    fn stamp(mono_ns: u64) -> Stamp {
        Stamp {
            mono_ns,
            ..Default::default()
        }
    }

    fn intent(coin: &str, side: Side) -> OrderIntent {
        OrderIntent {
            strategy: StrategyId::from("test"),
            coin: coin.into(),
            side: if matches!(side, Side::Buy) {
                mev_strategy::Side::Buy
            } else {
                mev_strategy::Side::Sell
            },
            limit_px: Some(ds("100")),
            size: Decimal::ONE,
            tif: TimeInForce::Alo,
            reduce_only: false,
            rationale: "t".into(),
            cloid: None,
            signal_ms: 0,
            decision_ms: 0,
        }
    }

    fn live(cloid: Cloid, coin: u16, state: OrderState) -> LiveOrder {
        LiveOrder {
            cloid,
            coin: CoinId(coin),
            side: Side::Buy,
            px: ds("100"),
            sz: Decimal::ONE,
            filled_sz: Decimal::ZERO,
            reduce_only: false,
            strategy: StrategyId::from("other"),
            state,
            req_id: None,
            oid: None,
        }
    }

    fn cloid(n: u8) -> Cloid {
        let mut bytes = [0u8; 16];
        bytes[0] = n;
        Cloid(bytes)
    }

    /// A strategy that records the coins it is called for and can emit a script.
    struct Recording {
        id: StrategyId,
        coin: CoinId,
        calls: Arc<Mutex<Vec<CoinId>>>,
        events: Arc<Mutex<Vec<OrderEvent>>>,
        script: Vec<Action>,
        emit_on_market: bool,
    }

    impl Recording {
        fn new(id: &str, coin: CoinId) -> Self {
            Self {
                id: StrategyId::from(id),
                coin,
                calls: Arc::new(Mutex::new(Vec::new())),
                events: Arc::new(Mutex::new(Vec::new())),
                script: Vec::new(),
                emit_on_market: true,
            }
        }

        fn with_script(mut self, script: Vec<Action>) -> Self {
            self.script = script;
            self
        }

        fn calls(&self) -> Arc<Mutex<Vec<CoinId>>> {
            self.calls.clone()
        }

        fn events(&self) -> Arc<Mutex<Vec<OrderEvent>>> {
            self.events.clone()
        }
    }

    impl Strategy for Recording {
        fn id(&self) -> StrategyId {
            self.id.clone()
        }
        fn cost(&self) -> CostModel {
            CostModel::default()
        }
        fn interests(&self) -> Interests {
            Interests::coins([self.coin], Stream::Bbo)
        }
        fn on_market(&mut self, coin: CoinId, _ctx: &Ctx<'_>, out: &mut Actions) {
            self.calls.lock().unwrap().push(coin);
            if self.emit_on_market {
                for action in &self.script {
                    match action {
                        Action::Place(intent) => out.place(intent.clone()),
                        Action::Cancel { cloid } => out.cancel(*cloid),
                        Action::Modify { cloid, px, sz } => out.modify(*cloid, *px, *sz),
                        Action::PlaceGroup(group) => out.place_group(group.clone()),
                    }
                }
            }
        }
        fn on_order(&mut self, update: &OrderEvent, _ctx: &Ctx<'_>, _out: &mut Actions) {
            self.events.lock().unwrap().push(update.clone());
        }
    }

    struct CapturingExec {
        posts: Arc<Mutex<Vec<UnsignedPost>>>,
        full: bool,
    }

    impl CapturingExec {
        fn new(full: bool) -> Self {
            Self {
                posts: Arc::new(Mutex::new(Vec::new())),
                full,
            }
        }
        fn posts(&self) -> Arc<Mutex<Vec<UnsignedPost>>> {
            self.posts.clone()
        }
    }

    impl ExecBackend for CapturingExec {
        fn try_send(&mut self, post: UnsignedPost) -> bool {
            if self.full {
                return false;
            }
            self.posts.lock().unwrap().push(post);
            true
        }
    }

    struct Harness {
        dispatcher: StrategyDispatcher,
        posts: Arc<Mutex<Vec<UnsignedPost>>>,
    }

    fn harness(strategies: Vec<Box<dyn Strategy>>, full: bool) -> Harness {
        let exec = CapturingExec::new(full);
        let posts = exec.posts();
        let dispatcher = StrategyDispatcher::new(
            strategies,
            registry(),
            table(),
            RiskGate::default(),
            Some(Box::new(exec)),
            DispatcherConfig::default(),
        );
        Harness { dispatcher, posts }
    }

    #[test]
    fn dispatches_only_to_strategies_watching_the_coin() {
        let a = Recording::new("a", CoinId(0));
        let b = Recording::new("b", CoinId(1));
        let a_calls = a.calls();
        let b_calls = b.calls();
        let mut h = harness(vec![Box::new(a), Box::new(b)], false);

        let state = state_with("100", "101");
        h.dispatcher.on_coin_state(CoinId(0), stamp(10), &state);
        assert_eq!(*a_calls.lock().unwrap(), vec![CoinId(0)]);
        assert!(b_calls.lock().unwrap().is_empty());

        h.dispatcher.on_coin_state(CoinId(1), stamp(11), &state);
        assert_eq!(*b_calls.lock().unwrap(), vec![CoinId(1)]);
        assert_eq!(a_calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn places_build_one_bulk_order_and_cancel_first() {
        let resting = cloid(9);
        let strategy = Recording::new("test", CoinId(0)).with_script(vec![
            Action::Cancel { cloid: resting },
            Action::Place(intent("BTC", Side::Buy)),
            Action::Place(intent("BTC", Side::Sell)),
        ]);
        let mut h = harness(vec![Box::new(strategy)], false);
        h.dispatcher
            .orders
            .insert(live(resting, 0, OrderState::Resting));

        let state = state_with("100", "101");
        h.dispatcher.on_coin_state(CoinId(0), stamp(10), &state);

        let posts = h.posts.lock().unwrap();
        assert_eq!(posts.len(), 2, "cancel post then order post");
        match &posts[0].action {
            VenueAction::CancelByCloid { cancels } => assert_eq!(cancels.len(), 1),
            other => panic!("expected cancelByCloid first, got {other:?}"),
        }
        assert_eq!(posts[0].cloids.as_slice(), &[resting]);
        match &posts[1].action {
            VenueAction::Order { orders, .. } => assert_eq!(orders.len(), 2),
            other => panic!("expected bulk order second, got {other:?}"),
        }
        // The two places got distinct engine-assigned cloids and live orders.
        assert_eq!(posts[1].cloids.len(), 2);
        assert_ne!(posts[1].cloids[0], posts[1].cloids[1]);
        for c in &posts[1].cloids {
            let order = h.dispatcher.orders.get(*c).expect("live order inserted");
            assert_eq!(order.state, OrderState::PendingNew);
            assert_eq!(order.req_id, Some(posts[1].req_id));
        }
    }

    #[test]
    fn kill_switch_cancels_every_working_cloid_then_rejects_places() {
        let strategy = Recording::new("test", CoinId(0));
        let mut h = harness(vec![Box::new(strategy)], false);
        let a = cloid(1);
        let b = cloid(2);
        h.dispatcher.orders.insert(live(a, 0, OrderState::Resting));
        h.dispatcher
            .orders
            .insert(live(b, 0, OrderState::PartiallyFilled));

        let state = state_with("100", "101");
        h.dispatcher
            .on_account_state(&AccountUpdate::Control(Control::KillSwitch), &state);
        assert!(h.dispatcher.risk.kill().is_active());

        {
            let posts = h.posts.lock().unwrap();
            assert_eq!(posts.len(), 1);
            match &posts[0].action {
                VenueAction::CancelByCloid { cancels } => assert_eq!(cancels.len(), 2),
                other => panic!("expected cancelByCloid, got {other:?}"),
            }
        }

        // A new place is refused: buffered directly, then run through the gate.
        h.dispatcher.actions.place(intent("BTC", Side::Buy));
        h.dispatcher.run_actions(stamp(10), &state, 0, 0);
        assert_eq!(
            h.posts.lock().unwrap().len(),
            1,
            "no new order post after kill"
        );
    }

    #[test]
    fn order_update_and_fill_reach_the_owning_strategy() {
        let strategy = Recording::new("test", CoinId(0))
            .with_script(vec![Action::Place(intent("BTC", Side::Buy))]);
        let events = strategy.events();
        let mut h = harness(vec![Box::new(strategy)], false);
        let state = state_with("100", "101");
        h.dispatcher.on_coin_state(CoinId(0), stamp(10), &state);

        let placed = *h
            .dispatcher
            .orders
            .iter()
            .next()
            .map(|order| &order.cloid)
            .unwrap();

        h.dispatcher.on_account_state(
            &AccountUpdate::OrderUpdate {
                stamp: stamp(20),
                cloid: placed,
                oid: 7,
                status: VenueOrderStatus::Resting,
                filled_sz: Decimal::ZERO,
                avg_px: Decimal::ZERO,
            },
            &state,
        );
        h.dispatcher.on_account_state(
            &AccountUpdate::Fill {
                stamp: stamp(21),
                cloid: Some(placed),
                oid: 7,
                tid: 100,
                coin: CoinId(0),
                side: Side::Buy,
                px: ds("100"),
                sz: ds("0.5"),
                fee: ds("0.01"),
                liquidation: false,
            },
            &state,
        );

        let events = events.lock().unwrap();
        assert_eq!(events.len(), 2, "{events:?}");
        assert_eq!(
            events[0].kind,
            OrderEventKind::Status(VenueOrderStatus::Resting)
        );
        assert_eq!(events[1].kind, OrderEventKind::Fill);
        assert_eq!(
            h.dispatcher.account.position_szi(CoinId(0)),
            ds("0.5"),
            "perp fill updates the position"
        );
    }

    fn fill_update(tid: u64, cloid: Option<Cloid>, oid: u64, sz: &str) -> AccountUpdate {
        AccountUpdate::Fill {
            stamp: stamp(tid),
            cloid,
            oid,
            tid,
            coin: CoinId(0),
            side: Side::Buy,
            px: ds("100"),
            sz: ds(sz),
            fee: Decimal::ZERO,
            liquidation: false,
        }
    }

    fn snapshot_fills(fills: &[(u64, u64, &str)]) -> AccountUpdate {
        AccountUpdate::Fills {
            stamp: stamp(1),
            fills: fills
                .iter()
                .map(|(tid, oid, sz)| crate::types::FillData {
                    cloid: None,
                    oid: *oid,
                    tid: *tid,
                    coin: CoinId(0),
                    side: Side::Buy,
                    px: ds("100"),
                    sz: ds(sz),
                    fee: Decimal::ZERO,
                    liquidation: false,
                })
                .collect(),
        }
    }

    #[test]
    fn same_fill_delivered_twice_moves_the_position_once() {
        let mut h = harness(Vec::new(), false);
        let state = state_with("100", "101");
        h.dispatcher
            .on_account_state(&fill_update(5, None, 1, "1"), &state);
        // A reconnect snapshot redelivers the same tid: it must be skipped.
        h.dispatcher
            .on_account_state(&snapshot_fills(&[(5, 1, "1")]), &state);
        assert_eq!(
            h.dispatcher.account.position_szi(CoinId(0)),
            ds("1"),
            "a fill seen twice moves the position once"
        );
    }

    #[test]
    fn first_connect_snapshot_does_not_move_the_position() {
        let mut h = harness(Vec::new(), false);
        let state = state_with("100", "101");
        h.dispatcher
            .on_account_state(&snapshot_fills(&[(1, 1, "1"), (2, 2, "1")]), &state);
        assert_eq!(
            h.dispatcher.account.position_szi(CoinId(0)),
            Decimal::ZERO,
            "the first snapshot is already in the starting position"
        );
        // Its tids are recorded, so a live redelivery is skipped too.
        h.dispatcher
            .on_account_state(&fill_update(1, None, 1, "1"), &state);
        assert_eq!(h.dispatcher.account.position_szi(CoinId(0)), Decimal::ZERO);
    }

    #[test]
    fn reconnect_snapshot_applies_only_new_tids() {
        let mut h = harness(Vec::new(), false);
        let state = state_with("100", "101");
        // First snapshot: skipped, tids 1 and 2 recorded.
        h.dispatcher
            .on_account_state(&snapshot_fills(&[(1, 1, "1"), (2, 2, "2")]), &state);
        // A live fill ends the first-connect phase and moves the position.
        h.dispatcher
            .on_account_state(&fill_update(3, None, 3, "1"), &state);
        assert_eq!(h.dispatcher.account.position_szi(CoinId(0)), ds("1"));
        // Reconnect snapshot: 2 and 3 already seen; only the new tid 4 applies.
        h.dispatcher.on_account_state(
            &snapshot_fills(&[(2, 2, "2"), (3, 3, "1"), (4, 4, "1")]),
            &state,
        );
        assert_eq!(h.dispatcher.account.position_szi(CoinId(0)), ds("2"));
    }

    #[test]
    fn fill_maps_to_order_by_oid_and_updates_filled_sz() {
        let strategy = Recording::new("test", CoinId(0))
            .with_script(vec![Action::Place(intent("BTC", Side::Buy))]);
        let events = strategy.events();
        let mut h = harness(vec![Box::new(strategy)], false);
        let state = state_with("100", "101");
        h.dispatcher.on_coin_state(CoinId(0), stamp(10), &state);
        let placed = *h
            .dispatcher
            .orders
            .iter()
            .next()
            .map(|order| &order.cloid)
            .unwrap();

        h.dispatcher.on_account_state(
            &AccountUpdate::OrderUpdate {
                stamp: stamp(20),
                cloid: placed,
                oid: 7,
                status: VenueOrderStatus::Resting,
                filled_sz: Decimal::ZERO,
                avg_px: Decimal::ZERO,
            },
            &state,
        );
        // The fill carries no cloid: it is mapped through the oid index.
        h.dispatcher
            .on_account_state(&fill_update(9, None, 7, "0.5"), &state);

        let order = h.dispatcher.orders.get(placed).unwrap();
        assert_eq!(order.filled_sz, ds("0.5"));
        assert!(
            events
                .lock()
                .unwrap()
                .iter()
                .any(|event| event.kind == OrderEventKind::Fill),
            "the owning strategy receives the fill"
        );
    }

    #[test]
    fn fill_after_terminal_update_and_pruning_still_reaches_the_owner() {
        let strategy = Recording::new("test", CoinId(0))
            .with_script(vec![Action::Place(intent("BTC", Side::Buy))]);
        let events = strategy.events();
        let mut h = harness(vec![Box::new(strategy)], false);
        let state = state_with("100", "101");
        h.dispatcher.on_coin_state(CoinId(0), stamp(10), &state);
        let placed = *h
            .dispatcher
            .orders
            .iter()
            .next()
            .map(|order| &order.cloid)
            .unwrap();

        h.dispatcher.on_account_state(
            &AccountUpdate::OrderUpdate {
                stamp: stamp(20),
                cloid: placed,
                oid: 7,
                status: VenueOrderStatus::Filled,
                filled_sz: Decimal::ONE,
                avg_px: ds("100"),
            },
            &state,
        );
        assert!(h.dispatcher.orders.get(placed).unwrap().state.is_terminal());
        // The loop prunes the terminal order and remembers its oid route.
        h.dispatcher.record_iteration(0, 0);
        assert!(h.dispatcher.orders.get(placed).is_none());

        h.dispatcher
            .on_account_state(&fill_update(11, None, 7, "1"), &state);
        assert!(
            events
                .lock()
                .unwrap()
                .iter()
                .any(|event| event.kind == OrderEventKind::Fill),
            "a late fill is still delivered after pruning"
        );
        assert_eq!(h.dispatcher.account.position_szi(CoinId(0)), ds("1"));
    }

    #[test]
    fn post_ack_routes_statuses_to_the_right_cloids() {
        let strategy = Recording::new("test", CoinId(0)).with_script(vec![
            Action::Place(intent("BTC", Side::Buy)),
            Action::Place(intent("BTC", Side::Sell)),
        ]);
        let events = strategy.events();
        let mut h = harness(vec![Box::new(strategy)], false);
        let state = state_with("100", "101");
        h.dispatcher.on_coin_state(CoinId(0), stamp(10), &state);

        let (req_id, cloids) = {
            let post = h.posts.lock().unwrap().first().unwrap().clone();
            (post.req_id, post.cloids.clone())
        };
        h.dispatcher.on_account_state(
            &AccountUpdate::PostAck {
                stamp: stamp(30),
                req_id,
                result: PostResult::Statuses(smallvec::smallvec![
                    crate::types::OrderAck {
                        status: VenueOrderStatus::Resting,
                        oid: None,
                    },
                    crate::types::OrderAck {
                        status: VenueOrderStatus::Filled,
                        oid: None,
                    },
                ]),
            },
            &state,
        );
        assert_eq!(
            h.dispatcher.orders.get(cloids[0]).unwrap().state,
            OrderState::Resting
        );
        assert_eq!(
            h.dispatcher.orders.get(cloids[1]).unwrap().state,
            OrderState::Filled
        );
        assert_eq!(events.lock().unwrap().len(), 2);
    }

    #[test]
    fn post_ack_error_marks_orders_unknown_and_trips_the_breaker() {
        let strategy = Recording::new("test", CoinId(0))
            .with_script(vec![Action::Place(intent("BTC", Side::Buy))]);
        let mut h = harness(vec![Box::new(strategy)], false);
        let state = state_with("100", "101");
        h.dispatcher.on_coin_state(CoinId(0), stamp(10), &state);

        let (req_id, cloids) = {
            let post = h.posts.lock().unwrap().first().unwrap().clone();
            (post.req_id, post.cloids.clone())
        };
        h.dispatcher.on_account_state(
            &AccountUpdate::PostAck {
                stamp: stamp(30),
                req_id,
                result: PostResult::Error("boom".into()),
            },
            &state,
        );
        assert!(h.dispatcher.risk.breakers().is_tripped());
        assert_eq!(h.dispatcher.risk.breakers().label(), Some("exec_error"));
        // A lost reply is not a rejection: the order may be resting.
        assert_eq!(
            h.dispatcher.orders.get(cloids[0]).unwrap().state,
            OrderState::Unknown
        );
        // It still counts for the dead-man switch and exposure.
        assert_eq!(h.dispatcher.orders.resting_count(), 1);
    }

    #[test]
    fn resolving_an_unknown_order_clears_the_exec_breaker() {
        let strategy = Recording::new("test", CoinId(0))
            .with_script(vec![Action::Place(intent("BTC", Side::Buy))]);
        let mut h = harness(vec![Box::new(strategy)], false);
        let state = state_with("100", "101");
        h.dispatcher.on_coin_state(CoinId(0), stamp(10), &state);

        let (req_id, cloids) = {
            let post = h.posts.lock().unwrap().first().unwrap().clone();
            (post.req_id, post.cloids.clone())
        };
        h.dispatcher.on_account_state(
            &AccountUpdate::PostAck {
                stamp: stamp(30),
                req_id,
                result: PostResult::Error("boom".into()),
            },
            &state,
        );
        assert!(h.dispatcher.risk.breakers().is_tripped());

        // The exec layer resolves it by cloid (`orderStatus`): it was resting.
        h.dispatcher.on_account_state(
            &AccountUpdate::OrderUpdate {
                stamp: stamp(40),
                cloid: cloids[0],
                oid: 7,
                status: VenueOrderStatus::Resting,
                filled_sz: Decimal::ZERO,
                avg_px: Decimal::ZERO,
            },
            &state,
        );
        assert_eq!(
            h.dispatcher.orders.get(cloids[0]).unwrap().state,
            OrderState::Resting
        );
        assert!(!h.dispatcher.risk.breakers().is_tripped());
    }

    fn order_status_response(status: &str) -> mev_hl_client::OrderStatusResponse {
        mev_hl_client::OrderStatusResponse {
            status: "order".into(),
            order: Some(mev_hl_client::OrderStatusOrder {
                order: Some(mev_hl_client::OpenOrder {
                    coin: "BTC".into(),
                    oid: 7,
                    side: "B".into(),
                    limit_px: ds("100"),
                    sz: ds("1"),
                    orig_sz: ds("1"),
                    timestamp: 0,
                    cloid: None,
                    reduce_only: false,
                }),
                status: status.into(),
                status_timestamp: 0,
            }),
        }
    }

    #[test]
    fn rejected_post_ack_marks_orders_rejected_and_leaves_the_breaker_clear() {
        let strategy = Recording::new("test", CoinId(0))
            .with_script(vec![Action::Place(intent("BTC", Side::Buy))]);
        let mut h = harness(vec![Box::new(strategy)], false);
        let state = state_with("100", "101");
        h.dispatcher.on_coin_state(CoinId(0), stamp(10), &state);

        let (req_id, cloids) = {
            let post = h.posts.lock().unwrap().first().unwrap().clone();
            (post.req_id, post.cloids.clone())
        };
        h.dispatcher.on_account_state(
            &AccountUpdate::PostAck {
                stamp: stamp(30),
                req_id,
                result: PostResult::Rejected("rate limited".into()),
            },
            &state,
        );
        // A definitive failure is terminal, not Unknown.
        assert!(matches!(
            h.dispatcher.orders.get(cloids[0]).unwrap().state,
            OrderState::Rejected(_)
        ));
        assert!(!h.dispatcher.orders.has_unknown());
        assert!(!h.dispatcher.risk.breakers().is_tripped());
    }

    #[test]
    fn resolve_unknown_via_order_status_clears_the_breaker() {
        let strategy = Recording::new("test", CoinId(0))
            .with_script(vec![Action::Place(intent("BTC", Side::Buy))]);
        let mut h = harness(vec![Box::new(strategy)], false);
        let state = state_with("100", "101");
        h.dispatcher.on_coin_state(CoinId(0), stamp(10), &state);

        let (req_id, cloids) = {
            let post = h.posts.lock().unwrap().first().unwrap().clone();
            (post.req_id, post.cloids.clone())
        };
        h.dispatcher.on_account_state(
            &AccountUpdate::PostAck {
                stamp: stamp(30),
                req_id,
                result: PostResult::Error("lost reply".into()),
            },
            &state,
        );
        assert!(h.dispatcher.risk.breakers().is_tripped());

        h.dispatcher.on_account_state(
            &AccountUpdate::ResolveUnknown {
                stamp: stamp(40),
                cloid: cloids[0],
                status: order_status_response("open"),
            },
            &state,
        );
        assert_eq!(
            h.dispatcher.orders.get(cloids[0]).unwrap().state,
            OrderState::Resting
        );
        assert!(!h.dispatcher.risk.breakers().is_tripped());
    }

    #[test]
    fn unknown_expired_marks_rejected_and_clears_the_breaker() {
        let strategy = Recording::new("test", CoinId(0))
            .with_script(vec![Action::Place(intent("BTC", Side::Buy))]);
        let mut h = harness(vec![Box::new(strategy)], false);
        let state = state_with("100", "101");
        h.dispatcher.on_coin_state(CoinId(0), stamp(10), &state);

        let (req_id, cloids) = {
            let post = h.posts.lock().unwrap().first().unwrap().clone();
            (post.req_id, post.cloids.clone())
        };
        h.dispatcher.on_account_state(
            &AccountUpdate::PostAck {
                stamp: stamp(30),
                req_id,
                result: PostResult::Error("lost reply".into()),
            },
            &state,
        );
        assert!(h.dispatcher.orders.has_unknown());

        // The bounded `orderStatus` retries found nothing: terminal now.
        h.dispatcher.on_account_state(
            &AccountUpdate::UnknownExpired {
                stamp: stamp(50),
                cloid: cloids[0],
            },
            &state,
        );
        assert!(matches!(
            h.dispatcher.orders.get(cloids[0]).unwrap().state,
            OrderState::Rejected(_)
        ));
        assert!(!h.dispatcher.orders.has_unknown());
        assert!(!h.dispatcher.risk.breakers().is_tripped());
    }

    #[test]
    fn stale_order_status_after_a_stream_update_is_ignored() {
        let strategy = Recording::new("test", CoinId(0))
            .with_script(vec![Action::Place(intent("BTC", Side::Buy))]);
        let mut h = harness(vec![Box::new(strategy)], false);
        let state = state_with("100", "101");
        h.dispatcher.on_coin_state(CoinId(0), stamp(10), &state);
        let placed = *h
            .dispatcher
            .orders
            .iter()
            .next()
            .map(|order| &order.cloid)
            .unwrap();

        // A stream update already resolved the order to Resting.
        h.dispatcher.on_account_state(
            &AccountUpdate::OrderUpdate {
                stamp: stamp(20),
                cloid: placed,
                oid: 7,
                status: VenueOrderStatus::Resting,
                filled_sz: Decimal::ZERO,
                avg_px: Decimal::ZERO,
            },
            &state,
        );
        // A stale `orderStatus` answer saying it was canceled must not apply.
        h.dispatcher.on_account_state(
            &AccountUpdate::ResolveUnknown {
                stamp: stamp(30),
                cloid: placed,
                status: order_status_response("canceled"),
            },
            &state,
        );
        assert_eq!(
            h.dispatcher.orders.get(placed).unwrap().state,
            OrderState::Resting
        );
    }

    #[test]
    fn backpressure_fails_closed_and_trips_the_exec_breaker() {
        let strategy = Recording::new("test", CoinId(0))
            .with_script(vec![Action::Place(intent("BTC", Side::Buy))]);
        let mut h = harness(vec![Box::new(strategy)], true);
        let state = state_with("100", "101");
        h.dispatcher.on_coin_state(CoinId(0), stamp(10), &state);

        assert!(h.posts.lock().unwrap().is_empty(), "nothing was sent");
        assert_eq!(
            h.dispatcher.risk.breakers().label(),
            Some("exec_backpressure")
        );
        assert!(matches!(
            h.dispatcher.orders.iter().next().unwrap().state,
            OrderState::Rejected(_)
        ));
    }

    #[test]
    fn reduce_only_still_places_when_the_account_stream_is_halted() {
        let mut reduce = intent("BTC", Side::Sell);
        reduce.reduce_only = true;
        let strategy = Recording::new("test", CoinId(0)).with_script(vec![
            Action::Place(intent("BTC", Side::Buy)),
            Action::Place(reduce),
        ]);
        let mut h = harness(vec![Box::new(strategy)], false);
        h.dispatcher.on_gap();
        let state = state_with("100", "101");
        h.dispatcher.on_coin_state(CoinId(0), stamp(10), &state);

        let posts = h.posts.lock().unwrap();
        assert_eq!(posts.len(), 1);
        match &posts[0].action {
            VenueAction::Order { orders, .. } => {
                assert_eq!(
                    orders.len(),
                    1,
                    "the increase was dropped, reduce-only kept"
                );
            }
            other => panic!("expected order post, got {other:?}"),
        }
    }

    #[test]
    fn apply_reconcile_updates_margin_and_resumes_places() {
        let mut h = harness(Vec::new(), false);
        h.dispatcher.on_gap();
        assert!(h.dispatcher.stream.places_halted());
        h.dispatcher.apply_reconcile(
            &AccountSnapshot {
                account_value: ds("1000"),
                margin_used: ds("250"),
            },
            false,
            false,
        );
        assert_eq!(h.dispatcher.account.account_value, ds("1000"));
        assert_eq!(h.dispatcher.account.margin_used, ds("250"));
        assert!(!h.dispatcher.stream.places_halted());
    }

    #[test]
    fn paper_backend_fills_a_taker_and_updates_the_account() {
        use std::thread::sleep;
        use std::time::Duration;

        use mev_strategy::{AccountView, FeeRates, Instrument};

        use crate::paper_exec::{PaperConfig, PaperExec};

        // A Gtc buy above the ask takes liquidity once its latency elapses.
        let mut buy = intent("BTC", Side::Buy);
        buy.limit_px = Some(ds("101"));
        buy.tif = TimeInForce::Gtc;
        let strategy = Recording::new("test", CoinId(0)).with_script(vec![Action::Place(buy)]);
        let events = strategy.events();

        let mut instruments = BTreeMap::new();
        instruments.insert("BTC".to_string(), Instrument::perp());
        let paper = PaperExec::new(
            PaperConfig {
                latency_ms: 20,
                maker_fills: true,
            },
            AccountView {
                account_value: ds("1000"),
                ..Default::default()
            },
            instruments,
            FeeRates::PERP,
            FeeRates::SPOT,
        );
        let mut dispatcher = StrategyDispatcher::new(
            vec![Box::new(strategy)],
            registry(),
            table(),
            RiskGate::default(),
            None,
            DispatcherConfig::default(),
        )
        .with_paper(paper);

        let state = state_with("100", "100");
        dispatcher.on_coin_state(CoinId(0), stamp(10), &state);
        // The order is queued but not yet eligible.
        assert_eq!(dispatcher.account().position_szi(CoinId(0)), Decimal::ZERO);

        sleep(Duration::from_millis(25));
        dispatcher.on_coin_state(CoinId(0), stamp(30), &state);
        assert_eq!(
            dispatcher.account().position_szi(CoinId(0)),
            Decimal::ONE,
            "paper taker should fill and move the position"
        );
        assert!(
            events
                .lock()
                .unwrap()
                .iter()
                .any(|event| event.kind == OrderEventKind::Fill),
            "the owning strategy should receive the fill"
        );
    }

    #[test]
    fn timer_routes_to_the_registered_strategy() {
        let mut strategy = Recording::new("test", CoinId(0));
        let calls = strategy.calls();
        strategy.emit_on_market = false;
        let mut h = harness(vec![Box::new(strategy)], false);
        h.dispatcher
            .register_timer(HeapTimerId(3), 0, StratTimerId(7));
        let state = state_with("100", "101");
        h.dispatcher
            .on_timer_state(HeapTimerId(3), stamp(10), &state);
        // on_timer is a no-op for Recording, but routing must not panic and the
        // strategy must not be called via on_market.
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn interests_aggregate_to_route_form() {
        let a = Recording::new("a", CoinId(0));
        let b = Recording::new("b", CoinId(1));
        let h = harness(vec![Box::new(a), Box::new(b)], false);
        let interests = h.dispatcher.interests();
        assert_eq!(interests.len(), 2);
        assert_eq!(interests[0].coins, vec![CoinId(0)]);
        assert_eq!(interests[1].coins, vec![CoinId(1)]);
    }

    #[test]
    fn owner_map_survives_a_cancel_and_dropped_place() {
        // A place that the builder drops (min notional) leaves no live order.
        let mut tiny = intent("BTC", Side::Buy);
        tiny.size = ds("0.0001");
        let strategy = Recording::new("test", CoinId(0)).with_script(vec![Action::Place(tiny)]);
        let mut h = harness(vec![Box::new(strategy)], false);
        let state = state_with("100", "101");
        h.dispatcher.on_coin_state(CoinId(0), stamp(10), &state);
        assert!(h.posts.lock().unwrap().is_empty());
        assert!(
            h.dispatcher.orders.is_empty(),
            "dropped place left no order"
        );
    }

    #[test]
    fn assigner_cloids_are_unique_across_places() {
        let assigner = CloidAssigner::new();
        let first = assigner.next();
        let second = assigner.next();
        assert_ne!(first, second);
    }
}
