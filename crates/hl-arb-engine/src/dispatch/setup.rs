//! Dispatcher construction, accessors, and the operator control surface.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use hl_arb_strategy::StrategyId;

use crate::builder::AssetTable;
use crate::clock::{LiveClock, SharedClock};
use crate::exec::{ExecBackend, ReqIds};
use crate::instrument::{LatencyRecorder, Metric};
use crate::journal::ActionSink;
use crate::orders::{CloidAssigner, OrderManager};
use crate::paper_exec::PaperExec;
use crate::risk::RiskGate;
use crate::routes::{Interests as RouteInterests, Routes};
use crate::state::{AccountState, AccountStreamState};
use crate::strategy::{Actions, Interests, Strategy};
use crate::timers::TimerId as HeapTimerId;
use crate::types::CoinRegistry;

use super::*;

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
    pub(super) fn sync_market_drops(&mut self) {
        let total = self.market_drops.load(Ordering::Relaxed);
        let delta = total.saturating_sub(self.drops_seen);
        for _ in 0..delta {
            self.recorder.record_market_drop();
        }
        self.drops_seen = total;
    }
}
