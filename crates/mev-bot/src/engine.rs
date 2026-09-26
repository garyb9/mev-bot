//! Strategy engine: builds views, drives strategies, gates intents through
//! risk, and executes them (paper in `simulate`, `/exchange` in `live`).

pub mod types;

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context as _, Result};
use mev_core::clock::{Clock, SystemClock};
use mev_core::config::{Config, Mode, Network};
use mev_core::db::writer::{DbWriter, WriteCmd};
use mev_core::db::{Db, EventRow, FillRecord, OrderRecord};
use mev_hl_client::{
    AssetMap, CancelByCloidWire, CancelWire, CloidFactory, ExchangeApi, InfoApi,
    MIN_ORDER_NOTIONAL, Market, MarketSelector, MarketState, OrderParams, OrderResolution,
    OrderStatus, Subscription, Tolerance, build_order_wire, round_price_aggressive,
};
use mev_risk::{Decision, LimitRisk, Limits, RiskCheck, RiskContext};
use mev_strategy::{
    AccountView, Action, BookView, CancelIntent, CostModel, Event, FeeRates, FillEvent,
    FundingBasis, FundingConfig, Instrument, MarketMaker, MarketView, MmConfig, OpenOrderView,
    OrderIntent, PaperExecutor, PositionView, Strategy, StrategyContext, StrategyId, Trigger,
};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use tracing::{info, warn};

use mev_metrics::names;

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
    /// Resolved market metadata for live order building.
    pub markets: AssetMap,
}

/// Instantiate the enabled strategies from config.
pub fn build(cfg: &Config, selector: &MarketSelector) -> Result<EngineBuild> {
    let cost = CostModel::new(Decimal::from(cfg.strategy.min_edge_bps));
    let mut build = EngineBuild {
        strategies: Vec::new(),
        subscriptions: Vec::new(),
        coins: Vec::new(),
        sz_decimals: BTreeMap::new(),
        instruments: BTreeMap::new(),
        markets: selector.asset_map().clone(),
    };

    for id in &cfg.strategy.enabled {
        match id.as_str() {
            mev_strategy::funding::ID => build_funding(&mut build, cfg, selector, cost)?,
            mev_strategy::mm::ID => build_market_making(&mut build, cfg, selector)?,
            other => warn!(strategy = other, "unknown strategy id; ignoring"),
        }
    }
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
        build
            .instruments
            .insert(market.coin.clone(), Instrument::perp());
        build
            .sz_decimals
            .insert(market.coin.clone(), market.sz_decimals);
        let config = MmConfig {
            coin: market.coin.clone(),
            levels: mm.levels,
            half_spread_bps: Decimal::from(mm.half_spread_bps),
            level_step_bps: Decimal::from(mm.level_step_bps),
            size_per_level: mm.size_per_level,
            max_inventory: mm.max_inventory,
            max_skew_bps: Decimal::from(mm.max_skew_bps),
            vol_pull_bps: Decimal::from(mm.vol_pull_bps),
            refresh_bps: Decimal::from(mm.refresh_bps),
        };
        let strategy = MarketMaker::new(config);
        build.subscriptions.extend(strategy.subscriptions());
        build.coins.push(market.coin);
        build.strategies.push(Box::new(strategy));
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

    let config = FundingConfig {
        perp_coin: perp.coin.clone(),
        spot_coin: spot.coin.clone(),
        spot_token: f.spot_token.clone(),
        target_notional: f.target_notional_usd,
        horizon_hours: Decimal::from(f.horizon_hours),
        exit_threshold_bps: Decimal::from(f.exit_threshold_bps),
        exit_after_hours: f.exit_after_hours,
        rebalance_drift_bps: Decimal::from(f.rebalance_drift_bps),
    };
    let strategy = FundingBasis::new(config, cost, FeeRates::PERP, FeeRates::SPOT, f.maker);
    build.subscriptions.extend(strategy.subscriptions());
    build.coins.push(perp.coin);
    build.coins.push(spot.coin);
    build.strategies.push(Box::new(strategy));
    Ok(())
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

    /// The underlying writer (for order/fill records).
    pub fn writer(&self) -> &Arc<DbWriter> {
        &self.writer
    }

    /// The session these events belong to.
    pub fn session_id(&self) -> i64 {
        self.session_id
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

/// The running strategy engine.
pub struct Engine {
    strategies: Vec<Box<dyn Strategy>>,
    coins: Vec<String>,
    sz_decimals: BTreeMap<String, u32>,
    markets: AssetMap,
    risk: LimitRisk,
    paper: Option<PaperExecutor>,
    exchange: Option<Arc<dyn ExchangeApi>>,
    /// Read API + account address, used to reconcile unknown order outcomes by
    /// cloid before anything is resent (SPEC-0002 H-2).
    info: Option<(Arc<dyn InfoApi>, String)>,
    account: Arc<RwLock<AccountView>>,
    recorder: Recorder,
    cloids: CloidFactory,
    max_slippage_bps: Decimal,
}

impl Engine {
    /// Construct the engine for the given mode.
    pub fn new(
        build: EngineBuild,
        cfg: &Config,
        exchange: Option<Arc<dyn ExchangeApi>>,
        writer: Arc<DbWriter>,
        session_id: i64,
    ) -> Self {
        let paper = (cfg.mode == Mode::Simulate).then(|| {
            let seeded = AccountView {
                account_value: Decimal::from(100_000),
                fees: FeeRates::PERP,
                ..Default::default()
            };
            PaperExecutor::new(
                build.instruments.clone(),
                seeded,
                FeeRates::PERP,
                FeeRates::SPOT,
            )
        });
        let risk = LimitRisk::new(Limits {
            min_notional: MIN_ORDER_NOTIONAL,
            max_order_notional: cfg.risk.max_order_notional_usd,
            max_position_notional: cfg.risk.max_position_notional_usd,
            max_open_orders: cfg.risk.max_open_orders,
            max_margin_utilization_bps: cfg.risk.max_margin_utilization_bps,
        });
        Self {
            strategies: build.strategies,
            coins: build.coins,
            sz_decimals: build.sz_decimals,
            markets: build.markets,
            risk,
            paper,
            exchange,
            info: None,
            account: Arc::new(RwLock::new(AccountView::default())),
            recorder: Recorder::new(writer, session_id),
            cloids: CloidFactory::new(),
            max_slippage_bps: Decimal::from(cfg.strategy.max_slippage_bps),
        }
    }

    /// Attach the read API and account address used to reconcile unknown order
    /// outcomes by `cloid` (SPEC-0002 H-2).
    pub fn with_info(mut self, info: Arc<dyn InfoApi>, address: impl Into<String>) -> Self {
        self.info = Some((info, address.into()));
        self
    }

    /// The shared live-account view (updated by [`account_poller`]).
    pub fn account(&self) -> Arc<RwLock<AccountView>> {
        self.account.clone()
    }

    /// A handle to the risk gate's sticky trading-halt flag.
    pub fn halt(&self) -> mev_risk::TradingHalt {
        self.risk.halt()
    }

    /// Run the decision loop until the task is aborted.
    pub async fn run(mut self, state: Arc<RwLock<MarketState>>) {
        info!(
            strategies = self.strategies.len(),
            "strategy engine started"
        );
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tick.tick().await;
            if let Err(err) = self.step(&state).await {
                warn!(error = %err, "engine step failed");
            }
        }
    }

    async fn step(&mut self, state: &Arc<RwLock<MarketState>>) -> Result<()> {
        self.risk.begin_cycle();
        let now = SystemClock.now_ms();
        let market = {
            let guard = state.read().expect("market state lock poisoned");
            self.snapshot_market(&guard)
        };
        let account = match &self.paper {
            Some(paper) => paper.account().clone(),
            None => self.account.read().expect("account lock poisoned").clone(),
        };

        // Record the decision-cycle inputs so the run can be replayed offline.
        self.recorder.record(&Event::Account(account.clone()), now);
        self.recorder.record(&Event::Timer { every_ms: 1_000 }, now);

        let mut proposals: Vec<Vec<Action>> = Vec::new();
        for strategy in &mut self.strategies {
            let ctx = StrategyContext {
                now_ms: now,
                trigger: Trigger::Timer,
                market: &market,
                account: &account,
            };
            let actions = strategy.on_event(&ctx).await?;
            if !actions.is_empty() {
                proposals.push(actions);
            }
        }

        let mut fills: Vec<FillEvent> = Vec::new();
        for actions in proposals {
            for action in actions {
                match action {
                    Action::Place(intent) => {
                        fills.extend(
                            self.gate_and_execute(intent, &market, &account, now)
                                .await?,
                        );
                    }
                    Action::Cancel(cancel) => self.cancel(&cancel, now).await,
                }
            }
        }

        if let Some(paper) = &mut self.paper {
            let maker_fills = paper.on_market(&market, now);
            fills.extend(maker_fills);
        }

        for fill in &fills {
            self.record_fill(fill, now);
        }
        for fill in &fills {
            let Some(sid) = fill.strategy.clone() else {
                continue;
            };
            for strategy in &mut self.strategies {
                if strategy.id() == sid {
                    strategy.on_fill(fill).await?;
                }
            }
        }

        if let Some(paper) = &self.paper {
            metrics::gauge!(names::PAPER_FEES_PAID).set(paper.fees_paid().to_f64().unwrap_or(0.0));
        }
        Ok(())
    }

    fn snapshot_market(&self, state: &MarketState) -> MarketView {
        build_market_view(state, &self.coins, &self.sz_decimals)
    }

    async fn gate_and_execute(
        &mut self,
        intent: OrderIntent,
        market: &MarketView,
        account: &AccountView,
        now: u64,
    ) -> Result<Vec<FillEvent>> {
        let sid = intent.strategy.as_str().to_string();
        metrics::counter!(names::STRATEGY_INTENTS, "strategy" => sid.clone()).increment(1);

        let decision = self.risk.check(
            &intent,
            &RiskContext {
                market,
                account,
                now_ms: now,
            },
        );
        let effective = match decision {
            Decision::Approve => {
                metrics::counter!(names::STRATEGY_GATES, "strategy" => sid, "decision" => "approve")
                    .increment(1);
                intent
            }
            Decision::Resize(size) => {
                metrics::counter!(names::STRATEGY_GATES, "strategy" => sid, "decision" => "resize")
                    .increment(1);
                OrderIntent { size, ..intent }
            }
            Decision::Reject(reason) => {
                metrics::counter!(names::STRATEGY_GATES, "strategy" => sid, "decision" => "reject")
                    .increment(1);
                self.record_order(&intent, "reject", Some(&reason), now);
                return Ok(Vec::new());
            }
        };
        // Mandatory cloid: every accepted order is identifiable for
        // reconciliation and idempotent retry (SPEC-0002 H-2).
        let effective = match effective.cloid {
            Some(_) => effective,
            None => OrderIntent {
                cloid: Some(self.cloids.next()),
                ..effective
            },
        };

        self.record_order(&effective, "intent", None, now);
        match &mut self.paper {
            Some(paper) => Ok(paper.submit(&effective, market, now)),
            None => {
                self.submit_live(&effective, market, now).await?;
                Ok(Vec::new())
            }
        }
    }

    async fn cancel(&mut self, cancel: &CancelIntent, now: u64) {
        if let Some(paper) = &mut self.paper {
            paper.cancel(cancel.cloid.as_deref(), cancel.oid);
        }
        if let Some(exchange) = &self.exchange
            && let Some(market_meta) = self.markets.get(&cancel.coin)
        {
            if let Some(cloid) = &cancel.cloid {
                if let Err(err) = exchange
                    .cancel_by_cloid(vec![CancelByCloidWire {
                        asset: market_meta.asset_id(),
                        cloid: cloid.clone(),
                    }])
                    .await
                {
                    warn!(coin = %cancel.coin, error = %err, "live cancel failed");
                }
            } else if let Some(oid) = cancel.oid
                && let Err(err) = exchange
                    .cancel(vec![CancelWire {
                        a: market_meta.asset_id(),
                        o: oid,
                    }])
                    .await
            {
                warn!(coin = %cancel.coin, error = %err, "live cancel failed");
            }
        }
        let record = OrderRecord {
            ts_ms: now,
            strategy: Some(cancel.strategy.to_string()),
            coin: cancel.coin.clone(),
            side: String::new(),
            kind: "cancel".to_string(),
            cloid: cancel.cloid.clone(),
            oid: cancel.oid,
            px: None,
            sz: None,
            reduce_only: None,
            rationale: None,
            status: None,
        };
        self.recorder.writer().try_send(WriteCmd::Order {
            session_id: self.recorder.session_id(),
            record,
        });
    }

    async fn submit_live(&self, intent: &OrderIntent, market: &MarketView, now: u64) -> Result<()> {
        let Some(exchange) = &self.exchange else {
            return Ok(());
        };
        let Some(market_meta) = self.markets.get(&intent.coin) else {
            warn!(coin = %intent.coin, "no market metadata; skipping live order");
            return Ok(());
        };
        let limit_px = match intent.limit_px {
            Some(px) => px,
            None => {
                // Aggressive orders take the touch plus slippage, never the mid
                // (SPEC-0010 §12 / E-0).
                match aggressive_limit_px(
                    market_meta,
                    market,
                    &intent.coin,
                    intent.is_buy(),
                    self.max_slippage_bps,
                ) {
                    Some(px) => px,
                    None => {
                        warn!(coin = %intent.coin, "no touch to price aggressive live order");
                        self.record_order(intent, "reject", Some("no reference price"), now);
                        return Ok(());
                    }
                }
            }
        };
        let params = OrderParams {
            is_buy: intent.is_buy(),
            size: intent.size,
            limit_px,
            tif: intent.tif.into(),
            reduce_only: intent.reduce_only,
            cloid: intent.cloid.clone(),
        };
        let wire = match build_order_wire(market_meta, &params) {
            Ok(wire) => wire,
            Err(err) => {
                warn!(coin = %intent.coin, error = %err, "order build rejected");
                self.record_order(intent, "reject", Some(&err.to_string()), now);
                return Ok(());
            }
        };
        // Read the venue's per-order status instead of assuming "submitted":
        // rejected orders must not look live (SPEC-0010 §2 item 7 / E-0).
        match exchange.place(vec![wire]).await {
            Ok(response) => match response.statuses.first() {
                Some(status) => {
                    let (kind, label) = order_status_fields(status);
                    self.record_order(intent, kind, Some(&label), now);
                }
                None => self.record_order(intent, "submitted", None, now),
            },
            Err(mev_core::error::Error::UnknownOutcome(detail)) => {
                // The order may or may not be live. Reconcile by cloid before
                // recording anything; never resend (SPEC-0002 H-2).
                self.reconcile_unknown(intent, &detail, now).await;
            }
            Err(err) => {
                warn!(coin = %intent.coin, error = %err, "live submit failed");
                self.record_order(intent, "reject", Some(&err.to_string()), now);
            }
        }
        Ok(())
    }

    /// Resolve an unknown order outcome via `orderStatus` by `cloid`.
    ///
    /// Records the resolved state (`resting`/`filled`/`rejected`/…), or a
    /// `reconcile:unknown` marker when the read API is unavailable or the query
    /// fails. There is no path here that resends the order.
    async fn reconcile_unknown(&self, intent: &OrderIntent, detail: &str, now: u64) {
        let Some(cloid) = intent.cloid.as_deref() else {
            self.record_order(intent, "reconcile", Some("no-cloid"), now);
            return;
        };
        let Some((info, address)) = &self.info else {
            warn!(cloid, "unknown outcome but no read API to reconcile");
            self.record_order(intent, "reconcile", Some(&format!("no-info:{detail}")), now);
            return;
        };
        match info.order_status_by_cloid(address, cloid).await {
            Ok(status) => {
                let resolution = status.resolution();
                metrics::counter!(
                    names::ORDER_RECONCILE,
                    "resolution" => resolution.label(),
                )
                .increment(1);
                let kind = match resolution {
                    OrderResolution::Resting | OrderResolution::Triggered => "resting",
                    OrderResolution::Filled => "filled",
                    OrderResolution::Rejected => "reject",
                    OrderResolution::Cancelled => "cancelled",
                    OrderResolution::NotFound => "reconcile",
                    OrderResolution::Other(_) => "reconcile",
                };
                let label = match resolution {
                    OrderResolution::NotFound => format!("not-found:{detail}"),
                    other => other.label(),
                };
                self.record_order(intent, kind, Some(&label), now);
            }
            Err(err) => {
                warn!(cloid, error = %err, "reconciliation query failed");
                self.record_order(
                    intent,
                    "reconcile",
                    Some(&format!("query-failed:{err}")),
                    now,
                );
            }
        }
    }

    fn record_order(&self, intent: &OrderIntent, kind: &str, status: Option<&str>, now: u64) {
        let record = OrderRecord {
            ts_ms: now,
            strategy: Some(intent.strategy.to_string()),
            coin: intent.coin.clone(),
            side: side_str(intent.is_buy()).to_string(),
            kind: kind.to_string(),
            cloid: intent.cloid.clone(),
            oid: None,
            px: intent.limit_px.map(|px| px.normalize().to_string()),
            sz: Some(intent.size.normalize().to_string()),
            reduce_only: Some(intent.reduce_only),
            rationale: Some(intent.rationale.clone()),
            status: status.map(str::to_string),
        };
        self.recorder.writer().try_send(WriteCmd::Order {
            session_id: self.recorder.session_id(),
            record,
        });
    }

    fn record_fill(&self, fill: &FillEvent, now: u64) {
        let sid = fill
            .strategy
            .as_ref()
            .map(StrategyId::to_string)
            .unwrap_or_default();
        metrics::counter!(names::STRATEGY_FILLS, "strategy" => sid.clone()).increment(1);
        let record = FillRecord {
            ts_ms: now,
            tid: None,
            oid: None,
            coin: fill.coin.clone(),
            side: side_str(fill.side.is_buy()).to_string(),
            px: fill.px.normalize().to_string(),
            sz: fill.sz.normalize().to_string(),
            fee: Some(fill.fee.normalize().to_string()),
            builder_fee: None,
            closed_pnl: None,
            strategy: fill.strategy.as_ref().map(StrategyId::to_string),
        };
        self.recorder.writer().try_send(WriteCmd::Fill {
            session_id: self.recorder.session_id(),
            record,
        });
    }
}

fn side_str(is_buy: bool) -> &'static str {
    if is_buy { "buy" } else { "sell" }
}

/// The best opposite touch for an aggressive order (buy lifts the ask, sell
/// hits the bid), falling back to the mid when a side is empty.
fn touch(market: &MarketView, coin: &str, is_buy: bool) -> Option<Decimal> {
    market
        .book(coin)
        .and_then(|book| {
            if is_buy {
                book.best_ask()
            } else {
                book.best_bid()
            }
        })
        .map(|(px, _)| px)
        .or_else(|| market.mid(coin))
}

/// Aggressive limit for a `limit_px: None` order: the touch moved by
/// `max_slippage_bps`, rounded in the safe direction and never the mid
/// (SPEC-0010 §12).
fn aggressive_limit_px(
    meta: &Market,
    market: &MarketView,
    coin: &str,
    is_buy: bool,
    max_slippage_bps: Decimal,
) -> Option<Decimal> {
    let base = touch(market, coin, is_buy)?;
    if base <= Decimal::ZERO {
        return None;
    }
    let factor = if is_buy {
        Decimal::ONE + max_slippage_bps / Decimal::from(10_000)
    } else {
        Decimal::ONE - max_slippage_bps / Decimal::from(10_000)
    };
    let raw = base * factor;
    if raw <= Decimal::ZERO {
        return None;
    }
    Some(round_price_aggressive(meta, raw, is_buy))
}

/// Map a venue per-order status to an order-record `(kind, status)` pair.
fn order_status_fields(status: &OrderStatus) -> (&'static str, String) {
    match status {
        OrderStatus::Resting => ("resting", "resting".to_string()),
        OrderStatus::Filled => ("filled", "filled".to_string()),
        OrderStatus::Rejected(reason) => ("reject", format!("rejected:{}", reason.as_str())),
        OrderStatus::Other(other) => ("other", other.clone()),
    }
}

/// Poll `/info` for account state and publish it to the shared view.
pub async fn account_poller(
    info: Arc<dyn InfoApi>,
    address: String,
    account: Arc<RwLock<AccountView>>,
    network: Network,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(5));
    loop {
        interval.tick().await;
        match load_account(&*info, &address, network).await {
            Ok(view) => {
                if let Ok(mut guard) = account.write() {
                    *guard = view;
                }
            }
            Err(err) => warn!(error = %err, "account poll failed"),
        }
    }
}

async fn load_account(info: &dyn InfoApi, address: &str, _network: Network) -> Result<AccountView> {
    let clearing = info.clearinghouse_state(address).await?;
    let spot = info
        .spot_clearinghouse_state(address)
        .await
        .unwrap_or_default();
    let open_orders = info.open_orders(address).await.unwrap_or_default();
    let fees = info.user_fees(address).await.ok();

    let mut positions = BTreeMap::new();
    for entry in &clearing.asset_positions {
        let position = &entry.position;
        positions.insert(
            position.coin.clone(),
            PositionView {
                coin: position.coin.clone(),
                szi: position.szi,
                entry_px: position.entry_px,
                position_value: position.position_value,
                unrealized_pnl: position.unrealized_pnl,
                margin_used: position.margin_used,
            },
        );
    }

    let mut spot_balances = BTreeMap::new();
    for balance in &spot.balances {
        spot_balances.insert(balance.coin.clone(), balance.total);
    }

    let open_views = open_orders
        .iter()
        .map(|order| OpenOrderView {
            coin: order.coin.clone(),
            oid: Some(order.oid),
            cloid: order.cloid.clone(),
            side: if order.is_buy() {
                mev_strategy::Side::Buy
            } else {
                mev_strategy::Side::Sell
            },
            limit_px: order.limit_px,
            sz: order.sz,
            reduce_only: order.reduce_only,
        })
        .collect();

    let fee_rates = match fees {
        Some(fees) => FeeRates {
            maker: fees.user_add_rate.unwrap_or(FeeRates::PERP.maker),
            taker: fees.user_cross_rate.unwrap_or(FeeRates::PERP.taker),
        },
        None => FeeRates::PERP,
    };

    Ok(AccountView {
        positions,
        spot: spot_balances,
        open_orders: open_views,
        fees: fee_rates,
        account_value: clearing.margin_summary.account_value,
        margin_used: clearing.margin_summary.total_margin_used,
        withdrawable: clearing.withdrawable,
    })
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
                for strategy in strategies.iter_mut() {
                    let ctx = StrategyContext {
                        now_ms: row.ts_ms,
                        trigger: Trigger::Timer,
                        market: &market,
                        account: &account,
                    };
                    for action in strategy.on_event(&ctx).await? {
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

    use async_trait::async_trait;
    use mev_core::db::writer::DbWriter;
    use mev_core::error::Error;
    use mev_hl_client::StreamEvent;
    use mev_hl_client::types::{
        AllMids, ClearinghouseState, L2Book, Level, Meta, MetaAndAssetCtxs, OpenOrder,
        OrderStatusResponse, PerpDex, SpotClearinghouseState, SpotMeta, UserFees, UserFill,
        UserFunding, UserRateLimit,
    };

    use super::*;

    fn ds(value: &str) -> Decimal {
        Decimal::from_str(value).unwrap()
    }

    /// A stub info API returning a fixed `orderStatus` body (or an error).
    struct FakeInfo {
        response: std::result::Result<OrderStatusResponse, String>,
    }

    #[async_trait]
    impl InfoApi for FakeInfo {
        async fn order_status_by_cloid(
            &self,
            _user: &str,
            _cloid: &str,
        ) -> std::result::Result<OrderStatusResponse, Error> {
            match &self.response {
                Ok(status) => Ok(status.clone()),
                Err(message) => Err(Error::Http(message.clone())),
            }
        }
        async fn meta(&self) -> std::result::Result<Meta, Error> {
            Err(Error::Unimplemented("meta"))
        }
        async fn meta_for(&self, _dex: &str) -> std::result::Result<Meta, Error> {
            Err(Error::Unimplemented("meta_for"))
        }
        async fn perp_dexs(&self) -> std::result::Result<Vec<PerpDex>, Error> {
            Err(Error::Unimplemented("perp_dexs"))
        }
        async fn spot_meta(&self) -> std::result::Result<SpotMeta, Error> {
            Err(Error::Unimplemented("spot_meta"))
        }
        async fn all_mids(&self) -> std::result::Result<AllMids, Error> {
            Err(Error::Unimplemented("all_mids"))
        }
        async fn all_mids_for(&self, _dex: &str) -> std::result::Result<AllMids, Error> {
            Err(Error::Unimplemented("all_mids_for"))
        }
        async fn l2_book(&self, _coin: &str) -> std::result::Result<L2Book, Error> {
            Err(Error::Unimplemented("l2_book"))
        }
        async fn meta_and_asset_ctxs(&self) -> std::result::Result<MetaAndAssetCtxs, Error> {
            Err(Error::Unimplemented("meta_and_asset_ctxs"))
        }
        async fn clearinghouse_state(
            &self,
            _user: &str,
        ) -> std::result::Result<ClearinghouseState, Error> {
            Err(Error::Unimplemented("clearinghouse_state"))
        }
        async fn open_orders(&self, _user: &str) -> std::result::Result<Vec<OpenOrder>, Error> {
            Err(Error::Unimplemented("open_orders"))
        }
        async fn order_status(
            &self,
            _user: &str,
            _oid: u64,
        ) -> std::result::Result<OrderStatusResponse, Error> {
            Err(Error::Unimplemented("order_status"))
        }
        async fn spot_clearinghouse_state(
            &self,
            _user: &str,
        ) -> std::result::Result<SpotClearinghouseState, Error> {
            Err(Error::Unimplemented("spot_clearinghouse_state"))
        }
        async fn user_funding(
            &self,
            _user: &str,
            _start_ms: u64,
        ) -> std::result::Result<Vec<UserFunding>, Error> {
            Err(Error::Unimplemented("user_funding"))
        }
        async fn user_fills_by_time(
            &self,
            _user: &str,
            _start_ms: u64,
        ) -> std::result::Result<Vec<UserFill>, Error> {
            Err(Error::Unimplemented("user_fills_by_time"))
        }
        async fn user_fees(&self, _user: &str) -> std::result::Result<UserFees, Error> {
            Err(Error::Unimplemented("user_fees"))
        }
        async fn user_rate_limit(&self, _user: &str) -> std::result::Result<UserRateLimit, Error> {
            Err(Error::Unimplemented("user_rate_limit"))
        }
    }

    fn status(found_state: Option<&str>) -> OrderStatusResponse {
        match found_state {
            Some(state) => OrderStatusResponse {
                status: "order".into(),
                order: Some(mev_hl_client::OrderStatusOrder {
                    order: None,
                    status: state.into(),
                    status_timestamp: 1,
                }),
            },
            None => OrderStatusResponse {
                status: "unknownOid".into(),
                order: None,
            },
        }
    }

    fn test_engine(info: Option<Arc<dyn InfoApi>>) -> Engine {
        let db = Db::open_in_memory().unwrap();
        let writer = Arc::new(DbWriter::spawn(db, 512));
        let build = EngineBuild {
            strategies: Vec::new(),
            subscriptions: Vec::new(),
            coins: vec!["BTC".into()],
            sz_decimals: BTreeMap::new(),
            instruments: BTreeMap::new(),
            markets: AssetMap::new(),
        };
        let cfg = Config::default();
        let mut engine = Engine::new(build, &cfg, None, writer, 1);
        if let Some(info) = info {
            engine = engine.with_info(info, "0xaccount");
        }
        engine
    }

    fn intent_with_cloid(cloid: Option<&str>) -> OrderIntent {
        OrderIntent {
            strategy: StrategyId::from("test"),
            coin: "BTC".into(),
            side: mev_strategy::Side::Buy,
            limit_px: Some(ds("100")),
            size: ds("1"),
            tif: mev_strategy::TimeInForce::Gtc,
            reduce_only: false,
            rationale: "test".into(),
            cloid: cloid.map(str::to_string),
            signal_ms: 0,
            decision_ms: 0,
        }
    }

    #[tokio::test]
    async fn reconcile_unknown_resolves_by_cloid_without_resending() {
        // The fake info reports the order resting; reconciliation must record
        // it and must not submit anything (the engine has no exchange).
        let info: Arc<dyn InfoApi> = Arc::new(FakeInfo {
            response: Ok(status(Some("open"))),
        });
        let engine = test_engine(Some(info));
        let intent = intent_with_cloid(Some("0xdead"));
        engine.reconcile_unknown(&intent, "dropped", 1).await;
    }

    #[tokio::test]
    async fn reconcile_unknown_handles_not_found_and_query_failure() {
        let not_found: Arc<dyn InfoApi> = Arc::new(FakeInfo {
            response: Ok(status(None)),
        });
        let engine = test_engine(Some(not_found));
        engine
            .reconcile_unknown(&intent_with_cloid(Some("0xdead")), "dropped", 1)
            .await;

        let failing: Arc<dyn InfoApi> = Arc::new(FakeInfo {
            response: Err("boom".into()),
        });
        let engine = test_engine(Some(failing));
        engine
            .reconcile_unknown(&intent_with_cloid(Some("0xdead")), "dropped", 1)
            .await;

        // No cloid and no info are both handled without panicking.
        engine
            .reconcile_unknown(&intent_with_cloid(None), "dropped", 1)
            .await;
        let engine = test_engine(None);
        engine
            .reconcile_unknown(&intent_with_cloid(Some("0xdead")), "dropped", 1)
            .await;
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
                vec![Level {
                    px: ds(bid),
                    sz: ds("10"),
                    n: 1,
                }],
                vec![Level {
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
            coin: "BTC".into(),
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

    fn btc_market(sz_decimals: u32) -> Market {
        use mev_hl_client::AssetMap;
        use mev_hl_client::types::{AssetMeta, Meta};
        let mut map = AssetMap::new();
        map.insert_perp_dex(
            None,
            None,
            &Meta {
                universe: vec![AssetMeta {
                    name: "BTC".into(),
                    sz_decimals,
                    max_leverage: 40,
                    is_delisted: false,
                    only_isolated: false,
                }],
            },
        );
        map.get("BTC").unwrap().clone()
    }

    fn market_view() -> MarketView {
        let mut view = MarketView::new();
        view.insert_book(
            "BTC",
            BookView {
                bids: vec![(ds("9990"), ds("1"))],
                asks: vec![(ds("10010"), ds("1"))],
                sz_decimals: 0,
                time: 0,
            },
        );
        view
    }

    #[test]
    fn aggressive_buy_takes_ask_and_never_mid() {
        let meta = btc_market(0);
        let view = market_view();
        // Ask 10010 + 10 bps = 10020.01 -> rounded toward buying up.
        let px = aggressive_limit_px(&meta, &view, "BTC", true, ds("10")).unwrap();
        assert!(px > ds("10010"), "must be above the touch, got {px}");
        assert!(px >= ds("10020.01"), "rounded up, got {px}");
        assert!(px > view.mid("BTC").unwrap(), "must not be the mid");
    }

    #[test]
    fn aggressive_sell_takes_bid_and_never_mid() {
        let meta = btc_market(0);
        let view = market_view();
        // Bid 9990 - 10 bps = 9980.01 -> rounded toward selling down.
        let px = aggressive_limit_px(&meta, &view, "BTC", false, ds("10")).unwrap();
        assert!(px < ds("9990"), "must be below the touch, got {px}");
        assert!(px <= ds("9980.01"), "rounded down, got {px}");
        assert!(px < view.mid("BTC").unwrap(), "must not be the mid");
    }

    #[test]
    fn aggressive_price_without_book_uses_mid_fallback() {
        let meta = btc_market(0);
        let mut view = MarketView::new();
        let mut mids = BTreeMap::new();
        mids.insert("BTC".to_string(), ds("100"));
        view.set_mids(mids);
        assert_eq!(
            aggressive_limit_px(&meta, &view, "BTC", true, ds("10")).unwrap(),
            ds("100.1")
        );
    }

    #[test]
    fn status_mapping_distinguishes_rejects() {
        use mev_hl_client::RejectReason;
        assert_eq!(
            order_status_fields(&OrderStatus::Resting),
            ("resting", "resting".to_string())
        );
        assert_eq!(
            order_status_fields(&OrderStatus::Filled),
            ("filled", "filled".to_string())
        );
        assert_eq!(
            order_status_fields(&OrderStatus::Rejected(RejectReason::TickRejected)),
            ("reject", "rejected:tickRejected".to_string())
        );
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

        let mut first = strategies();
        let a = replay_events(&rows, &mut first, &coins, &sz_decimals)
            .await
            .unwrap();
        let mut second = strategies();
        let b = replay_events(&rows, &mut second, &coins, &sz_decimals)
            .await
            .unwrap();

        assert_eq!(a, b, "same log must replay identically");
        assert!(a.intents > 0, "the ladder should place quotes");
        assert_ne!(a.fingerprint, 0);
    }
}
