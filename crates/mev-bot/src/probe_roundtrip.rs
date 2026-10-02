//! `hl probe testnet-roundtrip` — the SPEC-0002 H-10 harness.
//!
//! One controlled round trip against Hyperliquid **testnet only**: read the
//! mid, size a far-from-mid passive buy, arm the dead-man's switch (H-4), place
//! an ALO order with a mandatory `cloid`, confirm it rests via both `openOrders`
//! (info REST) and the `orderUpdates` stream (H-3), cancel it by `cloid`, and
//! confirm it is gone. Any failure after the place still cancels the order; the
//! dead-man switch is disarmed only once the order is verified gone.
//!
//! The key comes only from the process environment through [`Config`]; this
//! command never reads a `.env` file and never logs key or signature material.
//! It refuses on any network that is not [`Network::Testnet`] before any key is
//! used, a signer is built, or a socket is opened. (`Config::load` may already
//! hold the key in a `SecretString`; it is not used until the guard passes.)
//!
//! Risk: the place is approved through the same hot-path [`RiskGate`] the live
//! engine uses (SPEC-0010 §11), so there is no risk-gate bypass. The gate's
//! context here is **synthetic**: an empty `OrderManager`/`AccountState` and a
//! one-level book, so effectively only the per-order notional cap, the rate
//! budget, the minimum-notional check, and the (absent) tick check apply;
//! position-exposure and margin limits see zero state, and this `KillSwitch`
//! does not consult `data/KILL` (the live control task owns that).

use std::{future::Future, str::FromStr, sync::Arc, time::Duration, time::Instant};

use anyhow::Result as AnyhowResult;
use clap::Args;
use mev_core::{
    config::{Config, ConfigOverrides, Mode, Network},
    db::Db,
    error::{Error, Result},
};
use mev_engine::{
    AccountState, Action as EngineAction, AssetMeta, CoinId, Level, MarketSlot, OrderManager,
    Stamp,
    risk::{RateBudget, RiskCtx, RiskGate},
};
use mev_hl_client::{
    ActionResponse, AgentSigner, AssetMap, CancelByCloidWire, CancelWire, CloidFactory,
    DeadMansSwitch, ExchangeApi, HttpInfo, InfoApi, MIN_ORDER_NOTIONAL, Market, MarketSelector,
    MarketStream, OrderParams, OrderStatus, StreamEvent, Subscription, Tif, WsExchange,
    WsMarketStream, WsOrder, build_order_wire, now_ms, round_price_with,
};
use mev_metrics::names;
use mev_strategy::{OrderIntent, Side as StrategySide, StrategyId, TimeInForce};
use rust_decimal::{Decimal, RoundingStrategy};
use serde_json::Value;

/// Default coin for the round trip.
const DEFAULT_COIN: &str = "BTC";
/// Minimum distance below the mid, in basis points.
const MIN_OFFSET_BPS: u32 = 50;
/// Default distance below the mid, in basis points (10%).
const DEFAULT_OFFSET_BPS: u32 = 1_000;
/// Default target notional in USD (just above the venue's $10 minimum).
const DEFAULT_NOTIONAL_USD: &str = "11";
/// Default per-confirmation timeout.
const DEFAULT_CONFIRM_TIMEOUT_MS: u64 = 10_000;
/// Timeout on every individual `/info` call (SPEC-0002 H-10 review).
const INFO_TIMEOUT: Duration = Duration::from_secs(5);
/// Poll interval while waiting on `openOrders`.
const OPEN_ORDERS_POLL: Duration = Duration::from_millis(250);
/// Bounded window for the final "is it gone?" `openOrders` verification.
const VERIFY_TIMEOUT: Duration = Duration::from_secs(5);

/// Arguments for `hl probe testnet-roundtrip` (SPEC-0002 H-10).
#[derive(Args, Debug, Clone)]
pub struct RoundtripArgs {
    /// Market symbol (default: BTC).
    #[arg(long, default_value = DEFAULT_COIN)]
    pub coin: String,
    /// Distance below the mid in basis points (default: 1000 = 10%; minimum 50).
    #[arg(long, default_value_t = DEFAULT_OFFSET_BPS)]
    pub offset_bps: u32,
    /// Target order notional in USD (default: 11; must be >= the venue minimum).
    #[arg(long, default_value = DEFAULT_NOTIONAL_USD)]
    pub notional_usd: String,
    /// Per-confirmation timeout in milliseconds (default: 10000).
    #[arg(long, default_value_t = DEFAULT_CONFIRM_TIMEOUT_MS)]
    pub confirm_timeout_ms: u64,
}

/// Tunables for one probe run.
#[derive(Debug, Clone)]
struct ProbeParams {
    offset_bps: u32,
    notional_usd: Decimal,
    confirm_timeout: Duration,
    /// Bound on the final `openOrders` verification in cleanup.
    verify_timeout: Duration,
    ttl_ms: u64,
    /// Deterministic cloid override (tests); `None` generates a fresh one.
    cloid: Option<String>,
}

/// Everything one round trip needs, injected so tests use mocks and no network.
struct Probe {
    network: Network,
    agent: String,
    market: Market,
    params: ProbeParams,
    exchange: Arc<dyn ExchangeApi>,
    info: Arc<dyn InfoApi>,
    updates: Box<dyn MarketStream>,
    risk: RiskGate,
    rate: RateBudget,
}

/// A successful round-trip report (all timings are elapsed local durations).
#[derive(Debug, Clone)]
pub struct Report {
    /// Network the round trip ran on (always [`Network::Testnet`]).
    pub network: Network,
    /// The agent account address.
    pub agent: String,
    /// Coin traded.
    pub coin: String,
    /// Mid read at the start.
    pub mid: Decimal,
    /// Passive limit price placed.
    pub price: Decimal,
    /// Order size placed.
    pub size: Decimal,
    /// Offset below the mid, in basis points.
    pub offset_bps: u32,
    /// The client order id used.
    pub cloid: String,
    /// Submit-to-ack sample (H-7 `hl_submit_ack_seconds`).
    pub submit_ack: Duration,
    /// Time for the resting order to appear in `openOrders`.
    pub open_resting: Duration,
    /// Time for the resting order to appear on `orderUpdates`.
    pub updates_resting: Duration,
    /// Time for the cancelled order to clear `openOrders`.
    pub open_gone: Duration,
    /// Time for the cancellation to appear on `orderUpdates`.
    pub updates_cancelled: Duration,
}

/// Run the testnet round-trip, refusing on any non-testnet network.
///
/// Loads the resolved [`Config`] (for the agent key, account, risk limits, and
/// dead-man TTL), asserts the network is testnet **before any key is used, a
/// signer is built, or a socket is opened**, then performs the round trip and
/// prints the report. Any error exits non-zero through `main`.
pub async fn run(network: Option<Network>, args: RoundtripArgs) -> AnyhowResult<()> {
    let config = Config::load(ConfigOverrides {
        network,
        ..Default::default()
    })?;
    ensure_testnet(config.network)?;
    let report = execute_live(&config, args).await?;
    print_report(&report);
    Ok(())
}

/// Refuse any network that is not testnet with a typed config error.
///
/// This is deliberately separate from client construction so it runs before any
/// key is used, a signer is built, or a socket is opened.
fn ensure_testnet(network: Network) -> Result<()> {
    if network != Network::Testnet {
        return Err(Error::Config(format!(
            "hl probe testnet-roundtrip only runs on testnet, refusing {network:?}"
        )));
    }
    Ok(())
}

/// Build the live clients and run the round trip against testnet.
async fn execute_live(config: &Config, args: RoundtripArgs) -> Result<Report> {
    let key = config
        .agent_key()
        .ok_or_else(|| Error::Config("set HL_AGENT_PRIVATE_KEY to run the probe".into()))?;
    let agent = config
        .account_address
        .clone()
        .ok_or_else(|| Error::Config("set HL_ACCOUNT_ADDRESS to run the probe".into()))?;
    // Testnet signing: `mainnet = false`.
    let signer = AgentSigner::from_hex(key, false)?;

    // Hold the nonce database lock, like `hl run`, so the probe cannot race a
    // running bot (SPEC-0002 H-6).
    let _db_lock = crate::db_lock::DbLock::acquire(&config.db_path)
        .map_err(|err| Error::Config(format!("{err:#}")))?;
    let db = Arc::new(std::sync::Mutex::new(Db::open(&config.db_path)?));
    let exchange: Arc<dyn ExchangeApi> =
        Arc::new(WsExchange::new(Network::Testnet, Mode::Live, Some(signer))?.with_nonce_db(db)?);

    let http = HttpInfo::new(Network::Testnet);
    let include_hip3 = args.coin.contains(':');
    let map = with_timeout(
        "asset metadata",
        INFO_TIMEOUT,
        AssetMap::load(&http, include_hip3),
    )
    .await?;
    let market = MarketSelector::new(map).resolve(&args.coin)?;
    let info: Arc<dyn InfoApi> = Arc::new(http);

    // Subscribe to `orderUpdates` BEFORE placing, so the stream is live when
    // the order lands (SPEC-0002 H-3).
    let updates = with_timeout(
        "orderUpdates connect",
        INFO_TIMEOUT,
        WsMarketStream::connect(
            Network::Testnet,
            &[Subscription::OrderUpdates {
                user: agent.clone(),
            }],
        ),
    )
    .await?;

    let notional_usd = Decimal::from_str(&args.notional_usd).map_err(|err| {
        Error::Config(format!(
            "invalid --notional-usd `{}`: {err}",
            args.notional_usd
        ))
    })?;
    let params = ProbeParams {
        offset_bps: args.offset_bps,
        notional_usd,
        confirm_timeout: Duration::from_millis(args.confirm_timeout_ms.max(1)),
        verify_timeout: VERIFY_TIMEOUT,
        ttl_ms: config.schedule_cancel_ttl_ms,
        cloid: None,
    };

    let mut probe = Probe {
        network: Network::Testnet,
        agent,
        market,
        params,
        exchange,
        info,
        updates: Box::new(updates),
        risk: RiskGate::from_settings(&config.risk),
        rate: RateBudget::from_settings(&config.risk.rate_budget),
    };
    execute(&mut probe).await
}

/// The network-independent core, driven by injected clients.
async fn execute(probe: &mut Probe) -> Result<Report> {
    let book = with_timeout(
        "l2_book",
        INFO_TIMEOUT,
        probe.info.l2_book(&probe.market.coin),
    )
    .await?;
    let mid = book
        .mid()
        .ok_or_else(|| Error::Config(format!("no two-sided mid for {}", probe.market.coin)))?;
    let (price, size) = passive_buy(
        &probe.market,
        mid,
        probe.params.offset_bps,
        probe.params.notional_usd,
    )?;
    // Route the place through the live risk gate; never bypass it.
    let size = risk_approve(&mut probe.risk, &probe.rate, &probe.market, price, size)?;
    let cloid = match probe.params.cloid.clone() {
        Some(cloid) => cloid,
        None => CloidFactory::new().next(),
    };

    // Build the wire first, so a bad order cannot leave the venue armed.
    let wire = build_order_wire(
        &probe.market,
        &OrderParams {
            is_buy: true,
            size,
            limit_px: price,
            tif: Tif::Alo,
            reduce_only: false,
            cloid: Some(cloid.clone()),
        },
    )?;

    // 2. Arm the dead-man's switch BEFORE placing, so a crash cannot leave a
    // resting order (SPEC-0002 H-4).
    let mut switch = DeadMansSwitch::new(probe.params.ttl_ms);
    let arm = switch.arm(now_ms());
    probe.exchange.submit(&arm).await?;

    // 3. Place the ALO order and time submit-to-ack (SPEC-0002 H-7).
    let started = Instant::now();
    let placed = probe.exchange.place(vec![wire]).await;
    let submit_ack = started.elapsed();
    metrics::histogram!(names::SUBMIT_ACK_SECONDS, "transport" => "probe")
        .record(submit_ack.as_secs_f64());
    let placed = match placed {
        Ok(response) => response,
        Err(err) => {
            // An `UnknownOutcome` place may land late: cleanup must keep the
            // switch armed even if the order is absent now.
            let unknown = matches!(err, Error::UnknownOutcome(_));
            return cleanup_or(probe, &cloid, None, unknown, &mut switch, err).await;
        }
    };
    if !placed
        .statuses
        .iter()
        .any(|status| matches!(status, OrderStatus::Resting))
    {
        let err = Error::Config(format!("order was not resting: {:?}", placed.statuses));
        return cleanup_or(probe, &cloid, None, false, &mut switch, err).await;
    }

    // 4-5. Confirm resting, cancel by cloid, confirm gone. The venue `oid` is
    // captured as soon as it is known so cleanup has an oid-cancel fallback.
    let mut known_oid = None;
    let result =
        confirm_roundtrip(probe, &cloid, mid, price, size, submit_ack, &mut known_oid).await;
    match result {
        Ok(report) => {
            // The order is verified gone; disarming is safe. A failure is not
            // fatal: the venue expires the schedule on its own.
            if let Err(err) = disarm(probe, &mut switch).await {
                tracing::warn!(error = %err, "dead-man disarm failed; the venue will expire it");
            }
            Ok(report)
        }
        // Any failure after the place still cancels by cloid and verifies gone.
        Err(err) => cleanup_or(probe, &cloid, known_oid, false, &mut switch, err).await,
    }
}

/// Run cleanup and preserve its (louder) outcome when it fails.
///
/// Never discards the cleanup result: if cleanup cannot verify the order is
/// gone, its error (with the dead-man still armed) is what the caller sees.
async fn cleanup_or(
    probe: &mut Probe,
    cloid: &str,
    oid: Option<u64>,
    outcome_unknown: bool,
    switch: &mut DeadMansSwitch,
    original: Error,
) -> Result<Report> {
    match cleanup(probe, cloid, oid, outcome_unknown, switch).await {
        Ok(()) => Err(original),
        Err(cleanup_err) => Err(cleanup_err),
    }
}

/// Confirm the order rests, cancel it by cloid, and confirm it is gone.
async fn confirm_roundtrip(
    probe: &mut Probe,
    cloid: &str,
    mid: Decimal,
    price: Decimal,
    size: Decimal,
    submit_ack: Duration,
    known_oid: &mut Option<u64>,
) -> Result<Report> {
    let timeout = probe.params.confirm_timeout;

    let started = Instant::now();
    let oid = wait_open_orders(probe.info.as_ref(), &probe.agent, cloid, true, timeout).await?;
    *known_oid = oid;
    let open_resting = started.elapsed();

    let started = Instant::now();
    await_order_status(probe.updates.as_mut(), cloid, Wanted::Resting, timeout).await?;
    let updates_resting = started.elapsed();

    let response = probe
        .exchange
        .cancel_by_cloid(vec![CancelByCloidWire {
            asset: probe.market.asset_id(),
            cloid: cloid.to_string(),
        }])
        .await?;
    if cancel_reply_has_error(&response) {
        return Err(Error::Exchange(format!(
            "cancel-by-cloid for {cloid} returned an error status"
        )));
    }

    let started = Instant::now();
    wait_open_orders(probe.info.as_ref(), &probe.agent, cloid, false, timeout).await?;
    let open_gone = started.elapsed();

    let started = Instant::now();
    await_order_status(probe.updates.as_mut(), cloid, Wanted::Cancelled, timeout).await?;
    let updates_cancelled = started.elapsed();

    Ok(Report {
        network: probe.network,
        agent: probe.agent.clone(),
        coin: probe.market.coin.clone(),
        mid,
        price,
        size,
        offset_bps: probe.params.offset_bps,
        cloid: cloid.to_string(),
        submit_ack,
        open_resting,
        updates_resting,
        open_gone,
        updates_cancelled,
    })
}

/// Cancel by cloid, fall back to an oid cancel, then verify `openOrders`.
///
/// The dead-man's switch is disarmed **only** when the order is verified gone
/// and the place outcome was not unknown; otherwise it is left armed so the
/// venue TTL cancels any late-landing order.
async fn cleanup(
    probe: &mut Probe,
    cloid: &str,
    oid: Option<u64>,
    outcome_unknown: bool,
    switch: &mut DeadMansSwitch,
) -> Result<()> {
    let asset = probe.market.asset_id();

    // (a)/(b): cancel by cloid, and parse the reply for a per-order error.
    let cancel_failed = match probe
        .exchange
        .cancel_by_cloid(vec![CancelByCloidWire {
            asset,
            cloid: cloid.to_string(),
        }])
        .await
    {
        Ok(response) => cancel_reply_has_error(&response),
        Err(_) => true,
    };
    if cancel_failed && let Some(oid) = oid {
        // A second cancel mechanism. Its reply is not parsed further: the
        // `openOrders` poll below is the source of truth for the disarm gate.
        let _ = probe
            .exchange
            .cancel(vec![CancelWire { a: asset, o: oid }])
            .await;
    }

    // (c): bounded `openOrders` verification (250 ms interval, retrying
    // transient info errors). Absent is the only state that permits a disarm.
    let gone = matches!(
        poll_open_orders(
            probe.info.as_ref(),
            &probe.agent,
            cloid,
            false,
            probe.params.verify_timeout,
        )
        .await,
        Poll::Satisfied(_)
    );
    let ttl_s = probe.params.ttl_ms / 1000;
    if !gone {
        return Err(Error::UnknownOutcome(format!(
            "order {cloid} may still be resting; dead-man left armed, expires in {ttl_s}s"
        )));
    }
    // (d): an unknown place outcome may still land; leave the switch armed.
    if outcome_unknown {
        return Err(Error::UnknownOutcome(format!(
            "place outcome for {cloid} is unknown and the order may land late; \
             dead-man left armed, expires in {ttl_s}s"
        )));
    }
    disarm(probe, switch).await
}

/// Disarm the dead-man's switch if it is armed.
async fn disarm(probe: &mut Probe, switch: &mut DeadMansSwitch) -> Result<()> {
    if let Some(action) = switch.disarm() {
        probe.exchange.submit(&action).await?;
    }
    Ok(())
}

/// Whether a cancel reply carries a per-order error status.
///
/// A top-level non-ok reply already surfaces as an `Err`; this catches a reply
/// that is `ok` overall but whose per-order status is an error (`{"error":...}`
/// or a known reject string). A reply without per-order statuses (e.g. a plain
/// cancel ack) is accepted.
fn cancel_reply_has_error(response: &ActionResponse) -> bool {
    let Some(statuses) = response
        .value
        .get("data")
        .and_then(|data| data.get("statuses"))
        .and_then(Value::as_array)
    else {
        return false;
    };
    statuses.iter().any(|entry| match entry {
        Value::Object(map) => map.contains_key("error"),
        Value::String(status) => matches!(OrderStatus::parse(status), OrderStatus::Rejected(_)),
        _ => false,
    })
}

/// Read the mid and compute a passive buy price and lot-rounded size.
///
/// The price is rounded **down** (toward zero) to the same significant-figure
/// and decimal caps [`build_order_wire`] uses, so it stays at least
/// `offset_bps` below the mid. The size is rounded **up** to `szDecimals` and
/// floored at the venue minimum notional.
fn passive_buy(
    market: &Market,
    mid: Decimal,
    offset_bps: u32,
    notional_usd: Decimal,
) -> Result<(Decimal, Decimal)> {
    if mid <= Decimal::ZERO {
        return Err(Error::Config(format!("mid {mid} is not positive")));
    }
    if notional_usd <= Decimal::ZERO {
        return Err(Error::Config(format!(
            "notional {notional_usd} is not positive"
        )));
    }
    if !(MIN_OFFSET_BPS..10_000).contains(&offset_bps) {
        return Err(Error::Config(format!(
            "offset-bps {offset_bps} must be between {MIN_OFFSET_BPS} and 9999"
        )));
    }
    let factor = Decimal::ONE - Decimal::from(offset_bps) / Decimal::from(10_000u32);
    let target = mid * factor;
    let price = round_price_with(market, target, RoundingStrategy::ToNegativeInfinity);
    if price <= Decimal::ZERO {
        return Err(Error::Config(format!("price rounds to {price}")));
    }
    let lot = (notional_usd / price)
        .round_dp_with_strategy(market.sz_decimals, RoundingStrategy::ToPositiveInfinity);
    let min = (MIN_ORDER_NOTIONAL / price)
        .round_dp_with_strategy(market.sz_decimals, RoundingStrategy::ToPositiveInfinity);
    let size = lot.max(min);
    if size <= Decimal::ZERO {
        return Err(Error::Config(format!("size rounds to {size}")));
    }
    Ok((price, size))
}

/// Approve the probe place through the live [`RiskGate`] (SPEC-0010 §11).
///
/// Uses the same gate, limits, and rate budget as the engine, but with a
/// synthetic context (empty `OrderManager`/`AccountState`, a one-level book), so
/// only the per-order notional cap, the rate budget, the minimum-notional check,
/// and the (absent) tick check are effective. The gate's own `KillSwitch` is
/// fresh and does not read `data/KILL`. Returns the possibly resized size.
fn risk_approve(
    gate: &mut RiskGate,
    rate: &RateBudget,
    market: &Market,
    price: Decimal,
    size: Decimal,
) -> Result<Decimal> {
    let meta = AssetMeta::from_market(market);
    let orders = OrderManager::new(1);
    let account = AccountState::new(1);
    let level = Level {
        px: price,
        sz: Decimal::ONE,
        n: 1,
    };
    let slot = MarketSlot {
        bbo: Some((Some(level), Some(level), Stamp::default())),
        ..Default::default()
    };
    let intent = OrderIntent {
        strategy: StrategyId::from("probe"),
        coin: market.coin.clone(),
        side: StrategySide::Buy,
        limit_px: Some(price),
        size,
        tif: TimeInForce::Alo,
        reduce_only: false,
        rationale: "testnet round-trip probe".to_string(),
        cloid: None,
        signal_ms: 0,
        decision_ms: 0,
    };
    let ctx = RiskCtx {
        coin: CoinId(0),
        orders: &orders,
        account: &account,
        slot: &slot,
        meta: &meta,
        rate,
        now_ms: now_ms(),
    };
    match gate.evaluate(&EngineAction::Place(intent), &ctx) {
        Ok(mev_risk::Decision::Approve) => Ok(size),
        Ok(mev_risk::Decision::Resize(resized)) => Ok(resized),
        Ok(mev_risk::Decision::Reject(reason)) => Err(Error::Config(format!(
            "risk gate rejected the probe order: {reason}"
        ))),
        Err(reason) => Err(Error::Config(format!(
            "risk gate rejected the probe order: {reason}"
        ))),
    }
}

/// Which `orderUpdates` state to wait for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wanted {
    Resting,
    Cancelled,
}

impl Wanted {
    /// Whether a venue status string satisfies this wait.
    ///
    /// The only user-cancel status we expect is `canceled`; system cancels
    /// (`scheduledCancel`, `marginCanceled`, …) are deliberately not accepted.
    fn matches(self, status: &str) -> bool {
        match self {
            Wanted::Resting => status == "open" || status == "resting",
            Wanted::Cancelled => status == "canceled",
        }
    }
}

/// Wait for an `orderUpdates` message for `cloid` in the wanted state.
async fn await_order_status(
    stream: &mut dyn MarketStream,
    cloid: &str,
    wanted: Wanted,
    timeout: Duration,
) -> Result<WsOrder> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(timeout_error(cloid, wanted));
        }
        let event = match tokio::time::timeout(remaining, stream.next()).await {
            Ok(Ok(event)) => event,
            Ok(Err(err)) => return Err(err),
            Err(_) => return Err(timeout_error(cloid, wanted)),
        };
        if let StreamEvent::OrderUpdates(orders) = event
            && let Some(order) = orders.into_iter().find(|order| {
                order.order.cloid.as_deref() == Some(cloid) && wanted.matches(&order.status)
            })
        {
            return Ok(order);
        }
    }
}

/// Typed timeout error for a missing `orderUpdates` confirmation.
fn timeout_error(cloid: &str, wanted: Wanted) -> Error {
    Error::UnknownOutcome(format!(
        "no orderUpdates {wanted:?} for cloid {cloid} within the timeout"
    ))
}

/// Result of a bounded `openOrders` poll.
enum Poll {
    /// The wanted presence was observed; carries the `oid` when present.
    Satisfied(Option<u64>),
    /// The deadline passed (still present, or info errors/timeouts persisted).
    TimedOut,
}

/// Poll `openOrders` until `cloid` is present (`present`) or absent.
///
/// Each call is bounded by [`INFO_TIMEOUT`] (and the remaining window); a
/// transient/429 error is retried until the deadline instead of aborting.
async fn poll_open_orders(
    info: &dyn InfoApi,
    agent: &str,
    cloid: &str,
    present: bool,
    timeout: Duration,
) -> Poll {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Poll::TimedOut;
        }
        let remaining = deadline.saturating_duration_since(now);
        // A transient/429 error or a per-call timeout is retried within the
        // bound; only a successful read can satisfy the poll.
        if let Ok(Ok(orders)) =
            tokio::time::timeout(remaining.min(INFO_TIMEOUT), info.open_orders(agent)).await
        {
            let found = orders
                .iter()
                .find(|order| order.cloid.as_deref() == Some(cloid))
                .map(|order| order.oid);
            match (present, found) {
                (true, Some(oid)) => return Poll::Satisfied(Some(oid)),
                (false, None) => return Poll::Satisfied(None),
                _ => {}
            }
        }
        let sleep =
            OPEN_ORDERS_POLL.min(deadline.saturating_duration_since(tokio::time::Instant::now()));
        if !sleep.is_zero() {
            tokio::time::sleep(sleep).await;
        }
    }
}

/// Poll `openOrders` to a typed result.
async fn wait_open_orders(
    info: &dyn InfoApi,
    agent: &str,
    cloid: &str,
    present: bool,
    timeout: Duration,
) -> Result<Option<u64>> {
    match poll_open_orders(info, agent, cloid, present, timeout).await {
        Poll::Satisfied(oid) => Ok(oid),
        Poll::TimedOut => {
            let verb = if present { "appear in" } else { "clear from" };
            Err(Error::UnknownOutcome(format!(
                "cloid {cloid} did not {verb} openOrders within {timeout:?}"
            )))
        }
    }
}

/// Wrap a fallible future in a timeout, returning a typed error on expiry.
async fn with_timeout<T>(
    what: &str,
    timeout: Duration,
    fut: impl Future<Output = Result<T>>,
) -> Result<T> {
    match tokio::time::timeout(timeout, fut).await {
        Ok(result) => result,
        Err(_) => Err(Error::Http(format!("{what} timed out after {timeout:?}"))),
    }
}

/// Print the human-readable round-trip report.
fn print_report(report: &Report) {
    println!("network:      {:?}", report.network);
    println!("agent:        {}", report.agent);
    println!("coin:         {}", report.coin);
    println!("mid:          {}", report.mid);
    println!(
        "order:        buy {} @ {} ({} bps below mid, ALO, cloid {})",
        report.size, report.price, report.offset_bps, report.cloid
    );
    println!(
        "submit->ack:  {:.3} ms (single sample, H-7 hl_submit_ack_seconds)",
        report.submit_ack.as_secs_f64() * 1000.0
    );
    println!(
        "resting:      openOrders {} ms, orderUpdates {} ms",
        report.open_resting.as_millis(),
        report.updates_resting.as_millis()
    );
    println!(
        "cancelled:    openOrders {} ms, orderUpdates {} ms",
        report.open_gone.as_millis(),
        report.updates_cancelled.as_millis()
    );
    println!("result:       OK — placed, confirmed, and cancelled on testnet");
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use mev_hl_client::types::{
        AllMids, ClearinghouseState, Meta, MetaAndAssetCtxs, OpenOrder, OrderStatusResponse,
        PerpDex, SpotClearinghouseState, SpotMeta, UserFees, UserFill, UserFunding, UserRateLimit,
    };
    use mev_hl_client::{
        Action as VenueAction, L2Book, Level as HlLevel, MarketKind, OrderType, OrderWire,
        WsBasicOrder,
    };
    use serde_json::json;

    use super::*;

    const CLOID: &str = "0x00000000000000000000000000000001";

    /// How the mock replies to the next `order` action.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum PlaceScript {
        Resting,
        Filled,
        Unknown,
        Err,
    }

    /// How the mock replies to the next cancel action.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum CancelScript {
        Ok,
        ReplyError,
        Err,
    }

    /// Scripted per-call outcomes for the mock venue.
    struct Script {
        place: PlaceScript,
        cancel_cloid: CancelScript,
        cancel_oid: CancelScript,
        /// Number of leading `open_orders` calls that fail transiently.
        open_orders_failures: usize,
        /// `open_orders` never returns (simulates a hung REST call).
        open_orders_hang: bool,
    }

    impl Default for Script {
        fn default() -> Self {
            Self {
                place: PlaceScript::Resting,
                cancel_cloid: CancelScript::Ok,
                cancel_oid: CancelScript::Ok,
                open_orders_failures: 0,
                open_orders_hang: false,
            }
        }
    }

    /// State shared between the mock exchange and info clients.
    struct Shared {
        script: Script,
        actions: Vec<VenueAction>,
        open: bool,
    }

    fn ok_response() -> ActionResponse {
        ActionResponse {
            value: json!({"type":"cancel","data":{"statuses":["ok"]}}),
        }
    }

    fn resting_response() -> ActionResponse {
        ActionResponse {
            value: json!({"type":"order","data":{"statuses":[{"resting":{"oid":1}}]}}),
        }
    }

    fn filled_response() -> ActionResponse {
        ActionResponse {
            value: json!({"type":"order","data":{"statuses":["filled"]}}),
        }
    }

    fn error_status_response() -> ActionResponse {
        ActionResponse {
            value: json!({"type":"cancel","data":{"statuses":[{"error":"cancelRejected"}]}}),
        }
    }

    fn cancel_outcome(shared: &mut Shared, script: CancelScript) -> Result<ActionResponse> {
        match script {
            CancelScript::Ok => {
                shared.open = false;
                Ok(ok_response())
            }
            CancelScript::ReplyError => Ok(error_status_response()),
            CancelScript::Err => Err(Error::Exchange("cancel rejected".into())),
        }
    }

    /// Records every submitted action and applies the scripted outcome.
    struct MockExchange {
        shared: Arc<Mutex<Shared>>,
    }

    #[async_trait::async_trait]
    impl ExchangeApi for MockExchange {
        async fn submit(&self, action: &VenueAction) -> Result<ActionResponse> {
            let mut shared = self.shared.lock().unwrap();
            shared.actions.push(action.clone());
            match action {
                VenueAction::Order { .. } => match shared.script.place {
                    PlaceScript::Resting => {
                        shared.open = true;
                        Ok(resting_response())
                    }
                    PlaceScript::Filled => Ok(filled_response()),
                    PlaceScript::Unknown => Err(Error::UnknownOutcome("lost place reply".into())),
                    PlaceScript::Err => Err(Error::Exchange("place rejected".into())),
                },
                VenueAction::CancelByCloid { .. } => {
                    let script = shared.script.cancel_cloid;
                    cancel_outcome(&mut shared, script)
                }
                VenueAction::Cancel { .. } => {
                    let script = shared.script.cancel_oid;
                    cancel_outcome(&mut shared, script)
                }
                _ => Ok(ok_response()),
            }
        }
    }

    /// Minimal [`InfoApi`]: only `l2_book` and `open_orders` are implemented.
    struct MockInfo {
        shared: Arc<Mutex<Shared>>,
        cloid: String,
        oid: u64,
    }

    #[async_trait::async_trait]
    impl InfoApi for MockInfo {
        async fn l2_book(&self, _coin: &str) -> Result<L2Book> {
            Ok(L2Book {
                coin: "BTC".into(),
                time: 0,
                levels: [
                    vec![HlLevel {
                        px: Decimal::from(60_000),
                        sz: Decimal::ONE,
                        n: 1,
                    }],
                    vec![HlLevel {
                        px: Decimal::from(60_001),
                        sz: Decimal::ONE,
                        n: 1,
                    }],
                ],
            })
        }

        async fn open_orders(&self, _user: &str) -> Result<Vec<OpenOrder>> {
            let hang = {
                let mut shared = self.shared.lock().unwrap();
                let hang = shared.script.open_orders_hang;
                if !hang && shared.script.open_orders_failures > 0 {
                    shared.script.open_orders_failures -= 1;
                    return Err(Error::Http("transient 429".into()));
                }
                hang
            };
            if hang {
                tokio::time::sleep(Duration::from_secs(3_600)).await;
            }
            let open = self.shared.lock().unwrap().open;
            if !open {
                return Ok(Vec::new());
            }
            Ok(vec![OpenOrder {
                coin: "BTC".into(),
                oid: self.oid,
                side: "B".into(),
                limit_px: Decimal::from(50_000),
                sz: Decimal::ONE,
                orig_sz: Decimal::ONE,
                timestamp: 0,
                cloid: Some(self.cloid.clone()),
                reduce_only: false,
            }])
        }

        async fn meta(&self) -> Result<Meta> {
            Err(Error::Unimplemented("meta"))
        }
        async fn meta_for(&self, _dex: &str) -> Result<Meta> {
            Err(Error::Unimplemented("meta_for"))
        }
        async fn perp_dexs(&self) -> Result<Vec<PerpDex>> {
            Err(Error::Unimplemented("perp_dexs"))
        }
        async fn spot_meta(&self) -> Result<SpotMeta> {
            Err(Error::Unimplemented("spot_meta"))
        }
        async fn all_mids(&self) -> Result<AllMids> {
            Err(Error::Unimplemented("all_mids"))
        }
        async fn all_mids_for(&self, _dex: &str) -> Result<AllMids> {
            Err(Error::Unimplemented("all_mids_for"))
        }
        async fn meta_and_asset_ctxs(&self) -> Result<MetaAndAssetCtxs> {
            Err(Error::Unimplemented("meta_and_asset_ctxs"))
        }
        async fn clearinghouse_state(&self, _user: &str) -> Result<ClearinghouseState> {
            Err(Error::Unimplemented("clearinghouse_state"))
        }
        async fn order_status(&self, _user: &str, _oid: u64) -> Result<OrderStatusResponse> {
            Err(Error::Unimplemented("order_status"))
        }
        async fn order_status_by_cloid(
            &self,
            _user: &str,
            _cloid: &str,
        ) -> Result<OrderStatusResponse> {
            Err(Error::Unimplemented("order_status_by_cloid"))
        }
        async fn spot_clearinghouse_state(&self, _user: &str) -> Result<SpotClearinghouseState> {
            Err(Error::Unimplemented("spot_clearinghouse_state"))
        }
        async fn user_funding(&self, _user: &str, _start_ms: u64) -> Result<Vec<UserFunding>> {
            Err(Error::Unimplemented("user_funding"))
        }
        async fn user_fills_by_time(&self, _user: &str, _start_ms: u64) -> Result<Vec<UserFill>> {
            Err(Error::Unimplemented("user_fills_by_time"))
        }
        async fn user_fees(&self, _user: &str) -> Result<UserFees> {
            Err(Error::Unimplemented("user_fees"))
        }
        async fn user_rate_limit(&self, _user: &str) -> Result<UserRateLimit> {
            Err(Error::Unimplemented("user_rate_limit"))
        }
    }

    /// A scripted or stalling `orderUpdates` stream.
    struct MockUpdates {
        events: VecDeque<StreamEvent>,
        stall: bool,
    }

    #[async_trait::async_trait]
    impl MarketStream for MockUpdates {
        async fn subscribe(&mut self, _subs: &[Subscription]) -> Result<()> {
            Ok(())
        }

        async fn next(&mut self) -> Result<StreamEvent> {
            if self.stall {
                tokio::time::sleep(Duration::from_secs(3_600)).await;
            }
            self.events
                .pop_front()
                .ok_or_else(|| Error::Unimplemented("no more mock events"))
        }
    }

    fn btc_market() -> Market {
        Market {
            coin: "BTC".into(),
            name: "BTC".into(),
            kind: MarketKind::Perp,
            dex: None,
            dex_offset: None,
            index: 0,
            sz_decimals: 5,
            max_leverage: Some(40),
        }
    }

    fn coin_market(sz_decimals: u32) -> Market {
        Market {
            sz_decimals,
            ..btc_market()
        }
    }

    fn ws_order(cloid: &str, status: &str) -> WsOrder {
        WsOrder {
            order: WsBasicOrder {
                coin: "BTC".into(),
                side: "B".into(),
                limit_px: Decimal::from(50_000),
                sz: Decimal::ONE,
                oid: 1,
                timestamp: 0,
                orig_sz: Decimal::ONE,
                cloid: Some(cloid.to_string()),
            },
            status: status.to_string(),
            status_timestamp: 0,
        }
    }

    fn resting_events() -> VecDeque<StreamEvent> {
        VecDeque::from(vec![StreamEvent::OrderUpdates(vec![ws_order(
            CLOID, "open",
        )])])
    }

    fn full_events() -> VecDeque<StreamEvent> {
        VecDeque::from(vec![
            StreamEvent::OrderUpdates(vec![ws_order(CLOID, "open")]),
            StreamEvent::OrderUpdates(vec![ws_order(CLOID, "canceled")]),
        ])
    }

    fn test_probe(
        exchange: Arc<dyn ExchangeApi>,
        info: Arc<dyn InfoApi>,
        updates: Box<dyn MarketStream>,
        risk: RiskGate,
    ) -> Probe {
        Probe {
            network: Network::Testnet,
            agent: "0xabc".into(),
            market: btc_market(),
            params: ProbeParams {
                offset_bps: DEFAULT_OFFSET_BPS,
                notional_usd: Decimal::from_str(DEFAULT_NOTIONAL_USD).unwrap(),
                confirm_timeout: Duration::from_millis(50),
                verify_timeout: Duration::from_millis(50),
                ttl_ms: 120_000,
                cloid: Some(CLOID.to_string()),
            },
            exchange,
            info,
            updates,
            risk,
            rate: RateBudget::from_settings(&mev_core::config::RateBudgetSettings::default()),
        }
    }

    fn fixture(updates: Box<dyn MarketStream>) -> (Probe, Arc<Mutex<Shared>>) {
        fixture_with(Script::default(), updates)
    }

    fn fixture_with(script: Script, updates: Box<dyn MarketStream>) -> (Probe, Arc<Mutex<Shared>>) {
        let shared = Arc::new(Mutex::new(Shared {
            script,
            actions: Vec::new(),
            open: false,
        }));
        let exchange: Arc<dyn ExchangeApi> = Arc::new(MockExchange {
            shared: shared.clone(),
        });
        let info: Arc<dyn InfoApi> = Arc::new(MockInfo {
            shared: shared.clone(),
            cloid: CLOID.into(),
            oid: 1,
        });
        let risk = RiskGate::from_settings(&mev_core::config::RiskSettings::default());
        (test_probe(exchange, info, updates, risk), shared)
    }

    fn actions(shared: &Arc<Mutex<Shared>>) -> Vec<VenueAction> {
        shared.lock().unwrap().actions.clone()
    }

    fn submitted_order(shared: &Arc<Mutex<Shared>>) -> Vec<OrderWire> {
        actions(shared)
            .into_iter()
            .find_map(|action| match action {
                VenueAction::Order { orders, .. } => Some(orders),
                _ => None,
            })
            .expect("execute must submit an order action")
    }

    #[test]
    fn ensure_testnet_rejects_mainnet_with_a_typed_error() {
        match ensure_testnet(Network::Mainnet) {
            Err(Error::Config(message)) => assert!(message.contains("testnet"), "{message}"),
            other => panic!("expected a typed config error, got {other:?}"),
        }
        assert!(ensure_testnet(Network::Testnet).is_ok());
    }

    #[tokio::test]
    async fn run_refuses_mainnet_before_reading_a_key() {
        let args = RoundtripArgs {
            coin: "BTC".into(),
            offset_bps: DEFAULT_OFFSET_BPS,
            notional_usd: DEFAULT_NOTIONAL_USD.into(),
            confirm_timeout_ms: 10,
        };
        // With no agent key set, a key read would fail first if the network
        // check did not come before it.
        let err = run(Some(Network::Mainnet), args).await.unwrap_err();
        let typed = err.downcast_ref::<Error>();
        assert!(
            matches!(typed, Some(Error::Config(message)) if message.contains("testnet")),
            "expected the testnet refusal, got {err:?}"
        );
    }

    #[test]
    fn passive_buy_is_passive_alo_and_meets_the_minimum() {
        let market = btc_market();
        let mid = Decimal::from(60_000) + Decimal::new(5, 1); // 60000.5
        let (price, size) =
            passive_buy(&market, mid, DEFAULT_OFFSET_BPS, Decimal::from(11)).unwrap();

        // At least the requested distance below the mid, in bps.
        let distance_bps = (mid - price) / mid * Decimal::from(10_000u32);
        assert!(
            distance_bps >= Decimal::from(DEFAULT_OFFSET_BPS),
            "price {price} is only {distance_bps} bps below mid {mid}"
        );
        assert!(price * size >= MIN_ORDER_NOTIONAL, "notional below minimum");

        // The wire carries the passive, post-only client id.
        let wire = build_order_wire(
            &market,
            &OrderParams {
                is_buy: true,
                size,
                limit_px: price,
                tif: Tif::Alo,
                reduce_only: false,
                cloid: Some(CLOID.into()),
            },
        )
        .unwrap();
        assert!(wire.b, "must be a buy");
        assert_eq!(wire.t, OrderType::limit(Tif::Alo));
        assert_eq!(wire.c.as_deref(), Some(CLOID));
        assert_eq!(wire.p, price.normalize().to_string());
    }

    #[test]
    fn passive_buy_rejects_offsets_below_the_minimum() {
        let market = btc_market();
        let mid = Decimal::from(60_000);
        assert!(
            passive_buy(&market, mid, MIN_OFFSET_BPS - 1, Decimal::from(11)).is_err(),
            "below the minimum must be refused"
        );
        assert!(
            passive_buy(&market, mid, MIN_OFFSET_BPS, Decimal::from(11)).is_ok(),
            "exactly the minimum must be accepted"
        );
        assert!(
            passive_buy(&market, mid, 10_000, Decimal::from(11)).is_err(),
            "a zero/negative target must be refused"
        );
    }

    #[test]
    fn passive_buy_handles_low_price_and_high_sz_decimals() {
        // A low-priced coin: price stays positive and the size meets the floor.
        let low = coin_market(2);
        let mid = Decimal::new(5, 1); // 0.5
        let (price, size) = passive_buy(&low, mid, DEFAULT_OFFSET_BPS, Decimal::from(11)).unwrap();
        assert!(price > Decimal::ZERO);
        assert!(price * size >= MIN_ORDER_NOTIONAL);
        assert!(
            (mid - price) / mid * Decimal::from(10_000u32) >= Decimal::from(DEFAULT_OFFSET_BPS)
        );

        // High szDecimals (spot-like): sizes round to many decimals and the
        // minimum-notional floor still wins.
        let high = coin_market(8);
        let mid = Decimal::from(60_000) + Decimal::new(5, 1);
        let (price, size) = passive_buy(&high, mid, DEFAULT_OFFSET_BPS, Decimal::from(11)).unwrap();
        assert!(price > Decimal::ZERO);
        assert!(price * size >= MIN_ORDER_NOTIONAL);
        assert!(size.scale() <= 8);
    }

    #[test]
    fn wanted_cancelled_only_matches_the_exact_user_status() {
        assert!(Wanted::Cancelled.matches("canceled"));
        for rejected in [
            "scheduledCancel",
            "marginCanceled",
            "selfTradeCanceled",
            "reduceOnlyCanceled",
            "canceledFoo",
            "open",
        ] {
            assert!(
                !Wanted::Cancelled.matches(rejected),
                "{rejected} must not count as a user cancel"
            );
        }
        assert!(Wanted::Resting.matches("open"));
        assert!(Wanted::Resting.matches("resting"));
        assert!(!Wanted::Resting.matches("filled"));
    }

    #[tokio::test]
    async fn happy_path_places_confirms_and_cancels() {
        let (mut probe, shared) = fixture(Box::new(MockUpdates {
            events: full_events(),
            stall: false,
        }));

        let report = execute(&mut probe).await.unwrap();
        assert_eq!(report.cloid, CLOID);
        assert!(report.price < report.mid);
        assert!(report.price * report.size >= MIN_ORDER_NOTIONAL);

        // The actual Order action sent: buy, ALO, our cloid, and at least
        // `offset-bps` below the mid.
        let order = &submitted_order(&shared)[0];
        assert!(order.b, "must be a buy");
        assert_eq!(order.t, OrderType::limit(Tif::Alo));
        assert_eq!(order.c.as_deref(), Some(CLOID));
        let sent_px = Decimal::from_str(&order.p).unwrap();
        let distance_bps = (report.mid - sent_px) / report.mid * Decimal::from(10_000u32);
        assert!(distance_bps >= Decimal::from(DEFAULT_OFFSET_BPS));

        // 2 before 3: the dead-man armed, THEN the order was placed; and case
        // (a) ends with a verified disarm as the last action.
        let recorded = actions(&shared);
        assert!(
            matches!(recorded[0], VenueAction::ScheduleCancel { time: Some(_) }),
            "first action must be the dead-man arm: {recorded:?}"
        );
        assert!(
            matches!(recorded[1], VenueAction::Order { .. }),
            "second action must be the place: {recorded:?}"
        );
        assert!(
            matches!(
                recorded.last(),
                Some(VenueAction::ScheduleCancel { time: None })
            ),
            "the last action must be the verified disarm: {recorded:?}"
        );
    }

    #[tokio::test]
    async fn a_missing_order_update_still_cancels_and_disarms() {
        // The order reaches `openOrders`, but its `orderUpdates` message never
        // arrives: the run fails, cancels, verifies gone, and disarms.
        let (mut probe, shared) = fixture(Box::new(MockUpdates {
            events: VecDeque::new(),
            stall: true,
        }));

        let err = execute(&mut probe).await.unwrap_err();
        assert!(
            matches!(err, Error::UnknownOutcome(_)),
            "expected a typed timeout, got {err:?}"
        );

        let recorded = actions(&shared);
        assert!(
            recorded
                .iter()
                .any(|a| matches!(a, VenueAction::CancelByCloid { .. })),
            "the cancel must still be sent after the failure: {recorded:?}"
        );
        assert!(
            matches!(
                recorded.last(),
                Some(VenueAction::ScheduleCancel { time: None })
            ),
            "a verified-gone order disarms: {recorded:?}"
        );
    }

    #[tokio::test]
    async fn a_place_error_still_cancels_and_disarms() {
        let (mut probe, shared) = fixture_with(
            Script {
                place: PlaceScript::Err,
                ..Default::default()
            },
            Box::new(MockUpdates {
                events: VecDeque::new(),
                stall: false,
            }),
        );

        let err = execute(&mut probe).await.unwrap_err();
        assert!(matches!(err, Error::Exchange(_)), "got {err:?}");
        let recorded = actions(&shared);
        assert!(
            recorded
                .iter()
                .any(|a| matches!(a, VenueAction::CancelByCloid { .. })),
            "a place error must still trigger cleanup: {recorded:?}"
        );
        assert!(matches!(
            recorded.last(),
            Some(VenueAction::ScheduleCancel { time: None })
        ));
    }

    #[tokio::test]
    async fn a_non_resting_place_status_cleans_up() {
        let (mut probe, shared) = fixture_with(
            Script {
                place: PlaceScript::Filled,
                ..Default::default()
            },
            Box::new(MockUpdates {
                events: VecDeque::new(),
                stall: false,
            }),
        );

        let err = execute(&mut probe).await.unwrap_err();
        assert!(err.to_string().contains("not resting"), "got {err:?}");
        assert!(
            actions(&shared)
                .iter()
                .any(|a| matches!(a, VenueAction::CancelByCloid { .. }))
        );
    }

    #[tokio::test]
    async fn a_risk_rejection_aborts_before_arming() {
        let shared = Arc::new(Mutex::new(Shared {
            script: Script::default(),
            actions: Vec::new(),
            open: false,
        }));
        let exchange: Arc<dyn ExchangeApi> = Arc::new(MockExchange {
            shared: shared.clone(),
        });
        let info: Arc<dyn InfoApi> = Arc::new(MockInfo {
            shared: shared.clone(),
            cloid: CLOID.into(),
            oid: 1,
        });
        // A $1 per-order cap resizes the probe order below the $10 venue floor,
        // so the gate rejects it.
        let risk = RiskGate::from_settings(&mev_core::config::RiskSettings {
            max_order_notional_usd: Some(Decimal::ONE),
            ..Default::default()
        });
        let mut probe = test_probe(
            exchange,
            info,
            Box::new(MockUpdates {
                events: full_events(),
                stall: false,
            }),
            risk,
        );

        let err = execute(&mut probe).await.unwrap_err();
        assert!(err.to_string().contains("risk gate"), "got {err:?}");
        assert!(
            shared.lock().unwrap().actions.is_empty(),
            "no arm and no place may be sent on a risk rejection"
        );
    }

    #[tokio::test]
    async fn a_cancel_by_cloid_failure_uses_the_oid_fallback() {
        let (mut probe, shared) = fixture_with(
            Script {
                cancel_cloid: CancelScript::Err,
                cancel_oid: CancelScript::Ok,
                ..Default::default()
            },
            Box::new(MockUpdates {
                events: resting_events(),
                stall: false,
            }),
        );

        let err = execute(&mut probe).await.unwrap_err();
        // The original cancel-by-cloid failure surfaces, but cleanup succeeded.
        assert!(matches!(err, Error::Exchange(_)), "got {err:?}");
        let recorded = actions(&shared);
        assert!(
            recorded
                .iter()
                .any(|a| matches!(a, VenueAction::Cancel { .. })),
            "the oid fallback must be sent: {recorded:?}"
        );
        // Verified gone by the oid fallback, so the switch is disarmed last.
        assert!(matches!(
            recorded.last(),
            Some(VenueAction::ScheduleCancel { time: None })
        ));
    }

    #[tokio::test]
    async fn cleanup_case_c_leaves_the_deadman_armed() {
        let (mut probe, shared) = fixture_with(
            Script {
                cancel_cloid: CancelScript::Err,
                cancel_oid: CancelScript::Err,
                ..Default::default()
            },
            Box::new(MockUpdates {
                events: resting_events(),
                stall: false,
            }),
        );

        let err = execute(&mut probe).await.unwrap_err();
        assert!(
            err.to_string().contains("may still be resting"),
            "expected the loud not-verified error, got {err:?}"
        );
        let recorded = actions(&shared);
        assert!(
            !recorded
                .iter()
                .any(|a| matches!(a, VenueAction::ScheduleCancel { time: None })),
            "an unverified cancel must NOT disarm: {recorded:?}"
        );
        // The only schedule action is the initial arm.
        assert!(matches!(
            recorded.first(),
            Some(VenueAction::ScheduleCancel { time: Some(_) })
        ));
    }

    #[tokio::test]
    async fn cleanup_case_d_unknown_place_leaves_the_deadman_armed() {
        let (mut probe, shared) = fixture_with(
            Script {
                place: PlaceScript::Unknown,
                ..Default::default()
            },
            Box::new(MockUpdates {
                events: VecDeque::new(),
                stall: false,
            }),
        );

        let err = execute(&mut probe).await.unwrap_err();
        assert!(
            err.to_string().contains("unknown"),
            "expected the unknown-outcome error, got {err:?}"
        );
        let recorded = actions(&shared);
        assert!(
            recorded
                .iter()
                .any(|a| matches!(a, VenueAction::CancelByCloid { .. })),
            "an unknown place still attempts the cancel: {recorded:?}"
        );
        assert!(
            !recorded
                .iter()
                .any(|a| matches!(a, VenueAction::ScheduleCancel { time: None })),
            "an unknown place must NOT disarm: {recorded:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_info_call_timeout_still_reaches_cleanup() {
        let (mut probe, shared) = fixture_with(
            Script {
                open_orders_hang: true,
                ..Default::default()
            },
            Box::new(MockUpdates {
                events: VecDeque::new(),
                stall: false,
            }),
        );

        let err = execute(&mut probe).await.unwrap_err();
        assert!(
            err.to_string().contains("may still be resting"),
            "a hung info call must surface the not-verified error, got {err:?}"
        );
        assert!(
            actions(&shared)
                .iter()
                .any(|a| matches!(a, VenueAction::CancelByCloid { .. })),
            "a hung info call must not skip cleanup"
        );
    }

    #[tokio::test]
    async fn a_cancel_by_cloid_error_status_uses_the_oid_fallback() {
        let (mut probe, shared) = fixture_with(
            Script {
                cancel_cloid: CancelScript::ReplyError,
                cancel_oid: CancelScript::Ok,
                ..Default::default()
            },
            Box::new(MockUpdates {
                events: resting_events(),
                stall: false,
            }),
        );

        let err = execute(&mut probe).await.unwrap_err();
        assert!(err.to_string().contains("error status"), "got {err:?}");
        let recorded = actions(&shared);
        assert!(
            recorded
                .iter()
                .any(|a| matches!(a, VenueAction::Cancel { .. })),
            "the oid fallback must be sent for an error status: {recorded:?}"
        );
        assert!(matches!(
            recorded.last(),
            Some(VenueAction::ScheduleCancel { time: None })
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn a_transient_open_orders_error_is_retried() {
        let (mut probe, shared) = fixture_with(
            Script {
                open_orders_failures: 1,
                ..Default::default()
            },
            Box::new(MockUpdates {
                events: full_events(),
                stall: false,
            }),
        );
        // Long enough for one 250 ms poll retry.
        probe.params.confirm_timeout = Duration::from_millis(600);

        // The first `open_orders` call (the resting confirmation) fails with a
        // 429; the poll retries rather than aborting, so the round trip passes.
        let report = execute(&mut probe).await.unwrap();
        assert_eq!(report.cloid, CLOID);
        let _ = shared;
    }
}
