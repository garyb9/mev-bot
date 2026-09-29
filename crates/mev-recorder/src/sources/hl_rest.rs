//! Hyperliquid REST snapshotter (SPEC-0008 §8, task R-5).
//!
//! Schedules the `/info` requests from §8 at their cadences, meters them
//! through a weight token bucket (300/min by default), and records every
//! response as a `kind:"rest"` envelope with the raw body plus
//! `meta.req`/`status`/`latency_us`. `fundingHistory` (and `candleSnapshot`)
//! paging state is persisted to a small JSON file under `out_dir`, so a restart
//! resumes where it left off.
//!
//! Raw bodies are captured with a local `reqwest` POST rather than
//! `mev_hl_client::HttpInfo`, whose `info` decodes into a typed value and
//! discards the exact response text. The recorder must store the bytes
//! unmodified (SPEC-0008 §5.1), so a raw client is required.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mev_hl_client::types::PerpDex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::sync::Notify;
use tracing::{debug, warn};

use crate::envelope::{Envelope, EnvelopeClock};
use crate::mount_guard::MountGuard;
use crate::segment::SegmentWriter;

/// Default REST weight budget per minute (SPEC-0008 §8).
pub const DEFAULT_WEIGHT_PER_MIN: u32 = 300;

/// How close to "now" a page must be before paging stops.
const PAGE_TOLERANCE_MS: u64 = 60_000;

/// Maximum time range covered by one `candleSnapshot` page.
const CANDLE_PAGE_MS: u64 = 6 * 3_600 * 1_000;

/// Maximum `fundingHistory` items returned by one call (SPEC-0008 §8, V-1).
const FUNDING_MAX_PAGE: u32 = 500;

/// Pre-charged weight for a `fundingHistory` request (SPEC-0008 §8, V-1).
///
/// The weight is `20 + 1 per 20 items returned`, but the item count is only
/// known after the response. `candle_weight` likewise charges the page's
/// maximum up front, so charge the 500-item maximum: `20 + 500 / 20 = 45`.
const FUNDING_WEIGHT: u32 = 20 + FUNDING_MAX_PAGE / 20;

/// First retry delay after a failed or dropped request.
const RETRY_BACKOFF_INITIAL: Duration = Duration::from_secs(5);

/// Retry delay cap: failures retry within minutes, never a whole day.
const RETRY_BACKOFF_MAX: Duration = Duration::from_secs(300);

/// A response captured with its raw body intact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawResponse {
    /// HTTP status code.
    pub status: u16,
    /// Exact response body text.
    pub body: String,
    /// Round-trip latency in microseconds.
    pub latency_us: u64,
}

/// Errors raised by the REST snapshotter.
#[derive(Debug, Error)]
pub enum RestError {
    /// The HTTP request failed before a response was received.
    #[error("rest transport error: {0}")]
    Transport(String),
    /// The paging state file could not be read or written.
    #[error("snapshotter state i/o error: {0}")]
    Io(#[from] io::Error),
    /// The paging state file was not valid JSON.
    #[error("snapshotter state json error: {0}")]
    Json(#[from] serde_json::Error),
}

/// A destination for snapshotter envelopes.
pub trait EnvelopeSink: Send + Sync {
    /// Non-blocking send; returns `false` if the envelope was dropped.
    fn send(&self, env: Envelope) -> bool;
}

impl EnvelopeSink for SegmentWriter {
    fn send(&self, env: Envelope) -> bool {
        self.try_send(env)
    }
}

/// A raw JSON `POST /info` client.
#[derive(Debug, Clone)]
pub struct RawInfoClient {
    client: reqwest::Client,
    base_url: String,
}

impl RawInfoClient {
    /// Build a client against a REST base URL (e.g. `Network::rest_url()`).
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into(),
        }
    }

    /// POST `body` to `/info`, capturing the raw response text and latency.
    pub async fn post_info(&self, body: &Value) -> Result<RawResponse, RestError> {
        let url = format!("{}/info", self.base_url.trim_end_matches('/'));
        let started = Instant::now();
        let response = self
            .client
            .post(&url)
            .json(body)
            .send()
            .await
            .map_err(|err| RestError::Transport(err.to_string()))?;
        let status = response.status().as_u16();
        let body = response
            .text()
            .await
            .map_err(|err| RestError::Transport(err.to_string()))?;
        Ok(RawResponse {
            status,
            body,
            latency_us: started.elapsed().as_micros() as u64,
        })
    }
}

/// A weight token bucket: `capacity` weight per `window`, refilled linearly.
#[derive(Debug, Clone)]
pub struct WeightBucket {
    capacity: f64,
    window: Duration,
    tokens: f64,
    last: Instant,
}

impl WeightBucket {
    /// A bucket starting full.
    pub fn new(capacity: u32, window: Duration) -> Self {
        let capacity = capacity as f64;
        Self {
            capacity,
            window,
            tokens: capacity,
            last: Instant::now(),
        }
    }

    /// Try to reserve `weight` at `now`.
    ///
    /// Returns [`Duration::ZERO`] when the reservation succeeded, otherwise the
    /// time to wait before it will succeed. On failure nothing is reserved; the
    /// caller sleeps and retries.
    pub fn acquire_at(&mut self, weight: u32, now: Instant) -> Duration {
        self.refill(now);
        let weight = weight as f64;
        if self.tokens >= weight {
            self.tokens -= weight;
            return Duration::ZERO;
        }
        let rate = self.capacity / self.window.as_secs_f64();
        let deficit = weight - self.tokens;
        let secs = if rate > 0.0 { deficit / rate } else { 0.0 };
        Duration::from_secs_f64(secs.max(0.0))
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        let rate = self.capacity / self.window.as_secs_f64();
        self.tokens = (self.tokens + elapsed * rate).min(self.capacity);
        self.last = now;
    }
}

/// Persisted paging state (SPEC-0008 §8).
///
/// Each value is the **start** of the last page that was fetched and accepted,
/// not its maximum. On restart the page is fetched again, so a crash before the
/// segment writer flushed (or a dropped envelope) cannot leave a permanent
/// hole. The overlap is deliberate: P-1 must de-duplicate it when normalizing.
///
/// State written by the pre-page-start format (a bare `funding_last_ms` /
/// `candle_last_ms` cursor) is intentionally ignored: its cursor pointed at a
/// page maximum that may never have reached disk, so resuming from it could
/// skip the very tail this design protects. The backfill restarts from the
/// configured start instead.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotterState {
    /// Resume time for `fundingHistory`, per coin (ms since the epoch).
    #[serde(default)]
    pub funding_resume_ms: BTreeMap<String, u64>,
    /// Resume time for `candleSnapshot`, keyed by `coin|interval` (ms).
    #[serde(default)]
    pub candle_resume_ms: BTreeMap<String, u64>,
}

impl SnapshotterState {
    /// Load state from `path`; a missing file is an empty state.
    pub fn load(path: &Path) -> Result<Self, RestError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(serde_json::from_str(&text)?),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err.into()),
        }
    }

    /// Atomically write state to `path` (temp file then rename).
    pub fn save(&self, path: &Path) -> Result<(), RestError> {
        self.save_guarded(path, None)
    }

    /// Atomic save with an optional R-14 mount guard.
    ///
    /// When `guard` is set, nothing is written unless the required mount is
    /// still present: a tripped guard or a failed check skips the save silently
    /// (the failure also trips the guard). The directory is **never** created
    /// under a guard — if it does not already exist the save is skipped, so a
    /// disconnected drive can never recreate `out_dir` on the root disk. The
    /// mount is checked again before the rename so a loss between the two steps
    /// leaves the temp file on the old device instead of renaming it.
    pub fn save_guarded(&self, path: &Path, guard: Option<&MountGuard>) -> Result<(), RestError> {
        if let Some(guard) = guard {
            if guard.is_tripped() || guard.check_or_trip().is_err() {
                return Ok(());
            }
            match path.parent() {
                Some(parent) if parent.is_dir() => {}
                _ => return Ok(()),
            }
        } else if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temp = path.with_extension("json.tmp");
        std::fs::write(&temp, serde_json::to_vec_pretty(self)?)?;
        if let Some(guard) = guard
            && (guard.is_tripped() || guard.check_or_trip().is_err())
        {
            return Ok(());
        }
        std::fs::rename(&temp, path)?;
        Ok(())
    }

    /// The `startTime` to resume `fundingHistory` for `coin`.
    pub fn funding_start_ms(&self, coin: &str, fallback: u64) -> u64 {
        self.funding_resume_ms
            .get(coin)
            .copied()
            .unwrap_or(fallback)
    }

    /// Record the start of the last accepted `fundingHistory` page for `coin`.
    pub fn set_funding_resume(&mut self, coin: impl Into<String>, ms: u64) {
        self.funding_resume_ms.insert(coin.into(), ms);
    }

    /// The `startTime` to resume `candleSnapshot` for `coin|interval`.
    pub fn candle_start_ms(&self, coin: &str, interval: &str, fallback: u64) -> u64 {
        self.candle_resume_ms
            .get(&candle_key(coin, interval))
            .copied()
            .unwrap_or(fallback)
    }

    /// Record the start of the last accepted candle page for `coin|interval`.
    pub fn set_candle_resume(&mut self, coin: &str, interval: &str, ms: u64) {
        self.candle_resume_ms.insert(candle_key(coin, interval), ms);
    }
}

fn candle_key(coin: &str, interval: &str) -> String {
    format!("{coin}|{interval}")
}

/// Snapshotter configuration (SPEC-0008 §7.3 `[rest]` + §8).
#[derive(Debug, Clone)]
pub struct SnapshotterConfig {
    /// REST base URL.
    pub base_url: String,
    /// Source id (default `hl-rest`).
    pub src: String,
    /// Connection id (default `hl-rest`).
    pub conn: String,
    /// Directory holding the paging state file.
    pub out_dir: PathBuf,
    /// Weight budget per minute.
    pub weight_per_min: u32,
    /// Universe metadata refresh cadence.
    pub meta_refresh: Duration,
    /// `metaAndAssetCtxs` / `spotMetaAndAssetCtxs` cadence.
    pub ctx_interval: Duration,
    /// `predictedFundings` cadence.
    pub predicted_fundings_interval: Duration,
    /// Coins whose `fundingHistory` is recorded daily.
    pub funding_coins: Vec<String>,
    /// Coins whose candles are backfilled daily.
    pub candle_coins: Vec<String>,
    /// Candle intervals to backfill.
    pub candle_intervals: Vec<String>,
    /// How often the per-coin backfills run once caught up.
    pub daily_interval: Duration,
    /// `startTime` for a coin's first `fundingHistory` fetch.
    pub funding_backfill_start_ms: u64,
    /// `startTime` for a coin's first `candleSnapshot` fetch.
    pub candle_backfill_start_ms: u64,
    /// Optional R-14 mount guard for the state file. When set, the state file
    /// is only written while the required mount is present, and its directory
    /// is never created (it must already exist).
    pub mount_guard: Option<Arc<MountGuard>>,
}

impl Default for SnapshotterConfig {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            src: "hl-rest".to_string(),
            conn: "hl-rest".to_string(),
            out_dir: PathBuf::from("data/rec"),
            weight_per_min: DEFAULT_WEIGHT_PER_MIN,
            meta_refresh: Duration::from_secs(300),
            ctx_interval: Duration::from_secs(60),
            predicted_fundings_interval: Duration::from_secs(300),
            funding_coins: Vec::new(),
            candle_coins: Vec::new(),
            candle_intervals: vec!["1m".into(), "5m".into(), "1h".into()],
            daily_interval: Duration::from_secs(24 * 60 * 60),
            funding_backfill_start_ms: 0,
            candle_backfill_start_ms: 0,
            mount_guard: None,
        }
    }
}

impl SnapshotterConfig {
    /// Path of the persisted paging state file.
    pub fn state_path(&self) -> PathBuf {
        self.out_dir.join("hl-rest-state.json")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum JobKey {
    Meta,
    PerpDexs,
    SpotMeta,
    MetaDex(String),
    MetaAndAssetCtxs,
    SpotMetaAndAssetCtxs,
    PredictedFundings,
    Funding(String),
    Candles(String, String),
}

struct Job {
    key: JobKey,
    body: Value,
    weight: u32,
    period: Option<Duration>,
    next_at: Instant,
}

/// The Hyperliquid `/info` snapshotter task.
pub struct RestSnapshotter {
    client: RawInfoClient,
    sink: Arc<dyn EnvelopeSink>,
    clock: Arc<dyn EnvelopeClock>,
    bucket: WeightBucket,
    state: SnapshotterState,
    config: SnapshotterConfig,
    seq: u64,
    /// In-run paging cursors, past the last accepted page. Empty on a fresh
    /// process, so the first request for a coin resumes from the persisted
    /// page start (re-fetching its last page).
    next_funding: BTreeMap<String, u64>,
    next_candle: BTreeMap<String, u64>,
    /// Current capped backoff per paging job, reset on a successful request.
    backoff: BTreeMap<JobKey, Duration>,
}

impl RestSnapshotter {
    /// Build a snapshotter, loading paging state from `out_dir`.
    pub fn new(
        config: SnapshotterConfig,
        sink: Arc<dyn EnvelopeSink>,
        clock: Arc<dyn EnvelopeClock>,
    ) -> Result<Self, RestError> {
        let state = SnapshotterState::load(&config.state_path())?;
        let bucket = WeightBucket::new(config.weight_per_min, Duration::from_secs(60));
        Ok(Self {
            client: RawInfoClient::new(config.base_url.clone()),
            sink,
            clock,
            bucket,
            state,
            config,
            seq: 0,
            next_funding: BTreeMap::new(),
            next_candle: BTreeMap::new(),
            backoff: BTreeMap::new(),
        })
    }

    /// The `fundingHistory` request body for `coin`, resumed from state.
    pub fn funding_request(&self, coin: &str) -> Value {
        json!({
            "type": "fundingHistory",
            "coin": coin,
            "startTime": self.funding_start(coin),
        })
    }

    /// The next in-run `fundingHistory` cursor for `coin` (the persisted page
    /// start until a page is accepted).
    fn funding_start(&self, coin: &str) -> u64 {
        self.next_funding.get(coin).copied().unwrap_or_else(|| {
            self.state
                .funding_start_ms(coin, self.config.funding_backfill_start_ms)
        })
    }

    /// The next in-run `candleSnapshot` cursor for `coin|interval`.
    fn candle_start(&self, coin: &str, interval: &str) -> u64 {
        self.next_candle
            .get(&candle_key(coin, interval))
            .copied()
            .unwrap_or_else(|| {
                self.state
                    .candle_start_ms(coin, interval, self.config.candle_backfill_start_ms)
            })
    }

    /// Run until `shutdown` is notified.
    pub async fn run(mut self, shutdown: Arc<Notify>) {
        let mut jobs = self.initial_jobs();
        while let Some(index) = jobs
            .iter()
            .enumerate()
            .min_by_key(|(_, job)| job.next_at)
            .map(|(index, _)| index)
        {
            let wait = jobs[index]
                .next_at
                .saturating_duration_since(Instant::now());
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = shutdown.notified() => break,
            }

            let mut job = jobs.remove(index);
            if !self.acquire(job.weight, &shutdown).await {
                break;
            }
            let (response, accepted) = match self.execute(&job.body).await {
                Ok((response, accepted)) => (Some(response), accepted),
                Err(err) => {
                    warn!(error = %err, "rest request failed");
                    (None, false)
                }
            };

            let now = Instant::now();
            match job.key {
                JobKey::PerpDexs => {
                    if let Some(response) = &response {
                        self.add_dex_jobs(&response.body, &mut jobs, now);
                    }
                    job.next_at = now + self.config.meta_refresh;
                    jobs.push(job);
                }
                JobKey::Funding(_) | JobKey::Candles(_, _) => {
                    let start = job_start_ms(&job.body, &job.key);
                    let next_at =
                        self.finish_paging(&job.key, start, response.as_ref(), accepted, now);
                    let mut next = self.build_job(job.key.clone(), now);
                    next.next_at = next_at;
                    jobs.push(next);
                }
                _ => {
                    let period = job.period.unwrap_or(self.config.meta_refresh);
                    job.next_at = now + period;
                    jobs.push(job);
                }
            }
        }
        self.save_state();
        debug!("rest snapshotter stopped");
    }

    /// Persist the paging state through the mount guard (R-14 fix1 §1).
    ///
    /// The guard is checked immediately before each file operation, and a save
    /// that cannot be verified is skipped rather than written to a fallback
    /// location. This is the same path used on exit, so a disconnect during
    /// shutdown cannot recreate `out_dir` on the root disk.
    fn save_state(&self) {
        if let Err(err) = self.state.save_guarded(
            &self.config.state_path(),
            self.config.mount_guard.as_deref(),
        ) {
            warn!(error = %err, "failed to save snapshotter state");
        }
    }

    fn initial_jobs(&self) -> Vec<Job> {
        let now = Instant::now();
        let mut keys = vec![
            JobKey::Meta,
            JobKey::PerpDexs,
            JobKey::SpotMeta,
            JobKey::MetaAndAssetCtxs,
            JobKey::SpotMetaAndAssetCtxs,
            JobKey::PredictedFundings,
        ];
        keys.extend(
            self.config
                .funding_coins
                .iter()
                .cloned()
                .map(JobKey::Funding),
        );
        for coin in &self.config.candle_coins {
            for interval in &self.config.candle_intervals {
                keys.push(JobKey::Candles(coin.clone(), interval.clone()));
            }
        }
        keys.into_iter()
            .map(|key| {
                let mut job = self.build_job(key, now);
                job.next_at = now;
                job
            })
            .collect()
    }

    fn build_job(&self, key: JobKey, now: Instant) -> Job {
        let (body, weight, period) = match &key {
            JobKey::Meta => (
                json!({ "type": "meta" }),
                20,
                Some(self.config.meta_refresh),
            ),
            JobKey::PerpDexs => (
                json!({ "type": "perpDexs" }),
                20,
                Some(self.config.meta_refresh),
            ),
            JobKey::SpotMeta => (
                json!({ "type": "spotMeta" }),
                20,
                Some(self.config.meta_refresh),
            ),
            JobKey::MetaDex(dex) => (
                json!({ "type": "meta", "dex": dex }),
                20,
                Some(self.config.meta_refresh),
            ),
            JobKey::MetaAndAssetCtxs => (
                json!({ "type": "metaAndAssetCtxs" }),
                20,
                Some(self.config.ctx_interval),
            ),
            JobKey::SpotMetaAndAssetCtxs => (
                json!({ "type": "spotMetaAndAssetCtxs" }),
                20,
                Some(self.config.ctx_interval),
            ),
            JobKey::PredictedFundings => (
                json!({ "type": "predictedFundings" }),
                20,
                Some(self.config.predicted_fundings_interval),
            ),
            JobKey::Funding(coin) => (self.funding_request(coin), FUNDING_WEIGHT, None),
            JobKey::Candles(coin, interval) => {
                let start = self.candle_start(coin, interval);
                let end = start + CANDLE_PAGE_MS;
                (
                    json!({
                        "type": "candleSnapshot",
                        "req": {
                            "coin": coin,
                            "interval": interval,
                            "startTime": start,
                            "endTime": end,
                        }
                    }),
                    candle_weight(interval, start, end),
                    None,
                )
            }
        };
        Job {
            key,
            body,
            weight,
            period,
            next_at: now,
        }
    }

    fn add_dex_jobs(&self, body: &str, jobs: &mut Vec<Job>, now: Instant) {
        let dexes: BTreeSet<String> = match serde_json::from_str::<Vec<Option<PerpDex>>>(body) {
            Ok(parsed) => parsed
                .into_iter()
                .flatten()
                .map(|dex| dex.name)
                .filter(|name| !name.is_empty())
                .collect(),
            Err(err) => {
                debug!(error = %err, "could not parse perpDexs; skipping dex metas");
                return;
            }
        };
        for dex in dexes {
            let key = JobKey::MetaDex(dex);
            if jobs.iter().any(|job| job.key == key) {
                continue;
            }
            let mut job = self.build_job(key, now);
            job.next_at = now;
            jobs.push(job);
        }
    }

    async fn acquire(&mut self, weight: u32, shutdown: &Notify) -> bool {
        loop {
            let wait = self.bucket.acquire_at(weight, Instant::now());
            if wait.is_zero() {
                return true;
            }
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = shutdown.notified() => return false,
            }
        }
    }

    async fn execute(&mut self, body: &Value) -> Result<(RawResponse, bool), RestError> {
        match self.client.post_info(body).await {
            Ok(response) => {
                let accepted = self.record_rest(body, &response);
                Ok((response, accepted))
            }
            Err(err) => {
                self.record_gap(&err.to_string());
                Err(err)
            }
        }
    }

    /// Record a REST envelope, returning `false` if the sink dropped it.
    fn record_rest(&mut self, request: &Value, response: &RawResponse) -> bool {
        let meta = json!({
            "req": request,
            "status": response.status,
            "latency_us": response.latency_us,
        });
        let env = Envelope::rest(
            &*self.clock,
            &self.config.src,
            &self.config.conn,
            self.seq,
            response.body.clone(),
            meta,
        );
        self.seq += 1;
        let accepted = self.sink.send(env);
        if !accepted {
            warn!(src = %self.config.src, conn = %self.config.conn, "rest envelope dropped");
        }
        accepted
    }

    fn record_gap(&mut self, detail: &str) {
        let env = Envelope::gap_start(
            &*self.clock,
            &self.config.src,
            &self.config.conn,
            self.seq,
            "error",
            detail,
        );
        self.seq += 1;
        if !self.sink.send(env) {
            warn!(src = %self.config.src, conn = %self.config.conn, "gap envelope dropped");
        }
    }

    /// Advance paging state from an accepted response and return the next run
    /// time.
    ///
    /// `accepted` is `false` when the request failed or its envelope was
    /// dropped. In that case the cursor does not move, so the page is fetched
    /// again (after a capped backoff); a restart re-fetches the last accepted
    /// page from its start, so a crash cannot leave a permanent hole.
    fn finish_paging(
        &mut self,
        key: &JobKey,
        start_ms: u64,
        response: Option<&RawResponse>,
        accepted: bool,
        now: Instant,
    ) -> Instant {
        let now_ms = self.clock.now_ms();
        let max = response
            .and_then(|response| max_field(&response.body, key.time_field()))
            .unwrap_or(0);
        let progressed = accepted && max > 0 && max >= start_ms;
        if progressed {
            // Persist the page *start*; move the in-run cursor past it.
            match key {
                JobKey::Funding(coin) => {
                    self.state.set_funding_resume(coin.clone(), start_ms);
                    self.next_funding
                        .insert(coin.clone(), max.saturating_add(1));
                }
                JobKey::Candles(coin, interval) => {
                    self.state.set_candle_resume(coin, interval, start_ms);
                    self.next_candle
                        .insert(candle_key(coin, interval), max.saturating_add(1));
                }
                _ => {}
            }
            self.backoff.remove(key);
            self.save_state();
            let caught_up = max.saturating_add(PAGE_TOLERANCE_MS) >= now_ms;
            if caught_up {
                now + self.config.daily_interval
            } else {
                now
            }
        } else if accepted && response.is_some() {
            // A successful request that made no progress: caught up for a day.
            self.backoff.remove(key);
            now + self.config.daily_interval
        } else {
            // Dropped or failed: retry within the cap, never a full day.
            let wait = next_backoff(self.backoff.get(key).copied());
            self.backoff.insert(key.clone(), wait);
            now + wait
        }
    }
}

/// The next backoff delay: double the previous, capped.
fn next_backoff(previous: Option<Duration>) -> Duration {
    let next = match previous {
        None => RETRY_BACKOFF_INITIAL,
        Some(previous) => previous.saturating_mul(2),
    };
    next.min(RETRY_BACKOFF_MAX)
}

impl JobKey {
    fn time_field(&self) -> &'static str {
        match self {
            JobKey::Funding(_) => "time",
            JobKey::Candles(_, _) => "t",
            _ => "time",
        }
    }
}

/// The `startTime` encoded in a paging job's body.
fn job_start_ms(body: &Value, key: &JobKey) -> u64 {
    match key {
        JobKey::Funding(_) => body.get("startTime").and_then(Value::as_u64).unwrap_or(0),
        JobKey::Candles(_, _) => body
            .get("req")
            .and_then(|req| req.get("startTime"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        _ => 0,
    }
}

/// The maximum value of `field` across an array response.
fn max_field(body: &str, field: &str) -> Option<u64> {
    let parsed: Vec<Value> = serde_json::from_str(body).ok()?;
    parsed
        .iter()
        .filter_map(|entry| entry.get(field).and_then(Value::as_u64))
        .max()
}

fn interval_ms(interval: &str) -> u64 {
    match interval {
        "1m" => 60_000,
        "5m" => 300_000,
        "1h" => 3_600_000,
        _ => 60_000,
    }
}

/// Weight for a candle page: 20 plus 1 per 60 candles in the window
/// (SPEC-0008 §8; V-1 verified `20 + 1 per 60 items returned`).
fn candle_weight(interval: &str, start_ms: u64, end_ms: u64) -> u32 {
    let step = interval_ms(interval).max(1);
    let candles = end_ms.saturating_sub(start_ms) / step;
    20 + (candles / 60) as u32
}

#[cfg(test)]
mod tests {
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    use super::*;
    use crate::envelope::{Kind, SystemEnvelopeClock};
    use crate::mount_guard::test_support::FakeMountProbe;
    use crate::reader::read_envelopes;
    use crate::segment::{SegmentConfig, SegmentWriter};

    fn temp_dir(tag: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("mev-rec-rest-{tag}-"))
            .tempdir()
            .unwrap()
    }

    fn segment_config(dir: &Path, clock: Arc<dyn EnvelopeClock>) -> SegmentConfig {
        SegmentConfig {
            out_dir: dir.to_path_buf(),
            network: "testnet".into(),
            src: "hl-rest".into(),
            conn: "hl-rest".into(),
            clock,
            ..SegmentConfig::default()
        }
    }

    fn files_with_ext(dir: &Path, ext: &str, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_dir() {
                files_with_ext(&path, ext, out);
            } else if path.extension().and_then(|e| e.to_str()) == Some(ext) {
                out.push(path);
            }
        }
    }

    struct NoopSink;

    impl EnvelopeSink for NoopSink {
        fn send(&self, _env: Envelope) -> bool {
            true
        }
    }

    #[test]
    fn token_bucket_delays_over_budget_requests() {
        let mut bucket = WeightBucket::new(300, Duration::from_secs(60));
        let t0 = Instant::now();
        for _ in 0..15 {
            assert_eq!(bucket.acquire_at(20, t0), Duration::ZERO);
        }
        let wait = bucket.acquire_at(20, t0);
        assert!(
            wait >= Duration::from_millis(3_900) && wait <= Duration::from_millis(4_000),
            "expected ~4s wait, got {wait:?}"
        );
        assert_eq!(
            bucket.acquire_at(20, t0 + Duration::from_secs(4)),
            Duration::ZERO
        );

        let mut capped = WeightBucket::new(300, Duration::from_secs(60));
        assert_eq!(capped.acquire_at(40, t0), Duration::ZERO);
        assert_eq!(
            capped.acquire_at(40, t0 + Duration::from_secs(3_600)),
            Duration::ZERO
        );
    }

    #[tokio::test]
    async fn records_raw_bodies_and_meta() {
        let server = MockServer::start().await;
        let body = r#"{"ok":true,"n":1}"#;
        Mock::given(method("POST"))
            .and(path("/info"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        let tmp = temp_dir("raw");
        let dir = tmp.path();
        let clock: Arc<dyn EnvelopeClock> = Arc::new(SystemEnvelopeClock::new());
        let writer = SegmentWriter::spawn(segment_config(dir, clock.clone())).unwrap();
        let sink: Arc<dyn EnvelopeSink> = Arc::new(writer);
        let config = SnapshotterConfig {
            base_url: server.uri(),
            out_dir: dir.to_path_buf(),
            ..SnapshotterConfig::default()
        };
        let snapshotter = RestSnapshotter::new(config, sink.clone(), clock).unwrap();
        let shutdown = Arc::new(Notify::new());
        let handle = tokio::spawn(snapshotter.run(shutdown.clone()));
        tokio::time::sleep(Duration::from_millis(200)).await;
        shutdown.notify_one();
        handle.await.unwrap();
        drop(sink);

        let mut files = Vec::new();
        files_with_ext(dir, "zst", &mut files);
        assert_eq!(files.len(), 1);
        let envelopes = read_envelopes(&files[0]).unwrap();
        let rest: Vec<&Envelope> = envelopes
            .iter()
            .filter(|env| env.kind == Kind::Rest)
            .collect();
        assert_eq!(rest.len(), 6);
        for env in rest {
            assert_eq!(env.raw.as_deref(), Some(body));
            assert_eq!(env.src, "hl-rest");
            let meta = env.meta.as_ref().unwrap();
            assert_eq!(meta["status"], 200);
            assert!(meta["latency_us"].is_number());
            let req = meta["req"]["type"].as_str().unwrap();
            assert!(!req.is_empty());
        }
    }

    #[tokio::test]
    async fn funding_paging_resumes_from_state_file() {
        let server = MockServer::start().await;
        let body = r#"[{"coin":"BTC","time":5000,"fundingRate":"0.01"}]"#;
        Mock::given(method("POST"))
            .and(path("/info"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        let tmp = temp_dir("page");
        let dir = tmp.path();
        let clock: Arc<dyn EnvelopeClock> = Arc::new(SystemEnvelopeClock::new());
        let writer = SegmentWriter::spawn(segment_config(dir, clock.clone())).unwrap();
        let sink: Arc<dyn EnvelopeSink> = Arc::new(writer);
        let config = SnapshotterConfig {
            base_url: server.uri(),
            out_dir: dir.to_path_buf(),
            funding_coins: vec!["BTC".into()],
            funding_backfill_start_ms: 0,
            ..SnapshotterConfig::default()
        };
        let snapshotter =
            RestSnapshotter::new(config.clone(), sink.clone(), clock.clone()).unwrap();
        let shutdown = Arc::new(Notify::new());
        let handle = tokio::spawn(snapshotter.run(shutdown.clone()));
        tokio::time::sleep(Duration::from_millis(200)).await;
        shutdown.notify_one();
        handle.await.unwrap();
        drop(sink);

        let state = SnapshotterState::load(&config.state_path()).unwrap();
        assert_eq!(state.funding_resume_ms.get("BTC"), Some(&0));

        let restarted = RestSnapshotter::new(config, Arc::new(NoopSink), clock).unwrap();
        assert_eq!(restarted.funding_request("BTC")["startTime"], 0);
    }

    fn paging_response(body: &'static str) -> RawResponse {
        RawResponse {
            status: 200,
            body: body.to_string(),
            latency_us: 1,
        }
    }

    #[test]
    fn restart_refetches_the_last_page() {
        let tmp = temp_dir("restart-page");
        let dir = tmp.path();
        let clock: Arc<dyn EnvelopeClock> = Arc::new(SystemEnvelopeClock::new());
        let config = SnapshotterConfig {
            out_dir: dir.to_path_buf(),
            funding_coins: vec!["BTC".into()],
            funding_backfill_start_ms: 0,
            ..SnapshotterConfig::default()
        };
        let mut snapshotter =
            RestSnapshotter::new(config.clone(), Arc::new(NoopSink), clock.clone()).unwrap();
        let response = paging_response(r#"[{"coin":"BTC","time":9000}]"#);
        let now = Instant::now();

        // Page [4000, 9000] accepted: the in-run cursor advances to 9001.
        snapshotter.finish_paging(
            &JobKey::Funding("BTC".into()),
            4000,
            Some(&response),
            true,
            now,
        );
        assert_eq!(snapshotter.funding_start("BTC"), 9001);
        assert_eq!(snapshotter.state.funding_resume_ms.get("BTC"), Some(&4000));

        // A fresh process re-fetches the last page from its start, not 9001.
        let restarted = RestSnapshotter::new(config, Arc::new(NoopSink), clock).unwrap();
        assert_eq!(restarted.funding_request("BTC")["startTime"], 4000);
    }

    #[test]
    fn dropped_envelope_does_not_advance_state() {
        let tmp = temp_dir("dropped");
        let dir = tmp.path();
        let clock: Arc<dyn EnvelopeClock> = Arc::new(SystemEnvelopeClock::new());
        let config = SnapshotterConfig {
            out_dir: dir.to_path_buf(),
            funding_coins: vec!["BTC".into()],
            funding_backfill_start_ms: 1000,
            ..SnapshotterConfig::default()
        };
        let mut snapshotter = RestSnapshotter::new(config, Arc::new(NoopSink), clock).unwrap();
        let response = paging_response(r#"[{"coin":"BTC","time":9000}]"#);
        let now = Instant::now();

        // `accepted=false` models the sink dropping the rest envelope.
        snapshotter.finish_paging(
            &JobKey::Funding("BTC".into()),
            5000,
            Some(&response),
            false,
            now,
        );
        assert!(!snapshotter.state.funding_resume_ms.contains_key("BTC"));
        assert!(!snapshotter.next_funding.contains_key("BTC"));
        assert_eq!(snapshotter.funding_start("BTC"), 1000);
    }

    #[test]
    fn failed_request_retries_within_the_backoff_cap() {
        let tmp = temp_dir("backoff");
        let dir = tmp.path();
        let clock: Arc<dyn EnvelopeClock> = Arc::new(SystemEnvelopeClock::new());
        let config = SnapshotterConfig {
            out_dir: dir.to_path_buf(),
            funding_coins: vec!["BTC".into()],
            ..SnapshotterConfig::default()
        };
        let mut snapshotter = RestSnapshotter::new(config, Arc::new(NoopSink), clock).unwrap();
        let now = Instant::now();
        let key = JobKey::Funding("BTC".into());

        let mut last = Duration::ZERO;
        for _ in 0..12 {
            let next = snapshotter.finish_paging(&key, 0, None, false, now);
            let delay = next.saturating_duration_since(now);
            assert!(delay > Duration::ZERO);
            assert!(delay <= RETRY_BACKOFF_MAX, "retry waited {delay:?}");
            last = delay;
        }
        assert_eq!(last, RETRY_BACKOFF_MAX);
    }

    #[test]
    fn candle_weight_adds_surcharge() {
        // V-1: `20 + 1 per 60 items`; 6 h of 1m is 360 candles -> 20 + 6.
        assert_eq!(candle_weight("1m", 0, 6 * 3_600_000), 26);
        assert_eq!(candle_weight("1h", 0, 6 * 3_600_000), 20);
    }

    #[test]
    fn funding_weight_charges_the_page_maximum() {
        // V-1: `20 + 1 per 20 items`; the page max is 500 -> 20 + 25.
        assert_eq!(FUNDING_WEIGHT, 45);
        let tmp = temp_dir("funding-weight");
        let config = SnapshotterConfig {
            out_dir: tmp.path().to_path_buf(),
            funding_coins: vec!["BTC".into()],
            ..SnapshotterConfig::default()
        };
        let clock: Arc<dyn EnvelopeClock> = Arc::new(SystemEnvelopeClock::new());
        let snapshotter = RestSnapshotter::new(config, Arc::new(NoopSink), clock).unwrap();
        let job = snapshotter.build_job(JobKey::Funding("BTC".into()), Instant::now());
        assert_eq!(job.weight, 45);
    }

    #[test]
    fn state_round_trips() {
        let tmp = temp_dir("state");
        let dir = tmp.path();
        let path = dir.join("hl-rest-state.json");
        let mut state = SnapshotterState::default();
        state.set_funding_resume("BTC", 1000);
        state.set_candle_resume("BTC", "1m", 2000);
        state.save(&path).unwrap();
        let back = SnapshotterState::load(&path).unwrap();
        assert_eq!(back, state);
        assert_eq!(state.funding_start_ms("BTC", 0), 1000);
        assert_eq!(state.candle_start_ms("BTC", "1m", 0), 2000);
        assert_eq!(state.funding_start_ms("ETH", 7), 7);
    }

    #[test]
    fn legacy_state_format_is_ignored() {
        let tmp = temp_dir("legacy-state");
        let dir = tmp.path();
        let path = dir.join("hl-rest-state.json");
        std::fs::write(
            &path,
            r#"{"funding_last_ms":{"BTC":5000},"candle_last_ms":{"BTC|1m":2000}}"#,
        )
        .unwrap();
        let state = SnapshotterState::load(&path).unwrap();
        assert!(state.funding_resume_ms.is_empty());
        assert!(state.candle_resume_ms.is_empty());
        assert_eq!(state.funding_start_ms("BTC", 7), 7);
    }

    // -- R-14 fix1: the state-file save must be mount-guarded ----------------

    /// Guarded state-file writes must never create the directory, even when the
    /// mount is healthy.
    #[test]
    fn guarded_save_never_creates_the_directory() {
        let mount_tmp = temp_dir("save-mount-dir");
        let out = temp_dir("save-out-dir");
        let probe = Arc::new(FakeMountProbe::new(mount_tmp.path()));
        let guard = MountGuard::new(Some(mount_tmp.path().to_path_buf()), probe);
        let path = out.path().join("missing/sub/hl-rest-state.json");

        SnapshotterState::default()
            .save_guarded(&path, Some(&guard))
            .unwrap();
        assert!(
            !out.path().join("missing").exists(),
            "a guarded save must not create directories"
        );
    }

    #[test]
    fn guarded_save_writes_while_the_mount_is_present() {
        let mount_tmp = temp_dir("save-mount-ok");
        let out = temp_dir("save-out-ok");
        let probe = Arc::new(FakeMountProbe::new(mount_tmp.path()));
        let guard = MountGuard::new(Some(mount_tmp.path().to_path_buf()), probe);
        let path = out.path().join("hl-rest-state.json");

        SnapshotterState::default()
            .save_guarded(&path, Some(&guard))
            .unwrap();
        assert!(
            path.exists(),
            "a guarded save must still write while mounted"
        );
    }

    #[test]
    fn guarded_save_creates_nothing_when_the_mount_is_gone() {
        let mount_tmp = temp_dir("save-mount-gone");
        let out = temp_dir("save-out-gone");
        let probe = Arc::new(FakeMountProbe::new(mount_tmp.path()));
        let guard = MountGuard::new(Some(mount_tmp.path().to_path_buf()), probe.clone());
        probe.set_mounted(false);

        let path = out.path().join("hl-rest-state.json");
        SnapshotterState::default()
            .save_guarded(&path, Some(&guard))
            .unwrap();
        assert!(!path.exists(), "state written while unmounted");
        assert!(!path.with_extension("json.tmp").exists());
        assert_eq!(std::fs::read_dir(out.path()).unwrap().count(), 0);
        assert!(
            guard.is_tripped(),
            "a failed save check must trip the guard"
        );
    }

    /// The exit save uses the same guarded path, so a disconnect during
    /// shutdown cannot recreate `out_dir` on the root disk.
    #[test]
    fn guarded_exit_save_creates_nothing_when_the_mount_is_gone() {
        let mount_tmp = temp_dir("exit-mount");
        let out = temp_dir("exit-out");
        let probe = Arc::new(FakeMountProbe::new(mount_tmp.path()));
        let guard = Arc::new(MountGuard::new(
            Some(mount_tmp.path().to_path_buf()),
            probe.clone(),
        ));
        probe.set_mounted(false);

        let config = SnapshotterConfig {
            out_dir: out.path().to_path_buf(),
            mount_guard: Some(guard.clone()),
            ..SnapshotterConfig::default()
        };
        let clock: Arc<dyn EnvelopeClock> = Arc::new(SystemEnvelopeClock::new());
        let snapshotter = RestSnapshotter::new(config, Arc::new(NoopSink), clock).unwrap();

        snapshotter.save_state();
        assert_eq!(std::fs::read_dir(out.path()).unwrap().count(), 0);
        assert!(!out.path().join("hl-rest-state.json").exists());
    }
}
