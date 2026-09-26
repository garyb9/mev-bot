//! Event-driven engine core (SPEC-0010).
//!
//! This crate owns the shared types and the pieces of the engine that are not
//! the binary: interned ids and events ([`types`]), typed ingest decoders and
//! the market/account channels ([`ingest`]), and — in later tasks — the
//! decision loop, order manager, and exec backends. It sits above
//! `mev-hl-client` (reusing its wire types and frame handling) and below
//! `mev-bot` (which wires it to live I/O).
//!
//! Why a crate and not a module in `mev-bot`: the typed decoders must live
//! beside the types they produce, and `mev-hl-client` cannot depend on
//! `mev-bot`; a crate above `mev-hl-client` keeps the graph acyclic
//! (SPEC-0010 §23 Q1).

pub mod channels;
pub mod ingest;
pub mod routes;
pub mod run;
pub mod state;
pub mod strategies;
pub mod strategy;
pub mod timers;
pub mod types;

pub use state::{AccountState, EngineState, MarketSlot};
pub use strategies::funding::{FundingBasis, FundingConfig};
pub use strategies::mm::{MarketMaker, MmConfig};
pub use strategy::{
    Action, Actions, Ctx, GroupIntent, Interests, OrderEvent, OrderEventKind, Strategy, Stream,
    TimerId,
};
pub use types::{
    AccountSnapshot, AccountUpdate, AssetCtxLite, AssetMetaLite, BOOK_DEPTH, BookSnapshot, Cloid,
    CoinId, CoinRegistry, ConnId, Control, Level, MarketUpdate, PostResult, Px, Side, Stamp, Sz,
    Trade, VenueOrderStatus,
};
