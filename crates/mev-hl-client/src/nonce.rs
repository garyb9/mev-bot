//! Nonce generation with self-healing (SPEC-0002 §5).
//!
//! HyperCore nonces are client-supplied millisecond timestamps that must be
//! strictly increasing per agent wallet and within a recent window; the venue
//! validates but never assigns them. Because only monotonicity matters (not
//! contiguity), the failure modes are narrow and recoverable:
//!
//! - **Too far in the future** — corruption or a backward clock step pushed
//!   `last` beyond the venue's acceptance window. Such a value cannot have been
//!   accepted, so it is safe to reclaim and resync to the wall clock ([`RESET`]).
//! - **Behind / duplicate (stale, recent-window)** — the venue already saw a
//!   higher nonce. [`NonceManager::on_reject`] resyncs to the clock and advances.
//!
//! The caller persists [`NonceManager::last`] before sending (crash-safe) and
//! restores it on boot with [`NonceManager::restore`].

use std::time::{SystemTime, UNIX_EPOCH};

/// Default maximum a nonce may run ahead of the wall clock. Beyond this the
/// value is treated as corrupt and reclaimed. Conservative versus the venue's
/// window; tune with [`NonceManager::with_max_future_drift`].
pub const DEFAULT_MAX_FUTURE_DRIFT_MS: u64 = 60_000;

/// Why a nonce was reset, for metrics/logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetReason {
    /// `last` had run implausibly far ahead of the clock.
    FutureDrift,
    /// The venue rejected a nonce as stale/duplicate/recent-window.
    Rejected,
    /// An operator forced a resync via the escape hatch.
    Manual,
}

impl ResetReason {
    /// Stable label for metrics.
    pub const fn as_str(self) -> &'static str {
        match self {
            ResetReason::FutureDrift => "future_drift",
            ResetReason::Rejected => "rejected",
            ResetReason::Manual => "manual",
        }
    }
}

/// Current wall-clock time in milliseconds.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Monotonic nonce source with self-healing.
#[derive(Debug, Clone)]
pub struct NonceManager {
    last: u64,
    max_future_drift_ms: u64,
    resets: u64,
}

impl Default for NonceManager {
    fn default() -> Self {
        Self::new()
    }
}

impl NonceManager {
    /// A fresh manager with no history.
    pub fn new() -> Self {
        Self {
            last: 0,
            max_future_drift_ms: DEFAULT_MAX_FUTURE_DRIFT_MS,
            resets: 0,
        }
    }

    /// Restore the persisted high-water mark after a restart.
    pub fn restore(last: u64) -> Self {
        Self {
            last,
            ..Self::new()
        }
    }

    /// Override the future-drift guard.
    pub fn with_max_future_drift(mut self, ms: u64) -> Self {
        self.max_future_drift_ms = ms;
        self
    }

    /// The highest nonce handed out (persist this before sending).
    pub fn last(&self) -> u64 {
        self.last
    }

    /// Number of self-heal resets so far.
    pub fn resets(&self) -> u64 {
        self.resets
    }

    /// Reserve the next nonce: strictly greater than the last, never behind the
    /// clock, and never more than the drift guard ahead of it.
    pub fn next(&mut self, now_ms: u64) -> u64 {
        let candidate = self.last.saturating_add(1).max(now_ms);
        if candidate > now_ms.saturating_add(self.max_future_drift_ms) {
            // `last` is implausibly far ahead; the venue cannot have accepted it.
            return self.reclaim(now_ms, ResetReason::FutureDrift);
        }
        self.last = candidate;
        candidate
    }

    /// Resync after a stale/duplicate/recent-window rejection and return the
    /// nonce to retry with.
    pub fn on_reject(&mut self, now_ms: u64) -> u64 {
        self.last = self.last.max(now_ms);
        let candidate = self.last.saturating_add(1);
        self.resets += 1;
        self.last = candidate;
        candidate
    }

    /// Operator escape hatch: abandon history and resume from the wall clock.
    pub fn reset(&mut self, now_ms: u64) -> u64 {
        self.reclaim(now_ms, ResetReason::Manual)
    }

    fn reclaim(&mut self, now_ms: u64, _reason: ResetReason) -> u64 {
        let candidate = now_ms.saturating_add(1);
        self.last = candidate;
        self.resets += 1;
        candidate
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monotonic_within_same_millisecond() {
        let mut nonce = NonceManager::new();
        let a = nonce.next(1_000);
        let b = nonce.next(1_000);
        let c = nonce.next(1_000);
        assert_eq!((a, b, c), (1_000, 1_001, 1_002));
    }

    #[test]
    fn follows_wall_clock_when_it_advances() {
        let mut nonce = NonceManager::new();
        assert_eq!(nonce.next(1_000), 1_000);
        assert_eq!(nonce.next(5_000), 5_000);
        assert_eq!(nonce.last(), 5_000);
    }

    #[test]
    fn clock_regression_never_reuses() {
        let mut nonce = NonceManager::new();
        let a = nonce.next(10_000);
        let b = nonce.next(9_000); // clock stepped back
        assert!(b > a, "{b} must exceed {a}");
    }

    #[test]
    fn far_future_last_self_heals() {
        // Simulate corruption: `last` is a day ahead of the clock.
        let mut nonce = NonceManager::restore(86_400_000).with_max_future_drift(60_000);
        let healed = nonce.next(1_000_000);
        assert_eq!(healed, 1_000_001);
        assert_eq!(nonce.resets(), 1);
    }

    #[test]
    fn reject_resyncs_and_advances() {
        let mut nonce = NonceManager::restore(1_000);
        // Venue rejects: our nonce was behind its last-seen. Resync to now.
        let retry = nonce.on_reject(9_000);
        assert_eq!(retry, 9_001);
        assert!(retry > nonce.last() - 1);
    }

    #[test]
    fn manual_reset_resumes_from_clock() {
        let mut nonce = NonceManager::restore(u64::MAX - 1);
        let resumed = nonce.reset(500);
        assert_eq!(resumed, 501);
    }
}
