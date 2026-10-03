//! The shared write path: mode gating, EIP-712 signing, and nonce sequencing
//! (SPEC-0002 §5, H-6/H-7).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use hl_arb_core::clock::{Clock, SystemClock};
use hl_arb_core::config::Mode;
use hl_arb_core::db::{Db, writer::DbWriter};
use hl_arb_core::error::{Error, Result};

use crate::nonce::{
    DEFAULT_NONCE_LEASE_MS, NONCE_WARN_INTERVAL_MS, NONCE_WRITE_QUEUE, NonceLease, NonceManager,
    VENUE_MAX_FUTURE_MS,
};
use crate::order::Action;
use crate::signing::AgentSigner;

use super::{Prepared, WriteGate, build_request};

/// Shared write path used by every transport: mode gating, EIP-712 signing,
/// nonce sequencing, and write-behind lease persistence (SPEC-0002 §5, H-6).
///
/// Dropping a `WriteCore` with an attached nonce store drops its [`DbWriter`],
/// which drains the queue and joins the writer thread — a brief blocking wait
/// at shutdown.
pub struct WriteCore {
    signer: Option<AgentSigner>,
    pub(super) nonce: tokio::sync::Mutex<NonceManager>,
    /// Coalesced write-behind nonce persistence; absent when no store is
    /// attached, in which case no nonce is persisted (tests, `observe`).
    pub(super) nonce_store: Option<NonceLease>,
    /// Shared connection, kept only for the off-hot-path operator nonce reset
    /// (`reset_nonce`).
    nonce_db: Option<Arc<Mutex<Db>>>,
    /// Set when the persisted nonce was beyond the venue's future window at
    /// boot. `prepare` fails closed until an operator calls `reset_nonce`.
    nonce_corrupt: std::sync::atomic::AtomicBool,
    /// Time of the last rate-limited log for a runtime far-future refusal
    /// (SPEC-0002 H-6 follow-up). A stuck far-future value must not log per
    /// order.
    future_refusal_warn_ms: AtomicU64,
    clock: Arc<dyn Clock>,
    nonce_lease_ms: u64,
    gate: WriteGate,
    expires_after: Option<u64>,
    vault_address: Option<String>,
    /// Cached `hl_sign_seconds` handle (SPEC-0002 H-7). Pre-created so the hot
    /// statement does not format a label or build a key per sign.
    sign_histogram: metrics::Histogram,
    /// Cached runtime far-future refusal counter (SPEC-0002 H-6 follow-up):
    /// incremented per refused `prepare`, so no per-order key lookup.
    future_refusals: metrics::Counter,
}

impl WriteCore {
    /// Create a write core for `mode`; a signer is required unless blocked.
    pub fn new(mode: Mode, signer: Option<AgentSigner>) -> Result<Self> {
        let gate = WriteGate::from(mode);
        if gate != WriteGate::Blocked && signer.is_none() {
            return Err(Error::Config(format!(
                "{} mode requires an agent signer",
                match mode {
                    Mode::Observe => "observe",
                    Mode::Simulate => "simulate",
                    Mode::Live => "live",
                }
            )));
        }
        Ok(Self {
            signer,
            nonce: tokio::sync::Mutex::new(NonceManager::new()),
            nonce_store: None,
            nonce_db: None,
            nonce_corrupt: std::sync::atomic::AtomicBool::new(false),
            future_refusal_warn_ms: AtomicU64::new(0),
            clock: Arc::new(SystemClock),
            nonce_lease_ms: DEFAULT_NONCE_LEASE_MS,
            gate,
            expires_after: None,
            vault_address: None,
            sign_histogram: metrics::histogram!(hl_arb_metrics::names::SIGN_SECONDS),
            future_refusals: metrics::counter!(hl_arb_metrics::names::NONCE_FUTURE_REFUSALS_TOTAL),
        })
    }

    /// Inject the clock used for nonce generation (tests).
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Override the write-ahead nonce lease (in nonce space). Values below 4 are
    /// clamped so the half-lease refresh threshold stays meaningful.
    pub fn with_nonce_lease(mut self, lease_ms: u64) -> Self {
        self.nonce_lease_ms = lease_ms.max(4);
        self
    }

    /// Attach a durable nonce store and restore the persisted high-water mark.
    ///
    /// The bot **never resumes below the persisted value**: the manager starts
    /// at `max(now, restored + 1)`. The restart prime is `max(restored, now +
    /// lease)`, so a crash that sends nothing does not ratchet the horizon; a
    /// fast restart with `restored >= now + lease` may refuse the first send
    /// until the urgent write-behind refresh lands (milliseconds).
    ///
    /// A persisted value beyond the venue's future window
    /// ([`VENUE_MAX_FUTURE_MS`]) cannot be one this bot sent: it is treated as
    /// corruption, counted in `hl_nonce_resume_corrupt_total`, logged at
    /// `error`, and the core **fails closed** until an operator calls
    /// [`Self::reset_nonce`] (SPEC-0002 H-6). It is never silently clamped.
    pub fn with_nonce_db(mut self, db: Arc<Mutex<Db>>) -> Result<Self> {
        let now = self.clock.now_ms();
        let (resumed, horizon, corrupt) = {
            let guard = db
                .lock()
                .map_err(|_| Error::Config("db lock poisoned".into()))?;
            let restored = guard.nonce_last()?.unwrap_or(0);
            let ceiling = now.saturating_add(VENUE_MAX_FUTURE_MS);
            let corrupt = restored > ceiling;
            if corrupt {
                metrics::counter!(hl_arb_metrics::names::NONCE_RESUME_CORRUPT_TOTAL).increment(1);
                tracing::error!(
                    restored,
                    ceiling,
                    "persisted nonce is beyond the venue future window; refusing to send until an operator resets the nonce"
                );
            }
            let horizon = restored.max(now.saturating_add(self.nonce_lease_ms));
            if !corrupt {
                guard.set_nonce_last(horizon)?;
            }
            (restored, horizon, corrupt)
        };
        let writer = DbWriter::spawn_shared(db.clone(), NONCE_WRITE_QUEUE);
        let durable = writer.nonce_hwm();
        let lease = NonceLease::new(writer, horizon, self.nonce_lease_ms, durable, now);
        self.nonce = tokio::sync::Mutex::new(NonceManager::resume(resumed));
        self.nonce_store = Some(lease);
        self.nonce_db = Some(db);
        self.nonce_corrupt
            .store(corrupt, std::sync::atomic::Ordering::Release);
        Ok(self)
    }

    /// Test-only: attach a lease over an in-memory manager resumed from
    /// `resume_from`, bypassing SQLite.
    #[cfg(test)]
    pub(crate) fn with_test_nonce_store(mut self, lease: NonceLease, resume_from: u64) -> Self {
        self.nonce = tokio::sync::Mutex::new(NonceManager::resume(resume_from));
        self.nonce_store = Some(lease);
        self
    }

    /// The effective write gate.
    pub fn gate(&self) -> WriteGate {
        self.gate
    }

    /// Set the optional `expiresAfter` field applied to every action.
    pub fn with_expires_after(mut self, expires_after: Option<u64>) -> Self {
        self.expires_after = expires_after;
        self
    }

    /// Set the optional vault address applied to every action.
    pub fn with_vault_address(mut self, vault_address: Option<String>) -> Self {
        self.vault_address = vault_address;
        self
    }

    /// Restore a specific nonce high-water mark (explicit operator path).
    ///
    /// Clamped to never go below any known value (the in-memory last and the
    /// durable/requested marks), so it cannot force a reuse. The floor is read
    /// under the nonce lock in [`Self::rebase_nonce`], so a concurrent
    /// `prepare` cannot land between measuring the floor and rewriting it.
    /// Also clears a prior corruption fail-closed state and writes the new
    /// horizon synchronously so a restart sees it (SPEC-0002 H-6).
    pub async fn restore_nonce(&self, last: u64) -> Result<()> {
        self.rebase_nonce(last, false).await
    }

    /// Operator escape hatch for a corrupt persisted nonce: abandon history and
    /// resume from the wall clock, clearing the fail-closed state (SPEC-0002
    /// H-6). Intentionally allowed to lower the persisted value, so it is
    /// **gated on the boot corruption flag**: while that flag is clear, `prepare`
    /// may have handed out nonces and a reset could force a reuse.
    pub async fn reset_nonce(&self) -> Result<()> {
        if !self.nonce_corrupt.load(Ordering::Acquire) {
            return Err(Error::Config(
                "refusing to reset the nonce: it is not flagged corrupt, so nonces \
                 may already have been issued; restart to recover instead"
                    .into(),
            ));
        }
        let now = self.clock.now_ms();
        self.rebase_nonce(now, true).await
    }

    /// Write `requested` (and a lease ahead of it) to the durable store and
    /// rebase the in-memory manager and lease. The synchronous write is safe
    /// here: this is the explicit operator path, never the hot path.
    ///
    /// `allow_lower` is set only by the corrupt-value operator reset. On the
    /// restore path it is clear, so the floor — the in-memory last together
    /// with the durable and requested marks, read while holding the nonce lock
    /// that `prepare` reserves under — wins: a concurrent send can never be
    /// lowered underneath the rebase and handed out twice.
    ///
    /// The durable atomic is lowered under the same lock the writer thread takes
    /// to commit a nonce, so the database write and the atomic cannot be
    /// observed apart: no writer commit interleaves between them.
    async fn rebase_nonce(&self, requested: u64, allow_lower: bool) -> Result<()> {
        let now = self.clock.now_ms();
        // Replace the in-memory manager under its lock, and measure the floor
        // under the same lock so no `prepare` can reserve a nonce between.
        let mut manager = self.nonce.lock().await;
        let known = self
            .nonce_store
            .as_ref()
            .map_or(0, |store| store.durable().max(store.requested()));
        let floor = manager.last().max(known);
        let restore = if allow_lower {
            requested
        } else {
            requested.max(floor)
        };
        let horizon = restore.max(now.saturating_add(self.nonce_lease_ms));
        *manager = NonceManager::resume(restore);
        if let Some(db) = &self.nonce_db {
            let guard = db
                .lock()
                .map_err(|_| Error::Config("db lock poisoned".into()))?;
            guard.set_nonce_last(horizon)?;
            if let Some(store) = &self.nonce_store {
                store.rebase(horizon, now);
            }
        } else if let Some(store) = &self.nonce_store {
            store.rebase(horizon, now);
        }
        drop(manager);
        self.nonce_corrupt.store(false, Ordering::Release);
        Ok(())
    }

    /// Test-only: the confirmed durable and requested nonce horizons.
    #[cfg(test)]
    pub(crate) fn nonce_marks(&self) -> Option<(u64, u64)> {
        self.nonce_store
            .as_ref()
            .map(|store| (store.durable(), store.requested()))
    }

    /// The current nonce high-water mark (for persistence and tests).
    pub async fn last_nonce(&self) -> u64 {
        self.nonce.lock().await.last()
    }

    /// Resync the nonce after a stale/duplicate/recent-window rejection.
    ///
    /// Errors while the corrupt fail-closed state is set: the resync path would
    /// advance from a value known to be corrupt, so an operator reset is the only
    /// correct recovery.
    pub async fn heal_nonce(&self) -> Result<u64> {
        if self
            .nonce_corrupt
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(Error::NotSent(
                "persisted nonce is corrupt; an operator must reset it before resyncing".into(),
            ));
        }
        let now = self.clock.now_ms();
        let nonce = self.nonce.lock().await.on_reject(now);
        if let Some(store) = &self.nonce_store {
            store.force(nonce, now);
        }
        Ok(nonce)
    }

    /// Sign the next envelope for `action`, gated and persisted for `live`.
    pub async fn prepare(&self, action: &Action) -> Result<Prepared> {
        if self.gate == WriteGate::Blocked {
            return Err(Error::Config(
                "observe mode cannot sign or submit actions".into(),
            ));
        }
        let signer = self
            .signer
            .as_ref()
            .ok_or_else(|| Error::Config("no agent signer is configured".into()))?;
        // Fail closed until an operator resets a corrupt persisted nonce.
        if self
            .nonce_corrupt
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(Error::NotSent(
                "persisted nonce is corrupt; an operator must reset the nonce".to_string(),
            ));
        }
        let now = self.clock.now_ms();
        let nonce = match self.nonce.lock().await.next(now) {
            Ok(nonce) => nonce,
            Err(err) => {
                // The next candidate is beyond the venue's future window
                // (corruption or a large backwards clock step). Fail closed;
                // the condition clears once the clock catches up. Counted
                // separately from boot corruption, and the error is
                // rate-limited so a stuck value cannot log per order.
                self.future_refusals.increment(1);
                if refusal_log_due(&self.future_refusal_warn_ms, now) {
                    tracing::error!(
                        now,
                        "next nonce is beyond the venue future window; refusing to send"
                    );
                }
                return Err(err);
            }
        };
        let started = Instant::now();
        let built = build_request(
            action,
            signer,
            nonce,
            self.vault_address.clone(),
            self.expires_after,
        );
        // Build + msgpack + EIP-712 sign (SPEC-0002 H-7). Recorded even on
        // failure: the stage ran.
        self.sign_histogram.record(started.elapsed().as_secs_f64());
        let request = built?;
        if self.gate == WriteGate::DryRun {
            return Ok(Prepared::DryRun(Box::new(request)));
        }
        // Protective actions (cancels, the dead-man switch) must never be
        // blocked by a stalled nonce-durability write: refusing them would
        // leave exposure resting while the store is unavailable (SEC-007).
        // Order placement and anything else that creates exposure stays gated
        // exactly as before.
        if !is_protective(action)
            && let Some(store) = &self.nonce_store
            && let Err(err) = store.cover(nonce, now)
        {
            // The nonce is not covered, so it will not be sent: free it so a
            // stall does not burn through nonce space (cheap: the failure path
            // only, and a no-op if a later prepare advanced past it).
            self.nonce.lock().await.release(nonce);
            return Err(err);
        }
        Ok(Prepared::Send(Box::new(request)))
    }
}

/// Whether `action` protects the account rather than creating exposure.
///
/// Protective actions bypass the nonce-durability gate so a failing write to
/// the nonce store can never prevent a cancel or the dead-man switch from
/// reaching the venue (SEC-007).
fn is_protective(action: &Action) -> bool {
    matches!(
        action,
        Action::Cancel { .. } | Action::CancelByCloid { .. } | Action::ScheduleCancel { .. }
    )
}

/// Whether a runtime far-future refusal should be logged now, stamping
/// `last_ms` when it is. Rate-limited to one message per
/// [`NONCE_WARN_INTERVAL_MS`] (the same pattern as the write-behind warning) so
/// a stuck far-future value cannot flood the log on every `prepare`.
pub(super) fn refusal_log_due(last_ms: &AtomicU64, now_ms: u64) -> bool {
    let last = last_ms.load(Ordering::Relaxed);
    now_ms >= last.saturating_add(NONCE_WARN_INTERVAL_MS)
        && last_ms
            .compare_exchange(last, now_ms, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nonce::NonceLease;
    use crate::order::{CancelByCloidWire, CancelWire, Grouping, Tif, limit_order};
    use hl_arb_core::clock::FixedClock;

    const KEY: &str = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";

    fn signer() -> AgentSigner {
        AgentSigner::from_hex(KEY, true).unwrap()
    }

    fn order() -> Action {
        Action::Order {
            orders: vec![limit_order(0, true, "50000", "0.1", Tif::Gtc, false, None)],
            grouping: Grouping::Na,
        }
    }

    #[tokio::test]
    async fn protective_actions_bypass_the_nonce_durability_gate() {
        // A dead nonce-store writer makes every `cover()` fail. Order placement
        // must stay fail-closed; protective actions must proceed anyway so a
        // stalled store can never keep a cancel from reaching the venue
        // (SEC-007).
        let clock = Arc::new(FixedClock::new(1_000_000));
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        drop(rx); // writer gone: every enqueue fails
        let durable = Arc::new(AtomicU64::new(0));
        let core = WriteCore::new(Mode::Live, Some(signer()))
            .unwrap()
            .with_clock(clock)
            .with_test_nonce_store(
                NonceLease::with_channel(tx, 1_000_100, 100, durable, 1_000_000),
                1_000_000,
            );

        // Order placement is gated exactly as before.
        let mut place_blocked = false;
        for _ in 0..200 {
            match core.prepare(&order()).await {
                Ok(Prepared::Send(_)) => {}
                Ok(Prepared::DryRun(_)) => panic!("live mode must send"),
                Err(Error::NotSent(_)) => {
                    place_blocked = true;
                    break;
                }
                Err(other) => panic!("unexpected error: {other:?}"),
            }
        }
        assert!(place_blocked, "an order must stay gated by a failing store");

        // Cancels and the dead-man switch are exempt.
        let cancels = vec![CancelWire { a: 0, o: 42 }];
        let cloid_cancels = vec![CancelByCloidWire {
            asset: 0,
            cloid: "0x00000000000000000000000000000001".into(),
        }];
        for action in [
            Action::Cancel {
                cancels: cancels.clone(),
            },
            Action::CancelByCloid {
                cancels: cloid_cancels.clone(),
            },
            Action::ScheduleCancel {
                time: Some(1_060_000),
            },
        ] {
            match core.prepare(&action).await {
                Ok(Prepared::Send(_)) => {}
                other => panic!("protective action must bypass the gate: {other:?}"),
            }
        }
    }
}
