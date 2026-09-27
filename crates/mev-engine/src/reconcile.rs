//! Account reconciliation (SPEC-0010 §15, task E-8).
//!
//! The H-3 account stream is the source of truth for own orders and fills. The
//! REST reconciler is the backstop: every 30 s, after a reconnect, and on demand
//! after `Unknown` order outcomes, it snapshots the venue and diffs it against
//! local state. This module is **pure**: it performs no I/O and reads no clock;
//! callers pass the fetched venue data and a `now_ns` stamp. That keeps the
//! drift logic deterministic and lets the engine's reconcile timer stay off the
//! order path (SPEC-0010 §16).
//!
//! Design notes:
//! - [`VenueSnapshot`] is a local adapter. It carries the fields the engine
//!   needs to diff (positions, open orders, account value) without changing the
//!   shared [`AccountSnapshot`] event payload (owned by E-1).
//! - [`Reconciler::diff`] returns a [`SmallVec`] of [`Drift`]s; the venue is
//!   authoritative, so [`Reconciler::apply_drift`] corrects local state to match.
//! - A repeated drift of the same [`DriftKind`] three times within 10 minutes
//!   trips the breaker (SPEC-0010 §15).

use std::collections::BTreeMap;

use mev_hl_client::{ClearinghouseState, OpenOrder, OrderResolution, OrderStatusResponse};
use mev_strategy::StrategyId;
use rust_decimal::Decimal;
use smallvec::SmallVec;

use crate::orders::{LiveOrder, OrderManager, OrderState};
use crate::state::AccountState;
use crate::types::{AccountSnapshot, Cloid, CoinId, CoinRegistry, Px, Side, Sz, VenueOrderStatus};

/// The strategy id recorded on orders reconstructed purely from the venue.
const RECONCILE_STRATEGY: &str = "reconcile";

/// How long the repetition breaker looks back, in nanoseconds (10 minutes).
const DRIFT_WINDOW_NS: i64 = 600 * 1_000_000_000;

/// The kind of drift, used for the per-kind repetition breaker (SPEC-0010 §15).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriftKind {
    /// A locally working order is not present on the venue.
    OrderMissingOnVenue,
    /// A venue order is not tracked locally.
    OrderMissingLocally,
    /// A tracked order disagrees on filled size or state.
    OrderMismatch,
    /// A perp position disagrees.
    Position,
    /// Account value or margin disagrees.
    AccountValue,
}

impl DriftKind {
    /// Number of variants, for the fixed-size breaker history table.
    pub const COUNT: usize = 5;

    const fn index(self) -> usize {
        match self {
            DriftKind::OrderMissingOnVenue => 0,
            DriftKind::OrderMissingLocally => 1,
            DriftKind::OrderMismatch => 2,
            DriftKind::Position => 3,
            DriftKind::AccountValue => 4,
        }
    }
}

/// A venue open order, mapped to engine ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VenueOrder {
    /// Client order id.
    pub cloid: Cloid,
    /// Venue order id.
    pub oid: u64,
    /// Coin.
    pub coin: CoinId,
    /// Side.
    pub side: Side,
    /// Limit price.
    pub px: Px,
    /// Remaining size on the book.
    pub remaining: Sz,
    /// Size already filled (`orig_sz - remaining`).
    pub filled: Sz,
    /// Whether the order is reduce-only.
    pub reduce_only: bool,
}

/// A venue perp position, mapped to an engine [`CoinId`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VenuePosition {
    /// Coin.
    pub coin: CoinId,
    /// Signed size (positive long, negative short).
    pub szi: Sz,
}

/// A reconciled view of the account as reported by REST (SPEC-0010 §15).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VenueSnapshot {
    /// Receive stamp in nanoseconds.
    pub ts_ns: i64,
    /// Perp account value in USD.
    pub account_value: Decimal,
    /// Margin currently used.
    pub margin_used: Decimal,
    /// Perp positions, one entry per held coin.
    pub positions: Vec<VenuePosition>,
    /// Open (resting) orders.
    pub open_orders: SmallVec<[VenueOrder; 16]>,
}

impl VenueSnapshot {
    /// Project the shared fields onto the `AccountUpdate::Reconcile` payload.
    pub fn account_snapshot(&self) -> AccountSnapshot {
        AccountSnapshot {
            account_value: self.account_value,
            margin_used: self.margin_used,
        }
    }
}

/// One difference between local state and the venue snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Drift {
    /// A locally working order is not present on the venue; treated as terminal.
    OrderMissingOnVenue {
        /// The order.
        cloid: Cloid,
    },
    /// A venue order is not tracked locally; the venue is authoritative.
    OrderMissingLocally {
        /// The venue order to reconstruct locally.
        order: VenueOrder,
    },
    /// A tracked order disagrees on state or filled size.
    OrderMismatch {
        /// The order.
        cloid: Cloid,
        /// Local lifecycle state.
        local_state: OrderState,
        /// State implied by the venue snapshot.
        venue_state: OrderState,
        /// Local filled size.
        local_filled: Sz,
        /// Venue filled size.
        venue_filled: Sz,
    },
    /// A perp position disagrees.
    PositionMismatch {
        /// Coin.
        coin: CoinId,
        /// Local signed size.
        local: Sz,
        /// Venue signed size.
        venue: Sz,
    },
    /// Account value or margin disagrees.
    AccountValueMismatch {
        /// Local account value.
        local: Decimal,
        /// Venue account value.
        venue: Decimal,
        /// Local margin used.
        local_margin: Decimal,
        /// Venue margin used.
        venue_margin: Decimal,
    },
}

impl Drift {
    /// The breaker bucket this drift belongs to.
    pub fn kind(&self) -> DriftKind {
        match self {
            Drift::OrderMissingOnVenue { .. } => DriftKind::OrderMissingOnVenue,
            Drift::OrderMissingLocally { .. } => DriftKind::OrderMissingLocally,
            Drift::OrderMismatch { .. } => DriftKind::OrderMismatch,
            Drift::PositionMismatch { .. } => DriftKind::Position,
            Drift::AccountValueMismatch { .. } => DriftKind::AccountValue,
        }
    }
}

/// How an `Unknown` order resolved against a venue `orderStatus` query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnknownResolution {
    /// No such order is tracked locally.
    NotTracked,
    /// The venue could not resolve it; the order stays `Unknown`.
    Unresolved,
    /// The order resolved to a terminal or resting state.
    Resolved(OrderState),
}

/// The REST reconciler: builds venue snapshots, diffs them, and tracks drift
/// repetition. Pure and synchronous; owns no I/O.
#[derive(Debug)]
pub struct Reconciler {
    corrections: u64,
    trips: [SmallVec<[i64; 4]>; DriftKind::COUNT],
    tripped: bool,
}

impl Default for Reconciler {
    fn default() -> Self {
        Self {
            corrections: 0,
            trips: std::array::from_fn(|_| SmallVec::new()),
            tripped: false,
        }
    }
}

impl Reconciler {
    /// Map REST account data into a [`VenueSnapshot`].
    ///
    /// Coins outside the engine's interned universe and orders without a
    /// parseable cloid are skipped: the engine cannot track them, and they are
    /// outside the traded universe.
    pub fn build_snapshot(
        clearinghouse: &ClearinghouseState,
        open_orders: &[OpenOrder],
        registry: &CoinRegistry,
        now_ns: i64,
    ) -> VenueSnapshot {
        let mut positions = Vec::with_capacity(clearinghouse.asset_positions.len());
        for entry in &clearinghouse.asset_positions {
            if let Some(coin) = registry.id(&entry.position.coin) {
                positions.push(VenuePosition {
                    coin,
                    szi: entry.position.szi,
                });
            }
        }

        let mut orders: SmallVec<[VenueOrder; 16]> = SmallVec::new();
        for order in open_orders {
            let Some(cloid) = order.cloid.as_deref().and_then(Cloid::from_hex) else {
                continue;
            };
            let Some(coin) = registry.id(&order.coin) else {
                continue;
            };
            let remaining = order.sz;
            let filled = (order.orig_sz - remaining).max(Decimal::ZERO);
            orders.push(VenueOrder {
                cloid,
                oid: order.oid,
                coin,
                side: if order.is_buy() {
                    Side::Buy
                } else {
                    Side::Sell
                },
                px: order.limit_px,
                remaining,
                filled,
                reduce_only: order.reduce_only,
            });
        }

        VenueSnapshot {
            ts_ns: now_ns,
            account_value: clearinghouse.margin_summary.account_value,
            margin_used: clearinghouse.margin_summary.total_margin_used,
            positions,
            open_orders: orders,
        }
    }

    /// Diff local order and account state against the venue snapshot.
    ///
    /// Only locally *confirmed* orders (`Resting`/`PartiallyFilled`) are expected
    /// on the venue; in-flight orders (`PendingNew`/`PendingCancel`/…) are left
    /// for their own acks. `Unknown` orders are compared too, so an open-orders
    /// snapshot can also resolve them.
    pub fn diff(
        &self,
        local: &OrderManager,
        local_account: &AccountState,
        venue: &VenueSnapshot,
    ) -> SmallVec<[Drift; 16]> {
        let mut drifts: SmallVec<[Drift; 16]> = SmallVec::new();

        let mut venue_by_cloid: BTreeMap<Cloid, &VenueOrder> = BTreeMap::new();
        for order in &venue.open_orders {
            venue_by_cloid.insert(order.cloid, order);
        }

        for order in local.iter() {
            if matches!(
                order.state,
                OrderState::Resting | OrderState::PartiallyFilled
            ) && !venue_by_cloid.contains_key(&order.cloid)
            {
                drifts.push(Drift::OrderMissingOnVenue { cloid: order.cloid });
            }
        }

        for venue_order in &venue.open_orders {
            match local.get(venue_order.cloid) {
                None => drifts.push(Drift::OrderMissingLocally {
                    order: *venue_order,
                }),
                Some(local_order) => {
                    let venue_state = if venue_order.filled > Decimal::ZERO {
                        OrderState::PartiallyFilled
                    } else {
                        OrderState::Resting
                    };
                    let comparable = matches!(
                        local_order.state,
                        OrderState::Resting | OrderState::PartiallyFilled | OrderState::Unknown
                    );
                    if comparable
                        && (local_order.state != venue_state
                            || local_order.filled_sz != venue_order.filled)
                    {
                        drifts.push(Drift::OrderMismatch {
                            cloid: venue_order.cloid,
                            local_state: local_order.state,
                            venue_state,
                            local_filled: local_order.filled_sz,
                            venue_filled: venue_order.filled,
                        });
                    }
                }
            }
        }

        // Union of venue positions and local non-zero positions.
        let mut venue_pos: BTreeMap<CoinId, Sz> = venue
            .positions
            .iter()
            .map(|position| (position.coin, position.szi))
            .collect();
        let mut coins: Vec<CoinId> = venue_pos.keys().copied().collect();
        for (index, szi) in local_account.positions.iter().enumerate() {
            if *szi != Decimal::ZERO {
                coins.push(CoinId(index as u16));
            }
        }
        coins.sort_unstable();
        coins.dedup();
        for coin in coins {
            let local = local_account.position_szi(coin);
            let venue = venue_pos.remove(&coin).unwrap_or(Decimal::ZERO);
            if local != venue {
                drifts.push(Drift::PositionMismatch { coin, local, venue });
            }
        }

        if local_account.account_value != venue.account_value
            || local_account.margin_used != venue.margin_used
        {
            drifts.push(Drift::AccountValueMismatch {
                local: local_account.account_value,
                venue: venue.account_value,
                local_margin: local_account.margin_used,
                venue_margin: venue.margin_used,
            });
        }

        drifts
    }

    /// Correct local order and account state to match the venue.
    ///
    /// Returns the number of corrections applied and accumulates
    /// [`Self::corrections`]. A working order absent from the venue snapshot is
    /// marked `Cancelled` (terminal, so its in-flight exposure is released);
    /// a later `orderStatus` query can refine a fill from the same helper.
    pub fn apply_drift(
        &mut self,
        orders: &mut OrderManager,
        local_account: &mut AccountState,
        drifts: &[Drift],
    ) -> u64 {
        let mut corrected = 0u64;
        for drift in drifts {
            match drift {
                Drift::OrderMissingOnVenue { cloid } => {
                    let needs_fix = orders
                        .get(*cloid)
                        .is_some_and(|order| !order.state.is_terminal());
                    if needs_fix {
                        orders.set_state(*cloid, OrderState::Cancelled);
                        corrected += 1;
                    }
                }
                Drift::OrderMissingLocally { order } => {
                    orders.insert(LiveOrder {
                        cloid: order.cloid,
                        coin: order.coin,
                        side: order.side,
                        px: order.px,
                        sz: order.remaining + order.filled,
                        filled_sz: order.filled,
                        reduce_only: order.reduce_only,
                        strategy: StrategyId::from(RECONCILE_STRATEGY),
                        state: if order.filled > Decimal::ZERO {
                            OrderState::PartiallyFilled
                        } else {
                            OrderState::Resting
                        },
                        req_id: None,
                        oid: Some(order.oid),
                    });
                    orders.record_oid(order.oid, order.cloid);
                    corrected += 1;
                }
                Drift::OrderMismatch {
                    cloid,
                    venue_state,
                    venue_filled,
                    ..
                } => {
                    if let Some(status) = status_for_state(*venue_state) {
                        orders.on_order_update(*cloid, status, *venue_filled, Decimal::ZERO);
                        corrected += 1;
                    }
                }
                Drift::PositionMismatch { coin, venue, .. } => {
                    local_account.set_position_szi(*coin, *venue);
                    corrected += 1;
                }
                Drift::AccountValueMismatch {
                    venue,
                    venue_margin,
                    ..
                } => {
                    local_account.account_value = *venue;
                    local_account.margin_used = *venue_margin;
                    corrected += 1;
                }
            }
        }
        self.corrections += corrected;
        corrected
    }

    /// Record a drift occurrence and report whether the breaker trips.
    ///
    /// Returns `true` when the same [`DriftKind`] has now drifted three times
    /// within 10 minutes (SPEC-0010 §15).
    pub fn record_drift(&mut self, kind: DriftKind, now_ns: i64) -> bool {
        let history = &mut self.trips[kind.index()];
        history.push(now_ns);
        while let Some(&front) = history.first() {
            if now_ns - front > DRIFT_WINDOW_NS {
                history.remove(0);
            } else {
                break;
            }
        }
        if history.len() >= 3 {
            self.tripped = true;
        }
        self.tripped
    }

    /// Total corrections applied since construction.
    pub fn corrections(&self) -> u64 {
        self.corrections
    }

    /// Whether the drift-repetition breaker has tripped.
    pub fn should_trip(&self) -> bool {
        self.tripped
    }
}

/// Map a venue `orderStatus` response to the engine status and filled size for
/// an order, or `None` when the venue cannot resolve it (`unknownOid` or an
/// unmodelled state).
///
/// Shared by [`resolve_unknown`] and the live exec path, so a response is mapped
/// one way everywhere.
pub fn order_status_update(status: &OrderStatusResponse) -> Option<(VenueOrderStatus, Decimal)> {
    let filled = status
        .order
        .as_ref()
        .and_then(|wrapper| wrapper.order.as_ref())
        .map(|order| (order.orig_sz - order.sz).max(Decimal::ZERO))
        .unwrap_or(Decimal::ZERO);
    let mapped = match status.resolution() {
        OrderResolution::Resting | OrderResolution::Triggered => VenueOrderStatus::Resting,
        OrderResolution::Filled => VenueOrderStatus::Filled,
        OrderResolution::Cancelled => VenueOrderStatus::Cancelled,
        OrderResolution::Rejected => VenueOrderStatus::Rejected,
        OrderResolution::NotFound | OrderResolution::Other(_) => return None,
    };
    Some((mapped, filled))
}

/// Resolve an `Unknown` order using a venue `orderStatus` response.
///
/// The caller performs the I/O; this applies the response through the order
/// manager's transition guard. A response the venue cannot resolve (`unknownOid`
/// or an unmodelled state) leaves the order `Unknown` for a later retry.
pub fn resolve_unknown(
    orders: &mut OrderManager,
    cloid: Cloid,
    status: &OrderStatusResponse,
) -> UnknownResolution {
    let Some(order) = orders.get(cloid) else {
        return UnknownResolution::NotTracked;
    };
    if !order.state.is_unknown() {
        return UnknownResolution::Resolved(order.state);
    }
    let Some((venue_status, filled)) = order_status_update(status) else {
        return UnknownResolution::Unresolved;
    };
    orders.on_order_update(cloid, venue_status, filled, Decimal::ZERO);
    match orders.get(cloid) {
        Some(order) => UnknownResolution::Resolved(order.state),
        None => UnknownResolution::NotTracked,
    }
}

/// Map a lifecycle state back to the venue status that best describes it.
fn status_for_state(state: OrderState) -> Option<VenueOrderStatus> {
    Some(match state {
        OrderState::PendingNew | OrderState::Resting => VenueOrderStatus::Resting,
        OrderState::PartiallyFilled => VenueOrderStatus::PartiallyFilled,
        OrderState::Filled => VenueOrderStatus::Filled,
        OrderState::PendingCancel | OrderState::PendingModify | OrderState::Cancelled => {
            VenueOrderStatus::Cancelled
        }
        OrderState::Rejected(_) => VenueOrderStatus::Rejected,
        OrderState::Unknown => return None,
    })
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use mev_hl_client::{AssetPosition, MarginSummary, OrderStatusOrder, Position};

    use super::*;

    fn ds(value: &str) -> Decimal {
        Decimal::from_str(value).unwrap()
    }

    fn cloid(n: u8) -> Cloid {
        let mut bytes = [0u8; 16];
        bytes[0] = n;
        Cloid(bytes)
    }

    fn cloid_hex(n: u8) -> String {
        cloid(n).to_hex()
    }

    fn registry() -> CoinRegistry {
        CoinRegistry::from_coins(&["BTC".into(), "ETH".into()])
    }

    fn live(c: Cloid, coin: u16, px: &str, sz: &str, filled: &str, state: OrderState) -> LiveOrder {
        LiveOrder {
            cloid: c,
            coin: CoinId(coin),
            side: Side::Buy,
            px: ds(px),
            sz: ds(sz),
            filled_sz: ds(filled),
            reduce_only: false,
            strategy: StrategyId::from("t"),
            state,
            req_id: None,
            oid: None,
        }
    }

    fn venue_order(c: Cloid, coin: u16, remaining: &str, filled: &str) -> VenueOrder {
        VenueOrder {
            cloid: c,
            oid: 7,
            coin: CoinId(coin),
            side: Side::Buy,
            px: ds("100"),
            remaining: ds(remaining),
            filled: ds(filled),
            reduce_only: false,
        }
    }

    fn snapshot(orders: Vec<VenueOrder>, positions: Vec<VenuePosition>) -> VenueSnapshot {
        VenueSnapshot {
            account_value: Decimal::ZERO,
            margin_used: Decimal::ZERO,
            open_orders: orders.into_iter().collect(),
            positions,
            ..Default::default()
        }
    }

    fn open_order(cloid_hex: &str, orig: &str, remaining: &str) -> OpenOrder {
        OpenOrder {
            coin: "BTC".into(),
            oid: 7,
            side: "B".into(),
            limit_px: ds("100"),
            sz: ds(remaining),
            orig_sz: ds(orig),
            timestamp: 1,
            cloid: Some(cloid_hex.into()),
            reduce_only: false,
        }
    }

    fn position(coin: &str, szi: &str) -> AssetPosition {
        AssetPosition {
            position: Position {
                coin: coin.into(),
                szi: ds(szi),
                entry_px: None,
                position_value: Decimal::ZERO,
                unrealized_pnl: Decimal::ZERO,
                return_on_equity: Decimal::ZERO,
                liquidation_px: None,
                margin_used: Decimal::ZERO,
                leverage: None,
            },
        }
    }

    fn clearinghouse(
        value: &str,
        margin: &str,
        positions: Vec<AssetPosition>,
    ) -> ClearinghouseState {
        let summary = MarginSummary {
            account_value: ds(value),
            total_ntl_pos: Decimal::ZERO,
            total_raw_usd: Decimal::ZERO,
            total_margin_used: ds(margin),
        };
        ClearinghouseState {
            margin_summary: summary.clone(),
            cross_margin_summary: summary,
            withdrawable: Decimal::ZERO,
            asset_positions: positions,
        }
    }

    fn status_response(state: &str, orig: &str, remaining: &str) -> OrderStatusResponse {
        OrderStatusResponse {
            status: "order".into(),
            order: Some(OrderStatusOrder {
                order: Some(OpenOrder {
                    coin: "BTC".into(),
                    oid: 7,
                    side: "B".into(),
                    limit_px: ds("100"),
                    sz: ds(remaining),
                    orig_sz: ds(orig),
                    timestamp: 1,
                    cloid: None,
                    reduce_only: false,
                }),
                status: state.into(),
                status_timestamp: 1,
            }),
        }
    }

    #[test]
    fn build_snapshot_maps_positions_orders_and_value() {
        let clearing = clearinghouse("1000", "100", vec![position("BTC", "1.5")]);
        let orders = vec![open_order(&cloid_hex(1), "2", "1")];
        let snap = Reconciler::build_snapshot(&clearing, &orders, &registry(), 42);

        assert_eq!(snap.ts_ns, 42);
        assert_eq!(snap.account_value, ds("1000"));
        assert_eq!(snap.margin_used, ds("100"));
        assert_eq!(
            snap.positions,
            vec![VenuePosition {
                coin: CoinId(0),
                szi: ds("1.5")
            }]
        );
        assert_eq!(snap.open_orders.len(), 1);
        let order = snap.open_orders[0];
        assert_eq!(order.cloid, cloid(1));
        assert_eq!(order.coin, CoinId(0));
        assert_eq!(order.remaining, ds("1"));
        assert_eq!(order.filled, ds("1"));
        assert_eq!(snap.account_snapshot().account_value, ds("1000"));
    }

    #[test]
    fn build_snapshot_skips_uninterned_coins_and_bad_cloids() {
        let clearing = clearinghouse("1", "0", vec![position("DOGE", "5")]);
        let orders = vec![
            open_order("not-a-cloid", "1", "1"),
            OpenOrder {
                coin: "DOGE".into(),
                ..open_order(&cloid_hex(2), "1", "1")
            },
        ];
        let snap = Reconciler::build_snapshot(&clearing, &orders, &registry(), 0);
        assert!(snap.positions.is_empty());
        assert!(snap.open_orders.is_empty());
    }

    #[test]
    fn detects_orders_missing_on_venue_and_locally() {
        let mut local = OrderManager::new(2);
        let a = cloid(1);
        local.insert(live(a, 0, "100", "2", "0", OrderState::Resting));

        // Venue has a different order and not `a`.
        let b = cloid(2);
        let venue = snapshot(vec![venue_order(b, 1, "1", "0")], vec![]);
        let account = AccountState::new(2);

        let drifts = Reconciler::default().diff(&local, &account, &venue);
        assert!(
            drifts
                .iter()
                .any(|d| matches!(d, Drift::OrderMissingOnVenue { cloid: c } if *c == a))
        );
        assert!(
            drifts
                .iter()
                .any(|d| matches!(d, Drift::OrderMissingLocally { order } if order.cloid == b))
        );
    }

    #[test]
    fn detects_size_state_and_position_mismatch() {
        let mut local = OrderManager::new(2);
        let a = cloid(1);
        local.insert(live(a, 0, "100", "2", "0", OrderState::Resting));
        let mut account = AccountState::new(2);
        account.set_position_szi(CoinId(0), ds("1"));
        account.account_value = ds("1000");
        account.margin_used = ds("10");

        let venue = VenueSnapshot {
            account_value: ds("1200"),
            margin_used: ds("20"),
            open_orders: vec![venue_order(a, 0, "1", "1")].into_iter().collect(),
            positions: vec![VenuePosition {
                coin: CoinId(0),
                szi: ds("2"),
            }],
            ..Default::default()
        };

        let drifts = Reconciler::default().diff(&local, &account, &venue);
        assert!(drifts.iter().any(|d| matches!(
            d,
            Drift::OrderMismatch { cloid: c, venue_state, venue_filled, .. }
                if *c == a && *venue_state == OrderState::PartiallyFilled && *venue_filled == ds("1")
        )));
        assert!(drifts.iter().any(|d| matches!(
            d,
            Drift::PositionMismatch { coin, venue, .. } if *coin == CoinId(0) && *venue == ds("2")
        )));
        assert!(
            drifts
                .iter()
                .any(|d| matches!(d, Drift::AccountValueMismatch { .. }))
        );
    }

    #[test]
    fn apply_drift_makes_local_match_venue() {
        let mut reconciler = Reconciler::default();
        let mut local = OrderManager::new(2);
        let stale = cloid(1);
        local.insert(live(stale, 0, "100", "2", "0", OrderState::Resting));
        let mut account = AccountState::new(2);
        account.set_position_szi(CoinId(0), ds("1"));
        account.account_value = ds("1000");
        account.margin_used = ds("10");

        // Venue: no orders, flat, different value; and one order local lacks.
        let missing = cloid(2);
        let venue = VenueSnapshot {
            account_value: ds("1200"),
            margin_used: ds("20"),
            open_orders: vec![venue_order(missing, 1, "3", "0")]
                .into_iter()
                .collect(),
            positions: vec![],
            ..Default::default()
        };

        let drifts = reconciler.diff(&local, &account, &venue);
        assert!(!drifts.is_empty());
        let corrected = reconciler.apply_drift(&mut local, &mut account, &drifts);
        assert!(corrected > 0);
        assert_eq!(reconciler.corrections(), corrected);

        // Local now tracks the venue order and matches on every dimension.
        assert_eq!(
            local.get(stale).map(|o| o.state),
            Some(OrderState::Cancelled)
        );
        let tracked = local.get(missing).expect("venue order reconstructed");
        assert_eq!(tracked.coin, CoinId(1));
        assert_eq!(tracked.state, OrderState::Resting);
        assert_eq!(account.position_szi(CoinId(0)), Decimal::ZERO);
        assert_eq!(account.account_value, ds("1200"));
        assert_eq!(account.margin_used, ds("20"));

        assert!(reconciler.diff(&local, &account, &venue).is_empty());
    }

    #[test]
    fn order_mismatch_correction_applies_venue_fill() {
        let mut reconciler = Reconciler::default();
        let mut local = OrderManager::new(1);
        let a = cloid(1);
        local.insert(live(a, 0, "100", "2", "0", OrderState::Resting));
        let account = AccountState::new(1);
        let venue = snapshot(vec![venue_order(a, 0, "1", "1")], vec![]);

        let drifts = reconciler.diff(&local, &account, &venue);
        assert_eq!(drifts.len(), 1);
        reconciler.apply_drift(&mut local, &mut AccountState::new(1), &drifts);
        let order = local.get(a).unwrap();
        assert_eq!(order.filled_sz, ds("1"));
        assert_eq!(order.state, OrderState::PartiallyFilled);
    }

    #[test]
    fn breaker_trips_on_third_drift_within_window() {
        let mut reconciler = Reconciler::default();
        assert!(!reconciler.record_drift(DriftKind::Position, 0));
        assert!(!reconciler.record_drift(DriftKind::Position, 1));
        assert!(reconciler.record_drift(DriftKind::Position, 2));
        assert!(reconciler.should_trip());
    }

    #[test]
    fn breaker_does_not_trip_for_drifts_spread_beyond_window() {
        let minute = 60 * 1_000_000_000i64;
        let mut reconciler = Reconciler::default();
        assert!(!reconciler.record_drift(DriftKind::OrderMismatch, 0));
        assert!(!reconciler.record_drift(DriftKind::OrderMismatch, 6 * minute));
        // The first occurrence ages out; only two remain in the window.
        assert!(!reconciler.record_drift(DriftKind::OrderMismatch, 12 * minute));
        assert!(!reconciler.should_trip());
    }

    #[test]
    fn breaker_counts_kinds_independently() {
        let mut reconciler = Reconciler::default();
        reconciler.record_drift(DriftKind::Position, 0);
        reconciler.record_drift(DriftKind::AccountValue, 0);
        reconciler.record_drift(DriftKind::Position, 0);
        assert!(!reconciler.record_drift(DriftKind::AccountValue, 0));
        assert!(!reconciler.should_trip());
    }

    #[test]
    fn unknown_resolves_to_filled_cancelled_and_resting() {
        // Filled.
        let mut orders = OrderManager::new(1);
        let a = cloid(1);
        orders.insert(live(a, 0, "100", "2", "0", OrderState::Unknown));
        let resolution = resolve_unknown(&mut orders, a, &status_response("filled", "2", "0"));
        assert_eq!(resolution, UnknownResolution::Resolved(OrderState::Filled));
        assert_eq!(orders.get(a).unwrap().filled_sz, ds("2"));

        // Cancelled.
        let mut orders = OrderManager::new(1);
        orders.insert(live(a, 0, "100", "2", "0", OrderState::Unknown));
        let resolution = resolve_unknown(&mut orders, a, &status_response("canceled", "2", "2"));
        assert_eq!(
            resolution,
            UnknownResolution::Resolved(OrderState::Cancelled)
        );

        // Still resting.
        let mut orders = OrderManager::new(1);
        orders.insert(live(a, 0, "100", "2", "0", OrderState::Unknown));
        let resolution = resolve_unknown(&mut orders, a, &status_response("open", "2", "2"));
        assert_eq!(resolution, UnknownResolution::Resolved(OrderState::Resting));
    }

    #[test]
    fn unknown_stays_unknown_for_not_found_or_untracked() {
        let mut orders = OrderManager::new(1);
        let a = cloid(1);
        orders.insert(live(a, 0, "100", "2", "0", OrderState::Unknown));

        let not_found = OrderStatusResponse {
            status: "unknownOid".into(),
            order: None,
        };
        assert_eq!(
            resolve_unknown(&mut orders, a, &not_found),
            UnknownResolution::Unresolved
        );
        assert_eq!(orders.get(a).unwrap().state, OrderState::Unknown);

        assert_eq!(
            resolve_unknown(&mut orders, cloid(9), &status_response("filled", "2", "0")),
            UnknownResolution::NotTracked
        );
    }

    #[test]
    fn resolve_unknown_leaves_resolved_orders_alone() {
        let mut orders = OrderManager::new(1);
        let a = cloid(1);
        orders.insert(live(a, 0, "100", "2", "0", OrderState::Filled));
        let resolution = resolve_unknown(&mut orders, a, &status_response("open", "2", "2"));
        assert_eq!(resolution, UnknownResolution::Resolved(OrderState::Filled));
        assert_eq!(orders.get(a).unwrap().state, OrderState::Filled);
    }
}
