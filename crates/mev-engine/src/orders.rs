//! Order manager and per-order state machine (SPEC-0010 §10, task E-5).
//!
//! Every order the engine sends is tracked by its [`Cloid`]. The manager owns
//! the state machine, the request-id → cloid routing used to apply post acks,
//! and an incremental per-coin worst-case in-flight notional that risk consumes
//! (SPEC-0010 §10, §11). All operations are synchronous and bounded:
//! `BTreeMap` lookups, no scans of the order book, and a [`SmallVec`] batch per
//! request so a post ack applies without allocating.
//!
//! Terminal states are sticky: once an order is `Filled`/`Cancelled`/`Rejected`
//! a later, stale venue report cannot revive or re-classify it. This is what
//! makes the `fill`-before-`ack` and `cancel`-races-`fill` races deterministic.

use std::collections::BTreeMap;

use mev_hl_client::RejectReason;
use mev_strategy::StrategyId;
use rust_decimal::Decimal;
use smallvec::SmallVec;

use crate::types::{Cloid, CoinId, Px, Side, Sz, VenueOrderStatus};

/// The lifecycle state of a live order (SPEC-0010 §10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderState {
    /// Accepted by risk and handed to exec; no venue response yet.
    PendingNew,
    /// Acknowledged and working on the book.
    Resting,
    /// Working on the book with a partial fill.
    PartiallyFilled,
    /// A cancel was sent and is awaiting its response.
    PendingCancel,
    /// A modify was sent and is awaiting its response.
    PendingModify,
    /// Fully filled (terminal).
    Filled,
    /// Cancelled (terminal).
    Cancelled,
    /// Rejected by the venue, with a typed reason (terminal).
    Rejected(RejectReason),
    /// Exec disconnected or timed out with no ack; resolved by reconciliation.
    Unknown,
}

impl OrderState {
    /// Whether the order has reached a terminal state.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            OrderState::Filled | OrderState::Cancelled | OrderState::Rejected(_)
        )
    }

    /// Whether the order still exposes risk at the venue.
    ///
    /// Working orders count toward worst-case in-flight notional: they may still
    /// fill in full (SPEC-0010 §10).
    pub fn is_working(self) -> bool {
        matches!(
            self,
            OrderState::PendingNew
                | OrderState::Resting
                | OrderState::PartiallyFilled
                | OrderState::PendingCancel
                | OrderState::PendingModify
                | OrderState::Unknown
        )
    }

    /// Whether the order's outcome is unknown and needs reconciliation.
    pub fn is_unknown(self) -> bool {
        matches!(self, OrderState::Unknown)
    }
}

/// A live order tracked by the engine (SPEC-0010 §7, §10).
#[derive(Debug, Clone, PartialEq)]
pub struct LiveOrder {
    /// Client order id assigned by the engine.
    pub cloid: Cloid,
    /// Coin the order trades.
    pub coin: CoinId,
    /// Side.
    pub side: Side,
    /// Current limit price.
    pub px: Px,
    /// Original size.
    pub sz: Sz,
    /// Size filled so far.
    pub filled_sz: Sz,
    /// Whether the order is reduce-only.
    pub reduce_only: bool,
    /// Strategy that owns the order.
    pub strategy: StrategyId,
    /// Current lifecycle state.
    pub state: OrderState,
    /// Request id, set when the action is handed to exec.
    pub req_id: Option<u64>,
    /// Venue order id, once a post ack or order update reports it.
    pub oid: Option<u64>,
}

impl LiveOrder {
    /// Remaining size to fill, floored at zero.
    pub fn remaining(&self) -> Sz {
        (self.sz - self.filled_sz).max(Decimal::ZERO)
    }

    /// Worst-case notional still exposed (full remaining size fills).
    pub fn remaining_notional(&self) -> Decimal {
        self.remaining() * self.px
    }
}

/// Tracks every live order and the worst-case in-flight notional per coin.
#[derive(Debug, Default)]
pub struct OrderManager {
    orders: BTreeMap<Cloid, LiveOrder>,
    by_req: BTreeMap<u64, SmallVec<[Cloid; 8]>>,
    /// Venue order id → client order id, so a fill can be mapped to its order.
    by_oid: BTreeMap<u64, Cloid>,
    /// Incremental per-coin worst-case in-flight notional; indexes by `CoinId`.
    pending_notional: Vec<Decimal>,
    coin_count: usize,
    /// Incremental count of working orders (`state.is_working()`).
    working_count: usize,
    /// Incremental count of terminal orders still retained for delivery.
    terminal_count: usize,
    /// Incremental count of `Unknown` orders (avoids a scan per market event).
    unknown_count: usize,
}

impl OrderManager {
    /// Build a manager sized for `coin_count` interned coins.
    pub fn new(coin_count: usize) -> Self {
        Self {
            orders: BTreeMap::new(),
            by_req: BTreeMap::new(),
            by_oid: BTreeMap::new(),
            pending_notional: vec![Decimal::ZERO; coin_count],
            coin_count,
            working_count: 0,
            terminal_count: 0,
            unknown_count: 0,
        }
    }

    /// Number of interned coins this manager tracks exposure for.
    pub fn coin_count(&self) -> usize {
        self.coin_count
    }

    /// Insert an order, adding its remaining notional to the coin's in-flight
    /// exposure. Returns the order's cloid.
    pub fn insert(&mut self, order: LiveOrder) -> Cloid {
        let cloid = order.cloid;
        let coin = order.coin.index();
        let working = order.state.is_working();
        let terminal = order.state.is_terminal();
        let unknown = order.state.is_unknown();
        let contribution = order.remaining_notional();
        if let Some(old) = self.orders.insert(cloid, order) {
            if old.state.is_working()
                && let Some(slot) = self.pending_notional.get_mut(coin)
            {
                *slot -= old.remaining_notional();
            }
            if old.state.is_working() {
                self.working_count -= 1;
            }
            if old.state.is_terminal() {
                self.terminal_count -= 1;
            }
            if old.state.is_unknown() {
                self.unknown_count -= 1;
            }
        }
        if working && let Some(slot) = self.pending_notional.get_mut(coin) {
            *slot += contribution;
        }
        if working {
            self.working_count += 1;
        }
        if terminal {
            self.terminal_count += 1;
        }
        if unknown {
            self.unknown_count += 1;
        }
        cloid
    }

    /// Look up an order.
    pub fn get(&self, cloid: Cloid) -> Option<&LiveOrder> {
        self.orders.get(&cloid)
    }

    /// Mutable access to an order.
    pub fn get_mut(&mut self, cloid: Cloid) -> Option<&mut LiveOrder> {
        self.orders.get_mut(&cloid)
    }

    /// Remove an order, removing its in-flight exposure if it was working.
    pub fn remove(&mut self, cloid: Cloid) -> Option<LiveOrder> {
        let order = self.orders.remove(&cloid)?;
        if order.state.is_working() {
            if let Some(slot) = self.pending_notional.get_mut(order.coin.index()) {
                *slot -= order.remaining_notional();
            }
            self.working_count -= 1;
        }
        if order.state.is_terminal() {
            self.terminal_count -= 1;
        }
        if order.state.is_unknown() {
            self.unknown_count -= 1;
        }
        if let Some(oid) = order.oid {
            self.by_oid.remove(&oid);
        }
        Some(order)
    }

    /// Number of tracked orders.
    pub fn len(&self) -> usize {
        self.orders.len()
    }

    /// Number of terminal orders still retained for delivery.
    pub fn terminal_count(&self) -> usize {
        self.terminal_count
    }

    /// Whether no orders are tracked.
    pub fn is_empty(&self) -> bool {
        self.orders.is_empty()
    }

    /// Iterate every tracked order in `Cloid` order (deterministic).
    pub fn iter(&self) -> impl Iterator<Item = &LiveOrder> {
        self.orders.values()
    }

    /// Iterate the orders that still expose risk at the venue.
    pub fn working(&self) -> impl Iterator<Item = &LiveOrder> {
        self.orders
            .values()
            .filter(|order| order.state.is_working())
    }

    /// Number of orders that might still be resting at the venue, for the
    /// dead-man switch.
    ///
    /// Counts **every working order** — `PendingNew`, `Resting`,
    /// `PartiallyFilled`, `PendingCancel`, `PendingModify`, and `Unknown` — not
    /// just confirmed-resting ones: a lost reply (`Unknown`) or a not-yet-acked
    /// place may be on the book, and the scheduleCancel must cover it. This is
    /// `O(1)` (an incremental counter), so the loop can read it every iteration.
    pub fn resting_count(&self) -> usize {
        self.working_count
    }

    /// Whether any tracked order has an unknown outcome awaiting reconciliation.
    ///
    /// O(1): an incremental counter kept exact by [`Self::insert`],
    /// [`Self::remove`], and [`Self::set_state`].
    pub fn has_unknown(&self) -> bool {
        self.unknown_count > 0
    }

    /// Record the venue oid for a client order id, for fill mapping.
    ///
    /// Also sets the order's `oid` when it is tracked. Oids are never zero on
    /// the wire, so `0` is ignored.
    pub fn record_oid(&mut self, oid: u64, cloid: Cloid) {
        if oid == 0 {
            return;
        }
        self.by_oid.insert(oid, cloid);
        if let Some(order) = self.orders.get_mut(&cloid) {
            order.oid = Some(oid);
        }
    }

    /// The client order id for a venue oid, if it is still tracked or recent.
    pub fn cloid_for_oid(&self, oid: u64) -> Option<Cloid> {
        self.by_oid.get(&oid).copied()
    }

    /// Whether any order on `coin` has an unknown outcome.
    pub fn unknown_on_coin(&self, coin: CoinId) -> bool {
        self.orders
            .values()
            .any(|order| order.coin == coin && order.state.is_unknown())
    }

    /// Remove terminal (`Filled`/`Cancelled`/`Rejected`) orders so the map does
    /// not grow for the lifetime of a session. Returns the number removed.
    ///
    /// Called only when terminal orders exist, so the scan is off the steady
    /// path. Exposure is already zero for terminal orders.
    pub fn prune_terminal(&mut self) -> usize {
        if self.terminal_count == 0 {
            return 0;
        }
        let before = self.orders.len();
        self.orders.retain(|_, order| !order.state.is_terminal());
        self.terminal_count = 0;
        // Drop the oid index entries for pruned orders; a late fill for them is
        // delivered from the dispatcher's short-lived recent-route cache.
        self.by_oid
            .retain(|_, cloid| self.orders.contains_key(cloid));
        before - self.orders.len()
    }

    /// The worst-case in-flight notional for a coin.
    ///
    /// Confirmed positions are *not* included; that is `AccountState`.
    pub fn pending_notional(&self, coin: CoinId) -> Decimal {
        self.pending_notional
            .get(coin.index())
            .copied()
            .unwrap_or(Decimal::ZERO)
    }

    /// Move an order to `state`, keeping per-coin in-flight exposure exact.
    ///
    /// Transitions out of a terminal state are ignored (terminal is sticky), as
    /// are transitions the §10 table does not allow, so a stale ack cannot
    /// downgrade a partially filled or filled order. The exposure update is
    /// incremental: subtract the old contribution, set the state, add the new.
    pub fn set_state(&mut self, cloid: Cloid, state: OrderState) {
        let Some(order) = self.orders.get_mut(&cloid) else {
            return;
        };
        if !transition_allowed(order.state, state) {
            return;
        }
        let coin = order.coin.index();
        let remaining = order.remaining_notional();
        let was_working = order.state.is_working();
        let now_working = state.is_working();
        let was_terminal = order.state.is_terminal();
        let was_unknown = order.state.is_unknown();
        let now_unknown = state.is_unknown();
        order.state = state;
        if was_unknown != now_unknown {
            if now_unknown {
                self.unknown_count += 1;
            } else {
                self.unknown_count -= 1;
            }
        }
        if was_working != now_working
            && let Some(slot) = self.pending_notional.get_mut(coin)
        {
            if now_working {
                *slot += remaining;
            } else {
                *slot -= remaining;
            }
        }
        if was_working != now_working {
            if now_working {
                self.working_count += 1;
            } else {
                self.working_count -= 1;
            }
        }
        // Terminal is sticky, so a terminal order never becomes non-terminal.
        if !was_terminal && state.is_terminal() {
            self.terminal_count += 1;
        }
    }

    /// Register the cloids that a request will carry, in order.
    pub fn assign_req(&mut self, req_id: u64, cloids: &[Cloid]) {
        self.by_req.insert(req_id, cloids.iter().copied().collect());
    }

    /// Apply a post ack's per-order statuses to the request's cloids in order.
    ///
    /// The request entry is removed once resolved. Statuses beyond the
    /// registered cloids (or a missing request) are ignored.
    pub fn on_post_ack(&mut self, req_id: u64, statuses: &[VenueOrderStatus]) {
        let Some(cloids) = self.by_req.remove(&req_id) else {
            return;
        };
        for (cloid, status) in cloids.iter().zip(statuses.iter()) {
            self.apply_status(*cloid, *status, None);
        }
    }

    /// Apply an order update: monotonic filled size plus a status transition.
    ///
    /// A fill reported on an `Unknown`/`PendingNew` order resolves it (a fill
    /// can race its ack).
    pub fn on_order_update(
        &mut self,
        cloid: Cloid,
        status: VenueOrderStatus,
        filled_sz: Sz,
        _avg_px: Px,
    ) {
        self.apply_status(cloid, status, Some(filled_sz));
    }

    /// Record an incremental fill, moving the order to `PartiallyFilled` or
    /// `Filled`. A `None` cloid is ignored.
    pub fn on_fill(&mut self, cloid: Option<Cloid>, sz: Sz) {
        let Some(cloid) = cloid else {
            return;
        };
        let Some(order) = self.orders.get(&cloid) else {
            return;
        };
        let new_filled = (order.filled_sz + sz).min(order.sz);
        self.set_filled(cloid, new_filled);
        let full = self
            .orders
            .get(&cloid)
            .is_some_and(|order| order.filled_sz >= order.sz);
        self.set_state(
            cloid,
            if full {
                OrderState::Filled
            } else {
                OrderState::PartiallyFilled
            },
        );
    }

    /// Mark an order as awaiting its cancel response.
    pub fn mark_pending_cancel(&mut self, cloid: Cloid) {
        self.set_state(cloid, OrderState::PendingCancel);
    }

    /// Mark an order as awaiting its modify response.
    pub fn mark_pending_modify(&mut self, cloid: Cloid) {
        self.set_state(cloid, OrderState::PendingModify);
    }

    /// Map a venue status to a state, if it carries one.
    fn state_for_status(status: VenueOrderStatus) -> Option<OrderState> {
        Some(match status {
            VenueOrderStatus::Resting => OrderState::Resting,
            VenueOrderStatus::Filled => OrderState::Filled,
            VenueOrderStatus::PartiallyFilled => OrderState::PartiallyFilled,
            VenueOrderStatus::Cancelled => OrderState::Cancelled,
            VenueOrderStatus::Rejected => OrderState::Rejected(RejectReason::Unknown),
            VenueOrderStatus::Other => return None,
        })
    }

    /// Update the filled size monotonically (clamped to the order size),
    /// adjusting in-flight exposure when the order is working.
    fn set_filled(&mut self, cloid: Cloid, filled: Sz) {
        let Some(order) = self.orders.get_mut(&cloid) else {
            return;
        };
        let new_filled = order.filled_sz.max(filled).min(order.sz);
        if new_filled == order.filled_sz {
            return;
        }
        let coin = order.coin.index();
        let px = order.px;
        let working = order.state.is_working();
        let old_filled = order.filled_sz;
        order.filled_sz = new_filled;
        if working && let Some(slot) = self.pending_notional.get_mut(coin) {
            *slot -= (new_filled - old_filled) * px;
        }
    }

    /// Update filled size (when known) and apply a status transition.
    fn apply_status(&mut self, cloid: Cloid, status: VenueOrderStatus, filled: Option<Sz>) {
        let total = self.orders.get(&cloid).map(|order| order.sz);
        match Self::state_for_status(status) {
            Some(state) => {
                let target = if status == VenueOrderStatus::Filled {
                    total
                } else {
                    filled
                };
                if let Some(target) = target {
                    self.set_filled(cloid, target);
                }
                self.set_state(cloid, state);
            }
            None => {
                if let Some(target) = filled {
                    self.set_filled(cloid, target);
                }
            }
        }
    }
}

/// Whether the §10 state machine allows `from` → `to`.
///
/// Terminal states are sticky; `Unknown` resolves to any state reported by
/// reconciliation. `PartiallyFilled` may not fall back to `Resting`, so a
/// stale ack cannot erase a fill already seen.
fn transition_allowed(from: OrderState, to: OrderState) -> bool {
    if from == to {
        return true;
    }
    if from.is_terminal() {
        return false;
    }
    match from {
        OrderState::PendingNew => matches!(
            to,
            OrderState::Resting
                | OrderState::PartiallyFilled
                | OrderState::Filled
                | OrderState::Cancelled
                | OrderState::Rejected(_)
                | OrderState::Unknown
        ),
        OrderState::Resting => matches!(
            to,
            OrderState::PartiallyFilled
                | OrderState::Filled
                | OrderState::PendingCancel
                | OrderState::PendingModify
                | OrderState::Cancelled
                | OrderState::Unknown
        ),
        OrderState::PartiallyFilled => matches!(
            to,
            OrderState::Filled
                | OrderState::PendingCancel
                | OrderState::PendingModify
                | OrderState::Cancelled
                | OrderState::Unknown
        ),
        OrderState::PendingCancel | OrderState::PendingModify => matches!(
            to,
            OrderState::Resting
                | OrderState::PartiallyFilled
                | OrderState::Filled
                | OrderState::Cancelled
                | OrderState::Unknown
        ),
        OrderState::Unknown => true,
        OrderState::Filled | OrderState::Cancelled | OrderState::Rejected(_) => false,
    }
}

/// Assigns unique engine-side cloids by wrapping [`mev_hl_client::CloidFactory`].
#[derive(Debug, Default)]
pub struct CloidAssigner {
    factory: mev_hl_client::CloidFactory,
}

impl CloidAssigner {
    /// Create an assigner with a fresh per-process random prefix.
    pub fn new() -> Self {
        Self {
            factory: mev_hl_client::CloidFactory::new(),
        }
    }

    /// The next unique cloid.
    ///
    /// The factory always emits a `0x` + 32-hex-char string, so parsing is
    /// infallible; this is a pure in-process path with no I/O.
    pub fn next(&self) -> Cloid {
        Cloid::from_hex(&self.factory.next()).expect("CloidFactory emits 0x + 32 hex chars")
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;
    use crate::types::Side;

    fn ds(value: &str) -> Decimal {
        Decimal::from_str(value).unwrap()
    }

    fn cloid(n: u8) -> Cloid {
        let mut bytes = [0u8; 16];
        bytes[0] = n;
        Cloid(bytes)
    }

    fn live(cloid: Cloid, coin: u16, px: &str, sz: &str, state: OrderState) -> LiveOrder {
        LiveOrder {
            cloid,
            coin: CoinId(coin),
            side: Side::Buy,
            px: ds(px),
            sz: ds(sz),
            filled_sz: Decimal::ZERO,
            reduce_only: false,
            strategy: StrategyId::from("t"),
            state,
            req_id: None,
            oid: None,
        }
    }

    #[test]
    fn state_predicates_match_spec_10() {
        use OrderState::*;
        for state in [
            PendingNew,
            Resting,
            PartiallyFilled,
            PendingCancel,
            PendingModify,
            Unknown,
        ] {
            assert!(state.is_working(), "{state:?} should work");
            assert!(!state.is_terminal(), "{state:?} should not be terminal");
        }
        for state in [Filled, Cancelled, Rejected(RejectReason::TickRejected)] {
            assert!(state.is_terminal(), "{state:?} should be terminal");
            assert!(!state.is_working(), "{state:?} should not work");
        }
        assert!(Unknown.is_unknown());
        assert!(!Resting.is_unknown());
        assert!(!Filled.is_unknown());
    }

    #[test]
    fn spec_10_transitions_apply() {
        use OrderState::*;
        let cases: &[(OrderState, OrderState)] = &[
            (PendingNew, Resting),
            (PendingNew, Filled),
            (PendingNew, PartiallyFilled),
            (PendingNew, Rejected(RejectReason::TickRejected)),
            (PendingNew, Unknown),
            (Resting, Filled),
            (Resting, PendingCancel),
            (Resting, PendingModify),
            (Resting, Cancelled),
            (PartiallyFilled, Filled),
            (PartiallyFilled, PendingCancel),
            (PartiallyFilled, PendingModify),
            (PartiallyFilled, Cancelled),
            (PendingCancel, Cancelled),
            (PendingCancel, Resting),
            (PendingCancel, Filled),
            (PendingCancel, Unknown),
            (PendingModify, Cancelled),
            (PendingModify, Resting),
            (PendingModify, Filled),
            (PendingModify, Unknown),
            (Unknown, Resting),
            (Unknown, PartiallyFilled),
            (Unknown, Filled),
        ];
        for (from, to) in cases {
            let mut manager = OrderManager::new(1);
            let c = cloid(1);
            manager.insert(live(c, 0, "10", "2", *from));
            manager.set_state(c, *to);
            assert_eq!(
                manager.get(c).unwrap().state,
                *to,
                "{from:?} -> {to:?} should apply"
            );
        }
    }

    #[test]
    fn terminal_states_are_sticky() {
        for terminal in [
            OrderState::Filled,
            OrderState::Cancelled,
            OrderState::Rejected(RejectReason::Unknown),
        ] {
            let mut manager = OrderManager::new(1);
            let c = cloid(1);
            manager.insert(live(c, 0, "10", "2", terminal));
            manager.set_state(c, OrderState::Resting);
            assert_eq!(manager.get(c).unwrap().state, terminal, "{terminal:?}");
            manager.set_state(c, OrderState::Unknown);
            assert_eq!(manager.get(c).unwrap().state, terminal, "{terminal:?}");
        }
    }

    #[test]
    fn fill_before_ack_resolves_and_survives_ack() {
        let mut manager = OrderManager::new(1);

        let partial = cloid(1);
        manager.insert(live(partial, 0, "10", "2", OrderState::PendingNew));
        manager.assign_req(9, &[partial]);
        manager.on_fill(Some(partial), ds("1"));
        assert_eq!(
            manager.get(partial).unwrap().state,
            OrderState::PartiallyFilled
        );
        // A stale `resting` ack must not downgrade the fill.
        manager.on_post_ack(9, &[VenueOrderStatus::Resting]);
        assert_eq!(
            manager.get(partial).unwrap().state,
            OrderState::PartiallyFilled
        );

        let full = cloid(2);
        manager.insert(live(full, 0, "10", "2", OrderState::PendingNew));
        manager.assign_req(10, &[full]);
        manager.on_fill(Some(full), ds("2"));
        assert_eq!(manager.get(full).unwrap().state, OrderState::Filled);
        manager.on_post_ack(10, &[VenueOrderStatus::Resting]);
        assert_eq!(manager.get(full).unwrap().state, OrderState::Filled);
    }

    #[test]
    fn cancel_races_fill_and_fill_wins() {
        let mut manager = OrderManager::new(1);
        let c = cloid(1);
        manager.insert(live(c, 0, "10", "2", OrderState::Resting));

        manager.mark_pending_cancel(c);
        assert_eq!(manager.get(c).unwrap().state, OrderState::PendingCancel);

        manager.on_fill(Some(c), ds("2"));
        assert_eq!(manager.get(c).unwrap().state, OrderState::Filled);

        // The cancel response arrives after the fill: terminal wins.
        manager.on_order_update(c, VenueOrderStatus::Cancelled, ds("2"), ds("10"));
        assert_eq!(manager.get(c).unwrap().state, OrderState::Filled);
    }

    #[test]
    fn pending_notional_is_exact_after_each_transition() {
        let mut manager = OrderManager::new(2);
        let a = cloid(1);
        let b = cloid(2);

        manager.insert(live(a, 0, "10", "2", OrderState::Resting)); // 20
        manager.insert(live(b, 0, "5", "3", OrderState::Resting)); // +15
        assert_eq!(manager.pending_notional(CoinId(0)), ds("35"));
        assert_eq!(manager.pending_notional(CoinId(1)), Decimal::ZERO);

        manager.set_state(a, OrderState::Filled);
        assert_eq!(manager.pending_notional(CoinId(0)), ds("15"));

        manager.remove(b);
        assert_eq!(manager.pending_notional(CoinId(0)), Decimal::ZERO);

        // Cancel drops the contribution.
        manager.insert(live(a, 0, "10", "2", OrderState::Resting));
        assert_eq!(manager.pending_notional(CoinId(0)), ds("20"));
        manager.set_state(a, OrderState::Cancelled);
        assert_eq!(manager.pending_notional(CoinId(0)), Decimal::ZERO);

        // A partial fill reduces exposure to the remaining size.
        let c = cloid(3);
        manager.insert(live(c, 0, "10", "2", OrderState::Resting));
        manager.on_fill(Some(c), ds("1"));
        assert_eq!(manager.get(c).unwrap().state, OrderState::PartiallyFilled);
        assert_eq!(manager.pending_notional(CoinId(0)), ds("10"));
        manager.on_fill(Some(c), ds("1"));
        assert_eq!(manager.get(c).unwrap().state, OrderState::Filled);
        assert_eq!(manager.pending_notional(CoinId(0)), Decimal::ZERO);
    }

    #[test]
    fn unknown_on_coin_is_per_coin() {
        let mut manager = OrderManager::new(2);
        let unknown_coin0 = cloid(1);
        let resting_coin0 = cloid(2);
        let unknown_coin1 = cloid(3);
        manager.insert(live(unknown_coin0, 0, "10", "1", OrderState::Unknown));
        manager.insert(live(resting_coin0, 0, "10", "1", OrderState::Resting));
        manager.insert(live(unknown_coin1, 1, "10", "1", OrderState::Unknown));

        assert!(manager.unknown_on_coin(CoinId(0)));
        assert!(manager.unknown_on_coin(CoinId(1)));
        manager.set_state(unknown_coin0, OrderState::Resting);
        assert!(!manager.unknown_on_coin(CoinId(0)));
        assert!(manager.unknown_on_coin(CoinId(1)));
    }

    #[test]
    fn on_post_ack_routes_statuses_to_cloids_in_order() {
        let mut manager = OrderManager::new(1);
        let c1 = cloid(1);
        let c2 = cloid(2);
        manager.insert(live(c1, 0, "10", "2", OrderState::PendingNew));
        manager.insert(live(c2, 0, "10", "4", OrderState::PendingNew));

        // Only the assigned request carries cloids; an unknown req is ignored.
        manager.on_post_ack(7, &[VenueOrderStatus::Resting]);
        assert_eq!(manager.get(c1).unwrap().state, OrderState::PendingNew);

        manager.assign_req(8, &[c1, c2]);
        manager.on_post_ack(8, &[VenueOrderStatus::Resting, VenueOrderStatus::Filled]);
        assert_eq!(manager.get(c1).unwrap().state, OrderState::Resting);
        assert_eq!(manager.get(c2).unwrap().state, OrderState::Filled);
        assert_eq!(manager.get(c2).unwrap().filled_sz, ds("4"));
    }

    #[test]
    fn assign_req_and_resting_count_track_working_orders() {
        let mut manager = OrderManager::new(1);
        let c1 = cloid(1);
        let c2 = cloid(2);
        let c3 = cloid(3);
        manager.insert(live(c1, 0, "10", "1", OrderState::PendingNew));
        manager.insert(live(c2, 0, "10", "1", OrderState::PendingNew));
        manager.insert(live(c3, 0, "10", "1", OrderState::PendingNew));
        manager.assign_req(1, &[c1, c2]);

        manager.on_post_ack(1, &[VenueOrderStatus::Resting, VenueOrderStatus::Resting]);
        // The dead-man count includes every working order: c1/c2 resting and c3
        // still pending.
        assert_eq!(manager.resting_count(), 3);

        // c3 never got a request; a repeated ack is a no-op.
        manager.on_post_ack(1, &[VenueOrderStatus::Resting]);
        assert_eq!(manager.resting_count(), 3);

        // A partially filled order is still working, a filled one is terminal.
        manager.on_order_update(c1, VenueOrderStatus::PartiallyFilled, ds("0.4"), ds("10"));
        manager.on_order_update(c2, VenueOrderStatus::Filled, ds("1"), ds("10"));
        assert_eq!(manager.resting_count(), 2);
        assert_eq!(manager.len(), 3);
        // c1 (PartiallyFilled) and c3 (PendingNew) still work; c2 is terminal.
        assert_eq!(manager.working().count(), 2);
        assert!(!manager.is_empty());
        assert_eq!(manager.iter().count(), 3);
        assert_eq!(manager.coin_count(), 1);
    }

    #[test]
    fn prune_terminal_removes_finished_orders_and_keeps_exposure() {
        let mut manager = OrderManager::new(1);
        let c1 = cloid(1);
        let c2 = cloid(2);
        manager.insert(live(c1, 0, "10", "1", OrderState::PendingNew));
        manager.insert(live(c2, 0, "10", "1", OrderState::PendingNew));
        assert_eq!(manager.resting_count(), 2);
        assert_eq!(manager.pending_notional(CoinId(0)), ds("20"));

        manager.set_state(c1, OrderState::Filled);
        assert_eq!(manager.prune_terminal(), 1);
        assert_eq!(manager.len(), 1);
        assert_eq!(manager.resting_count(), 1);
        // c1's exposure was released on the terminal transition.
        assert_eq!(manager.pending_notional(CoinId(0)), ds("10"));
        // A second prune is a no-op once no terminal orders remain.
        assert_eq!(manager.prune_terminal(), 0);
    }

    #[test]
    fn unknown_orders_are_working_and_queryable() {
        let mut manager = OrderManager::new(1);
        let c1 = cloid(1);
        manager.insert(live(c1, 0, "10", "1", OrderState::PendingNew));
        manager.set_state(c1, OrderState::Unknown);
        assert!(manager.has_unknown());
        assert!(manager.unknown_on_coin(CoinId(0)));
        // An unknown order still counts for the dead-man switch and exposure.
        assert_eq!(manager.resting_count(), 1);
        assert_eq!(manager.pending_notional(CoinId(0)), ds("10"));
    }

    #[test]
    fn cloid_assigner_emits_distinct_parseable_cloids() {
        let assigner = CloidAssigner::new();
        let first = assigner.next();
        let second = assigner.next();
        assert_ne!(first, second);
        assert_eq!(Cloid::from_hex(&first.to_hex()), Some(first));
        assert_eq!(Cloid::from_hex(&second.to_hex()), Some(second));
        assert!(first.to_hex().starts_with("0x"));
        assert_eq!(first.to_hex().len(), 34);
    }
}
