//! Unit tests for the strategy dispatcher (SPEC-0010).

use std::str::FromStr;
use std::sync::Mutex;

use hl_arb_client::types::{AssetMeta as WireAssetMeta, Meta};
use hl_arb_client::{Action as VenueAction, AssetMap};
use hl_arb_strategy::{CostModel, TimeInForce};
use rust_decimal::Decimal;

use super::*;
use crate::orders::CloidAssigner;
use crate::risk::RiskGate;
use crate::state::EngineState;
use crate::strategy::TimerId as StratTimerId;
use crate::types::{AccountSnapshot, Level, VenueOrderStatus};

fn ds(value: &str) -> Decimal {
    Decimal::from_str(value).unwrap()
}

fn registry() -> CoinRegistry {
    CoinRegistry::from_coins(&["BTC".into(), "ETH".into()])
}

fn asset_map() -> AssetMap {
    let mut map = AssetMap::new();
    map.insert_perp_dex(
        None,
        None,
        &Meta {
            universe: vec![
                WireAssetMeta {
                    name: "BTC".into(),
                    sz_decimals: 0,
                    max_leverage: 40,
                    is_delisted: false,
                    only_isolated: false,
                },
                WireAssetMeta {
                    name: "ETH".into(),
                    sz_decimals: 0,
                    max_leverage: 40,
                    is_delisted: false,
                    only_isolated: false,
                },
            ],
        },
    );
    map
}

fn table() -> AssetTable {
    AssetTable::from_markets(&registry(), &asset_map())
}

fn level(px: &str) -> Level {
    Level {
        px: ds(px),
        sz: Decimal::ONE,
        n: 1,
    }
}

fn state_with(bid: &str, ask: &str) -> EngineState {
    let mut state = EngineState::new(2);
    let slot = state.slot_mut(CoinId(0)).unwrap();
    slot.bbo = Some((
        Some(level(bid)),
        Some(level(ask)),
        Stamp {
            mono_ns: 1_000,
            ..Default::default()
        },
    ));
    state
}

/// A one-sided bbo: `None` marks an empty side reported by the venue.
fn state_with_sides(bid: Option<&str>, ask: Option<&str>) -> EngineState {
    let mut state = EngineState::new(2);
    let slot = state.slot_mut(CoinId(0)).unwrap();
    slot.bbo = Some((bid.map(level), ask.map(level), stamp(1_000)));
    state
}

/// The wire limit price of the single order in a post.
fn post_order_price(post: &UnsignedPost) -> Px {
    match &post.action {
        VenueAction::Order { orders, .. } => {
            orders[0].p.as_str().parse().expect("wire price parses")
        }
        other => panic!("expected order post, got {other:?}"),
    }
}

fn stamp(mono_ns: u64) -> Stamp {
    Stamp {
        mono_ns,
        ..Default::default()
    }
}

fn intent(coin: &str, side: Side) -> OrderIntent {
    OrderIntent {
        strategy: StrategyId::from("test"),
        coin: coin.into(),
        side: if matches!(side, Side::Buy) {
            hl_arb_strategy::Side::Buy
        } else {
            hl_arb_strategy::Side::Sell
        },
        limit_px: Some(ds("100")),
        size: Decimal::ONE,
        tif: TimeInForce::Alo,
        reduce_only: false,
        rationale: "t".into(),
        cloid: None,
        signal_ms: 0,
        decision_ms: 0,
    }
}

/// An aggressive (marketable, no limit) variant of [`intent`].
fn aggressive(coin: &str, side: Side) -> OrderIntent {
    OrderIntent {
        limit_px: None,
        ..intent(coin, side)
    }
}

fn live(cloid: Cloid, coin: u16, state: OrderState) -> LiveOrder {
    LiveOrder {
        cloid,
        coin: CoinId(coin),
        side: Side::Buy,
        px: ds("100"),
        sz: Decimal::ONE,
        filled_sz: Decimal::ZERO,
        reduce_only: false,
        strategy: StrategyId::from("other"),
        state,
        req_id: None,
        oid: None,
    }
}

fn cloid(n: u8) -> Cloid {
    let mut bytes = [0u8; 16];
    bytes[0] = n;
    Cloid(bytes)
}

/// A strategy that records the coins it is called for and can emit a script.
struct Recording {
    id: StrategyId,
    coin: CoinId,
    calls: Arc<Mutex<Vec<CoinId>>>,
    events: Arc<Mutex<Vec<OrderEvent>>>,
    script: Vec<Action>,
    emit_on_market: bool,
}

impl Recording {
    fn new(id: &str, coin: CoinId) -> Self {
        Self {
            id: StrategyId::from(id),
            coin,
            calls: Arc::new(Mutex::new(Vec::new())),
            events: Arc::new(Mutex::new(Vec::new())),
            script: Vec::new(),
            emit_on_market: true,
        }
    }

    fn with_script(mut self, script: Vec<Action>) -> Self {
        self.script = script;
        self
    }

    fn calls(&self) -> Arc<Mutex<Vec<CoinId>>> {
        self.calls.clone()
    }

    fn events(&self) -> Arc<Mutex<Vec<OrderEvent>>> {
        self.events.clone()
    }
}

impl Strategy for Recording {
    fn id(&self) -> StrategyId {
        self.id.clone()
    }
    fn cost(&self) -> CostModel {
        CostModel::default()
    }
    fn interests(&self) -> Interests {
        Interests::coins([self.coin], Stream::Bbo)
    }
    fn on_market(&mut self, coin: CoinId, _ctx: &Ctx<'_>, out: &mut Actions) {
        self.calls.lock().unwrap().push(coin);
        if self.emit_on_market {
            for action in &self.script {
                match action {
                    Action::Place(intent) => out.place(intent.clone()),
                    Action::Cancel { cloid } => out.cancel(*cloid),
                    Action::Modify { cloid, px, sz } => out.modify(*cloid, *px, *sz),
                    Action::PlaceGroup(group) => out.place_group(group.clone()),
                }
            }
        }
    }
    fn on_order(&mut self, update: &OrderEvent, _ctx: &Ctx<'_>, _out: &mut Actions) {
        self.events.lock().unwrap().push(update.clone());
    }
}

struct CapturingExec {
    posts: Arc<Mutex<Vec<UnsignedPost>>>,
    full: bool,
}

impl CapturingExec {
    fn new(full: bool) -> Self {
        Self {
            posts: Arc::new(Mutex::new(Vec::new())),
            full,
        }
    }
    fn posts(&self) -> Arc<Mutex<Vec<UnsignedPost>>> {
        self.posts.clone()
    }
}

impl ExecBackend for CapturingExec {
    fn try_send(&mut self, post: UnsignedPost) -> bool {
        if self.full {
            return false;
        }
        self.posts.lock().unwrap().push(post);
        true
    }
}

struct Harness {
    dispatcher: StrategyDispatcher,
    posts: Arc<Mutex<Vec<UnsignedPost>>>,
}

fn harness(strategies: Vec<Box<dyn Strategy>>, full: bool) -> Harness {
    let exec = CapturingExec::new(full);
    let posts = exec.posts();
    let dispatcher = StrategyDispatcher::new(
        strategies,
        registry(),
        table(),
        RiskGate::default(),
        Some(Box::new(exec)),
        DispatcherConfig::default(),
    );
    Harness { dispatcher, posts }
}

#[test]
fn dispatches_only_to_strategies_watching_the_coin() {
    let a = Recording::new("a", CoinId(0));
    let b = Recording::new("b", CoinId(1));
    let a_calls = a.calls();
    let b_calls = b.calls();
    let mut h = harness(vec![Box::new(a), Box::new(b)], false);

    let state = state_with("100", "101");
    h.dispatcher.on_coin_state(CoinId(0), stamp(10), &state);
    assert_eq!(*a_calls.lock().unwrap(), vec![CoinId(0)]);
    assert!(b_calls.lock().unwrap().is_empty());

    h.dispatcher.on_coin_state(CoinId(1), stamp(11), &state);
    assert_eq!(*b_calls.lock().unwrap(), vec![CoinId(1)]);
    assert_eq!(a_calls.lock().unwrap().len(), 1);
}

#[test]
fn a_stale_coin_is_rejected_by_risk_until_a_book_refreshes_it() {
    let strategy = Recording::new("test", CoinId(0))
        .with_script(vec![Action::Place(intent("BTC", Side::Buy))]);
    let mut h = harness(vec![Box::new(strategy)], false);
    let mut state = state_with("100", "101");
    state.slot_mut(CoinId(0)).unwrap().stale = true;

    // Feed gap: the risk gate fails closed on the non-reduce-only place.
    h.dispatcher.on_coin_state(CoinId(0), stamp(10), &state);
    assert!(
        h.posts.lock().unwrap().is_empty(),
        "a stale coin must not place"
    );

    // A fresh book snapshot clears staleness and orders pass again.
    state.slot_mut(CoinId(0)).unwrap().stale = false;
    h.dispatcher.on_coin_state(CoinId(0), stamp(20), &state);
    assert_eq!(h.posts.lock().unwrap().len(), 1, "fresh coin places again");
}

#[test]
fn places_build_one_bulk_order_and_cancel_first() {
    let resting = cloid(9);
    let strategy = Recording::new("test", CoinId(0)).with_script(vec![
        Action::Cancel { cloid: resting },
        Action::Place(intent("BTC", Side::Buy)),
        Action::Place(intent("BTC", Side::Sell)),
    ]);
    let mut h = harness(vec![Box::new(strategy)], false);
    h.dispatcher
        .orders
        .insert(live(resting, 0, OrderState::Resting));

    let state = state_with("100", "101");
    h.dispatcher.on_coin_state(CoinId(0), stamp(10), &state);

    let posts = h.posts.lock().unwrap();
    assert_eq!(posts.len(), 2, "cancel post then order post");
    match &posts[0].action {
        VenueAction::CancelByCloid { cancels } => assert_eq!(cancels.len(), 1),
        other => panic!("expected cancelByCloid first, got {other:?}"),
    }
    assert_eq!(posts[0].cloids.as_slice(), &[resting]);
    match &posts[1].action {
        VenueAction::Order { orders, .. } => assert_eq!(orders.len(), 2),
        other => panic!("expected bulk order second, got {other:?}"),
    }
    // The two places got distinct engine-assigned cloids and live orders.
    assert_eq!(posts[1].cloids.len(), 2);
    assert_ne!(posts[1].cloids[0], posts[1].cloids[1]);
    for c in &posts[1].cloids {
        let order = h.dispatcher.orders.get(*c).expect("live order inserted");
        assert_eq!(order.state, OrderState::PendingNew);
        assert_eq!(order.req_id, Some(posts[1].req_id));
    }
}

#[test]
fn built_posts_carry_the_frames_recv_mono_ns() {
    let strategy = Recording::new("test", CoinId(0))
        .with_script(vec![Action::Place(intent("BTC", Side::Buy))]);
    let mut h = harness(vec![Box::new(strategy)], false);

    let state = state_with("100", "101");
    h.dispatcher.on_coin_state(CoinId(0), stamp(4242), &state);

    let posts = h.posts.lock().unwrap();
    assert!(!posts.is_empty(), "the place built one post");
    for post in posts.iter() {
        assert_eq!(
            post.recv_mono_ns, 4242,
            "each post must carry the triggering frame's read time"
        );
    }
}

#[test]
fn aggressive_buy_prices_from_the_ask_when_the_bid_side_is_empty() {
    let strategy = Recording::new("test", CoinId(0))
        .with_script(vec![Action::Place(aggressive("BTC", Side::Buy))]);
    let mut h = harness(vec![Box::new(strategy)], false);

    // The bid side is empty, but a buy only needs the ask.
    let state = state_with_sides(None, Some("100"));
    h.dispatcher.on_coin_state(CoinId(0), stamp(10), &state);

    let posts = h.posts.lock().unwrap();
    assert_eq!(posts.len(), 1, "the buy was not dropped");
    let px = post_order_price(&posts[0]);
    assert!(
        px >= ds("100"),
        "buy must price at or beyond the ask, got {px}"
    );
}

#[test]
fn aggressive_sell_prices_from_the_bid_when_the_ask_side_is_empty() {
    let strategy = Recording::new("test", CoinId(0))
        .with_script(vec![Action::Place(aggressive("BTC", Side::Sell))]);
    let mut h = harness(vec![Box::new(strategy)], false);

    // The ask side is empty, but a sell only needs the bid.
    let state = state_with_sides(Some("100"), None);
    h.dispatcher.on_coin_state(CoinId(0), stamp(10), &state);

    let posts = h.posts.lock().unwrap();
    assert_eq!(posts.len(), 1, "the sell was not dropped");
    let px = post_order_price(&posts[0]);
    assert!(
        px <= ds("100"),
        "sell must price at or below the bid, got {px}"
    );
}

#[test]
fn aggressive_order_is_dropped_when_both_sides_are_empty() {
    let strategy = Recording::new("test", CoinId(0))
        .with_script(vec![Action::Place(aggressive("BTC", Side::Buy))]);
    let mut h = harness(vec![Box::new(strategy)], false);

    let state = state_with_sides(None, None);
    h.dispatcher.on_coin_state(CoinId(0), stamp(10), &state);

    assert!(
        h.posts.lock().unwrap().is_empty(),
        "no reference price means no post"
    );
    assert!(
        h.dispatcher.orders.is_empty(),
        "the dropped place left no live order"
    );
}

#[test]
fn kill_switch_cancels_every_working_cloid_then_rejects_places() {
    let strategy = Recording::new("test", CoinId(0));
    let mut h = harness(vec![Box::new(strategy)], false);
    let a = cloid(1);
    let b = cloid(2);
    h.dispatcher.orders.insert(live(a, 0, OrderState::Resting));
    h.dispatcher
        .orders
        .insert(live(b, 0, OrderState::PartiallyFilled));

    let state = state_with("100", "101");
    h.dispatcher
        .on_account_state(&AccountUpdate::Control(Control::KillSwitch), &state);
    assert!(h.dispatcher.risk.kill().is_active());

    {
        let posts = h.posts.lock().unwrap();
        assert_eq!(posts.len(), 1);
        match &posts[0].action {
            VenueAction::CancelByCloid { cancels } => assert_eq!(cancels.len(), 2),
            other => panic!("expected cancelByCloid, got {other:?}"),
        }
    }

    // A new place is refused: buffered directly, then run through the gate.
    h.dispatcher.actions.place(intent("BTC", Side::Buy));
    h.dispatcher.run_actions(stamp(10), &state, 0, 0);
    assert_eq!(
        h.posts.lock().unwrap().len(),
        1,
        "no new order post after kill"
    );
}

#[test]
fn order_update_and_fill_reach_the_owning_strategy() {
    let strategy = Recording::new("test", CoinId(0))
        .with_script(vec![Action::Place(intent("BTC", Side::Buy))]);
    let events = strategy.events();
    let mut h = harness(vec![Box::new(strategy)], false);
    let state = state_with("100", "101");
    h.dispatcher.on_coin_state(CoinId(0), stamp(10), &state);

    let placed = *h
        .dispatcher
        .orders
        .iter()
        .next()
        .map(|order| &order.cloid)
        .unwrap();

    h.dispatcher.on_account_state(
        &AccountUpdate::OrderUpdate {
            stamp: stamp(20),
            cloid: placed,
            oid: 7,
            status: VenueOrderStatus::Resting,
            filled_sz: Decimal::ZERO,
            avg_px: Decimal::ZERO,
        },
        &state,
    );
    h.dispatcher.on_account_state(
        &AccountUpdate::Fill {
            stamp: stamp(21),
            cloid: Some(placed),
            oid: 7,
            tid: 100,
            coin: CoinId(0),
            side: Side::Buy,
            px: ds("100"),
            sz: ds("0.5"),
            fee: ds("0.01"),
            liquidation: false,
        },
        &state,
    );

    let events = events.lock().unwrap();
    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(
        events[0].kind,
        OrderEventKind::Status(VenueOrderStatus::Resting)
    );
    assert_eq!(events[1].kind, OrderEventKind::Fill);
    assert_eq!(
        h.dispatcher.account.position_szi(CoinId(0)),
        ds("0.5"),
        "perp fill updates the position"
    );
}

fn fill_update(tid: u64, cloid: Option<Cloid>, oid: u64, sz: &str) -> AccountUpdate {
    AccountUpdate::Fill {
        stamp: stamp(tid),
        cloid,
        oid,
        tid,
        coin: CoinId(0),
        side: Side::Buy,
        px: ds("100"),
        sz: ds(sz),
        fee: Decimal::ZERO,
        liquidation: false,
    }
}

fn snapshot_fills(fills: &[(u64, u64, &str)]) -> AccountUpdate {
    AccountUpdate::Fills {
        stamp: stamp(1),
        fills: fills
            .iter()
            .map(|(tid, oid, sz)| crate::types::FillData {
                cloid: None,
                oid: *oid,
                tid: *tid,
                coin: CoinId(0),
                side: Side::Buy,
                px: ds("100"),
                sz: ds(sz),
                fee: Decimal::ZERO,
                liquidation: false,
            })
            .collect(),
    }
}

#[test]
fn same_fill_delivered_twice_moves_the_position_once() {
    let mut h = harness(Vec::new(), false);
    let state = state_with("100", "101");
    h.dispatcher
        .on_account_state(&fill_update(5, None, 1, "1"), &state);
    // A reconnect snapshot redelivers the same tid: it must be skipped.
    h.dispatcher
        .on_account_state(&snapshot_fills(&[(5, 1, "1")]), &state);
    assert_eq!(
        h.dispatcher.account.position_szi(CoinId(0)),
        ds("1"),
        "a fill seen twice moves the position once"
    );
}

#[test]
fn first_connect_snapshot_does_not_move_the_position() {
    let mut h = harness(Vec::new(), false);
    let state = state_with("100", "101");
    h.dispatcher
        .on_account_state(&snapshot_fills(&[(1, 1, "1"), (2, 2, "1")]), &state);
    assert_eq!(
        h.dispatcher.account.position_szi(CoinId(0)),
        Decimal::ZERO,
        "the first snapshot is already in the starting position"
    );
    // Its tids are recorded, so a live redelivery is skipped too.
    h.dispatcher
        .on_account_state(&fill_update(1, None, 1, "1"), &state);
    assert_eq!(h.dispatcher.account.position_szi(CoinId(0)), Decimal::ZERO);
}

#[test]
fn reconnect_snapshot_applies_only_new_tids() {
    let mut h = harness(Vec::new(), false);
    let state = state_with("100", "101");
    // First snapshot: skipped, tids 1 and 2 recorded.
    h.dispatcher
        .on_account_state(&snapshot_fills(&[(1, 1, "1"), (2, 2, "2")]), &state);
    // A live fill ends the first-connect phase and moves the position.
    h.dispatcher
        .on_account_state(&fill_update(3, None, 3, "1"), &state);
    assert_eq!(h.dispatcher.account.position_szi(CoinId(0)), ds("1"));
    // Reconnect snapshot: 2 and 3 already seen; only the new tid 4 applies.
    h.dispatcher.on_account_state(
        &snapshot_fills(&[(2, 2, "2"), (3, 3, "1"), (4, 4, "1")]),
        &state,
    );
    assert_eq!(h.dispatcher.account.position_szi(CoinId(0)), ds("2"));
}

#[test]
fn fill_maps_to_order_by_oid_and_updates_filled_sz() {
    let strategy = Recording::new("test", CoinId(0))
        .with_script(vec![Action::Place(intent("BTC", Side::Buy))]);
    let events = strategy.events();
    let mut h = harness(vec![Box::new(strategy)], false);
    let state = state_with("100", "101");
    h.dispatcher.on_coin_state(CoinId(0), stamp(10), &state);
    let placed = *h
        .dispatcher
        .orders
        .iter()
        .next()
        .map(|order| &order.cloid)
        .unwrap();

    h.dispatcher.on_account_state(
        &AccountUpdate::OrderUpdate {
            stamp: stamp(20),
            cloid: placed,
            oid: 7,
            status: VenueOrderStatus::Resting,
            filled_sz: Decimal::ZERO,
            avg_px: Decimal::ZERO,
        },
        &state,
    );
    // The fill carries no cloid: it is mapped through the oid index.
    h.dispatcher
        .on_account_state(&fill_update(9, None, 7, "0.5"), &state);

    let order = h.dispatcher.orders.get(placed).unwrap();
    assert_eq!(order.filled_sz, ds("0.5"));
    assert!(
        events
            .lock()
            .unwrap()
            .iter()
            .any(|event| event.kind == OrderEventKind::Fill),
        "the owning strategy receives the fill"
    );
}

#[test]
fn fill_after_terminal_update_and_pruning_still_reaches_the_owner() {
    let strategy = Recording::new("test", CoinId(0))
        .with_script(vec![Action::Place(intent("BTC", Side::Buy))]);
    let events = strategy.events();
    let mut h = harness(vec![Box::new(strategy)], false);
    let state = state_with("100", "101");
    h.dispatcher.on_coin_state(CoinId(0), stamp(10), &state);
    let placed = *h
        .dispatcher
        .orders
        .iter()
        .next()
        .map(|order| &order.cloid)
        .unwrap();

    h.dispatcher.on_account_state(
        &AccountUpdate::OrderUpdate {
            stamp: stamp(20),
            cloid: placed,
            oid: 7,
            status: VenueOrderStatus::Filled,
            filled_sz: Decimal::ONE,
            avg_px: ds("100"),
        },
        &state,
    );
    assert!(h.dispatcher.orders.get(placed).unwrap().state.is_terminal());
    // The loop prunes the terminal order and remembers its oid route.
    h.dispatcher.record_iteration(0, 0);
    assert!(h.dispatcher.orders.get(placed).is_none());

    h.dispatcher
        .on_account_state(&fill_update(11, None, 7, "1"), &state);
    assert!(
        events
            .lock()
            .unwrap()
            .iter()
            .any(|event| event.kind == OrderEventKind::Fill),
        "a late fill is still delivered after pruning"
    );
    assert_eq!(h.dispatcher.account.position_szi(CoinId(0)), ds("1"));
}

#[test]
fn post_ack_routes_statuses_to_the_right_cloids() {
    let strategy = Recording::new("test", CoinId(0)).with_script(vec![
        Action::Place(intent("BTC", Side::Buy)),
        Action::Place(intent("BTC", Side::Sell)),
    ]);
    let events = strategy.events();
    let mut h = harness(vec![Box::new(strategy)], false);
    let state = state_with("100", "101");
    h.dispatcher.on_coin_state(CoinId(0), stamp(10), &state);

    let (req_id, cloids) = {
        let post = h.posts.lock().unwrap().first().unwrap().clone();
        (post.req_id, post.cloids.clone())
    };
    h.dispatcher.on_account_state(
        &AccountUpdate::PostAck {
            stamp: stamp(30),
            req_id,
            result: PostResult::Statuses(smallvec::smallvec![
                crate::types::OrderAck {
                    status: VenueOrderStatus::Resting,
                    oid: None,
                },
                crate::types::OrderAck {
                    status: VenueOrderStatus::Filled,
                    oid: None,
                },
            ]),
        },
        &state,
    );
    assert_eq!(
        h.dispatcher.orders.get(cloids[0]).unwrap().state,
        OrderState::Resting
    );
    assert_eq!(
        h.dispatcher.orders.get(cloids[1]).unwrap().state,
        OrderState::Filled
    );
    assert_eq!(events.lock().unwrap().len(), 2);
}

#[test]
fn post_ack_error_marks_orders_unknown_and_trips_the_breaker() {
    let strategy = Recording::new("test", CoinId(0))
        .with_script(vec![Action::Place(intent("BTC", Side::Buy))]);
    let mut h = harness(vec![Box::new(strategy)], false);
    let state = state_with("100", "101");
    h.dispatcher.on_coin_state(CoinId(0), stamp(10), &state);

    let (req_id, cloids) = {
        let post = h.posts.lock().unwrap().first().unwrap().clone();
        (post.req_id, post.cloids.clone())
    };
    h.dispatcher.on_account_state(
        &AccountUpdate::PostAck {
            stamp: stamp(30),
            req_id,
            result: PostResult::Error("boom".into()),
        },
        &state,
    );
    assert!(h.dispatcher.risk.breakers().is_tripped());
    assert_eq!(h.dispatcher.risk.breakers().label(), Some("exec_error"));
    // A lost reply is not a rejection: the order may be resting.
    assert_eq!(
        h.dispatcher.orders.get(cloids[0]).unwrap().state,
        OrderState::Unknown
    );
    // It still counts for the dead-man switch and exposure.
    assert_eq!(h.dispatcher.orders.resting_count(), 1);
}

#[test]
fn resolving_an_unknown_order_clears_the_exec_breaker() {
    let strategy = Recording::new("test", CoinId(0))
        .with_script(vec![Action::Place(intent("BTC", Side::Buy))]);
    let mut h = harness(vec![Box::new(strategy)], false);
    let state = state_with("100", "101");
    h.dispatcher.on_coin_state(CoinId(0), stamp(10), &state);

    let (req_id, cloids) = {
        let post = h.posts.lock().unwrap().first().unwrap().clone();
        (post.req_id, post.cloids.clone())
    };
    h.dispatcher.on_account_state(
        &AccountUpdate::PostAck {
            stamp: stamp(30),
            req_id,
            result: PostResult::Error("boom".into()),
        },
        &state,
    );
    assert!(h.dispatcher.risk.breakers().is_tripped());

    // The exec layer resolves it by cloid (`orderStatus`): it was resting.
    h.dispatcher.on_account_state(
        &AccountUpdate::OrderUpdate {
            stamp: stamp(40),
            cloid: cloids[0],
            oid: 7,
            status: VenueOrderStatus::Resting,
            filled_sz: Decimal::ZERO,
            avg_px: Decimal::ZERO,
        },
        &state,
    );
    assert_eq!(
        h.dispatcher.orders.get(cloids[0]).unwrap().state,
        OrderState::Resting
    );
    assert!(!h.dispatcher.risk.breakers().is_tripped());
}

fn order_status_response(status: &str) -> hl_arb_client::OrderStatusResponse {
    hl_arb_client::OrderStatusResponse {
        status: "order".into(),
        order: Some(hl_arb_client::OrderStatusOrder {
            order: Some(hl_arb_client::OpenOrder {
                coin: "BTC".into(),
                oid: 7,
                side: "B".into(),
                limit_px: ds("100"),
                sz: ds("1"),
                orig_sz: ds("1"),
                timestamp: 0,
                cloid: None,
                reduce_only: false,
            }),
            status: status.into(),
            status_timestamp: 0,
        }),
    }
}

#[test]
fn rejected_post_ack_marks_orders_rejected_and_leaves_the_breaker_clear() {
    let strategy = Recording::new("test", CoinId(0))
        .with_script(vec![Action::Place(intent("BTC", Side::Buy))]);
    let mut h = harness(vec![Box::new(strategy)], false);
    let state = state_with("100", "101");
    h.dispatcher.on_coin_state(CoinId(0), stamp(10), &state);

    let (req_id, cloids) = {
        let post = h.posts.lock().unwrap().first().unwrap().clone();
        (post.req_id, post.cloids.clone())
    };
    h.dispatcher.on_account_state(
        &AccountUpdate::PostAck {
            stamp: stamp(30),
            req_id,
            result: PostResult::Rejected("rate limited".into()),
        },
        &state,
    );
    // A definitive failure is terminal, not Unknown.
    assert!(matches!(
        h.dispatcher.orders.get(cloids[0]).unwrap().state,
        OrderState::Rejected(_)
    ));
    assert!(!h.dispatcher.orders.has_unknown());
    assert!(!h.dispatcher.risk.breakers().is_tripped());
}

#[test]
fn resolve_unknown_via_order_status_clears_the_breaker() {
    let strategy = Recording::new("test", CoinId(0))
        .with_script(vec![Action::Place(intent("BTC", Side::Buy))]);
    let mut h = harness(vec![Box::new(strategy)], false);
    let state = state_with("100", "101");
    h.dispatcher.on_coin_state(CoinId(0), stamp(10), &state);

    let (req_id, cloids) = {
        let post = h.posts.lock().unwrap().first().unwrap().clone();
        (post.req_id, post.cloids.clone())
    };
    h.dispatcher.on_account_state(
        &AccountUpdate::PostAck {
            stamp: stamp(30),
            req_id,
            result: PostResult::Error("lost reply".into()),
        },
        &state,
    );
    assert!(h.dispatcher.risk.breakers().is_tripped());

    h.dispatcher.on_account_state(
        &AccountUpdate::ResolveUnknown {
            stamp: stamp(40),
            cloid: cloids[0],
            status: order_status_response("open"),
        },
        &state,
    );
    assert_eq!(
        h.dispatcher.orders.get(cloids[0]).unwrap().state,
        OrderState::Resting
    );
    assert!(!h.dispatcher.risk.breakers().is_tripped());
}

#[test]
fn unknown_expired_marks_rejected_and_clears_the_breaker() {
    let strategy = Recording::new("test", CoinId(0))
        .with_script(vec![Action::Place(intent("BTC", Side::Buy))]);
    let mut h = harness(vec![Box::new(strategy)], false);
    let state = state_with("100", "101");
    h.dispatcher.on_coin_state(CoinId(0), stamp(10), &state);

    let (req_id, cloids) = {
        let post = h.posts.lock().unwrap().first().unwrap().clone();
        (post.req_id, post.cloids.clone())
    };
    h.dispatcher.on_account_state(
        &AccountUpdate::PostAck {
            stamp: stamp(30),
            req_id,
            result: PostResult::Error("lost reply".into()),
        },
        &state,
    );
    assert!(h.dispatcher.orders.has_unknown());

    // The bounded `orderStatus` retries found nothing: terminal now.
    h.dispatcher.on_account_state(
        &AccountUpdate::UnknownExpired {
            stamp: stamp(50),
            cloid: cloids[0],
        },
        &state,
    );
    assert!(matches!(
        h.dispatcher.orders.get(cloids[0]).unwrap().state,
        OrderState::Rejected(_)
    ));
    assert!(!h.dispatcher.orders.has_unknown());
    assert!(!h.dispatcher.risk.breakers().is_tripped());
}

#[test]
fn resolving_unknowns_keeps_exec_backpressure_tripped() {
    let strategy = Recording::new("test", CoinId(0))
        .with_script(vec![Action::Place(intent("BTC", Side::Buy))]);
    let mut h = harness(vec![Box::new(strategy)], false);
    let state = state_with("100", "101");
    h.dispatcher.on_coin_state(CoinId(0), stamp(10), &state);

    let (req_id, cloids) = {
        let post = h.posts.lock().unwrap().first().unwrap().clone();
        (post.req_id, post.cloids.clone())
    };
    // A lost reply trips exec_error and marks the order Unknown.
    h.dispatcher.on_account_state(
        &AccountUpdate::PostAck {
            stamp: stamp(30),
            req_id,
            result: PostResult::Error("lost reply".into()),
        },
        &state,
    );
    // A later, independent backpressure trip.
    h.dispatcher.risk.breakers_mut().trip("exec_backpressure");

    // Resolving the Unknown clears exec_error only.
    h.dispatcher.on_account_state(
        &AccountUpdate::ResolveUnknown {
            stamp: stamp(40),
            cloid: cloids[0],
            status: order_status_response("open"),
        },
        &state,
    );
    assert!(!h.dispatcher.orders.has_unknown());
    assert!(
        !h.dispatcher.risk.breakers().is_label_tripped("exec_error"),
        "exec_error clears once the Unknowns resolve"
    );
    assert!(
        h.dispatcher
            .risk
            .breakers()
            .is_label_tripped("exec_backpressure"),
        "exec_backpressure is a separate breaker and stays tripped"
    );
    assert!(h.dispatcher.risk.breakers().is_tripped());
}

#[test]
fn stale_order_status_after_a_stream_update_is_ignored() {
    let strategy = Recording::new("test", CoinId(0))
        .with_script(vec![Action::Place(intent("BTC", Side::Buy))]);
    let mut h = harness(vec![Box::new(strategy)], false);
    let state = state_with("100", "101");
    h.dispatcher.on_coin_state(CoinId(0), stamp(10), &state);
    let placed = *h
        .dispatcher
        .orders
        .iter()
        .next()
        .map(|order| &order.cloid)
        .unwrap();

    // A stream update already resolved the order to Resting.
    h.dispatcher.on_account_state(
        &AccountUpdate::OrderUpdate {
            stamp: stamp(20),
            cloid: placed,
            oid: 7,
            status: VenueOrderStatus::Resting,
            filled_sz: Decimal::ZERO,
            avg_px: Decimal::ZERO,
        },
        &state,
    );
    // A stale `orderStatus` answer saying it was canceled must not apply.
    h.dispatcher.on_account_state(
        &AccountUpdate::ResolveUnknown {
            stamp: stamp(30),
            cloid: placed,
            status: order_status_response("canceled"),
        },
        &state,
    );
    assert_eq!(
        h.dispatcher.orders.get(placed).unwrap().state,
        OrderState::Resting
    );
}

#[test]
fn backpressure_fails_closed_and_trips_the_exec_breaker() {
    let strategy = Recording::new("test", CoinId(0))
        .with_script(vec![Action::Place(intent("BTC", Side::Buy))]);
    let mut h = harness(vec![Box::new(strategy)], true);
    let state = state_with("100", "101");
    h.dispatcher.on_coin_state(CoinId(0), stamp(10), &state);

    assert!(h.posts.lock().unwrap().is_empty(), "nothing was sent");
    assert_eq!(
        h.dispatcher.risk.breakers().label(),
        Some("exec_backpressure")
    );
    assert!(matches!(
        h.dispatcher.orders.iter().next().unwrap().state,
        OrderState::Rejected(_)
    ));
}

#[test]
fn reduce_only_still_places_when_the_account_stream_is_halted() {
    let mut reduce = intent("BTC", Side::Sell);
    reduce.reduce_only = true;
    let strategy = Recording::new("test", CoinId(0)).with_script(vec![
        Action::Place(intent("BTC", Side::Buy)),
        Action::Place(reduce),
    ]);
    let mut h = harness(vec![Box::new(strategy)], false);
    h.dispatcher.on_gap();
    let state = state_with("100", "101");
    h.dispatcher.on_coin_state(CoinId(0), stamp(10), &state);

    let posts = h.posts.lock().unwrap();
    assert_eq!(posts.len(), 1);
    match &posts[0].action {
        VenueAction::Order { orders, .. } => {
            assert_eq!(
                orders.len(),
                1,
                "the increase was dropped, reduce-only kept"
            );
        }
        other => panic!("expected order post, got {other:?}"),
    }
}

#[test]
fn apply_reconcile_updates_margin_and_resumes_places() {
    let mut h = harness(Vec::new(), false);
    h.dispatcher.on_gap();
    assert!(h.dispatcher.stream.places_halted());
    h.dispatcher.apply_reconcile(
        &AccountSnapshot {
            account_value: ds("1000"),
            margin_used: ds("250"),
        },
        false,
        false,
    );
    assert_eq!(h.dispatcher.account.account_value, ds("1000"));
    assert_eq!(h.dispatcher.account.margin_used, ds("250"));
    assert!(!h.dispatcher.stream.places_halted());
}

#[test]
fn paper_backend_fills_a_taker_and_updates_the_account() {
    use std::thread::sleep;
    use std::time::Duration;

    use hl_arb_strategy::{AccountView, FeeRates, Instrument};

    use crate::paper_exec::{PaperConfig, PaperExec};

    // A Gtc buy above the ask takes liquidity once its latency elapses.
    let mut buy = intent("BTC", Side::Buy);
    buy.limit_px = Some(ds("101"));
    buy.tif = TimeInForce::Gtc;
    let strategy = Recording::new("test", CoinId(0)).with_script(vec![Action::Place(buy)]);
    let events = strategy.events();

    let mut instruments = BTreeMap::new();
    instruments.insert("BTC".to_string(), Instrument::perp());
    let paper = PaperExec::new(
        PaperConfig {
            latency_ms: 20,
            maker_fills: true,
        },
        AccountView {
            account_value: ds("1000"),
            ..Default::default()
        },
        instruments,
        FeeRates::PERP,
        FeeRates::SPOT,
    );
    let mut dispatcher = StrategyDispatcher::new(
        vec![Box::new(strategy)],
        registry(),
        table(),
        RiskGate::default(),
        None,
        DispatcherConfig::default(),
    )
    .with_paper(paper);

    let state = state_with("100", "100");
    dispatcher.on_coin_state(CoinId(0), stamp(10), &state);
    // The order is queued but not yet eligible.
    assert_eq!(dispatcher.account().position_szi(CoinId(0)), Decimal::ZERO);

    sleep(Duration::from_millis(25));
    dispatcher.on_coin_state(CoinId(0), stamp(30), &state);
    assert_eq!(
        dispatcher.account().position_szi(CoinId(0)),
        Decimal::ONE,
        "paper taker should fill and move the position"
    );
    assert!(
        events
            .lock()
            .unwrap()
            .iter()
            .any(|event| event.kind == OrderEventKind::Fill),
        "the owning strategy should receive the fill"
    );
}

#[test]
fn journal_records_approved_actions_and_fills_on_replay_time() {
    use std::sync::Arc;

    use hl_arb_strategy::{AccountView, FeeRates, Instrument};

    use crate::clock::ReplayClock;
    use crate::journal::{JournalEntry, SharedSink};
    use crate::paper_exec::{PaperConfig, PaperExec};

    let mut buy = intent("BTC", Side::Buy);
    buy.limit_px = Some(ds("101"));
    buy.tif = TimeInForce::Gtc;
    let strategy = Recording::new("test", CoinId(0)).with_script(vec![Action::Place(buy)]);

    let mut instruments = BTreeMap::new();
    instruments.insert("BTC".to_string(), Instrument::perp());
    let paper = PaperExec::new(
        PaperConfig {
            latency_ms: 20,
            maker_fills: true,
        },
        AccountView {
            account_value: ds("1000"),
            ..Default::default()
        },
        instruments,
        FeeRates::PERP,
        FeeRates::SPOT,
    );
    let sink = SharedSink::new();
    let clock = Arc::new(ReplayClock::new());
    clock.set_ms(1_000_000, 0);
    let mut dispatcher = StrategyDispatcher::new(
        vec![Box::new(strategy)],
        registry(),
        table(),
        RiskGate::default(),
        None,
        DispatcherConfig::default(),
    )
    .with_paper(paper)
    .with_clock(clock.clone())
    .with_journal(Box::new(sink.clone()));

    let state = state_with("100", "100");
    dispatcher.on_coin_state(CoinId(0), stamp(10), &state);
    // Advance replay time past the paper latency (now 21 ms) and dispatch
    // again; the taker fills without any wall-clock sleep.
    clock.set_ms(21_000_000, 20_000_000);
    dispatcher.on_coin_state(CoinId(0), stamp(30), &state);

    let entries = sink.entries();
    assert!(
        matches!(entries.first(), Some(JournalEntry::Place { .. })),
        "the place is journaled first: {entries:?}"
    );
    assert!(
        entries
            .iter()
            .any(|entry| matches!(entry, JournalEntry::Fill { .. })),
        "the fill is journaled: {entries:?}"
    );
}

/// The same event timeline produces the same journal whether or not the
/// caller stalls between iterations. This is the property that makes
/// `simulate` over live data and `replay` over the same recorded window
/// agree (SPEC-0010 E-7 done-when): decisions use event time, not the wall
/// clock. The clock is advanced to the *same* event times in both runs; only
/// the real-time gap differs.
#[test]
fn replay_is_independent_of_wall_clock_delays() {
    use std::sync::Arc;
    use std::time::Duration;

    use hl_arb_strategy::{AccountView, FeeRates, Instrument};

    use crate::clock::ReplayClock;
    use crate::journal::{JournalEntry, SharedSink};
    use crate::paper_exec::{PaperConfig, PaperExec};

    fn run_with_delay(delay: bool) -> Vec<JournalEntry> {
        let mut buy = intent("BTC", Side::Buy);
        buy.limit_px = Some(ds("101"));
        buy.tif = TimeInForce::Gtc;
        let strategy = Recording::new("test", CoinId(0)).with_script(vec![Action::Place(buy)]);

        let mut instruments = BTreeMap::new();
        instruments.insert("BTC".to_string(), Instrument::perp());
        let paper = PaperExec::new(
            PaperConfig {
                latency_ms: 20,
                maker_fills: true,
            },
            AccountView {
                account_value: ds("1000"),
                ..Default::default()
            },
            instruments,
            FeeRates::PERP,
            FeeRates::SPOT,
        );
        let sink = SharedSink::new();
        let clock = Arc::new(ReplayClock::new());
        clock.set_ms(1_000_000, 0);
        let mut dispatcher = StrategyDispatcher::new(
            vec![Box::new(strategy)],
            registry(),
            table(),
            RiskGate::default(),
            None,
            DispatcherConfig::default(),
        )
        .with_paper(paper)
        .with_clock(clock.clone())
        .with_cloid_prefix(0)
        .with_journal(Box::new(sink.clone()));

        let state = state_with("100", "100");
        dispatcher.on_coin_state(CoinId(0), stamp(10), &state);
        if delay {
            std::thread::sleep(Duration::from_millis(30));
        }
        clock.set_ms(21_000_000, 20_000_000);
        dispatcher.on_coin_state(CoinId(0), stamp(30), &state);
        sink.entries()
    }

    let without_delay = run_with_delay(false);
    let with_delay = run_with_delay(true);
    assert!(!without_delay.is_empty(), "the run should journal actions");
    assert_eq!(
        without_delay, with_delay,
        "wall-clock stalls must not change the journal"
    );
}

/// SPEC-0004 K-3 end-to-end in `simulate`: a kill-switch trigger cancels
/// every working order and places nothing new within one iteration.
#[test]
fn simulate_kill_switch_cancels_all_and_places_none() {
    use hl_arb_strategy::{AccountView, FeeRates, Instrument};

    use crate::paper_exec::{PaperConfig, PaperExec};

    // A resting Gtc buy: its limit is below the bid, so it does not cross.
    let mut buy = intent("BTC", Side::Buy);
    buy.limit_px = Some(ds("99"));
    buy.tif = hl_arb_strategy::TimeInForce::Gtc;
    let strategy = Recording::new("test", CoinId(0)).with_script(vec![Action::Place(buy)]);

    let mut instruments = BTreeMap::new();
    instruments.insert("BTC".to_string(), Instrument::perp());
    let paper = PaperExec::new(
        PaperConfig {
            latency_ms: 20,
            maker_fills: true,
        },
        AccountView {
            account_value: ds("1000"),
            ..Default::default()
        },
        instruments,
        FeeRates::PERP,
        FeeRates::SPOT,
    );
    let mut dispatcher = StrategyDispatcher::new(
        vec![Box::new(strategy)],
        registry(),
        table(),
        RiskGate::default(),
        None,
        DispatcherConfig::default(),
    )
    .with_paper(paper);
    let state = state_with("100", "101");

    // One iteration places one order (still pending in the paper backend).
    dispatcher.on_coin_state(CoinId(0), stamp(10), &state);
    assert_eq!(dispatcher.orders.working().count(), 1, "one order placed");

    // Trigger: kill switch. One further iteration applies the paper cancel.
    dispatcher.on_account_state(&AccountUpdate::Control(Control::KillSwitch), &state);
    dispatcher.on_coin_state(CoinId(0), stamp(20), &state);
    assert_eq!(
        dispatcher.orders.working().count(),
        0,
        "all orders cancelled within one iteration"
    );

    // And nothing new is placed while killed.
    let before = dispatcher.orders.len();
    dispatcher.on_coin_state(CoinId(0), stamp(30), &state);
    assert_eq!(
        dispatcher.orders.len(),
        before,
        "no new places while killed"
    );
}

#[test]
fn timer_routes_to_the_registered_strategy() {
    let mut strategy = Recording::new("test", CoinId(0));
    let calls = strategy.calls();
    strategy.emit_on_market = false;
    let mut h = harness(vec![Box::new(strategy)], false);
    h.dispatcher
        .register_timer(HeapTimerId(3), 0, StratTimerId(7));
    let state = state_with("100", "101");
    h.dispatcher
        .on_timer_state(HeapTimerId(3), stamp(10), &state);
    // on_timer is a no-op for Recording, but routing must not panic and the
    // strategy must not be called via on_market.
    assert!(calls.lock().unwrap().is_empty());
}

#[test]
fn interests_aggregate_to_route_form() {
    let a = Recording::new("a", CoinId(0));
    let b = Recording::new("b", CoinId(1));
    let h = harness(vec![Box::new(a), Box::new(b)], false);
    let interests = h.dispatcher.interests();
    assert_eq!(interests.len(), 2);
    assert_eq!(interests[0].coins, vec![CoinId(0)]);
    assert_eq!(interests[1].coins, vec![CoinId(1)]);
}

#[test]
fn owner_map_survives_a_cancel_and_dropped_place() {
    // A place that the builder drops (min notional) leaves no live order.
    let mut tiny = intent("BTC", Side::Buy);
    tiny.size = ds("0.0001");
    let strategy = Recording::new("test", CoinId(0)).with_script(vec![Action::Place(tiny)]);
    let mut h = harness(vec![Box::new(strategy)], false);
    let state = state_with("100", "101");
    h.dispatcher.on_coin_state(CoinId(0), stamp(10), &state);
    assert!(h.posts.lock().unwrap().is_empty());
    assert!(
        h.dispatcher.orders.is_empty(),
        "dropped place left no order"
    );
}

#[test]
fn assigner_cloids_are_unique_across_places() {
    let assigner = CloidAssigner::new();
    let first = assigner.next();
    let second = assigner.next();
    assert_ne!(first, second);
}
