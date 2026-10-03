//! The engine thread and its per-iteration loop (SPEC-0010 §5, §9).
//!
//! The loop drains lossless account/control updates first, then all available
//! market updates (marking coins dirty), fires due timers, dispatches each
//! dirty coin once, and finally idles by spinning briefly and then blocking on
//! both channels with a timeout at the next timer deadline.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crossbeam_channel::{Receiver, select};

use crate::channels::{Inputs, MarketControl};
use crate::clock::{LiveClock, SharedClock};
use crate::dispatch::StrategyDispatcher;
use crate::instrument::{LatencyRecorder, Metric};
use crate::routes::Routes;
use crate::state::EngineState;
use crate::timers::{TimerHeap, TimerId};
use crate::types::{AccountUpdate, CoinId, ConnId, MarketUpdate, Stamp};

/// A pluggable consumer of dispatch events.
///
/// E-3 tests use this seam directly; E-4 supplies the concrete strategy
/// dispatch. Implementations must be synchronous and must not block.
pub trait Dispatcher {
    /// The routes to build from this dispatcher's interests.
    fn interests(&self) -> Vec<crate::routes::Interests>;
    /// Apply a market update to state; the dispatcher may inspect it before the
    /// loop marks `coin` dirty, but should not block.
    fn on_market(&mut self, _update: &MarketUpdate) {}
    /// A coin's state changed; decide and emit actions.
    fn on_coin(&mut self, _coin: CoinId, _stamp: Stamp) {}
    /// A timer fired.
    fn on_timer(&mut self, _id: TimerId, _stamp: Stamp) {}

    /// The loop registered a repeating timer for a strategy (SPEC-0010 §8).
    ///
    /// Called once at first iteration for each `Interests::timers_ms` entry, so
    /// a fired heap timer can be routed to the strategy-local timer id. The
    /// default is a no-op for dispatchers that do not use timers.
    fn on_timer_registered(
        &mut self,
        _heap: TimerId,
        _strategy_index: usize,
        _strategy_timer: crate::strategy::TimerId,
    ) {
    }
    /// A lossless account/control update arrived.
    fn on_account(&mut self, _update: &AccountUpdate) {}

    /// Like [`Dispatcher::on_coin`], but with the loop's read-only market state
    /// so a concrete dispatcher can build a strategy [`crate::strategy::Ctx`].
    ///
    /// The default forwards to [`Dispatcher::on_coin`], so the E-3 seam (and its
    /// test dispatchers) keep working unchanged.
    fn on_coin_state(&mut self, coin: CoinId, stamp: Stamp, _state: &EngineState) {
        self.on_coin(coin, stamp);
    }

    /// Like [`Dispatcher::on_timer`], with the loop's market state.
    fn on_timer_state(&mut self, id: TimerId, stamp: Stamp, _state: &EngineState) {
        self.on_timer(id, stamp);
    }

    /// Like [`Dispatcher::on_account`], with the loop's market state.
    fn on_account_state(&mut self, update: &AccountUpdate, _state: &EngineState) {
        self.on_account(update);
    }

    /// Record one loop iteration's wall time and drained-event count (E-10).
    ///
    /// Default is a no-op so simple dispatchers pay nothing.
    fn record_iteration(&mut self, _iteration_ns: u64, _events: usize) {}
}

/// Upper bound on how long `idle` blocks without a scheduled timer, so the
/// loop re-checks the stop signal promptly.
const MAX_BLOCK: Duration = Duration::from_millis(250);

/// First heap id reserved for the loop's repeating strategy timers, well above
/// any id a test or the driver schedules by hand.
const REPEAT_TIMER_BASE: u64 = 1 << 32;

/// The engine loop's runtime knobs.
#[derive(Debug, Clone)]
pub struct LoopConfig {
    /// Busy-spin window before blocking (0 disables spinning).
    pub spin_us: u64,
    /// Number of interned coins.
    pub coin_count: usize,
}

impl Default for LoopConfig {
    fn default() -> Self {
        Self {
            spin_us: 50,
            coin_count: 0,
        }
    }
}

/// Why the loop stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// Both input channels were disconnected (the process is shutting down).
    InputsClosed,
    /// The loop was asked to stop via [`EngineLoop::stop`].
    Requested,
}

/// The engine loop, generic over a [`Dispatcher`].
pub struct EngineLoop<D: Dispatcher> {
    inputs: Inputs,
    state: EngineState,
    timers: TimerHeap,
    routes: Routes,
    dispatcher: D,
    config: LoopConfig,
    stop: Receiver<()>,
    clock: SharedClock,
    /// Repeating timers declared via `Interests::timers_ms`:
    /// `(strategy index, period_ms)` in declaration order.
    strategy_timers: Vec<(usize, u64)>,
    /// Heap timer id → period in ns, for the repeating timers.
    repeating: BTreeMap<TimerId, u64>,
    /// Whether the repeating timers were seeded (done at the first iteration,
    /// once the clock is final).
    seeded: bool,
}

impl<D: Dispatcher> EngineLoop<D> {
    /// Build a loop from inputs, a dispatcher, and knobs.
    pub fn new(inputs: Inputs, dispatcher: D, config: LoopConfig, stop: Receiver<()>) -> Self {
        let interests = dispatcher.interests();
        let routes = Routes::build(config.coin_count, &interests);
        let strategy_timers: Vec<(usize, u64)> = interests
            .iter()
            .enumerate()
            .flat_map(|(index, interest)| {
                interest
                    .timers_ms
                    .iter()
                    .map(move |ms| (index, *ms))
                    .collect::<Vec<_>>()
            })
            .collect();
        let state = EngineState::new(config.coin_count);
        Self {
            inputs,
            state,
            timers: TimerHeap::new(),
            routes,
            dispatcher,
            config,
            stop,
            clock: Arc::new(LiveClock::new()),
            strategy_timers,
            repeating: BTreeMap::new(),
            seeded: false,
        }
    }

    /// Schedule every declared repeating timer at its first deadline and tell
    /// the dispatcher which strategy owns it (SPEC-0010 §8).
    ///
    /// Runs at the first iteration so the clock is final (the replay driver
    /// installs its clock before driving the loop).
    fn seed_timers(&mut self) {
        if self.seeded {
            return;
        }
        self.seeded = true;
        let base = self.clock.mono_ns();
        for (n, (strategy_index, period_ms)) in self.strategy_timers.iter().enumerate() {
            let period_ns = period_ms.saturating_mul(1_000_000);
            if period_ns == 0 {
                continue;
            }
            let heap_id = TimerId(REPEAT_TIMER_BASE + n as u64);
            let local = crate::strategy::TimerId(n as u32);
            self.timers
                .schedule(heap_id, base.saturating_add(period_ns));
            self.repeating.insert(heap_id, period_ns);
            self.dispatcher
                .on_timer_registered(heap_id, *strategy_index, local);
        }
    }

    /// Use an explicit clock (replay); defaults to [`LiveClock`].
    ///
    /// The caller must pass the *same* clock to the dispatcher for the paper
    /// backend and the loop to agree on event time.
    pub fn with_clock(mut self, clock: SharedClock) -> Self {
        self.clock = clock;
        self
    }

    /// The loop's clock.
    pub fn clock(&self) -> &SharedClock {
        &self.clock
    }

    /// Access the current market state (tests / introspection).
    pub fn state(&self) -> &EngineState {
        &self.state
    }

    /// The precomputed dispatch routes.
    pub fn routes(&self) -> &Routes {
        &self.routes
    }

    /// Schedule a timer.
    pub fn schedule(&mut self, id: TimerId, deadline_ns: u64) {
        self.timers.schedule(id, deadline_ns);
    }

    /// Run until the inputs close or a stop is requested.
    pub fn run(mut self) -> StopReason {
        loop {
            let processed = self.iterate(self.clock.mono_ns());
            if processed == 0 {
                match self.idle() {
                    StopReason::InputsClosed => return StopReason::InputsClosed,
                    StopReason::Requested => {}
                }
            }
            if self.stopped() {
                return StopReason::Requested;
            }
            if self.inputs_closed() {
                return StopReason::InputsClosed;
            }
        }
    }

    fn stopped(&self) -> bool {
        self.stop.try_recv().is_ok()
    }

    /// Run one pass of the §9 algorithm; returns the number of events drained.
    pub fn iterate(&mut self, now_mono_ns: u64) -> usize {
        self.seed_timers();
        let iteration_start = std::time::Instant::now();
        let mut n = 0;

        // 0. Lossless feed-control signals (gap opens) first, before any
        // account or market processing. `apply_account` runs an account event's
        // actions immediately (`dispatch.rs` `run_actions`), so a gap already
        // queued when that event lands must gate those actions in the same
        // iteration; draining it only after the account drain would let a fill
        // or post-ack place a non-reduce-only order on pre-gap state
        // (SPEC-0010 §23 Q-Gap-Edge (a)).
        if let Some(gap_mono_ns) = self.drain_gap_control() {
            self.state.mark_all_stale(gap_mono_ns);
        }

        // 1. Lossless account/control/exec updates first.
        while let Ok(update) = self.inputs.account.try_recv() {
            self.dispatcher.on_account_state(&update, &self.state);
            n += 1;
        }

        // 2. All available market updates; conflation marks dirty coins once.
        while let Ok(update) = self.inputs.market.try_recv() {
            self.apply_market(&update);
            n += 1;
        }

        // 2b. Drain control again. A gap the producer detects **during** this
        // iteration's account/market processing, or the mid-iteration
        // fail-closed latch, is not visible to the early drain; without this
        // second drain it would only gate the *next* iteration's dispatch, so
        // this iteration's dirty coins could be decided on pre-gap state. The
        // market drain above still runs before this so a pre-gap snapshot
        // already queued cannot restore a coin, and any order dispatched later
        // this iteration is gated by the stale flag. Re-draining is cheap on the
        // idle path: one empty `try_recv` plus one acquire load (the latch swap
        // only runs when the load sees true).
        if let Some(gap_mono_ns) = self.drain_gap_control() {
            self.state.mark_all_stale(gap_mono_ns);
        }

        // 3. Timers. The wall-clock half of the stamp comes from the clock, so
        // replay sees the recorded event time, not the host clock.
        let now_stamp = Stamp {
            t_recv_ns: self.clock.now().t_recv_ns,
            mono_ns: now_mono_ns,
            ts_exch_ms: 0,
        };
        for (id, deadline) in self.timers.pop_due_entries(now_mono_ns) {
            self.dispatcher.on_timer_state(id, now_stamp, &self.state);
            // Re-arm a repeating timer at the next multiple of its period after
            // now, so missed periods collapse into one firing (conflation) and
            // replay stays deterministic.
            if let Some(period) = self.repeating.get(&id).copied() {
                let mut next = deadline.saturating_add(period);
                while next <= now_mono_ns {
                    next = next.saturating_add(period);
                }
                self.timers.schedule(id, next);
            }
        }

        // 4. Dispatch each dirty coin once, in CoinId order, with the time of
        // the update that marked it dirty (SPEC-0010 §8).
        for coin in self.state.drain_dirty() {
            let stamp = self
                .state
                .slot(coin)
                .and_then(|slot| slot.latest_stamp())
                .unwrap_or(now_stamp);
            self.dispatcher.on_coin_state(coin, stamp, &self.state);
        }

        // 5. Loop-health instrumentation (E-10 §17).
        let iteration_ns = iteration_start.elapsed().as_nanos() as u64;
        self.dispatcher.record_iteration(iteration_ns, n);

        n
    }

    /// Drain the lossless feed-control channel and the fail-closed latch,
    /// returning the latest gap-detection time (monotonic ns) if any.
    ///
    /// The gap's timestamp is the moment the drop was **detected**, carried on
    /// the control message (`disconnect_ns`, on the shared frame clock) as
    /// [`hl_arb_client::raw_ws::mono_ns`], not the iteration start
    /// (`now_mono_ns`). The market drain can still be pulling pre-gap frames
    /// queued while the socket was alive, so an iteration start taken before the
    /// drain would predate them and let a pre-gap book clear staleness. The
    /// fail-closed latch carries no timestamp, so it falls back to the read
    /// time, which is never earlier than detection (fail closed). See
    /// [`crate::state::EngineState::mark_all_stale`].
    ///
    /// The latch is probed with an `Acquire` load first; the `swap` — a locked
    /// read-modify-write that invalidates the shared cache line — runs only when
    /// the load sees `true`. This is correct: the producer's `Release` store
    /// (`channels.rs` `signal_gap`) happens-before an acquire load that observes
    /// it, and the only place that clears the latch is this single engine
    /// thread, so a load that sees `true` implies the swap returns `true`. If
    /// the producer sets the latch concurrently after the load, the swap may
    /// miss it, but the latch stays set and the next iteration observes it: no
    /// signal is lost, and an idle iteration pays only a plain load.
    fn drain_gap_control(&mut self) -> Option<u64> {
        let mut gap_detect: Option<u64> = None;
        while let Ok(control) = self.inputs.control.try_recv() {
            match control {
                MarketControl::GapOpen { disconnect_ns } => {
                    gap_detect = Some(gap_detect.map_or(disconnect_ns, |g| g.max(disconnect_ns)));
                }
            }
        }
        if self.inputs.control_failed.load(Ordering::Acquire)
            && self.inputs.control_failed.swap(false, Ordering::AcqRel)
        {
            let drain_ns = self.clock.mono_ns();
            gap_detect = Some(gap_detect.map_or(drain_ns, |g| g.max(drain_ns)));
        }
        gap_detect
    }

    fn apply_market(&mut self, update: &MarketUpdate) {
        let stamp = match update {
            MarketUpdate::Bbo { stamp, .. }
            | MarketUpdate::Book { stamp, .. }
            | MarketUpdate::Trades { stamp, .. }
            | MarketUpdate::Ctx { stamp, .. }
            | MarketUpdate::Gap { stamp, .. } => *stamp,
        };
        match update {
            MarketUpdate::Bbo { coin, bid, ask, .. } => {
                if let Some(slot) = self.state.slot_mut(*coin) {
                    slot.bbo = Some((*bid, *ask, stamp));
                }
                // A bbo is a full snapshot of the top of book, but it is not a
                // full l2 book: it does not clear staleness (see
                // `EngineState::mark_fresh`). The strategies price off the l2
                // book, which would otherwise stay pre-gap.
            }
            MarketUpdate::Book { coin, book, .. } => {
                if let Some(slot) = self.state.slot_mut(*coin) {
                    slot.book = Some((*book, stamp));
                }
                // The only update that clears a coin's staleness (SPEC-0010 §16).
                self.state.mark_fresh(*coin, stamp.mono_ns);
            }
            // Trades and asset context are not book snapshots: they must not
            // clear staleness, or a coin whose l2 book is still pre-gap would
            // pass the risk gate (SPEC-0010 §16).
            MarketUpdate::Trades { .. } => {}
            MarketUpdate::Ctx { coin, ctx, .. } => {
                if let Some(slot) = self.state.slot_mut(*coin) {
                    slot.ctx = Some((*ctx, stamp));
                }
                // A `Ctx` is not a book snapshot, so it does not clear the
                // coin's book staleness. It clears only the separate ctx-stale
                // flag, and only when it is stamped strictly after the latest
                // gap (SPEC-0010 §23 Q-Gap-Edge (c)).
                self.state.mark_ctx_fresh(*coin, stamp.mono_ns);
            }
            // A gap on a shared connection cannot be attributed to one coin, so
            // mark every coin stale until a fresh book arrives (SPEC-0010 §16).
            MarketUpdate::Gap { open: true, .. } => self.state.mark_all_stale(stamp.mono_ns),
            MarketUpdate::Gap { open: false, .. } => {}
        }
        self.dispatcher.on_market(update);
        if let Some(coin) = update_coin(update) {
            self.state.mark_dirty(coin);
        }
    }

    /// Spin briefly, then block on both channels until data, the next timer, or
    /// a stop request. Returns [`StopReason::InputsClosed`] only when both
    /// inputs are gone.
    fn idle(&mut self) -> StopReason {
        // Busy-spin window: cheap polls before sleeping.
        let spin_deadline = std::time::Instant::now() + Duration::from_micros(self.config.spin_us);
        while std::time::Instant::now() < spin_deadline {
            if self.stopped() {
                return StopReason::Requested;
            }
            if !self.inputs.account.is_empty()
                || !self.inputs.market.is_empty()
                || !self.inputs.control.is_empty()
            {
                return StopReason::Requested; // something to do; caller loops
            }
            std::hint::spin_loop();
        }

        if self.inputs_closed() {
            return StopReason::InputsClosed;
        }

        // Block until either channel has data or the next timer is due. Cap the
        // block so the loop re-checks `stopped()` promptly even when no timer
        // is scheduled.
        let timeout = self
            .timers
            .next_deadline()
            .map(|deadline| {
                let now = self.clock.mono_ns();
                Duration::from_nanos(deadline.saturating_sub(now))
            })
            .unwrap_or(MAX_BLOCK)
            .min(MAX_BLOCK);

        select! {
            recv(self.inputs.account) -> _ => {}
            recv(self.inputs.market) -> _ => {}
            recv(self.inputs.control) -> _ => {}
            default(timeout) => {}
        }
        StopReason::Requested
    }

    fn inputs_closed(&self) -> bool {
        // A channel with no senders and no buffered messages reports
        // disconnected on a zero-length try_recv.
        let account_closed = matches!(
            self.inputs.account.try_recv(),
            Err(crossbeam_channel::TryRecvError::Disconnected)
        );
        let market_closed = matches!(
            self.inputs.market.try_recv(),
            Err(crossbeam_channel::TryRecvError::Disconnected)
        );
        account_closed && market_closed
    }

    /// The connection id a gap concerns, if any.
    pub fn gap_conn(update: &MarketUpdate) -> Option<ConnId> {
        match update {
            MarketUpdate::Gap { conn, .. } => Some(*conn),
            _ => None,
        }
    }
}

/// Convenience wiring for the concrete [`StrategyDispatcher`] (E-13).
///
/// `EngineLoop::new` already accepts any `D: Dispatcher`; these methods add the
/// sanctioned constructor name plus access to the dispatcher and its E-10
/// latency recorder for the 1 s metrics exporter.
impl EngineLoop<StrategyDispatcher> {
    /// Build the v2 engine loop around the concrete strategy dispatcher.
    pub fn with_dispatcher(
        inputs: Inputs,
        dispatcher: StrategyDispatcher,
        config: LoopConfig,
        stop: Receiver<()>,
    ) -> Self {
        Self::new(inputs, dispatcher, config, stop)
    }

    /// The concrete dispatcher (read-only).
    pub fn dispatcher(&self) -> &StrategyDispatcher {
        &self.dispatcher
    }

    /// The concrete dispatcher (mutable).
    pub fn dispatcher_mut(&mut self) -> &mut StrategyDispatcher {
        &mut self.dispatcher
    }

    /// The E-10 latency recorder.
    pub fn recorder(&self) -> &LatencyRecorder {
        self.dispatcher.recorder()
    }

    /// Mutable access to the E-10 latency recorder.
    pub fn recorder_mut(&mut self) -> &mut LatencyRecorder {
        self.dispatcher.recorder_mut()
    }

    /// Export the recorder's window and reset it (metrics exporter).
    pub fn flush(&mut self) -> Vec<(&'static str, Metric)> {
        self.dispatcher.flush()
    }

    /// A shared counter the market producer increments on a full channel.
    pub fn market_drop_counter(&self) -> Arc<AtomicU64> {
        self.dispatcher.market_drop_counter()
    }
}

fn update_coin(update: &MarketUpdate) -> Option<CoinId> {
    match update {
        MarketUpdate::Bbo { coin, .. }
        | MarketUpdate::Book { coin, .. }
        | MarketUpdate::Trades { coin, .. }
        | MarketUpdate::Ctx { coin, .. } => Some(*coin),
        MarketUpdate::Gap { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::{InputHandles, inputs};
    use crate::routes::Interests;
    use crate::types::{Level, MarketUpdate, Stamp};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct Record {
        coins: Vec<CoinId>,
        timers: Vec<TimerId>,
        accounts: usize,
    }

    struct Recorder {
        record: Arc<Mutex<Record>>,
        interests: Vec<Interests>,
    }

    impl Dispatcher for Recorder {
        fn interests(&self) -> Vec<Interests> {
            self.interests.clone()
        }
        fn on_coin(&mut self, coin: CoinId, _stamp: Stamp) {
            self.record.lock().unwrap().coins.push(coin);
        }
        fn on_timer(&mut self, id: TimerId, _stamp: Stamp) {
            self.record.lock().unwrap().timers.push(id);
        }
        fn on_account(&mut self, _update: &AccountUpdate) {
            self.record.lock().unwrap().accounts += 1;
        }
    }

    fn bbo(coin: u16, mono_ns: u64) -> MarketUpdate {
        MarketUpdate::Bbo {
            coin: CoinId(coin),
            stamp: Stamp {
                mono_ns,
                ..Default::default()
            },
            bid: Some(Level::default()),
            ask: Some(Level::default()),
        }
    }

    fn book_update(coin: u16, mono_ns: u64) -> MarketUpdate {
        MarketUpdate::Book {
            coin: CoinId(coin),
            stamp: Stamp {
                mono_ns,
                ..Default::default()
            },
            book: crate::types::BookSnapshot::default(),
        }
    }

    fn trades_update(coin: u16, mono_ns: u64) -> MarketUpdate {
        MarketUpdate::Trades {
            coin: CoinId(coin),
            stamp: Stamp {
                mono_ns,
                ..Default::default()
            },
            trades: smallvec::SmallVec::new(),
        }
    }

    fn ctx_update(coin: u16, mono_ns: u64) -> MarketUpdate {
        MarketUpdate::Ctx {
            coin: CoinId(coin),
            stamp: Stamp {
                mono_ns,
                ..Default::default()
            },
            ctx: crate::types::AssetCtxLite::default(),
        }
    }

    fn gap_update(conn: u16, mono_ns: u64) -> MarketUpdate {
        MarketUpdate::Gap {
            conn: ConnId(conn),
            stamp: Stamp {
                mono_ns,
                ..Default::default()
            },
            open: true,
        }
    }

    /// Whether the risk gate rejects a non-reduce-only place on `slot` as stale.
    ///
    /// Built with default limits: the stale check (#3) fires before any notional
    /// or margin check, so this isolates the staleness decision.
    fn risk_gate_rejects_non_reduce_only(slot: &crate::state::MarketSlot) -> bool {
        use rust_decimal::Decimal;

        use hl_arb_strategy::{OrderIntent, Side, StrategyId, TimeInForce};

        use crate::builder::AssetMeta;
        use crate::orders::OrderManager;
        use crate::risk::{RateBudget, RateBudgetConfig, RiskCtx, RiskGate, RiskReason};
        use crate::state::AccountState;
        use crate::strategy::Action;

        let orders = OrderManager::new(1);
        let account = AccountState::new(1);
        let rate = RateBudget::new(RateBudgetConfig::default());
        let meta = AssetMeta {
            asset_id: 0,
            sz_decimals: 0,
            is_spot: false,
            tick_size: None,
        };
        let ctx = RiskCtx {
            coin: CoinId(0),
            orders: &orders,
            account: &account,
            slot,
            meta: &meta,
            rate: &rate,
            now_ms: 0,
        };
        let intent = OrderIntent {
            strategy: StrategyId::from("t"),
            coin: "BTC".into(),
            side: Side::Buy,
            limit_px: Some(Decimal::from(100)),
            size: Decimal::ONE,
            tif: TimeInForce::Alo,
            reduce_only: false,
            rationale: "test".into(),
            cloid: None,
            signal_ms: 0,
            decision_ms: 0,
        };
        RiskGate::default().evaluate(&Action::Place(intent), &ctx) == Err(RiskReason::StaleCoin)
    }

    type TestLoop = (
        EngineLoop<Recorder>,
        InputHandles,
        crossbeam_channel::Sender<()>,
    );

    fn loop_with(record: Arc<Mutex<Record>>, coin_count: usize, spin_us: u64) -> TestLoop {
        let (handles, inputs) = inputs(64, 64);
        let (stop_tx, stop_rx) = crossbeam_channel::bounded(1);
        let dispatcher = Recorder {
            record,
            interests: vec![Interests::coins([CoinId(0), CoinId(1)])],
        };
        let config = LoopConfig {
            spin_us,
            coin_count,
        };
        (
            EngineLoop::new(inputs, dispatcher, config, stop_rx),
            handles,
            stop_tx,
        )
    }

    #[test]
    fn drains_account_before_market_and_conflates_coins() {
        let record = Arc::new(Mutex::new(Record::default()));
        let (mut engine, handles, _stop) = loop_with(record.clone(), 2, 0);

        // 5 updates for coin 1 and 1 for coin 0, plus one account update.
        for i in 0..5 {
            handles.send_market(bbo(1, i));
        }
        handles.send_market(bbo(0, 9));
        handles.send_account(AccountUpdate::Control(crate::types::Control::Resume));

        let processed = engine.iterate(1_000);
        assert_eq!(processed, 7);
        let record = record.lock().unwrap();
        // Account first (counted), then each coin dispatched once, in id order.
        assert_eq!(record.accounts, 1);
        assert_eq!(record.coins, vec![CoinId(0), CoinId(1)]);
    }

    #[test]
    fn gap_stays_stale_until_a_book_snapshot_and_only_a_book_clears_it() {
        let record = Arc::new(Mutex::new(Record::default()));
        let (mut engine, handles, _stop) = loop_with(record, 2, 0);

        // A gap via the lossless control signal marks every coin stale.
        handles.signal_gap(1_000);
        engine.iterate(1_000);
        assert!(engine.state().slot(CoinId(0)).unwrap().stale);
        assert!(engine.state().slot(CoinId(1)).unwrap().stale);

        // Trades, asset context and bbo are not l2 book snapshots and must not
        // clear staleness (a bbo would leave the l2 book pre-gap).
        handles.send_market(trades_update(1, 1_100));
        engine.iterate(1_100);
        assert!(engine.state().slot(CoinId(1)).unwrap().stale);
        handles.send_market(ctx_update(1, 1_200));
        engine.iterate(1_200);
        assert!(engine.state().slot(CoinId(1)).unwrap().stale);
        handles.send_market(bbo(1, 1_300));
        engine.iterate(1_300);
        assert!(engine.state().slot(CoinId(1)).unwrap().stale);

        // A book snapshot received after the gap clears only coin 1.
        handles.send_market(book_update(1, 1_400));
        engine.iterate(1_400);
        assert!(engine.state().slot(CoinId(0)).unwrap().stale);
        assert!(!engine.state().slot(CoinId(1)).unwrap().stale);
    }

    #[test]
    fn a_book_stamped_before_the_gap_cannot_clear_staleness() {
        let record = Arc::new(Mutex::new(Record::default()));
        let (mut engine, handles, _stop) = loop_with(record, 1, 0);

        // The gap is observed first, then a book that was received *before* the
        // gap drains late (the receive stamp, 500, predates the gap at 1000).
        // It must not clear the flag: its data is pre-gap.
        handles.signal_gap(1_000);
        engine.iterate(1_000);
        assert!(engine.state().slot(CoinId(0)).unwrap().stale);

        handles.send_market(book_update(0, 500));
        engine.iterate(1_001);
        assert!(engine.state().slot(CoinId(0)).unwrap().stale);

        // A book received after the gap does clear it.
        handles.send_market(book_update(0, 1_001));
        engine.iterate(1_002);
        assert!(!engine.state().slot(CoinId(0)).unwrap().stale);
    }

    #[test]
    fn a_book_received_between_iteration_start_and_the_drop_cannot_clear_staleness() {
        let record = Arc::new(Mutex::new(Record::default()));
        let (mut engine, handles, _stop) = loop_with(record, 1, 0);

        // The engine iteration starts at 500, but the drop was detected at
        // 1000. A pre-gap book (700) is still queued and delivered after the
        // gap signal: it is newer than the iteration start but older than the
        // drop, so it must not clear the flag.
        handles.signal_gap(1_000);
        engine.iterate(500);
        assert!(engine.state().slot(CoinId(0)).unwrap().stale);

        handles.send_market(book_update(0, 700));
        engine.iterate(700);
        assert!(engine.state().slot(CoinId(0)).unwrap().stale);

        // The risk gate keys on that flag, so a non-reduce-only order priced on
        // the pre-gap book is still rejected.
        let slot = engine.state().slot(CoinId(0)).unwrap();
        assert!(
            risk_gate_rejects_non_reduce_only(slot),
            "stale pre-gap book must keep non-reduce-only orders rejected"
        );
    }

    #[test]
    fn a_book_received_after_the_drop_clears_staleness() {
        let record = Arc::new(Mutex::new(Record::default()));
        let (mut engine, handles, _stop) = loop_with(record, 1, 0);

        handles.signal_gap(1_000);
        engine.iterate(500);
        assert!(engine.state().slot(CoinId(0)).unwrap().stale);

        // A book received strictly after the drop (1001) is post-gap and clears.
        handles.send_market(book_update(0, 1_001));
        engine.iterate(1_001);
        assert!(!engine.state().slot(CoinId(0)).unwrap().stale);
    }

    #[test]
    fn a_market_channel_gap_marks_stale_and_a_book_clears_it() {
        // The replay path delivers gaps on the market channel; it must keep
        // working. The replay producer is synchronous, so the gap is ordered
        // before the post-gap book.
        let record = Arc::new(Mutex::new(Record::default()));
        let (mut engine, handles, _stop) = loop_with(record, 1, 0);

        handles.send_market(gap_update(0, 500));
        engine.iterate(500);
        assert!(engine.state().slot(CoinId(0)).unwrap().stale);

        handles.send_market(book_update(0, 501));
        engine.iterate(501);
        assert!(!engine.state().slot(CoinId(0)).unwrap().stale);
    }

    #[test]
    fn a_gap_marks_stale_even_when_the_market_channel_is_full() {
        use crate::channels::{MarketSend, inputs};

        let record = Arc::new(Mutex::new(Record::default()));
        let (handles, inputs) = inputs(1, 4);
        let (_stop_tx, stop_rx) = crossbeam_channel::bounded(1);
        let dispatcher = Recorder {
            record,
            interests: vec![Interests::coins([CoinId(0)])],
        };
        let mut engine = EngineLoop::new(
            inputs,
            dispatcher,
            LoopConfig {
                spin_us: 0,
                coin_count: 1,
            },
            stop_rx,
        );

        // Saturate the tiny market queue so a real producer's next send drops.
        assert_eq!(handles.send_market(bbo(0, 1)), MarketSend::Queued);
        assert_eq!(handles.send_market(bbo(0, 2)), MarketSend::Dropped);
        // The gap still reaches the engine on the independent control channel.
        assert_eq!(
            handles.signal_gap(1_000),
            crate::channels::GapSignal::Queued
        );
        engine.iterate(1_000);
        assert!(engine.state().slot(CoinId(0)).unwrap().stale);
    }

    #[test]
    fn an_undeliverable_gap_signal_fails_closed_at_the_drain_time() {
        let record = Arc::new(Mutex::new(Record::default()));
        let (handles, inputs) = inputs(64, 64);
        let (_stop_tx, stop_rx) = crossbeam_channel::bounded(1);
        let dispatcher = Recorder {
            record,
            interests: vec![Interests::coins([CoinId(0), CoinId(1)])],
        };
        // The fail-closed latch has no timestamp, so the engine uses the drain
        // time. Pin the clock so the cut is deterministic: drain time 500.
        let clock = Arc::new(crate::clock::ReplayClock::new());
        clock.set_ms(0, 500);
        let mut engine = EngineLoop::new(
            inputs,
            dispatcher,
            LoopConfig {
                spin_us: 0,
                coin_count: 2,
            },
            stop_rx,
        )
        .with_clock(clock);

        // A producer that could not enqueue the control signal (the control
        // queue was full / the engine was unreachable) set the shared latch, so
        // the engine must treat every coin stale despite the later iteration
        // time (9_999).
        handles.force_control_failure();
        engine.iterate(9_999);
        assert!(engine.state().slot(CoinId(0)).unwrap().stale);
        assert!(engine.state().slot(CoinId(1)).unwrap().stale);

        // A book received before the drain time (400) stays stale.
        handles.send_market(book_update(0, 400));
        engine.iterate(400);
        assert!(engine.state().slot(CoinId(0)).unwrap().stale);

        // A book received after the drain time (700) clears it: the latch used
        // the drain time (500), not the iteration time (9_999).
        handles.send_market(book_update(0, 700));
        engine.iterate(700);
        assert!(!engine.state().slot(CoinId(0)).unwrap().stale);
    }

    /// Records, for every account update, whether the coin was already stale and
    /// whether the risk gate would reject a non-reduce-only place at that point.
    struct AccountGateProbe {
        coin: CoinId,
        seen: Arc<Mutex<Vec<(bool, bool)>>>,
    }

    impl Dispatcher for AccountGateProbe {
        fn interests(&self) -> Vec<Interests> {
            vec![Interests::coins([self.coin])]
        }
        fn on_account_state(&mut self, _update: &AccountUpdate, state: &EngineState) {
            let slot = state.slot(self.coin).expect("slot in range");
            self.seen
                .lock()
                .unwrap()
                .push((slot.stale, risk_gate_rejects_non_reduce_only(slot)));
        }
    }

    #[test]
    fn a_gap_queued_with_an_account_update_gates_that_iterations_order() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (handles, inputs) = inputs(64, 64);
        let (_stop_tx, stop_rx) = crossbeam_channel::bounded(1);
        let mut engine = EngineLoop::new(
            inputs,
            AccountGateProbe {
                coin: CoinId(0),
                seen: seen.clone(),
            },
            LoopConfig {
                spin_us: 0,
                coin_count: 1,
            },
            stop_rx,
        );

        // Both are queued before the iteration. Draining the gap only after the
        // account drain (as the E-9 code did) would gate the account-driven
        // action on pre-gap state; the early drain must mark the coin stale
        // first, so a non-reduce-only order is rejected in the same iteration.
        handles.send_account(AccountUpdate::Control(crate::types::Control::Resume));
        handles.signal_gap(1_000);
        engine.iterate(1_000);

        assert_eq!(
            *seen.lock().unwrap(),
            vec![(true, true)],
            "a non-reduce-only order from an account update must be rejected as stale"
        );
    }

    /// Signals a gap from inside market processing, so only the second
    /// (post-market) control drain can see it, then records whether the coin was
    /// stale when it was dispatched.
    struct MidIterationGap {
        handles: InputHandles,
        seen: Arc<Mutex<Vec<bool>>>,
        signaled: bool,
    }

    impl Dispatcher for MidIterationGap {
        fn interests(&self) -> Vec<Interests> {
            vec![Interests::coins([CoinId(0)])]
        }
        fn on_market(&mut self, _update: &MarketUpdate) {
            if !self.signaled {
                self.signaled = true;
                let _ = self.handles.signal_gap(1_000);
            }
        }
        fn on_coin_state(&mut self, _coin: CoinId, _stamp: Stamp, state: &EngineState) {
            self.seen
                .lock()
                .unwrap()
                .push(state.slot(CoinId(0)).unwrap().stale);
        }
    }

    #[test]
    fn a_gap_signaled_during_market_processing_gates_that_iterations_dispatch() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (handles, inputs) = inputs(64, 64);
        let (_stop_tx, stop_rx) = crossbeam_channel::bounded(1);
        let mut engine = EngineLoop::new(
            inputs,
            MidIterationGap {
                handles: handles.clone(),
                seen: seen.clone(),
                signaled: false,
            },
            LoopConfig {
                spin_us: 0,
                coin_count: 1,
            },
            stop_rx,
        );

        // A market frame arrives; the dispatcher's `on_market` signals a gap
        // while that frame is applied. The early drain ran before the market
        // drain, so only the post-market drain can gate this iteration's
        // dispatch.
        handles.send_market(bbo(0, 500));
        engine.iterate(500);

        assert_eq!(
            *seen.lock().unwrap(),
            vec![true],
            "a gap detected mid-iteration must still gate this iteration's dispatch"
        );
    }

    #[test]
    fn a_fresh_book_does_not_clear_a_stale_ctx_and_a_post_gap_ctx_does() {
        let record = Arc::new(Mutex::new(Record::default()));
        let (mut engine, handles, _stop) = loop_with(record, 1, 0);

        handles.signal_gap(1_000);
        engine.iterate(1_000);
        assert!(engine.state().slot(CoinId(0)).unwrap().stale);
        assert!(engine.state().slot(CoinId(0)).unwrap().ctx_stale);

        // A post-gap book clears book staleness but not the ctx's: the ctx may
        // still be pre-gap (SPEC-0010 §23 Q-Gap-Edge (c)).
        handles.send_market(book_update(0, 1_001));
        engine.iterate(1_001);
        assert!(!engine.state().slot(CoinId(0)).unwrap().stale);
        assert!(engine.state().slot(CoinId(0)).unwrap().ctx_stale);

        // A ctx stamped before the gap is ignored.
        handles.send_market(ctx_update(0, 900));
        engine.iterate(900);
        assert!(engine.state().slot(CoinId(0)).unwrap().ctx_stale);

        // Only a ctx stamped strictly after the gap clears it.
        handles.send_market(ctx_update(0, 1_002));
        engine.iterate(1_002);
        assert!(!engine.state().slot(CoinId(0)).unwrap().ctx_stale);
    }

    #[test]
    fn repeating_timers_fire_on_replay_time_and_rearm() {
        let record = Arc::new(Mutex::new(Record::default()));
        let (handles, inputs) = inputs(16, 16);
        let (_stop_tx, stop_rx) = crossbeam_channel::bounded(1);
        let interests = vec![Interests {
            timers_ms: vec![1_000],
            ..Interests::default()
        }];
        let dispatcher = Recorder {
            record: record.clone(),
            interests,
        };
        let clock = Arc::new(crate::clock::ReplayClock::new());
        clock.set_ms(1_000_000, 0);
        let mut engine = EngineLoop::new(
            inputs,
            dispatcher,
            LoopConfig {
                spin_us: 0,
                coin_count: 0,
            },
            stop_rx,
        )
        .with_clock(clock.clone());
        drop(handles);

        // First iteration seeds the timer at mono 0 + 1 s; nothing is due yet.
        engine.iterate(0);
        assert!(record.lock().unwrap().timers.is_empty());

        clock.set_ms(2_000_000, 1_000_000_000);
        engine.iterate(1_000_000_000);
        assert_eq!(
            record.lock().unwrap().timers,
            vec![TimerId(REPEAT_TIMER_BASE)]
        );

        // Not due again until the next period elapses.
        engine.iterate(1_500_000_000);
        assert_eq!(record.lock().unwrap().timers.len(), 1);

        clock.set_ms(3_000_000, 2_000_000_000);
        engine.iterate(2_000_000_000);
        assert_eq!(record.lock().unwrap().timers.len(), 2);
    }

    #[test]
    fn due_timers_fire_in_order() {
        let record = Arc::new(Mutex::new(Record::default()));
        let (mut engine, _handles, _stop) = loop_with(record.clone(), 2, 0);
        engine.schedule(TimerId(2), 100);
        engine.schedule(TimerId(0), 50);
        engine.schedule(TimerId(1), 100);

        engine.iterate(49); // nothing due
        assert!(record.lock().unwrap().timers.is_empty());
        engine.iterate(100);
        assert_eq!(
            record.lock().unwrap().timers,
            vec![TimerId(0), TimerId(1), TimerId(2)]
        );
    }

    #[test]
    fn routes_match_dispatcher_interests() {
        let record = Arc::new(Mutex::new(Record::default()));
        let (engine, _handles, _stop) = loop_with(record, 2, 0);
        assert_eq!(engine.routes().for_coin(CoinId(0)), &[0]);
        assert_eq!(engine.routes().for_coin(CoinId(1)), &[0]);
    }

    #[test]
    fn run_processes_then_stops_on_request() {
        let record = Arc::new(Mutex::new(Record::default()));
        let (engine, handles, stop) = loop_with(record.clone(), 2, 0);
        handles.send_market(bbo(0, 1));
        let reason = Arc::new(Mutex::new(None));
        let reason_clone = reason.clone();
        let thread = std::thread::spawn(move || {
            let stop_reason = engine.run();
            *reason_clone.lock().unwrap() = Some(stop_reason);
        });
        // Let the loop drain and block, then request a stop.
        std::thread::sleep(Duration::from_millis(20));
        stop.send(()).unwrap();
        thread.join().unwrap();
        assert_eq!(*reason.lock().unwrap(), Some(StopReason::Requested));
        assert_eq!(record.lock().unwrap().coins, vec![CoinId(0)]);
    }

    #[test]
    fn run_stops_when_inputs_close() {
        let record = Arc::new(Mutex::new(Record::default()));
        let (engine, handles, _stop) = loop_with(record, 2, 0);
        // Drop the only senders.
        drop(handles);
        assert_eq!(engine.run(), StopReason::InputsClosed);
    }

    struct StampRecorder {
        seen: Arc<Mutex<Vec<(CoinId, u64, i64)>>>,
        interests: Vec<Interests>,
    }

    impl Dispatcher for StampRecorder {
        fn interests(&self) -> Vec<Interests> {
            self.interests.clone()
        }
        fn on_coin(&mut self, coin: CoinId, stamp: Stamp) {
            self.seen
                .lock()
                .unwrap()
                .push((coin, stamp.mono_ns, stamp.t_recv_ns));
        }
    }

    #[test]
    fn dispatch_uses_the_coins_event_stamp_not_the_iteration_time() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (handles, inputs) = inputs(64, 64);
        let (_stop_tx, stop_rx) = crossbeam_channel::bounded(1);
        let dispatcher = StampRecorder {
            seen: seen.clone(),
            interests: vec![Interests::coins([CoinId(0)])],
        };
        let mut engine = EngineLoop::new(
            inputs,
            dispatcher,
            LoopConfig {
                spin_us: 0,
                coin_count: 1,
            },
            stop_rx,
        );
        handles.send_market(MarketUpdate::Bbo {
            coin: CoinId(0),
            stamp: Stamp {
                t_recv_ns: 123,
                mono_ns: 456,
                ts_exch_ms: 0,
            },
            bid: Some(Level::default()),
            ask: Some(Level::default()),
        });
        // The iteration's own time is deliberately different from the event's.
        engine.iterate(9_999);
        assert_eq!(
            *seen.lock().unwrap(),
            vec![(CoinId(0), 456, 123)],
            "ctx.now must be the update's stamp, so replay is event-time"
        );
    }

    #[test]
    fn idle_returns_when_data_is_pending() {
        let record = Arc::new(Mutex::new(Record::default()));
        let (mut engine, handles, _stop) = loop_with(record, 2, 50);
        handles.send_market(bbo(0, 1));
        // `idle` should short-circuit on the spinning check with work pending.
        let reason = engine.idle();
        assert_eq!(reason, StopReason::Requested);
        // And the loop then processes it.
        assert_eq!(engine.iterate(1), 1);
    }

    #[test]
    fn strategy_dispatcher_smoke_emits_a_market_maker_post() {
        use std::sync::Mutex;

        use hl_arb_client::types::{AssetMeta as WireAssetMeta, Meta};
        use hl_arb_client::{Action as VenueAction, AssetMap};
        use rust_decimal::Decimal;

        use crate::builder::AssetTable;
        use crate::dispatch::{DispatcherConfig, StrategyDispatcher};
        use crate::exec::{ExecBackend, UnsignedPost};
        use crate::risk::RiskGate;
        use crate::strategies::mm::{MarketMaker, MmConfig};
        use crate::types::{BOOK_DEPTH, BookSnapshot, CoinRegistry, Level};

        struct CapturingExec {
            posts: Arc<Mutex<Vec<UnsignedPost>>>,
            full: bool,
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

        fn book(px: &str) -> BookSnapshot {
            let mut book = BookSnapshot {
                bids: [Level::default(); BOOK_DEPTH],
                asks: [Level::default(); BOOK_DEPTH],
                n_bids: 1,
                n_asks: 1,
                time_ms: 0,
            };
            let level = Level {
                px: px.parse().unwrap(),
                sz: Decimal::ONE,
                n: 1,
            };
            book.bids[0] = level;
            book.asks[0] = level;
            book
        }

        let registry = CoinRegistry::from_coins(&["BTC".into()]);
        let mut map = AssetMap::new();
        map.insert_perp_dex(
            None,
            None,
            &Meta {
                universe: vec![WireAssetMeta {
                    name: "BTC".into(),
                    sz_decimals: 3,
                    max_leverage: 40,
                    is_delisted: false,
                    only_isolated: false,
                }],
            },
        );
        let table = AssetTable::from_markets(&registry, &map);

        let exec = CapturingExec {
            posts: Arc::new(Mutex::new(Vec::new())),
            full: false,
        };
        let posts = exec.posts.clone();

        let mm = MarketMaker::new(MmConfig {
            coin: CoinId(0),
            sz_decimals: 3,
            levels: 2,
            half_spread_bps: Decimal::from(5),
            level_step_bps: Decimal::from(5),
            size_per_level: Decimal::ONE,
            max_inventory: Decimal::from(10),
            max_skew_bps: Decimal::from(5),
            vol_pull_bps: Decimal::from(50),
            refresh_bps: Decimal::from(2),
        });
        let dispatcher = StrategyDispatcher::new(
            vec![Box::new(mm)],
            registry,
            table,
            RiskGate::default(),
            Some(Box::new(exec)),
            DispatcherConfig::default(),
        );

        let (handles, inputs) = inputs(16, 16);
        let (_stop_tx, stop_rx) = crossbeam_channel::bounded(1);
        let config = LoopConfig {
            spin_us: 0,
            coin_count: 1,
        };
        let mut engine = EngineLoop::with_dispatcher(inputs, dispatcher, config, stop_rx);

        handles.send_market(MarketUpdate::Book {
            coin: CoinId(0),
            stamp: Stamp {
                mono_ns: 1_000,
                ..Default::default()
            },
            book: book("100"),
        });
        assert_eq!(engine.iterate(1_000), 1);

        let posts = posts.lock().unwrap();
        assert_eq!(posts.len(), 1, "one bulk order post");
        match &posts[0].action {
            VenueAction::Order { orders, .. } => assert_eq!(orders.len(), 4),
            other => panic!("expected an order post, got {other:?}"),
        }
        // E-10 wiring: the iteration was recorded.
        assert!(engine.recorder().market_drops() == 0);
    }
}
