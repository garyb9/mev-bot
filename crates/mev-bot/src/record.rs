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
use mev_core::config::Network;
use mev_hl_client::{
    AssetMap, HlProtocol, HttpInfo, InfoApi, MarketKind, Protocol, RawEvent, RawWsConn,
};
use mev_metrics::{health::Health, names};
use mev_recorder::{
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
const READY_WATCHDOG: Duration = mev_hl_client::raw_ws::DEFAULT_WATCHDOG;
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

// ---------------------------------------------------------------------------
// Configuration (config/record.toml)
// ---------------------------------------------------------------------------

/// Top-level `config/record.toml`.
#[derive(Debug, Clone, Deserialize, Default)]
struct RecordConfig {
    #[serde(default)]
    profile: BTreeMap<String, Profile>,
}

/// One `[profile.<name>]` block (SPEC-0008 §7.3).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct Profile {
    /// `mainnet` or `testnet`.
    network: String,
    /// Root of the recording tree.
    out_dir: PathBuf,
    /// zstd compression level.
    zstd_level: i32,
    /// Stop recording when free disk drops below this many GiB.
    min_free_gb: u64,
    /// Absolute mount path the recorder must write under, if any (R-14). When
    /// set, startup fails unless it is a real mount and `out_dir` is inside it,
    /// and the stream stops if the mount disappears. Unset = current behaviour.
    require_mount: Option<PathBuf>,
    /// Optional mount source (e.g. `E:\`) that `require_mount` must also match,
    /// case-insensitively (R-14 fix1 §7). Ignored unless `require_mount` is set.
    require_mount_source: Option<String>,
    /// Local retention in days (applied by R-10 shipping, not yet here).
    #[allow(dead_code)]
    retain_days: u64,
    /// Metadata refresh cadence in seconds.
    meta_refresh_secs: u64,
    /// Port for `/healthz` `/readyz` `/metrics`.
    http_port: u16,
    /// Hyperliquid WS subscriptions.
    hl: HlSection,
    /// Hyperliquid REST snapshotter.
    rest: RestSection,
    /// Deribit public options summaries (R-12).
    deribit: DeribitSection,
    /// Reference CEX symbols (R-8).
    cex: CexSection,
    /// HyperEVM pool source (R-9, not started yet).
    #[allow(dead_code)]
    hyperevm: HyperevmSection,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            network: "mainnet".to_string(),
            out_dir: PathBuf::from("data/rec"),
            zstd_level: 3,
            min_free_gb: 20,
            require_mount: None,
            require_mount_source: None,
            retain_days: 30,
            meta_refresh_secs: 300,
            http_port: 9091,
            hl: HlSection::default(),
            rest: RestSection::default(),
            deribit: DeribitSection::default(),
            cex: CexSection::default(),
            hyperevm: HyperevmSection::default(),
        }
    }
}

/// The `[profile.<name>.hl]` block.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct HlSection {
    bbo: Vec<String>,
    trades: Vec<String>,
    active_asset_ctx: Vec<String>,
    all_mids: bool,
    l2book: Vec<String>,
    max_subs: usize,
    subs_per_conn: usize,
    connections: usize,
    allow_truncate: bool,
}

impl Default for HlSection {
    fn default() -> Self {
        Self {
            bbo: Vec::new(),
            trades: Vec::new(),
            active_asset_ctx: Vec::new(),
            all_mids: false,
            l2book: Vec::new(),
            max_subs: 900,
            subs_per_conn: 150,
            connections: 8,
            allow_truncate: false,
        }
    }
}

impl HlSection {
    /// The planner profile, with the CLI `--allow-truncate` flag ORed in.
    fn to_planner(&self, allow_truncate: bool) -> HlProfile {
        HlProfile {
            bbo: self.bbo.clone(),
            trades: self.trades.clone(),
            active_asset_ctx: self.active_asset_ctx.clone(),
            all_mids: self.all_mids,
            l2book: self.l2book.clone(),
            max_subs: self.max_subs,
            subs_per_conn: self.subs_per_conn,
            connections: self.connections,
            allow_truncate: allow_truncate || self.allow_truncate,
        }
    }

    /// Whether resolving this profile needs HIP-3 dex metadata.
    fn wants_hip3(&self) -> bool {
        self.all_mids
            || self
                .bbo
                .iter()
                .chain(&self.trades)
                .chain(&self.active_asset_ctx)
                .chain(&self.l2book)
                .any(|selector| selector.starts_with("hip3:"))
    }
}

/// The `[profile.<name>.rest]` block.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct RestSection {
    enabled: bool,
    weight_per_min: u32,
}

impl Default for RestSection {
    fn default() -> Self {
        Self {
            enabled: true,
            weight_per_min: 300,
        }
    }
}

/// The `[profile.<name>.deribit]` block (R-12, SPEC-0008 §9.1).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct DeribitSection {
    /// Whether to poll Deribit's public endpoints. Off by default so existing
    /// profiles and recordings are unchanged.
    enabled: bool,
    /// Currencies to poll (`BTC`, `ETH`, …).
    currencies: Vec<String>,
    /// JSON-RPC base URL (public, unauthenticated endpoint).
    base_url: String,
}

impl Default for DeribitSection {
    fn default() -> Self {
        Self {
            enabled: false,
            currencies: vec!["BTC".to_string(), "ETH".to_string()],
            base_url: DEFAULT_BASE_URL.to_string(),
        }
    }
}

/// Validate every profile after parsing (config errors are fatal at startup).
fn validate_config(config: &RecordConfig) -> Result<()> {
    for (name, profile) in &config.profile {
        profile
            .validate()
            .with_context(|| format!("profile `{name}`"))?;
    }
    Ok(())
}

impl Profile {
    /// Reject profiles that would start a source with no work to do.
    fn validate(&self) -> Result<()> {
        if self.deribit.enabled && self.deribit.currencies.is_empty() {
            bail!("deribit.enabled = true but deribit.currencies is empty");
        }
        Ok(())
    }
}

/// Parse a recorder config from figment providers and validate every profile.
fn parse_config(figment: Figment) -> Result<RecordConfig> {
    let config: RecordConfig = figment.extract().context("loading config/record.toml")?;
    validate_config(&config)?;
    Ok(config)
}

/// The `[profile.<name>.cex]` block (R-8).
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
struct CexSection {
    binance_usdm: Vec<String>,
    binance_spot: Vec<String>,
    bybit_linear: Vec<String>,
}

/// The `[profile.<name>.hyperevm]` block (R-9).
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
struct HyperevmSection {
    enabled: bool,
    rpc_ws: Option<String>,
    pools: Option<String>,
}

/// Resolve a profile name and an optional `--network` override.
fn load_profile(
    name: Option<&str>,
    override_network: Option<Network>,
) -> Result<(String, Profile)> {
    let figment = Figment::new()
        .merge(Toml::file("config/record.toml"))
        .merge(Env::prefixed("HL_RECORD_").split("__"));
    let config = parse_config(figment)?;
    let name = name.unwrap_or("default").to_string();
    let mut profile = config
        .profile
        .get(&name)
        .cloned()
        .with_context(|| format!("profile `{name}` not found in config/record.toml"))?;
    if let Some(network) = override_network {
        profile.network = network_dir(network).to_string();
    }
    Ok((name, profile))
}

/// The directory name for a network (SPEC-0008 §6).
pub(crate) fn network_dir(network: Network) -> &'static str {
    match network {
        Network::Mainnet => "mainnet",
        Network::Testnet => "testnet",
    }
}

/// Parse the `network` string from a profile.
fn parse_network(name: &str) -> Result<Network> {
    match name.to_ascii_lowercase().as_str() {
        "mainnet" => Ok(Network::Mainnet),
        "testnet" => Ok(Network::Testnet),
        other => bail!("unknown network `{other}` (expected mainnet or testnet)"),
    }
}

/// Resolve the profile network string to the enum.
fn profile_network(profile: &Profile) -> Result<Network> {
    parse_network(&profile.network)
}

// ---------------------------------------------------------------------------
// Universe resolution
// ---------------------------------------------------------------------------

/// The metadata the planner needs: the asset map, volume index, and spot meta.
struct Universe {
    map: AssetMap,
    volumes: VolumeIndex,
    spot_meta: mev_hl_client::types::SpotMeta,
}

impl Universe {
    /// Fetch universe metadata over REST (`/info`). The planner itself is pure.
    async fn load(network: Network, include_hip3: bool) -> Result<Self> {
        let info = HttpInfo::new(network);
        let map = AssetMap::load(&info, include_hip3).await?;
        let spot_meta = info.spot_meta().await?;
        let mut volumes = VolumeIndex::from_perp_ctxs(&info.meta_and_asset_ctxs().await?);
        // Spot volumes: `spotMetaAndAssetCtxs.ctxs` is indexed by the spot pair
        // `index`, not by position in `spotMeta.universe` (V-1, 2026-09-28), so
        // matching `market.index == ctxs position` joins on that index. Missing
        // volumes rank last.
        if let Ok(value) = info
            .info::<Value>(json!({ "type": "spotMetaAndAssetCtxs" }))
            .await
            && let Some(ctxs) = value.get(1).and_then(Value::as_array)
        {
            for (index, ctx) in ctxs.iter().enumerate() {
                let Some(volume) = ctx
                    .get("dayNtlVlm")
                    .and_then(|value| match value {
                        Value::String(text) => Some(text.clone()),
                        Value::Number(number) => Some(number.to_string()),
                        _ => None,
                    })
                    .and_then(|text| Decimal::from_str(&text).ok())
                else {
                    continue;
                };
                if let Some(market) = map
                    .iter()
                    .find(|m| m.kind == MarketKind::Spot && m.index == index as u32)
                {
                    volumes.insert(market.coin.clone(), volume);
                }
            }
        }
        Ok(Self {
            map,
            volumes,
            spot_meta,
        })
    }
}

// ---------------------------------------------------------------------------
// `hl record plan`
// ---------------------------------------------------------------------------

/// Resolve the plan and print it (SPEC-0008 §7.4, §12.1). No recording sockets.
pub async fn plan(
    profile: Option<String>,
    network: Option<Network>,
    allow_truncate: bool,
) -> Result<()> {
    let (name, profile) = load_profile(profile.as_deref(), network)?;
    let network = profile_network(&profile)?;
    let universe = Universe::load(network, profile.hl.wants_hip3()).await?;
    let plan = build_plan(
        &profile.hl.to_planner(allow_truncate),
        &universe.map,
        &universe.volumes,
        Some(&universe.spot_meta),
    )?;
    println!("profile: {name}  network: {}", network_dir(network));
    println!("{plan}");
    println!(
        "limits: connections {}/10, subscriptions {}/1000",
        plan.totals.connections, plan.totals.subscriptions
    );
    for dropped in &plan.dropped {
        println!("dropped: {dropped}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// `hl record` run
// ---------------------------------------------------------------------------

/// Run the recorder until SIGTERM/SIGINT (SPEC-0008 §12.1).
pub async fn run(
    profile: Option<String>,
    network: Option<Network>,
    allow_truncate: bool,
) -> Result<()> {
    let (name, profile) = load_profile(profile.as_deref(), network)?;
    let network = profile_network(&profile)?;

    // Fail fast, before any network access or directory creation, if the
    // required recording mount is not a real mount or `out_dir` escapes it
    // (R-14 §1). `out_dir` is not created until after the network metadata
    // load, so it is re-validated immediately before creation (R-14 fix1 §3).
    let mount_guard = Arc::new(
        MountGuard::new(profile.require_mount.clone(), Arc::new(SystemMountProbe))
            .with_source(profile.require_mount_source.clone()),
    );
    mount_guard
        .validate_startup(&profile.out_dir)
        .context("validating the recording profile require_mount")?;

    let prometheus = mev_metrics::prometheus::install_recorder();
    metrics::counter!(names::STARTUPS).increment(1);

    if profile.hyperevm.enabled {
        warn!("hyperevm recording is R-9 and not wired; ignoring");
    }

    let universe = Universe::load(network, profile.hl.wants_hip3()).await?;
    let plan = build_plan(
        &profile.hl.to_planner(allow_truncate),
        &universe.map,
        &universe.volumes,
        Some(&universe.spot_meta),
    )?;

    // The mount was verified before the (seconds-long) network metadata load;
    // re-check and create `out_dir` as a direct child of the verified mount,
    // never `create_dir_all` along a path that may have become the root disk.
    create_out_dir(&profile.out_dir, &mount_guard)?;

    let clock: Arc<dyn EnvelopeClock> = Arc::new(SystemEnvelopeClock::new());
    let meta = SegmentOpenMeta {
        host: hostname(),
        git_sha: git_sha(),
        profile: name,
        recorder_version: env!("CARGO_PKG_VERSION").to_string(),
    };

    info!(
        network = network_dir(network),
        connections = plan.totals.connections,
        subscriptions = plan.totals.subscriptions,
        out_dir = %profile.out_dir.display(),
        "starting recorder"
    );

    let health = Health::new();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let rest_shutdown = Arc::new(Notify::new());
    let deribit_shutdown = Arc::new(Notify::new());
    let cex_shutdown = Arc::new(Notify::new());

    // Register connection health before spawning the readiness monitor.
    let mut states: Vec<(String, Arc<ConnState>)> = plan
        .connections
        .iter()
        .map(|conn| (conn.id.clone(), ConnState::new()))
        .collect();
    states.sort_by(|a, b| a.0.cmp(&b.0));
    let recorder_health = RecorderHealth::new(states.clone());

    let mut tasks: Vec<JoinHandle<()>> = Vec::new();

    // One WebSocket task per planned connection, dialed 1 per 3 s (§7.4 step 7).
    for (index, conn) in plan.connections.iter().enumerate() {
        let state = states
            .iter()
            .find(|(id, _)| id == &conn.id)
            .map(|(_, state)| state.clone())
            .context("connection state missing")?;
        let writer = SegmentWriter::spawn(segment_config(
            &profile,
            network,
            "hl-ws",
            &conn.id,
            &meta,
            clock.clone(),
            connection_priority(conn),
            mount_guard.clone(),
        ))
        .with_context(|| format!("spawning segment writer for {}", conn.id))?;
        let protocol: Box<dyn Fn() -> Box<dyn Protocol> + Send> =
            Box::new(move || Box::new(HlProtocol::new(network)));
        let delay = mev_recorder::MIN_NEW_CONN_INTERVAL * index as u32;
        tasks.push(tokio::spawn(run_ws_conn(
            conn.clone(),
            protocol,
            writer,
            clock.clone(),
            state,
            shutdown_rx.clone(),
            delay,
        )));
    }

    // REST snapshotter (SPEC-0008 §8).
    if profile.rest.enabled {
        let writer = Arc::new(
            SegmentWriter::spawn(segment_config(
                &profile,
                network,
                "hl-rest",
                "hl-rest",
                &meta,
                clock.clone(),
                1,
                mount_guard.clone(),
            ))
            .context("spawning segment writer for hl-rest")?,
        );
        let snapshotter = snapshotter_config(&profile, &plan, network, mount_guard.clone())?;
        tasks.push(tokio::spawn(run_rest(
            snapshotter,
            writer,
            clock.clone(),
            recorder_health.clone(),
            rest_shutdown.clone(),
        )));
    } else {
        warn!("rest snapshotter disabled by profile");
    }

    // Deribit public options source (SPEC-0008 §9.1, R-12). Off by default.
    if profile.deribit.enabled {
        let writer = Arc::new(
            SegmentWriter::spawn(segment_config(
                &profile,
                network,
                "deribit",
                "deribit",
                &meta,
                clock.clone(),
                1,
                mount_guard.clone(),
            ))
            .context("spawning segment writer for deribit")?,
        );
        tasks.push(tokio::spawn(run_deribit(
            deribit_config(&profile),
            writer,
            clock.clone(),
            recorder_health.clone(),
            deribit_shutdown.clone(),
        )));
    } else {
        debug!("deribit source disabled by profile");
    }

    // Binance/Bybit reference sources (SPEC-0008 §9, R-8). One connection per
    // exchange stream, each with its own `(src, src)` SegmentWriter so the
    // R-14 mount guard applies unchanged. Only spawned when the profile has a
    // non-empty symbol list for that venue.
    for (kind, symbols) in cex_venues(&profile) {
        if symbols.is_empty() {
            continue;
        }
        let src = kind.src();
        let writer = Arc::new(
            SegmentWriter::spawn(segment_config(
                &profile,
                network,
                src,
                src,
                &meta,
                clock.clone(),
                1,
                mount_guard.clone(),
            ))
            .with_context(|| format!("spawning segment writer for {src}"))?,
        );
        let sink = Arc::new(CountingSink {
            writer: writer.clone(),
            clock: clock.clone(),
            health: RecorderHealth::new(Vec::new()),
            metrics: ConnMetrics::new(src, src),
            state: ConnState::new(),
            account_hl_rest: false,
        });
        let source = CexSource::new(CexConfig::new(kind, symbols.clone()), sink, clock.clone());
        info!(src, symbols = symbols.len(), "starting cex source");
        tasks.push(tokio::spawn(run_cex(source, cex_shutdown.clone())));
    }

    tasks.push(tokio::spawn(disk_monitor(
        profile.out_dir.clone(),
        shutdown_rx.clone(),
    )));
    tasks.push(tokio::spawn(readiness_monitor(
        recorder_health,
        health.clone(),
        clock.clone(),
        mount_guard.clone(),
        shutdown_rx.clone(),
    )));

    // Serve HTTP until a shutdown signal; `serve` also handles SIGTERM/SIGINT.
    let mut serve_task = tokio::spawn(crate::serve(health, prometheus, profile.http_port));

    // Also stop when the required mount disappears, not only on a signal
    // (R-14 §2). The wait is pending forever when no mount is required.
    let mut serve_result = None;
    let mount_tripped = tokio::select! {
        result = &mut serve_task => {
            serve_result = Some(result);
            false
        }
        _ = wait_for_mount_stop(mount_guard.clone()) => true,
    };
    if let Some(Ok(Err(err))) = serve_result {
        warn!(error = %err, "http server stopped with an error");
    }

    info!("shutdown requested; finalizing recorder");
    let _ = shutdown_tx.send(true);
    rest_shutdown.notify_one();
    deribit_shutdown.notify_one();
    cex_shutdown.notify_one();
    for task in tasks {
        let _ = task.await;
    }

    // Re-read the guard AFTER every task has stopped: a writer can trip inside
    // its final `finish()` while a SIGTERM was racing, and that must still
    // produce a non-zero exit (R-14 fix1 §2).
    if let Err(err) = recorder_exit(mount_tripped, &mount_guard) {
        serve_task.abort();
        return Err(err);
    }
    info!("recorder stopped");
    Ok(())
}

/// Final exit decision once every recorder task has stopped.
///
/// Non-zero when the mount guard tripped at any point, whether the async
/// watchdog caught it or a writer tripped during its shutdown finalize.
fn recorder_exit(mount_tripped: bool, guard: &MountGuard) -> Result<()> {
    if mount_tripped || guard.is_tripped() {
        let mount = guard
            .require_mount()
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        bail!("require_mount `{mount}` is no longer a mount; recorder stopped");
    }
    Ok(())
}

/// Create `out_dir`, taking exactly one guarded step (R-14 fix1 §3).
///
/// Unguarded: behaves as before (`create_dir_all`). Guarded: re-checks the
/// mount immediately before creating and then makes `out_dir` as a direct child
/// of the verified mount with `create_dir` — never `create_dir_all`, so a path
/// that has become the root disk cannot have parents created along it. An
/// existing `out_dir` is fine.
fn create_out_dir(out_dir: &Path, guard: &MountGuard) -> Result<()> {
    if !guard.is_guarded() {
        return std::fs::create_dir_all(out_dir)
            .with_context(|| format!("creating {}", out_dir.display()));
    }
    guard
        .check_or_trip()
        .context("re-checking require_mount before creating out_dir")?;
    let mount = guard
        .require_mount()
        .context("require_mount missing after a successful check")?;
    if out_dir.parent() != Some(mount) {
        bail!(
            "out_dir `{}` must be a direct child of require_mount `{}`",
            out_dir.display(),
            mount.display()
        );
    }
    match std::fs::create_dir(out_dir) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(err) => Err(err).with_context(|| format!("creating {}", out_dir.display())),
    }
}

/// Wait until the mount guard trips, or forever when none is required.
///
/// The probe runs on a blocking thread under [`PROBE_TIMEOUT`]; a probe that
/// fails or overruns is treated as a lost mount and trips the guard, so a hung
/// filesystem cannot wedge this watchdog. The writer thread keeps a direct
/// `stat` for its per-creation check (no per-check thread spawn there); only
/// this async path is bounded.
async fn wait_for_mount_stop(guard: Arc<MountGuard>) {
    if !guard.is_guarded() {
        std::future::pending::<()>().await;
        return;
    }
    let mut tick = tokio::time::interval(MOUNT_RECHECK_INTERVAL);
    tick.tick().await; // consume the immediate first tick
    loop {
        tick.tick().await;
        if guard.is_tripped() {
            return;
        }
        if !probe_mount(guard.clone(), PROBE_TIMEOUT).await {
            guard.trip();
            return;
        }
    }
}

/// Run one mount probe on a blocking thread with a bounded wait.
///
/// Returns whether the mount is still healthy. A failed check or a timeout
/// trips the guard. `timeout` is a parameter so tests can use a small bound.
async fn probe_mount(guard: Arc<MountGuard>, timeout: Duration) -> bool {
    let probe_guard = guard.clone();
    let probe = tokio::task::spawn_blocking(move || probe_guard.check_or_trip().is_ok());
    match tokio::time::timeout(timeout, probe).await {
        Ok(Ok(healthy)) => healthy,
        Ok(Err(_join)) => {
            guard.trip();
            false
        }
        Err(_elapsed) => {
            warn!(
                mount = ?guard.require_mount(),
                "mount probe timed out; treating the mount as lost"
            );
            guard.trip();
            false
        }
    }
}

/// Priority for a connection's disk guard: the most important (lowest) stream on
/// it, so `bbo` connections outlive `l2Book` ones.
fn connection_priority(conn: &Connection) -> u8 {
    conn.subs
        .iter()
        .map(|sub| sub.stream.priority())
        .min()
        .unwrap_or(3)
}

/// Build the [`SegmentConfig`] for one `(src, conn)` writer.
#[allow(clippy::too_many_arguments)]
fn segment_config(
    profile: &Profile,
    network: Network,
    src: &str,
    conn: &str,
    meta: &SegmentOpenMeta,
    clock: Arc<dyn EnvelopeClock>,
    priority: u8,
    mount_guard: Arc<MountGuard>,
) -> SegmentConfig {
    SegmentConfig {
        out_dir: profile.out_dir.clone(),
        network: network_dir(network).to_string(),
        src: src.to_string(),
        conn: conn.to_string(),
        zstd_level: profile.zstd_level,
        channel_capacity: 65_536,
        max_raw_bytes: 1 << 30,
        flush_interval: Duration::from_secs(5),
        min_free_gb: profile.min_free_gb,
        segment_open_meta: meta.clone(),
        clock,
        disk: Arc::new(SystemDiskSpace),
        priority,
        mount_guard,
        shutdown_join_timeout: Duration::from_secs(10),
    }
}

/// Build the [`SnapshotterConfig`] from the profile and the resolved plan.
fn snapshotter_config(
    profile: &Profile,
    plan: &mev_recorder::Plan,
    network: Network,
    mount_guard: Arc<MountGuard>,
) -> Result<SnapshotterConfig> {
    let now_ms = now_epoch_ms();
    let mut funding_coins = BTreeSet::new();
    let mut candle_coins = BTreeSet::new();
    for conn in &plan.connections {
        for sub in &conn.subs {
            let Some(coin) = &sub.coin else { continue };
            match sub.stream {
                Stream::ActiveAssetCtx => {
                    funding_coins.insert(coin.clone());
                }
                Stream::Bbo => {
                    candle_coins.insert(coin.clone());
                }
                _ => {}
            }
        }
    }
    Ok(SnapshotterConfig {
        base_url: network.rest_url().to_string(),
        src: "hl-rest".to_string(),
        conn: "hl-rest".to_string(),
        out_dir: profile.out_dir.clone(),
        weight_per_min: profile.rest.weight_per_min.max(1),
        meta_refresh: Duration::from_secs(profile.meta_refresh_secs.max(1)),
        ctx_interval: Duration::from_secs(60),
        predicted_fundings_interval: Duration::from_secs(300),
        funding_coins: funding_coins.into_iter().collect(),
        candle_coins: candle_coins.into_iter().collect(),
        candle_intervals: vec!["1m".into(), "5m".into(), "1h".into()],
        daily_interval: Duration::from_secs(24 * 60 * 60),
        funding_backfill_start_ms: now_ms.saturating_sub(FUNDING_BACKFILL_DAYS * 86_400_000),
        candle_backfill_start_ms: now_ms.saturating_sub(CANDLE_BACKFILL_DAYS * 86_400_000),
        // Only guard the state file when the profile actually requires a mount;
        // an unguarded profile keeps creating `out_dir` as before.
        mount_guard: mount_guard.is_guarded().then_some(mount_guard),
    })
}

/// Build the [`DeribitConfig`] from the profile (SPEC-0008 §9.1).
fn deribit_config(profile: &Profile) -> DeribitConfig {
    DeribitConfig {
        base_url: profile.deribit.base_url.clone(),
        currencies: profile.deribit.currencies.clone(),
        ..DeribitConfig::default()
    }
}

/// The configured CEX venues and their symbol lists (SPEC-0008 §9, R-8).
fn cex_venues(profile: &Profile) -> [(CexKind, &Vec<String>); 3] {
    [
        (CexKind::BinanceUsdm, &profile.cex.binance_usdm),
        (CexKind::BinanceSpot, &profile.cex.binance_spot),
        (CexKind::BybitLinear, &profile.cex.bybit_linear),
    ]
}

/// Wall-clock milliseconds since the epoch.
fn now_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Host name for `segment_open.meta.host`.
fn hostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("HOST"))
        .unwrap_or_else(|_| "unknown".to_string())
}

/// Git commit for `segment_open.meta.git_sha` (set at build time if available).
fn git_sha() -> String {
    option_env!("HL_GIT_SHA").unwrap_or("unknown").to_string()
}

// ---------------------------------------------------------------------------
// WebSocket connection task
// ---------------------------------------------------------------------------

/// Liveness state for one planned connection, read by the readiness monitor.
#[derive(Debug, Default)]
struct ConnState {
    connected: AtomicBool,
    last_ns: AtomicU64,
}

impl ConnState {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn set_connected(&self, connected: bool) {
        self.connected.store(connected, Ordering::Relaxed);
    }

    fn touch(&self, clock: &dyn EnvelopeClock) {
        self.last_ns.store(clock.mono_ns(), Ordering::Relaxed);
    }
}

/// Process-wide readiness input.
struct RecorderHealth {
    conns: Vec<(String, Arc<ConnState>)>,
    rest_last_ns: AtomicU64,
}

impl RecorderHealth {
    fn new(conns: Vec<(String, Arc<ConnState>)>) -> Arc<Self> {
        Arc::new(Self {
            conns,
            rest_last_ns: AtomicU64::new(0),
        })
    }

    fn touch_rest(&self, mono_ns: u64) {
        self.rest_last_ns.store(mono_ns, Ordering::Relaxed);
    }

    /// Every planned connection is connected and fed within the watchdog, and
    /// the REST stream has produced data recently.
    fn ready(&self, now_ns: u64, watchdog_ns: u64, rest_stale_ns: u64) -> bool {
        let rest_last = self.rest_last_ns.load(Ordering::Relaxed);
        let rest_ok = rest_last == 0 || now_ns.saturating_sub(rest_last) <= rest_stale_ns;
        if !rest_ok {
            return false;
        }
        self.conns.iter().all(|(_, state)| {
            if !state.connected.load(Ordering::Relaxed) {
                return false;
            }
            let last = state.last_ns.load(Ordering::Relaxed);
            last != 0 && now_ns.saturating_sub(last) <= watchdog_ns
        })
    }
}

/// Per-connection metric handles (fixed labels, no per-event allocation).
struct ConnMetrics {
    records: HashMap<&'static str, metrics::Counter>,
    dropped: metrics::Counter,
}

impl ConnMetrics {
    fn new(src: &str, conn: &str) -> Self {
        let mut records = HashMap::new();
        for kind in [
            Kind::Frame,
            Kind::FrameBin,
            Kind::Rest,
            Kind::Sub,
            Kind::ConnOpen,
            Kind::GapStart,
            Kind::GapEnd,
            Kind::Clock,
            Kind::SegmentOpen,
            Kind::SegmentClose,
        ] {
            records.insert(
                kind.as_str(),
                metrics::counter!(names::REC_RECORDS_TOTAL, "src" => src.to_string(), "conn" => conn.to_string(), "kind" => kind.as_str()),
            );
        }
        Self {
            records,
            dropped: metrics::counter!(names::REC_DROPPED_TOTAL, "src" => src.to_string(), "conn" => conn.to_string()),
        }
    }

    fn record(&self, kind: &'static str) {
        if let Some(counter) = self.records.get(kind) {
            counter.increment(1);
        }
    }
}

/// Send one envelope, update metrics, and mark the connection as alive.
///
/// Returns whether the envelope reached the writer queue.
fn emit(
    metrics: &ConnMetrics,
    writer: &SegmentWriter,
    state: &ConnState,
    clock: &dyn EnvelopeClock,
    env: Envelope,
) -> bool {
    let kind = env.kind.as_str();
    if writer.try_send(env) {
        metrics.record(kind);
        state.touch(clock);
        true
    } else {
        metrics.dropped.increment(1);
        false
    }
}

/// Consume one raw WebSocket connection and write its envelopes until shutdown.
#[allow(clippy::too_many_arguments)]
async fn run_ws_conn(
    conn: Connection,
    protocol: Box<dyn Fn() -> Box<dyn Protocol> + Send>,
    writer: SegmentWriter,
    clock: Arc<dyn EnvelopeClock>,
    state: Arc<ConnState>,
    mut shutdown: watch::Receiver<bool>,
    start_delay: Duration,
) {
    let src = "hl-ws";
    let conn_id = conn.id.clone();
    let url = protocol().url();
    let metrics = ConnMetrics::new(src, conn_id.as_str());
    let mut seq: u64 = 0;

    // Pace new connections (§7.4 step 7).
    if !start_delay.is_zero() {
        tokio::select! {
            _ = tokio::time::sleep(start_delay) => {}
            _ = shutdown.changed() => {
                shutdown_writer(writer);
                return;
            }
        }
    }

    let subs: Vec<String> = conn
        .subs
        .iter()
        .map(|sub| sub.to_json().to_string())
        .collect();

    // Retry the initial dial; `RawWsConn` handles reconnects after that.
    let mut gap_started: Option<Instant> = None;
    let mut gap_reason = String::new();
    let mut raw = loop {
        match RawWsConn::connect(protocol(), subs.clone()).await {
            Ok(raw) => break raw,
            Err(err) => {
                warn!(conn = %conn_id, error = %err, "websocket connect failed; retrying");
                seq = emit_gap_start(
                    &metrics,
                    &writer,
                    &state,
                    &*clock,
                    &conn_id,
                    "error",
                    &err.to_string(),
                    seq,
                );
                gap_reason = "error".to_string();
                gap_started = Some(Instant::now());
                tokio::select! {
                    _ = tokio::time::sleep(CONNECT_RETRY) => {}
                    _ = shutdown.changed() => {
                        shutdown_writer(writer);
                        return;
                    }
                }
            }
        }
    };
    state.set_connected(true);

    seq = write_conn_open(&metrics, &writer, &state, &*clock, &conn_id, &url, 1, seq);
    for sub in &conn.subs {
        let env = Envelope::sub(&*clock, src, conn_id.as_str(), seq, sub.to_json());
        emit(&metrics, &writer, &state, &*clock, env);
        seq += 1;
    }
    if let Some(started) = gap_started.take() {
        let gap_ms = started.elapsed().as_millis() as u64;
        let env = Envelope::gap_end(&*clock, src, conn_id.as_str(), seq, gap_ms);
        emit(&metrics, &writer, &state, &*clock, env);
        seq += 1;
        record_gap_seconds(src, &conn_id, &gap_reason, gap_ms);
    }

    let mut clock_tick = tokio::time::interval(Duration::from_secs(60));
    clock_tick.tick().await; // consume the immediate first tick

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                emit_gap_start(&metrics, &writer, &state, &*clock, &conn_id, "shutdown", "shutdown requested", seq);
                break;
            }
            _ = clock_tick.tick() => {
                let (offset_ns, stratum) = chrony_tracking().await;
                if let Some(offset) = offset_ns {
                    metrics::gauge!(names::REC_CLOCK_OFFSET_NS).set(offset as f64);
                }
                let env = Envelope::clock(&*clock, src, conn_id.as_str(), seq, offset_ns, stratum);
                emit(&metrics, &writer, &state, &*clock, env);
                seq += 1;
            }
            event = raw.next() => match event {
                Ok(RawEvent::Text { text, .. }) => {
                    let env = Envelope::frame(&*clock, src, conn_id.as_str(), seq, text);
                    emit(&metrics, &writer, &state, &*clock, env);
                    seq += 1;
                }
                Ok(RawEvent::Binary { bytes, .. }) => {
                    let env = Envelope::frame_bin(&*clock, src, conn_id.as_str(), seq, base64_encode(&bytes));
                    emit(&metrics, &writer, &state, &*clock, env);
                    seq += 1;
                }
                Ok(RawEvent::Opened { attempt }) => {
                    state.set_connected(true);
                    seq = write_conn_open(&metrics, &writer, &state, &*clock, &conn_id, &url, attempt, seq);
                    for sub in &conn.subs {
                        let env = Envelope::sub(&*clock, src, conn_id.as_str(), seq, sub.to_json());
                        emit(&metrics, &writer, &state, &*clock, env);
                        seq += 1;
                    }
                    if let Some(started) = gap_started.take() {
                        let gap_ms = started.elapsed().as_millis() as u64;
                        let env = Envelope::gap_end(&*clock, src, conn_id.as_str(), seq, gap_ms);
                        emit(&metrics, &writer, &state, &*clock, env);
                        seq += 1;
                        record_gap_seconds(src, conn_id.as_str(), &gap_reason, gap_ms);
                    }
                }
                Ok(RawEvent::Gap { reason, detail }) => {
                    state.set_connected(false);
                    seq = emit_gap_start(&metrics, &writer, &state, &*clock, &conn_id, &reason, &detail, seq);
                    gap_reason = reason;
                    gap_started = Some(Instant::now());
                }
                Err(err) => {
                    state.set_connected(false);
                    emit_gap_start(&metrics, &writer, &state, &*clock, &conn_id, "error", &err.to_string(), seq);
                    break;
                }
            }
        }
    }

    state.set_connected(false);
    shutdown_writer(writer);
}

/// Stop a segment writer, surfacing a bounded-join timeout.
///
/// `SegmentWriter::shutdown` returns [`SegmentError::ShutdownTimeout`] and trips
/// the mount guard when the writer thread did not stop in time, so `run` exits
/// non-zero without waiting for the hung thread (R-14 fix2 §2).
fn shutdown_writer(writer: SegmentWriter) {
    if let Err(err) = writer.shutdown() {
        warn!(error = %err, "segment writer shutdown timed out; mount guard tripped");
    }
}

/// Write a `conn_open` envelope and return the next `seq`.
#[allow(clippy::too_many_arguments)]
fn write_conn_open(
    metrics: &ConnMetrics,
    writer: &SegmentWriter,
    state: &ConnState,
    clock: &dyn EnvelopeClock,
    conn: &str,
    url: &str,
    attempt: u32,
    seq: u64,
) -> u64 {
    let env = Envelope::conn_open(clock, "hl-ws", conn, seq, url, attempt);
    emit(metrics, writer, state, clock, env);
    seq + 1
}

/// Write a `gap_start` envelope and return the next `seq`.
#[allow(clippy::too_many_arguments)]
fn emit_gap_start(
    metrics: &ConnMetrics,
    writer: &SegmentWriter,
    state: &ConnState,
    clock: &dyn EnvelopeClock,
    conn: &str,
    reason: &str,
    detail: &str,
    seq: u64,
) -> u64 {
    let env = Envelope::gap_start(clock, "hl-ws", conn, seq, reason, detail);
    emit(metrics, writer, state, clock, env);
    seq + 1
}

/// Record gap time in the §12.2 counter.
fn record_gap_seconds(src: &str, conn: &str, reason: &str, gap_ms: u64) {
    metrics::counter!(
        names::REC_GAP_SECONDS_TOTAL,
        "src" => src.to_string(),
        "conn" => conn.to_string(),
        "reason" => reason.to_string(),
    )
    .increment(gap_ms / 1_000);
}

// ---------------------------------------------------------------------------
// REST snapshotter task
// ---------------------------------------------------------------------------

/// An [`EnvelopeSink`] that counts records and marks REST liveness.
///
/// `account_hl_rest` gates the Hyperliquid-specific side effects (the §8 weight
/// counter and `/readyz` REST freshness); non-HL sources such as Deribit reuse
/// the sink with it off so their polls do not consume the HL budget or mask an
/// `hl-rest` outage.
struct CountingSink {
    writer: Arc<SegmentWriter>,
    clock: Arc<dyn EnvelopeClock>,
    health: Arc<RecorderHealth>,
    metrics: ConnMetrics,
    state: Arc<ConnState>,
    account_hl_rest: bool,
}

impl EnvelopeSink for CountingSink {
    fn send(&self, env: Envelope) -> bool {
        // Mirror the §8 weights so the 300/min budget is observable. R-5 meters
        // its own bucket; this counter is the recorder-side view of it.
        let weight = if self.account_hl_rest {
            env.meta
                .as_ref()
                .and_then(|meta| meta.get("req"))
                .map(request_weight)
                .unwrap_or(0)
        } else {
            0
        };
        let mono_ns = self.clock.mono_ns();
        let sent = emit(&self.metrics, &self.writer, &self.state, &*self.clock, env);
        if sent && self.account_hl_rest {
            if weight > 0 {
                metrics::counter!(names::REST_WEIGHT_USED_TOTAL, "src" => "recorder".to_string())
                    .increment(weight as u64);
            }
            self.health.touch_rest(mono_ns);
        }
        sent
    }
}

/// The §8 weight of a `/info` request, mirroring the snapshotter's schedule.
///
/// Weights follow the V-1 facts (2026-09-28): `candleSnapshot` is
/// `20 + 1 per 60 items returned`, and `fundingHistory` is
/// `20 + 1 per 20 items returned`, pre-charged by its 500-item page maximum
/// (`20 + 25`) because the item count is not known before the response.
fn request_weight(request: &Value) -> u32 {
    match request.get("type").and_then(Value::as_str) {
        Some("fundingHistory") => 20 + FUNDING_MAX_PAGE / 20,
        Some("candleSnapshot") => {
            let req = request.get("req");
            let interval = req
                .and_then(|req| req.get("interval"))
                .and_then(Value::as_str)
                .unwrap_or("1m");
            let start = req
                .and_then(|req| req.get("startTime"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let end = req
                .and_then(|req| req.get("endTime"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let step = match interval {
                "1m" => 60_000,
                "5m" => 300_000,
                "1h" => 3_600_000,
                _ => 60_000,
            };
            let candles = end.saturating_sub(start) / step.max(1);
            20 + (candles / 60) as u32
        }
        _ => 20,
    }
}

/// Run the REST snapshotter until `shutdown` is notified, then finalize.
async fn run_rest(
    config: SnapshotterConfig,
    writer: Arc<SegmentWriter>,
    clock: Arc<dyn EnvelopeClock>,
    health: Arc<RecorderHealth>,
    shutdown: Arc<Notify>,
) {
    let state = ConnState::new();
    let sink = Arc::new(CountingSink {
        writer: writer.clone(),
        clock: clock.clone(),
        health,
        metrics: ConnMetrics::new("hl-rest", "hl-rest"),
        state,
        account_hl_rest: true,
    });
    match RestSnapshotter::new(config, sink, clock) {
        Ok(snapshotter) => snapshotter.run(shutdown).await,
        Err(err) => warn!(error = %err, "rest snapshotter failed to start"),
    }
    // Dropping the last `Arc` finalizes the segment (SegmentWriter::drop).
    drop(writer);
}

/// Run the Deribit options source until `shutdown` is notified, then finalize.
async fn run_deribit(
    config: DeribitConfig,
    writer: Arc<SegmentWriter>,
    clock: Arc<dyn EnvelopeClock>,
    health: Arc<RecorderHealth>,
    shutdown: Arc<Notify>,
) {
    let state = ConnState::new();
    let sink = Arc::new(CountingSink {
        writer: writer.clone(),
        clock: clock.clone(),
        health,
        metrics: ConnMetrics::new("deribit", "deribit"),
        state,
        account_hl_rest: false,
    });
    DeribitSource::new(config, sink, clock).run(shutdown).await;
    // Dropping the last `Arc` finalizes the segment (SegmentWriter::drop).
    drop(writer);
}

/// Run one CEX reference source (SPEC-0008 §9, R-8) until shutdown, then
/// finalize its segment.
async fn run_cex(source: CexSource, shutdown: Arc<Notify>) {
    source.run(shutdown).await;
}

// ---------------------------------------------------------------------------
// Background monitors
// ---------------------------------------------------------------------------

/// Sample the free-disk gauge until shutdown.
async fn disk_monitor(out_dir: PathBuf, mut shutdown: watch::Receiver<bool>) {
    let disk = SystemDiskSpace;
    let mut tick = tokio::time::interval(DISK_SAMPLE_INTERVAL);
    loop {
        tokio::select! {
            _ = tick.tick() => match disk.free_bytes(&out_dir) {
                Ok(free) => metrics::gauge!(names::REC_DISK_FREE_BYTES).set(free as f64),
                Err(err) => debug!(error = %err, "disk free check failed"),
            },
            _ = shutdown.changed() => break,
        }
    }
}

/// Update `/readyz` from the connection/REST liveness state until shutdown.
///
/// A tripped [`MountGuard`] forces not-ready even while the sockets are still
/// healthy, so `/readyz` reflects that the recorder can no longer write.
async fn readiness_monitor(
    recorder: Arc<RecorderHealth>,
    health: Health,
    clock: Arc<dyn EnvelopeClock>,
    mount_guard: Arc<MountGuard>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut tick = tokio::time::interval(READY_SAMPLE_INTERVAL);
    loop {
        tokio::select! {
            _ = tick.tick() => {
                let ready = !mount_guard.is_tripped() && recorder.ready(
                    clock.mono_ns(),
                    READY_WATCHDOG.as_nanos() as u64,
                    REST_READY_STALE.as_nanos() as u64,
                );
                health.set_ready(ready);
            }
            _ = shutdown.changed() => break,
        }
    }
    health.set_ready(false);
}

// ---------------------------------------------------------------------------
// `hl record inspect` / `hl record verify`
// ---------------------------------------------------------------------------

/// Print the `inspect` report for one or more segment files or directories.
pub fn inspect(paths: &[PathBuf]) -> Result<()> {
    if paths.is_empty() {
        bail!("no paths given");
    }
    let mut files: Vec<PathBuf> = Vec::new();
    for path in paths {
        collect_segments(path, &mut files)?;
    }
    if files.is_empty() {
        bail!("no segment files found");
    }
    files.sort();
    let report = reader::inspect(&files)?;
    println!("files: {}", report.files);
    println!("records: {}", report.records);
    println!("crashed files: {}", report.crashed_files);
    if let (Some(first), Some(last)) = (report.first_t_ns, report.last_t_ns) {
        println!("first_t_ns: {first}");
        println!("last_t_ns: {last}");
    }
    println!("gaps: {} ({} ms)", report.gap_count, report.gap_total_ms);
    println!("by src:");
    for (src, count) in &report.by_src {
        println!("  {src:<16} {count}");
    }
    println!("by kind:");
    for (kind, count) in &report.by_kind {
        println!("  {kind:<16} {count}");
    }
    println!("by channel:");
    for (channel, count) in &report.by_channel {
        println!("  {channel:<16} {count}");
    }
    for holes in &report.seq_holes {
        println!(
            "seq holes: {}/{} missing {} value(s)",
            holes.src,
            holes.conn,
            holes.missing.len()
        );
    }
    Ok(())
}

/// Print the `verify` report for a UTC date.
pub fn verify(profile: Option<String>, network: Option<Network>, date: &str) -> Result<()> {
    let (name, profile) = load_profile(profile.as_deref(), network)?;
    let network = profile_network(&profile)?;
    let report = reader::verify(&reader::VerifyConfig {
        out_dir: profile.out_dir.clone(),
        network: network_dir(network).to_string(),
        date: date.to_string(),
    })?;
    println!("profile: {name}  network: {}", network_dir(network));
    println!("date: {}  files: {}", report.date, report.files.len());
    for file in &report.files {
        println!(
            "  {:<70} records={}{} size={}{}",
            file.file,
            file.records_manifest,
            if file.records_ok { "" } else { " MISMATCH" },
            file.bytes_zst_manifest,
            if file.size_ok { "" } else { " MISMATCH" }
        );
    }
    println!("coverage:");
    for stream in &report.coverage {
        println!(
            "  {}/{}: {:.2}% ({} ms)",
            stream.src, stream.conn, stream.coverage_pct, stream.covered_ms
        );
    }
    Ok(())
}

/// Collect segment files under `path` (a file is used as-is; a directory is
/// walked recursively for `*.zst` / `*.crashed`).
fn collect_segments(path: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    if path.is_dir() {
        for entry in
            std::fs::read_dir(path).with_context(|| format!("reading {}", path.display()))?
        {
            let entry = entry?;
            collect_segments(&entry.path(), out)?;
        }
    } else if is_segment_file(path) {
        out.push(path.to_path_buf());
    }
    Ok(())
}

fn is_segment_file(path: &Path) -> bool {
    let name = path.to_string_lossy();
    name.ends_with(".jsonl.zst") || name.ends_with(".jsonl.zst.crashed")
}

/// Encode bytes with the standard base64 alphabet (SPEC-0008 §5.1).
fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((triple >> 18) & 63) as usize] as char);
        out.push(TABLE[((triple >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((triple >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(triple & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

// ---------------------------------------------------------------------------
// Clock discipline (SPEC-0008 §11)
// ---------------------------------------------------------------------------

/// Parse a `chronyc -c tracking` CSV line into `(offset_ns, stratum)`.
///
/// The verified column order (V-1, 2026-09-28) is `RefID, RefName, Stratum,
/// RefTime, SystemTime, LastOffset, …`; `SystemTime` (the current offset in
/// seconds, `0.000012345` ≈ 12.3 µs) is column **4**. A line with fewer than
/// five columns is malformed and returns `None` rather than panicking.
fn parse_chrony_tracking(line: &str) -> Option<(Option<i64>, Option<u8>)> {
    let columns: Vec<&str> = line.split(',').collect();
    // `SystemTime` (index 4) is the offset; index 3 is the RefTime epoch.
    let system_time = columns.get(4)?;
    let offset_ns = system_time
        .trim()
        .parse::<f64>()
        .ok()
        .map(|seconds| (seconds * 1_000_000_000.0) as i64);
    let stratum = columns
        .get(2)
        .and_then(|value| value.trim().parse::<u8>().ok());
    Some((offset_ns, stratum))
}

/// Read the chrony offset/stratum via `chronyc -c tracking`, if available.
async fn chrony_tracking() -> (Option<i64>, Option<u8>) {
    let parsed = tokio::task::spawn_blocking(|| {
        let output = std::process::Command::new("chronyc")
            .args(["-c", "tracking"])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8(output.stdout).ok()?;
        let line = text.lines().next()?;
        parse_chrony_tracking(line)
    })
    .await
    .ok()
    .flatten();
    parsed.unwrap_or((None, None))
}

// ---------------------------------------------------------------------------
// `hl probe latency`
// ---------------------------------------------------------------------------

/// Measure TCP connect, TLS handshake, WS ping→pong, and `/info` `allMids` RTT
/// (SPEC-0008 §12.1, used by V-4).
pub async fn probe_latency(network: Option<Network>, count: u32) -> Result<()> {
    let network = network.unwrap_or(Network::Mainnet);
    let count = count.max(1);
    let (host, port) = authority(network.rest_url())?;
    let ws_url = network.ws_url().to_string();
    let http = HttpInfo::new(network);
    let tls = tls_measurement_config();

    let mut tcp_us = Vec::new();
    let mut tls_us = Vec::new();
    let mut ws_us = Vec::new();
    let mut info_us = Vec::new();

    for iteration in 0..count {
        match measure_tcp_tls(&host, port, tls.clone()).await {
            Ok((tcp, handshake)) => {
                tcp_us.push(tcp as f64);
                tls_us.push(handshake as f64);
            }
            Err(err) => warn!(iteration, error = %err, "tcp/tls probe failed"),
        }
        match measure_ws_ping(&ws_url).await {
            Ok(rtt) => ws_us.push(rtt as f64),
            Err(err) => warn!(iteration, error = %err, "ws ping probe failed"),
        }
        let started = Instant::now();
        match http.all_mids().await {
            Ok(_) => info_us.push(started.elapsed().as_micros() as f64),
            Err(err) => warn!(iteration, error = %err, "info probe failed"),
        }
    }

    if tcp_us.is_empty() && ws_us.is_empty() && info_us.is_empty() {
        bail!("all latency probes failed; is the network reachable?");
    }

    println!("target: {host}:{port} (websocket {ws_url})");
    println!("samples: {} of {count}", info_us.len().max(tcp_us.len()));
    print_latency("tcp connect", &tcp_us);
    print_latency("tls handshake", &tls_us);
    print_latency("ws ping->pong", &ws_us);
    print_latency("info allMids", &info_us);
    Ok(())
}

/// Print p50/p90/max in milliseconds for a microsecond sample set.
fn print_latency(label: &str, samples_us: &[f64]) {
    if samples_us.is_empty() {
        println!("  {label:<16} (no samples)");
        return;
    }
    let mut sorted = samples_us.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    println!(
        "  {label:<16} p50={:.3} ms  p90={:.3} ms  max={:.3} ms  (n={})",
        percentile(&sorted, 0.5) / 1_000.0,
        percentile(&sorted, 0.9) / 1_000.0,
        sorted[sorted.len() - 1] / 1_000.0,
        sorted.len()
    );
}

/// Nearest-rank percentile over a sorted slice.
fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[index.min(sorted.len() - 1)]
}

/// Split a URL into `(host, port)` with default ports for http(s)/ws(s).
fn authority(url: &str) -> Result<(String, u16)> {
    let (scheme, rest) = url
        .split_once("://")
        .with_context(|| format!("malformed url `{url}`"))?;
    let host_port = rest.split('/').next().unwrap_or(rest);
    let default_port = if scheme.ends_with('s') { 443 } else { 80 };
    match host_port.rsplit_once(':') {
        Some((host, port)) => Ok((host.to_string(), port.parse()?)),
        None => Ok((host_port.to_string(), default_port)),
    }
}

/// Connect and complete a rustls handshake, returning `(tcp_us, tls_us)`.
async fn measure_tcp_tls(
    host: &str,
    port: u16,
    config: Arc<rustls::ClientConfig>,
) -> Result<(u64, u64)> {
    let host = host.to_string();
    tokio::task::spawn_blocking(move || {
        let started = Instant::now();
        let socket = std::net::TcpStream::connect((host.as_str(), port))?;
        let tcp_us = started.elapsed().as_micros() as u64;
        let server_name = rustls::pki_types::ServerName::try_from(host.clone())
            .map_err(|err| anyhow::anyhow!("invalid server name: {err}"))?;
        let connection = rustls::ClientConnection::new(config, server_name)
            .map_err(|err| anyhow::anyhow!("tls client: {err}"))?;
        let mut stream = rustls::StreamOwned::new(connection, socket);
        let started = Instant::now();
        while stream.conn.is_handshaking() {
            stream.conn.complete_io(&mut stream.sock)?;
        }
        Ok::<_, anyhow::Error>((tcp_us, started.elapsed().as_micros() as u64))
    })
    .await?
}

/// Open a WS connection, send one app ping, and time the pong.
async fn measure_ws_ping(ws_url: &str) -> Result<u64> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let (mut socket, _) = tokio_tungstenite::connect_async(ws_url).await?;
    let started = Instant::now();
    socket
        .send(Message::Text(r#"{"method":"ping"}"#.to_string().into()))
        .await?;
    loop {
        match socket.next().await {
            Some(Ok(Message::Text(text))) => {
                let is_pong = serde_json::from_str::<Value>(&text)
                    .ok()
                    .and_then(|value| {
                        value
                            .get("channel")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    })
                    .as_deref()
                    == Some("pong");
                if is_pong {
                    return Ok(started.elapsed().as_micros() as u64);
                }
            }
            Some(Ok(_)) => {}
            Some(Err(err)) => return Err(err.into()),
            None => bail!("websocket closed before pong"),
        }
    }
}

/// A rustls config that skips certificate verification for the latency probe.
///
/// The probe transmits no application data over the TLS connection; it only
/// times the handshake. Using the system root store would add a dependency for
/// no benefit here (SPEC-0008 §12.1).
fn tls_measurement_config() -> Arc<rustls::ClientConfig> {
    let provider = rustls::crypto::ring::default_provider();
    let _ = provider.clone().install_default();
    let config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyServerCert(Arc::new(provider))))
        .with_no_client_auth();
    Arc::new(config)
}

/// Certificate verifier that accepts everything (probe only).
struct AcceptAnyServerCert(Arc<rustls::crypto::CryptoProvider>);

impl std::fmt::Debug for AcceptAnyServerCert {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcceptAnyServerCert")
            .finish_non_exhaustive()
    }
}

impl rustls::client::danger::ServerCertVerifier for AcceptAnyServerCert {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _certificate: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _certificate: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use mev_recorder::{MountProbe, Subscription};
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    fn temp_dir(tag: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("mev-bot-record-{tag}-"))
            .tempdir()
            .unwrap()
    }

    fn segment_files(root: &Path) -> Vec<PathBuf> {
        let mut files = Vec::new();
        collect_segments(root, &mut files).unwrap();
        files.sort();
        files
    }

    /// A protocol that dials a fixed local mock URL.
    struct TestProtocol {
        url: String,
    }

    impl Protocol for TestProtocol {
        fn name(&self) -> &'static str {
            "test"
        }
        fn url(&self) -> String {
            self.url.clone()
        }
        fn subscribe_frame(&self, sub: &str) -> String {
            sub.to_string()
        }
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(&[0, 1, 2, 3]), "AAECAw==");
    }

    #[test]
    fn authority_handles_default_ports() {
        assert_eq!(
            authority("https://api.hyperliquid.xyz").unwrap(),
            ("api.hyperliquid.xyz".to_string(), 443)
        );
        assert_eq!(
            authority("wss://api.hyperliquid.xyz/ws").unwrap(),
            ("api.hyperliquid.xyz".to_string(), 443)
        );
        assert_eq!(
            authority("http://127.0.0.1:8080/info").unwrap(),
            ("127.0.0.1".to_string(), 8080)
        );
    }

    #[test]
    fn chrony_tracking_reads_system_time_column() {
        // Realistic `chronyc -c tracking` order (V-1): RefID, RefName, Stratum,
        // RefTime, SystemTime, LastOffset, …. SystemTime is column 4.
        let line = "C0000000,time.cloudflare.com,3,1790000000.123,0.000012345,\
                    0.000004,0.000006,12.345,0.001,-0.000123,0.012345,0.006789,8.0,Normal";
        let (offset_ns, stratum) = parse_chrony_tracking(line).expect("well-formed line");
        assert_eq!(offset_ns, Some(12_345));
        assert_eq!(stratum, Some(3));
    }

    #[test]
    fn chrony_tracking_rejects_short_lines() {
        assert_eq!(
            parse_chrony_tracking("C0000000,time.cloudflare.com,3"),
            None
        );
        assert_eq!(parse_chrony_tracking(""), None);
    }

    #[test]
    fn request_weight_matches_v1_weights() {
        // fundingHistory: `20 + 1 per 20 items`, pre-charged at the 500-item max.
        assert_eq!(
            request_weight(&serde_json::json!({
                "type": "fundingHistory",
                "coin": "BTC",
                "startTime": 0,
            })),
            20 + 25
        );
        // candleSnapshot: `20 + 1 per 60 items` over the requested window; 6 h of
        // 1m is 360 candles -> 26.
        assert_eq!(
            request_weight(&serde_json::json!({
                "type": "candleSnapshot",
                "req": {
                    "coin": "BTC",
                    "interval": "1m",
                    "startTime": 0,
                    "endTime": 21_600_000,
                },
            })),
            26
        );
        // 6 h of 1h is 6 candles -> base weight only.
        assert_eq!(
            request_weight(&serde_json::json!({
                "type": "candleSnapshot",
                "req": { "interval": "1h", "startTime": 0, "endTime": 21_600_000 },
            })),
            20
        );
        assert_eq!(request_weight(&serde_json::json!({ "type": "meta" })), 20);
    }

    /// Parse a recorder config from a TOML string (no file or env), validating.
    fn parse_toml(text: &str) -> Result<RecordConfig> {
        parse_config(Figment::new().merge(Toml::string(text)))
    }

    #[test]
    fn require_mount_defaults_to_unset() {
        let config = parse_toml("[profile.default]\nnetwork = \"mainnet\"\n").unwrap();
        assert_eq!(config.profile.get("default").unwrap().require_mount, None);
    }

    #[test]
    fn require_mount_parses_an_absolute_path() {
        let config = parse_toml(
            "[profile.default]\n\
             out_dir = \"/mnt/e/mev-rec\"\n\
             require_mount = \"/mnt/e\"\n",
        )
        .unwrap();
        assert_eq!(
            config
                .profile
                .get("default")
                .unwrap()
                .require_mount
                .as_deref(),
            Some(Path::new("/mnt/e"))
        );
    }

    #[test]
    fn require_mount_source_defaults_to_unset() {
        let config = parse_toml("[profile.default]\nnetwork = \"mainnet\"\n").unwrap();
        assert_eq!(
            config.profile.get("default").unwrap().require_mount_source,
            None
        );
    }

    #[test]
    fn require_mount_source_parses_a_literal_windows_source() {
        let config = parse_toml(
            "[profile.default]\n\
             require_mount = \"/mnt/e\"\n\
             require_mount_source = 'E:\\'\n",
        )
        .unwrap();
        assert_eq!(
            config
                .profile
                .get("default")
                .unwrap()
                .require_mount_source
                .as_deref(),
            Some("E:\\")
        );
    }

    /// A tripped guard forces `/readyz` not-ready even while the connections
    /// look healthy (R-14 §2).
    #[tokio::test]
    async fn readiness_monitor_is_not_ready_when_the_mount_is_tripped() {
        let clock: Arc<dyn EnvelopeClock> = Arc::new(mev_recorder::FixedEnvelopeClock::new(1, 0));
        let health = Health::new();
        health.set_ready(true);
        let guard = Arc::new(MountGuard::unguarded());
        guard.trip();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(readiness_monitor(
            RecorderHealth::new(Vec::new()),
            health.clone(),
            clock,
            guard,
            shutdown_rx,
        ));
        for _ in 0..1_000 {
            tokio::task::yield_now().await;
            if !health.is_ready() {
                break;
            }
        }
        let _ = shutdown_tx.send(true);
        let _ = task.await;
        assert!(
            !health.is_ready(),
            "a tripped mount must make /readyz not ready"
        );
    }

    /// The mount watchdog fires once the required mount is gone, which is what
    /// makes `run` return an error (process exit non-zero).
    #[tokio::test(start_paused = true)]
    async fn mount_watchdog_fires_when_the_required_mount_is_not_a_mount() {
        let tmp = temp_dir("watchdog-run");
        let guard = Arc::new(MountGuard::new(
            Some(tmp.path().to_path_buf()),
            Arc::new(SystemMountProbe),
        ));
        tokio::time::timeout(Duration::from_secs(10), wait_for_mount_stop(guard.clone()))
            .await
            .expect("watchdog did not fire");
        assert!(guard.is_tripped());
    }

    /// With no `require_mount`, the watchdog never fires (current behaviour).
    #[tokio::test(start_paused = true)]
    async fn mount_watchdog_stays_pending_when_unguarded() {
        let guard = Arc::new(MountGuard::unguarded());
        let result =
            tokio::time::timeout(Duration::from_secs(60), wait_for_mount_stop(guard.clone())).await;
        assert!(result.is_err(), "unguarded watchdog must never complete");
        assert!(!guard.is_tripped());
    }

    #[test]
    fn segment_config_carries_the_mount_guard() {
        let guard = Arc::new(MountGuard::unguarded());
        let clock: Arc<dyn EnvelopeClock> = Arc::new(SystemEnvelopeClock::new());
        let config = segment_config(
            &Profile::default(),
            Network::Mainnet,
            "hl-ws",
            "hl-ws-01",
            &SegmentOpenMeta::default(),
            clock,
            1,
            guard.clone(),
        );
        assert!(Arc::ptr_eq(&config.mount_guard, &guard));
    }

    // -- R-14 fix1: startup, exit and probe hardening ------------------------

    /// A probe whose mount device can be flipped, mirroring the recorder crate's
    /// test fake (which is not visible from this crate).
    struct FlipProbe {
        mount: PathBuf,
        mount_dev: u64,
        host_dev: u64,
        mounted: AtomicBool,
    }

    impl FlipProbe {
        fn new(mount: &Path) -> Self {
            Self {
                mount: std::fs::canonicalize(mount).unwrap_or_else(|_| mount.to_path_buf()),
                mount_dev: 7,
                host_dev: 3,
                mounted: AtomicBool::new(true),
            }
        }

        fn set_mounted(&self, mounted: bool) {
            self.mounted.store(mounted, Ordering::SeqCst);
        }
    }

    impl MountProbe for FlipProbe {
        fn stat(&self, path: &Path) -> std::io::Result<(u64, bool)> {
            let meta = std::fs::metadata(path)?;
            if !self.mounted.load(Ordering::SeqCst) {
                return Ok((self.host_dev, meta.is_dir()));
            }
            let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
            let device = if canonical.starts_with(&self.mount) {
                self.mount_dev
            } else {
                self.host_dev
            };
            Ok((device, meta.is_dir()))
        }
    }

    /// A probe that reports `mount` as a healthy mount but blocks on every
    /// call, to exercise the bounded-wait path in isolation. If the timeout
    /// were removed the probe would eventually report the mount as healthy.
    struct SlowProbe {
        mount: PathBuf,
        delay: Duration,
    }

    impl MountProbe for SlowProbe {
        fn stat(&self, path: &Path) -> std::io::Result<(u64, bool)> {
            std::thread::sleep(self.delay);
            let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
            let device = if canonical.starts_with(&self.mount) {
                7
            } else {
                3
            };
            Ok((device, true))
        }
    }

    #[test]
    fn guarded_create_out_dir_rechecks_and_creates_a_direct_child() {
        let mount_tmp = temp_dir("create-ok");
        let mount = mount_tmp.path().to_path_buf();
        let out = mount.join("mev-rec");
        let probe = Arc::new(FlipProbe::new(&mount));
        let guard = MountGuard::new(Some(mount.clone()), probe);
        guard.validate_startup(&out).unwrap();

        create_out_dir(&out, &guard).unwrap();
        assert!(out.is_dir(), "out_dir was not created");
        // An existing directory is fine.
        create_out_dir(&out, &guard).unwrap();
        assert!(!guard.is_tripped());
    }

    #[test]
    fn guarded_create_out_dir_refuses_when_the_mount_flips_after_validation() {
        let mount_tmp = temp_dir("create-flip");
        let mount = mount_tmp.path().to_path_buf();
        let out = mount.join("mev-rec");
        let probe = Arc::new(FlipProbe::new(&mount));
        let guard = MountGuard::new(Some(mount.clone()), probe.clone());
        guard.validate_startup(&out).unwrap();

        // The mount disappears between the startup validation and the create.
        probe.set_mounted(false);
        let err = create_out_dir(&out, &guard).unwrap_err();
        assert!(
            format!("{err:#}").contains("require_mount"),
            "unexpected error: {err:#}"
        );
        assert!(!out.exists(), "out_dir must not be created after a flip");
        assert!(guard.is_tripped());
    }

    #[test]
    fn guarded_create_out_dir_never_creates_parents() {
        let mount_tmp = temp_dir("create-deep");
        let mount = mount_tmp.path().to_path_buf();
        let out = mount.join("a/b");
        let probe = Arc::new(FlipProbe::new(&mount));
        let guard = MountGuard::new(Some(mount.clone()), probe);
        guard.validate_startup(&out).unwrap();

        let err = create_out_dir(&out, &guard).unwrap_err();
        assert!(
            format!("{err:#}").contains("direct child"),
            "unexpected error: {err:#}"
        );
        assert!(!mount.join("a").exists(), "parents must not be created");
    }

    #[test]
    fn create_out_dir_is_unchanged_when_unguarded() {
        let tmp = temp_dir("create-unguarded");
        let out = tmp.path().join("nested/rec");
        create_out_dir(&out, &MountGuard::unguarded()).unwrap();
        assert!(out.is_dir());
    }

    /// Exit is non-zero whenever the guard tripped, including a write that
    /// tripped it inside a writer's shutdown finalize (R-14 fix1 §2).
    #[test]
    fn exit_is_non_zero_when_the_guard_tripped_during_shutdown() {
        let guard = MountGuard::unguarded();
        assert!(recorder_exit(false, &guard).is_ok());
        assert!(recorder_exit(true, &guard).is_err(), "explicit trip");

        // A writer trips after the SIGTERM decision was taken: the post-join
        // re-read must still fail the run.
        guard.trip();
        let err = recorder_exit(false, &guard).unwrap_err();
        assert!(
            err.to_string().contains("no longer a mount"),
            "unexpected error: {err}"
        );
    }

    /// A probe that outruns its bound is a failed check and trips the guard
    /// (R-14 fix1 §4).
    #[tokio::test]
    async fn probe_timeout_trips_the_guard() {
        let tmp = temp_dir("probe-timeout");
        let mount = std::fs::canonicalize(tmp.path()).unwrap();
        let guard = Arc::new(MountGuard::new(
            Some(mount.clone()),
            Arc::new(SlowProbe {
                mount,
                delay: Duration::from_millis(150),
            }),
        ));
        // The probe itself would report the mount healthy; only the bound makes
        // this a failure.
        assert!(!probe_mount(guard.clone(), Duration::from_millis(20)).await);
        assert!(guard.is_tripped());
    }

    #[test]
    fn deribit_defaults_to_disabled() {
        let config = parse_toml("[profile.default]\nnetwork = \"mainnet\"\n").unwrap();
        let deribit = &config.profile.get("default").unwrap().deribit;
        assert!(!deribit.enabled, "deribit is on unless explicitly enabled");
        assert_eq!(
            deribit.currencies,
            vec!["BTC".to_string(), "ETH".to_string()]
        );
        assert_eq!(deribit.base_url, DEFAULT_BASE_URL);
    }

    #[test]
    fn deribit_enabled_parses_currencies_and_base_url() {
        let config = parse_toml(
            "[profile.default.deribit]\n\
             enabled = true\n\
             currencies = [\"SOL\"]\n\
             base_url = \"https://example.test/api/v2\"\n",
        )
        .unwrap();
        let deribit = &config.profile.get("default").unwrap().deribit;
        assert!(deribit.enabled);
        assert_eq!(deribit.currencies, vec!["SOL".to_string()]);
        assert_eq!(deribit.base_url, "https://example.test/api/v2");
    }

    #[test]
    fn deribit_enabled_with_empty_currencies_is_rejected() {
        let err = parse_toml(
            "[profile.default.deribit]\n\
             enabled = true\n\
             currencies = []\n",
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("currencies"),
            "unexpected error: {err:#}"
        );
    }

    /// End-to-end wiring: an enabled deribit profile polls a mock server and
    /// leaves a finalized `deribit` segment holding `rest` envelopes.
    #[tokio::test]
    async fn deribit_enabled_writes_rest_segment() {
        let tmp = temp_dir("deribit");
        let dir = tmp.path();

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{\"result\":[]}"))
            .mount(&server)
            .await;

        let profile = Profile {
            out_dir: dir.to_path_buf(),
            deribit: DeribitSection {
                enabled: true,
                currencies: vec!["BTC".to_string()],
                base_url: server.uri(),
            },
            ..Profile::default()
        };
        profile.validate().unwrap();

        let clock: Arc<dyn EnvelopeClock> = Arc::new(SystemEnvelopeClock::new());
        let writer = Arc::new(
            SegmentWriter::spawn(segment_config(
                &profile,
                Network::Testnet,
                "deribit",
                "deribit",
                &SegmentOpenMeta::default(),
                clock.clone(),
                1,
                Arc::new(MountGuard::unguarded()),
            ))
            .unwrap(),
        );
        let health = RecorderHealth::new(Vec::new());
        let shutdown = Arc::new(Notify::new());
        let task = tokio::spawn(run_deribit(
            deribit_config(&profile),
            writer,
            clock,
            health,
            shutdown.clone(),
        ));

        // One round is the options summary plus the index price for one currency.
        let mut served = 0;
        for _ in 0..200 {
            served = server
                .received_requests()
                .await
                .map(|requests| requests.len())
                .unwrap_or(0);
            if served >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(served >= 2, "deribit source did not poll the mock");
        // Give the source a moment to consume the responses and write the sink.
        tokio::time::sleep(Duration::from_millis(50)).await;

        shutdown.notify_one();
        task.await.unwrap();

        let files = segment_files(dir);
        assert!(!files.is_empty(), "no deribit segment was written");
        let report = reader::inspect(&files).unwrap();
        assert!(
            report.by_src.get("deribit").copied().unwrap_or(0) > 0,
            "segment has no deribit records: {:#?}",
            report.by_src
        );
        assert!(
            report.by_kind.get("rest").copied().unwrap_or(0) >= 2,
            "expected rest envelopes: {:#?}",
            report.by_kind
        );
        assert!(
            report.by_kind.get("segment_close").copied().unwrap_or(0) == 1,
            "segment did not finalize: {:#?}",
            report.by_kind
        );
    }

    /// `cex_venues` maps the three `[cex]` lists to their source ids.
    #[test]
    fn cex_venues_map_to_the_spec_srcs() {
        let profile = Profile {
            cex: CexSection {
                binance_usdm: vec!["BTCUSDT".to_string()],
                binance_spot: vec![],
                bybit_linear: vec!["ETHUSDT".to_string()],
            },
            ..Profile::default()
        };
        let venues = cex_venues(&profile);
        assert_eq!(venues[0].0.src(), "binance-usdm");
        assert_eq!(venues[1].0.src(), "binance-spot");
        assert_eq!(venues[2].0.src(), "bybit-linear");
        assert_eq!(venues[0].1, &vec!["BTCUSDT".to_string()]);
        assert!(venues[1].1.is_empty());
        assert_eq!(venues[2].1, &vec!["ETHUSDT".to_string()]);
    }

    /// The default profile enables all three CEX venues (SPEC-0008 §7.3).
    #[test]
    fn default_profile_has_non_empty_cex_lists() {
        // `Profile::default()` has empty lists; the §7.3 symbols live in the
        // repository's `config/record.toml`. Parse that file to prove the
        // wiring sees them (guards against a silent rename of the `[cex]` keys).
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("config/record.toml");
        let config = parse_toml(&std::fs::read_to_string(path).unwrap()).unwrap();
        let default = config.profile.get("default").unwrap();
        assert!(!default.cex.binance_usdm.is_empty());
        assert!(!default.cex.binance_spot.is_empty());
        assert!(!default.cex.bybit_linear.is_empty());
    }

    /// A CEX source pointed at a mock server writes a finalized `bybit-linear`
    /// segment with `frame` envelopes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cex_source_writes_a_bybit_linear_segment() {
        let tmp = temp_dir("cex-bybit");
        let dir = tmp.path();

        // Mock WS server: accept one connection, push frames, and read the
        // subscribe message.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = accept_async(stream).await.unwrap();
            let _ = socket.next().await; // the subscribe frame
            let frame = r#"{"topic":"orderbook.1.BTCUSDT","data":{"b":[["1","1"]]}}"#;
            loop {
                if socket.send(Message::Text(frame.into())).await.is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });

        let clock: Arc<dyn EnvelopeClock> = Arc::new(SystemEnvelopeClock::new());
        let writer = Arc::new(
            SegmentWriter::spawn(segment_config(
                &Profile {
                    out_dir: dir.to_path_buf(),
                    ..Profile::default()
                },
                Network::Testnet,
                "bybit-linear",
                "bybit-linear",
                &SegmentOpenMeta::default(),
                clock.clone(),
                1,
                Arc::new(MountGuard::unguarded()),
            ))
            .unwrap(),
        );
        let sink = Arc::new(CountingSink {
            writer: writer.clone(),
            clock: clock.clone(),
            health: RecorderHealth::new(Vec::new()),
            metrics: ConnMetrics::new("bybit-linear", "bybit-linear"),
            state: ConnState::new(),
            account_hl_rest: false,
        });
        let config = CexConfig {
            base_url: format!("ws://{addr}"),
            ..CexConfig::new(CexKind::BybitLinear, vec!["BTCUSDT".to_string()])
        };
        let source = CexSource::new(config, sink, clock);
        let shutdown = Arc::new(Notify::new());
        let task = tokio::spawn(run_cex(source, shutdown.clone()));

        // Wait for at least one frame to reach the segment writer.
        for _ in 0..400 {
            tokio::time::sleep(Duration::from_millis(5)).await;
            let has_bytes = segment_files(dir)
                .first()
                .and_then(|path| std::fs::metadata(path).ok())
                .map(|meta| meta.len())
                .unwrap_or(0)
                > 0;
            if has_bytes {
                break;
            }
        }
        shutdown.notify_one();
        task.await.unwrap();
        server.abort();
        drop(writer);

        let files = segment_files(dir);
        assert!(!files.is_empty(), "no bybit-linear segment was written");
        let report = reader::inspect(&files).unwrap();
        assert!(
            report.by_src.get("bybit-linear").copied().unwrap_or(0) > 0,
            "segment has no bybit-linear records: {:#?}",
            report.by_src
        );
        assert!(
            report.by_kind.get("frame").copied().unwrap_or(0) > 0,
            "expected frame envelopes: {:#?}",
            report.by_kind
        );
        assert!(
            report.by_kind.get("segment_close").copied().unwrap_or(0) == 1,
            "segment did not finalize: {:#?}",
            report.by_kind
        );
    }

    /// End-to-end wiring test: a mock WS server and a mock REST server feed the
    /// real connection/snapshotter tasks for ~2 s; the resulting segments must
    /// contain `segment_open`, `sub`, `frame`, and `segment_close`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn recorder_writes_segments_from_mock_feeds() {
        let tmp = temp_dir("e2e");
        let dir = tmp.path();

        // Mock WS server: accept one connection, then push a frame every 50 ms.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ws_addr = listener.local_addr().unwrap();
        let (ws_shutdown_tx, mut ws_shutdown_rx) = watch::channel(false);
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = accept_async(stream).await.unwrap();
            let mut tick = tokio::time::interval(Duration::from_millis(50));
            loop {
                tokio::select! {
                    _ = ws_shutdown_rx.changed() => break,
                    _ = tick.tick() => {
                        let frame = r#"{"channel":"bbo","data":{"coin":"BTC"}}"#;
                        if socket.send(Message::Text(frame.to_string().into())).await.is_err() {
                            break;
                        }
                    }
                    message = socket.next() => {
                        if message.is_none() { break; }
                    }
                }
            }
        });

        // Mock REST server.
        let rest = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/info"))
            .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
            .mount(&rest)
            .await;

        let network = Network::Testnet;
        let clock: Arc<dyn EnvelopeClock> = Arc::new(SystemEnvelopeClock::new());
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let rest_shutdown = Arc::new(Notify::new());
        let health = RecorderHealth::new(vec![("hl-ws-01".to_string(), ConnState::new())]);

        // WS connection task.
        let conn = Connection {
            id: "hl-ws-01".to_string(),
            subs: vec![Subscription {
                stream: Stream::Bbo,
                coin: Some("BTC".to_string()),
                dex: None,
            }],
        };
        let writer = SegmentWriter::spawn(segment_config(
            &Profile {
                out_dir: dir.to_path_buf(),
                ..Profile::default()
            },
            network,
            "hl-ws",
            "hl-ws-01",
            &SegmentOpenMeta::default(),
            clock.clone(),
            1,
            Arc::new(MountGuard::unguarded()),
        ))
        .unwrap();
        let ws_url = format!("ws://{ws_addr}");
        let ws_task = tokio::spawn(run_ws_conn(
            conn,
            Box::new(move || {
                Box::new(TestProtocol {
                    url: ws_url.clone(),
                }) as Box<dyn Protocol>
            }),
            writer,
            clock.clone(),
            ConnState::new(),
            shutdown_rx.clone(),
            Duration::ZERO,
        ));

        // REST task.
        let rest_writer = Arc::new(
            SegmentWriter::spawn(segment_config(
                &Profile {
                    out_dir: dir.to_path_buf(),
                    ..Profile::default()
                },
                network,
                "hl-rest",
                "hl-rest",
                &SegmentOpenMeta::default(),
                clock.clone(),
                1,
                Arc::new(MountGuard::unguarded()),
            ))
            .unwrap(),
        );
        let snapshotter = SnapshotterConfig {
            base_url: rest.uri(),
            out_dir: dir.to_path_buf(),
            funding_coins: Vec::new(),
            candle_coins: Vec::new(),
            meta_refresh: Duration::from_secs(3600),
            ctx_interval: Duration::from_secs(3600),
            predicted_fundings_interval: Duration::from_secs(3600),
            ..SnapshotterConfig::default()
        };
        let rest_task = tokio::spawn(run_rest(
            snapshotter,
            rest_writer,
            clock.clone(),
            health,
            rest_shutdown.clone(),
        ));

        tokio::time::sleep(Duration::from_millis(2_000)).await;
        let _ = shutdown_tx.send(true);
        let _ = ws_shutdown_tx.send(true);
        rest_shutdown.notify_one();
        let _ = ws_task.await;
        let _ = rest_task.await;
        let _ = server.await;

        let files = segment_files(dir);
        assert!(!files.is_empty(), "no segment files were written");
        let report = reader::inspect(&files).unwrap();
        for kind in ["segment_open", "sub", "frame", "segment_close"] {
            assert!(
                report.by_kind.get(kind).copied().unwrap_or(0) > 0,
                "missing {kind} in {:#?}",
                report.by_kind
            );
        }
        assert!(report.records >= 4);
    }
}
