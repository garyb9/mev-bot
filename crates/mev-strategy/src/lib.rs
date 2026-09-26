//! Strategy building blocks and the shared cost/edge model (SPEC-0003).
//!
//! This crate holds the venue-agnostic pieces strategies use: the
//! [`CostModel`]/[`FeeRates`], market/account [`view`]s, [`OrderIntent`]s,
//! sizing, the paper executor, and deterministic randomness. The synchronous
//! [`mev_engine::Strategy`] trait the engine drives lives in `mev-engine`
//! (SPEC-0010 §8): its context names engine types, and `mev-engine` already
//! depends on this crate, so a trait here would invert the dependency.
//!
//! Strategies turn views into exchange-agnostic [`OrderIntent`]s; risk
//! (SPEC-0004) gates them and execution (SPEC-0002) acts on them. The engine
//! feeds events and timers deterministically, so a recorded event log replays
//! to identical intents.

pub mod cost;
pub mod event;
pub mod id;
pub mod intent;
pub mod paper;
pub mod size;
pub mod view;

pub use cost::{CostModel, FeeRates};
pub use event::{DeterministicRng, Event, FillEvent};
pub use id::StrategyId;
pub use intent::{OrderIntent, Side, TimeInForce};
pub use paper::{Instrument, PaperExecutor};
pub use size::Sizer;
pub use view::{AccountView, BookView, MarketView, OpenOrderView, PositionView, Walk};
