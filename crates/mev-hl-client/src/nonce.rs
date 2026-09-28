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
//! Durability is write-behind ([`NonceLease`]): a high-water mark a *lease*
//! ahead of what is sent is persisted off the hot path, at most once per second,
//! and the manager resumes from it on boot with [`NonceManager::resume`].
//! [`NonceManager::restore`] is the synchronous, untrusted restore kept for
//! callers that persist the exact sent value.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
#[cfg(test)]
use std::sync::mpsc::SyncSender;
use std::time::{SystemTime, UNIX_EPOCH};

use mev_core::db::writer::{DbWriter, WriteCmd};
use mev_core::error::{Error, Result};

/// Default maximum a nonce may run ahead of the wall clock. Beyond this the
/// value is treated as corrupt and reclaimed. Conservative versus the venue's
/// window; tune with [`NonceManager::with_max_future_drift`].
pub const DEFAULT_MAX_FUTURE_DRIFT_MS: u64 = 60_000;

/// Default look-ahead (in nonce space) persisted ahead of the last sent nonce
/// by [`NonceLease`] (SPEC-0002 H-6). A 30 s reserve is far larger than the
/// burst-ahead-of-clock drift of any realistic agent.
pub const DEFAULT_NONCE_LEASE_MS: u64 = 30_000;

/// Maximum write-behind persist rate for the nonce high-water mark: at most one
/// enqueue per second once a fresh horizon is due (SPEC-0002 H-6). Urgent (low
/// headroom) and forced refreshes may exceed this.
pub const NONCE_WRITE_INTERVAL_MS: u64 = 1_000;

/// Retry interval after a failed or unconfirmed persist, so a dropped enqueue
/// or a failed SQLite write is retried well before the lease is spent
/// (SPEC-0002 H-6 review).
pub const NONCE_RETRY_INTERVAL_MS: u64 = 100;

/// Minimum interval between rate-limited `warn` logs for dropped persists.
pub const NONCE_WARN_INTERVAL_MS: u64 = 1_000;

/// Bounded queue for coalesced nonce persists. Tiny: a `Copy` value, written at
/// most once per second.
pub const NONCE_WRITE_QUEUE: usize = 64;

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
    /// Highest value known to have been legitimate (restored from the durable
    /// lease). The future-drift guard measures against `max(now, floor)` so a
    /// trusted write-behind restore is never mistaken for corruption.
    floor: u64,
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
            floor: 0,
            max_future_drift_ms: DEFAULT_MAX_FUTURE_DRIFT_MS,
            resets: 0,
        }
    }

    /// Restore the persisted high-water mark after a restart.
    ///
    /// The value is treated as untrusted: the future-drift guard may reclaim it
    /// if it is implausibly ahead of the clock.
    pub fn restore(last: u64) -> Self {
        Self {
            last,
            ..Self::new()
        }
    }

    /// Resume from a trusted write-behind high-water mark (SPEC-0002 H-6).
    ///
    /// Unlike [`Self::restore`], the restored value also becomes the drift
    /// guard's floor: the persisted lease is intentionally ahead of the clock
    /// and must not be reclaimed as if it were corrupt.
    pub fn resume(hwm: u64) -> Self {
        Self {
            last: hwm,
            floor: hwm,
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
        let baseline = now_ms.max(self.floor);
        if candidate > baseline.saturating_add(self.max_future_drift_ms) {
            // `last` is implausibly far ahead; the venue cannot have accepted it.
            return self.reclaim(baseline, ResetReason::FutureDrift);
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
        self.floor = now_ms;
        self.reclaim(now_ms, ResetReason::Manual)
    }

    fn reclaim(&mut self, baseline: u64, _reason: ResetReason) -> u64 {
        let candidate = baseline.saturating_add(1);
        self.last = candidate;
        self.resets += 1;
        candidate
    }
}

/// Where a nonce horizon is written behind (SPEC-0002 H-6).
enum NonceSink {
    /// Production: the SQLite write-behind actor.
    Db(DbWriter),
    /// Tests: a channel standing in for the writer, so an enqueue can be
    /// observed (or made to fail) without touching SQLite.
    #[cfg(test)]
    Channel(SyncSender<u64>),
}

impl NonceSink {
    /// Enqueue `nonce` without blocking; `false` when the queue is full/closed.
    fn enqueue(&self, nonce: u64) -> bool {
        match self {
            NonceSink::Db(writer) => writer.try_send(WriteCmd::Nonce { nonce }),
            #[cfg(test)]
            NonceSink::Channel(tx) => tx.try_send(nonce).is_ok(),
        }
    }
}

/// Write-behind nonce durability via a [`DbWriter`] (SPEC-0002 H-6).
///
/// The manager hands out nonces off the wall clock; this controller persists a
/// high-water mark a *lease* ahead of what is sent, coalesced to at most one
/// enqueue per [`NONCE_WRITE_INTERVAL_MS`] (urgent and forced refreshes may
/// exceed that). Because the persisted value always covers every sent nonce, a
/// crash that loses the write-behind window cannot reuse a nonce: on restart the
/// bot resumes from the last confirmed durable value.
///
/// The durable marker is a shared atomic published by the writer thread *after*
/// the write is committed. `cover` refuses a send when the reserved nonce is not
/// covered, so a stalled writer fails closed instead of reusing a nonce; a
/// dropped enqueue or failed write is retried on [`NONCE_RETRY_INTERVAL_MS`].
pub struct NonceLease {
    lease: u64,
    durable: Arc<AtomicU64>,
    /// Last requested persisted horizon (monotonic).
    requested: AtomicU64,
    /// Time of the last enqueue attempt.
    last_attempt_ms: AtomicU64,
    /// The last attempt failed or a requested write is unconfirmed: retry on the
    /// short interval rather than the coalescing interval.
    enqueue_failed: AtomicBool,
    last_warn_ms: AtomicU64,
    sink: NonceSink,
    /// Cached metric handles (no per-order label or formatting work).
    refusals: metrics::Counter,
    dropped: metrics::Counter,
}

impl NonceLease {
    /// Wrap `writer`, publishing `horizon` as the initial durable marker.
    ///
    /// The caller must have already persisted `horizon` (the startup prime).
    pub fn new(
        writer: DbWriter,
        horizon: u64,
        lease: u64,
        durable: Arc<AtomicU64>,
        now_ms: u64,
    ) -> Self {
        Self::with_sink(NonceSink::Db(writer), horizon, lease, durable, now_ms)
    }

    /// Test constructor over a channel sink, returning the shared durable mark
    /// so a test can simulate the writer confirming (or never confirming).
    #[cfg(test)]
    pub(crate) fn with_channel(
        sink: SyncSender<u64>,
        horizon: u64,
        lease: u64,
        durable: Arc<AtomicU64>,
        now_ms: u64,
    ) -> Self {
        Self::with_sink(NonceSink::Channel(sink), horizon, lease, durable, now_ms)
    }

    fn with_sink(
        sink: NonceSink,
        horizon: u64,
        lease: u64,
        durable: Arc<AtomicU64>,
        now_ms: u64,
    ) -> Self {
        durable.store(horizon, Ordering::Release);
        Self {
            lease,
            durable,
            requested: AtomicU64::new(horizon),
            last_attempt_ms: AtomicU64::new(now_ms),
            enqueue_failed: AtomicBool::new(false),
            last_warn_ms: AtomicU64::new(0),
            sink,
            refusals: metrics::counter!(mev_metrics::names::NONCE_LEASE_REFUSALS_TOTAL),
            dropped: metrics::counter!(mev_metrics::names::NONCE_PERSIST_DROPPED_TOTAL),
        }
    }

    /// The last requested persisted horizon.
    pub fn requested(&self) -> u64 {
        self.requested.load(Ordering::Acquire)
    }

    /// The confirmed durable high-water mark.
    pub fn durable(&self) -> u64 {
        self.durable.load(Ordering::Acquire)
    }

    /// Ensure `nonce` is covered by a durable high-water mark, refreshing
    /// write-behind when the confirmed mark is half-spent.
    ///
    /// Returns an error (without sending) when durability cannot be guaranteed.
    pub fn cover(&self, nonce: u64, now_ms: u64) -> Result<()> {
        let durable = self.durable.load(Ordering::Acquire);
        // Trigger on the *confirmed* durable mark (not just the requested
        // horizon), so a dropped enqueue or failed write is retried.
        if nonce.saturating_add(self.lease / 2) >= durable {
            self.maybe_request(nonce, now_ms, durable);
        }
        if nonce > self.durable.load(Ordering::Acquire) {
            self.refusals.increment(1);
            return Err(Error::NotSent(
                "nonce not covered by the durable high-water mark; refusing to send".to_string(),
            ));
        }
        Ok(())
    }

    /// Force a write-ahead horizon now (e.g. after a reject resync).
    pub fn force(&self, nonce: u64, now_ms: u64) {
        self.request(nonce, now_ms);
    }

    fn maybe_request(&self, nonce: u64, now_ms: u64, durable: u64) {
        // Coalesce to at most one write per interval, unless durability is at
        // risk and we must extend the horizon immediately.
        let urgent = nonce.saturating_add(self.lease / 4) >= durable;
        if !urgent {
            let pending = self.enqueue_failed.load(Ordering::Acquire)
                || self.requested.load(Ordering::Acquire) > durable;
            let interval = if pending {
                NONCE_RETRY_INTERVAL_MS
            } else {
                NONCE_WRITE_INTERVAL_MS
            };
            let last = self.last_attempt_ms.load(Ordering::Relaxed);
            if now_ms < last.saturating_add(interval) {
                return;
            }
        }
        self.request(nonce, now_ms);
    }

    fn request(&self, nonce: u64, now_ms: u64) {
        let target = nonce.saturating_add(self.lease);
        // Monotonic: never enqueue a smaller horizon than one already requested.
        let mut prev = self.requested.load(Ordering::Relaxed);
        loop {
            if target <= prev {
                return;
            }
            match self.requested.compare_exchange_weak(
                prev,
                target,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(current) => prev = current,
            }
        }
        self.last_attempt_ms.store(now_ms, Ordering::Relaxed);
        // A `Copy` value on a bounded channel; never blocks, never allocates.
        if self.sink.enqueue(target) {
            self.enqueue_failed.store(false, Ordering::Release);
            return;
        }
        // Roll the horizon back so a later attempt can retry it; if another
        // thread already advanced past us, leave its higher value in place.
        let _ = self
            .requested
            .compare_exchange(target, prev, Ordering::AcqRel, Ordering::Relaxed);
        self.enqueue_failed.store(true, Ordering::Release);
        self.dropped.increment(1);
        // Rate-limited: a persistent failure must not log per order.
        let last = self.last_warn_ms.load(Ordering::Relaxed);
        if now_ms >= last.saturating_add(NONCE_WARN_INTERVAL_MS) {
            self.last_warn_ms.store(now_ms, Ordering::Relaxed);
            tracing::warn!(
                horizon = target,
                "nonce write-behind enqueue dropped; durability retry scheduled"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_metrics::counter_value;
    use metrics_util::debugging::DebuggingRecorder;
    use mev_metrics::names;

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

    #[test]
    fn resume_trusts_the_lease_ahead_of_the_clock() {
        // The persisted lease is intentionally ahead; the guard must not reclaim
        // it as corruption.
        let mut nonce = NonceManager::resume(30_000).with_max_future_drift(60_000);
        assert_eq!(nonce.next(1_000), 30_001);
        assert_eq!(nonce.resets(), 0);
    }

    #[test]
    fn resume_follows_the_clock_once_it_passes() {
        let mut nonce = NonceManager::resume(30_000).with_max_future_drift(60_000);
        assert_eq!(nonce.next(40_000), 40_000);
    }

    #[test]
    fn lease_extends_the_horizon_write_behind() {
        let db = Arc::new(std::sync::Mutex::new(
            mev_core::db::Db::open_in_memory().unwrap(),
        ));
        let writer = DbWriter::spawn_shared(db, NONCE_WRITE_QUEUE);
        let durable = writer.nonce_hwm();
        let lease = NonceLease::new(writer, 1_000, 100, durable, 0);
        assert_eq!(lease.requested(), 1_000);
        // Within the prime: covered, no new write requested.
        assert!(lease.cover(900, 2_000).is_ok());
        assert_eq!(lease.requested(), 1_000);
        // Crossing the half-lease extends the requested horizon.
        assert!(lease.cover(950, 2_000).is_ok());
        assert_eq!(lease.requested(), 1_050);
        // Still covered by the confirmed prime.
        assert!(lease.cover(1_000, 2_000).is_ok());
    }

    #[test]
    fn lease_retries_after_a_dropped_enqueue() {
        let durable = Arc::new(AtomicU64::new(0));
        // Capacity 1 with no consumer: the first refresh is buffered (queue
        // full), the retry succeeds once the test drains it.
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let lease = NonceLease::with_channel(tx, 10_000, 1_000, durable, 0);

        // First refresh: buffered.
        assert!(lease.cover(9_500, 2_000).is_ok());
        assert_eq!(lease.requested(), 10_500);

        // Second refresh on a full queue: dropped, requested rolled back.
        assert!(lease.cover(9_600, 2_100).is_ok());
        assert_eq!(lease.requested(), 10_500);

        // The short retry interval then re-enqueues the same horizon.
        assert_eq!(rx.try_recv().unwrap(), 10_500);
        assert!(lease.cover(9_600, 2_200).is_ok());
        assert_eq!(lease.requested(), 10_600);
        assert_eq!(rx.try_recv().unwrap(), 10_600);
    }

    #[test]
    fn lease_refuses_and_counts_when_the_writer_is_gone() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let refused = metrics::with_local_recorder(&recorder, || {
            let durable = Arc::new(AtomicU64::new(0));
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            drop(rx); // writer gone: every enqueue fails
            let lease = NonceLease::with_channel(tx, 1_000, 100, durable.clone(), 0);
            // A crossing tries to persist and is dropped.
            let _ = lease.cover(950, 2_000);
            // Past the durable prime the lease fails closed.
            let refused = lease.cover(1_001, 2_000).unwrap_err();
            assert_eq!(durable.load(Ordering::Acquire), 1_000);
            refused
        });

        assert!(matches!(refused, Error::NotSent(_)), "got {refused:?}");
        assert_eq!(
            counter_value(
                snapshotter.snapshot(),
                names::NONCE_LEASE_REFUSALS_TOTAL,
                None
            ),
            1
        );
        assert!(
            counter_value(
                snapshotter.snapshot(),
                names::NONCE_PERSIST_DROPPED_TOTAL,
                None
            ) >= 1,
            "the dropped enqueue must be counted"
        );
    }
}
