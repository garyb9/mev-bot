//! Core primitives shared across the trading system.
//!
//! This crate is intentionally dependency-light and depends on no other internal
//! crate. It owns shared types, errors, the clock abstraction, and config.

pub mod clock;
pub mod error;

/// Human-readable name of the binary/product.
pub const NAME: &str = "mev-bot";

/// Crate version, sourced from Cargo metadata.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
