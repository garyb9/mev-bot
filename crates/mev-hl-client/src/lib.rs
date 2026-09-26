//! Hyperliquid (HyperCore) client.
//!
//! Owns market data (SPEC-0001) and execution (SPEC-0002) behind
//! backend-swappable traits. REST `/info` and the WebSocket market stream land
//! in M1.1/M1.2; execution follows in later milestones.

pub mod assets;
pub mod client;
pub mod cloid;
pub mod deadman;
pub mod exchange;
pub mod market;
pub mod nonce;
pub mod order;
pub mod raw_ws;
pub mod signing;
pub mod types;
pub mod ws;
pub mod ws_exchange;

pub use assets::{AssetMap, Market, MarketKind, MarketSelector};
pub use client::{HttpInfo, InfoApi};
pub use cloid::CloidFactory;
pub use deadman::DeadMansSwitch;
pub use exchange::{
    ActionResponse, ExchangeApi, ExchangeRequest, ExchangeResponse, HttpExchange, OrderResponse,
    OrderStatus, Prepared, RejectReason, WriteCore, WriteGate, build_request,
};
pub use market::{FEED_BOOK, FEED_CTX, FeedAge, MarketState, OrderBook, Tolerance};
pub use nonce::{NonceManager, ResetReason, now_ms};
pub use order::{
    Action, CancelByCloidWire, CancelWire, Grouping, MIN_ORDER_NOTIONAL, OrderParams, OrderType,
    OrderWire, Tif, Tpsl, build_order_wire, round_price, round_price_aggressive, round_price_with,
    round_size, wire_decimal,
};
pub use raw_ws::{HlProtocol, Protocol, RawEvent, RawWsConn};
pub use signing::{AgentSigner, Signature, action_hash, recover_address, signing_hash};
pub use types::{
    AllMids, AssetCtx, AssetCtxUpdate, AssetMeta, AssetPosition, Bbo, ClearinghouseState,
    FundingDelta, L2Book, Level, Leverage, MarginSummary, Meta, MetaAndAssetCtxs, OpenOrder,
    OrderResolution, OrderStatusOrder, OrderStatusResponse, PerpDex, Position, SpotBalance,
    SpotClearinghouseState, SpotMeta, SpotPair, SpotToken, Trade, UserFees, UserFill, UserFunding,
    UserRateLimit,
};
pub use ws::{MarketStream, StreamEvent, Subscription, WsMarketStream};
pub use ws_exchange::WsExchange;

/// Network endpoints live in `mev-core` config; re-exported for convenience.
pub use mev_core::config::Network;
