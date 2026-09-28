//! The engine thread and its per-iteration loop (SPEC-0010 §5, §9).
//!
//! The loop drains lossless account/control updates first, then all available
//! market updates (marking coins dirty), fires due timers, dispatches each
//! dirty coin once, and finally idles by spinning briefly and then blocking on
//! both channels with a timeout at the next timer deadline.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use crossbeam_channel::{Receiver, select};

use crate::channels::Inputs;
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
                // Fresh data proves the feed for this coin is live again.
                self.state.mark_fresh(*coin);
            }
            MarketUpdate::Book { coin, book, .. } => {
                if let Some(slot) = self.state.slot_mut(*coin) {
                    slot.book = Some((*book, stamp));
                }
                self.state.mark_fresh(*coin);
            }
            MarketUpdate::Trades { coin, .. } => self.state.mark_fresh(*coin),
            MarketUpdate::Ctx { coin, ctx, .. } => {
                if let Some(slot) = self.state.slot_mut(*coin) {
                    slot.ctx = Some((*ctx, stamp));
                }
                self.state.mark_fresh(*coin);
            }
            // A gap on a shared connection cannot be attributed to one coin, so
            // mark every coin stale until fresh data arrives (SPEC-0010 §16).
            MarketUpdate::Gap { open: true, .. } => self.state.mark_all_stale(),
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
            if !self.inputs.account.is_empty() || !self.inputs.market.is_empty() {
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
    fn gap_marks_coins_stale_until_fresh_data_arrives() {
        let record = Arc::new(Mutex::new(Record::default()));
        let (mut engine, handles, _stop) = loop_with(record, 2, 0);

        handles.send_market(MarketUpdate::Gap {
            conn: ConnId(0),
            stamp: Stamp::default(),
            open: true,
        });
        engine.iterate(1_000);
        assert!(engine.state().slot(CoinId(0)).unwrap().stale);
        assert!(engine.state().slot(CoinId(1)).unwrap().stale);

        // Fresh data for coin 1 clears only coin 1.
        handles.send_market(bbo(1, 1));
        engine.iterate(2_000);
        assert!(engine.state().slot(CoinId(0)).unwrap().stale);
        assert!(!engine.state().slot(CoinId(1)).unwrap().stale);
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

        use mev_hl_client::types::{AssetMeta as WireAssetMeta, Meta};
        use mev_hl_client::{Action as VenueAction, AssetMap};
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
