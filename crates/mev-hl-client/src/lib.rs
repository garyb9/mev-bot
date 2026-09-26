//! Hyperliquid (HyperCore) client.
//!
//! Owns market data (SPEC-0001) and execution (SPEC-0002) behind
//! backend-swappable traits. REST `/info` lands in M1.1; WebSocket streams and
//! execution follow in later milestones.

pub mod client;
pub mod types;

pub use client::{HttpInfo, InfoApi};
pub use types::{
    AllMids, AssetCtx, AssetMeta, L2Book, Level, Meta, MetaAndAssetCtxs, SpotMeta, SpotPair,
    SpotToken,
};

/// Network endpoints live in `mev-core` config; re-exported for convenience.
pub use mev_core::config::Network;
