//! Hyperliquid wire types (SPEC-0001 §6–§7).
//!
//! Field names follow the API's camelCase; numeric strings are decoded into
//! [`rust_decimal::Decimal`] to avoid floating-point in money paths.

use std::collections::BTreeMap;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::exchange::RejectReason;

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

/// Best bid/offer snapshot (`bbo`). Either side may be absent.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Bbo {
    /// Coin.
    pub coin: String,
    /// Server timestamp in milliseconds.
    pub time: u64,
    /// `[bid, ask]`, each optionally absent.
    pub bbo: [Option<Level>; 2],
}

impl Bbo {
    /// Best bid, if present.
    pub fn bid(&self) -> Option<&Level> {
        self.bbo[0].as_ref()
    }

    /// Best ask, if present.
    pub fn ask(&self) -> Option<&Level> {
        self.bbo[1].as_ref()
    }
}

/// A public trade print.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Trade {
    /// Coin.
    pub coin: String,
    /// Aggressor side (`B` or `A`).
    pub side: String,
    /// Price.
    pub px: Decimal,
    /// Size.
    pub sz: Decimal,
    /// Server timestamp in milliseconds.
    pub time: u64,
    /// Transaction hash.
    #[serde(default)]
    pub hash: Option<String>,
    /// Trade id.
    #[serde(default)]
    pub tid: Option<u64>,
}

/// `activeAssetCtx` payload: coin plus its context.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AssetCtxUpdate {
    /// Coin.
    pub coin: String,
    /// Asset context.
    pub ctx: AssetCtx,
}

/// A builder-deployed HIP-3 perpetual dex (`perpDexs`).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PerpDex {
    /// Short dex name, used as the `dex` parameter and coin prefix.
    pub name: String,
    /// Human-readable name.
    #[serde(default)]
    pub full_name: Option<String>,
}

/// `metaAndAssetCtxs` response: metadata plus per-asset contexts.
#[derive(Debug, Clone)]
pub struct MetaAndAssetCtxs {
    /// Perpetuals metadata.
    pub meta: Meta,
    /// Asset contexts, positionally aligned with `meta.universe`.
    pub asset_ctxs: Vec<AssetCtx>,
}

/// A single perp position (`clearinghouseState.assetPositions[].position`).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Position {
    /// Coin.
    pub coin: String,
    /// Signed size (positive long, negative short).
    pub szi: Decimal,
    /// Entry price.
    pub entry_px: Option<Decimal>,
    /// Position value in USD.
    pub position_value: Decimal,
    /// Unrealized PnL.
    pub unrealized_pnl: Decimal,
    /// Return on equity.
    pub return_on_equity: Decimal,
    /// Liquidation price, if any.
    #[serde(default)]
    pub liquidation_px: Option<Decimal>,
    /// Margin used.
    pub margin_used: Decimal,
    /// Leverage details.
    #[serde(default)]
    pub leverage: Option<Leverage>,
}

/// Leverage settings for a position.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Leverage {
    /// Leverage type (`cross` or `isolated`).
    #[serde(rename = "type")]
    pub type_field: String,
    /// Leverage value.
    pub value: u32,
}

/// One entry of `assetPositions` (wraps a [`Position`]).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AssetPosition {
    /// The wrapped position.
    pub position: Position,
}

/// Perp clearinghouse account summary.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClearinghouseState {
    /// Account value.
    pub margin_summary: MarginSummary,
    /// Cross-margin summary.
    pub cross_margin_summary: MarginSummary,
    /// Withdrawable USDC.
    pub withdrawable: Decimal,
    /// Open positions.
    #[serde(default)]
    pub asset_positions: Vec<AssetPosition>,
}

impl ClearinghouseState {
    /// Find a position by coin.
    pub fn position(&self, coin: &str) -> Option<&Position> {
        self.asset_positions
            .iter()
            .map(|entry| &entry.position)
            .find(|position| position.coin == coin)
    }
}

/// Margin summary block.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MarginSummary {
    /// Account value.
    pub account_value: Decimal,
    /// Total notional position.
    pub total_ntl_pos: Decimal,
    /// Total raw USD used as margin.
    pub total_raw_usd: Decimal,
    /// Total margin used.
    pub total_margin_used: Decimal,
}

/// An open order (`openOrders` / `frontendOpenOrders`).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenOrder {
    /// Coin.
    pub coin: String,
    /// Order id.
    pub oid: u64,
    /// Buy if true.
    pub side: String,
    /// Limit price.
    pub limit_px: Decimal,
    /// Remaining size.
    pub sz: Decimal,
    /// Original size.
    pub orig_sz: Decimal,
    /// Server timestamp.
    pub timestamp: u64,
    /// Client order id, if any.
    #[serde(default)]
    pub cloid: Option<String>,
    /// Whether reduce-only.
    #[serde(default)]
    pub reduce_only: bool,
}

impl OpenOrder {
    /// Whether this order is a buy.
    pub fn is_buy(&self) -> bool {
        self.side.eq_ignore_ascii_case("B")
    }
}

/// `orderStatus` response for a single query (by `oid` or `cloid`).
///
/// The venue returns `{"status":"order","order":{"order":{…},"status":<state>}}`
/// for a hit and `{"status":"unknownOid"}` for a miss (SPEC-0002 §9).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderStatusResponse {
    /// `order` when found, `unknownOid` when not.
    pub status: String,
    /// The order wrapper when the order was found.
    #[serde(default)]
    pub order: Option<OrderStatusOrder>,
}

/// The nested `order` block of an [`OrderStatusResponse`].
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderStatusOrder {
    /// The order details.
    #[serde(default)]
    pub order: Option<OpenOrder>,
    /// The order state: `open`, `filled`, `canceled`, `triggered`, rejected
    /// classes, etc. (SPEC-0002 §9).
    #[serde(default)]
    pub status: String,
    /// When the state was recorded.
    #[serde(default)]
    pub status_timestamp: u64,
}

impl OrderStatusResponse {
    /// Whether the order exists (the venue did not return `unknownOid`).
    pub fn is_found(&self) -> bool {
        self.status != "unknownOid"
    }

    /// The resolved order state, if found.
    pub fn order_status(&self) -> Option<&str> {
        self.order.as_ref().map(|order| order.status.as_str())
    }

    /// Whether the order was already fully filled.
    pub fn is_filled(&self) -> bool {
        self.order_status() == Some("filled")
    }

    /// Resolve the query into a typed [`OrderResolution`] for reconciliation.
    pub fn resolution(&self) -> OrderResolution {
        match self.order_status() {
            Some("open") => OrderResolution::Resting,
            Some("filled") => OrderResolution::Filled,
            Some("triggered") => OrderResolution::Triggered,
            Some(status) if is_canceled(status) => OrderResolution::Cancelled,
            Some("rejected") => OrderResolution::Rejected,
            Some(status) if RejectReason::parse(status).is_some() => OrderResolution::Rejected,
            Some(other) => OrderResolution::Other(other.to_string()),
            None if !self.is_found() => OrderResolution::NotFound,
            None => {
                // Found but the inner block was absent; treat as unknown.
                OrderResolution::Other("unknown".to_string())
            }
        }
    }
}

/// Whether a venue status string is a cancellation class (`canceled`,
/// `marginCanceled`, `scheduledCancel`, …).
fn is_canceled(status: &str) -> bool {
    status == "canceled" || status.ends_with("Canceled") || status == "scheduledCancel"
}

/// The reconciled state of an order queried by `orderStatus` (SPEC-0002 H-2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderResolution {
    /// Working on the book.
    Resting,
    /// Fully filled.
    Filled,
    /// Trigger order triggered.
    Triggered,
    /// Cancelled (by user, margin, self-trade, dead-man, …).
    Cancelled,
    /// Rejected at placement or by a rejection class.
    Rejected,
    /// The venue has no such order.
    NotFound,
    /// Any other state string, preserved verbatim.
    Other(String),
}

impl OrderResolution {
    /// Stable label for metrics and order records.
    pub fn label(&self) -> String {
        match self {
            OrderResolution::Resting => "resting".into(),
            OrderResolution::Filled => "filled".into(),
            OrderResolution::Triggered => "triggered".into(),
            OrderResolution::Cancelled => "cancelled".into(),
            OrderResolution::Rejected => "rejected".into(),
            OrderResolution::NotFound => "notFound".into(),
            OrderResolution::Other(other) => other.clone(),
        }
    }
}

/// User fee schedule (`userFees`), reduced to the fields the EV model needs.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserFees {
    /// Active fee schedule.
    #[serde(default)]
    pub fee_schedule: Option<FeeSchedule>,
    /// Recent daily volumes.
    #[serde(default)]
    pub daily_user_vlm: Vec<DailyVolume>,
    /// Current maker/taker rates applied to the user.
    pub user_cross_rate: Option<Decimal>,
    /// Current spot maker/taker rates.
    pub user_add_rate: Option<Decimal>,
}

/// Fee tier schedule details.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FeeSchedule {
    /// Maker rate as a string (e.g. `"0.00015"`).
    #[serde(default)]
    pub add: Option<String>,
    /// Taker rate as a string.
    #[serde(default)]
    pub cross: Option<String>,
}

/// Daily volume entry.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DailyVolume {
    /// Date.
    #[serde(default)]
    pub date: String,
    /// Volume.
    #[serde(default)]
    pub user_vlm: Decimal,
}

/// A single spot balance (`spotClearinghouseState.balances[]`).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpotBalance {
    /// Token symbol, e.g. `USDC`.
    pub coin: String,
    /// Token index.
    pub token: u32,
    /// Amount held (locked by resting orders).
    pub hold: Decimal,
    /// Total balance.
    pub total: Decimal,
    /// Entry notional.
    #[serde(default)]
    pub entry_ntl: Decimal,
}

/// Spot clearinghouse state (`spotClearinghouseState`).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct SpotClearinghouseState {
    /// Balances, one per held token.
    #[serde(default)]
    pub balances: Vec<SpotBalance>,
}

impl SpotClearinghouseState {
    /// The balance for a token symbol, if held.
    pub fn balance(&self, coin: &str) -> Option<&SpotBalance> {
        self.balances.iter().find(|b| b.coin == coin)
    }
}

/// A user funding payment (`userFunding`), flattened from the nested `delta`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserFunding {
    /// Settlement time in milliseconds.
    pub time: u64,
    /// Transaction hash.
    #[serde(default)]
    pub hash: Option<String>,
    /// The funding delta.
    pub delta: FundingDelta,
}

/// The `delta` block of a [`UserFunding`] entry.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FundingDelta {
    /// Record type (`funding`).
    #[serde(rename = "type")]
    pub type_field: String,
    /// Coin.
    pub coin: String,
    /// Signed USDC amount (negative is paid).
    pub usdc: Decimal,
    /// Position size at settlement.
    pub szi: Decimal,
    /// Funding rate applied.
    #[serde(default)]
    pub rate: Decimal,
}

/// A user fill (`userFills` / `userFillsByTime`).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserFill {
    /// Coin.
    pub coin: String,
    /// Fill price.
    pub px: Decimal,
    /// Fill size.
    pub sz: Decimal,
    /// Aggressor side (`B` bid / `A` ask).
    pub side: String,
    /// Fill time in milliseconds.
    pub time: u64,
    /// Realized PnL.
    #[serde(default)]
    pub closed_pnl: Decimal,
    /// Order id.
    #[serde(default)]
    pub oid: Option<u64>,
    /// Whether the fill crossed the spread (taker).
    #[serde(default)]
    pub crossed: bool,
    /// Fee paid.
    #[serde(default)]
    pub fee: Decimal,
    /// Trade id.
    #[serde(default)]
    pub tid: Option<u64>,
    /// Direction label (`Open Long`, `Close Short`, ...).
    #[serde(default)]
    pub dir: Option<String>,
    /// Builder fee paid.
    #[serde(default)]
    pub builder_fee: Option<Decimal>,
}

impl UserFill {
    /// Whether this fill is a buy.
    pub fn is_buy(&self) -> bool {
        self.side.eq_ignore_ascii_case("B")
    }

    /// Whether this fill was a maker fill.
    pub fn is_maker(&self) -> bool {
        !self.crossed
    }
}

/// One entry of an `orderUpdates` push (SPEC-0002 H-3).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WsOrder {
    /// The order details.
    pub order: WsBasicOrder,
    /// Venue status string (see `OrderResolution` for the vocabulary).
    pub status: String,
    /// When the status was recorded.
    #[serde(default)]
    pub status_timestamp: u64,
}

/// The basic order block carried by `WsOrder`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WsBasicOrder {
    /// Coin.
    pub coin: String,
    /// Side (`B`/`A`).
    pub side: String,
    /// Limit price.
    pub limit_px: Decimal,
    /// Remaining size.
    pub sz: Decimal,
    /// Order id.
    pub oid: u64,
    /// Venue timestamp.
    pub timestamp: u64,
    /// Original size.
    pub orig_sz: Decimal,
    /// Client order id, if any.
    #[serde(default)]
    pub cloid: Option<String>,
}

/// `userFills` push: a snapshot (`isSnapshot: true`) then streaming fills.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WsUserFills {
    /// Whether this is the subscription's initial snapshot.
    #[serde(default)]
    pub is_snapshot: bool,
    /// The user.
    #[serde(default)]
    pub user: String,
    /// The fills.
    #[serde(default)]
    pub fills: Vec<UserFill>,
}

/// `userEvents` push. Exactly one variant field is present.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WsUserEvent {
    /// Fills (also carried by `userFills`).
    #[serde(default)]
    pub fills: Option<Vec<UserFill>>,
    /// A funding payment.
    #[serde(default)]
    pub funding: Option<WsUserFunding>,
    /// A liquidation.
    #[serde(default)]
    pub liquidation: Option<WsLiquidation>,
    /// Orders cancelled not by the user.
    #[serde(default)]
    pub non_user_cancel: Option<Vec<WsNonUserCancel>>,
}

/// A funding payment pushed on `userEvents` / `userFundings`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WsUserFunding {
    /// Settlement time in milliseconds.
    pub time: u64,
    /// Coin.
    pub coin: String,
    /// Signed USDC amount.
    pub usdc: Decimal,
    /// Position size at settlement.
    #[serde(default)]
    pub szi: Decimal,
    /// Funding rate applied.
    #[serde(default)]
    pub funding_rate: Decimal,
}

/// A liquidation event.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WsLiquidation {
    /// Liquidation id.
    #[serde(default)]
    pub lid: u64,
    /// The liquidator's address.
    #[serde(default)]
    pub liquidator: String,
    /// The liquidated user's address.
    #[serde(default)]
    pub liquidated_user: String,
    /// Liquidated notional position.
    #[serde(default)]
    pub liquidated_ntl_pos: Decimal,
    /// Liquidated account value.
    #[serde(default)]
    pub liquidated_account_value: Decimal,
}

/// An order cancelled by the system (not the user).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WsNonUserCancel {
    /// Coin.
    pub coin: String,
    /// Order id.
    pub oid: u64,
}

/// `userRateLimit` response.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserRateLimit {
    /// Requests used so far.
    #[serde(default)]
    pub n_requests_used: u64,
    /// Total request budget.
    #[serde(default)]
    pub n_requests_cap: u64,
    /// Time (ms) until the budget resets.
    #[serde(default)]
    pub request_used: u64,
}

impl UserRateLimit {
    /// Requests still available before the cap.
    pub fn remaining(&self) -> u64 {
        self.n_requests_cap.saturating_sub(self.n_requests_used)
    }
}
