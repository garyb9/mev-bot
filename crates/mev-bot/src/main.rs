//! `hl` — orchestration binary for the Hyperliquid-first trading system.
//!
//! M0.x: platform (config, observability, health, shutdown).
//! M1.1:  REST `/info` client and the `markets`/`book` commands.

use std::{
    collections::{BTreeMap, BTreeSet},
    net::SocketAddr,
    path::PathBuf,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

mod engine;
mod record;

use anyhow::{Context as _, Result};
use axum::{Router, extract::State, http::StatusCode, routing::get};
use clap::{Parser, Subcommand, ValueEnum};
use futures_util::{StreamExt, stream::FuturesUnordered};
use mev_core::{
    clock::{Clock, SystemClock},
    config::{Config, ConfigOverrides, Mode, Network},
    db::{Db, writer::DbWriter},
};
use mev_engine::{
    AccountUpdate, Cloid, CoinRegistry, Control, MarketUpdate, OrderAck, PostResult, Stamp,
    StrategyDispatcher, VenueOrderStatus,
    builder::AssetTable,
    channels::{
        ACCOUNT_CHANNEL_CAP, InputHandles, MARKET_CHANNEL_CAP, MarketSend, inputs, outbound,
    },
    dispatch::DispatcherConfig,
    exec::UnsignedPost,
    ingest::Ingest,
    paper_exec::{PaperConfig, PaperExec},
    risk::RiskGate,
    run::{EngineLoop, LoopConfig},
    types::ConnId,
};
use mev_hl_client::{
    Action, AgentSigner, AssetMap, DeadMansSwitch, ExchangeApi, HlProtocol, HttpInfo, InfoApi,
    Market, MarketKind, MarketSelector, MarketState, MarketStream, OrderParams, OrderStatus,
    RawEvent, RawWsConn, StreamEvent, Subscription, Tif, Tolerance, WsExchange, WsMarketStream,
    build_order_wire, build_request, now_ms,
};
use mev_metrics::{health::Health, prometheus::PrometheusHandle};
use mev_strategy::{AccountView, FeeRates, Instrument};
use rust_decimal::Decimal;
use smallvec::SmallVec;
use tokio::signal;
use tracing::{error, info};

#[derive(Parser)]
#[command(name = "hl", version, about = "Hyperliquid-first trading system")]
struct Cli {
    /// Network override (default: from config).
    #[arg(long, value_enum, global = true)]
    network: Option<NetworkArg>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Launch the bot with the configured watchlist.
    Run {
        /// Execution mode (default: observe).
        #[arg(long, value_enum)]
        mode: Option<ModeArg>,
        /// Comma-separated coin override, e.g. `--coins BTC,ETH`.
        #[arg(long, value_delimiter = ',')]
        coins: Option<Vec<String>>,
        /// SQLite database path override.
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// Inspect resolved configuration.
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
    /// List/search markets (SPEC-0001).
    Markets {
        /// Optional filter substring.
        query: Option<String>,
        /// Show perpetuals only.
        #[arg(long)]
        perp: bool,
        /// Show spot pairs only.
        #[arg(long)]
        spot: bool,
        /// List a builder-deployed HIP-3 dex (e.g. `xyz`, `cash`) instead of the default.
        #[arg(long)]
        dex: Option<String>,
    },
    /// List builder-deployed HIP-3 perpetual dexes (SPEC-0001).
    Dexs,
    /// Show a live book snapshot (SPEC-0001).
    Book {
        /// Market symbol.
        coin: String,
        /// Number of book levels per side.
        #[arg(long, default_value_t = 5)]
        levels: usize,
    },
    /// Stream market data (SPEC-0001).
    Watch {
        /// Market symbols.
        coins: Vec<String>,
    },
    /// Build and sign an order without submitting it (SPEC-0002).
    Order(OrderArgs),
    /// Show account state (positions, margin, open orders).
    Account {
        /// Account address (`0x...`).
        address: String,
    },
    /// Edit the persisted watchlist (SPEC-0001).
    Select {
        /// Replace the watchlist with these coins.
        coins: Vec<String>,
        /// Add coins to the watchlist.
        #[arg(long)]
        add: Vec<String>,
        /// Remove coins from the watchlist.
        #[arg(long)]
        remove: Vec<String>,
    },
    /// Deterministically replay a recorded session (SPEC-0003 §10).
    Replay {
        /// Session id to replay (default: the most recent).
        #[arg(long)]
        session: Option<i64>,
        /// SQLite database path override.
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// Trip the kill switch: write the flag file the running bot polls
    /// (SPEC-0004 K-3). Does not touch the network.
    Panic,
    /// Clear the kill switch: remove the flag file. The in-process flag stays
    /// sticky until the bot is restarted or resumed (SPEC-0004 K-3).
    Resume,
    /// Market-data recorder (SPEC-0008). Never loads keys or places orders.
    Record {
        /// Recording profile name in `config/record.toml` (default: `default`).
        #[arg(long, global = true)]
        profile: Option<String>,
        /// Allow dropping priority-1 subscriptions when over budget (SPEC-0008 §7.4).
        #[arg(long, global = true)]
        allow_truncate: bool,
        #[command(subcommand)]
        cmd: Option<RecordCmd>,
    },
    /// Connectivity and latency probes (SPEC-0008 §12.1).
    Probe {
        #[command(subcommand)]
        cmd: ProbeCmd,
    },
}

/// Subcommands of `hl record`.
#[derive(Subcommand)]
enum RecordCmd {
    /// Print the resolved subscription plan and exit; opens no recording sockets.
    Plan,
    /// Inspect one or more segment files or directories (SPEC-0008 §12.1).
    Inspect {
        /// Segment files, or directories to walk recursively.
        paths: Vec<PathBuf>,
    },
    /// Check a day's manifest against the files on disk (SPEC-0008 §12.1).
    Verify {
        /// UTC date to verify, `YYYY-MM-DD`.
        #[arg(long)]
        date: String,
    },
}

/// Subcommands of `hl probe`.
#[derive(Subcommand)]
enum ProbeCmd {
    /// Measure TCP/TLS/WS/REST latency to Hyperliquid (used by SPEC-0008 V-4).
    Latency {
        /// Number of samples per measurement.
        #[arg(long, default_value_t = 20)]
        count: u32,
    },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Print the resolved config with secrets redacted.
    Show,
}

/// Arguments for the dry-run `order` command.
#[derive(clap::Args)]
struct OrderArgs {
    /// Market symbol (e.g. `BTC` or `xyz:TSLA`).
    coin: String,
    /// Order side.
    #[arg(long, value_enum)]
    side: Side,
    /// Order size.
    #[arg(long)]
    sz: String,
    /// Limit price.
    #[arg(long)]
    px: String,
    /// Time in force.
    #[arg(long, value_enum, default_value_t = TifArg::Gtc)]
    tif: TifArg,
    /// Mark the order reduce-only.
    #[arg(long)]
    reduce_only: bool,
    /// Optional client order id (`0x` + 32 hex chars).
    #[arg(long)]
    cloid: Option<String>,
}

#[derive(Clone, Copy, ValueEnum)]
enum ModeArg {
    Observe,
    Simulate,
    Live,
}

#[derive(Clone, Copy, ValueEnum)]
enum Side {
    Buy,
    Sell,
}

#[derive(Clone, Copy, ValueEnum)]
enum TifArg {
    Alo,
    Ioc,
    Gtc,
}

impl From<TifArg> for Tif {
    fn from(value: TifArg) -> Self {
        match value {
            TifArg::Alo => Tif::Alo,
            TifArg::Ioc => Tif::Ioc,
            TifArg::Gtc => Tif::Gtc,
        }
    }
}

impl From<ModeArg> for Mode {
    fn from(value: ModeArg) -> Self {
        match value {
            ModeArg::Observe => Mode::Observe,
            ModeArg::Simulate => Mode::Simulate,
            ModeArg::Live => Mode::Live,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum NetworkArg {
    Mainnet,
    Testnet,
}

impl From<NetworkArg> for Network {
    fn from(value: NetworkArg) -> Self {
        match value {
            NetworkArg::Mainnet => Network::Mainnet,
            NetworkArg::Testnet => Network::Testnet,
        }
    }
}

#[tokio::main]
async fn main() {
    mev_metrics::logging::init();
    std::panic::set_hook(Box::new(|info| {
        error!(panic = %info, "panic");
    }));

    let cli = Cli::parse();
    let command = cli.command.unwrap_or(Command::Run {
        mode: None,
        coins: None,
        db: None,
    });

    if let Err(err) = dispatch(command, cli.network).await {
        error!(error = %err, "fatal");
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}

async fn dispatch(command: Command, network: Option<NetworkArg>) -> Result<()> {
    match command {
        Command::Run { mode, coins, db } => run(mode, network, coins, db).await,
        Command::Config { cmd } => {
            match cmd {
                ConfigCmd::Show => show_config()?,
            }
            Ok(())
        }
        Command::Markets {
            query,
            perp,
            spot,
            dex,
        } => markets(network, query, perp, spot, dex).await,
        Command::Dexs => dexs(network).await,
        Command::Book { coin, levels } => book(network, coin, levels).await,
        Command::Watch { coins } => watch(network, coins).await,
        Command::Order(args) => order(network, args).await,
        Command::Account { address } => account(network, address).await,
        Command::Select { coins, add, remove } => select(network, coins, add, remove).await,
        Command::Replay { session, db } => replay(network, session, db).await,
        Command::Panic => set_kill_switch(network, true),
        Command::Resume => set_kill_switch(network, false),
        Command::Record {
            profile,
            allow_truncate,
            cmd,
        } => match cmd {
            None => record::run(profile, network.map(Into::into), allow_truncate).await,
            Some(RecordCmd::Plan) => {
                record::plan(profile, network.map(Into::into), allow_truncate).await
            }
            Some(RecordCmd::Inspect { paths }) => record::inspect(&paths),
            Some(RecordCmd::Verify { date }) => {
                record::verify(profile, network.map(Into::into), &date)
            }
        },
        Command::Probe { cmd } => match cmd {
            ProbeCmd::Latency { count } => {
                record::probe_latency(network.map(Into::into), count).await
            }
        },
    }
}

/// Write (`trip`) or remove (`clear`) the kill-switch flag file (SPEC-0004 K-3).
fn set_kill_switch(network: Option<NetworkArg>, trip: bool) -> Result<()> {
    let config = Config::load(ConfigOverrides {
        network: network.map(Into::into),
        ..Default::default()
    })?;
    let path = &config.kill_file;
    if trip {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(path, b"kill\n").with_context(|| format!("writing {}", path.display()))?;
        println!("kill switch tripped: {}", path.display());
    } else if path.exists() {
        std::fs::remove_file(path).with_context(|| format!("removing {}", path.display()))?;
        println!("kill switch flag removed: {}", path.display());
    } else {
        println!("kill switch flag already absent: {}", path.display());
    }
    Ok(())
}

fn resolve_network(network: Option<NetworkArg>) -> Result<Network> {
    if let Some(network) = network {
        return Ok(network.into());
    }
    Ok(Config::load(ConfigOverrides::default())?.network)
}

async fn run(
    mode: Option<ModeArg>,
    network: Option<NetworkArg>,
    coins: Option<Vec<String>>,
    db: Option<PathBuf>,
) -> Result<()> {
    let overrides = ConfigOverrides {
        mode: mode.map(Into::into),
        network: network.map(Into::into),
        coins,
        db_path: db,
    };
    let config = Config::load(overrides)?;

    let selector = selector_for(config.network, &config.watchlist).await?;
    let watchlist: Vec<String> = selector
        .resolve_all(&config.watchlist)?
        .iter()
        .map(|market| market.coin.clone())
        .collect();

    let metrics = mev_metrics::prometheus::install_recorder();
    metrics::counter!(mev_metrics::names::STARTUPS).increment(1);

    info!(
        network = ?config.network,
        mode = ?config.mode,
        autonomy = ?config.autonomy,
        watchlist = ?watchlist,
        "starting"
    );

    // Strategies run in `simulate` and `live`; `observe` stays read-only.
    let plan = if config.mode == Mode::Observe {
        None
    } else {
        Some(engine::build(&config, &selector)?)
    };

    let (subscriptions, book_coins, ctx_coins) = build_subscriptions(&watchlist, plan.as_ref());

    // Health-only market snapshot: it drives `/readyz` and feed staleness
    // metrics and never gates a decision (the engine owns trading state).
    let health_state = Arc::new(RwLock::new(MarketState::new(Tolerance::default())));
    if let Ok(mut guard) = health_state.write() {
        for coin in &book_coins {
            guard.expect_book(coin);
        }
        for coin in &ctx_coins {
            guard.expect_ctx(coin);
        }
    }
    let health = Health::new();

    // Simulate/live record their inputs to SQLite for deterministic replay.
    let session = if config.mode == Mode::Observe {
        None
    } else {
        let db = Db::open(&config.db_path)?;
        let session_id = db.create_session(
            &format!("{:?}", config.network),
            &format!("{:?}", config.mode),
            None,
            SystemClock.now_ms(),
        )?;
        info!(session_id, "recording session");
        Some((Arc::new(DbWriter::spawn(db, 4096)), session_id))
    };
    let recorder = session
        .as_ref()
        .map(|(writer, session_id)| engine::Recorder::new(writer.clone(), *session_id));

    let info: Arc<dyn InfoApi> = Arc::new(HttpInfo::new(config.network));
    let exchange = live_exchange(&config)?;

    // Interned coins: the strategy universe when strategies run, else the
    // watchlist. The market decoder resolves names through this registry.
    let registry = plan.as_ref().map_or_else(
        || CoinRegistry::from_coins(&watchlist),
        |build| build.registry.clone(),
    );
    let coin_count = registry.len();

    // Inbound channels (market lossy, account lossless) and the loop stop flag.
    let (handles, inputs) = inputs(MARKET_CHANNEL_CAP, ACCOUNT_CHANNEL_CAP);
    let (stop_tx, stop_rx) = crossbeam_channel::bounded(1);

    // Exec backend: the paper backend fills in-process for `simulate`; `live`
    // hands unsigned posts to a WS writer; `observe` has no backend.
    let mut exec_writer = None;
    let exec: Option<Box<dyn mev_engine::exec::ExecBackend + Send>> = match config.mode {
        Mode::Live => {
            let exchange = exchange
                .clone()
                .context("live mode requires a configured exchange")?;
            let (out, posts) = outbound::<UnsignedPost>(ACCOUNT_CHANNEL_CAP);
            exec_writer = Some(spawn_exec_writer(
                posts,
                exchange,
                info.clone(),
                config.account_address.clone(),
                handles.clone(),
            ));
            Some(Box::new(out))
        }
        _ => None,
    };

    let paper = if config.mode == Mode::Simulate {
        let instruments: BTreeMap<String, Instrument> = plan
            .as_ref()
            .map(|build| build.instruments.clone())
            .unwrap_or_default();
        let seeded = AccountView {
            account_value: Decimal::from(100_000),
            fees: FeeRates::PERP,
            ..Default::default()
        };
        Some(PaperExec::new(
            PaperConfig::default(),
            seeded,
            instruments,
            FeeRates::PERP,
            FeeRates::SPOT,
        ))
    } else {
        None
    };

    let strategies = plan.map(|build| build.strategies).unwrap_or_default();
    let table = AssetTable::from_selector(&registry, &selector);
    let risk = RiskGate::from_settings(&config.risk);
    let dispatcher_config = DispatcherConfig {
        max_slippage_bps: Decimal::from(config.strategy.max_slippage_bps),
    };
    let mut dispatcher = StrategyDispatcher::new(
        strategies,
        registry.clone(),
        table,
        risk,
        exec,
        dispatcher_config,
    );
    if let Some(paper) = paper {
        dispatcher = dispatcher.with_paper(paper);
    }
    let resting = dispatcher.resting_order_counter();
    let market_drops = dispatcher.market_drop_counter();

    // The loop is synchronous and owns all trading state: run it on its own
    // thread, stopped through the crossbeam channel on shutdown.
    let spin_us = config.engine.spin_us;
    let engine_handle = std::thread::Builder::new()
        .name("mev-engine".to_string())
        .spawn(move || {
            let loop_config = LoopConfig {
                spin_us,
                coin_count,
            };
            let engine = EngineLoop::with_dispatcher(inputs, dispatcher, loop_config, stop_rx);
            let _ = engine.run();
        })
        .context("spawning the engine thread")?;

    let heartbeat = tokio::spawn(heartbeat());
    // The health snapshot and the SQLite replay log are off the engine path:
    // the ingest task hands every raw frame to this sidecar and never waits.
    let (side_tx, side_rx) = tokio::sync::mpsc::channel::<SideFrame>(SIDE_CHANNEL_CAP);
    let sidecar_task = tokio::spawn(recording_sidecar(side_rx, health_state.clone(), recorder));
    let ingest_task = tokio::spawn(ingest(
        handles.clone(),
        subscriptions,
        registry.clone(),
        config.network,
        side_tx,
        market_drops,
    ));
    let monitor_task = tokio::spawn(monitor(health_state, health.clone()));
    // SPEC-0010 §15: the H-3 account stream is the source of truth for own
    // orders and fills; the REST reconciler is the 30 s backstop.
    let reconciler = config.account_address.clone().map(|address| {
        tokio::spawn(engine::account_reconciler(
            info.clone(),
            address,
            registry.clone(),
            handles.clone(),
        ))
    });
    let account_stream_task = config.account_address.as_ref().map(|address| {
        tokio::spawn(account_stream(
            handles.clone(),
            vec![
                Subscription::OrderUpdates {
                    user: address.clone(),
                },
                Subscription::UserFills {
                    user: address.clone(),
                },
                Subscription::UserEvents {
                    user: address.clone(),
                },
            ],
            registry.clone(),
            config.network,
        ))
    });
    let control_task = tokio::spawn(control(handles.clone(), config.kill_file.clone()));
    let deadman_task = exchange.clone().map(|exchange| {
        tokio::spawn(deadman(
            exchange,
            info.clone(),
            config.account_address.clone(),
            resting.clone(),
            handles.clone(),
            config.schedule_cancel_ttl_ms,
        ))
    });

    info!("waiting for feeds to become ready");
    serve(health, metrics, config.http_port).await?;

    // Stop the engine first, then tear the I/O tasks down.
    let _ = stop_tx.send(());
    let _ = engine_handle.join();
    ingest_task.abort();
    sidecar_task.abort();
    monitor_task.abort();
    heartbeat.abort();
    control_task.abort();
    if let Some(account_stream) = account_stream_task {
        account_stream.abort();
    }
    if let Some(reconciler) = reconciler {
        reconciler.abort();
    }
    if let Some(writer) = exec_writer {
        writer.abort();
    }
    // The dead-man task disarms `scheduleCancel` on the same shutdown signal;
    // give it a moment to submit before the process exits.
    if let Some(deadman) = deadman_task {
        let _ = tokio::time::timeout(Duration::from_secs(5), deadman).await;
    }
    info!("shutdown complete");
    Ok(())
}

/// Bridge the engine's synchronous exec channel to the async WS writer.
///
/// The engine hands [`UnsignedPost`]s to the crossbeam `Receiver` off the engine
/// thread; a small std thread moves them onto a tokio channel. The writer loop
/// **signs and enqueues** each post in receive order (cancels before places, per
/// the builder) and pushes only the reply wait into a [`FuturesUnordered`], so
/// the loop keeps taking posts while earlier replies are in flight: a post
/// arriving during a slow reply no longer waits a round trip, and a lost reply
/// cannot stall later posts or the kill switch (SPEC-0002 H-1, SPEC-0010 §12).
fn spawn_exec_writer(
    posts: crossbeam_channel::Receiver<UnsignedPost>,
    exchange: Arc<dyn ExchangeApi>,
    info: Arc<dyn InfoApi>,
    address: Option<String>,
    handles: InputHandles,
) -> tokio::task::JoinHandle<()> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<UnsignedPost>();
    let _ = std::thread::Builder::new()
        .name("mev-exec-bridge".to_string())
        .spawn(move || {
            while let Ok(post) = posts.recv() {
                if tx.send(post).is_err() {
                    break;
                }
            }
        });

    tokio::spawn(async move {
        let mut inflight = FuturesUnordered::new();
        loop {
            tokio::select! {
                maybe = rx.recv() => {
                    let Some(post) = maybe else { break };
                    let queued = Instant::now();
                    // Sign and enqueue now, in order; only the reply is awaited
                    // later. No lock is held across this await.
                    match exchange.enqueue(&post.action).await {
                        Ok(handle) => {
                            metrics::histogram!(mev_metrics::names::EXEC_QUEUE_SECONDS)
                                .record(queued.elapsed().as_secs_f64());
                            inflight.push(finish_post(
                                handle,
                                info.clone(),
                                address.clone(),
                                handles.clone(),
                                post,
                            ));
                        }
                        Err(err) => {
                            // A pre-send failure: the orders are terminal.
                            let result = mev_engine::exec::post_result_from_error(&err);
                            let _ = handles.send_account(AccountUpdate::PostAck {
                                stamp: engine::now_stamp(),
                                req_id: post.req_id,
                                result,
                            });
                        }
                    }
                }
                Some(_) = inflight.next(), if !inflight.is_empty() => {}
            }
        }
        // Drain any replies still in flight before the task exits.
        while inflight.next().await.is_some() {}
    })
}

/// Await one enqueued post's reply and report it to the engine.
///
/// A successful reply becomes a [`PostResult::Statuses`]. A definitive failure
/// becomes [`PostResult::Rejected`] (terminal); a sent request with no reliable
/// answer becomes [`PostResult::Error`] (`Unknown`), and its `orderStatus`
/// reconciliation is spawned so it cannot delay the writer (SPEC-0002 H-2,
/// SPEC-0010 §10/§16). Never resends.
async fn finish_post(
    handle: mev_hl_client::ReplyHandle,
    info: Arc<dyn InfoApi>,
    address: Option<String>,
    handles: InputHandles,
    post: UnsignedPost,
) {
    let req_id = post.req_id;
    let (result, unknown) = match handle.wait().await {
        Ok(response) => {
            let result = match response.order_response() {
                Ok(orders) => PostResult::Statuses(
                    orders
                        .statuses
                        .iter()
                        .zip(orders.oids.iter())
                        .map(|(status, oid)| OrderAck {
                            status: map_venue_status(status),
                            oid: *oid,
                        })
                        .collect(),
                ),
                // Non-order actions (cancels) carry no per-order statuses; their
                // outcome arrives on the account stream.
                Err(_) => PostResult::Statuses(SmallVec::new()),
            };
            (result, false)
        }
        Err(err) => {
            let result = mev_engine::exec::post_result_from_error(&err);
            let unknown = matches!(result, PostResult::Error(_));
            (result, unknown)
        }
    };
    let update = AccountUpdate::PostAck {
        stamp: engine::now_stamp(),
        req_id,
        result,
    };
    if !handles.send_account(update) {
        return;
    }
    // A lost reply is reconciled by cloid off the writer's critical path: the
    // dispatcher has already marked the orders `Unknown` (the PostAck is ahead
    // of these on the same FIFO account channel).
    if unknown
        && let Some(address) = address
        && !post.cloids.is_empty()
    {
        tokio::spawn(recover_unknown(
            info,
            address,
            handles,
            post.cloids,
            RecoveryPolicy::default(),
        ));
    }
}

/// Bounded `orderStatus` retry policy for `Unknown` orders (SPEC-0002 H-2).
#[derive(Debug, Clone, Copy)]
struct RecoveryPolicy {
    /// Number of `orderStatus` queries per cloid before giving up.
    attempts: usize,
    /// First backoff delay; doubles per attempt, capped.
    base_delay: Duration,
}

impl Default for RecoveryPolicy {
    fn default() -> Self {
        Self {
            attempts: 5,
            base_delay: Duration::from_millis(250),
        }
    }
}

/// Resolve `Unknown` orders by `cloid` through `orderStatus`, with capped
/// backoff.
///
/// A found order is applied only while the engine still holds it `Unknown`
/// (through [`AccountUpdate::ResolveUnknown`]); after the retry bound an order
/// the venue never saw is resolved as `Rejected`
/// ([`AccountUpdate::UnknownExpired`]). Never resends an order.
async fn recover_unknown(
    info: Arc<dyn InfoApi>,
    address: String,
    handles: InputHandles,
    cloids: SmallVec<[Cloid; 8]>,
    policy: RecoveryPolicy,
) {
    for cloid in cloids {
        let mut resolved = false;
        for attempt in 0..policy.attempts {
            if let Ok(status) = info.order_status_by_cloid(&address, &cloid.to_hex()).await
                && status.is_found()
            {
                resolved = handles.send_account(AccountUpdate::ResolveUnknown {
                    stamp: engine::now_stamp(),
                    cloid,
                    status,
                });
                break;
            }
            if attempt + 1 < policy.attempts {
                let shift = attempt.min(3) as u32;
                tokio::time::sleep(policy.base_delay * (1u32 << shift)).await;
            }
        }
        if !resolved
            && !handles.send_account(AccountUpdate::UnknownExpired {
                stamp: engine::now_stamp(),
                cloid,
            })
        {
            return;
        }
    }
}

/// Map a wire per-order status to the engine's typed status.
fn map_venue_status(status: &OrderStatus) -> VenueOrderStatus {
    match status {
        OrderStatus::Resting => VenueOrderStatus::Resting,
        OrderStatus::Filled => VenueOrderStatus::Filled,
        OrderStatus::Rejected(_) => VenueOrderStatus::Rejected,
        OrderStatus::Other(_) => VenueOrderStatus::Other,
    }
}

/// Merge the watchlist feeds with any strategy-required feeds, de-duplicated.
fn build_subscriptions(
    watchlist: &[String],
    plan: Option<&engine::EngineBuild>,
) -> (Vec<Subscription>, BTreeSet<String>, BTreeSet<String>) {
    let mut book_coins = BTreeSet::new();
    let mut ctx_coins = BTreeSet::new();
    let mut subs = Vec::new();

    for coin in watchlist {
        book_coins.insert(coin.clone());
        ctx_coins.insert(coin.clone());
        subs.push(Subscription::L2Book { coin: coin.clone() });
        subs.push(Subscription::ActiveAssetCtx { coin: coin.clone() });
        subs.push(Subscription::Trades { coin: coin.clone() });
    }
    if let Some(plan) = plan {
        for sub in &plan.subscriptions {
            match sub {
                Subscription::L2Book { coin } => {
                    book_coins.insert(coin.clone());
                }
                Subscription::ActiveAssetCtx { coin } => {
                    ctx_coins.insert(coin.clone());
                }
                _ => {}
            }
            subs.push(sub.clone());
        }
    }

    let mut seen = BTreeSet::new();
    subs.retain(|sub| {
        let key = serde_json::to_string(sub).unwrap_or_default();
        seen.insert(key)
    });
    (subs, book_coins, ctx_coins)
}

/// Build the live write client when running `live` with an agent key.
///
/// Returns `None` in `observe`/`simulate`, so no socket is opened and no key is
/// needed. The returned client shares the SQLite nonce high-water mark.
fn live_exchange(config: &Config) -> Result<Option<Arc<dyn ExchangeApi>>> {
    if config.mode != Mode::Live {
        return Ok(None);
    }
    let Some(key) = config.agent_key() else {
        return Ok(None);
    };
    let signer = AgentSigner::from_hex(key, config.network == Network::Mainnet)?;
    let db = Arc::new(Mutex::new(Db::open(&config.db_path)?));
    let exchange = WsExchange::new(config.network, config.mode, Some(signer))?.with_nonce_db(db)?;
    Ok(Some(Arc::new(exchange)))
}

/// Keep `scheduleCancel` armed while (and only while) orders rest, and fail
/// closed on any arm/refresh error (SPEC-0002 H-4).
///
/// Each cycle the task reads the engine's resting-order count (an atomic the
/// dispatcher refreshes per iteration; reading it never blocks the order path).
/// The switch (a) arms when the first order rests, (b) refreshes at half the
/// TTL, and (c) disarms when the last order leaves — so an idle bot does not
/// spend address rate-limit budget. If arming/refreshing fails while orders
/// rest, a fail-closed `Control::KillSwitch` is sent through the engine's
/// account channel (halting dispatch and cancelling working orders). The task
/// also polls `userRateLimit` every 60 s and exposes the remaining address
/// budget as a metric.
async fn deadman(
    exchange: Arc<dyn ExchangeApi>,
    info: Arc<dyn InfoApi>,
    address: Option<String>,
    resting: Arc<AtomicUsize>,
    handles: InputHandles,
    ttl_ms: u64,
) {
    let mut switch = DeadMansSwitch::new(ttl_ms);
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.tick().await;
    let mut rate = tokio::time::interval(Duration::from_secs(60));

    loop {
        tokio::select! {
            _ = shutdown_signal() => break,
            _ = rate.tick() => {
                if let Some(address) = &address {
                    match info.user_rate_limit(address).await {
                        Ok(budget) => {
                            metrics::gauge!(
                                mev_metrics::names::RATE_BUDGET_REMAINING,
                                "kind" => "address",
                            )
                            .set(budget.remaining() as f64);
                        }
                        Err(err) => {
                            tracing::debug!(error = %err, "userRateLimit poll failed");
                        }
                    }
                }
            }
            _ = tick.tick() => {
                // The engine refreshes this count once per iteration; reading
                // it never blocks the order path.
                let resting_orders = resting.load(Ordering::Relaxed);
                if let Some(action) = switch.update(now_ms(), resting_orders) {
                    let arming = resting_orders > 0;
                    match exchange.submit(&action).await {
                        Ok(_) => {
                            if arming {
                                metrics::counter!(mev_metrics::names::DEADMAN_REFRESHES).increment(1);
                            } else {
                                info!("dead-man switch disarmed (no resting orders)");
                            }
                        }
                        Err(err) if arming => {
                            error!(error = %err, "dead-man arm/refresh failed; halting trading");
                            metrics::counter!(mev_metrics::names::DEADMAN_FAILURES).increment(1);
                            // Fail closed: halt the v2 dispatcher through the
                            // lossless account channel (it also cancels working
                            // orders via Control::KillSwitch).
                            if !handles.send_account(AccountUpdate::Control(Control::KillSwitch)) {
                                break;
                            }
                        }
                        Err(err) => {
                            // Disarm failed; the venue expires the schedule anyway.
                            tracing::debug!(error = %err, "dead-man disarm failed");
                        }
                    }
                }
                metrics::gauge!(mev_metrics::names::DEADMAN_ARMED)
                    .set(if switch.is_armed() { 1.0 } else { 0.0 });
            }
        }
    }

    if let Some(action) = switch.disarm() {
        match exchange.submit(&action).await {
            Ok(_) => info!("dead-man switch disarmed"),
            Err(err) => tracing::debug!(error = %err, "dead-man disarm failed"),
        }
    }
    metrics::gauge!(mev_metrics::names::DEADMAN_ARMED).set(0.0);
}

/// A raw frame handed off the ingest task for health/recording.
struct SideFrame {
    /// Exact text received.
    text: String,
    /// Local receive time in wall-clock ms.
    ts_ms: u64,
}

/// Bounded queue from the ingest task to the health/SQLite sidecar.
const SIDE_CHANNEL_CAP: usize = 16_384;

/// Ingest market data into the v2 engine.
///
/// The typed decode and the hand-off to the engine happen **first**, so the
/// engine is never queued behind the slower legacy decode (ADR-0001 put
/// `ws::decode` at 130–260 µs). The raw frame is then offered to the health /
/// SQLite sidecar on a bounded, non-blocking channel (drop on full; the engine
/// never waits on recording).
async fn ingest(
    handles: InputHandles,
    subscriptions: Vec<Subscription>,
    registry: CoinRegistry,
    network: Network,
    side_tx: tokio::sync::mpsc::Sender<SideFrame>,
    market_drops: Arc<AtomicU64>,
) {
    let ingester = Ingest::new(ConnId(0), registry);
    let planned: Vec<String> = subscriptions
        .iter()
        .map(|sub| serde_json::to_string(sub).unwrap_or_default())
        .collect();
    let mut backoff = Duration::from_secs(1);

    loop {
        match RawWsConn::connect(Box::new(HlProtocol::new(network)), planned.clone()).await {
            Ok(mut conn) => {
                info!(subscriptions = planned.len(), "market stream connected");
                // A fresh connection closes any prior feed gap so the engine can
                // clear stale coins once data flows again (SPEC-0010 §16).
                let _ = handles.send_market(MarketUpdate::Gap {
                    conn: ConnId(0),
                    stamp: Stamp::default(),
                    open: false,
                });
                loop {
                    match conn.next().await {
                        Ok(RawEvent::Text {
                            t_ns,
                            mono_ns,
                            text,
                        }) => {
                            let stamp = Stamp {
                                // `RawEvent::Text::t_ns` is wall-clock ms.
                                t_recv_ns: t_ns.saturating_mul(1_000_000),
                                mono_ns,
                                ts_exch_ms: 0,
                            };
                            match ingester.decode(&text, stamp) {
                                Ok(Some(update)) => {
                                    if handles.send_market(update) == MarketSend::Dropped {
                                        // Folded into `hl_engine_market_drops_total`
                                        // by the dispatcher's next flush.
                                        market_drops.fetch_add(1, Ordering::Relaxed);
                                    }
                                }
                                Ok(None) => {}
                                Err(err) => {
                                    tracing::debug!(error = %err, "undecodable market frame");
                                }
                            }
                            // Off the engine path: decode again for the health
                            // snapshot and the replay log on a sidecar task.
                            let _ = side_tx.try_send(SideFrame {
                                text,
                                ts_ms: SystemClock.now_ms(),
                            });
                        }
                        Ok(RawEvent::Gap { reason, detail }) => {
                            tracing::warn!(reason, detail, "market feed gap");
                            if reason == "shutdown" {
                                return;
                            }
                            // Mark coins stale until fresh data arrives.
                            let _ = handles.send_market(MarketUpdate::Gap {
                                conn: ConnId(0),
                                stamp: Stamp::default(),
                                open: true,
                            });
                        }
                        Ok(_) => {}
                        Err(err) => {
                            tracing::warn!(error = %err, "market stream ended");
                            let _ = handles.send_market(MarketUpdate::Gap {
                                conn: ConnId(0),
                                stamp: Stamp::default(),
                                open: true,
                            });
                            return;
                        }
                    }
                }
            }
            Err(err) => {
                tracing::warn!(error = %err, "market stream connect failed");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }
}

/// Apply health and SQLite recording to raw frames, off the ingest path.
///
/// Runs the legacy `ws::decode` here so it never delays the engine hand-off
/// (ADR-0001: 130–260 µs per frame). A dropped frame only costs health/recording
/// fidelity; it is folded into a gap by the next update.
async fn recording_sidecar(
    mut rx: tokio::sync::mpsc::Receiver<SideFrame>,
    health_state: Arc<RwLock<MarketState>>,
    recorder: Option<engine::Recorder>,
) {
    while let Some(frame) = rx.recv().await {
        if let Ok(Some(event)) = mev_hl_client::ws::decode(&frame.text) {
            if let Some(recorder) = &recorder {
                recorder.record(&mev_strategy::Event::Market(event.clone()), frame.ts_ms);
            }
            if let Ok(mut guard) = health_state.write() {
                guard.apply(&event);
            }
        }
    }
}

/// Consume the H-3 account channels into lossless [`AccountUpdate`]s.
///
/// Runs on its own connection, separate from market data, and is never lossy:
/// the engine resolves live cancels, fills, and order state from this stream
/// (SPEC-0010 §15, SPEC-0002 H-3). On a reconnect the venue's `userFills`
/// snapshot resyncs state; the REST reconciler is the 30 s backstop.
async fn account_stream(
    handles: InputHandles,
    subscriptions: Vec<Subscription>,
    registry: CoinRegistry,
    network: Network,
) {
    let ingester = Ingest::new(ConnId(1), registry);
    let planned: Vec<String> = subscriptions
        .iter()
        .map(|sub| serde_json::to_string(sub).unwrap_or_default())
        .collect();
    let mut backoff = Duration::from_secs(1);

    loop {
        match RawWsConn::connect(Box::new(HlProtocol::new(network)), planned.clone()).await {
            Ok(mut conn) => {
                info!("account stream connected");
                loop {
                    match conn.next().await {
                        Ok(RawEvent::Text {
                            t_ns,
                            mono_ns,
                            text,
                        }) => {
                            let stamp = Stamp {
                                t_recv_ns: t_ns.saturating_mul(1_000_000),
                                mono_ns,
                                ts_exch_ms: 0,
                            };
                            match ingester.decode_account(&text, stamp) {
                                Ok(updates) => {
                                    for update in updates {
                                        if !handles.send_account(update) {
                                            return;
                                        }
                                    }
                                }
                                Err(err) => {
                                    tracing::debug!(error = %err, "undecodable account frame");
                                }
                            }
                        }
                        Ok(RawEvent::Gap { reason, detail }) => {
                            tracing::warn!(reason, detail, "account feed gap");
                            if reason == "shutdown" {
                                return;
                            }
                        }
                        Ok(_) => {}
                        Err(err) => {
                            tracing::warn!(error = %err, "account stream ended");
                            return;
                        }
                    }
                }
            }
            Err(err) => {
                tracing::warn!(error = %err, "account stream connect failed");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }
}

/// Poll the kill-switch triggers and drive [`Control`] into the engine.
///
/// Triggers (SPEC-0004 K-3): `SIGUSR1`, the kill flag file, and `hl panic`
/// (which writes that file). `SIGUSR2` sends `Control::Resume`; clearing also
/// needs the file removed (the two-key rule), so a poll re-trips otherwise.
#[cfg(unix)]
async fn control(handles: InputHandles, kill_file: PathBuf) {
    use tokio::signal::unix::{SignalKind, signal};
    let mut sigusr1 = match signal(SignalKind::user_defined1()) {
        Ok(sig) => sig,
        Err(err) => {
            error!(error = %err, "failed to register SIGUSR1");
            return;
        }
    };
    let mut sigusr2 = match signal(SignalKind::user_defined2()) {
        Ok(sig) => sig,
        Err(err) => {
            error!(error = %err, "failed to register SIGUSR2");
            return;
        }
    };
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    let mut killed = false;
    loop {
        tokio::select! {
            _ = shutdown_signal() => break,
            _ = sigusr1.recv() => {
                if !killed {
                    if !handles.send_account(AccountUpdate::Control(Control::KillSwitch)) {
                        break;
                    }
                    killed = true;
                }
            }
            _ = sigusr2.recv() => {
                if killed {
                    if !handles.send_account(AccountUpdate::Control(Control::Resume)) {
                        break;
                    }
                    killed = false;
                }
            }
            _ = tick.tick() => {
                if !killed && mev_risk::kill::check_flag_file(&kill_file) {
                    if !handles.send_account(AccountUpdate::Control(Control::KillSwitch)) {
                        break;
                    }
                    killed = true;
                }
            }
        }
    }
}

/// Poll the kill-switch flag file on platforms without `SIGUSR1`/`SIGUSR2`.
#[cfg(not(unix))]
async fn control(handles: InputHandles, kill_file: PathBuf) {
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    let mut killed = false;
    loop {
        tokio::select! {
            _ = shutdown_signal() => break,
            _ = tick.tick() => {
                if !killed && mev_risk::kill::check_flag_file(&kill_file) {
                    if !handles.send_account(AccountUpdate::Control(Control::KillSwitch)) {
                        break;
                    }
                    killed = true;
                }
            }
        }
    }
}

/// Publish feed staleness and readiness while the process runs.
async fn monitor(state: Arc<RwLock<MarketState>>, health: Health) {
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    loop {
        interval.tick().await;
        let now = Instant::now();
        let (ready, ages) = {
            let guard = state.read().expect("market state lock poisoned");
            (guard.is_ready(now), guard.ages(now))
        };
        health.set_ready(ready);
        for age in ages {
            metrics::gauge!(
                mev_metrics::names::FEED_STALENESS_SECONDS,
                "feed" => age.feed,
                "coin" => age.coin,
            )
            .set(age.age_secs.min(1e9));
        }
    }
}

async fn markets(
    network: Option<NetworkArg>,
    query: Option<String>,
    perp: bool,
    spot: bool,
    dex: Option<String>,
) -> Result<()> {
    let network = resolve_network(network)?;
    let info = HttpInfo::new(network);

    let filter = query.unwrap_or_default().to_lowercase();
    let matches = |name: &str| filter.is_empty() || name.to_lowercase().contains(&filter);
    let show_perp = perp || !spot;
    let show_spot = spot || !perp;

    match dex {
        Some(dex) => {
            let meta = info.meta_for(&dex).await?;
            println!("{} perps ({}):", dex, meta.universe.len());
            for asset in &meta.universe {
                if matches(&asset.name) {
                    println!(
                        "  {:<20} szDecimals={} maxLeverage={}",
                        asset.name, asset.sz_decimals, asset.max_leverage
                    );
                }
            }
        }
        None => {
            if show_perp {
                let meta = info.meta().await?;
                println!("perps ({}):", meta.universe.len());
                for asset in &meta.universe {
                    if matches(&asset.name) {
                        println!(
                            "  {:<12} szDecimals={} maxLeverage={}",
                            asset.name, asset.sz_decimals, asset.max_leverage
                        );
                    }
                }

                let dexs = info.perp_dexs().await.unwrap_or_default();
                let names: Vec<&str> = dexs.iter().map(|d| d.name.as_str()).collect();
                if !names.is_empty() {
                    println!("hip-3 dexes: {} (use --dex <name>)", names.join(", "));
                }
            }

            if show_spot {
                let spot = info.spot_meta().await?;
                println!("spot ({}):", spot.universe.len());
                for pair in &spot.universe {
                    if matches(&pair.name) {
                        println!("  {:<12} @{}", pair.name, pair.index);
                    }
                }
            }
        }
    }
    Ok(())
}

/// Resolve and validate the persisted watchlist against live metadata.
async fn select(
    network: Option<NetworkArg>,
    coins: Vec<String>,
    add: Vec<String>,
    remove: Vec<String>,
) -> Result<()> {
    let config = Config::load(ConfigOverrides {
        network: network.map(Into::into),
        ..Default::default()
    })?;

    let mut watchlist = mev_core::watchlist::load(&config.watchlist_path)?;
    if watchlist.is_empty() {
        watchlist = config.watchlist.clone();
    }

    if !coins.is_empty() {
        watchlist = coins;
    }
    for coin in add {
        if !watchlist.iter().any(|c| c.eq_ignore_ascii_case(&coin)) {
            watchlist.push(coin);
        }
    }
    if !remove.is_empty() {
        watchlist.retain(|c| !remove.iter().any(|r| r.eq_ignore_ascii_case(c)));
    }
    if watchlist.is_empty() {
        anyhow::bail!("refusing to persist an empty watchlist");
    }

    let selector = selector_for(config.network, &watchlist).await?;
    let resolved = selector.resolve_all(&watchlist)?;
    let canonical: Vec<String> = resolved.iter().map(|m| m.coin.clone()).collect();

    mev_core::watchlist::save(&config.watchlist_path, &canonical)?;
    println!(
        "watchlist ({}): {}",
        canonical.len(),
        config.watchlist_path.display()
    );
    for market in &resolved {
        println!("  {}", format_market(market));
    }
    Ok(())
}

/// Build and sign a single order, printing the wire form and envelope. Never
/// submits, so it is safe to run in any mode.
async fn order(network: Option<NetworkArg>, args: OrderArgs) -> Result<()> {
    use rust_decimal::Decimal;
    use std::str::FromStr;

    let config = Config::load(ConfigOverrides {
        network: network.map(Into::into),
        ..Default::default()
    })?;
    let key = config
        .agent_key()
        .ok_or_else(|| anyhow::anyhow!("set HL_AGENT_PRIVATE_KEY to sign an order"))?;
    let signer = AgentSigner::from_hex(key, config.network == Network::Mainnet)?;

    let selector = selector_for(config.network, std::slice::from_ref(&args.coin)).await?;
    let market = selector.resolve(&args.coin)?;

    let params = OrderParams {
        is_buy: matches!(args.side, Side::Buy),
        size: Decimal::from_str(&args.sz)
            .map_err(|e| anyhow::anyhow!("invalid --sz `{}`: {e}", args.sz))?,
        limit_px: Decimal::from_str(&args.px)
            .map_err(|e| anyhow::anyhow!("invalid --px `{}`: {e}", args.px))?,
        tif: args.tif.into(),
        reduce_only: args.reduce_only,
        cloid: args.cloid,
    };
    let wire = build_order_wire(&market, &params)?;
    let action = Action::order(vec![wire.clone()]);
    let nonce = now_ms();
    let request = build_request(&action, &signer, nonce, None, None)?;

    println!("mode:      dry-run (nothing submitted)");
    println!("network:   {:?}", config.network);
    println!("agent:     {}", signer.address());
    println!(
        "market:    {} (asset_id={}, szDecimals={})",
        market.coin,
        market.asset_id(),
        market.sz_decimals
    );
    println!("order:     {}", serde_json::to_string(&wire)?);
    println!("nonce:     {nonce}");
    println!(
        "signature: r={} s={} v={}",
        request.signature.r_hex(),
        request.signature.s_hex(),
        request.signature.v
    );
    println!("envelope:  {}", serde_json::to_string(&request)?);
    Ok(())
}

/// Print account state and open orders for an address.
async fn account(network: Option<NetworkArg>, address: String) -> Result<()> {
    let network = resolve_network(network)?;
    let info = HttpInfo::new(network);

    let state = info.clearinghouse_state(&address).await?;
    println!(
        "account {address}  value={}  withdrawable={}  marginUsed={}",
        state.margin_summary.account_value,
        state.withdrawable,
        state.margin_summary.total_margin_used
    );
    if state.asset_positions.is_empty() {
        println!("positions: none");
    } else {
        println!("positions:");
        for entry in &state.asset_positions {
            let position = &entry.position;
            println!(
                "  {:<12} szi={:<14} entry={:<12} value={:<14} uPnl={}",
                position.coin,
                position.szi,
                position.entry_px.map(|p| p.to_string()).unwrap_or_default(),
                position.position_value,
                position.unrealized_pnl,
            );
        }
    }

    let orders = info.open_orders(&address).await?;
    println!("open orders: {}", orders.len());
    for order in &orders {
        println!(
            "  {:<12} {} {:<12} @ {:<12} oid={}",
            order.coin,
            if order.is_buy() { "buy " } else { "sell" },
            order.sz,
            order.limit_px,
            order.oid,
        );
    }
    Ok(())
}

/// Deterministically replay a recorded session and print its fingerprint.
async fn replay(
    network: Option<NetworkArg>,
    session: Option<i64>,
    db: Option<PathBuf>,
) -> Result<()> {
    let network = resolve_network(network)?;
    let overrides = ConfigOverrides {
        network: Some(network),
        db_path: db,
        ..Default::default()
    };
    let config = Config::load(overrides)?;
    let selector = selector_for(config.network, &config.watchlist).await?;

    let outcome = engine::replay(&config, &selector, session, &config.db_path).await?;
    println!(
        "replay: events={} intents={} fingerprint=0x{:016x}",
        outcome.events, outcome.intents, outcome.fingerprint
    );
    Ok(())
}

/// Build a selector, loading HIP-3 metadata only when the list needs it.
async fn selector_for(network: Network, coins: &[String]) -> Result<MarketSelector> {
    let info = HttpInfo::new(network);
    let include_hip3 = coins.iter().any(|coin| coin.contains(':'));
    Ok(MarketSelector::new(
        AssetMap::load(&info, include_hip3).await?,
    ))
}

fn format_market(market: &Market) -> String {
    let kind = match (market.kind, market.dex.as_deref()) {
        (MarketKind::Perp, Some(dex)) => format!("perp {dex}"),
        (MarketKind::Perp, None) => "perp".to_string(),
        (MarketKind::Spot, _) => "spot".to_string(),
    };
    format!(
        "{:<16} {:<10} index={:<4} szDecimals={}",
        market.coin, kind, market.index, market.sz_decimals
    )
}

async fn dexs(network: Option<NetworkArg>) -> Result<()> {
    let network = resolve_network(network)?;
    let info = HttpInfo::new(network);
    let dexs = info.perp_dexs().await?;
    for dex in &dexs {
        match &dex.full_name {
            Some(full) => println!("{:<8} {}", dex.name, full),
            None => println!("{}", dex.name),
        }
    }
    Ok(())
}

async fn book(network: Option<NetworkArg>, coin: String, levels: usize) -> Result<()> {
    let network = resolve_network(network)?;
    let info = HttpInfo::new(network);
    let book = info.l2_book(&coin).await?;

    println!("{}  time={}  mid={:?}", book.coin, book.time, book.mid());
    println!("  bids:");
    for level in book.levels[0].iter().take(levels) {
        println!("    {:<16} {}", level.px, level.sz);
    }
    println!("  asks:");
    for level in book.levels[1].iter().take(levels) {
        println!("    {:<16} {}", level.px, level.sz);
    }
    Ok(())
}

async fn watch(network: Option<NetworkArg>, coins: Vec<String>) -> Result<()> {
    let network = resolve_network(network)?;
    let coins = if coins.is_empty() {
        Config::load(ConfigOverrides::default())?.watchlist
    } else {
        coins
    };
    if coins.is_empty() {
        anyhow::bail!("no coins to watch; pass coin names or configure a watchlist");
    }
    let coins: Vec<String> = selector_for(network, &coins)
        .await?
        .resolve_all(&coins)?
        .iter()
        .map(|market| market.coin.clone())
        .collect();

    let mut subs = vec![Subscription::AllMids];
    for coin in &coins {
        subs.push(Subscription::L2Book { coin: coin.clone() });
        subs.push(Subscription::Trades { coin: coin.clone() });
        subs.push(Subscription::ActiveAssetCtx { coin: coin.clone() });
    }

    let mut stream = WsMarketStream::connect(network, &subs).await?;
    println!(
        "watching {} on {:?} (ctrl-c to stop)",
        coins.join(", "),
        network
    );

    loop {
        tokio::select! {
            _ = shutdown_signal() => break,
            event = stream.next() => match event {
                Ok(event) => print_event(event),
                Err(err) => {
                    error!(error = %err, "stream error");
                    break;
                }
            },
        }
    }
    Ok(())
}

fn print_event(event: StreamEvent) {
    match event {
        StreamEvent::Mids(mids) => {
            let btc = mids.get("BTC").copied().unwrap_or_default();
            println!("mids      {} coins (BTC={})", mids.len(), btc);
        }
        StreamEvent::Book(book) => println!(
            "book      {:<12} bid={:?} ask={:?} mid={:?}",
            book.coin,
            book.best_bid().map(|l| l.px),
            book.best_ask().map(|l| l.px),
            book.mid(),
        ),
        StreamEvent::Bbo(bbo) => println!(
            "bbo       {:<12} bid={:?} ask={:?}",
            bbo.coin,
            bbo.bid().map(|l| l.px),
            bbo.ask().map(|l| l.px),
        ),
        StreamEvent::Trades(trades) => {
            if let Some(trade) = trades.last() {
                println!(
                    "trade     {:<12} {} {} @ {}",
                    trade.coin, trade.side, trade.sz, trade.px
                );
            }
        }
        StreamEvent::AssetCtx(update) => println!(
            "ctx       {:<12} mark={} oracle={} funding={}",
            update.coin, update.ctx.mark_px, update.ctx.oracle_px, update.ctx.funding,
        ),
        StreamEvent::OrderUpdates(orders) => {
            if let Some(order) = orders.first() {
                println!(
                    "order     {:<12} oid={} status={}",
                    order.order.coin, order.order.oid, order.status
                );
            }
        }
        StreamEvent::UserFills(fills) => {
            println!(
                "fills     {} (snapshot={})",
                fills.fills.len(),
                fills.is_snapshot
            );
        }
        StreamEvent::UserEvent(event) => {
            if let Some(fills) = &event.fills {
                println!("userEvent fills={}", fills.len());
            } else if event.funding.is_some() {
                println!("userEvent funding");
            } else if event.liquidation.is_some() {
                println!("userEvent liquidation");
            } else if event.non_user_cancel.is_some() {
                println!("userEvent nonUserCancel");
            }
        }
    }
}

fn show_config() -> Result<()> {
    let config = Config::load(ConfigOverrides::default())?;
    println!("{}", config.summary());
    Ok(())
}

async fn heartbeat() {
    let started = Instant::now();
    let mut interval = tokio::time::interval(Duration::from_secs(15));
    loop {
        interval.tick().await;
        let uptime = started.elapsed().as_secs();
        metrics::gauge!(mev_metrics::names::UPTIME_SECONDS).set(uptime as f64);
        metrics::counter!(mev_metrics::names::HEARTBEATS).increment(1);
        tracing::debug!(uptime_seconds = uptime, "heartbeat");
    }
}

#[derive(Clone)]
struct AppState {
    health: Health,
    metrics: PrometheusHandle,
}

pub(crate) async fn serve(health: Health, metrics: PrometheusHandle, port: u16) -> Result<()> {
    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/metrics", get(render_metrics))
        .with_state(AppState { health, metrics });

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(%addr, "http server listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn healthz() -> (StatusCode, &'static str) {
    (StatusCode::OK, "ok\n")
}

async fn readyz(State(state): State<AppState>) -> (StatusCode, &'static str) {
    if state.health.is_ready() {
        (StatusCode::OK, "ready\n")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "not ready\n")
    }
}

async fn render_metrics(State(state): State<AppState>) -> String {
    state.metrics.render()
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c().await.expect("failed to listen for ctrl-c");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to listen for SIGTERM")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_exchange_is_none_outside_live() {
        for mode in [Mode::Observe, Mode::Simulate] {
            let config = Config {
                mode,
                ..Config::default()
            };
            assert!(live_exchange(&config).unwrap().is_none(), "{mode:?}");
        }
    }

    /// A tiny policy so the recovery tests don't sleep for seconds.
    fn fast_policy() -> RecoveryPolicy {
        RecoveryPolicy {
            attempts: 2,
            base_delay: Duration::from_millis(1),
        }
    }

    fn cloid_with(byte: u8) -> Cloid {
        Cloid([byte; 16])
    }

    fn one(cloid: Cloid) -> SmallVec<[Cloid; 8]> {
        let mut out = SmallVec::new();
        out.push(cloid);
        out
    }

    #[tokio::test]
    async fn recovery_bounds_a_never_seen_order_to_expired() {
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/info"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"status":"unknownOid"}"#))
            .mount(&server)
            .await;

        let info: Arc<dyn InfoApi> = Arc::new(HttpInfo::with_base_url(server.uri()));
        let (handles, inputs) = inputs(MARKET_CHANNEL_CAP, ACCOUNT_CHANNEL_CAP);
        let cloid = cloid_with(1);
        recover_unknown(info, "0xabc".into(), handles, one(cloid), fast_policy()).await;

        match inputs.account.try_recv() {
            Ok(AccountUpdate::UnknownExpired { cloid: got, .. }) => assert_eq!(got, cloid),
            other => panic!("expected UnknownExpired, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn recovery_applies_a_found_order_status() {
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };

        let body = r#"{"status":"order","order":{"order":{"coin":"BTC","side":"B","limitPx":"100","sz":"1","oid":7,"timestamp":0,"origSz":"1","reduceOnly":false},"status":"open","statusTimestamp":0}}"#;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/info"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        let info: Arc<dyn InfoApi> = Arc::new(HttpInfo::with_base_url(server.uri()));
        let (handles, inputs) = inputs(MARKET_CHANNEL_CAP, ACCOUNT_CHANNEL_CAP);
        let cloid = cloid_with(2);
        recover_unknown(info, "0xabc".into(), handles, one(cloid), fast_policy()).await;

        match inputs.account.try_recv() {
            Ok(AccountUpdate::ResolveUnknown { cloid: got, .. }) => assert_eq!(got, cloid),
            other => panic!("expected ResolveUnknown, got {other:?}"),
        }
    }

    /// One scripted reply for [`MockExchange`].
    #[derive(Clone, Copy)]
    struct Outcome {
        delay: Duration,
        fail: bool,
    }

    /// An [`ExchangeApi`] whose `enqueue` records order and returns a scripted
    /// reply handle, so the writer's split enqueue/await can be tested.
    struct MockExchange {
        enqueued: Arc<Mutex<Vec<mev_hl_client::Action>>>,
        outcomes: Arc<Mutex<std::collections::VecDeque<Outcome>>>,
    }

    #[async_trait::async_trait]
    impl ExchangeApi for MockExchange {
        async fn submit(
            &self,
            action: &mev_hl_client::Action,
        ) -> mev_core::error::Result<mev_hl_client::ActionResponse> {
            self.enqueue(action).await?.wait().await
        }

        async fn enqueue(
            &self,
            action: &mev_hl_client::Action,
        ) -> mev_core::error::Result<mev_hl_client::ReplyHandle> {
            self.enqueued.lock().unwrap().push(action.clone());
            let outcome = self
                .outcomes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Outcome {
                    delay: Duration::ZERO,
                    fail: false,
                });
            Ok(mev_hl_client::ReplyHandle::new(async move {
                tokio::time::sleep(outcome.delay).await;
                if outcome.fail {
                    Err(mev_core::error::Error::UnknownOutcome("lost reply".into()))
                } else {
                    Ok(mev_hl_client::ActionResponse {
                        value: serde_json::json!({"data": {"statuses": ["resting"]}}),
                    })
                }
            }))
        }
    }

    fn mock_exchange(outcomes: Vec<Outcome>) -> Arc<MockExchange> {
        Arc::new(MockExchange {
            enqueued: Arc::new(Mutex::new(Vec::new())),
            outcomes: Arc::new(Mutex::new(outcomes.into_iter().collect())),
        })
    }

    fn test_post(req_id: u64, action: mev_hl_client::Action) -> UnsignedPost {
        UnsignedPost {
            req_id,
            action,
            cloids: SmallVec::new(),
        }
    }

    /// A never-dialing info client; recovery calls just fail and retry.
    fn dead_info() -> Arc<dyn InfoApi> {
        Arc::new(HttpInfo::with_base_url("http://127.0.0.1:1"))
    }

    #[tokio::test]
    async fn a_post_during_a_slow_reply_is_enqueued_immediately() {
        use mev_hl_client::{Action as VenueAction, Grouping};

        let exchange = mock_exchange(vec![
            Outcome {
                delay: Duration::from_secs(5),
                fail: false,
            },
            Outcome {
                delay: Duration::ZERO,
                fail: false,
            },
        ]);
        let (handles, _inputs) = inputs(MARKET_CHANNEL_CAP, ACCOUNT_CHANNEL_CAP);
        let (tx, rx) = crossbeam_channel::unbounded();
        let writer = spawn_exec_writer(rx, exchange.clone(), dead_info(), None, handles);

        tx.send(test_post(1, VenueAction::CancelByCloid { cancels: vec![] }))
            .unwrap();
        let started = Instant::now();
        tx.send(test_post(
            2,
            VenueAction::Order {
                orders: vec![],
                grouping: Grouping::Na,
            },
        ))
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        while exchange.enqueued.lock().unwrap().len() < 2 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let queue_to_socket = started.elapsed();
        assert_eq!(
            exchange.enqueued.lock().unwrap().len(),
            2,
            "the second post must reach the socket while the first reply is pending"
        );
        assert!(
            queue_to_socket < Duration::from_secs(1),
            "queue-to-socket took {queue_to_socket:?}"
        );

        drop(tx);
        writer.abort();
    }

    #[tokio::test]
    async fn a_cancel_is_enqueued_before_a_place_in_a_batch() {
        use mev_hl_client::{Action as VenueAction, Grouping};

        let exchange = mock_exchange(vec![]);
        let (handles, _inputs) = inputs(MARKET_CHANNEL_CAP, ACCOUNT_CHANNEL_CAP);
        let (tx, rx) = crossbeam_channel::unbounded();
        let writer = spawn_exec_writer(rx, exchange.clone(), dead_info(), None, handles);

        tx.send(test_post(1, VenueAction::CancelByCloid { cancels: vec![] }))
            .unwrap();
        tx.send(test_post(
            2,
            VenueAction::Order {
                orders: vec![],
                grouping: Grouping::Na,
            },
        ))
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        while exchange.enqueued.lock().unwrap().len() < 2 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let enqueued = exchange.enqueued.lock().unwrap();
        assert_eq!(enqueued.len(), 2);
        assert!(
            matches!(enqueued[0], VenueAction::CancelByCloid { .. }),
            "the cancel goes first"
        );
        assert!(
            matches!(enqueued[1], VenueAction::Order { .. }),
            "the place goes second"
        );

        drop(tx);
        writer.abort();
    }

    #[tokio::test]
    async fn a_lost_reply_and_its_recovery_do_not_delay_the_next_post() {
        use mev_hl_client::{Action as VenueAction, Grouping};

        let exchange = mock_exchange(vec![
            Outcome {
                delay: Duration::from_millis(200),
                fail: true,
            },
            Outcome {
                delay: Duration::ZERO,
                fail: false,
            },
        ]);
        let (handles, _inputs) = inputs(MARKET_CHANNEL_CAP, ACCOUNT_CHANNEL_CAP);
        let (tx, rx) = crossbeam_channel::unbounded();
        let writer = spawn_exec_writer(
            rx,
            exchange.clone(),
            dead_info(),
            Some("0xabc".into()),
            handles,
        );

        let mut first = test_post(1, VenueAction::CancelByCloid { cancels: vec![] });
        first.cloids.push(cloid_with(7));
        tx.send(first).unwrap();
        tx.send(test_post(
            2,
            VenueAction::Order {
                orders: vec![],
                grouping: Grouping::Na,
            },
        ))
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        while exchange.enqueued.lock().unwrap().len() < 2 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert_eq!(
            exchange.enqueued.lock().unwrap().len(),
            2,
            "recovery must not delay the next post"
        );

        drop(tx);
        writer.abort();
    }
}
