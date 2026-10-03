//! Events and deterministic randomness (SPEC-0003 §8, §10).

use hl_arb_client::StreamEvent;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::id::StrategyId;
use crate::intent::Side;
use crate::view::AccountView;

/// An input the engine replays and feeds to strategies.
///
/// Serialized into the SQLite event log, so replay reconstructs the exact same
/// sequence without touching the network.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// A market-data event from the WebSocket stream.
    Market(StreamEvent),
    /// A snapshot of account state.
    Account(AccountView),
    /// A strategy timer fired.
    Timer {
        /// Timer period in milliseconds.
        every_ms: u64,
    },
    /// A fill (paper or real).
    Fill(FillEvent),
}

/// A fill delivered to strategies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FillEvent {
    /// Owning strategy, when known.
    pub strategy: Option<StrategyId>,
    /// Coin.
    pub coin: String,
    /// The side that rested/matched.
    pub side: Side,
    /// Fill price.
    pub px: Decimal,
    /// Fill size.
    pub sz: Decimal,
    /// Fee paid (positive is a cost).
    pub fee: Decimal,
    /// Whether the fill was a maker.
    pub maker: bool,
    /// Whether the order was reduce-only.
    pub reduce_only: bool,
}

/// A small deterministic PRNG (SplitMix64) for quote jitter and ids.
///
/// Strategies own one and seed it from config, so identical inputs replay to
/// identical outputs.
#[derive(Debug, Clone)]
pub struct DeterministicRng {
    state: u64,
}

impl DeterministicRng {
    /// Create a generator from a seed.
    pub fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// Next raw 64-bit value.
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value in `[0, 1)`.
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// A jitter offset in `[0, max)`, or zero when `max` is zero.
    pub fn jitter(&mut self, max: u64) -> u64 {
        if max == 0 { 0 } else { self.next_u64() % max }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rng_is_reproducible_for_a_seed() {
        let mut a = DeterministicRng::new(42);
        let mut b = DeterministicRng::new(42);
        for _ in 0..8 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn rng_differs_by_seed() {
        let mut a = DeterministicRng::new(1);
        let mut b = DeterministicRng::new(2);
        assert_ne!(a.next_u64(), b.next_u64());
    }

    #[test]
    fn f64_is_in_unit_interval() {
        let mut rng = DeterministicRng::new(7);
        for _ in 0..1000 {
            let value = rng.next_f64();
            assert!((0.0..1.0).contains(&value));
        }
    }

    #[test]
    fn jitter_bounds() {
        let mut rng = DeterministicRng::new(9);
        assert_eq!(rng.jitter(0), 0);
        for _ in 0..1000 {
            assert!(rng.jitter(50) < 50);
        }
    }

    #[test]
    fn event_round_trips_through_json() {
        let event = Event::Timer { every_ms: 1_000 };
        let json = serde_json::to_string(&event).unwrap();
        assert_eq!(json, r#"{"type":"timer","every_ms":1000}"#);
        let back: Event = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, Event::Timer { every_ms: 1_000 }));
    }
}
