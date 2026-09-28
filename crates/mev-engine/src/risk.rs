//! Hot-path risk gate (SPEC-0010 §11, task E-9), with the SPEC-0004 K-2
//! (exposure including in-flight orders) and K-3/K-5 (kill switch, rate budget)
//! pieces.
//!
//! [`RiskGate::check`] is the synchronous, allocation-free gate every
//! risk-increasing [`Action`] passes before it reaches the order builder. It is
//! deliberately O(1): it reads incremental exposure already maintained by the
//! [`OrderManager`] instead of scanning live orders, and it never touches the
//! clock, the network, or a lock.
//!
//! Check order (first failure wins), from SPEC-0010 §11:
//!
//! 1. kill switch (K-3)
//! 2. breaker state
//! 3. stale coin (feed gap / staleness)
//! 4. unknown orders on the coin
//! 5. rate budget (K-5)
//! 6. per-order notional
//! 7. per-coin projected exposure (`confirmed + in-flight`)
//! 8. account margin utilization
//! 9. min-notional / rounding validity
//!
//! **Cancels are never blocked by risk**, except by the rate budget's hard
//! floor (they reduce exposure). The concrete cancel-all collector lives here
//! because [`Cloid`]/[`OrderManager`] are engine types; the flag mechanism
//! itself is in `mev-risk::kill`.
//!
//! SPEC-0011 group exposure (net and worst single-leg) is **not** implemented:
//! it needs the group/leg types that do not exist yet (recorded as an open
//! item, not guessed; see `specs/SPEC-0004-risk-portfolio-accounting.md` K-2).

use std::cell::Cell;

use mev_core::config::{RateBudgetSettings, RiskSettings};
use mev_hl_client::MIN_ORDER_NOTIONAL;
use mev_risk::Decision;
use mev_risk::kill::KillSwitch;
use rust_decimal::Decimal;

use crate::builder::AssetMeta;
use crate::orders::OrderManager;
use crate::state::{AccountState, MarketSlot};
use crate::strategy::Action;
use crate::types::{Cloid, CoinId, Px, Sz};

/// Why a risk check refused an action. Named so callers can meter the exact
/// rejection (SPEC-0004 §6: "the reason is metric-tagged").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RiskReason {
    /// The kill switch is active (SPEC-0004 K-3).
    KillSwitch,
    /// A circuit breaker is tripped; the label identifies it.
    Breaker(&'static str),
    /// The coin's market inputs are stale or inside a feed gap.
    StaleCoin,
    /// At least one order on the coin has an unknown outcome.
    UnknownOrders,
    /// The rate budget is below the place minimum (or the hard floor for a
    /// cancel).
    RateBudget,
    /// No reference price is available to value an aggressive order.
    NoReferencePrice,
    /// The order size is zero or negative.
    NonPositiveSize,
    /// The projected per-coin exposure is already at or over its cap.
    PositionExposure,
    /// Margin utilization is over its cap.
    MarginUtilization,
    /// The (possibly resized) order notional is below the venue minimum.
    MinNotional,
    /// The price is not aligned to the asset's tick size.
    TickInvalid,
    /// A modify referenced a cloid the order manager does not track.
    UnknownCloid,
    /// Multi-leg groups are not implemented yet (SPEC-0011).
    GroupUnsupported,
}

impl std::fmt::Display for RiskReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RiskReason::KillSwitch => f.write_str("kill switch active"),
            RiskReason::Breaker(label) => write!(f, "breaker tripped: {label}"),
            RiskReason::StaleCoin => f.write_str("stale coin"),
            RiskReason::UnknownOrders => f.write_str("unknown orders on coin"),
            RiskReason::RateBudget => f.write_str("rate budget exhausted"),
            RiskReason::NoReferencePrice => f.write_str("no reference price"),
            RiskReason::NonPositiveSize => f.write_str("non-positive size"),
            RiskReason::PositionExposure => f.write_str("position exposure cap reached"),
            RiskReason::MarginUtilization => f.write_str("margin utilization cap exceeded"),
            RiskReason::MinNotional => f.write_str("below minimum notional"),
            RiskReason::TickInvalid => f.write_str("price not aligned to tick"),
            RiskReason::UnknownCloid => f.write_str("unknown cloid"),
            RiskReason::GroupUnsupported => f.write_str("multi-leg groups unsupported"),
        }
    }
}

/// Which token bucket a rate-budget operation consumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateKind {
    /// Hyperliquid's IP weight (shared across every client on the host).
    IpWeight,
    /// The address's order budget (from `userRateLimit`).
    Address,
}

/// Token-bucket configuration for the engine's share of the API rate budget
/// (SPEC-0010 §12, SPEC-0004 K-5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateBudgetConfig {
    /// The engine's own IP-weight capacity/refill per minute.
    pub ip_per_min: u32,
    /// Address order capacity/refill per minute.
    pub address_per_min: u32,
    /// Minimum IP weight below which new places are rejected.
    pub ip_min: u32,
    /// Minimum address budget below which new places are rejected.
    pub address_min: u32,
    /// Below this, even cancels are rejected.
    pub hard_floor: u32,
}

impl RateBudgetConfig {
    /// Build from the resolved config file settings.
    pub fn from_settings(settings: &RateBudgetSettings) -> Self {
        let ip = settings
            .engine_ip_share
            .min(settings.ip_weight_per_min)
            .max(1);
        let address = settings.address_per_min.max(1);
        Self {
            ip_per_min: ip,
            address_per_min: address,
            ip_min: settings.ip_weight_min,
            address_min: settings.address_min,
            hard_floor: settings.hard_floor,
        }
    }
}

impl Default for RateBudgetConfig {
    fn default() -> Self {
        Self::from_settings(&RateBudgetSettings::default())
    }
}

/// A small fixed-size token bucket pair for the hot path.
///
/// `try_consume`/`remaining` take `&self`: the buckets use [`Cell`] so a
/// read-only [`RiskCtx`] can still consume. This keeps the budget where the
/// engine already owns it and avoids a second mutable borrow through the gate.
/// The gate calls [`RateBudget::refill`] once per action with the event time.
#[derive(Debug)]
pub struct RateBudget {
    config: RateBudgetConfig,
    ip: Bucket,
    address: Bucket,
}

#[derive(Debug)]
struct Bucket {
    capacity: u32,
    remaining: Cell<u32>,
    refill_per_min: u32,
    last_ms: Cell<u64>,
}

impl Bucket {
    fn new(capacity_per_min: u32) -> Self {
        Self {
            capacity: capacity_per_min,
            remaining: Cell::new(capacity_per_min),
            refill_per_min: capacity_per_min,
            last_ms: Cell::new(0),
        }
    }

    fn refill(&self, now_ms: u64) {
        let last = self.last_ms.get();
        if now_ms <= last || self.refill_per_min == 0 {
            return;
        }
        let elapsed = (now_ms - last) as u128;
        let added = elapsed * self.refill_per_min as u128 / 60_000;
        if added == 0 {
            return;
        }
        if added >= self.capacity as u128 {
            // A long gap (including the first call against a fresh 0 anchor)
            // fills the bucket and discards the excess credit.
            self.remaining.set(self.capacity);
            self.last_ms.set(now_ms);
            return;
        }
        let added = added as u32;
        let tokens = (self.remaining.get() + added).min(self.capacity);
        self.remaining.set(tokens);
        // Advance by exactly the time that produced `added` tokens, keeping the
        // sub-token remainder so refill is not lost to integer division.
        let consumed_ms = (added as u128 * 60_000 / self.refill_per_min as u128) as u64;
        self.last_ms.set(last + consumed_ms);
    }

    fn remaining(&self) -> u32 {
        self.remaining.get()
    }

    fn try_consume(&self, n: u32) -> bool {
        let current = self.remaining.get();
        if current >= n {
            self.remaining.set(current - n);
            true
        } else {
            false
        }
    }
}

impl RateBudget {
    /// Build from explicit configuration.
    pub fn new(config: RateBudgetConfig) -> Self {
        Self {
            ip: Bucket::new(config.ip_per_min),
            address: Bucket::new(config.address_per_min),
            config,
        }
    }

    /// Build from the resolved config file settings.
    pub fn from_settings(settings: &RateBudgetSettings) -> Self {
        Self::new(RateBudgetConfig::from_settings(settings))
    }

    /// The configured thresholds.
    pub fn config(&self) -> &RateBudgetConfig {
        &self.config
    }

    /// Refill both buckets to `now_ms` (caller-supplied event time; O(1), no
    /// clock read).
    pub fn refill(&self, now_ms: u64) {
        self.ip.refill(now_ms);
        self.address.refill(now_ms);
    }

    /// Remaining tokens for a bucket. The metric `hl_rate_budget_remaining`
    /// (`{kind}`) reads this; metric registration itself is out of scope here
    /// (SPEC-0010 §12 / §17).
    pub fn remaining(&self, kind: RateKind) -> u32 {
        match kind {
            RateKind::IpWeight => self.ip.remaining(),
            RateKind::Address => self.address.remaining(),
        }
    }

    /// Try to consume `n` tokens from a bucket.
    pub fn try_consume(&self, kind: RateKind, n: u32) -> bool {
        match kind {
            RateKind::IpWeight => self.ip.try_consume(n),
            RateKind::Address => self.address.try_consume(n),
        }
    }

    /// Whether new places are below either place minimum.
    fn below_place_min(&self) -> bool {
        self.ip.remaining() < self.config.ip_min
            || self.address.remaining() < self.config.address_min
    }

    /// Whether even cancels are below the hard floor.
    fn below_hard_floor(&self) -> bool {
        self.ip.remaining() < self.config.hard_floor
            || self.address.remaining() < self.config.hard_floor
    }

    fn consume_place(&self) {
        let _ = self.try_consume(RateKind::IpWeight, 1);
        let _ = self.try_consume(RateKind::Address, 1);
    }

    fn consume_cancel(&self) {
        let _ = self.try_consume(RateKind::IpWeight, 1);
        let _ = self.try_consume(RateKind::Address, 1);
    }
}

/// Sticky circuit breakers, one latch per label (SPEC-0004 K-4).
///
/// Each breaker is independent: tripping `exec_error` and later
/// `exec_backpressure` leaves both tripped, and clearing `exec_error` (once its
/// `Unknown` orders resolve) does not clear `exec_backpressure`. The number of
/// distinct breakers is small, so a `Vec` is cheaper than a map on the gate.
#[derive(Debug, Clone, Default)]
pub struct Breakers {
    labels: Vec<&'static str>,
}

impl Breakers {
    /// A new, clear breaker set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether any breaker is tripped.
    pub fn is_tripped(&self) -> bool {
        !self.labels.is_empty()
    }

    /// The label of the first tripped breaker, if any.
    pub fn label(&self) -> Option<&'static str> {
        self.labels.first().copied()
    }

    /// Every tripped breaker label, for `/healthz` and metrics.
    pub fn labels(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.labels.iter().copied()
    }

    /// Whether `label` is tripped.
    pub fn is_label_tripped(&self, label: &'static str) -> bool {
        self.labels.contains(&label)
    }

    /// Trip `label`. Sticky; returns `true` if it was not already tripped.
    pub fn trip(&mut self, label: &'static str) -> bool {
        if self.labels.contains(&label) {
            return false;
        }
        self.labels.push(label);
        true
    }

    /// Clear every breaker (operator action). Returns `true` if any was tripped.
    pub fn clear(&mut self) -> bool {
        let was = !self.labels.is_empty();
        self.labels.clear();
        was
    }

    /// Clear only `label`. Returns `true` when it was tripped and is now clear.
    ///
    /// Used for self-resolving conditions, e.g. the `exec_error` breaker clears
    /// once every `Unknown` order has been reconciled (SPEC-0010 §16).
    pub fn clear_label(&mut self, label: &'static str) -> bool {
        let before = self.labels.len();
        self.labels.retain(|tripped| *tripped != label);
        self.labels.len() != before
    }
}

/// The engine-relevant subset of the risk limits (SPEC-0004 §5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RiskLimits {
    /// Maximum notional for one order.
    pub max_order_notional: Option<Decimal>,
    /// Maximum absolute per-coin exposure, confirmed plus in-flight.
    pub max_position_notional: Option<Decimal>,
    /// Maximum margin utilization (bps) before new risk is refused.
    pub max_margin_utilization_bps: Option<Decimal>,
    /// Minimum order notional (venue floor by default).
    pub min_notional: Decimal,
}

impl Default for RiskLimits {
    fn default() -> Self {
        Self {
            max_order_notional: None,
            max_position_notional: None,
            max_margin_utilization_bps: None,
            min_notional: MIN_ORDER_NOTIONAL,
        }
    }
}

impl RiskLimits {
    /// Project the resolved config's risk settings onto the engine limits.
    pub fn from_settings(settings: &RiskSettings) -> Self {
        Self {
            max_order_notional: settings.max_order_notional_usd,
            max_position_notional: settings.max_position_notional_usd,
            max_margin_utilization_bps: settings.max_margin_utilization_bps,
            min_notional: MIN_ORDER_NOTIONAL,
        }
    }
}

/// Read-only state one risk check needs. All borrows point at engine-owned
/// state; constructing one is cheap (no ownership churn), which is what lets
/// `check` be called per action without copying.
pub struct RiskCtx<'a> {
    /// The coin the action applies to (resolved by the caller from the intent,
    /// or from the modified order).
    pub coin: CoinId,
    /// Every tracked order, for unknown-state and in-flight exposure.
    pub orders: &'a OrderManager,
    /// Confirmed positions and margin.
    pub account: &'a AccountState,
    /// The coin's market slot, for staleness and the reference mid.
    pub slot: &'a MarketSlot,
    /// Precomputed asset metadata (tick size).
    pub meta: &'a AssetMeta,
    /// The shared rate-budget counters.
    pub rate: &'a RateBudget,
    /// Event time in milliseconds, used to refill the budget.
    pub now_ms: u64,
}

impl RiskCtx<'_> {
    fn mid(&self) -> Option<Px> {
        let bid = self.slot.best_bid()?.px;
        let ask = self.slot.best_ask()?.px;
        Some((bid + ask) / Decimal::TWO)
    }
}

/// The synchronous, O(1) hot-path risk gate (SPEC-0010 §11).
#[derive(Debug, Clone, Default)]
pub struct RiskGate {
    limits: RiskLimits,
    breakers: Breakers,
    kill: KillSwitch,
}

impl RiskGate {
    /// Build a gate from explicit limits and shared kill/breaker state.
    pub fn new(limits: RiskLimits, kill: KillSwitch, breakers: Breakers) -> Self {
        Self {
            limits,
            breakers,
            kill,
        }
    }

    /// Build a gate from the resolved config.
    pub fn from_settings(risk: &RiskSettings) -> Self {
        Self::new(
            RiskLimits::from_settings(risk),
            KillSwitch::new(),
            Breakers::new(),
        )
    }

    /// The configured limits.
    pub fn limits(&self) -> &RiskLimits {
        &self.limits
    }

    /// Mutable access, for `Control::ReloadLimits`.
    pub fn limits_mut(&mut self) -> &mut RiskLimits {
        &mut self.limits
    }

    /// The shared kill switch (set by `Control::KillSwitch`, SIGUSR1, or the
    /// flag-file poller).
    pub fn kill(&self) -> &KillSwitch {
        &self.kill
    }

    /// The shared breakers.
    pub fn breakers(&self) -> &Breakers {
        &self.breakers
    }

    /// Mutable access to the breakers (K-4 trip points).
    pub fn breakers_mut(&mut self) -> &mut Breakers {
        &mut self.breakers
    }

    /// Evaluate an action, returning the exact [`RiskReason`] on rejection.
    ///
    /// This is the testable core; [`RiskGate::check`] wraps it into a
    /// [`Decision`]. Both consume rate budget, so call exactly one.
    pub fn evaluate(&mut self, action: &Action, ctx: &RiskCtx<'_>) -> Result<Decision, RiskReason> {
        ctx.rate.refill(ctx.now_ms);

        // Cancels reduce risk: never blocked except by the budget hard floor.
        if matches!(action, Action::Cancel { .. }) {
            if ctx.rate.below_hard_floor() {
                return Err(RiskReason::RateBudget);
            }
            ctx.rate.consume_cancel();
            return Ok(Decision::Approve);
        }

        // 1. kill switch, 2. breaker state.
        if self.kill.is_active() {
            return Err(RiskReason::KillSwitch);
        }
        if self.breakers.is_tripped() {
            return Err(RiskReason::Breaker(
                self.breakers.label().unwrap_or("unknown"),
            ));
        }

        match action {
            Action::Place(intent) => self.check_increasing(
                ctx.coin,
                intent.reduce_only,
                intent.limit_px,
                intent.size,
                ctx,
            ),
            Action::Modify { cloid, px, sz } => {
                let Some(order) = ctx.orders.get(*cloid) else {
                    return Err(RiskReason::UnknownCloid);
                };
                let reduce_only = order.reduce_only;
                self.check_increasing(ctx.coin, reduce_only, Some(*px), *sz, ctx)
            }
            Action::PlaceGroup(_) => Err(RiskReason::GroupUnsupported),
            Action::Cancel { .. } => unreachable!("handled above"),
        }
    }

    /// Evaluate an action and flatten the result to a [`Decision`].
    ///
    /// Approvals and resizes are allocation-free; only a rejection builds its
    /// reason string (rare by construction).
    pub fn check(&mut self, action: &Action, ctx: &RiskCtx<'_>) -> Decision {
        match self.evaluate(action, ctx) {
            Ok(decision) => decision,
            Err(reason) => Decision::Reject(reason.to_string()),
        }
    }

    /// The shared checks for any risk-increasing action (place or modify).
    fn check_increasing(
        &self,
        coin: CoinId,
        reduce_only: bool,
        limit_px: Option<Px>,
        size: Sz,
        ctx: &RiskCtx<'_>,
    ) -> Result<Decision, RiskReason> {
        // 3. stale coin (reduce-only orders may still close a bad position).
        if !reduce_only && ctx.slot.stale {
            return Err(RiskReason::StaleCoin);
        }
        // 4. unknown orders on the coin block new non-reduce-only places.
        if !reduce_only && ctx.orders.unknown_on_coin(coin) {
            return Err(RiskReason::UnknownOrders);
        }
        // 5. rate budget.
        if ctx.rate.below_place_min() {
            return Err(RiskReason::RateBudget);
        }
        ctx.rate.consume_place();

        // 6. reference price and per-order notional.
        let reference = match limit_px {
            Some(px) => px,
            None => ctx.mid().ok_or(RiskReason::NoReferencePrice)?,
        };
        if reference <= Decimal::ZERO {
            return Err(RiskReason::NoReferencePrice);
        }
        let mut size = size;
        let mut resized = false;
        if let Some(cap) = self.limits.max_order_notional
            && size * reference > cap
        {
            size = cap / reference;
            resized = true;
        }

        // 7. per-coin projected exposure = confirmed + worst-case in-flight.
        if let Some(cap) = self.limits.max_position_notional
            && !reduce_only
        {
            let confirmed = ctx.account.projected_notional(coin, reference);
            let in_flight = ctx.orders.pending_notional(coin);
            let room = cap - confirmed - in_flight;
            if room <= Decimal::ZERO {
                return Err(RiskReason::PositionExposure);
            }
            if size * reference > room {
                size = room / reference;
                resized = true;
            }
        }

        // 8. account margin utilization.
        if let Some(cap) = self.limits.max_margin_utilization_bps {
            let utilization = margin_utilization_bps(ctx.account);
            if utilization > cap {
                return Err(RiskReason::MarginUtilization);
            }
        }

        // 9. min-notional / rounding validity.
        if size <= Decimal::ZERO {
            return Err(RiskReason::NonPositiveSize);
        }
        if size * reference < self.limits.min_notional {
            return Err(RiskReason::MinNotional);
        }
        if let Some(tick) = ctx.meta.tick_size
            && tick > Decimal::ZERO
            && reference % tick != Decimal::ZERO
        {
            return Err(RiskReason::TickInvalid);
        }

        let final_size = if resized { size.normalize() } else { size };
        Ok(if resized {
            Decision::Resize(final_size)
        } else {
            Decision::Approve
        })
    }
}

/// Margin utilization in bps of account value; zero when value is unknown
/// (mirrors `mev_risk::limits`).
fn margin_utilization_bps(account: &AccountState) -> Decimal {
    if account.account_value <= Decimal::ZERO {
        return Decimal::ZERO;
    }
    account.margin_used / account.account_value * Decimal::from(10_000)
}

/// Every cloid a kill response must cancel: all working orders (SPEC-0004 K-3).
///
/// The flag mechanism lives in `mev-risk::kill`; this concrete collector lives
/// here because it needs the engine's [`OrderManager`]/[`Cloid`].
pub fn cancel_all_cloids(orders: &OrderManager) -> Vec<Cloid> {
    mev_risk::kill::cancel_all_cloids(orders.working().map(|order| order.cloid))
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use mev_core::config::{RateBudgetSettings, RiskSettings};
    use mev_strategy::{OrderIntent, Side as StrategySide, StrategyId, TimeInForce};

    use super::*;
    use crate::builder::AssetMeta;
    use crate::orders::{LiveOrder, OrderState};
    use crate::types::{Side, Stamp};

    fn ds(value: &str) -> Decimal {
        Decimal::from_str(value).unwrap()
    }

    fn level(px: i64) -> crate::types::Level {
        crate::types::Level {
            px: Decimal::from(px),
            sz: Decimal::ONE,
            n: 1,
        }
    }

    fn asset_meta() -> AssetMeta {
        AssetMeta {
            asset_id: 0,
            sz_decimals: 0,
            is_spot: false,
            tick_size: None,
        }
    }

    fn slot() -> MarketSlot {
        MarketSlot {
            bbo: Some((Some(level(99)), Some(level(101)), Stamp::default())),
            ..Default::default()
        }
    }

    fn account(szi: &str, account_value: &str, margin_used: &str) -> AccountState {
        let mut account = AccountState::new(1);
        account.set_position_szi(CoinId(0), ds(szi));
        account.account_value = ds(account_value);
        account.margin_used = ds(margin_used);
        account
    }

    fn live(
        cloid: Cloid,
        coin: u16,
        px: Px,
        sz: Sz,
        state: OrderState,
        reduce_only: bool,
    ) -> LiveOrder {
        LiveOrder {
            cloid,
            coin: CoinId(coin),
            side: Side::Buy,
            px,
            sz,
            filled_sz: Decimal::ZERO,
            reduce_only,
            strategy: StrategyId::from("t"),
            state,
            req_id: None,
            oid: None,
        }
    }

    fn intent(
        side: StrategySide,
        limit_px: Option<Decimal>,
        size: Decimal,
        reduce_only: bool,
    ) -> Action {
        Action::Place(OrderIntent {
            strategy: StrategyId::from("t"),
            coin: "BTC".into(),
            side,
            limit_px,
            size,
            tif: TimeInForce::Alo,
            reduce_only,
            rationale: "test".into(),
            cloid: None,
            signal_ms: 0,
            decision_ms: 0,
        })
    }

    fn buy(limit_px: Option<Decimal>, size: Decimal) -> Action {
        intent(StrategySide::Buy, limit_px, size, false)
    }

    fn huge_budget() -> RateBudget {
        RateBudget::new(RateBudgetConfig {
            ip_per_min: 1_000_000,
            address_per_min: 1_000_000,
            ip_min: 0,
            address_min: 0,
            hard_floor: 0,
        })
    }

    fn ctx<'a>(
        coin: CoinId,
        orders: &'a OrderManager,
        account: &'a AccountState,
        slot: &'a MarketSlot,
        meta: &'a AssetMeta,
        rate: &'a RateBudget,
    ) -> RiskCtx<'a> {
        RiskCtx {
            coin,
            orders,
            account,
            slot,
            meta,
            rate,
            now_ms: 0,
        }
    }

    fn generous_gate() -> RiskGate {
        RiskGate::new(
            RiskLimits {
                max_order_notional: Some(ds("5000")),
                max_position_notional: Some(ds("100000")),
                max_margin_utilization_bps: Some(ds("9000")),
                min_notional: ds("10"),
            },
            KillSwitch::new(),
            Breakers::new(),
        )
    }

    #[test]
    fn check_order_reasons_fire_in_spec_11_order() {
        // Each case turns on exactly its target condition and keeps every
        // earlier condition off, so the reason identifies the first failure.
        let orders = OrderManager::new(1);
        let acct = account("0", "1000", "0");
        let mut gate = generous_gate();
        let mut slot = slot();
        let meta = asset_meta();
        let budget = huge_budget();

        // 1. kill switch wins over everything else.
        let unknown = {
            let mut o = OrderManager::new(1);
            o.insert(live(
                Cloid([7; 16]),
                0,
                ds("100"),
                ds("1"),
                OrderState::Unknown,
                false,
            ));
            o
        };
        gate.kill().set();
        slot.stale = true;
        gate.breakers_mut().trip("test-breaker");
        let ctx_all = ctx(CoinId(0), &unknown, &acct, &slot, &meta, &budget);
        assert_eq!(
            gate.evaluate(&buy(Some(ds("100")), ds("1")), &ctx_all),
            Err(RiskReason::KillSwitch)
        );
        gate.kill().clear();

        // 2. breaker next.
        let ctx_all = ctx(CoinId(0), &unknown, &acct, &slot, &meta, &budget);
        assert_eq!(
            gate.evaluate(&buy(Some(ds("100")), ds("1")), &ctx_all),
            Err(RiskReason::Breaker("test-breaker"))
        );
        gate.breakers_mut().clear();
        slot.stale = false;

        // 3. stale coin.
        slot.stale = true;
        let ctx_stale = ctx(CoinId(0), &orders, &acct, &slot, &meta, &budget);
        assert_eq!(
            gate.evaluate(&buy(Some(ds("100")), ds("1")), &ctx_stale),
            Err(RiskReason::StaleCoin)
        );
        slot.stale = false;

        // 4. unknown orders on the coin.
        let ctx_unknown = ctx(CoinId(0), &unknown, &acct, &slot, &meta, &budget);
        assert_eq!(
            gate.evaluate(&buy(Some(ds("100")), ds("1")), &ctx_unknown),
            Err(RiskReason::UnknownOrders)
        );

        // 5. rate budget.
        let tight = RateBudget::new(RateBudgetConfig {
            ip_per_min: 1200,
            address_per_min: 1200,
            ip_min: 100,
            address_min: 500,
            hard_floor: 1,
        });
        assert!(tight.try_consume(RateKind::IpWeight, 1101));
        let ctx_rate = ctx(CoinId(0), &orders, &acct, &slot, &meta, &tight);
        assert_eq!(
            gate.evaluate(&buy(Some(ds("100")), ds("1")), &ctx_rate),
            Err(RiskReason::RateBudget)
        );

        // 6. no reference price (aggressive order, empty slot).
        let empty_slot = MarketSlot::default();
        let ctx_noref = ctx(CoinId(0), &orders, &acct, &empty_slot, &meta, &budget);
        assert_eq!(
            gate.evaluate(&buy(None, ds("1")), &ctx_noref),
            Err(RiskReason::NoReferencePrice)
        );

        // 7. per-coin exposure cap already reached.
        let sized = account("1", "1000", "0"); // 1 * 100 = $100 confirmed
        let mut small = generous_gate();
        small.limits_mut().max_position_notional = Some(ds("100"));
        let ctx_small = ctx(CoinId(0), &orders, &sized, &slot, &meta, &budget);
        assert_eq!(
            small.evaluate(&buy(Some(ds("100")), ds("1")), &ctx_small),
            Err(RiskReason::PositionExposure)
        );

        // 8. margin utilization cap.
        let leveraged = account("0", "1000", "900");
        let mut margin_gate = generous_gate();
        margin_gate.limits_mut().max_margin_utilization_bps = Some(ds("5000"));
        let ctx_margin = ctx(CoinId(0), &orders, &leveraged, &slot, &meta, &budget);
        assert_eq!(
            margin_gate.evaluate(&buy(Some(ds("100")), ds("1")), &ctx_margin),
            Err(RiskReason::MarginUtilization)
        );

        // 9a. min notional after resize.
        let mut min_gate = generous_gate();
        min_gate.limits.min_notional = ds("200");
        let ctx_min = ctx(CoinId(0), &orders, &acct, &slot, &meta, &budget);
        assert_eq!(
            min_gate.evaluate(&buy(Some(ds("100")), ds("1")), &ctx_min),
            Err(RiskReason::MinNotional)
        );

        // 9b. non-positive size.
        let ctx_zero = ctx(CoinId(0), &orders, &acct, &slot, &meta, &budget);
        assert_eq!(
            gate.evaluate(&buy(Some(ds("100")), Decimal::ZERO), &ctx_zero),
            Err(RiskReason::NonPositiveSize)
        );

        // 9c. tick alignment.
        let mut tick_meta = asset_meta();
        tick_meta.tick_size = Some(ds("0.5"));
        let ctx_tick = ctx(CoinId(0), &orders, &acct, &slot, &tick_meta, &budget);
        assert_eq!(
            gate.evaluate(&buy(Some(ds("100.3")), ds("1")), &ctx_tick),
            Err(RiskReason::TickInvalid)
        );
    }

    #[test]
    fn reduce_only_bypasses_stale_and_unknown_but_still_pays_budget() {
        let mut orders = OrderManager::new(1);
        orders.insert(live(
            Cloid([1; 16]),
            0,
            ds("100"),
            ds("1"),
            OrderState::Unknown,
            false,
        ));
        let acct = account("0", "1000", "0");
        let mut slot = slot();
        slot.stale = true;
        let meta = asset_meta();
        let budget = huge_budget();
        let mut gate = generous_gate();
        let ctx = ctx(CoinId(0), &orders, &acct, &slot, &meta, &budget);
        // Non reduce-only would be blocked by stale/unknown; reduce-only passes.
        let reduce = intent(StrategySide::Sell, Some(ds("100")), ds("1"), true);
        assert_eq!(gate.evaluate(&reduce, &ctx), Ok(Decision::Approve));
    }

    #[test]
    fn per_order_notional_resizes_and_position_room_resizes() {
        let orders = OrderManager::new(1);
        let acct = account("4", "1000", "0"); // $400 confirmed at 100
        let slot = slot();
        let meta = asset_meta();
        let budget = huge_budget();
        let mut gate = RiskGate::new(
            RiskLimits {
                max_order_notional: Some(ds("250")),     // 2.5 units
                max_position_notional: Some(ds("1000")), // 10 units total
                max_margin_utilization_bps: Some(ds("9000")),
                min_notional: ds("10"),
            },
            KillSwitch::new(),
            Breakers::new(),
        );
        let ctx = ctx(CoinId(0), &orders, &acct, &slot, &meta, &budget);
        // Order cap first resizes 10 -> 2.5; then position room (10 - 4 = 6) is
        // larger, so the order cap wins.
        assert_eq!(
            gate.evaluate(&buy(Some(ds("100")), ds("10")), &ctx),
            Ok(Decision::Resize(ds("2.5")))
        );
    }

    #[test]
    fn in_flight_orders_count_toward_exposure() {
        let mut orders = OrderManager::new(1);
        orders.insert(live(
            Cloid([1; 16]),
            0,
            ds("100"),
            ds("8"),
            OrderState::Resting,
            false,
        ));
        let acct = account("0", "1000", "0");
        let slot = slot();
        let meta = asset_meta();
        let budget = huge_budget();
        let mut gate = RiskGate::new(
            RiskLimits {
                max_order_notional: None,
                max_position_notional: Some(ds("1000")), // 10 units total
                max_margin_utilization_bps: None,
                min_notional: ds("10"),
            },
            KillSwitch::new(),
            Breakers::new(),
        );
        let ctx = ctx(CoinId(0), &orders, &acct, &slot, &meta, &budget);
        // $800 already in flight, only $200 of room remains.
        assert_eq!(
            gate.evaluate(&buy(Some(ds("100")), ds("4")), &ctx),
            Ok(Decision::Resize(ds("2")))
        );
    }

    #[test]
    fn cancels_pass_with_kill_switch_active() {
        let mut orders = OrderManager::new(1);
        let c = Cloid([1; 16]);
        orders.insert(live(c, 0, ds("100"), ds("1"), OrderState::Resting, false));
        let acct = account("0", "1000", "0");
        let mut slot = slot();
        slot.stale = true;
        let meta = asset_meta();
        let budget = huge_budget();
        let mut gate = generous_gate();
        gate.kill().set();
        gate.breakers_mut().trip("x");
        let ctx = ctx(CoinId(0), &orders, &acct, &slot, &meta, &budget);
        assert_eq!(
            gate.evaluate(&Action::Cancel { cloid: c }, &ctx),
            Ok(Decision::Approve)
        );
    }

    #[test]
    fn cancels_are_blocked_only_by_the_hard_floor() {
        let mut orders = OrderManager::new(1);
        let c = Cloid([1; 16]);
        orders.insert(live(c, 0, ds("100"), ds("1"), OrderState::Resting, false));
        let acct = account("0", "1000", "0");
        let slot = slot();
        let meta = asset_meta();
        let mut gate = generous_gate();
        gate.kill().set();
        let budget = RateBudget::new(RateBudgetConfig {
            ip_per_min: 1,
            address_per_min: 1,
            ip_min: 0,
            address_min: 0,
            hard_floor: 1,
        });
        let ctx = ctx(CoinId(0), &orders, &acct, &slot, &meta, &budget);
        // Drain the buckets to zero: below the hard floor.
        assert!(ctx.rate.try_consume(RateKind::IpWeight, 1));
        assert!(ctx.rate.try_consume(RateKind::Address, 1));
        assert_eq!(
            gate.evaluate(&Action::Cancel { cloid: c }, &ctx),
            Err(RiskReason::RateBudget)
        );
    }

    #[test]
    fn kill_switch_stops_all_new_places_within_one_iteration() {
        let mut orders = OrderManager::new(1);
        for i in 0..3u8 {
            orders.insert(live(
                Cloid([i + 1; 16]),
                0,
                ds("100"),
                ds("1"),
                OrderState::Resting,
                false,
            ));
        }
        let acct = account("0", "1000", "0");
        let slot = slot();
        let meta = asset_meta();
        let budget = huge_budget();
        let mut gate = generous_gate();
        let ctx = ctx(CoinId(0), &orders, &acct, &slot, &meta, &budget);

        // Before the switch: approved.
        assert_eq!(
            gate.evaluate(&buy(Some(ds("100")), ds("1")), &ctx),
            Ok(Decision::Approve)
        );

        // Trip it, and every place in the next iteration is stopped.
        assert!(gate.kill().set());
        for _ in 0..5 {
            assert_eq!(
                gate.evaluate(&buy(Some(ds("100")), ds("1")), &ctx),
                Err(RiskReason::KillSwitch)
            );
        }
        // The cancel-all response covers every working order.
        let to_cancel = cancel_all_cloids(&orders);
        assert_eq!(to_cancel.len(), 3);
        for i in 0..3u8 {
            assert!(to_cancel.contains(&Cloid([i + 1; 16])));
        }

        // Sticky until explicitly cleared.
        assert!(gate.kill().is_active());
        assert!(!gate.kill().set(), "re-setting is not a transition");
        assert!(gate.kill().is_active());
        assert!(gate.kill().clear());
        assert_eq!(
            gate.evaluate(&buy(Some(ds("100")), ds("1")), &ctx),
            Ok(Decision::Approve)
        );
    }

    #[test]
    fn breakers_keep_independent_state() {
        let mut breakers = Breakers::new();
        assert!(!breakers.is_tripped());
        assert!(breakers.trip("exec_error"));
        assert!(!breakers.trip("exec_error"), "re-trip is not a transition");
        assert!(breakers.trip("exec_backpressure"));
        assert!(breakers.is_label_tripped("exec_error"));
        assert!(breakers.is_label_tripped("exec_backpressure"));

        // Clearing one does not clear the other.
        assert!(breakers.clear_label("exec_error"));
        assert!(!breakers.clear_label("exec_error"), "already clear");
        assert!(!breakers.is_label_tripped("exec_error"));
        assert!(breakers.is_tripped());
        assert_eq!(breakers.label(), Some("exec_backpressure"));

        // An operator clear drops every breaker.
        assert!(breakers.clear());
        assert!(!breakers.is_tripped());
        assert_eq!(breakers.labels().count(), 0);
    }

    #[test]
    fn cancel_all_returns_working_cloids_only() {
        let mut orders = OrderManager::new(1);
        orders.insert(live(
            Cloid([1; 16]),
            0,
            ds("100"),
            ds("1"),
            OrderState::Resting,
            false,
        ));
        orders.insert(live(
            Cloid([2; 16]),
            0,
            ds("100"),
            ds("1"),
            OrderState::PendingNew,
            false,
        ));
        orders.insert(live(
            Cloid([3; 16]),
            0,
            ds("100"),
            ds("1"),
            OrderState::Unknown,
            false,
        ));
        orders.insert(live(
            Cloid([4; 16]),
            0,
            ds("100"),
            ds("1"),
            OrderState::Filled,
            false,
        ));
        orders.insert(live(
            Cloid([5; 16]),
            0,
            ds("100"),
            ds("1"),
            OrderState::Cancelled,
            false,
        ));
        let to_cancel = cancel_all_cloids(&orders);
        assert_eq!(to_cancel.len(), 3);
        assert!(to_cancel.contains(&Cloid([1; 16])));
        assert!(to_cancel.contains(&Cloid([2; 16])));
        assert!(to_cancel.contains(&Cloid([3; 16])));
    }

    #[test]
    fn rate_budget_consumes_down_and_guards_cancels() {
        let budget = RateBudget::new(RateBudgetConfig {
            ip_per_min: 10,
            address_per_min: 10,
            ip_min: 4,
            address_min: 4,
            hard_floor: 1,
        });
        assert_eq!(budget.remaining(RateKind::IpWeight), 10);
        for _ in 0..6 {
            assert!(budget.try_consume(RateKind::IpWeight, 1));
        }
        assert_eq!(budget.remaining(RateKind::IpWeight), 4);
        assert!(!budget.below_place_min(), "at the minimum is still allowed");
        assert!(budget.try_consume(RateKind::IpWeight, 1));
        assert_eq!(budget.remaining(RateKind::IpWeight), 3);
        assert!(budget.below_place_min());
        assert!(!budget.below_hard_floor());

        // Places reject below the minimum; cancels still pass.
        let orders = OrderManager::new(1);
        let acct = account("0", "1000", "0");
        let slot = slot();
        let meta = asset_meta();
        let mut gate = generous_gate();
        let ctx = ctx(CoinId(0), &orders, &acct, &slot, &meta, &budget);
        assert_eq!(
            gate.evaluate(&buy(Some(ds("100")), ds("1")), &ctx),
            Err(RiskReason::RateBudget)
        );
        assert_eq!(
            gate.evaluate(
                &Action::Cancel {
                    cloid: Cloid([1; 16])
                },
                &ctx
            ),
            Ok(Decision::Approve)
        );

        // Drain below the hard floor: cancels are refused too.
        let ip = budget.remaining(RateKind::IpWeight);
        let address = budget.remaining(RateKind::Address);
        assert!(budget.try_consume(RateKind::IpWeight, ip));
        assert!(budget.try_consume(RateKind::Address, address));
        assert!(budget.below_hard_floor());
        assert_eq!(
            gate.evaluate(
                &Action::Cancel {
                    cloid: Cloid([1; 16])
                },
                &ctx
            ),
            Err(RiskReason::RateBudget)
        );
    }

    #[test]
    fn rate_budget_refills_over_time() {
        let budget = RateBudget::new(RateBudgetConfig {
            ip_per_min: 1200,
            address_per_min: 1200,
            ip_min: 100,
            address_min: 100,
            hard_floor: 1,
        });
        assert!(budget.try_consume(RateKind::IpWeight, 1200));
        assert_eq!(budget.remaining(RateKind::IpWeight), 0);
        budget.refill(60_000); // one minute
        assert_eq!(budget.remaining(RateKind::IpWeight), 1200);
        budget.refill(60_000); // no time passed since the last refill
        assert_eq!(budget.remaining(RateKind::IpWeight), 1200);
    }

    #[test]
    fn rate_budget_handles_fresh_anchor_and_partial_refill() {
        let budget = RateBudget::new(RateBudgetConfig {
            ip_per_min: 1200,
            address_per_min: 1200,
            ip_min: 1,
            address_min: 1,
            hard_floor: 1,
        });
        // The first refill sees a wall-clock-scale timestamp against the fresh
        // bucket's 0 anchor; it must fill, not overflow.
        budget.refill(1_700_000_000_000);
        assert_eq!(budget.remaining(RateKind::IpWeight), 1200);
        assert!(budget.try_consume(RateKind::IpWeight, 1200));
        // 50 ms at 20 tokens/s yields exactly one token; the remainder is kept.
        budget.refill(1_700_000_000_050);
        assert_eq!(budget.remaining(RateKind::IpWeight), 1);
        budget.refill(1_700_000_000_070);
        assert_eq!(budget.remaining(RateKind::IpWeight), 1);
        budget.refill(1_700_000_000_100);
        assert_eq!(budget.remaining(RateKind::IpWeight), 2);
    }

    #[test]
    fn config_maps_into_limits_and_budget() {
        let settings = RiskSettings {
            max_order_notional_usd: Some(ds("250")),
            max_position_notional_usd: Some(ds("1000")),
            max_margin_utilization_bps: Some(ds("5000")),
            ..Default::default()
        };
        let limits = RiskLimits::from_settings(&settings);
        assert_eq!(limits.max_order_notional, Some(ds("250")));
        assert_eq!(limits.min_notional, MIN_ORDER_NOTIONAL);

        let budget = RateBudgetConfig::from_settings(&RateBudgetSettings {
            ip_weight_per_min: 1200,
            engine_ip_share: 600,
            address_per_min: 800,
            ip_weight_min: 100,
            address_min: 50,
            hard_floor: 2,
        });
        assert_eq!(budget.ip_per_min, 600, "engine share caps the bucket");
        assert_eq!(budget.address_per_min, 800);
        assert_eq!(budget.hard_floor, 2);
    }

    /// A tiny deterministic xorshift PRNG (no new dependency; SPEC-0010 E-9).
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n.max(1)
        }
    }

    fn nth_cloid(n: u32) -> Cloid {
        let mut bytes = [0u8; 16];
        bytes[0..4].copy_from_slice(&n.to_le_bytes());
        Cloid(bytes)
    }

    fn exposure_ok(
        account: &AccountState,
        orders: &OrderManager,
        cap: Decimal,
        ref_px: Px,
    ) -> bool {
        account.projected_notional(CoinId(0), ref_px) + orders.pending_notional(CoinId(0)) <= cap
    }

    #[test]
    fn randomized_approved_actions_never_exceed_the_cap() {
        let cap = ds("1000");
        let ref_px = ds("100");
        let limits = RiskLimits {
            max_order_notional: Some(ds("300")),
            max_position_notional: Some(cap),
            max_margin_utilization_bps: Some(ds("10_000")),
            min_notional: ds("10"),
        };
        let mut gate = RiskGate::new(limits, KillSwitch::new(), Breakers::new());
        let mut orders = OrderManager::new(1);
        let mut account = AccountState::new(1);
        account.account_value = ds("1_000_000");
        let slot = slot();
        let meta = asset_meta();
        let budget = huge_budget();
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let mut next_cloid = 0u32;

        for _ in 0..5_000 {
            let op = rng.below(10);
            if op < 6 {
                // Place a random buy/sell; apply the decision, then assert the
                // invariant on the resulting state.
                let is_buy = rng.below(2) == 0;
                let size = Decimal::from(rng.below(5) + 1);
                let side = if is_buy {
                    StrategySide::Buy
                } else {
                    StrategySide::Sell
                };
                let action = intent(side, Some(ref_px), size, false);
                let cloid = nth_cloid(next_cloid);
                next_cloid += 1;
                let decision = {
                    let ctx = ctx(CoinId(0), &orders, &account, &slot, &meta, &budget);
                    gate.evaluate(&action, &ctx)
                };
                let applied = match decision {
                    Ok(Decision::Approve) => Some(size),
                    Ok(Decision::Resize(resized)) => Some(resized),
                    Ok(_) => unreachable!("checks return Approve or Resize"),
                    Err(_) => None,
                };
                if let Some(applied) = applied {
                    let mut order = live(cloid, 0, ref_px, applied, OrderState::Resting, false);
                    order.side = if is_buy { Side::Buy } else { Side::Sell };
                    orders.insert(order);
                }
                assert!(
                    exposure_ok(&account, &orders, cap, ref_px),
                    "approved place pushed exposure over cap"
                );
            } else if op < 8 {
                // Cancel a random working order.
                let working: Vec<Cloid> = orders.working().map(|order| order.cloid).collect();
                if working.is_empty() {
                    continue;
                }
                let cloid = working[rng.below(working.len() as u64) as usize];
                let ctx = ctx(CoinId(0), &orders, &account, &slot, &meta, &budget);
                assert_eq!(
                    gate.evaluate(&Action::Cancel { cloid }, &ctx),
                    Ok(Decision::Approve)
                );
                orders.set_state(cloid, OrderState::Cancelled);
                assert!(exposure_ok(&account, &orders, cap, ref_px));
            } else {
                // Fill a random working order in full: in-flight becomes
                // confirmed at the same reference price.
                let working: Vec<(Cloid, bool, Decimal)> = orders
                    .working()
                    .map(|order| {
                        (
                            order.cloid,
                            matches!(order.side, Side::Buy),
                            order.remaining(),
                        )
                    })
                    .collect();
                if working.is_empty() {
                    continue;
                }
                let (cloid, is_buy, remaining) = working[rng.below(working.len() as u64) as usize];
                orders.on_fill(Some(cloid), remaining);
                let szi =
                    account.position_szi(CoinId(0)) + if is_buy { remaining } else { -remaining };
                account.set_position_szi(CoinId(0), szi);
                assert!(exposure_ok(&account, &orders, cap, ref_px));
            }
        }
    }
}
