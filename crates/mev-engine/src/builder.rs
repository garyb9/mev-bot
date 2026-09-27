//! Order building and per-iteration batching (SPEC-0010 §12, task E-6).
//!
//! One engine iteration's approved [`Action`]s are coalesced into at most two
//! unsigned posts, in this order: **cancels first, then places**. All cancels
//! become a single `cancelByCloid` action and all places a single bulk `order`
//! action (SPEC-0010 §12). `Action::Modify` and `Action::PlaceGroup` are not
//! part of E-6 (the `batchModify` wire fields are unverified — SPEC-0010 §22),
//! so they are dropped and reported rather than guessed at.
//!
//! The builder is synchronous and allocation-bounded: it never signs, never
//! touches the clock, and never does I/O. Aggressive prices are computed from
//! the freshest touch with the §12 rule and rounded in the safe direction by
//! [`mev_hl_client::round_price_aggressive`], never to the mid.

use mev_hl_client::{
    Action as VenueAction, AssetMap, CancelByCloidWire, Grouping, MIN_ORDER_NOTIONAL, Market,
    MarketSelector, OrderParams, OrderWire, build_order_wire, round_price, round_price_aggressive,
    round_size,
};
use rust_decimal::{Decimal, RoundingStrategy};
use smallvec::SmallVec;

use crate::exec::{ReqIds, UnsignedPost};
use crate::orders::{CloidAssigner, OrderManager};
use crate::strategy::Action;
use crate::types::{AssetMetaLite, Cloid, CoinId, CoinRegistry, Px};

/// Precomputed per-asset order metadata (SPEC-0010 §7, §12).
///
/// Mirrors [`AssetMetaLite`] plus an optional tick size. Hyperliquid has no
/// exchange tick size (prices are capped by significant figures), so
/// [`Self::from_market`] leaves `tick_size` as `None`; the field exists so a
/// venue or test fixture that does have one can round to it via
/// [`aggressive_limit_px`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetMeta {
    /// Wire asset id for orders.
    pub asset_id: u32,
    /// Size decimals for rounding.
    pub sz_decimals: u32,
    /// Whether the market is spot.
    pub is_spot: bool,
    /// Optional price tick size, if the venue defines one.
    pub tick_size: Option<Decimal>,
}

impl AssetMeta {
    /// Precompute from a resolved market.
    ///
    /// Delegates the wire-field extraction to [`AssetMetaLite::from_market`] so
    /// there is one source of truth for `asset_id`/`sz_decimals`/`is_spot`.
    pub fn from_market(market: &Market) -> Self {
        let lite = AssetMetaLite::from_market(market);
        Self {
            asset_id: lite.asset_id,
            sz_decimals: lite.sz_decimals,
            is_spot: lite.is_spot,
            tick_size: None,
        }
    }
}

/// Precomputed [`Market`]s indexed by [`CoinId`], for the order builder.
///
/// The engine's coin universe (a [`CoinRegistry`]) is fixed at startup, so the
/// builder can resolve an asset id and the venue rounding rules without a
/// string lookup on the hot path. `from_markets`/`from_selector` are the only
/// constructors; index `None` means the coin is not tradable in this universe.
#[derive(Debug, Clone, Default)]
pub struct AssetTable {
    markets: Vec<Option<Market>>,
}

impl AssetTable {
    /// Build a table by resolving every coin in `registry` against `map`.
    ///
    /// The vector is indexed by [`CoinId`]; a coin missing from `map` gets a
    /// `None` slot and its orders are later dropped as `MissingAsset`.
    pub fn from_markets(registry: &CoinRegistry, map: &AssetMap) -> Self {
        let markets = registry
            .iter()
            .map(|(_, coin)| map.get(coin).cloned())
            .collect();
        Self { markets }
    }

    /// Build a table from a [`MarketSelector`], reusing its asset map.
    pub fn from_selector(registry: &CoinRegistry, selector: &MarketSelector) -> Self {
        Self::from_markets(registry, selector.asset_map())
    }

    /// The resolved market for a coin.
    pub fn get(&self, coin: CoinId) -> Option<&Market> {
        self.markets.get(coin.index())?.as_ref()
    }

    /// The precomputed order metadata for a coin.
    pub fn meta(&self, coin: CoinId) -> Option<AssetMeta> {
        self.get(coin).map(AssetMeta::from_market)
    }

    /// Number of coin slots (tradable and not).
    pub fn len(&self) -> usize {
        self.markets.len()
    }

    /// Whether the table has no coin slots.
    pub fn is_empty(&self) -> bool {
        self.markets.is_empty()
    }
}

/// Why a place/cancel was not turned into a wire order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    /// The rounded notional is below [`MIN_ORDER_NOTIONAL`].
    MinNotional,
    /// The size rounds to zero or the wire builder rejected the order.
    BadSize,
    /// The coin is not in the registry or has no resolved market.
    MissingAsset,
    /// An aggressive order had no reference touch price.
    NoReferencePrice,
    /// A cancel referenced a cloid the order manager does not track.
    UnknownCloid,
    /// `Action::Modify` is not implemented in E-6 (`batchModify` unverified).
    ModifyUnsupported,
    /// `Action::PlaceGroup` is not implemented in E-6 (SPEC-0011).
    GroupUnsupported,
}

/// The unsigned posts and dropped actions produced by one iteration.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BuiltBatch {
    /// Posts to dispatch, in §12 order: cancels first, then places.
    pub posts: Vec<UnsignedPost>,
    /// Actions that could not be built, with the reason and their cloid.
    pub dropped: Vec<(Cloid, DropReason)>,
}

/// The aggressive limit for a `limit_px: None` order.
///
/// `touch` is the best **opposite** price (ask for a buy, bid for a sell). The
/// result is `touch × (1 ± bps/1e4)`, rounded in the safe direction. When
/// `meta.tick_size` is set, rounding is to that tick (up for a buy, down for a
/// sell); otherwise the raw adjusted price is returned. See
/// [`aggressive_limit_px_market`] for the venue-aware version the builder uses.
pub fn aggressive_limit_px(
    meta: &AssetMeta,
    is_buy: bool,
    touch: Px,
    max_slippage_bps: Decimal,
) -> Px {
    let raw = slippage_adjusted(touch, is_buy, max_slippage_bps);
    if raw <= Decimal::ZERO {
        return Decimal::ZERO;
    }
    match meta.tick_size {
        Some(tick) if tick > Decimal::ZERO => {
            let strategy = if is_buy {
                RoundingStrategy::ToPositiveInfinity
            } else {
                RoundingStrategy::ToNegativeInfinity
            };
            ((raw / tick).round_dp_with_strategy(0, strategy) * tick).normalize()
        }
        _ => raw,
    }
}

/// The aggressive limit for a `limit_px: None` order, rounded for `market`.
///
/// Rounds `touch × (1 ± bps/1e4)` up for a buy and down for a sell via
/// [`round_price_aggressive`], so the resulting limit stays at or beyond the
/// touch and never collapses to the mid (SPEC-0010 §12).
pub fn aggressive_limit_px_market(
    market: &Market,
    is_buy: bool,
    touch: Px,
    max_slippage_bps: Decimal,
) -> Px {
    let raw = slippage_adjusted(touch, is_buy, max_slippage_bps);
    if raw <= Decimal::ZERO {
        return Decimal::ZERO;
    }
    round_price_aggressive(market, raw, is_buy)
}

/// `touch × (1 ± bps/1e4)`; zero for a non-positive touch.
fn slippage_adjusted(touch: Px, is_buy: bool, max_slippage_bps: Decimal) -> Px {
    if touch <= Decimal::ZERO {
        return Decimal::ZERO;
    }
    let factor = if is_buy {
        Decimal::ONE + max_slippage_bps / Decimal::from(10_000)
    } else {
        Decimal::ONE - max_slippage_bps / Decimal::from(10_000)
    };
    touch * factor
}

/// Plan one iteration's approved `actions` into an unsigned [`BuiltBatch`].
///
/// Cancels are resolved through `orders` (for the coin) and `table` (for the
/// asset id); places are resolved through `registry`/`table`. `touch` returns
/// the freshest `(bid, ask)` for a coin, used only for aggressive places. Posts
/// are emitted cancels-first and all req ids come from `req_ids`.
#[allow(clippy::too_many_arguments)]
pub fn plan_iteration(
    actions: &[Action],
    registry: &CoinRegistry,
    table: &AssetTable,
    orders: &OrderManager,
    cloids: &CloidAssigner,
    touch: &dyn Fn(CoinId) -> Option<(Px, Px)>,
    max_slippage_bps: Decimal,
    req_ids: &mut ReqIds,
) -> BuiltBatch {
    let mut cancels: Vec<CancelByCloidWire> = Vec::new();
    let mut cancel_cloids: SmallVec<[Cloid; 8]> = SmallVec::new();
    let mut places: Vec<OrderWire> = Vec::new();
    let mut place_cloids: SmallVec<[Cloid; 8]> = SmallVec::new();
    let mut dropped: Vec<(Cloid, DropReason)> = Vec::new();

    for action in actions {
        match action {
            Action::Cancel { cloid } => match resolve_cancel(*cloid, table, orders) {
                Ok(wire) => {
                    cancels.push(wire);
                    cancel_cloids.push(*cloid);
                }
                Err(reason) => dropped.push((*cloid, reason)),
            },
            Action::Place(intent) => {
                match build_place(intent, registry, table, cloids, touch, max_slippage_bps) {
                    Ok((wire, cloid)) => {
                        places.push(wire);
                        place_cloids.push(cloid);
                    }
                    Err((cloid, reason)) => dropped.push((cloid, reason)),
                }
            }
            Action::Modify { cloid, .. } => {
                dropped.push((*cloid, DropReason::ModifyUnsupported));
            }
            Action::PlaceGroup(group) => {
                for leg in &group.legs {
                    dropped.push((intent_cloid(leg, cloids), DropReason::GroupUnsupported));
                }
            }
        }
    }

    let mut posts = Vec::with_capacity(2);
    if !cancels.is_empty() {
        posts.push(UnsignedPost {
            req_id: req_ids.next(),
            action: VenueAction::CancelByCloid { cancels },
            cloids: cancel_cloids,
        });
    }
    if !places.is_empty() {
        posts.push(UnsignedPost {
            req_id: req_ids.next(),
            action: VenueAction::Order {
                orders: places,
                grouping: Grouping::Na,
            },
            cloids: place_cloids,
        });
    }
    BuiltBatch { posts, dropped }
}

/// Resolve one cancel into a `cancelByCloid` entry.
fn resolve_cancel(
    cloid: Cloid,
    table: &AssetTable,
    orders: &OrderManager,
) -> Result<CancelByCloidWire, DropReason> {
    let order = orders.get(cloid).ok_or(DropReason::UnknownCloid)?;
    let market = table.get(order.coin).ok_or(DropReason::MissingAsset)?;
    Ok(CancelByCloidWire {
        asset: market.asset_id(),
        cloid: cloid.to_hex(),
    })
}

/// Build one place action's wire order, assigning a cloid if absent.
fn build_place(
    intent: &mev_strategy::OrderIntent,
    registry: &CoinRegistry,
    table: &AssetTable,
    cloids: &CloidAssigner,
    touch: &dyn Fn(CoinId) -> Option<(Px, Px)>,
    max_slippage_bps: Decimal,
) -> Result<(OrderWire, Cloid), (Cloid, DropReason)> {
    let cloid = intent_cloid(intent, cloids);
    let coin = registry
        .id(&intent.coin)
        .ok_or((cloid, DropReason::MissingAsset))?;
    let market = table.get(coin).ok_or((cloid, DropReason::MissingAsset))?;
    let is_buy = intent.side.is_buy();

    let size = round_size(market, intent.size);
    if size <= Decimal::ZERO {
        return Err((cloid, DropReason::BadSize));
    }

    let limit_px = match intent.limit_px {
        Some(px) => px,
        None => {
            let (bid, ask) = touch(coin).ok_or((cloid, DropReason::NoReferencePrice))?;
            let reference = if is_buy { ask } else { bid };
            if reference <= Decimal::ZERO {
                return Err((cloid, DropReason::NoReferencePrice));
            }
            aggressive_limit_px_market(market, is_buy, reference, max_slippage_bps)
        }
    };

    let rounded_px = round_price(market, limit_px);
    if rounded_px <= Decimal::ZERO {
        return Err((cloid, DropReason::BadSize));
    }
    if rounded_px * size < MIN_ORDER_NOTIONAL {
        return Err((cloid, DropReason::MinNotional));
    }

    let params = OrderParams {
        is_buy,
        size,
        limit_px,
        tif: intent.tif.into(),
        reduce_only: intent.reduce_only,
        cloid: Some(cloid.to_hex()),
    };
    match build_order_wire(market, &params) {
        Ok(wire) => Ok((wire, cloid)),
        Err(_) => Err((cloid, DropReason::BadSize)),
    }
}

/// The cloid for an intent: the parsed `cloid` if present, else the next fresh
/// one from `cloids`.
fn intent_cloid(intent: &mev_strategy::OrderIntent, cloids: &CloidAssigner) -> Cloid {
    intent
        .cloid
        .as_deref()
        .and_then(Cloid::from_hex)
        .unwrap_or_else(|| cloids.next())
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use mev_hl_client::AssetMap;
    use mev_hl_client::types::{AssetMeta as WireAssetMeta, Meta};
    use mev_strategy::{OrderIntent, Side, StrategyId, TimeInForce};

    use super::*;
    use crate::orders::{LiveOrder, OrderState};
    use crate::types::{CoinId, CoinRegistry, Side as EngineSide};

    fn ds(value: &str) -> Decimal {
        Decimal::from_str(value).unwrap()
    }

    fn asset_map(sz_decimals: u32) -> AssetMap {
        let mut map = AssetMap::new();
        map.insert_perp_dex(
            None,
            None,
            &Meta {
                universe: vec![WireAssetMeta {
                    name: "BTC".into(),
                    sz_decimals,
                    max_leverage: 40,
                    is_delisted: false,
                    only_isolated: false,
                }],
            },
        );
        map
    }

    fn registry() -> CoinRegistry {
        CoinRegistry::from_coins(&["BTC".into()])
    }

    fn table(sz_decimals: u32) -> AssetTable {
        AssetTable::from_markets(&registry(), &asset_map(sz_decimals))
    }

    fn cloid(n: u8) -> Cloid {
        let mut bytes = [0u8; 16];
        bytes[0] = n;
        Cloid(bytes)
    }

    fn intent(side: Side, limit_px: Option<Decimal>, size: Decimal) -> OrderIntent {
        OrderIntent {
            strategy: StrategyId::from("t"),
            coin: "BTC".into(),
            side,
            limit_px,
            size,
            tif: TimeInForce::Ioc,
            reduce_only: false,
            rationale: "test".into(),
            cloid: None,
            signal_ms: 0,
            decision_ms: 0,
        }
    }

    fn touch() -> impl Fn(CoinId) -> Option<(Px, Px)> {
        |_| Some((ds("9990"), ds("10010")))
    }

    fn resting(cloid: Cloid) -> LiveOrder {
        LiveOrder {
            cloid,
            coin: CoinId(0),
            side: EngineSide::Buy,
            px: ds("100"),
            sz: Decimal::ONE,
            filled_sz: Decimal::ZERO,
            reduce_only: false,
            strategy: StrategyId::from("t"),
            state: OrderState::Resting,
            req_id: None,
            oid: None,
        }
    }

    #[test]
    fn cancels_first_then_one_bulk_order_post() {
        let registry = registry();
        let table = table(0);
        let mut orders = OrderManager::new(1);
        let cancel_cloid = cloid(9);
        orders.insert(resting(cancel_cloid));

        let actions = vec![
            Action::Place(intent(Side::Buy, Some(ds("10000")), ds("1"))),
            Action::Place(intent(Side::Sell, Some(ds("10050")), ds("1"))),
            Action::Cancel {
                cloid: cancel_cloid,
            },
        ];
        let mut req_ids = ReqIds::new();
        let batch = plan_iteration(
            &actions,
            &registry,
            &table,
            &orders,
            &CloidAssigner::new(),
            &touch(),
            ds("10"),
            &mut req_ids,
        );

        assert!(batch.dropped.is_empty(), "{:?}", batch.dropped);
        assert_eq!(batch.posts.len(), 2);

        // Cancels first: a single cancelByCloid with only that cancel.
        match &batch.posts[0].action {
            VenueAction::CancelByCloid { cancels } => {
                assert_eq!(cancels.len(), 1);
                assert_eq!(cancels[0].cloid, cancel_cloid.to_hex());
            }
            other => panic!("first post should be cancelByCloid, got {other:?}"),
        }
        // Then one bulk order carrying both places, in action order.
        match &batch.posts[1].action {
            VenueAction::Order { orders, grouping } => {
                assert_eq!(*grouping, Grouping::Na);
                assert_eq!(orders.len(), 2);
                assert!(orders[0].b);
                assert!(!orders[1].b);
            }
            other => panic!("second post should be order, got {other:?}"),
        }
        // Req ids are assigned cancels-first.
        assert_eq!(batch.posts[0].req_id, 0);
        assert_eq!(batch.posts[1].req_id, 1);
        assert_eq!(batch.posts[1].cloids.len(), 2);
    }

    #[test]
    fn aggressive_buy_rounds_up_and_never_mid() {
        let registry = registry();
        let table = table(0);
        let orders = OrderManager::new(1);
        let actions = vec![Action::Place(intent(Side::Buy, None, ds("1")))];
        let mut req_ids = ReqIds::new();
        let batch = plan_iteration(
            &actions,
            &registry,
            &table,
            &orders,
            &CloidAssigner::new(),
            &touch(),
            ds("10"),
            &mut req_ids,
        );

        let px = order_price(&batch);
        assert!(px >= ds("10010"), "buy limit {px} below the ask");
        assert!(px > ds("10000"), "buy limit {px} is not above the mid");
    }

    #[test]
    fn aggressive_sell_rounds_down_and_never_mid() {
        let registry = registry();
        let table = table(0);
        let orders = OrderManager::new(1);
        let actions = vec![Action::Place(intent(Side::Sell, None, ds("1")))];
        let mut req_ids = ReqIds::new();
        let batch = plan_iteration(
            &actions,
            &registry,
            &table,
            &orders,
            &CloidAssigner::new(),
            &touch(),
            ds("10"),
            &mut req_ids,
        );

        let px = order_price(&batch);
        assert!(px <= ds("9990"), "sell limit {px} above the bid");
        assert!(px < ds("10000"), "sell limit {px} is not below the mid");
    }

    #[test]
    fn min_notional_place_is_dropped_and_emits_no_post() {
        let registry = registry();
        let table = table(2);
        let orders = OrderManager::new(1);
        // price 100 × size 0.05 = $5 < $10.
        let actions = vec![Action::Place(intent(
            Side::Buy,
            Some(ds("100")),
            ds("0.05"),
        ))];
        let mut req_ids = ReqIds::new();
        let batch = plan_iteration(
            &actions,
            &registry,
            &table,
            &orders,
            &CloidAssigner::new(),
            &touch(),
            ds("10"),
            &mut req_ids,
        );

        assert!(batch.posts.is_empty());
        assert_eq!(batch.dropped.len(), 1);
        assert_eq!(batch.dropped[0].1, DropReason::MinNotional);
    }

    #[test]
    fn place_without_cloid_gets_one_carried_in_post_order() {
        let registry = registry();
        let table = table(2);
        let orders = OrderManager::new(1);
        let actions = vec![
            Action::Place(intent(Side::Buy, Some(ds("10000")), ds("0.01"))),
            Action::Place(intent(Side::Sell, Some(ds("10000")), ds("0.01"))),
        ];
        let mut req_ids = ReqIds::new();
        let batch = plan_iteration(
            &actions,
            &registry,
            &table,
            &orders,
            &CloidAssigner::new(),
            &touch(),
            ds("10"),
            &mut req_ids,
        );

        assert_eq!(batch.posts.len(), 1);
        let post = &batch.posts[0];
        assert_eq!(post.cloids.len(), 2);
        let VenueAction::Order { orders, .. } = &post.action else {
            panic!("expected order post");
        };
        assert_eq!(
            orders[0].c.as_deref(),
            Some(post.cloids[0].to_hex().as_str())
        );
        assert_eq!(
            orders[1].c.as_deref(),
            Some(post.cloids[1].to_hex().as_str())
        );
        assert_ne!(post.cloids[0], post.cloids[1]);
    }

    #[test]
    fn unknown_cancel_cloid_is_dropped() {
        let registry = registry();
        let table = table(0);
        let orders = OrderManager::new(1);
        let ghost = cloid(42);
        let actions = vec![Action::Cancel { cloid: ghost }];
        let mut req_ids = ReqIds::new();
        let batch = plan_iteration(
            &actions,
            &registry,
            &table,
            &orders,
            &CloidAssigner::new(),
            &touch(),
            ds("10"),
            &mut req_ids,
        );

        assert!(batch.posts.is_empty());
        assert_eq!(batch.dropped, vec![(ghost, DropReason::UnknownCloid)]);
    }

    #[test]
    fn modify_and_group_are_dropped_as_unsupported() {
        let registry = registry();
        let table = table(0);
        let orders = OrderManager::new(1);
        let modify_cloid = cloid(1);
        let actions = vec![
            Action::Modify {
                cloid: modify_cloid,
                px: ds("100"),
                sz: ds("1"),
            },
            Action::PlaceGroup(crate::strategy::GroupIntent {
                legs: vec![intent(Side::Buy, Some(ds("100")), ds("1"))],
                all_or_none: true,
            }),
        ];
        let mut req_ids = ReqIds::new();
        let batch = plan_iteration(
            &actions,
            &registry,
            &table,
            &orders,
            &CloidAssigner::new(),
            &touch(),
            ds("10"),
            &mut req_ids,
        );

        assert!(batch.posts.is_empty());
        assert_eq!(
            batch.dropped[0],
            (modify_cloid, DropReason::ModifyUnsupported)
        );
        assert_eq!(batch.dropped[1].1, DropReason::GroupUnsupported);
    }

    #[test]
    fn aggressive_price_without_touch_is_dropped() {
        let registry = registry();
        let table = table(0);
        let orders = OrderManager::new(1);
        let actions = vec![Action::Place(intent(Side::Buy, None, ds("1")))];
        let no_touch = |_: CoinId| None;
        let mut req_ids = ReqIds::new();
        let batch = plan_iteration(
            &actions,
            &registry,
            &table,
            &orders,
            &CloidAssigner::new(),
            &no_touch,
            ds("10"),
            &mut req_ids,
        );

        assert!(batch.posts.is_empty());
        assert_eq!(batch.dropped[0].1, DropReason::NoReferencePrice);
    }

    #[test]
    fn table_meta_reuses_wire_ids() {
        let meta = table(0).meta(CoinId(0)).unwrap();
        assert_eq!(meta.asset_id, 0);
        assert_eq!(meta.sz_decimals, 0);
        assert!(!meta.is_spot);
        assert!(meta.tick_size.is_none());
        assert!(table(0).get(CoinId(9)).is_none());
        assert_eq!(table(0).len(), 1);
        assert!(!table(0).is_empty());
    }

    /// The wire limit price of the single order in a batch.
    fn order_price(batch: &BuiltBatch) -> Decimal {
        assert_eq!(batch.posts.len(), 1, "{:?}", batch.dropped);
        let VenueAction::Order { orders, .. } = &batch.posts[0].action else {
            panic!("expected order post");
        };
        Decimal::from_str(&orders[0].p).unwrap()
    }
}
