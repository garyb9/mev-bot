//! Bounded channels into and out of the engine (SPEC-0010 §5).
//!
//! The engine thread reads two lossless account/control inputs and one lossy,
//! conflatable market input; it writes records and exec payloads over bounded
//! outbound channels, failing closed (never blocking) when they are full.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};

use crate::types::{AccountUpdate, MarketUpdate};

/// Default capacity of the market input channel.
pub const MARKET_CHANNEL_CAP: usize = 65_536;
/// Default capacity of the lossless account/control input channel.
pub const ACCOUNT_CHANNEL_CAP: usize = 16_384;
/// Capacity of the market feed-control channel (gap signals).
///
/// Small on purpose: gap opens coalesce (a pending gap already means stale), and
/// a full queue means the engine is not draining. In that case the producer
/// sets the shared fail-closed latch instead of dropping the signal silently.
pub const CONTROL_CHANNEL_CAP: usize = 8;

/// A market-feed control signal delivered on its own channel.
///
/// Unlike [`MarketUpdate`]s, control signals must not be lost to a full market
/// queue: a dropped gap would leave a feed that we know is broken looking fresh
/// exactly when the system is most loaded (SPEC-0010 §16).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarketControl {
    /// A feed gap opened: every coin is stale until a fresh book snapshot.
    GapOpen {
        /// Process-monotonic nanoseconds when the drop was detected, on the
        /// same clock the market frames are stamped with
        /// ([`hl_arb_client::raw_ws::mono_ns`]). This is the cut a book
        /// snapshot must be strictly newer than to clear staleness; see
        /// [`crate::state::EngineState::mark_all_stale`].
        disconnect_ns: u64,
    },
}

/// Outcome of [`InputHandles::signal_gap`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GapSignal {
    /// The gap was queued on the control channel.
    Queued,
    /// The control channel was full or disconnected, so the shared fail-closed
    /// latch was set: the engine still treats every coin stale.
    FailClosed,
}

/// The engine's inbound channels (owned by the engine thread).
#[derive(Debug)]
pub struct Inputs {
    /// Lossless account/control/exec updates.
    pub account: Receiver<AccountUpdate>,
    /// Lossy, conflatable market updates.
    pub market: Receiver<MarketUpdate>,
    /// Market feed-control signals (gap opens), drained after the market queue.
    pub control: Receiver<MarketControl>,
    /// Set when a producer could not enqueue a control signal. The engine must
    /// treat every coin stale while it is set (fail closed).
    pub control_failed: Arc<AtomicBool>,
}

/// Producer handles for the engine's inbound channels.
#[derive(Debug, Clone)]
pub struct InputHandles {
    /// Lossless account/control/exec updates.
    pub account: Sender<AccountUpdate>,
    /// Lossy market updates.
    pub market: Sender<MarketUpdate>,
    /// Market feed-control signals (gap opens); never silently dropped.
    control: Sender<MarketControl>,
    /// Shared fail-closed latch, mirrored into [`Inputs`].
    control_failed: Arc<AtomicBool>,
}

/// Build a matched pair of input handles and readers.
pub fn inputs(market_cap: usize, account_cap: usize) -> (InputHandles, Inputs) {
    let (market_tx, market) = bounded(market_cap);
    let (account_tx, account) = bounded(account_cap);
    let (control_tx, control) = bounded(CONTROL_CHANNEL_CAP);
    let control_failed = Arc::new(AtomicBool::new(false));
    (
        InputHandles {
            account: account_tx,
            market: market_tx,
            control: control_tx,
            control_failed: control_failed.clone(),
        },
        Inputs {
            account,
            market,
            control,
            control_failed,
        },
    )
}

/// Why a market send was not accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarketSend {
    /// The update was queued.
    Queued,
    /// The channel was full; the update was dropped (the producer should count
    /// it and rely on the next snapshot to repair state).
    Dropped,
}

impl InputHandles {
    /// Send a market update, dropping it (never blocking) when the queue is
    /// full. The caller counts drops per coin (SPEC-0010 §5).
    pub fn send_market(&self, update: MarketUpdate) -> MarketSend {
        match self.market.try_send(update) {
            Ok(()) => MarketSend::Queued,
            Err(TrySendError::Full(_)) => MarketSend::Dropped,
            // A disconnected engine is a fatal state the caller handles.
            Err(TrySendError::Disconnected(_)) => MarketSend::Dropped,
        }
    }

    /// Send an account/control update, returning `false` if the engine is gone.
    ///
    /// This channel is lossless by contract; callers should treat `false` as a
    /// fatal shutdown condition rather than dropping updates silently.
    pub fn send_account(&self, update: AccountUpdate) -> bool {
        self.account.send(update).is_ok()
    }

    /// Signal that a market feed gap opened. Never silent.
    ///
    /// The signal goes on the dedicated control channel, carrying
    /// `disconnect_ns` (the process-monotonic time the drop was detected, from
    /// the same clock that stamps market frames) so the engine can cut off
    /// pre-gap books. If the control queue is full (the engine is behind) or
    /// the engine is gone, the shared fail-closed latch is set instead, so the
    /// engine still treats every coin stale. The caller must not treat
    /// [`GapSignal::FailClosed`] as "nothing to do".
    pub fn signal_gap(&self, disconnect_ns: u64) -> GapSignal {
        match self
            .control
            .try_send(MarketControl::GapOpen { disconnect_ns })
        {
            Ok(()) => GapSignal::Queued,
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                self.control_failed.store(true, Ordering::Release);
                GapSignal::FailClosed
            }
        }
    }

    /// Test seam: set the fail-closed latch as an undeliverable producer would.
    #[cfg(test)]
    pub(crate) fn force_control_failure(&self) {
        self.control_failed.store(true, Ordering::Release);
    }
}

/// An outbound sink the engine pushes to without blocking.
#[derive(Debug, Clone)]
pub struct Outbound<T> {
    tx: Sender<T>,
}

impl<T> Outbound<T> {
    /// Wrap a sender.
    pub fn new(tx: Sender<T>) -> Self {
        Self { tx }
    }

    /// Try to enqueue `value`; `false` means full or disconnected (the engine
    /// must fail closed, not block).
    pub fn try_send(&self, value: T) -> bool {
        self.tx.try_send(value).is_ok()
    }
}

/// Build a bounded outbound channel.
pub fn outbound<T>(cap: usize) -> (Outbound<T>, Receiver<T>) {
    let (tx, rx) = bounded(cap);
    (Outbound::new(tx), rx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Stamp;

    fn bbo() -> MarketUpdate {
        MarketUpdate::Gap {
            conn: crate::types::ConnId(0),
            stamp: Stamp::default(),
            open: true,
        }
    }

    #[test]
    fn market_send_drops_when_full() {
        let (handles, inputs) = inputs(1, 1);
        assert_eq!(handles.send_market(bbo()), MarketSend::Queued);
        assert_eq!(handles.send_market(bbo()), MarketSend::Dropped);
        assert!(inputs.market.try_recv().is_ok());
    }

    #[test]
    fn account_send_reports_disconnect() {
        let (handles, inputs) = inputs(1, 1);
        drop(inputs.account);
        assert!(!handles.send_account(AccountUpdate::Control(crate::types::Control::KillSwitch)));
    }

    #[test]
    fn outbound_fails_closed_when_full() {
        let (out, rx) = outbound::<u64>(1);
        assert!(out.try_send(1));
        assert!(!out.try_send(2));
        drop(rx);
        assert!(!out.try_send(3));
    }

    #[test]
    fn gap_signal_is_not_dropped_when_the_market_channel_is_full() {
        // A gap is independent of the market queue: even with the market
        // channel saturated, the control signal reaches the engine.
        let (handles, inputs) = inputs(1, 1);
        assert_eq!(handles.send_market(bbo()), MarketSend::Queued);
        assert_eq!(handles.send_market(bbo()), MarketSend::Dropped);
        assert_eq!(handles.signal_gap(1234), GapSignal::Queued);
        assert_eq!(
            inputs.control.try_recv(),
            Ok(MarketControl::GapOpen {
                disconnect_ns: 1234
            })
        );
        assert!(!inputs.control_failed.load(Ordering::Acquire));
    }

    #[test]
    fn a_full_control_channel_sets_the_fail_closed_latch() {
        let (handles, inputs) = inputs(1, 1);
        for _ in 0..CONTROL_CHANNEL_CAP {
            assert_eq!(handles.signal_gap(1), GapSignal::Queued);
        }
        // The next signal cannot be queued: the latch makes the engine treat
        // every coin stale instead of pretending the feed is fine.
        assert_eq!(handles.signal_gap(1), GapSignal::FailClosed);
        assert!(inputs.control_failed.load(Ordering::Acquire));
    }
}
