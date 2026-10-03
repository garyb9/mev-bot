//! Strategy/config building, recording, and replay helpers for the `hl`
//! binary.
//!
//! SPEC-0010 E-13 removed the legacy 1 s tick engine: the decision loop now
//! lives in [`hl_arb_engine::run::EngineLoop`] over a
//! [`hl_arb_engine::StrategyDispatcher`]. This module keeps the pieces that are
//! reusable and not tied to the tick loop:
//!
//! - strategy/config construction ([`build`], [`EngineBuild`]) and the
//!   [`Interests`] → market [`Subscription`] mapping;
//! - the SQLite [`Recorder`] for market/replay input;
//! - the REST account reconciler backstop that feeds the engine's account
//!   channel (SPEC-0010 §15);
//! - the deterministic replay helpers ([`replay`], [`replay_events`]) used by
//!   `hl replay`.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use hl_arb_client::{InfoApi, MarketSelector, MarketState, Subscription, Tolerance};
use hl_arb_core::clock::{Clock, SystemClock};
use hl_arb_core::config::Config;
use hl_arb_core::db::writer::{DbWriter, WriteCmd};
use hl_arb_core::db::{Db, EventRow};
use hl_arb_engine::channels::InputHandles;
use hl_arb_engine::reconcile::Reconciler;
use hl_arb_engine::{
    AccountSnapshot, AccountState, AccountUpdate, Action, Actions, AssetCtxLite, BOOK_DEPTH,
    BookSnapshot, CoinRegistry, Ctx, FundingBasis, FundingConfig, Interests, Level, MarketMaker,
    MarketSlot, MmConfig, Stamp, Strategy, Stream,
};
use hl_arb_strategy::{AccountView, BookView, CostModel, Event, FeeRates, Instrument, MarketView};
use rust_decimal::Decimal;
use tracing::{info, warn};

/// Strategies plus the market feeds they need.
pub struct EngineBuild {
    /// Configured strategies.
    pub strategies: Vec<Box<dyn Strategy>>,
    /// Extra subscriptions the engine requires beyond the watchlist.
    pub subscriptions: Vec<Subscription>,
    /// Every coin the engine snapshots.
    pub coins: Vec<String>,
    /// Size decimals per coin for building book views.
    pub sz_decimals: BTreeMap<String, u32>,
    /// Instruments for paper routing.
    pub instruments: BTreeMap<String, Instrument>,
    /// Interned coin map shared with the strategies' configs.
    pub registry: CoinRegistry,
}

/// Instantiate the enabled strategies from config.
///
/// Two passes: the first resolves every configured coin and builds the interned
/// [`CoinRegistry`], the second constructs strategies with their `CoinId`s and
/// derives the feed subscriptions from each strategy's [`Interests`].
pub fn build(cfg: &Config, selector: &MarketSelector) -> Result<EngineBuild> {
    let cost = CostModel::new(Decimal::from(cfg.strategy.min_edge_bps));
    let mut build = EngineBuild {
        strategies: Vec::new(),
        subscriptions: Vec::new(),
        coins: Vec::new(),
        sz_decimals: BTreeMap::new(),
        instruments: BTreeMap::new(),
        registry: CoinRegistry::default(),
    };

    // Pass 1: register every strategy coin and its routing metadata.
    for id in &cfg.strategy.enabled {
        match id.as_str() {
            hl_arb_engine::strategies::funding::ID => {
                let f = &cfg.strategy.funding;
                let perp = selector
                    .resolve(&f.perp_coin)
                    .with_context(|| format!("resolving perp {}", f.perp_coin))?;
                let spot = selector
                    .resolve(&f.spot_pair)
                    .with_context(|| format!("resolving spot {}", f.spot_pair))?;
                build
                    .instruments
                    .insert(perp.coin.clone(), Instrument::perp());
                build
                    .instruments
                    .insert(spot.coin.clone(), Instrument::spot(f.spot_token.clone()));
                build
                    .sz_decimals
                    .insert(perp.coin.clone(), perp.sz_decimals);
                build
                    .sz_decimals
                    .insert(spot.coin.clone(), spot.sz_decimals);
                build.coins.push(perp.coin);
                build.coins.push(spot.coin);
            }
            hl_arb_engine::strategies::mm::ID => {
                for coin in &cfg.strategy.market_making.coins {
                    let market = selector
                        .resolve(coin)
                        .with_context(|| format!("resolving mm coin {coin}"))?;
                    build
                        .instruments
                        .insert(market.coin.clone(), Instrument::perp());
                    build
                        .sz_decimals
                        .insert(market.coin.clone(), market.sz_decimals);
                    build.coins.push(market.coin);
                }
            }
            other => warn!(strategy = other, "unknown strategy id; ignoring"),
        }
    }
    build.registry = CoinRegistry::from_coins(&build.coins);

    // Pass 2: construct the strategies now that ids are stable.
    for id in &cfg.strategy.enabled {
        match id.as_str() {
            hl_arb_engine::strategies::funding::ID => {
                build_funding(&mut build, cfg, selector, cost)?;
            }
            hl_arb_engine::strategies::mm::ID => build_market_making(&mut build, cfg, selector)?,
            _ => {}
        }
    }

    let subscriptions: Vec<Subscription> = build
        .strategies
        .iter()
        .flat_map(|strategy| interests_to_subscriptions(&strategy.interests(), &build.registry))
        .collect();
    build.subscriptions = subscriptions;
    Ok(build)
}

fn build_market_making(
    build: &mut EngineBuild,
    cfg: &Config,
    selector: &MarketSelector,
) -> Result<()> {
    let mm = &cfg.strategy.market_making;
    for coin in &mm.coins {
        let market = selector
            .resolve(coin)
            .with_context(|| format!("resolving mm coin {coin}"))?;
        let coin_id = build
            .registry
            .id(&market.coin)
            .with_context(|| format!("coin {} not interned", market.coin))?;
        let config = MmConfig {
            coin: coin_id,
            sz_decimals: market.sz_decimals,
            levels: mm.levels,
            half_spread_bps: Decimal::from(mm.half_spread_bps),
            level_step_bps: Decimal::from(mm.level_step_bps),
            size_per_level: mm.size_per_level,
            max_inventory: mm.max_inventory,
            max_skew_bps: Decimal::from(mm.max_skew_bps),
            vol_pull_bps: Decimal::from(mm.vol_pull_bps),
            refresh_bps: Decimal::from(mm.refresh_bps),
        };
        build.strategies.push(Box::new(MarketMaker::new(config)));
    }
    Ok(())
}

fn build_funding(
    build: &mut EngineBuild,
    cfg: &Config,
    selector: &MarketSelector,
    cost: CostModel,
) -> Result<()> {
    let f = &cfg.strategy.funding;
    let perp = selector
        .resolve(&f.perp_coin)
        .with_context(|| format!("resolving perp {}", f.perp_coin))?;
    let spot = selector
        .resolve(&f.spot_pair)
        .with_context(|| format!("resolving spot {}", f.spot_pair))?;
    let perp_id = build
        .registry
        .id(&perp.coin)
        .with_context(|| format!("coin {} not interned", perp.coin))?;
    let spot_id = build
        .registry
        .id(&spot.coin)
        .with_context(|| format!("coin {} not interned", spot.coin))?;

    let config = FundingConfig {
        perp_coin: perp_id,
        spot_coin: spot_id,
        spot_token: f.spot_token.clone(),
        spot_sz_decimals: spot.sz_decimals,
        target_notional: f.target_notional_usd,
        horizon_hours: Decimal::from(f.horizon_hours),
        exit_threshold_bps: Decimal::from(f.exit_threshold_bps),
        exit_after_hours: f.exit_after_hours,
        rebalance_drift_bps: Decimal::from(f.rebalance_drift_bps),
    };
    let strategy = FundingBasis::new(config, cost, FeeRates::PERP, FeeRates::SPOT, f.maker);
    build.strategies.push(Box::new(strategy));
    Ok(())
}

/// Map strategy [`Interests`] onto market-data [`Subscription`]s.
///
/// Book/BBO interest becomes an L2 book feed and context interest an asset
/// context feed; duplicate subscriptions are collapsed.
pub fn interests_to_subscriptions(
    interests: &Interests,
    registry: &CoinRegistry,
) -> Vec<Subscription> {
    let mut subs: Vec<Subscription> = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for (coin, stream) in &interests.coins {
        let Some(name) = registry.coin(*coin) else {
            continue;
        };
        let sub = match stream {
            Stream::Book => Subscription::L2Book {
                coin: name.to_string(),
            },
            Stream::Bbo => Subscription::Bbo {
                coin: name.to_string(),
            },
            Stream::Ctx => Subscription::ActiveAssetCtx {
                coin: name.to_string(),
            },
            Stream::Trades => Subscription::Trades {
                coin: name.to_string(),
            },
        };
        let key = serde_json::to_string(&sub).unwrap_or_default();
        if seen.insert(key) {
            subs.push(sub);
        }
    }
    subs
}

/// Appends input events to the SQLite replay log.
#[derive(Clone)]
pub struct Recorder {
    writer: Arc<DbWriter>,
    session_id: i64,
}

impl Recorder {
    /// Build a recorder bound to a session.
    pub fn new(writer: Arc<DbWriter>, session_id: i64) -> Self {
        Self { writer, session_id }
    }

    /// Record one input event at `ts_ms`.
    pub fn record(&self, event: &Event, ts_ms: u64) {
        match serde_json::to_string(event) {
            Ok(payload) => {
                self.writer.try_send(WriteCmd::Event {
                    session_id: self.session_id,
                    ts_ms,
                    kind: event_kind(event).to_string(),
                    payload,
                });
            }
            Err(err) => warn!(error = %err, "failed to encode event for recording"),
        }
    }
}

/// The replay-log tag for an event.
pub fn event_kind(event: &Event) -> &'static str {
    match event {
        Event::Market(_) => "market",
        Event::Account(_) => "account",
        Event::Timer { .. } => "timer",
        Event::Fill(_) => "fill",
    }
}

/// Refresh the engine's account snapshot on the reconciler cadence.
///
/// SPEC-0010 §15: the H-3 stream is the source of truth for own orders and
/// fills; the REST reconciler is the backstop and runs every 30 s (and after a
/// reconnect). This replaces the former 5 s `account_poller`. It is a background
/// task, never on the order path. The snapshot is delivered losslessly through
/// the engine's account channel as [`AccountUpdate::Reconcile`].
pub async fn account_reconciler(
    info: Arc<dyn InfoApi>,
    address: String,
    registry: CoinRegistry,
    handles: InputHandles,
) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
    loop {
        interval.tick().await;
        match load_account_snapshot(&*info, &address, &registry).await {
            Ok(snapshot) => {
                let update = AccountUpdate::Reconcile {
                    stamp: now_stamp(),
                    snapshot,
                };
                if !handles.send_account(update) {
                    break;
                }
            }
            Err(err) => warn!(error = %err, "account reconcile failed"),
        }
    }
}

async fn load_account_snapshot(
    info: &dyn InfoApi,
    address: &str,
    registry: &CoinRegistry,
) -> Result<AccountSnapshot> {
    let clearing = info.clearinghouse_state(address).await?;
    let open_orders = info.open_orders(address).await.unwrap_or_default();
    let now_ns = i64::try_from(SystemClock.now_ms().saturating_mul(1_000_000)).unwrap_or(i64::MAX);
    let snapshot = Reconciler::build_snapshot(&clearing, &open_orders, registry, now_ns);
    Ok(snapshot.account_snapshot())
}

/// A receive stamp for an account update read off-thread.
pub fn now_stamp() -> Stamp {
    let now = SystemClock.now_ms();
    Stamp {
        t_recv_ns: i64::try_from(now)
            .unwrap_or(i64::MAX)
            .saturating_mul(1_000_000),
        mono_ns: now.saturating_mul(1_000_000),
        ts_exch_ms: 0,
    }
}

/// Build a market snapshot from state for the given coins.
fn build_market_view(
    state: &MarketState,
    coins: &[String],
    sz_decimals: &BTreeMap<String, u32>,
) -> MarketView {
    let mut view = MarketView::new();
    for coin in coins {
        if let Some(book) = state.book(coin) {
            let sz = sz_decimals.get(coin).copied().unwrap_or(0);
            view.insert_book(coin.clone(), BookView::from_order_book(book, sz));
        }
        if let Some(ctx) = state.ctx(coin) {
            view.insert_ctx(coin.clone(), ctx.clone());
        }
    }
    if let Some(mids) = state.mids() {
        view.set_mids(mids.clone());
    }
    view
}

/// Convert a [`MarketView`] into per-coin [`MarketSlot`]s.
fn to_slots(registry: &CoinRegistry, market: &MarketView) -> Vec<MarketSlot> {
    let mut slots = vec![MarketSlot::default(); registry.len()];
    for (coin_id, coin) in registry.iter() {
        let slot = &mut slots[coin_id.index()];
        if let Some(book) = market.book(coin) {
            let mut snapshot = BookSnapshot {
                time_ms: book.time,
                ..Default::default()
            };
            let mut n_bids = 0u8;
            for (level, (px, sz)) in snapshot
                .bids
                .iter_mut()
                .zip(book.bids.iter().take(BOOK_DEPTH))
            {
                *level = Level {
                    px: *px,
                    sz: *sz,
                    n: 1,
                };
                n_bids += 1;
            }
            let mut n_asks = 0u8;
            for (level, (px, sz)) in snapshot
                .asks
                .iter_mut()
                .zip(book.asks.iter().take(BOOK_DEPTH))
            {
                *level = Level {
                    px: *px,
                    sz: *sz,
                    n: 1,
                };
                n_asks += 1;
            }
            snapshot.n_bids = n_bids;
            snapshot.n_asks = n_asks;
            if let (Some((bid_px, bid_sz)), Some((ask_px, ask_sz))) =
                (book.best_bid(), book.best_ask())
            {
                slot.bbo = Some((
                    Some(Level {
                        px: bid_px,
                        sz: bid_sz,
                        n: 1,
                    }),
                    Some(Level {
                        px: ask_px,
                        sz: ask_sz,
                        n: 1,
                    }),
                    Stamp::default(),
                ));
            }
            slot.book = Some((snapshot, Stamp::default()));
        }
        if let Some(ctx) = market.ctx(coin) {
            slot.ctx = Some((
                AssetCtxLite {
                    funding: ctx.funding,
                    mark_px: ctx.mark_px,
                    oracle_px: ctx.oracle_px,
                    open_interest: ctx.open_interest,
                },
                Stamp::default(),
            ));
        }
    }
    slots
}

/// Convert an [`AccountView`] into a per-coin [`AccountState`].
fn to_account(registry: &CoinRegistry, view: &AccountView) -> AccountState {
    let mut account = AccountState::new(registry.len());
    for (coin_id, coin) in registry.iter() {
        account.set_position_szi(coin_id, view.position_szi(coin));
    }
    account.spot = view.spot.clone();
    account.account_value = view.account_value;
    account.margin_used = view.margin_used;
    account
}

/// Outcome of a deterministic replay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayOutcome {
    /// Events consumed.
    pub events: usize,
    /// Intents emitted across every timer cycle.
    pub intents: usize,
    /// Stable fingerprint of the emitted intents; identical inputs must match.
    pub fingerprint: u64,
}

/// Re-drive strategies from a recorded event log with no network or clock.
///
/// Market events update a local [`MarketState`], account events replace the
/// account snapshot, and timer events run one decision cycle at the recorded
/// event time. Only placements are fingerprinted (cancels are covered by the
/// `cloid`s they reference).
pub async fn replay_events(
    rows: &[EventRow],
    strategies: &mut [Box<dyn Strategy>],
    coins: &[String],
    sz_decimals: &BTreeMap<String, u32>,
    registry: &CoinRegistry,
) -> Result<ReplayOutcome> {
    let mut state = MarketState::new(Tolerance::default());
    for coin in coins {
        state.expect_book(coin);
    }

    let mut account = AccountView::default();
    let mut trace = String::new();
    let mut intents = 0usize;

    for row in rows {
        let event: Event = serde_json::from_str(&row.payload)
            .with_context(|| format!("decoding event seq {}", row.seq))?;
        match event {
            Event::Market(market_event) => state.apply(&market_event),
            Event::Account(view) => account = view,
            Event::Timer { .. } => {
                let market = build_market_view(&state, coins, sz_decimals);
                let slots = to_slots(registry, &market);
                let account_state = to_account(registry, &account);
                let ctx = Ctx {
                    now: Stamp {
                        t_recv_ns: (row.ts_ms as i64) * 1_000_000,
                        ..Default::default()
                    },
                    markets: &slots,
                    account: &account_state,
                    registry,
                };
                for strategy in strategies.iter_mut() {
                    let mut actions = Actions::new();
                    for coin in strategy.interests().distinct_coins() {
                        strategy.on_market(coin, &ctx, &mut actions);
                    }
                    for action in actions.take() {
                        if let Action::Place(intent) = action {
                            intents += 1;
                            trace.push_str(&serde_json::to_string(&intent)?);
                        }
                    }
                }
            }
            Event::Fill(_) => {}
        }
    }

    Ok(ReplayOutcome {
        events: rows.len(),
        intents,
        fingerprint: fingerprint(trace.as_bytes()),
    })
}

/// FNV-1a 64-bit fingerprint.
fn fingerprint(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Load a session's events and replay them with the configured strategies.
pub async fn replay(
    cfg: &Config,
    selector: &MarketSelector,
    session_id: Option<i64>,
    db_path: &std::path::Path,
) -> Result<ReplayOutcome> {
    let db = Db::open(db_path)?;
    let session_id = match session_id {
        Some(id) => id,
        None => db
            .latest_session()?
            .context("no recorded sessions; run `hl run --mode simulate` first")?,
    };
    let rows = db.read_events(session_id)?;
    let mut build = build(cfg, selector)?;
    let outcome = replay_events(
        &rows,
        &mut build.strategies,
        &build.coins,
        &build.sz_decimals,
        &build.registry,
    )
    .await?;
    info!(
        session_id,
        events = outcome.events,
        intents = outcome.intents,
        "replay complete"
    );
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::str::FromStr;
    use std::sync::{Arc, Mutex};

    use hl_arb_client::types::{AssetMeta, L2Book, Level as WireLevel, Meta};
    use hl_arb_client::{AssetMap, StreamEvent};
    use hl_arb_engine::builder::AssetTable;
    use hl_arb_engine::channels::inputs;
    use hl_arb_engine::dispatch::{DispatcherConfig, StrategyDispatcher};
    use hl_arb_engine::exec::{ExecBackend, UnsignedPost};
    use hl_arb_engine::risk::RiskGate;
    use hl_arb_engine::run::{EngineLoop, LoopConfig};
    use hl_arb_engine::types::{CoinId, MarketUpdate};

    use super::*;

    fn ds(value: &str) -> Decimal {
        Decimal::from_str(value).unwrap()
    }

    fn row(seq: u64, ts_ms: u64, kind: &str, event: &Event) -> EventRow {
        EventRow {
            seq,
            ts_ms,
            kind: kind.to_string(),
            payload: serde_json::to_string(event).unwrap(),
        }
    }

    fn book_row(seq: u64, ts_ms: u64, bid: &str, ask: &str) -> EventRow {
        let event = StreamEvent::Book(L2Book {
            coin: "BTC".into(),
            time: ts_ms,
            levels: [
                vec![WireLevel {
                    px: ds(bid),
                    sz: ds("10"),
                    n: 1,
                }],
                vec![WireLevel {
                    px: ds(ask),
                    sz: ds("10"),
                    n: 1,
                }],
            ],
        });
        row(seq, ts_ms, "market", &Event::Market(event))
    }

    fn timer_row(seq: u64, ts_ms: u64) -> EventRow {
        row(seq, ts_ms, "timer", &Event::Timer { every_ms: 1_000 })
    }

    fn account_row(seq: u64, ts_ms: u64) -> EventRow {
        row(
            seq,
            ts_ms,
            "account",
            &Event::Account(AccountView::default()),
        )
    }

    fn strategies() -> Vec<Box<dyn Strategy>> {
        vec![Box::new(MarketMaker::new(MmConfig {
            coin: hl_arb_engine::CoinId(0),
            sz_decimals: 3,
            levels: 2,
            half_spread_bps: ds("5"),
            level_step_bps: ds("5"),
            size_per_level: ds("1"),
            max_inventory: ds("10"),
            max_skew_bps: ds("5"),
            vol_pull_bps: ds("50"),
            refresh_bps: ds("2"),
        }))]
    }

    #[tokio::test]
    async fn replay_is_deterministic() {
        let rows = vec![
            book_row(0, 1, "100", "100"),
            account_row(1, 1),
            timer_row(2, 1),
            book_row(3, 2, "101", "101"),
            timer_row(4, 2),
        ];
        let coins = vec!["BTC".to_string()];
        let mut sz_decimals = BTreeMap::new();
        sz_decimals.insert("BTC".to_string(), 3u32);
        let registry = CoinRegistry::from_coins(&coins);

        let mut first = strategies();
        let a = replay_events(&rows, &mut first, &coins, &sz_decimals, &registry)
            .await
            .unwrap();
        let mut second = strategies();
        let b = replay_events(&rows, &mut second, &coins, &sz_decimals, &registry)
            .await
            .unwrap();

        assert_eq!(a, b, "same log must replay identically");
        assert!(a.intents > 0, "the ladder should place quotes");
        assert_ne!(a.fingerprint, 0);
    }

    struct CapturingExec {
        posts: Arc<Mutex<Vec<UnsignedPost>>>,
    }

    impl ExecBackend for CapturingExec {
        fn try_send(&mut self, post: UnsignedPost) -> bool {
            self.posts
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(post);
            true
        }
    }

    fn btc_asset_map() -> AssetMap {
        let mut map = AssetMap::new();
        map.insert_perp_dex(
            None,
            None,
            &Meta {
                universe: vec![AssetMeta {
                    name: "BTC".into(),
                    sz_decimals: 3,
                    max_leverage: 40,
                    is_delisted: false,
                    only_isolated: false,
                }],
            },
        );
        map
    }

    fn book_snapshot(px: &str) -> BookSnapshot {
        let mut book = BookSnapshot {
            bids: [Level::default(); BOOK_DEPTH],
            asks: [Level::default(); BOOK_DEPTH],
            n_bids: 1,
            n_asks: 1,
            time_ms: 0,
        };
        let level = Level {
            px: ds(px),
            sz: ds("1"),
            n: 1,
        };
        book.bids[0] = level;
        book.asks[0] = level;
        book
    }

    /// The v2 `EngineLoop` + `StrategyDispatcher` is what runs: a market event
    /// dispatched through the loop yields a built order post.
    #[test]
    fn v2_loop_turns_a_market_event_into_a_post() {
        let registry = CoinRegistry::from_coins(&["BTC".into()]);
        let table = AssetTable::from_markets(&registry, &btc_asset_map());
        let exec = CapturingExec {
            posts: Arc::new(Mutex::new(Vec::new())),
        };
        let posts = exec.posts.clone();
        let mm = MarketMaker::new(MmConfig {
            coin: CoinId(0),
            sz_decimals: 3,
            levels: 2,
            half_spread_bps: ds("5"),
            level_step_bps: ds("5"),
            size_per_level: ds("1"),
            max_inventory: ds("10"),
            max_skew_bps: ds("5"),
            vol_pull_bps: ds("50"),
            refresh_bps: ds("2"),
        });
        let dispatcher = StrategyDispatcher::new(
            vec![Box::new(mm)],
            registry,
            table,
            RiskGate::default(),
            Some(Box::new(exec)),
            DispatcherConfig::default(),
        );

        let (handles, inputs) = inputs(16, 16);
        let (_stop_tx, stop_rx) = crossbeam_channel::bounded(1);
        let mut engine = EngineLoop::with_dispatcher(
            inputs,
            dispatcher,
            LoopConfig {
                spin_us: 0,
                coin_count: 1,
            },
            stop_rx,
        );

        handles.send_market(MarketUpdate::Book {
            coin: CoinId(0),
            stamp: Stamp {
                mono_ns: 1_000,
                ..Default::default()
            },
            book: book_snapshot("100"),
        });
        assert_eq!(engine.iterate(1_000), 1);

        let posts = posts.lock().unwrap_or_else(|p| p.into_inner());
        assert_eq!(posts.len(), 1, "one bulk order post from the loop");
    }

    #[test]
    fn interests_map_bbo_to_bbo_and_book_to_l2book() {
        use hl_arb_engine::strategy::{Interests, Stream};
        let registry = CoinRegistry::from_coins(&["BTC".to_string(), "ETH".to_string()]);
        let interests = Interests {
            coins: vec![(CoinId(0), Stream::Bbo), (CoinId(1), Stream::Book)],
            ..Interests::default()
        };
        let subs = interests_to_subscriptions(&interests, &registry);
        assert!(
            subs.iter()
                .any(|s| matches!(s, Subscription::Bbo { coin } if coin == "BTC")),
            "a Bbo interest must subscribe to the bbo channel, not l2Book"
        );
        assert!(
            subs.iter()
                .any(|s| matches!(s, Subscription::L2Book { coin } if coin == "ETH")),
            "a Book interest still subscribes to l2Book"
        );
    }
}
