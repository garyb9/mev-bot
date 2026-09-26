//! Pluggable trading strategies and the shared cost/edge model (SPEC-0003).
//!
//! Strategies turn market/account [`view`]s into exchange-agnostic
//! [`OrderIntent`]s using the shared [`CostModel`]; risk (SPEC-0004) gates the
//! intents and execution (SPEC-0002) acts on them. The engine feeds events and
//! timers deterministically (SPEC-0003 §8), so a recorded event log replays to
//! identical intents.

pub mod cost;
pub mod event;
pub mod funding;
pub mod id;
pub mod intent;
pub mod paper;
pub mod size;
pub mod strategy;
pub mod view;

pub use cost::{CostModel, FeeRates};
pub use event::{DeterministicRng, Event, FillEvent};
pub use funding::{FundingBasis, FundingConfig};
pub use id::StrategyId;
pub use intent::{OrderIntent, Side, TimeInForce};
pub use paper::{Instrument, PaperExecutor};
pub use size::Sizer;
pub use strategy::{Strategy, StrategyContext, Trigger};
pub use view::{AccountView, BookView, MarketView, OpenOrderView, PositionView, Walk};
