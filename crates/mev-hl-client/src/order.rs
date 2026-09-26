//! Order model, wire actions, and mandatory rounding (SPEC-0002 §6–§7).

use rust_decimal::{Decimal, RoundingStrategy};
use serde::Serialize;

use mev_core::error::{Error, Result};

use crate::assets::{Market, MarketKind};

/// Maximum significant figures allowed in a price.
pub const MAX_PRICE_SIG_FIGS: u32 = 5;
/// Minimum order notional in USD.
pub const MIN_ORDER_NOTIONAL: Decimal = Decimal::TEN;

/// Time in force for limit orders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Tif {
    /// Add-liquidity-only (post-only).
    Alo,
    /// Immediate-or-cancel.
    Ioc,
    /// Good-till-cancelled.
    Gtc,
}

/// Trigger direction for trigger orders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Tpsl {
    /// Take-profit.
    Tp,
    /// Stop-loss.
    Sl,
}

/// Limit order type payload (`{"limit":{"tif":...}}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct LimitType {
    /// Time in force.
    pub tif: Tif,
}

/// Trigger order type payload (`{"trigger":{...}}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TriggerType {
    /// Whether the trigger exits at market once touched.
    pub is_market: bool,
    /// Trigger price, wire-formatted.
    pub trigger_px: String,
    /// Take-profit or stop-loss.
    pub tpsl: Tpsl,
}

/// Order type wrapper (`{"limit": ...}` or `{"trigger": ...}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum OrderType {
    /// Limit order.
    #[serde(rename = "limit")]
    Limit(LimitType),
    /// Trigger order.
    #[serde(rename = "trigger")]
    Trigger(TriggerType),
}

impl OrderType {
    /// A limit order with the given time in force.
    pub fn limit(tif: Tif) -> Self {
        Self::Limit(LimitType { tif })
    }
}

/// Order grouping for the `order` action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Grouping {
    /// Normal (default) grouping.
    Na,
    /// Normal take-profit/stop-loss grouping.
    NormalTpsl,
    /// Position take-profit/stop-loss grouping.
    PositionTpsl,
}

/// Wire order (`{"a","b","p","s","r","t","c"}`). Field order is significant:
/// it must match Hyperliquid's msgpack encoder for the action hash to verify.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OrderWire {
    /// Asset id.
    pub a: u32,
    /// Whether this is a buy.
    pub b: bool,
    /// Limit price as a wire decimal string.
    pub p: String,
    /// Size as a wire decimal string.
    pub s: String,
    /// Reduce-only flag.
    pub r: bool,
    /// Order type.
    pub t: OrderType,
    /// Client order id, omitted when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub c: Option<String>,
}

/// Construct a wire order from already-formatted strings (used for fixtures).
#[allow(clippy::too_many_arguments)]
pub fn limit_order(
    asset: u32,
    is_buy: bool,
    price: &str,
    size: &str,
    tif: Tif,
    reduce_only: bool,
    cloid: Option<&str>,
) -> OrderWire {
    OrderWire {
        a: asset,
        b: is_buy,
        p: price.to_string(),
        s: size.to_string(),
        r: reduce_only,
        t: OrderType::limit(tif),
        c: cloid.map(str::to_string),
    }
}

/// A single cancel by order id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CancelWire {
    /// Asset id.
    pub a: u32,
    /// Order id.
    pub o: u64,
}

/// A single cancel by client order id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CancelByCloidWire {
    /// Asset id.
    pub asset: u32,
    /// Client order id (`0x`-prefixed 16 bytes).
    pub cloid: String,
}

/// A signed HyperCore action (SPEC-0002 §7). `type` is serialized first so the
/// msgpack bytes match the official encoder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Action {
    /// Place one or more orders.
    Order {
        /// Orders to place.
        orders: Vec<OrderWire>,
        /// Grouping.
        grouping: Grouping,
    },
    /// Cancel orders by id.
    Cancel {
        /// Cancels.
        cancels: Vec<CancelWire>,
    },
    /// Cancel orders by client order id.
    CancelByCloid {
        /// Cancels.
        cancels: Vec<CancelByCloidWire>,
    },
    /// Arm/disarm the dead-man's switch. `None` disarms.
    ScheduleCancel {
        /// Cancel-after time in ms; omitted to disarm.
        #[serde(skip_serializing_if = "Option::is_none")]
        time: Option<u64>,
    },
    /// Set cross/isolated leverage for an asset.
    UpdateLeverage {
        /// Asset id.
        asset: u32,
        /// Whether the margin is cross.
        is_cross: bool,
        /// Leverage.
        leverage: u32,
    },
}

impl Action {
    /// Build a single-order `order` action.
    pub fn order(orders: Vec<OrderWire>) -> Self {
        Action::Order {
            orders,
            grouping: Grouping::Na,
        }
    }
}

/// Parameters for building a rounded, validated wire order.
#[derive(Debug, Clone)]
pub struct OrderParams {
    /// Buy if true, sell if false.
    pub is_buy: bool,
    /// Unrounded size.
    pub size: Decimal,
    /// Unrounded limit price.
    pub limit_px: Decimal,
    /// Time in force.
    pub tif: Tif,
    /// Reduce-only flag.
    pub reduce_only: bool,
    /// Optional client order id.
    pub cloid: Option<String>,
}

/// Max decimal places for a price given the market's size decimals.
pub fn max_price_decimals(market: &Market) -> u32 {
    let base: u32 = match market.kind {
        MarketKind::Perp => 6,
        MarketKind::Spot => 8,
    };
    base.saturating_sub(market.sz_decimals)
}

/// Round a size to the market's `szDecimals`.
pub fn round_size(market: &Market, size: Decimal) -> Decimal {
    size.round_dp_with_strategy(market.sz_decimals, RoundingStrategy::MidpointAwayFromZero)
        .normalize()
}

/// Round a price to `MAX_PRICE_SIG_FIGS`, capped by the market's decimal limit.
pub fn round_price(market: &Market, price: Decimal) -> Decimal {
    round_price_with(market, price, RoundingStrategy::MidpointAwayFromZero)
}

/// Round a price in the safe (aggressive) direction for a marketable order.
///
/// A taker buy must round **up** and a taker sell **down**, so rounding never
/// moves the limit away from the touch and turns a fill into a miss
/// (SPEC-0010 §12). The result still obeys the significant-figure and decimal
/// caps.
pub fn round_price_aggressive(market: &Market, price: Decimal, is_buy: bool) -> Decimal {
    let strategy = if is_buy {
        RoundingStrategy::ToPositiveInfinity
    } else {
        RoundingStrategy::ToNegativeInfinity
    };
    round_price_with(market, price, strategy)
}

/// Round a price with an explicit [`RoundingStrategy`].
pub fn round_price_with(market: &Market, price: Decimal, strategy: RoundingStrategy) -> Decimal {
    let mut rounded = round_sig_figs(price, MAX_PRICE_SIG_FIGS, strategy);
    let max_decimals = max_price_decimals(market);
    if rounded.scale() > max_decimals {
        rounded = rounded
            .round_dp_with_strategy(max_decimals, strategy)
            .normalize();
    }
    rounded
}

/// Build a validated, rounded wire order. Returns a precise error instead of
/// sending a request the venue would reject.
pub fn build_order_wire(market: &Market, params: &OrderParams) -> Result<OrderWire> {
    if params.limit_px <= Decimal::ZERO {
        return Err(Error::Config("limit price must be positive".into()));
    }
    if params.size <= Decimal::ZERO {
        return Err(Error::Config("order size must be positive".into()));
    }

    let price = round_price(market, params.limit_px);
    let size = round_size(market, params.size);
    if price <= Decimal::ZERO {
        return Err(Error::Config("limit price rounds to zero".into()));
    }
    if size <= Decimal::ZERO {
        return Err(Error::Config("order size rounds to zero".into()));
    }
    let notional = price * size;
    if notional < MIN_ORDER_NOTIONAL {
        return Err(Error::Config(format!(
            "order notional {notional} below minimum {MIN_ORDER_NOTIONAL}"
        )));
    }

    Ok(OrderWire {
        a: market.asset_id(),
        b: params.is_buy,
        p: wire_decimal(price),
        s: wire_decimal(size),
        r: params.reduce_only,
        t: OrderType::limit(params.tif),
        c: params.cloid.clone(),
    })
}

/// Format a decimal for the wire: minimal, no trailing zeros, no exponent.
pub fn wire_decimal(value: Decimal) -> String {
    value.normalize().to_string()
}

/// Round to `figs` significant figures with the given strategy.
fn round_sig_figs(value: Decimal, figs: u32, strategy: RoundingStrategy) -> Decimal {
    if value.is_zero() {
        return value;
    }
    // Use a single normalized view: mixing normalized digits with the raw scale
    // produces off-by-one rounding for values with trailing zeros.
    let value = value.normalize();
    let scale = value.scale() as i32;
    let digits = significant_digits(value) as i32;
    let dp = scale - digits + figs as i32;
    let rounded = if dp >= 0 {
        value.round_dp_with_strategy(dp as u32, strategy)
    } else {
        let factor = Decimal::from(10i128.pow((-dp) as u32));
        (value / factor).round_dp_with_strategy(0, strategy) * factor
    };
    rounded.normalize()
}

/// Number of significant digits in a decimal.
///
/// Trailing zeros are not significant (so `118680` has 5, matching the way the
/// reference implementation rounds large values to `...0`). This mirrors
/// `normalize` plus integer trailing-zero stripping.
fn significant_digits(value: Decimal) -> u32 {
    let mut remaining = value.normalize().mantissa().unsigned_abs();
    if remaining == 0 {
        return 1;
    }
    while remaining.is_multiple_of(10) {
        remaining /= 10;
    }
    let mut digits = 0;
    while remaining > 0 {
        digits += 1;
        remaining /= 10;
    }
    digits
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::AssetMap;
    use crate::types::{AssetMeta, Meta};
    use std::str::FromStr;

    fn market(sz_decimals: u32) -> Market {
        let mut map = AssetMap::new();
        map.insert_perp_dex(
            None,
            None,
            &Meta {
                universe: vec![AssetMeta {
                    name: "BTC".into(),
                    sz_decimals,
                    max_leverage: 40,
                    is_delisted: false,
                    only_isolated: false,
                }],
            },
        );
        map.get("BTC").unwrap().clone()
    }

    fn btc() -> Market {
        market(5)
    }

    fn dec(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    #[test]
    fn size_rounds_to_sz_decimals() {
        let market = btc();
        assert_eq!(round_size(&market, dec("0.123456789")), dec("0.12346"));
        assert_eq!(round_size(&market, dec("1.20000")), dec("1.2"));
    }

    #[test]
    fn price_rounds_to_five_sig_figs() {
        // szDecimals=0 -> perp cap of 6 decimals, so 5 sig figs fit here.
        let market = market(0);
        assert_eq!(round_price(&market, dec("12345.678")), dec("12346"));
        assert_eq!(round_price(&market, dec("0.123456789")), dec("0.12346"));
        assert_eq!(round_price(&market, dec("50000")), dec("50000"));
    }

    #[test]
    fn price_respects_decimal_cap() {
        // szDecimals=5 -> max 1 decimal place for perps.
        let market = btc();
        assert_eq!(round_price(&market, dec("0.123456")), dec("0.1"));
        assert_eq!(round_price(&market, dec("1234.5678")), dec("1234.6"));
    }

    #[test]
    fn build_rejects_bad_orders() {
        let market = btc();
        let base = OrderParams {
            is_buy: true,
            size: dec("1"),
            limit_px: dec("50000"),
            tif: Tif::Gtc,
            reduce_only: false,
            cloid: None,
        };

        assert!(
            build_order_wire(
                &market,
                &OrderParams {
                    size: dec("0"),
                    ..base.clone()
                }
            )
            .is_err()
        );
        assert!(
            build_order_wire(
                &market,
                &OrderParams {
                    limit_px: dec("-1"),
                    ..base.clone()
                }
            )
            .is_err()
        );
        assert!(
            build_order_wire(
                &market,
                &OrderParams {
                    size: dec("0.0000001"),
                    limit_px: dec("100"),
                    ..base.clone()
                }
            )
            .is_err()
        );
        // Notional below $10.
        assert!(
            build_order_wire(
                &market,
                &OrderParams {
                    size: dec("0.0001"),
                    limit_px: dec("100"),
                    ..base.clone()
                }
            )
            .is_err()
        );
    }

    #[test]
    fn build_produces_rounded_wire() {
        let market = btc();
        let wire = build_order_wire(
            &market,
            &OrderParams {
                is_buy: false,
                size: dec("0.123456"),
                limit_px: dec("61000.7"),
                tif: Tif::Alo,
                reduce_only: true,
                cloid: Some("0x0123456789abcdef0123456789abcdef".into()),
            },
        )
        .unwrap();
        assert_eq!(wire.a, 0);
        assert!(!wire.b);
        assert_eq!(wire.p, "61001");
        assert_eq!(wire.s, "0.12346");
        assert!(wire.r);
        assert_eq!(wire.t, OrderType::limit(Tif::Alo));
        assert_eq!(
            wire.c.as_deref(),
            Some("0x0123456789abcdef0123456789abcdef")
        );
    }

    #[test]
    fn aggressive_rounding_never_softens_the_limit() {
        let five_figs = market(0);
        let raw = dec("12345.678");
        let up = round_price_aggressive(&five_figs, raw, true);
        let down = round_price_aggressive(&five_figs, raw, false);
        assert!(up >= raw, "buy {up} < raw {raw}");
        assert!(down <= raw, "sell {down} > raw {raw}");
        assert_eq!(up, dec("12346"));
        assert_eq!(down, dec("12345"));

        // The decimal cap must also round in the safe direction.
        let capped = market(5); // perp: max 1 decimal place
        let raw = dec("1234.5678");
        let buy = round_price_aggressive(&capped, raw, true);
        let sell = round_price_aggressive(&capped, raw, false);
        assert!(buy >= raw, "buy {buy} < raw {raw}");
        assert!(sell <= raw, "sell {sell} > raw {raw}");
        for px in [up, down, buy, sell] {
            assert!(significant_digits(px) <= MAX_PRICE_SIG_FIGS, "{px}");
        }
        assert!(buy.scale() <= max_price_decimals(&capped));
        assert!(sell.scale() <= max_price_decimals(&capped));
    }

    /// Rounded values always satisfy the venue's sig-fig / decimal / lot rules.
    #[test]
    fn rounding_invariants_hold() {
        let market = btc();
        let mut px = dec("0.000001");
        for _ in 0..5000 {
            let rounded = round_price(&market, px);
            // Very small prices may round to zero; the builder rejects those
            // rather than sending them. Any non-zero result must obey the rules.
            assert!(rounded >= Decimal::ZERO, "px {px} -> {rounded}");
            assert!(
                significant_digits(rounded) <= MAX_PRICE_SIG_FIGS,
                "px {px} -> {rounded} has too many sig figs"
            );
            assert!(
                rounded.scale() <= max_price_decimals(&market),
                "px {px} -> {rounded} has too many decimals"
            );
            px *= dec("1.37");
            if px > dec("1000000") {
                px = dec("0.000001");
            }
        }

        let mut sz = dec("0.00001");
        for _ in 0..5000 {
            let rounded = round_size(&market, sz);
            assert!(rounded.scale() <= market.sz_decimals);
            sz *= dec("1.23");
            if sz > dec("100000") {
                sz = dec("0.00001");
            }
        }
    }
}
