//! Engine hot-path benchmarks (SPEC-0010 §17, E-10).
//!
//! Runs the real engine pieces, excluding the socket:
//!
//! - `bbo_to_action`: one `Bbo` event → apply → a trivial threshold strategy →
//!   `LimitRisk` → `plan_iteration` (build) → sign. The engine's own risk is
//!   E-9; until it lands this uses the existing `hl-arb-risk` gate, whose pending
//!   bookkeeping allocates — that cost is part of the measurement today.
//! - `drain_1000`: 1000 queued market events drained in one iteration, with one
//!   decision per dirty coin (conflation), through the real `EngineLoop`.
//! - `replay_throughput`: events/second through the typed ingest + apply path
//!   (target ≥ 1M events/s per §17).
//!
//! `zero_alloc` is deliberately **not** here: it is a test binary with a
//! counting allocator (`crates/hl-arb-engine/tests/zero_alloc.rs`), not a
//! criterion bench.
//!
//! ## Quick mode
//!
//! Set `MEV_BENCH_QUICK=1` to shrink the sample and shorten the measurement
//! window so CI can run the benches warn-only:
//!
//! ```sh
//! MEV_BENCH_QUICK=1 cargo bench -p hl-arb-bot --bench engine
//! ```
//!
//! Criterion's own `--quick` flag still works too.

use std::time::Duration;

use criterion::{BatchSize, Criterion, Throughput, black_box, criterion_group, criterion_main};
use rust_decimal::Decimal;

use hl_arb_client::AssetMap;
use hl_arb_client::signing::AgentSigner;
use hl_arb_client::types::{AssetMeta as WireAssetMeta, Meta};
use hl_arb_engine::builder::{AssetTable, plan_iteration};
use hl_arb_engine::channels::inputs;
use hl_arb_engine::exec::ReqIds;
use hl_arb_engine::ingest::Ingest;
use hl_arb_engine::instrument::{LatencyRecorder, Stamps};
use hl_arb_engine::orders::{CloidAssigner, OrderManager};
use hl_arb_engine::routes::Interests as RouteInterests;
use hl_arb_engine::run::{Dispatcher, EngineLoop, LoopConfig};
use hl_arb_engine::state::{AccountState, EngineState};
use hl_arb_engine::strategy::{Action, Actions, Ctx, OrderEvent, Strategy};
use hl_arb_engine::types::{CoinId, CoinRegistry, ConnId, Level, MarketUpdate, Stamp};
use hl_arb_risk::{LimitRisk, Limits, RiskCheck, RiskContext};
use hl_arb_strategy::{
    AccountView, BookView, MarketView, OrderIntent, Side, StrategyId, TimeInForce,
};

/// Public, zero-funds throwaway signing key (the second default Anvil/Hardhat
/// account, already public in `hl-arb-client`'s tests). Never a secret, and it
/// is never sent anywhere: the bench signs locally and drops the signature.
const BENCH_AGENT_KEY: &str = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";

const L2BOOK: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../hl-arb-client/benches/fixtures/l2Book.jsonl"
));
const TRADES: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../hl-arb-client/benches/fixtures/trades.jsonl"
));
const CTX: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../hl-arb-client/benches/fixtures/activeAssetCtx.jsonl"
));

fn criterion_config() -> Criterion {
    let quick = std::env::var("MEV_BENCH_QUICK")
        .map(|v| v != "0")
        .unwrap_or(false);
    let c = Criterion::default();
    if quick {
        c.sample_size(10)
            .warm_up_time(Duration::from_millis(200))
            .measurement_time(Duration::from_secs(1))
    } else {
        c
    }
}

fn btc_registry() -> CoinRegistry {
    CoinRegistry::from_coins(&["BTC".into()])
}

fn btc_asset_map() -> AssetMap {
    let mut map = AssetMap::new();
    map.insert_perp_dex(
        None,
        None,
        &Meta {
            universe: vec![WireAssetMeta {
                name: "BTC".into(),
                sz_decimals: 2,
                max_leverage: 40,
                is_delisted: false,
                only_isolated: false,
            }],
        },
    );
    map
}

fn level(px: i64, sz: i64) -> Level {
    Level {
        px: Decimal::from(px),
        sz: Decimal::from(sz),
        n: 1,
    }
}

fn bbo_update(coin: CoinId) -> MarketUpdate {
    MarketUpdate::Bbo {
        coin,
        stamp: Stamp {
            t_recv_ns: 1,
            mono_ns: 2,
            ts_exch_ms: 0,
        },
        bid: Some(level(100, 10)),
        ask: Some(level(101, 10)),
    }
}

/// A trivial threshold strategy: buys at the touch when the ask is at or below
/// `threshold`.
struct ThresholdStrategy {
    id: StrategyId,
    coin: CoinId,
    threshold: Decimal,
}

impl Strategy for ThresholdStrategy {
    fn id(&self) -> StrategyId {
        self.id.clone()
    }

    fn cost(&self) -> hl_arb_strategy::CostModel {
        hl_arb_strategy::CostModel::default()
    }

    fn interests(&self) -> hl_arb_engine::strategy::Interests {
        hl_arb_engine::strategy::Interests::coins([self.coin], hl_arb_engine::strategy::Stream::Bbo)
    }

    fn on_market(&mut self, coin: CoinId, ctx: &Ctx<'_>, out: &mut Actions) {
        if coin != self.coin {
            return;
        }
        let Some(ask) = ctx.best_ask(coin) else {
            return;
        };
        if ask <= self.threshold {
            out.place(OrderIntent {
                strategy: self.id.clone(),
                coin: "BTC".into(),
                side: Side::Buy,
                limit_px: Some(ask),
                size: Decimal::ONE,
                tif: TimeInForce::Gtc,
                reduce_only: false,
                rationale: "bench".into(),
                cloid: None,
                signal_ms: 0,
                decision_ms: 0,
            });
        }
    }

    fn on_order(&mut self, _update: &OrderEvent, _ctx: &Ctx<'_>, _out: &mut Actions) {}
}

fn bbo_to_action(c: &mut Criterion) {
    let registry = btc_registry();
    let table = AssetTable::from_markets(&registry, &btc_asset_map());
    let coin = CoinId(0);

    let mut state = EngineState::new(1);
    let account = AccountState::new(1);
    let account_view = AccountView::default();
    let mut strategy = ThresholdStrategy {
        id: StrategyId::from("bench"),
        coin,
        threshold: Decimal::from(102),
    };
    let mut actions = Actions::new();

    let orders = OrderManager::new(1);
    let cloids = CloidAssigner::new();
    let mut req_ids = ReqIds::new();
    let mut risk = LimitRisk::new(Limits::default());

    let mut market_view = MarketView::new();
    market_view.insert_book(
        "BTC",
        BookView {
            bids: vec![(Decimal::from(100), Decimal::from(10))],
            asks: vec![(Decimal::from(101), Decimal::from(10))],
            sz_decimals: 2,
            time: 0,
        },
    );

    let signer = AgentSigner::from_hex(BENCH_AGENT_KEY, true).expect("bench signer");
    let mut nonce = 1_700_000_000_000u64;
    let mut recorder = LatencyRecorder::new();
    let update = bbo_update(coin);

    // Apply once so the strategy context is warm before timing.
    apply_market(&mut state, &update);

    c.bench_function("bbo_to_action", |b| {
        b.iter(|| {
            // 1. apply the event
            apply_market(&mut state, black_box(&update));

            // 2. decide
            actions.clear();
            let now = Stamp {
                t_recv_ns: 1,
                mono_ns: 2,
                ts_exch_ms: 0,
            };
            let ctx = Ctx {
                now,
                markets: state.slots(),
                account: &account,
                registry: &registry,
            };
            strategy.on_market(black_box(coin), &ctx, &mut actions);

            // 3. risk
            let risk_ctx = RiskContext {
                market: &market_view,
                account: &account_view,
                now_ms: 0,
            };
            for action in actions.as_slice() {
                if let Action::Place(intent) = action {
                    black_box(risk.check(intent, &risk_ctx));
                }
            }

            // 4. build
            let touch = |_coin: CoinId, is_buy: bool| {
                Some(if is_buy {
                    Decimal::from(101)
                } else {
                    Decimal::from(100)
                })
            };
            let batch = plan_iteration(
                actions.as_slice(),
                &registry,
                &table,
                &orders,
                &cloids,
                &touch,
                Decimal::from(10),
                &mut req_ids,
            );

            // 5. sign
            for post in &batch.posts {
                let signature = signer.sign_l1(&post.action, nonce, None, None);
                nonce = nonce.wrapping_add(1);
                black_box(signature.is_ok());
            }

            // 6. instrument (the E-10 recorder is on the measured path).
            let stamps = Stamps {
                t_recv: 1,
                t_decoded: 3,
                t_dequeued: 4,
                t_decided: 8,
                t_risked: 10,
                t_signed: 500,
                t_handoff: 505,
                t_written: 510,
                t_ack: 900,
            };
            recorder.record(&stamps);
        })
    });
}

fn apply_market(state: &mut EngineState, update: &MarketUpdate) {
    match update {
        MarketUpdate::Bbo {
            coin,
            bid,
            ask,
            stamp,
        } => {
            if let Some(slot) = state.slot_mut(*coin) {
                slot.bbo = Some((*bid, *ask, *stamp));
            }
        }
        MarketUpdate::Book { coin, book, stamp } => {
            if let Some(slot) = state.slot_mut(*coin) {
                slot.book = Some((*book, *stamp));
            }
        }
        MarketUpdate::Ctx { coin, ctx, stamp } => {
            if let Some(slot) = state.slot_mut(*coin) {
                slot.ctx = Some((*ctx, *stamp));
            }
        }
        MarketUpdate::Trades { .. } | MarketUpdate::Gap { .. } => {}
    }
    if let Some(coin) = update_coin(update) {
        state.mark_dirty(coin);
    }
}

fn update_coin(update: &MarketUpdate) -> Option<CoinId> {
    match update {
        MarketUpdate::Bbo { coin, .. }
        | MarketUpdate::Book { coin, .. }
        | MarketUpdate::Trades { coin, .. }
        | MarketUpdate::Ctx { coin, .. } => Some(*coin),
        MarketUpdate::Gap { .. } => None,
    }
}

const DRAIN_COINS: u16 = 10;
const DRAIN_EVENTS: usize = 1_000;

/// Counts one decision per dirty coin drained by the real loop.
struct DrainDispatcher {
    decisions: u64,
}

impl Dispatcher for DrainDispatcher {
    fn interests(&self) -> Vec<RouteInterests> {
        vec![RouteInterests::coins((0..DRAIN_COINS).map(CoinId))]
    }

    fn on_coin(&mut self, _coin: CoinId, _stamp: Stamp) {
        self.decisions += 1;
    }
}

fn drain_1000(c: &mut Criterion) {
    let (handles, inputs) = inputs(DRAIN_EVENTS + 16, 16);
    let (_stop_tx, stop_rx) = crossbeam_channel::bounded(1);
    let mut engine = EngineLoop::new(
        inputs,
        DrainDispatcher { decisions: 0 },
        LoopConfig {
            spin_us: 0,
            coin_count: DRAIN_COINS as usize,
        },
        stop_rx,
    );

    c.bench_function("drain_1000", |b| {
        b.iter_batched(
            || {
                for i in 0..DRAIN_EVENTS {
                    let coin = CoinId((i % DRAIN_COINS as usize) as u16);
                    let _ = handles.send_market(bbo_update(coin));
                }
            },
            |()| {
                // One iteration drains all 1000 events and dispatches each of
                // the 10 dirty coins exactly once (conflation).
                black_box(engine.iterate(1));
            },
            // Queue exactly one batch of 1000 per measured iteration: a larger
            // batch would overflow the 1_016-slot channel during setup and the
            // later iterations would drain nothing.
            BatchSize::PerIteration,
        )
    });
}

fn replay_throughput(c: &mut Criterion) {
    let registry =
        CoinRegistry::from_coins(&["BTC".into(), "ETH".into(), "SOL".into(), "xyz:TSLA".into()]);
    let ingest = Ingest::new(ConnId(0), registry);
    let stamp = Stamp::default();
    let frames: Vec<&str> = [L2BOOK, TRADES, CTX]
        .into_iter()
        .flat_map(|raw| raw.lines())
        .collect();
    let mut state = EngineState::new(4);

    let mut group = c.benchmark_group("replay_throughput");
    group.throughput(Throughput::Elements(frames.len() as u64));
    group.bench_function("ingest_apply", |b| {
        b.iter(|| {
            for frame in &frames {
                if let Ok(Some(update)) = ingest.decode(black_box(frame), stamp) {
                    apply_market(&mut state, &update);
                }
            }
        });
    });
    group.finish();
}

/// The ingest→engine path: typed decode, non-blocking hand-off, and one loop
/// iteration — the path the E-13 wiring must keep ahead of the legacy decode
/// (the post-E-13 review found the legacy `ws::decode`, 130–260 µs, running
/// *before* the hand-off). `legacy_ws_decode` measures that decode separately so
/// a regression in either is visible.
fn ingest_to_engine(c: &mut Criterion) {
    let registry =
        CoinRegistry::from_coins(&["BTC".into(), "ETH".into(), "SOL".into(), "xyz:TSLA".into()]);
    let ingest = Ingest::new(ConnId(0), registry);
    let stamp = Stamp::default();
    let frames: Vec<&str> = [L2BOOK, TRADES, CTX]
        .into_iter()
        .flat_map(|raw| raw.lines())
        .collect();

    let (handles, inputs) = inputs(frames.len() + 16, 16);
    let (_stop_tx, stop_rx) = crossbeam_channel::bounded(1);
    let mut engine = EngineLoop::new(
        inputs,
        DrainDispatcher { decisions: 0 },
        LoopConfig {
            spin_us: 0,
            coin_count: DRAIN_COINS as usize,
        },
        stop_rx,
    );

    let mut group = c.benchmark_group("ingest_to_engine");
    group.throughput(Throughput::Elements(frames.len() as u64));
    group.bench_function("decode_send_dispatch", |b| {
        b.iter(|| {
            for frame in &frames {
                if let Ok(Some(update)) = ingest.decode(black_box(frame), stamp) {
                    // `send_market` is non-blocking (drops when full).
                    let _ = handles.send_market(update);
                }
                black_box(engine.iterate(1));
            }
        });
    });
    group.bench_function("legacy_ws_decode", |b| {
        b.iter(|| {
            for frame in &frames {
                let _ = black_box(hl_arb_client::ws::decode(black_box(frame)));
            }
        });
    });
    group.finish();
}

criterion_group! {
    name = benches;
    config = criterion_config();
    targets = bbo_to_action, drain_1000, replay_throughput, ingest_to_engine
}
criterion_main!(benches);
