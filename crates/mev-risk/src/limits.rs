//! Minimal fail-closed limits (SPEC-0003 §5, SPEC-0004 §5–§6).
//!
//! [`LimitRisk`] implements the ordered pre-trade checks the strategies rely
//! on: order notional, projected per-coin position notional, open-order count,
//! and margin utilization. Any missing or unknown input rejects the intent.

use std::collections::BTreeMap;

use rust_decimal::Decimal;

use mev_strategy::OrderIntent;

use crate::{Decision, RiskCheck, RiskContext, TradingHalt};

/// The default minimum order notional, mirroring the venue's $10 floor.
pub const DEFAULT_MIN_NOTIONAL: Decimal = Decimal::TEN;

/// Configurable pre-trade limits. Every cap is optional; `None` disables it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// Minimum order notional required after any resize.
    pub min_notional: Decimal,
    /// Maximum notional for a single order.
    pub max_order_notional: Option<Decimal>,
    /// Maximum absolute per-coin position notional after this order.
    pub max_position_notional: Option<Decimal>,
    /// Maximum resting orders allowed per coin.
    pub max_open_orders: Option<usize>,
    /// Maximum margin utilization in bps, above which new risk is refused.
    pub max_margin_utilization_bps: Option<Decimal>,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            min_notional: DEFAULT_MIN_NOTIONAL,
            max_order_notional: None,
            max_position_notional: None,
            max_open_orders: None,
            max_margin_utilization_bps: None,
        }
    }
}

/// A fail-closed limits checker.
#[derive(Debug, Clone, Default)]
pub struct LimitRisk {
    limits: Limits,
    /// Sticky halt flag, checked first (SPEC-0002 H-4 / SPEC-0004 K-3).
    halt: TradingHalt,
    /// Signed base-unit size approved earlier in the current decision cycle, by
    /// coin. Reset by [`LimitRisk::begin_cycle`]; projected exposure counts it
    /// so several intents in one cycle cannot each pass and together breach a
    /// cap (SPEC-0010 E-0).
    pending: BTreeMap<String, Decimal>,
}

impl LimitRisk {
    /// Build a gate from explicit limits.
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            halt: TradingHalt::new(),
            pending: BTreeMap::new(),
        }
    }

    /// A clone of the halt flag, for components that set it (dead-man, kill
    /// switch).
    pub fn halt(&self) -> TradingHalt {
        self.halt.clone()
    }

    /// The configured limits.
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// Start a new decision cycle: forget intents approved in the previous one.
    pub fn begin_cycle(&mut self) {
        self.pending.clear();
    }

    /// Signed size already approved for `coin` this cycle.
    pub fn pending_size(&self, coin: &str) -> Decimal {
        self.pending.get(coin).copied().unwrap_or(Decimal::ZERO)
    }

    fn record_approved(&mut self, coin: &str, is_buy: bool, size: Decimal) {
        let signed = if is_buy { size } else { -size };
        *self.pending.entry(coin.to_string()).or_default() += signed;
    }
}

impl RiskCheck for LimitRisk {
    fn check(&mut self, intent: &OrderIntent, ctx: &RiskContext<'_>) -> Decision {
        let limits = self.limits.clone();

        // Halt first: a set halt blocks all new risk. Cancels reduce risk and
        // are never blocked here (SPEC-0002 H-4).
        if self.halt.is_halted() {
            return Decision::Reject("trading halted".into());
        }
        if intent.size <= Decimal::ZERO {
            return Decision::Reject("non-positive size".into());
        }
        let Some(reference_px) = intent.limit_px.or_else(|| ctx.market.mid(&intent.coin)) else {
            return Decision::Reject(format!("no reference price for {}", intent.coin));
        };
        if reference_px <= Decimal::ZERO {
            return Decision::Reject("non-positive reference price".into());
        }

        // Margin health: refuse all new risk above the utilization cap.
        if let Some(cap) = limits.max_margin_utilization_bps {
            let utilization = ctx.account.margin_utilization_bps();
            if utilization > cap {
                return Decision::Reject(format!(
                    "margin utilization {utilization} bps exceeds cap {cap} bps"
                ));
            }
        }

        // Resting-order count for this coin.
        if let Some(cap) = limits.max_open_orders
            && !intent.reduce_only
        {
            let count = ctx
                .account
                .open_orders
                .iter()
                .filter(|order| order.coin == intent.coin)
                .count();
            if count >= cap {
                return Decision::Reject(format!(
                    "{} has {count} open orders (cap {cap})",
                    intent.coin
                ));
            }
        }

        let mut size = intent.size;
        let mut resized = false;

        // Single-order notional cap.
        if let Some(cap) = limits.max_order_notional
            && size * reference_px > cap
        {
            size = cap / reference_px;
            resized = true;
        }

        // Projected position notional cap (skip for reduce-only and reductions).
        // Count, as if already position: (a) exposure approved earlier in this
        // cycle, and (b) resting in-flight orders from the account snapshot that
        // have not yet shown up as position. This matches the engine's
        // "confirmed + worst-case in-flight" exposure (SPEC-0010 §11, SPEC-0004
        // K-2); gross (both sides) in-flight is intentionally conservative.
        if let Some(cap) = limits.max_position_notional
            && !intent.reduce_only
        {
            let pending = self.pending_size(&intent.coin);
            let in_flight: Decimal = ctx
                .account
                .open_orders
                .iter()
                .filter(|order| order.coin == intent.coin && !order.reduce_only)
                .map(|order| order.sz.abs())
                .sum();
            let current = ctx.account.position_szi(&intent.coin) + pending;
            let signed = if intent.is_buy() { size } else { -size };
            // Gross worst case: the confirmed/pending net plus every working
            // order filling, on both sides.
            let projected = (current + signed).abs() + in_flight;
            let current_abs = current.abs() + in_flight;
            if projected > current_abs {
                let max_size = cap / reference_px;
                if current_abs >= max_size {
                    return Decision::Reject(format!(
                        "{} position cap {cap} already reached",
                        intent.coin
                    ));
                }
                let allowed = max_size - current_abs;
                if allowed < size {
                    size = allowed;
                    resized = true;
                }
            }
        }

        if size * reference_px < limits.min_notional {
            return Decision::Reject(format!(
                "order notional {} below minimum {}",
                size * reference_px,
                limits.min_notional
            ));
        }

        let final_size = if resized { size.normalize() } else { size };
        self.record_approved(&intent.coin, intent.is_buy(), final_size);
        if resized {
            Decision::Resize(final_size)
        } else {
            Decision::Approve
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::str::FromStr;

    use mev_strategy::{
        AccountView, BookView, MarketView, OpenOrderView, PositionView, Side, StrategyId,
        TimeInForce,
    };

    use super::*;

    fn ds(value: &str) -> Decimal {
        Decimal::from_str(value).unwrap()
    }

    fn market() -> MarketView {
        let mut market = MarketView::new();
        market.insert_book(
            "BTC",
            BookView {
                bids: vec![(ds("99"), ds("10"))],
                asks: vec![(ds("101"), ds("10"))],
                sz_decimals: 3,
                time: 0,
            },
        );
        market
    }

    fn intent(side: Side, size: Decimal, limit_px: Option<Decimal>) -> OrderIntent {
        OrderIntent {
            strategy: StrategyId::from("test"),
            coin: "BTC".into(),
            side,
            limit_px,
            size,
            tif: TimeInForce::Alo,
            reduce_only: false,
            rationale: "test".into(),
            cloid: None,
            signal_ms: 0,
            decision_ms: 0,
        }
    }

    fn check(limits: Limits, intent: &OrderIntent, account: &AccountView) -> Decision {
        let market = market();
        let ctx = RiskContext {
            market: &market,
            account,
            now_ms: 0,
        };
        LimitRisk::new(limits).check(intent, &ctx)
    }

    #[test]
    fn approves_a_reasonable_order() {
        let limits = Limits {
            max_order_notional: Some(ds("50000")),
            max_position_notional: Some(ds("100000")),
            max_open_orders: Some(4),
            ..Default::default()
        };
        assert_eq!(
            check(
                limits,
                &intent(Side::Buy, ds("1"), None),
                &AccountView::default()
            ),
            Decision::Approve
        );
    }

    #[test]
    fn rejects_non_positive_size() {
        let decision = check(
            Limits::default(),
            &intent(Side::Buy, Decimal::ZERO, None),
            &AccountView::default(),
        );
        assert!(matches!(decision, Decision::Reject(_)));
    }

    #[test]
    fn rejects_when_no_reference_price() {
        let market = MarketView::new();
        let ctx = RiskContext {
            market: &market,
            account: &AccountView::default(),
            now_ms: 0,
        };
        let decision = LimitRisk::default().check(&intent(Side::Buy, ds("1"), None), &ctx);
        assert!(matches!(decision, Decision::Reject(_)));
    }

    #[test]
    fn resizes_to_order_cap() {
        // Cap $500 at mid 100 => max size 5.
        let limits = Limits {
            max_order_notional: Some(ds("500")),
            ..Default::default()
        };
        assert_eq!(
            check(
                limits,
                &intent(Side::Buy, ds("10"), None),
                &AccountView::default()
            ),
            Decision::Resize(ds("5"))
        );
    }

    #[test]
    fn resizes_to_remaining_position_room() {
        let mut account = AccountView::default();
        account.positions.insert(
            "BTC".into(),
            PositionView {
                coin: "BTC".into(),
                szi: ds("4"),
                ..Default::default()
            },
        );
        // Position cap $1000 at 100 => max 10; already 4, room 6.
        let limits = Limits {
            max_position_notional: Some(ds("1000")),
            ..Default::default()
        };
        assert_eq!(
            check(limits, &intent(Side::Buy, ds("8"), None), &account),
            Decision::Resize(ds("6"))
        );
    }

    #[test]
    fn halted_rejects_all_new_risk() {
        let mut account = AccountView::default();
        account.open_orders.push(OpenOrderView {
            coin: "BTC".into(),
            oid: Some(1),
            cloid: None,
            side: Side::Buy,
            limit_px: ds("100"),
            sz: ds("1"),
            reduce_only: false,
        });
        let limits = Limits::default();
        let mut risk = LimitRisk::new(limits);
        let halt = risk.halt();
        let market = market();
        let ctx = RiskContext {
            market: &market,
            account: &account,
            now_ms: 0,
        };
        assert!(
            risk.check(&intent(Side::Buy, ds("1"), None), &ctx)
                .is_allowed()
        );
        halt.set();
        assert!(matches!(
            risk.check(&intent(Side::Buy, ds("1"), None), &ctx),
            Decision::Reject(_)
        ));
        halt.clear();
        assert!(
            risk.check(&intent(Side::Buy, ds("1"), None), &ctx)
                .is_allowed()
        );
    }

    #[test]
    fn same_cycle_intents_count_toward_position_cap() {
        // Cap $1000 at mid 100 => max 10 BTC. Two 6 BTC buys cannot both pass.
        let limits = Limits {
            max_position_notional: Some(ds("1000")),
            ..Default::default()
        };
        let market = market();
        let ctx = RiskContext {
            market: &market,
            account: &AccountView::default(),
            now_ms: 0,
        };
        let mut risk = LimitRisk::new(limits);
        risk.begin_cycle();
        assert_eq!(
            risk.check(&intent(Side::Buy, ds("6"), None), &ctx),
            Decision::Approve
        );
        assert_eq!(
            risk.check(&intent(Side::Buy, ds("6"), None), &ctx),
            Decision::Resize(ds("4"))
        );
        assert!(matches!(
            risk.check(&intent(Side::Buy, ds("1"), None), &ctx),
            Decision::Reject(_)
        ));

        // A new cycle forgets approvals the account snapshot has caught up on.
        risk.begin_cycle();
        assert_eq!(risk.pending_size("BTC"), Decimal::ZERO);
        assert_eq!(
            risk.check(&intent(Side::Buy, ds("6"), None), &ctx),
            Decision::Approve
        );
    }

    #[test]
    fn counts_in_flight_open_orders_toward_position_cap() {
        // Cap $1000 at mid 100 => 10 BTC max. A resting 8 BTC buy is in flight,
        // so only 2 BTC of room remain even though the position is flat.
        let mut account = AccountView::default();
        account.open_orders.push(OpenOrderView {
            coin: "BTC".into(),
            oid: Some(1),
            cloid: None,
            side: Side::Buy,
            limit_px: ds("100"),
            sz: ds("8"),
            reduce_only: false,
        });
        let limits = Limits {
            max_position_notional: Some(ds("1000")),
            ..Default::default()
        };
        assert_eq!(
            check(limits.clone(), &intent(Side::Buy, ds("4"), None), &account),
            Decision::Resize(ds("2"))
        );
        // A reduce-only resting order cannot increase exposure and is ignored.
        account.open_orders[0].reduce_only = true;
        assert_eq!(
            check(limits, &intent(Side::Buy, ds("4"), None), &account),
            Decision::Approve
        );
    }

    #[test]
    fn allows_reduction_beyond_position_cap() {
        let mut account = AccountView::default();
        account.positions.insert(
            "BTC".into(),
            PositionView {
                coin: "BTC".into(),
                szi: ds("20"),
                ..Default::default()
            },
        );
        let limits = Limits {
            max_position_notional: Some(ds("1000")),
            ..Default::default()
        };
        // Selling 5 reduces a too-large position; must be allowed.
        assert_eq!(
            check(limits, &intent(Side::Sell, ds("5"), None), &account),
            Decision::Approve
        );
    }

    #[test]
    fn rejects_when_position_cap_already_reached() {
        let mut account = AccountView::default();
        account.positions.insert(
            "BTC".into(),
            PositionView {
                coin: "BTC".into(),
                szi: ds("11"),
                ..Default::default()
            },
        );
        let limits = Limits {
            max_position_notional: Some(ds("1000")),
            ..Default::default()
        };
        let decision = check(limits, &intent(Side::Buy, ds("1"), None), &account);
        assert!(matches!(decision, Decision::Reject(_)));
    }

    #[test]
    fn rejects_at_open_order_cap() {
        let mut account = AccountView::default();
        for oid in 0..3 {
            account.open_orders.push(OpenOrderView {
                coin: "BTC".into(),
                oid: Some(oid),
                cloid: None,
                side: Side::Buy,
                limit_px: ds("100"),
                sz: ds("1"),
                reduce_only: false,
            });
        }
        let limits = Limits {
            max_open_orders: Some(3),
            ..Default::default()
        };
        assert!(matches!(
            check(limits, &intent(Side::Buy, ds("1"), None), &account),
            Decision::Reject(_)
        ));
    }

    #[test]
    fn rejects_when_margin_utilization_exceeded() {
        let account = AccountView {
            account_value: ds("1000"),
            margin_used: ds("900"), // 9000 bps
            ..Default::default()
        };
        let limits = Limits {
            max_margin_utilization_bps: Some(ds("8000")),
            ..Default::default()
        };
        assert!(matches!(
            check(limits, &intent(Side::Buy, ds("1"), None), &account),
            Decision::Reject(_)
        ));
    }

    #[test]
    fn rejects_when_resize_falls_below_min_notional() {
        // Cap $9 at mid 100 => 0.09, below the $10 floor.
        let limits = Limits {
            max_order_notional: Some(ds("9")),
            ..Default::default()
        };
        assert!(matches!(
            check(
                limits,
                &intent(Side::Buy, ds("10"), None),
                &AccountView::default()
            ),
            Decision::Reject(_)
        ));
    }

    #[test]
    fn falls_back_to_allmids_when_no_book() {
        let mut market = MarketView::new();
        let mut mids = BTreeMap::new();
        mids.insert("BTC".to_string(), ds("100"));
        market.set_mids(mids);
        let ctx = RiskContext {
            market: &market,
            account: &AccountView::default(),
            now_ms: 0,
        };
        assert!(
            LimitRisk::default()
                .check(&intent(Side::Buy, ds("1"), None), &ctx)
                .is_allowed()
        );
    }
}
