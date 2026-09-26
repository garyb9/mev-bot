//! Client order id (`cloid`) generation (SPEC-0002 §6, SPEC-0010 §10).
//!
//! Every order the engine sends carries a `cloid` so an unknown outcome can be
//! reconciled by `orderStatus` instead of blindly resent (SPEC-0002 H-2). The id
//! is 16 bytes: a per-process random 8-byte prefix plus an 8-byte counter. The
//! prefix keeps ids unique across process restarts (and therefore across
//! sessions sharing an agent wallet); the counter keeps them unique within a
//! process without coordination.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};

/// Generates unique 16-byte `cloid`s as `0x`-prefixed 32-hex-char strings.
#[derive(Debug)]
pub struct CloidFactory {
    prefix: u64,
    counter: AtomicU64,
}

impl Default for CloidFactory {
    fn default() -> Self {
        Self::new()
    }
}

impl CloidFactory {
    /// Create a factory with a fresh random per-process prefix.
    pub fn new() -> Self {
        // `RandomState` is randomly seeded by the standard library; mixing in the
        // process id guards against two processes started in the same nanosecond
        // sharing a seed.
        let prefix = RandomState::new().build_hasher().finish() ^ u64::from(std::process::id());
        Self {
            prefix,
            counter: AtomicU64::new(1),
        }
    }

    /// Create a factory with a fixed prefix (deterministic; tests only).
    #[doc(hidden)]
    pub fn with_prefix(prefix: u64) -> Self {
        Self {
            prefix,
            counter: AtomicU64::new(1),
        }
    }

    /// The next unique order id: `0x` + 16 hex chars of prefix + 16 of counter.
    pub fn next(&self) -> String {
        let counter = self.counter.fetch_add(1, Ordering::Relaxed);
        format!("0x{:016x}{:016x}", self.prefix, counter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_32_hex_chars_and_unique() {
        let factory = CloidFactory::new();
        let a = factory.next();
        let b = factory.next();
        assert_eq!(a.len(), 34, "{a}");
        assert!(a.starts_with("0x"), "{a}");
        assert!(a[2..].bytes().all(|byte| byte.is_ascii_hexdigit()), "{a}");
        assert_ne!(a, b);
    }

    #[test]
    fn counter_advances_by_one() {
        let factory = CloidFactory::with_prefix(0xdead_beef);
        assert_eq!(factory.next(), "0x00000000deadbeef0000000000000001");
        assert_eq!(factory.next(), "0x00000000deadbeef0000000000000002");
    }

    #[test]
    fn prefix_is_stable_within_a_factory() {
        let factory = CloidFactory::new();
        let first = factory.next();
        let second = factory.next();
        assert_eq!(first[..18], second[..18]);
    }
}
