//! Shared health/readiness state for `/healthz` and `/readyz`.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// Process health. `ready` gates `/readyz`; it starts false and is set true
/// once all required feeds are fresh (SPEC-0001 wires staleness in later).
#[derive(Debug, Clone, Default)]
pub struct Health {
    ready: Arc<AtomicBool>,
}

impl Health {
    /// Create a not-ready health state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set readiness.
    pub fn set_ready(&self, ready: bool) {
        self.ready.store(ready, Ordering::SeqCst);
    }

    /// Whether the process is ready to serve/trade.
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readiness_toggles() {
        let health = Health::new();
        assert!(!health.is_ready());
        health.set_ready(true);
        assert!(health.is_ready());
    }
}
