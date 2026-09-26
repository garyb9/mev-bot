//! Sticky trading-halt flag (SPEC-0002 H-4, SPEC-0004 K-3/K-4).
//!
//! A small shared flag the risk gate reads first. It is set fail-closed by
//! components that detect an unsafe condition — the dead-man switch uses it
//! when arming/refreshing fails — and by the kill switch. It is **sticky**: it
//! stays set until an operator clears it, so a transient failure cannot silently
//! re-enable trading.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// A cloneable, lock-free trading-halt flag.
#[derive(Debug, Clone, Default)]
pub struct TradingHalt {
    halted: Arc<AtomicBool>,
}

impl TradingHalt {
    /// A new, non-halted flag.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether trading is currently halted.
    pub fn is_halted(&self) -> bool {
        self.halted.load(Ordering::Acquire)
    }

    /// Set the halt. Returns `true` if this call transitioned to halted.
    pub fn set(&self) -> bool {
        !self.halted.swap(true, Ordering::AcqRel)
    }

    /// Clear the halt (operator action). Returns `true` if it was halted.
    pub fn clear(&self) -> bool {
        self.halted.swap(false, Ordering::AcqRel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_clear_and_is_sticky_until_cleared() {
        let halt = TradingHalt::new();
        assert!(!halt.is_halted());
        assert!(halt.set());
        assert!(halt.is_halted());
        // Re-setting does not report a transition.
        assert!(!halt.set());
        assert!(halt.is_halted());
        assert!(halt.clear());
        assert!(!halt.is_halted());
        assert!(!halt.clear());
    }

    #[test]
    fn clones_share_state() {
        let halt = TradingHalt::new();
        let clone = halt.clone();
        halt.set();
        assert!(clone.is_halted());
    }
}
