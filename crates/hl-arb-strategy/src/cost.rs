//! Shared fee, slippage, and funding model (SPEC-0003 §5).
//!
//! Every trade must be justified net of fees, funding carry, and slippage, plus
//! a configurable buffer. The math is deliberately primitive and deterministic
//! so replay and unit tests can pin it exactly.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::intent::Side;
use crate::view::BookView;

/// Maker/taker fee rates as fractions of notional (e.g. `0.00015` = 1.5 bps).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeeRates {
    /// Maker (add-liquidity) rate.
    pub maker: Decimal,
    /// Taker (cross) rate.
    pub taker: Decimal,
}

impl FeeRates {
    /// Base perp rates: 0.015% maker / 0.045% taker.
    pub const PERP: Self = Self {
        maker: Decimal::from_parts(15, 0, 0, false, 5),
        taker: Decimal::from_parts(45, 0, 0, false, 5),
    };

    /// Base spot rates: 0.04% maker / 0.07% taker.
    pub const SPOT: Self = Self {
        maker: Decimal::from_parts(4, 0, 0, false, 4),
        taker: Decimal::from_parts(7, 0, 0, false, 4),
    };

    /// The rate for the given liquidity role.
    pub fn rate(&self, maker: bool) -> Decimal {
        if maker { self.maker } else { self.taker }
    }

    /// The rate in basis points.
    pub fn bps(&self, maker: bool) -> Decimal {
        self.rate(maker) * Decimal::from(10_000)
    }
}

/// The shared cost/edge model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CostModel {
    /// Safety margin (bps) subtracted from every edge calculation.
    pub buffer_bps: Decimal,
}

impl Default for CostModel {
    fn default() -> Self {
        Self {
            buffer_bps: Decimal::ZERO,
        }
    }
}

impl CostModel {
    /// A model with the given buffer in basis points.
    pub fn new(buffer_bps: Decimal) -> Self {
        Self { buffer_bps }
    }

    /// Net edge in bps: `gross − costs − buffer`.
    pub fn net_edge_bps(&self, gross_bps: Decimal, cost_bps: Decimal) -> Decimal {
        gross_bps - cost_bps - self.buffer_bps
    }

    /// Whether a net edge clears the (implicit) zero line after buffer.
    pub fn is_positive(&self, gross_bps: Decimal, cost_bps: Decimal) -> bool {
        self.net_edge_bps(gross_bps, cost_bps) > Decimal::ZERO
    }

    /// Fee cost in bps for one fill at the given rates.
    pub fn fee_bps(&self, rates: FeeRates, maker: bool) -> Decimal {
        rates.bps(maker)
    }

    /// Round-trip fee cost (open + close) for one leg.
    pub fn round_trip_fee_bps(&self, rates: FeeRates, maker: bool) -> Decimal {
        self.fee_bps(rates, maker) * Decimal::TWO
    }

    /// Expected slippage in bps for marketable size against `book`.
    ///
    /// Returns `None` when the book lacks the depth to fill `size` (the trade
    /// cannot be priced and should be refused).
    pub fn slippage_bps(&self, book: &BookView, side: Side, size: Decimal) -> Option<Decimal> {
        if size <= Decimal::ZERO {
            return Some(Decimal::ZERO);
        }
        let mid = book.mid()?;
        if mid <= Decimal::ZERO {
            return None;
        }
        let walk = book.walk(side, size);
        if walk.filled < size {
            return None;
        }
        let diff = match side {
            Side::Buy => walk.avg_px - mid,
            Side::Sell => mid - walk.avg_px,
        };
        Some(diff / mid * Decimal::from(10_000))
    }

    /// Funding carry in bps of notional: `rate × hours`, in bps.
    pub fn funding_bps(&self, hourly_rate: Decimal, hours: Decimal) -> Decimal {
        hourly_rate * hours * Decimal::from(10_000)
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    fn ds(value: &str) -> Decimal {
        Decimal::from_str(value).unwrap()
    }

    fn book() -> BookView {
        BookView {
            bids: vec![(Decimal::from(99), Decimal::ONE)],
            asks: vec![(Decimal::from(101), Decimal::ONE)],
            sz_decimals: 2,
            time: 0,
        }
    }

    #[test]
    fn base_fee_rates_in_bps() {
        assert_eq!(FeeRates::PERP.bps(true), ds("1.5"));
        assert_eq!(FeeRates::PERP.bps(false), ds("4.5"));
        assert_eq!(FeeRates::SPOT.bps(true), ds("4"));
        assert_eq!(FeeRates::SPOT.bps(false), ds("7"));
    }

    #[test]
    fn net_edge_subtracts_costs_and_buffer() {
        let model = CostModel::new(Decimal::from(5));
        assert_eq!(model.net_edge_bps(ds("20"), ds("11")), ds("4"));
        assert!(model.is_positive(ds("20"), ds("11")));
        assert!(!model.is_positive(ds("14"), ds("11")));
    }

    #[test]
    fn round_trip_is_double_one_way() {
        let model = CostModel::default();
        assert_eq!(model.round_trip_fee_bps(FeeRates::PERP, false), ds("9"));
    }

    #[test]
    fn slippage_from_book_walk() {
        let model = CostModel::default();
        // mid = 100; buying the single ask at 101 is 1% = 100 bps.
        assert_eq!(
            model.slippage_bps(&book(), Side::Buy, Decimal::ONE),
            Some(ds("100"))
        );
        // Selling the bid at 99 is symmetric.
        assert_eq!(
            model.slippage_bps(&book(), Side::Sell, Decimal::ONE),
            Some(ds("100"))
        );
    }

    #[test]
    fn slippage_refuses_insufficient_depth() {
        let model = CostModel::default();
        assert_eq!(
            model.slippage_bps(&book(), Side::Buy, Decimal::from(2)),
            None
        );
    }

    #[test]
    fn funding_carry_in_bps() {
        let model = CostModel::default();
        // 1 bp/hour over 8 hours = 8 bps.
        assert_eq!(model.funding_bps(ds("0.0001"), ds("8")), ds("8"));
    }
}
