//! `hl record` run: mount guarding, source spawning, and shutdown wiring
//! (SPEC-0008 §12.1).

use super::config::{Profile, Universe, load_profile, network_dir, profile_network};
use super::connection::{ConnMetrics, ConnState, RecorderHealth, run_ws_conn};
use super::monitor::{disk_monitor, readiness_monitor};
use super::sources::{CountingSink, run_cex, run_deribit, run_rest};
use super::*;

/// Bound on waiting for one recorder task to stop during shutdown.
///
/// A task that misses the shutdown signal must not stall the process until an
/// operator's SIGKILL (PERF-001); after this bound the join is abandoned, the
/// laggard is named in a warning, and shutdown continues.
const SHUTDOWN_JOIN_TIMEOUT: Duration = Duration::from_secs(20);

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

    let mut tasks: Vec<(&'static str, JoinHandle<()>)> = Vec::new();

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
        tasks.push((
            "hl-ws",
            tokio::spawn(run_ws_conn(
                conn.clone(),
                protocol,
                writer,
                clock.clone(),
                state,
                shutdown_rx.clone(),
                delay,
            )),
        ));
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
        tasks.push((
            "hl-rest",
            tokio::spawn(run_rest(
                snapshotter,
                writer,
                clock.clone(),
                recorder_health.clone(),
                rest_shutdown.clone(),
            )),
        ));
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
        tasks.push((
            "deribit",
            tokio::spawn(run_deribit(
                deribit_config(&profile),
                writer,
                clock.clone(),
                recorder_health.clone(),
                deribit_shutdown.clone(),
            )),
        ));
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
        tasks.push((src, tokio::spawn(run_cex(source, shutdown_rx.clone()))));
    }

    tasks.push((
        "disk-monitor",
        tokio::spawn(disk_monitor(profile.out_dir.clone(), shutdown_rx.clone())),
    ));
    tasks.push((
        "readiness-monitor",
        tokio::spawn(readiness_monitor(
            recorder_health,
            health.clone(),
            clock.clone(),
            mount_guard.clone(),
            shutdown_rx.clone(),
        )),
    ));

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

    let stop_cause = if mount_tripped {
        "mount_guard"
    } else {
        "signal"
    };
    info!(
        cause = stop_cause,
        "shutdown requested; finalizing recorder"
    );
    let _ = shutdown_tx.send(true);
    rest_shutdown.notify_one();
    deribit_shutdown.notify_one();
    // The CEX sources share `shutdown_rx` (a watch channel), so the one
    // `shutdown_tx.send(true)` above wakes all of them. Bound each join so a
    // task that misses the signal cannot hold the process until SIGKILL
    // (PERF-001); name the laggard and keep going.
    for (source, task) in tasks {
        if tokio::time::timeout(SHUTDOWN_JOIN_TIMEOUT, task)
            .await
            .is_err()
        {
            warn!(
                source,
                timeout_secs = SHUTDOWN_JOIN_TIMEOUT.as_secs(),
                "recorder task did not stop within the shutdown timeout"
            );
        }
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
pub(super) fn recorder_exit(mount_tripped: bool, guard: &MountGuard) -> Result<()> {
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
pub(super) fn create_out_dir(out_dir: &Path, guard: &MountGuard) -> Result<()> {
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
pub(super) async fn wait_for_mount_stop(guard: Arc<MountGuard>) {
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
pub(super) async fn probe_mount(guard: Arc<MountGuard>, timeout: Duration) -> bool {
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
pub(super) fn connection_priority(conn: &Connection) -> u8 {
    conn.subs
        .iter()
        .map(|sub| sub.stream.priority())
        .min()
        .unwrap_or(3)
}

/// Build the [`SegmentConfig`] for one `(src, conn)` writer.
#[allow(clippy::too_many_arguments)]
pub(super) fn segment_config(
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
pub(super) fn snapshotter_config(
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
pub(super) fn deribit_config(profile: &Profile) -> DeribitConfig {
    DeribitConfig {
        base_url: profile.deribit.base_url.clone(),
        currencies: profile.deribit.currencies.clone(),
        ..DeribitConfig::default()
    }
}

/// The configured CEX venues and their symbol lists (SPEC-0008 §9, R-8).
pub(super) fn cex_venues(profile: &Profile) -> [(CexKind, &Vec<String>); 3] {
    [
        (CexKind::BinanceUsdm, &profile.cex.binance_usdm),
        (CexKind::BinanceSpot, &profile.cex.binance_spot),
        (CexKind::BybitLinear, &profile.cex.bybit_linear),
    ]
}

/// Wall-clock milliseconds since the epoch.
pub(super) fn now_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Host name for `segment_open.meta.host`.
pub(super) fn hostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("HOST"))
        .unwrap_or_else(|_| "unknown".to_string())
}

/// Git commit for `segment_open.meta.git_sha` (set at build time if available).
pub(super) fn git_sha() -> String {
    option_env!("HL_GIT_SHA").unwrap_or("unknown").to_string()
}
