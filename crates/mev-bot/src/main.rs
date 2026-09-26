//! `hl` — orchestration binary for the Hyperliquid-first trading system.
//!
//! M0.x: platform (config, observability, health, shutdown).
//! M1.1:  REST `/info` client and the `markets`/`book` commands.

use std::{
    collections::BTreeSet,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant},
};

mod engine;

use anyhow::Result;
use axum::{Router, extract::State, http::StatusCode, routing::get};
use clap::{Parser, Subcommand, ValueEnum};
use mev_core::{
    clock::{Clock, SystemClock},
    config::{Config, ConfigOverrides, Mode, Network},
    db::{Db, writer::DbWriter},
};
use mev_hl_client::{
    Action, AgentSigner, AssetMap, DeadMansSwitch, ExchangeApi, HttpInfo, InfoApi, Market,
    MarketKind, MarketSelector, MarketState, MarketStream, OrderParams, StreamEvent, Subscription,
    Tif, Tolerance, WsExchange, WsMarketStream, build_order_wire, build_request, now_ms,
};
use mev_metrics::{health::Health, prometheus::PrometheusHandle};
use mev_risk::TradingHalt;
use mev_strategy::AccountView;
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
    }
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
    let health = Health::new();
    let state = Arc::new(RwLock::new(MarketState::new(Tolerance::default())));
    {
        let mut guard = state.write().expect("market state lock poisoned");
        for coin in &book_coins {
            guard.expect_book(coin);
        }
        for coin in &ctx_coins {
            guard.expect_ctx(coin);
        }
    }

    let exchange = live_exchange(&config)?;

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
    let engine_task = plan.map(|plan| {
        let (writer, session_id) = session
            .clone()
            .expect("engine requires a recording session");
        let mut engine = engine::Engine::new(plan, &config, exchange.clone(), writer, session_id);
        // Live runs reconcile unknown order outcomes by cloid (SPEC-0002 H-2).
        if let Some(address) = config.account_address.clone() {
            engine = engine.with_info(info.clone(), address);
        }
        let account = engine.account();
        let halt = engine.halt();
        // SPEC-0010 §15: the H-3 stream is the source of truth; this is the
        // REST reconciler backstop, not the order path.
        let reconciler = config.account_address.clone().map(|address| {
            tokio::spawn(engine::account_reconciler(
                info.clone(),
                address,
                account.clone(),
                config.network,
            ))
        });
        (
            tokio::spawn(engine.run(state.clone())),
            reconciler,
            account,
            halt,
        )
    });

    let heartbeat = tokio::spawn(heartbeat());
    let ingest = tokio::spawn(ingest(
        state.clone(),
        subscriptions,
        config.network,
        recorder,
    ));
    let monitor = tokio::spawn(monitor(state, health.clone()));
    let deadman = exchange.clone().map(|exchange| {
        let (account, halt) = engine_task.as_ref().map_or_else(
            || {
                (
                    Arc::new(RwLock::new(AccountView::default())),
                    mev_risk::TradingHalt::new(),
                )
            },
            |(_, _, account, halt)| (account.clone(), halt.clone()),
        );
        tokio::spawn(deadman(
            exchange,
            info.clone(),
            config.account_address.clone(),
            account,
            halt,
            config.schedule_cancel_ttl_ms,
        ))
    });

    info!("waiting for feeds to become ready");
    serve(health, metrics, config.http_port).await?;

    ingest.abort();
    monitor.abort();
    heartbeat.abort();
    if let Some((engine, reconciler, _, _)) = engine_task {
        engine.abort();
        if let Some(reconciler) = reconciler {
            reconciler.abort();
        }
    }
    // The dead-man task disarms `scheduleCancel` on the same shutdown signal;
    // give it a moment to submit before the process exits.
    if let Some(deadman) = deadman {
        let _ = tokio::time::timeout(Duration::from_secs(5), deadman).await;
    }
    info!("shutdown complete");
    Ok(())
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
/// Each cycle the task reads the live account's open-order count. The switch
/// (a) arms when the first order rests, (b) refreshes at half the TTL, and
/// (c) disarms when the last order leaves — so an idle bot does not spend
/// address rate-limit budget. If arming/refreshing fails while orders rest, a
/// sticky [`TradingHalt`] is set so the risk gate refuses new orders. The task
/// also polls `userRateLimit` every 60 s and exposes the remaining address
/// budget as a metric.
async fn deadman(
    exchange: Arc<dyn ExchangeApi>,
    info: Arc<dyn InfoApi>,
    address: Option<String>,
    account: Arc<RwLock<AccountView>>,
    halt: TradingHalt,
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
                let resting = account
                    .read()
                    .map(|guard| guard.open_orders.len())
                    .unwrap_or(0);
                if let Some(action) = switch.update(now_ms(), resting) {
                    let arming = resting > 0;
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
                            halt.set();
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

/// Ingest market data into the shared state until the process stops.
async fn ingest(
    state: Arc<RwLock<MarketState>>,
    subscriptions: Vec<Subscription>,
    network: Network,
    recorder: Option<engine::Recorder>,
) {
    loop {
        match WsMarketStream::connect(network, &subscriptions).await {
            Ok(mut stream) => {
                info!(
                    subscriptions = subscriptions.len(),
                    "market stream connected"
                );
                loop {
                    match stream.next().await {
                        Ok(event) => {
                            if let Some(recorder) = &recorder {
                                recorder.record(
                                    &mev_strategy::Event::Market(event.clone()),
                                    SystemClock.now_ms(),
                                );
                            }
                            if let Ok(mut guard) = state.write() {
                                guard.apply(&event);
                            }
                        }
                        Err(err) => {
                            tracing::warn!(error = %err, "market stream ended");
                            break;
                        }
                    }
                }
            }
            Err(err) => tracing::warn!(error = %err, "market stream connect failed"),
        }

        tokio::time::sleep(Duration::from_secs(5)).await;
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

async fn serve(health: Health, metrics: PrometheusHandle, port: u16) -> Result<()> {
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
}
