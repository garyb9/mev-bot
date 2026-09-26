//! Bounded channels into and out of the engine (SPEC-0010 §5).
//!
//! The engine thread reads two lossless account/control inputs and one lossy,
//! conflatable market input; it writes records and exec payloads over bounded
//! outbound channels, failing closed (never blocking) when they are full.

use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};

use crate::types::{AccountUpdate, MarketUpdate};

/// Default capacity of the market input channel.
pub const MARKET_CHANNEL_CAP: usize = 65_536;
/// Default capacity of the lossless account/control input channel.
pub const ACCOUNT_CHANNEL_CAP: usize = 16_384;

/// The engine's inbound channels (owned by the engine thread).
#[derive(Debug)]
pub struct Inputs {
    /// Lossless account/control/exec updates.
    pub account: Receiver<AccountUpdate>,
    /// Lossy, conflatable market updates.
    pub market: Receiver<MarketUpdate>,
}

/// Producer handles for the engine's inbound channels.
#[derive(Debug, Clone)]
pub struct InputHandles {
    /// Lossless account/control/exec updates.
    pub account: Sender<AccountUpdate>,
    /// Lossy market updates.
    pub market: Sender<MarketUpdate>,
}

/// Build a matched pair of input handles and readers.
pub fn inputs(market_cap: usize, account_cap: usize) -> (InputHandles, Inputs) {
    let (market_tx, market) = bounded(market_cap);
    let (account_tx, account) = bounded(account_cap);
    (
        InputHandles {
            account: account_tx,
            market: market_tx,
        },
        Inputs { account, market },
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
    use crate::types::{CoinId, Stamp};

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
    fn coin_ids_compile_in_tests() {
        // Keeps the import used and documents intent.
        assert_eq!(CoinId(0).index(), 0);
    }
}
