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
    spot_meta: hl_arb_client::types::SpotMeta,
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

    let prometheus = hl_arb_metrics::prometheus::install_recorder();
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

    // Non-gating reference streams (R-8). Created up front so their liveness is
    // registered with the readiness monitor before any source is spawned.
    let cex_states: Vec<(String, Arc<ConnState>)> = cex_venues(&profile)
        .into_iter()
        .filter(|(_, symbols)| !symbols.is_empty())
        .map(|(kind, _)| {
            let state = ConnState::new();
            // Seed freshness so a source that is still dialing at startup is
            // not reported stale for the first watchdog window.
            state.touch(&*clock);
            (kind.src().to_string(), state)
        })
        .collect();
    let recorder_health = RecorderHealth::new(states.clone(), cex_states.clone());

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
        let delay = hl_arb_recorder::MIN_NEW_CONN_INTERVAL * index as u32;
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
        let state = cex_states
            .iter()
            .find(|(name, _)| name == src)
            .map(|(_, state)| state.clone())
            .context("cex connection state missing")?;
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
            health: recorder_health.clone(),
            metrics: ConnMetrics::new(src, src),
            state: state.clone(),
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
    plan: &hl_arb_recorder::Plan,
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
    /// Gating connections: `/readyz` is not ready if any of these is down.
    conns: Vec<(String, Arc<ConnState>)>,
    /// Non-gating reference streams (R-8 CEX). A dead one is reported (a
    /// rate-limited WARN and the `hl_ws_connected{src}` gauge) but does not
    /// turn `/readyz` red, so a flaky reference feed cannot take the recorder
    /// out of rotation.
    cex: Vec<(String, Arc<ConnState>)>,
    rest_last_ns: AtomicU64,
}

impl RecorderHealth {
    fn new(conns: Vec<(String, Arc<ConnState>)>, cex: Vec<(String, Arc<ConnState>)>) -> Arc<Self> {
        Arc::new(Self {
            conns,
            cex,
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

    /// The first CEX stream that has not produced data within the watchdog
    /// (or never has), for reporting. Non-gating: never affects `ready`.
    fn cex_down(&self, now_ns: u64, watchdog_ns: u64) -> Option<&str> {
        self.cex.iter().find_map(|(name, state)| {
            let last = state.last_ns.load(Ordering::Relaxed);
            if last != 0 && now_ns.saturating_sub(last) <= watchdog_ns {
                None
            } else {
                Some(name.as_str())
            }
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
    //
    // A gap is stamped at the wall-clock/monotonic instant it began (the raw
    // `Gap` carries the disconnect instant, `SPEC-0008 RW-2`) so
    // `gap_end.t_ns - gap_start.t_ns` measures the real downtime. Only the
    // first failure of an outage emits a `gap_start`.
    let mut gap_started: Option<(i64, u64)> = None;
    let mut gap_reason = String::new();
    let mut raw = loop {
        match RawWsConn::connect(protocol(), subs.clone()).await {
            Ok(raw) => break raw,
            Err(err) => {
                warn!(conn = %conn_id, error = %err, "websocket connect failed; retrying");
                if gap_started.is_none() {
                    let t_ns = clock.t_ns();
                    let mono_ns = clock.mono_ns();
                    seq = emit_gap_start(
                        &metrics,
                        &writer,
                        &state,
                        &*clock,
                        &conn_id,
                        t_ns,
                        mono_ns,
                        "error",
                        &err.to_string(),
                        seq,
                    );
                    gap_started = Some((t_ns, mono_ns));
                }
                gap_reason = "error".to_string();
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
    if let Some((started_t_ns, _)) = gap_started.take() {
        let t_ns = clock.t_ns();
        let gap_ms = t_ns.saturating_sub(started_t_ns).max(0) as u64 / 1_000_000;
        let env = Envelope::gap_end_at(src, conn_id.as_str(), seq, t_ns, clock.mono_ns(), gap_ms);
        emit(&metrics, &writer, &state, &*clock, env);
        seq += 1;
        record_gap_seconds(src, &conn_id, &gap_reason, gap_ms);
    }

    let mut clock_tick = tokio::time::interval(Duration::from_secs(60));
    clock_tick.tick().await; // consume the immediate first tick

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                let t_ns = clock.t_ns();
                let mono_ns = clock.mono_ns();
                emit_gap_start(&metrics, &writer, &state, &*clock, &conn_id, t_ns, mono_ns, "shutdown", "shutdown requested", seq);
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
                    if let Some((started_t_ns, _)) = gap_started.take() {
                        let t_ns = clock.t_ns();
                        let gap_ms = t_ns.saturating_sub(started_t_ns).max(0) as u64 / 1_000_000;
                        let env = Envelope::gap_end_at(
                            src,
                            conn_id.as_str(),
                            seq,
                            t_ns,
                            clock.mono_ns(),
                            gap_ms,
                        );
                        emit(&metrics, &writer, &state, &*clock, env);
                        seq += 1;
                        record_gap_seconds(src, conn_id.as_str(), &gap_reason, gap_ms);
                    }
                }
                Ok(RawEvent::Gap {
                    reason,
                    detail,
                    disconnect_ns,
                    t_ns,
                }) => {
                    state.set_connected(false);
                    // Exactly one `gap_start` per outage: `RawWsConn` now emits
                    // a single `Gap` for the whole outage (it stays cancel-safe
                    // across clock ticks), so keep the first disconnect instant
                    // rather than overwriting it if another `Gap` ever slipped
                    // through. `gap_end` then covers the whole outage.
                    if gap_started.is_none() {
                        seq = emit_gap_start(
                            &metrics,
                            &writer,
                            &state,
                            &*clock,
                            &conn_id,
                            t_ns,
                            disconnect_ns,
                            &reason,
                            &detail,
                            seq,
                        );
                        gap_reason = reason;
                        gap_started = Some((t_ns, disconnect_ns));
                    }
                }
                Err(err) => {
                    state.set_connected(false);
                    let t_ns = clock.t_ns();
                    let mono_ns = clock.mono_ns();
                    emit_gap_start(&metrics, &writer, &state, &*clock, &conn_id, t_ns, mono_ns, "error", &err.to_string(), seq);
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

/// Write a `gap_start` envelope stamped at `t_ns`/`mono_ns` and return the next
/// `seq`.
#[allow(clippy::too_many_arguments)]
fn emit_gap_start(
    metrics: &ConnMetrics,
    writer: &SegmentWriter,
    state: &ConnState,
    clock: &dyn EnvelopeClock,
    conn: &str,
    t_ns: i64,
    mono_ns: u64,
    reason: &str,
    detail: &str,
    seq: u64,
) -> u64 {
    let env = Envelope::gap_start_at("hl-ws", conn, seq, t_ns, mono_ns, reason, detail);
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
///
/// Non-gating CEX streams are reported here (a transition WARN; the
/// `hl_ws_connected{src}` gauge is maintained by `RawWsConn`) but never affect
/// `/readyz`, so a dead reference feed cannot take the recorder out of rotation.
async fn readiness_monitor(
    recorder: Arc<RecorderHealth>,
    health: Health,
    clock: Arc<dyn EnvelopeClock>,
    mount_guard: Arc<MountGuard>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut tick = tokio::time::interval(READY_SAMPLE_INTERVAL);
    let mut cex_degraded = false;
    loop {
        tokio::select! {
            _ = tick.tick() => {
                let now_ns = clock.mono_ns();
                let ready = !mount_guard.is_tripped() && recorder.ready(
                    now_ns,
                    READY_WATCHDOG.as_nanos() as u64,
                    REST_READY_STALE.as_nanos() as u64,
                );
                health.set_ready(ready);
                match recorder.cex_down(now_ns, READY_WATCHDOG.as_nanos() as u64) {
                    Some(src) if !cex_degraded => {
                        warn!(src, "cex reference stream is stale (non-gating)");
                        cex_degraded = true;
                    }
                    None if cex_degraded => {
                        info!("cex reference streams recovered");
                        cex_degraded = false;
                    }
                    _ => {}
                }
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
pub fn verify(
    profile: Option<String>,
    network: Option<Network>,
    date: &str,
    allow_orphans: bool,
    strict: bool,
) -> Result<()> {
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
    println!("partials: {}", report.partials.len());
    for partial in &report.partials {
        println!("  PARTIAL {partial}");
    }
    if !report.partials.is_empty() {
        println!(
            "  note: `.partial` files are present, so a recorder may be running; \
             orphan findings below are reported as possibly in flight and do not \
             fail verify"
        );
    }
    println!("orphans: {}", report.orphans.len());
    for orphan in &report.orphans {
        println!(
            "  ORPHAN {:<70} {}/{} records={}",
            orphan.file, orphan.src, orphan.conn, orphan.records
        );
    }
    println!(
        "crashed, no manifest line: {}",
        report.crashed_no_manifest.len()
    );
    for seg in &report.crashed_no_manifest {
        println!(
            "  CRASHED {:<70} {}/{} records={}",
            seg.file, seg.src, seg.conn, seg.records
        );
    }
    println!(
        "unfinalized, no manifest line: {}",
        report.unfinalized.len()
    );
    for seg in &report.unfinalized {
        println!(
            "  UNFINALIZED {:<70} {}/{} records={}",
            seg.file, seg.src, seg.conn, seg.records
        );
    }
    println!("corrupt orphans: {}", report.corrupt_orphans.len());
    for seg in &report.corrupt_orphans {
        println!("  CORRUPT {:<70} {}", seg.file, seg.error);
    }
    println!("coverage:");
    for stream in &report.coverage {
        println!(
            "  {}/{}: {:.2}% ({} ms)",
            stream.src, stream.conn, stream.coverage_pct, stream.covered_ms
        );
    }
    verify_exit(&report, allow_orphans, strict)
}

/// Decide `verify`'s exit status from the report (SPEC-0008 §17 #36).
///
/// Failing findings: repairable finished orphans (unless `--allow-orphans`, or
/// `*.partial` files are present and they may be in flight), corrupt orphans and
/// unfinalized segments (unless `--allow-orphans`). Warning only, unless
/// `--strict`: `.crashed` segments with no manifest line, which are recovered by
/// restarting the recorder.
fn verify_exit(report: &reader::VerifyReport, allow_orphans: bool, strict: bool) -> Result<()> {
    let in_flight = !report.partials.is_empty();
    if in_flight {
        warn!(
            partials = report.partials.len(),
            "`.partial` files are present; treating orphan findings as possibly in flight"
        );
    }

    if !report.orphans.is_empty() {
        if allow_orphans || in_flight {
            warn!(
                orphans = report.orphans.len(),
                "finished orphans ignored ({})",
                if in_flight {
                    "possibly in flight; a `.partial` file is present"
                } else {
                    "--allow-orphans"
                }
            );
        } else {
            bail!(
                "{} finished segment(s) on disk have no manifest line (orphans); run \
                 `hl record repair-manifest --date {}` or pass --allow-orphans",
                report.orphans.len(),
                report.date
            );
        }
    }

    if !report.corrupt_orphans.is_empty() {
        if allow_orphans {
            warn!(
                corrupt_orphans = report.corrupt_orphans.len(),
                "corrupt orphan segments ignored because --allow-orphans was passed"
            );
        } else {
            let first = &report.corrupt_orphans[0];
            bail!(
                "{} segment(s) on disk have no manifest line and did not decode \
                 (corrupt orphans), e.g. `{}`: {}; not repairable by repair-manifest, \
                 recover by restarting the recorder; pass --allow-orphans to ignore",
                report.corrupt_orphans.len(),
                first.file,
                first.error
            );
        }
    }

    // A finalized-name segment without `segment_close` cannot be repaired by
    // `repair-manifest` (restarting the recorder only recovers `.partial`
    // files), so it fails like a corrupt orphan unless allowed.
    if !report.unfinalized.is_empty() {
        if allow_orphans {
            warn!(
                unfinalized = report.unfinalized.len(),
                "unfinalized segments ignored because --allow-orphans was passed"
            );
        } else {
            let first = &report.unfinalized[0];
            bail!(
                "{} segment(s) on disk have no manifest line and no segment_close \
                 (unfinalized), e.g. `{}`; not repairable by repair-manifest; pass \
                 --allow-orphans to ignore",
                report.unfinalized.len(),
                first.file
            );
        }
    }

    // A `.crashed` file without a manifest line is expected after a crash whose
    // manifest append was lost: warn, and fail only with `--strict`.
    if !report.crashed_no_manifest.is_empty() {
        if strict {
            bail!(
                "{} crashed segment(s) on disk have no manifest line; not repairable by \
                 repair-manifest, recover by restarting the recorder",
                report.crashed_no_manifest.len()
            );
        }
        warn!(
            crashed = report.crashed_no_manifest.len(),
            "crashed segments with no manifest line are not repairable by repair-manifest; \
             recover by restarting the recorder (pass --strict to fail)"
        );
    }
    Ok(())
}

/// Append the missing manifest line for each orphan finished segment (SPEC-0008
/// §17 #36). A maintenance tool: it never runs automatically and never creates a
/// directory.
pub fn repair_manifest(
    profile: Option<String>,
    network: Option<Network>,
    date: &str,
    dry_run: bool,
) -> Result<()> {
    let (name, profile) = load_profile(profile.as_deref(), network)?;
    let network = profile_network(&profile)?;
    let actions = reader::repair_manifest(&reader::RepairConfig {
        out_dir: profile.out_dir.clone(),
        network: network_dir(network).to_string(),
        date: date.to_string(),
        dry_run,
    })?;
    println!("profile: {name}  network: {}", network_dir(network));
    println!("date: {date}");
    let mut appended = 0usize;
    for action in &actions {
        match action {
            reader::RepairAction::Append { entry } => {
                appended += 1;
                if dry_run {
                    println!("would append: {}", serde_json::to_string(entry)?);
                } else {
                    println!("appended: {}", entry.file);
                }
            }
            reader::RepairAction::Skip { file, reason } => {
                println!("skipped: {file} ({reason})");
            }
        }
    }
    println!(
        "repaired: {appended}{}",
        if dry_run { " (dry run)" } else { "" }
    );
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

#[cfg(test)]
mod tests;
