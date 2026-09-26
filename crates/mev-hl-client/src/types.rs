//! Hyperliquid wire types (SPEC-0001 §6–§7).
//!
//! Field names follow the API's camelCase; numeric strings are decoded into
//! [`rust_decimal::Decimal`] to avoid floating-point in money paths.

use std::collections::BTreeMap;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// Perpetuals universe metadata (`meta`).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Meta {
    /// Per-asset metadata, indexed by asset id.
    pub universe: Vec<AssetMeta>,
}

/// One perpetual asset's metadata.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetMeta {
    /// Asset name, e.g. `BTC`.
    pub name: String,
    /// Size decimals used for order sizing.
    pub sz_decimals: u32,
    /// Maximum leverage.
    pub max_leverage: u32,
    /// Whether the asset is delisted.
    #[serde(default)]
    pub is_delisted: bool,
    /// Whether only isolated margin is allowed.
    #[serde(default)]
    pub only_isolated: bool,
}

/// Spot universe metadata (`spotMeta`).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SpotMeta {
    /// Spot pairs, indexed by spot index.
    pub universe: Vec<SpotPair>,
    /// Spot tokens.
    pub tokens: Vec<SpotToken>,
}

/// One spot pair.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SpotPair {
    /// Pair name, e.g. `UBTC/USDC` or `PURR/USDC`.
    pub name: String,
    /// Spot pair index (`@{index}` for non-PURR pairs).
    pub index: u32,
    /// The two token indices `[base, quote]`.
    pub tokens: [u32; 2],
}

/// One spot token.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpotToken {
    /// Token name.
    pub name: String,
    /// Token index.
    pub index: u32,
    /// Size decimals.
    pub sz_decimals: u32,
}

/// Mid prices keyed by coin (`allMids`).
pub type AllMids = BTreeMap<String, Decimal>;

/// An L2 book snapshot.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct L2Book {
    /// Coin.
    pub coin: String,
    /// Server timestamp in milliseconds.
    pub time: u64,
    /// `[bids, asks]`.
    pub levels: [Vec<Level>; 2],
}

/// A single book level.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Level {
    /// Price.
    pub px: Decimal,
    /// Total size.
    pub sz: Decimal,
    /// Number of orders at this level.
    pub n: u32,
}

impl L2Book {
    /// Best bid, if any.
    pub fn best_bid(&self) -> Option<&Level> {
        self.levels[0].first()
    }

    /// Best ask, if any.
    pub fn best_ask(&self) -> Option<&Level> {
        self.levels[1].first()
    }

    /// Mid price, if the book has both sides.
    pub fn mid(&self) -> Option<Decimal> {
        Some((self.best_bid()?.px + self.best_ask()?.px) / Decimal::TWO)
    }
}

/// Per-coin asset context (`metaAndAssetCtxs`, `activeAssetCtx`).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetCtx {
    /// Current funding rate (hourly).
    pub funding: Decimal,
    /// Open interest.
    pub open_interest: Decimal,
    /// Previous day price.
    pub prev_day_px: Decimal,
    /// 24h notional volume.
    pub day_ntl_vlm: Decimal,
    /// Premium (may be absent).
    #[serde(default)]
    pub premium: Option<Decimal>,
    /// Oracle price.
    pub oracle_px: Decimal,
    /// Mark price.
    pub mark_px: Decimal,
    /// Mid price (may be absent).
    #[serde(default)]
    pub mid_px: Option<Decimal>,
}

/// `metaAndAssetCtxs` response: metadata plus per-asset contexts.
#[derive(Debug, Clone)]
pub struct MetaAndAssetCtxs {
    /// Perpetuals metadata.
    pub meta: Meta,
    /// Asset contexts, positionally aligned with `meta.universe`.
    pub asset_ctxs: Vec<AssetCtx>,
}
