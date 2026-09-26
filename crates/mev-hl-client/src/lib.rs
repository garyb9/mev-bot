//! Hyperliquid (HyperCore) client.
//!
//! Owns market data (SPEC-0001) and execution (SPEC-0002) behind
//! backend-swappable traits. Implementations land in later milestones.

/// Network endpoints live in `mev-core` config; re-exported for convenience.
pub use mev_core::config::Network;
