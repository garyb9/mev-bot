//! Market-data recorder (SPEC-0008 Part A).
//!
//! The recorder stores raw Hyperliquid (and reference-venue) market data as
//! newline-delimited JSON envelopes compressed with zstd into rotating segment
//! files. It never loads keys and never places orders.
//!
//! This crate provides the envelope format, the per-connection segment writer
//! and reader, the subscription planner, and the REST snapshotter.

pub mod envelope;
pub mod planner;
pub mod reader;
pub mod segment;
pub mod sources;

pub use envelope::{
    Envelope, EnvelopeClock, FixedEnvelopeClock, Kind, MonoClock, SCHEMA_VERSION, SegmentOpenMeta,
    SystemEnvelopeClock,
};
pub use planner::{
    Connection, HlProfile, MAX_SUBSCRIBE_MSGS_PER_SEC, MIN_NEW_CONN_INTERVAL, Pacer, Plan,
    PlanLimits, PlanTotals, PlannerError, Stream, Subscription, VolumeIndex,
};
pub use reader::{
    FileCheck, InspectReport, MergeIter, MissingSeqs, ReaderError, SegmentReader, SeqHoles,
    SeqRange, StreamCoverage, VerifyConfig, VerifyReport,
};
pub use segment::{
    DiskSpace, ManifestEntry, SegmentConfig, SegmentError, SegmentWriter, SystemDiskSpace,
};
pub use sources::hl_rest::{
    DEFAULT_WEIGHT_PER_MIN, EnvelopeSink, RawInfoClient, RawResponse, RestError, RestSnapshotter,
    SnapshotterConfig, SnapshotterState, WeightBucket,
};
