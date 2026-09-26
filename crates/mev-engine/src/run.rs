//! The engine thread and its per-iteration loop (SPEC-0010 §5, §9).
//!
//! The loop drains lossless account/control updates first, then all available
//! market updates (marking coins dirty), fires due timers, dispatches each
//! dirty coin once, and finally idles by spinning briefly and then blocking on
//! both channels with a timeout at the next timer deadline.

use std::time::Duration;

use crossbeam_channel::{Receiver, select};
use mev_core::clock::{Clock, SystemClock};

use crate::channels::Inputs;
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
    /// A lossless account/control update arrived.
    fn on_account(&mut self, _update: &AccountUpdate) {}
}

/// Upper bound on how long `idle` blocks without a scheduled timer, so the
/// loop re-checks the stop signal promptly.
const MAX_BLOCK: Duration = Duration::from_millis(250);

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
}

impl<D: Dispatcher> EngineLoop<D> {
    /// Build a loop from inputs, a dispatcher, and knobs.
    pub fn new(inputs: Inputs, dispatcher: D, config: LoopConfig, stop: Receiver<()>) -> Self {
        let interests = dispatcher.interests();
        let routes = Routes::build(config.coin_count, &interests);
        let state = EngineState::new(config.coin_count);
        Self {
            inputs,
            state,
            timers: TimerHeap::new(),
            routes,
            dispatcher,
            config,
            stop,
        }
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
            let processed = self.iterate(SystemClock.now_ms() * 1_000_000);
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
        let mut n = 0;

        // 1. Lossless account/control/exec updates first.
        while let Ok(update) = self.inputs.account.try_recv() {
            self.dispatcher.on_account(&update);
            n += 1;
        }

        // 2. All available market updates; conflation marks dirty coins once.
        while let Ok(update) = self.inputs.market.try_recv() {
            self.apply_market(&update);
            n += 1;
        }

        // 3. Timers.
        let now_stamp = Stamp {
            mono_ns: now_mono_ns,
            ..Default::default()
        };
        for id in self.timers.pop_due(now_mono_ns) {
            self.dispatcher.on_timer(id, now_stamp);
        }

        // 4. Dispatch each dirty coin once, in CoinId order.
        for coin in self.state.drain_dirty() {
            self.dispatcher.on_coin(coin, now_stamp);
        }

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
            }
            MarketUpdate::Book { coin, book, .. } => {
                if let Some(slot) = self.state.slot_mut(*coin) {
                    slot.book = Some((*book, stamp));
                }
            }
            MarketUpdate::Trades { .. } => {}
            MarketUpdate::Ctx { coin, ctx, .. } => {
                if let Some(slot) = self.state.slot_mut(*coin) {
                    slot.ctx = Some((*ctx, stamp));
                }
            }
            MarketUpdate::Gap { .. } => {}
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
                let now = SystemClock.now_ms() * 1_000_000;
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
            bid: Level::default(),
            ask: Level::default(),
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
}
