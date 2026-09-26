//! Hyperliquid (HyperCore) client.
//!
//! Owns market data (SPEC-0001) and execution (SPEC-0002) behind
//! backend-swappable traits. REST `/info` and the WebSocket market stream land
//! in M1.1/M1.2; execution follows in later milestones.

pub mod assets;
pub mod client;
pub mod market;
pub mod types;
pub mod ws;

pub use assets::{AssetMap, Market, MarketKind, MarketSelector};
pub use client::{HttpInfo, InfoApi};
pub use market::{FEED_BOOK, FEED_CTX, FeedAge, MarketState, OrderBook, Tolerance};
pub use types::{
    AllMids, AssetCtx, AssetCtxUpdate, AssetMeta, Bbo, L2Book, Level, Meta, MetaAndAssetCtxs,
    PerpDex, SpotMeta, SpotPair, SpotToken, Trade,
};
pub use ws::{MarketStream, StreamEvent, Subscription, WsMarketStream};

/// Network endpoints live in `mev-core` config; re-exported for convenience.
pub use mev_core::config::Network;
