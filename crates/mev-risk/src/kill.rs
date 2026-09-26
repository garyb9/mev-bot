//! Kill switch (SPEC-0004 §10, K-3).
//!
//! The kill flag is the **first** thing every risk-increasing check reads. It is
//! intentionally tiny and policy-free: this module owns only the sticky flag and
//! the flag-file helper, never signal handling or I/O scheduling (SPEC-0010 §16
//! keeps those in the control task, which polls and caches the file result).
//!
//! Triggers (SPEC-0004 K-3): `SIGUSR1`, the existence of the configured flag
//! file, and `hl panic` (which writes that file). Action: cancel every working
//! order and halt new risk. Clearing needs an explicit operator `hl resume`
//! **and** deleting the file: the in-process flag is sticky until [`KillSwitch::clear`].
//!
//! The concrete [`mev_engine::orders::OrderManager`]/`Cloid` types must not be
//! pulled into `mev-risk` (the engine depends on this crate, not the reverse),
//! so [`cancel_all_cloids`] is generic over the id and the engine supplies
//! `orders.working().map(|o| o.cloid)`.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// A cloneable, lock-free, **sticky** kill flag.
///
/// Sticky means [`KillSwitch::set`] latches until [`KillSwitch::clear`]; a
/// transient trigger observation cannot silently re-enable trading.
#[derive(Debug, Clone, Default)]
pub struct KillSwitch {
    active: Arc<AtomicBool>,
}

impl KillSwitch {
    /// A new, inactive kill switch.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether trading is currently killed.
    #[inline]
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    /// Trip the switch. Returns `true` if this call transitioned it active.
    ///
    /// The caller may be a signal handler, the flag-file poller, or the CLI via
    /// `Control::KillSwitch`; the flag itself is just an atomic swap.
    pub fn set(&self) -> bool {
        !self.active.swap(true, Ordering::AcqRel)
    }

    /// Clear the switch (explicit operator action). Returns `true` if it was
    /// active. Clearing the kill flag does **not** delete the flag file; the
    /// next poll would re-trip it, which is the intended two-key behaviour.
    pub fn clear(&self) -> bool {
        self.active.swap(false, Ordering::AcqRel)
    }
}

/// Whether the kill flag file exists at `path`.
///
/// This performs a stat and is **not** meant for the hot path: the control task
/// polls it (SPEC-0004 K-3 says every 250 ms) and caches the result, tripping
/// the [`KillSwitch`] when it appears. Kept as a free function so callers can
/// point it at `HL_KILL_FILE` without constructing a switch.
pub fn check_flag_file(path: impl AsRef<Path>) -> bool {
    path.as_ref().exists()
}

/// Collect the ids a cancel-all should emit.
///
/// Generic over the id because `Cloid` and the order manager live in
/// `mev-engine` and this crate must not depend on them (see the module docs).
/// The engine calls it with `orders.working().map(|o| o.cloid)`.
pub fn cancel_all_cloids<T: Copy>(working: impl IntoIterator<Item = T>) -> Vec<T> {
    working.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_flag_path() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("mev-risk-kill-{}-{n}.flag", std::process::id()))
    }

    #[test]
    fn starts_clear_and_is_sticky_until_cleared() {
        let kill = KillSwitch::new();
        assert!(!kill.is_active());
        assert!(kill.set());
        assert!(kill.is_active());
        // Re-setting does not report a transition and stays active.
        assert!(!kill.set());
        assert!(kill.is_active());
        assert!(kill.clear());
        assert!(!kill.is_active());
        assert!(!kill.clear());
        // Sticky only means latched, not un-clearable.
        assert!(kill.set());
        assert!(kill.is_active());
        kill.clear();
    }

    #[test]
    fn clones_share_state() {
        let kill = KillSwitch::new();
        let clone = kill.clone();
        kill.set();
        assert!(clone.is_active());
        clone.clear();
        assert!(!kill.is_active());
    }

    #[test]
    fn check_flag_file_detects_existence() {
        let path = temp_flag_path();
        assert!(!check_flag_file(&path));
        std::fs::write(&path, b"kill\n").expect("write flag");
        assert!(check_flag_file(&path));
        std::fs::remove_file(&path).expect("remove flag");
        assert!(!check_flag_file(&path));
    }

    #[test]
    fn cancel_all_collects_every_working_id() {
        assert_eq!(
            cancel_all_cloids([1u64, 2, 3]),
            vec![1u64, 2, 3],
            "helper preserves order and count"
        );
        let empty: Vec<u64> = cancel_all_cloids(std::iter::empty());
        assert!(empty.is_empty());
    }
}
