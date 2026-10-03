//! `hl` — orchestration binary for the Hyperliquid-first trading system.
//!
//! M0.x: platform (config, observability, health, shutdown).
//! M1.1:  REST `/info` client and the `markets`/`book` commands.

use std::{
    collections::{BTreeMap, BTreeSet},
    net::SocketAddr,
    path::PathBuf,
    str::FromStr,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

mod cmd;
mod db_lock;
mod engine;
mod live;
mod probe_roundtrip;
mod record;
mod replay;

use anyhow::{Context as _, Result};
use axum::{Router, extract::State, http::StatusCode, routing::get};
use clap::{Parser, Subcommand, ValueEnum};
use hl_arb_client::{
    Action, AgentSigner, AssetMap, DeadMansSwitch, ExchangeApi, HlProtocol, HttpInfo, InfoApi,
    Market, MarketKind, MarketSelector, MarketState, MarketStream, OrderParams, RawEvent,
    RawWsConn, StreamEvent, Subscription, Tif, Tolerance, WsExchange, WsMarketStream,
    build_order_wire, build_request, now_ms,
};
use hl_arb_core::{
    clock::{Clock, SystemClock},
    config::{Config, ConfigOverrides, Mode, Network},
    db::{Db, writer::DbWriter},
};
use hl_arb_engine::{
    AccountUpdate, CoinRegistry, Control, Stamp, StrategyDispatcher,
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
use hl_arb_metrics::{health::Health, prometheus::PrometheusHandle};
use hl_arb_strategy::{AccountView, FeeRates, Instrument};
use rust_decimal::Decimal;
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
    /// Deterministically replay a recorded session (SPEC-0003 §10) or recorder
    /// segments through the v2 engine (SPEC-0010 §14, E-7).
    Replay(ReplayArgs),
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
    /// Nonce maintenance (SPEC-0002 H-6).
    Nonce {
        #[command(subcommand)]
        cmd: NonceCmd,
    },
}

/// Subcommands of `hl nonce`.
#[derive(Subcommand)]
enum NonceCmd {
    /// Rewrite a corrupt persisted nonce so the bot can send again.
    ///
    /// Only valid when the stored value is beyond the venue's future window
    /// (the fail-closed startup error). Does not load or need any key.
    Reset,
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
        /// Do not fail when finished segments have no manifest line (orphans).
        #[arg(long)]
        allow_orphans: bool,
        /// Fail on `.crashed`/unfinalized segments with no manifest line too
        /// (they are warnings otherwise).
        #[arg(long)]
        strict: bool,
    },
    /// Append the missing manifest line for orphan finished segments
    /// (SPEC-0008 §17 #36). Maintenance tool; never run automatically.
    RepairManifest {
        /// UTC date whose orphans to repair, `YYYY-MM-DD`.
        #[arg(long)]
        date: String,
        /// Print the lines that would be appended without writing them.
        #[arg(long)]
        dry_run: bool,
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
    /// Testnet-only signed order round-trip (SPEC-0002 H-10).
    TestnetRoundtrip(probe_roundtrip::RoundtripArgs),
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

/// Arguments for `hl replay` (SPEC-0010 §14).
#[derive(clap::Args)]
struct ReplayArgs {
    /// Session id to replay (SQLite path; default: the most recent).
    #[arg(long)]
    session: Option<i64>,
    /// SQLite database path override.
    #[arg(long)]
    db: Option<PathBuf>,
    /// Replay recorder segments from this UTC date (`YYYY-MM-DD`); requires
    /// `--to`. Selects the segment driver instead of the SQLite session.
    #[arg(long)]
    from: Option<String>,
    /// Inclusive UTC end date for `--from` (`YYYY-MM-DD`).
    #[arg(long)]
    to: Option<String>,
    /// Recorder output root for `--from`/`--to`.
    #[arg(long, default_value = "data/rec")]
    rec_dir: PathBuf,
    /// Write the action journal here as JSONL.
    #[arg(long)]
    out: Option<PathBuf>,
    /// Strategy ids to run (comma-separated; default: config `enabled`).
    #[arg(long, value_delimiter = ',')]
    strategies: Vec<String>,
    /// Simulated one-way paper latency in milliseconds.
    #[arg(long, default_value_t = 20)]
    latency_ms: u64,
    /// Deterministic cloid prefix for replay.
    #[arg(long, default_value_t = 0)]
    cloid_prefix: u64,
    /// Starting paper account value (USD).
    #[arg(long, default_value = "100000")]
    account_value: String,
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
    hl_arb_metrics::logging::init();
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
        // `?err` prints the whole anyhow chain; `%err` showed only the top
        // context ("spawning segment writer"), hiding the cause (PERF-007).
        error!(error = ?err, "fatal");
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

async fn dispatch(command: Command, network: Option<NetworkArg>) -> Result<()> {
    match command {
        Command::Run { mode, coins, db } => cmd::run::run(mode, network, coins, db).await,
        Command::Config { cmd } => {
            match cmd {
                ConfigCmd::Show => cmd::config::show_config()?,
            }
            Ok(())
        }
        Command::Markets {
            query,
            perp,
            spot,
            dex,
        } => cmd::market::markets(network, query, perp, spot, dex).await,
        Command::Dexs => cmd::market::dexs(network).await,
        Command::Book { coin, levels } => cmd::market::book(network, coin, levels).await,
        Command::Watch { coins } => cmd::market::watch(network, coins).await,
        Command::Order(args) => cmd::order::order(network, args).await,
        Command::Account { address } => cmd::account::account(network, address).await,
        Command::Select { coins, add, remove } => {
            cmd::select::select(network, coins, add, remove).await
        }
        Command::Replay(args) => cmd::replay::replay(network, args).await,
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
            Some(RecordCmd::Verify {
                date,
                allow_orphans,
                strict,
            }) => record::verify(
                profile,
                network.map(Into::into),
                &date,
                allow_orphans,
                strict,
            ),
            Some(RecordCmd::RepairManifest { date, dry_run }) => {
                record::repair_manifest(profile, network.map(Into::into), &date, dry_run)
            }
        },
        Command::Probe { cmd } => match cmd {
            ProbeCmd::Latency { count } => {
                record::probe_latency(network.map(Into::into), count).await
            }
            ProbeCmd::TestnetRoundtrip(args) => {
                probe_roundtrip::run(network.map(Into::into), args).await
            }
        },
        Command::Nonce { cmd } => match cmd {
            NonceCmd::Reset => cmd::nonce::nonce_reset(network),
        },
    }
}

/// Resolve the configured kill-switch flag file and write/remove it
/// (SPEC-0004 K-3).
fn set_kill_switch(network: Option<NetworkArg>, trip: bool) -> Result<()> {
    let config = Config::load(ConfigOverrides {
        network: network.map(Into::into),
        ..Default::default()
    })?;
    live::set_kill_switch(&config.kill_file, trip)
}

fn resolve_network(network: Option<NetworkArg>) -> Result<Network> {
    if let Some(network) = network {
        return Ok(network.into());
    }
    Ok(Config::load(ConfigOverrides::default())?.network)
}
/// Build a selector, loading HIP-3 metadata only when the list needs it.
async fn selector_for(network: Network, coins: &[String]) -> Result<MarketSelector> {
    let info = HttpInfo::new(network);
    let include_hip3 = coins.iter().any(|coin| coin.contains(':'));
    Ok(MarketSelector::new(
        AssetMap::load(&info, include_hip3).await?,
    ))
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

pub(crate) async fn shutdown_signal() {
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
    use crate::cmd::nonce::reset_nonce_row;
    use crate::cmd::run::{live_exchange, signal_raw_gap};

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

    #[test]
    fn nonce_reset_rewrites_only_a_corrupt_row() {
        use hl_arb_client::nonce::{DEFAULT_NONCE_LEASE_MS, VENUE_MAX_FUTURE_MS};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hlbot.db");

        // A normal stored value must be left alone.
        Db::open(&path).unwrap().set_nonce_last(1_000).unwrap();
        assert!(
            reset_nonce_row(&path).is_err(),
            "a non-corrupt row must be refused"
        );
        assert_eq!(Db::open(&path).unwrap().nonce_last().unwrap(), Some(1_000));

        // A corrupt value (beyond the venue future window) is rewritten. Leave
        // a margin so the forward-moving wall clock cannot make it non-corrupt
        // between here and the reset.
        let now = SystemClock.now_ms();
        Db::open(&path)
            .unwrap()
            .set_nonce_last(now + VENUE_MAX_FUTURE_MS + 60_000)
            .unwrap();
        reset_nonce_row(&path).unwrap();
        let rewritten = Db::open(&path).unwrap().nonce_last().unwrap().unwrap();
        assert!(
            rewritten >= now && rewritten <= now + DEFAULT_NONCE_LEASE_MS + 60_000,
            "{rewritten} should be the new lease, not the corrupt value"
        );
    }

    #[test]
    fn raw_gap_carries_its_disconnect_time_to_the_engine() {
        use hl_arb_engine::channels::{MarketControl, inputs};

        let (handles, channel_inputs) = inputs(4, 4);
        let event = RawEvent::Gap {
            reason: "closed".into(),
            detail: "server closed".into(),
            disconnect_ns: 4_242,
            t_ns: 0,
        };
        assert!(!signal_raw_gap(&handles, &event));
        assert_eq!(
            channel_inputs.control.try_recv(),
            Ok(MarketControl::GapOpen {
                disconnect_ns: 4_242
            })
        );
    }

    #[test]
    fn nonce_reset_refuses_while_the_db_lock_is_held() {
        use hl_arb_client::nonce::VENUE_MAX_FUTURE_MS;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hlbot.db");
        let now = SystemClock.now_ms();
        Db::open(&path)
            .unwrap()
            .set_nonce_last(now + VENUE_MAX_FUTURE_MS + 60_000)
            .unwrap();

        // A running bot holds the lock: the reset must refuse, even for a row
        // that is otherwise corrupt and resettable.
        let lock = db_lock::DbLock::acquire(&path).unwrap();
        let err = reset_nonce_row(&path).unwrap_err();
        assert!(
            err.to_string().contains("running bot"),
            "unexpected error: {err}"
        );

        // Once the bot is gone the reset proceeds.
        drop(lock);
        reset_nonce_row(&path).unwrap();
    }
}
