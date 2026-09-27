//! The engine's time source (SPEC-0010 §13).
//!
//! The engine reads time only through [`EngineClock`], so the same code runs in
//! `live`, `simulate`, and deterministic `replay`:
//!
//! - [`LiveClock`] reads the system wall clock and a millisecond-resolution
//!   monotonic base (the loop's timer base).
//! - [`ReplayClock`] is advanced by the replay driver to the recorded event
//!   time, so paper fills, risk rate budgets, and latency stamps are all
//!   event-driven and a replay is reproducible.
//!
//! The loop's `iterate(now_mono_ns)` still takes the monotonic time explicitly;
//! the clock supplies the wall-clock half of the [`Stamp`] and the time for
//! `run()`'s own iterations.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use mev_core::clock::{Clock, SystemClock};

use crate::types::Stamp;

/// A shared, thread-safe engine clock.
pub type SharedClock = Arc<dyn EngineClock>;

/// The engine's time source (SPEC-0010 §13).
pub trait EngineClock: Send + Sync + 'static {
    /// The current time as an event [`Stamp`].
    fn now(&self) -> Stamp;

    /// Monotonic nanoseconds on the loop's timer base.
    fn mono_ns(&self) -> u64;

    /// Wall-clock milliseconds since the Unix epoch.
    fn now_ms(&self) -> u64;
}

/// Wall-clock time for `live`/`simulate`.
#[derive(Debug, Default, Clone, Copy)]
pub struct LiveClock;

impl LiveClock {
    /// Build a live clock.
    pub fn new() -> Self {
        Self
    }
}

impl EngineClock for LiveClock {
    fn now(&self) -> Stamp {
        Stamp {
            t_recv_ns: self.now_ms() as i64 * 1_000_000,
            mono_ns: self.mono_ns(),
            ts_exch_ms: 0,
        }
    }

    fn mono_ns(&self) -> u64 {
        SystemClock.now_ms().saturating_mul(1_000_000)
    }

    fn now_ms(&self) -> u64 {
        SystemClock.now_ms()
    }
}

/// Event time supplied by the replay driver (SPEC-0010 §13/§14).
///
/// The driver sets it to the envelope's `t_ns`/`mono_ns` before each iteration,
/// so every decision, paper fill, and latency stamp uses recorded time.
#[derive(Debug, Default)]
pub struct ReplayClock {
    t_ns: AtomicI64,
    mono_ns: AtomicU64,
}

impl ReplayClock {
    /// Build a clock at time zero; the driver advances it per event.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the clock to an event [`Stamp`].
    pub fn set(&self, stamp: Stamp) {
        self.t_ns.store(stamp.t_recv_ns, Ordering::SeqCst);
        self.mono_ns.store(stamp.mono_ns, Ordering::SeqCst);
    }

    /// Set the wall and monotonic times directly.
    pub fn set_ms(&self, t_ns: i64, mono_ns: u64) {
        self.t_ns.store(t_ns, Ordering::SeqCst);
        self.mono_ns.store(mono_ns, Ordering::SeqCst);
    }
}

impl EngineClock for ReplayClock {
    fn now(&self) -> Stamp {
        Stamp {
            t_recv_ns: self.t_ns.load(Ordering::SeqCst),
            mono_ns: self.mono_ns.load(Ordering::SeqCst),
            ts_exch_ms: 0,
        }
    }

    fn mono_ns(&self) -> u64 {
        self.mono_ns.load(Ordering::SeqCst)
    }

    fn now_ms(&self) -> u64 {
        (self.t_ns.load(Ordering::SeqCst).max(0) as u64) / 1_000_000
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_clock_is_monotonic_and_consistent() {
        let clock = LiveClock::new();
        assert!(clock.now_ms() > 0);
        assert_eq!(clock.now().t_recv_ns, clock.now_ms() as i64 * 1_000_000);
        assert_eq!(clock.mono_ns(), clock.now_ms().saturating_mul(1_000_000));
    }

    #[test]
    fn replay_clock_reports_the_driven_event_time() {
        let clock = ReplayClock::new();
        assert_eq!(clock.now_ms(), 0);
        clock.set(Stamp {
            t_recv_ns: 1_700_000_000_123_000_000,
            mono_ns: 42,
            ts_exch_ms: 0,
        });
        assert_eq!(clock.now_ms(), 1_700_000_000_123);
        assert_eq!(clock.now().mono_ns, 42);
        clock.set_ms(0, 7);
        assert_eq!(clock.now_ms(), 0);
        assert_eq!(clock.mono_ns(), 7);
    }
}
