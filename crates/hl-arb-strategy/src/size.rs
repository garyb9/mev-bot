//! Position sizing with venue constraints (SPEC-0003 §5).
//!
//! The sizer is intentionally tiny: strategies choose a target notional or
//! size, and the sizer rounds to the market's lot, enforces the minimum
//! notional, and refuses anything outside the configured bounds.

use rust_decimal::{Decimal, RoundingStrategy};
use serde::{Deserialize, Serialize};

use hl_arb_client::order::MIN_ORDER_NOTIONAL;

/// Rounds and validates order sizes against notional bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sizer {
    /// Minimum order notional in USD.
    pub min_notional: Decimal,
    /// Maximum order notional in USD; `None` means uncapped here.
    pub max_notional: Option<Decimal>,
}

impl Default for Sizer {
    fn default() -> Self {
        Self {
            min_notional: MIN_ORDER_NOTIONAL,
            max_notional: None,
        }
    }
}

impl Sizer {
    /// A sizer with an explicit minimum and optional maximum notional.
    pub fn new(min_notional: Decimal, max_notional: Option<Decimal>) -> Self {
        Self {
            min_notional,
            max_notional,
        }
    }

    /// Size in base units for a target notional at `px`.
    pub fn size_for_notional(&self, px: Decimal, notional: Decimal) -> Decimal {
        if px <= Decimal::ZERO {
            return Decimal::ZERO;
        }
        notional / px
    }

    /// Round a size to the market's `sz_decimals` toward zero and normalize.
    ///
    /// Never rounds up, so the sized order can never exceed the requested size
    /// (PERF-003).
    pub fn round(&self, size: Decimal, sz_decimals: u32) -> Decimal {
        size.round_dp_with_strategy(sz_decimals, RoundingStrategy::ToZero)
            .normalize()
    }

    /// Round `desired` to the lot and return it only when the resulting order is
    /// valid: positive, at or above the minimum notional, and within the cap.
    pub fn clamp(&self, desired: Decimal, px: Decimal, sz_decimals: u32) -> Option<Decimal> {
        if px <= Decimal::ZERO || desired <= Decimal::ZERO {
            return None;
        }
        let size = self.round(desired, sz_decimals);
        if size <= Decimal::ZERO {
            return None;
        }
        let notional = size * px;
        if notional < self.min_notional {
            return None;
        }
        if let Some(max) = self.max_notional
            && notional > max
        {
            return None;
        }
        Some(size)
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    fn ds(value: &str) -> Decimal {
        Decimal::from_str(value).unwrap()
    }

    #[test]
    fn size_for_notional_divides() {
        let sizer = Sizer::default();
        assert_eq!(
            sizer.size_for_notional(Decimal::from(50_000), Decimal::from(1_000)),
            ds("0.02")
        );
        assert_eq!(
            sizer.size_for_notional(Decimal::ZERO, Decimal::from(1)),
            Decimal::ZERO
        );
    }

    #[test]
    fn clamp_rounds_to_lot() {
        let sizer = Sizer::new(Decimal::TEN, None);
        // 0.123456 rounds to 0.123 at 3 decimals.
        let size = sizer
            .clamp(ds("0.123456"), Decimal::from(60_000), 3)
            .unwrap();
        assert_eq!(size, ds("0.123"));
    }

    #[test]
    fn clamp_rejects_below_min_notional() {
        let sizer = Sizer::new(Decimal::from(10), None);
        // 0.0001 BTC @ 60k = $6 < $10.
        assert!(
            sizer
                .clamp(ds("0.0001"), Decimal::from(60_000), 4)
                .is_none()
        );
    }

    #[test]
    fn clamp_rejects_above_cap() {
        let sizer = Sizer::new(Decimal::TEN, Some(Decimal::from(100)));
        assert!(sizer.clamp(ds("1"), Decimal::from(200), 2).is_none());
        assert!(sizer.clamp(ds("0.5"), Decimal::from(200), 2).is_some());
    }

    #[test]
    fn clamp_rejects_zero_and_negative() {
        let sizer = Sizer::default();
        assert!(sizer.clamp(Decimal::ZERO, Decimal::from(100), 2).is_none());
        assert!(sizer.clamp(ds("-1"), Decimal::from(100), 2).is_none());
        assert!(sizer.clamp(ds("1"), Decimal::ZERO, 2).is_none());
    }

    #[test]
    fn clamp_rejects_size_rounding_to_zero() {
        let sizer = Sizer::default();
        // 0.0001 rounds to 0.000 at 3 decimals.
        assert!(sizer.clamp(ds("0.0001"), Decimal::from(1000), 3).is_none());
    }

    #[test]
    fn round_truncates_toward_zero() {
        let sizer = Sizer::default();
        assert_eq!(sizer.round(ds("1.9"), 0), ds("1"));
        assert_eq!(sizer.round(ds("0.129"), 2), ds("0.12"));
    }

    /// The clamped size never exceeds the requested size, for every lot size.
    #[test]
    fn clamp_never_exceeds_requested_size() {
        let sizer = Sizer::new(Decimal::ZERO, None);
        for sz_decimals in 0..=5u32 {
            let mut desired = ds("0.000001");
            for _ in 0..5_000 {
                if let Some(size) = sizer.clamp(desired, Decimal::from(100), sz_decimals) {
                    assert!(
                        size <= desired,
                        "sz_decimals={sz_decimals}: {desired} -> {size} rounded up"
                    );
                }
                desired *= ds("1.37");
                if desired > ds("1000000") {
                    desired = ds("0.000001");
                }
            }
        }
    }
}
