//! The engine's time source (SPEC-0010 §13).
//!
//! The engine reads time only through [`EngineClock`], so the same code runs in
//! `live`, `simulate`, and deterministic `replay`:
//!
//! - [`LiveClock`] reads the system wall clock for wall time and the
//!   process-global monotonic clock shared with the raw socket
//!   ([`hl_arb_client::raw_ws::mono_ns`]) for its monotonic lane, so engine
//!   spans and received-frame stamps share one timeline.
//! - [`ReplayClock`] is advanced by the replay driver to the recorded event
//!   time, so paper fills, risk rate budgets, and latency stamps are all
//!   event-driven and a replay is reproducible.
//!
//! The loop's `iterate(now_mono_ns)` still takes the monotonic time explicitly;
//! the clock supplies the wall-clock half of the [`Stamp`] and the time for
//! `run()`'s own iterations.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use hl_arb_core::clock::{Clock, SystemClock};

use crate::types::Stamp;

/// A shared, thread-safe engine clock.
pub type SharedClock = Arc<dyn EngineClock>;

/// The engine's time source (SPEC-0010 §13).
pub trait EngineClock: Send + Sync + 'static {
    /// The current time as an event [`Stamp`].
    fn now(&self) -> Stamp;

    /// Monotonic nanoseconds on the engine's shared monotonic base.
    ///
    /// In live this is the process-global raw-socket clock
    /// ([`hl_arb_client::raw_ws::mono_ns`]); replay drives it from recorded
    /// stamps.
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
        hl_arb_client::raw_ws::mono_ns()
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
    fn live_clock_shares_the_raw_socket_monotonic_base() {
        let clock = LiveClock::new();
        assert!(clock.now_ms() > 0);
        let t_recv_ns = clock.now().t_recv_ns;
        assert!(t_recv_ns > 0);
        assert_eq!(t_recv_ns % 1_000_000, 0, "wall lane is millisecond-grained");

        // A frame stamped by the raw socket, then a later LiveClock reading,
        // must be on the same timeline: a small, positive delta (not the
        // ~1e18 ns wall-clock-vs-uptime skew the old millisecond base gave).
        let t_recv = hl_arb_client::raw_ws::mono_ns();
        let t_written = clock.mono_ns();
        let delta = t_written.saturating_sub(t_recv);
        assert!(t_written >= t_recv, "clock went backwards: {delta} ns");
        assert!(delta < 1_000_000_000, "delta not sub-second: {delta} ns");
    }

    #[test]
    fn live_clock_is_monotonic() {
        let clock = LiveClock::new();
        let first = clock.mono_ns();
        std::thread::sleep(std::time::Duration::from_millis(1));
        let second = clock.mono_ns();
        assert!(second > first, "monotonic clock did not advance");
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
