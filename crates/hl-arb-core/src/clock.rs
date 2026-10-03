//! Clock abstraction so time-dependent logic is testable and deterministic.
//!
//! Hyperliquid nonces are millisecond timestamps, so the system clock must be
//! injectable (see SPEC-0002). Tests use [`FixedClock`].

use std::time::{SystemTime, UNIX_EPOCH};

/// A source of "now" in milliseconds since the Unix epoch.
pub trait Clock: Send + Sync + 'static {
    /// Current time in milliseconds since the Unix epoch.
    fn now_ms(&self) -> u64;
}

/// Wall-clock implementation backed by [`SystemTime`].
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

/// Deterministic clock for tests.
#[derive(Debug, Clone)]
pub struct FixedClock {
    ms: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl FixedClock {
    /// Create a clock pinned to `ms`.
    pub fn new(ms: u64) -> Self {
        Self {
            ms: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(ms)),
        }
    }

    /// Advance the clock by `delta_ms`.
    pub fn advance(&self, delta_ms: u64) {
        self.ms
            .fetch_add(delta_ms, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Clock for FixedClock {
    fn now_ms(&self) -> u64 {
        self.ms.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_clock_advances() {
        let clock = FixedClock::new(1_000);
        assert_eq!(clock.now_ms(), 1_000);
        clock.advance(250);
        assert_eq!(clock.now_ms(), 1_250);
    }

    #[test]
    fn system_clock_is_positive() {
        assert!(SystemClock.now_ms() > 0);
    }
}
