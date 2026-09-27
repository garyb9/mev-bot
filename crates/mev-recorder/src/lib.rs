//! Market-data recorder (SPEC-0008 Part A).
//!
//! The recorder stores raw Hyperliquid (and reference-venue) market data as
//! newline-delimited JSON envelopes compressed with zstd into rotating segment
//! files. It never loads keys and never places orders.
//!
//! This crate currently provides the envelope format and the per-connection
//! segment writer; sources, the planner, and the reader land in later tasks.

pub mod envelope;
pub mod segment;

pub use envelope::{
    Envelope, EnvelopeClock, FixedEnvelopeClock, Kind, MonoClock, SCHEMA_VERSION, SegmentOpenMeta,
    SystemEnvelopeClock,
};
pub use segment::{DiskSpace, SegmentConfig, SegmentError, SegmentWriter, SystemDiskSpace};
