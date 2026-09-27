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
use crate::segment::SegmentWriter;

/// Default REST weight budget per minute (SPEC-0008 §8).
pub const DEFAULT_WEIGHT_PER_MIN: u32 = 300;

/// How close to "now" a page must be before paging stops.
const PAGE_TOLERANCE_MS: u64 = 60_000;

/// Maximum time range covered by one `candleSnapshot` page.
const CANDLE_PAGE_MS: u64 = 6 * 3_600 * 1_000;

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

/// Persisted paging state (SPEC-0008 §8: last fetched time per coin).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotterState {
    /// Last `fundingHistory` time fetched, per coin (ms since the epoch).
    #[serde(default)]
    pub funding_last_ms: BTreeMap<String, u64>,
    /// Last `candleSnapshot` open time fetched, keyed by `coin|interval` (ms).
    #[serde(default)]
    pub candle_last_ms: BTreeMap<String, u64>,
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
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temp = path.with_extension("json.tmp");
        std::fs::write(&temp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&temp, path)?;
        Ok(())
    }

    /// The `startTime` to resume `fundingHistory` for `coin`.
    pub fn funding_start_ms(&self, coin: &str, fallback: u64) -> u64 {
        self.funding_last_ms
            .get(coin)
            .map_or(fallback, |last| last.saturating_add(1))
    }

    /// Record the last fetched `fundingHistory` time for `coin`.
    pub fn set_funding_last(&mut self, coin: impl Into<String>, ms: u64) {
        self.funding_last_ms.insert(coin.into(), ms);
    }

    /// The `startTime` to resume `candleSnapshot` for `coin|interval`.
    pub fn candle_start_ms(&self, coin: &str, interval: &str, fallback: u64) -> u64 {
        self.candle_last_ms
            .get(&candle_key(coin, interval))
            .map_or(fallback, |last| last.saturating_add(1))
    }

    /// Record the last fetched candle open time for `coin|interval`.
    pub fn set_candle_last(&mut self, coin: &str, interval: &str, ms: u64) {
        self.candle_last_ms.insert(candle_key(coin, interval), ms);
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
        })
    }

    /// The `fundingHistory` request body for `coin`, resumed from state.
    pub fn funding_request(&self, coin: &str) -> Value {
        let start = self
            .state
            .funding_start_ms(coin, self.config.funding_backfill_start_ms);
        json!({ "type": "fundingHistory", "coin": coin, "startTime": start })
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
            let response = match self.execute(&job.body).await {
                Ok(response) => Some(response),
                Err(err) => {
                    warn!(error = %err, "rest request failed");
                    None
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
                    let next_at = self.finish_paging(&job.key, start, response.as_ref(), now);
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
        if let Err(err) = self.state.save(&self.config.state_path()) {
            warn!(error = %err, "failed to save snapshotter state");
        }
        debug!("rest snapshotter stopped");
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
            JobKey::Funding(coin) => (self.funding_request(coin), 20, None),
            JobKey::Candles(coin, interval) => {
                let start = self.state.candle_start_ms(
                    coin,
                    interval,
                    self.config.candle_backfill_start_ms,
                );
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

    async fn execute(&mut self, body: &Value) -> Result<RawResponse, RestError> {
        match self.client.post_info(body).await {
            Ok(response) => {
                self.record_rest(body, &response);
                Ok(response)
            }
            Err(err) => {
                self.record_gap(&err.to_string());
                Err(err)
            }
        }
    }

    fn record_rest(&mut self, request: &Value, response: &RawResponse) {
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
        if !self.sink.send(env) {
            warn!(src = %self.config.src, conn = %self.config.conn, "rest envelope dropped");
        }
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

    /// Update paging state from a response and return the next run time.
    fn finish_paging(
        &mut self,
        key: &JobKey,
        start_ms: u64,
        response: Option<&RawResponse>,
        now: Instant,
    ) -> Instant {
        let now_ms = self.clock.now_ms();
        let max = response
            .and_then(|response| max_field(&response.body, key.time_field()))
            .unwrap_or(0);
        let progressed = max > 0 && max >= start_ms;
        if progressed {
            match key {
                JobKey::Funding(coin) => self.state.set_funding_last(coin.clone(), max),
                JobKey::Candles(coin, interval) => {
                    self.state.set_candle_last(coin, interval, max);
                }
                _ => {}
            }
        }
        if let Err(err) = self.state.save(&self.config.state_path()) {
            warn!(error = %err, "failed to save snapshotter state");
        }
        let caught_up = max.saturating_add(PAGE_TOLERANCE_MS) >= now_ms;
        if progressed && !caught_up {
            now
        } else {
            now + self.config.daily_interval
        }
    }
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

/// Weight for a candle page: 20 plus 20 per 60 candles (SPEC-0008 §8).
fn candle_weight(interval: &str, start_ms: u64, end_ms: u64) -> u32 {
    let step = interval_ms(interval).max(1);
    let candles = end_ms.saturating_sub(start_ms) / step;
    20 + (20 * (candles / 60)) as u32
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    use super::*;
    use crate::envelope::{Kind, SystemEnvelopeClock};
    use crate::reader::read_envelopes;
    use crate::segment::{SegmentConfig, SegmentWriter};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(tag: &str) -> PathBuf {
        let n = TEMP_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("mev-rec-rest-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
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

        let dir = temp_dir("raw");
        let clock: Arc<dyn EnvelopeClock> = Arc::new(SystemEnvelopeClock::new());
        let writer = SegmentWriter::spawn(segment_config(&dir, clock.clone())).unwrap();
        let sink: Arc<dyn EnvelopeSink> = Arc::new(writer);
        let config = SnapshotterConfig {
            base_url: server.uri(),
            out_dir: dir.clone(),
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
        files_with_ext(&dir, "zst", &mut files);
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

        let dir = temp_dir("page");
        let clock: Arc<dyn EnvelopeClock> = Arc::new(SystemEnvelopeClock::new());
        let writer = SegmentWriter::spawn(segment_config(&dir, clock.clone())).unwrap();
        let sink: Arc<dyn EnvelopeSink> = Arc::new(writer);
        let config = SnapshotterConfig {
            base_url: server.uri(),
            out_dir: dir.clone(),
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
        assert_eq!(state.funding_last_ms.get("BTC"), Some(&5000));

        let restarted = RestSnapshotter::new(config, Arc::new(NoopSink), clock).unwrap();
        assert_eq!(restarted.funding_request("BTC")["startTime"], 5001);
    }

    #[test]
    fn candle_weight_adds_surcharge() {
        assert_eq!(candle_weight("1m", 0, 6 * 3_600_000), 20 + 20 * 6);
        assert_eq!(candle_weight("1h", 0, 6 * 3_600_000), 20);
    }

    #[test]
    fn state_round_trips() {
        let dir = temp_dir("state");
        let path = dir.join("hl-rest-state.json");
        let mut state = SnapshotterState::default();
        state.set_funding_last("BTC", 1000);
        state.set_candle_last("BTC", "1m", 2000);
        state.save(&path).unwrap();
        let back = SnapshotterState::load(&path).unwrap();
        assert_eq!(back, state);
        assert_eq!(state.funding_start_ms("BTC", 0), 1001);
        assert_eq!(state.candle_start_ms("BTC", "1m", 0), 2001);
        assert_eq!(state.funding_start_ms("ETH", 7), 7);
    }
}
