//! Market-data recorder CLI and wiring (SPEC-0008 Part A, task R-6).
//!
//! `hl record` resolves a recording profile into a subscription [`Plan`]
//! (SPEC-0008 §7.4), opens one [`RawWsConn`] per planned connection, writes the
//! raw frames through one [`SegmentWriter`] per `(src, conn)`, runs the REST
//! snapshotter (R-5), emits the periodic `clock` envelope (§11), serves
//! `/healthz` `/readyz` `/metrics` (§12.2), and shuts down cleanly on
//! SIGTERM/SIGINT (a `gap_start{shutdown}` on every connection, then finalized
//! segments).
//!
//! This module never loads keys and never places orders (§3): it only reads
//! public market data.
//!
//! The subcommands are `hl record` (run), `hl record plan`, `hl record inspect`,
//! `hl record verify`, and `hl probe latency` (§12.1). The CLI surface itself is
//! declared in `main.rs`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use figment::{
    Figment,
    providers::{Env, Format, Toml},
};
use futures_util::{SinkExt, StreamExt};
use hl_arb_client::{
    AssetMap, HlProtocol, HttpInfo, InfoApi, MarketKind, Protocol, RawEvent, RawWsConn,
};
use hl_arb_core::config::Network;
use hl_arb_metrics::{health::Health, names};
use hl_arb_recorder::{
    Connection, DiskSpace, Envelope, EnvelopeClock, EnvelopeSink, HlProfile, Kind,
    MOUNT_RECHECK_INTERVAL, MountGuard, SegmentConfig, SegmentOpenMeta, SegmentWriter,
    SystemDiskSpace, SystemEnvelopeClock, SystemMountProbe, VolumeIndex,
    planner::{Stream, plan as build_plan},
    reader,
    sources::{
        cex::{CexConfig, CexKind, CexSource},
        deribit::{DEFAULT_BASE_URL, DeribitConfig, DeribitSource},
        hl_rest::{RestSnapshotter, SnapshotterConfig},
    },
};
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{Notify, watch};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

/// Watchdog window used for `/readyz` (matches `RawWsConn`'s default).
const READY_WATCHDOG: Duration = hl_arb_client::raw_ws::DEFAULT_WATCHDOG;
/// A REST feed older than this makes the recorder not ready.
const REST_READY_STALE: Duration = Duration::from_secs(300);
/// How often the free-disk gauge is sampled.
const DISK_SAMPLE_INTERVAL: Duration = Duration::from_secs(30);
/// How often `/readyz` is evaluated.
const READY_SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
/// Delay before retrying a failed initial WebSocket dial.
const CONNECT_RETRY: Duration = Duration::from_secs(3);
/// Funding backfill window on a fresh state file.
const FUNDING_BACKFILL_DAYS: u64 = 30;
/// Candle backfill window on a fresh state file (1m history is short).
const CANDLE_BACKFILL_DAYS: u64 = 7;
/// Maximum `fundingHistory` items returned by one call (SPEC-0008 §8, V-1).
const FUNDING_MAX_PAGE: u32 = 500;
/// Upper bound on one mount probe run from the async watchdog (R-14 fix1 §4).
/// A probe that exceeds it is a failed check and trips the guard.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

mod clock;
mod config;
mod connection;
mod inspect;
mod monitor;
mod probe;
mod runner;
mod sources;

#[cfg(test)]
mod tests;

pub(crate) use config::network_dir;
pub use config::plan;
pub use inspect::{inspect, repair_manifest, verify};
pub use probe::probe_latency;
pub use runner::run;
