//! Pluggable trading strategies and the shared cost/edge model.
//!
//! See SPEC-0003. Strategies turn market/account state into [`OrderIntent`]s;
//! risk (SPEC-0004) gates them and execution (SPEC-0002) acts on them.

/// Which side of the book an intent is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// Buy / bid.
    Buy,
    /// Sell / ask.
    Sell,
}

/// A proposed order, exchange-agnostic and unsigned.
#[derive(Debug, Clone)]
pub struct OrderIntent {
    /// Market symbol (e.g. `BTC`, `@107`).
    pub coin: String,
    /// Direction.
    pub side: Side,
    /// Limit price, if any (`None` = aggressive/market).
    pub limit_px: Option<rust_decimal::Decimal>,
    /// Size in base units.
    pub size: rust_decimal::Decimal,
    /// Human-readable justification, logged for audit.
    pub rationale: String,
}
