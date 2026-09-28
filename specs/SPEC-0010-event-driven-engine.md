# SPEC-0010 — Event-Driven Engine & Hot Path

**Status:** Draft
**Milestone:** M4 (engine), **Tier T1** ([`docs/GOAL.md`](../docs/GOAL.md) §2.1). It serves every arb.
**Depends on:** SPEC-0001 (market data), SPEC-0002 incl. §17 (H-1 concurrent WS post, H-2 mandatory `cloid`, H-3 account stream), SPEC-0008 R-3 (`RawWsConn`) and R-7 (segment reader, for replay).
**Supersedes:** the decision loop in SPEC-0003 §8 and the current tick engine in `crates/mev-bot/src/engine.rs`.
**Blocks:** SPEC-0011 (multi-leg execution), M5 (first strategy live), all T1 strategies.

---

## 0. How to use this spec

1. Read [`docs/GOAL.md`](../docs/GOAL.md), especially **§5 (Latency first)**, and [`AGENTS.md`](../AGENTS.md).
2. Pick a task from **§20**. Respect its dependencies and do exactly its **Do**; it's finished when every **Done when** item holds. Tick the status in the same commit.
3. Every hot-path change needs a benchmark or latency-histogram evidence (GOAL §4.7). A latency regression is a bug.
4. If something here is wrong or unclear, write it under §22 and stop. Don't improvise on the hot path.

## 1. Purpose

Replace the 1-second polling engine with a **single-threaded, event-driven engine** that reacts to every market and account event immediately. It must run the full decision → risk → build → sign → send path in **microseconds**, and it must run **the same code** in `live`, `simulate`, and deterministic **replay** over recorded data.

## 2. Why: the current engine (as of commit `b3718e0`)

| # | Current behavior | Where | Consequence |
|---|---|---|---|
| 1 | Decides on a fixed `interval(1s)` tick, not on events | `engine.rs` `Engine::run` | Reacts on average 500 ms, and up to 1 s, after a price change. Fatal for any arb. |
| 2 | Clones the whole `MarketView` and `AccountView` every tick, under `RwLock`s | `engine.rs` `step`, `snapshot_market` | Allocation and lock traffic on the hot path |
| 3 | Account state comes from REST polling every 5 s | `engine.rs` `account_poller` | Risk sees a position up to 5 s stale, and own orders placed since the last poll are invisible |
| 4 | Strategy trait is `async` (`async_trait`) | `mev-strategy/src/strategy.rs` | A boxed future per call; strategies do no I/O, so async buys nothing |
| 5 | Orders are sent one at a time, each awaited | `engine.rs` `gate_and_execute` → `submit_live` | A 2-leg trade waits a full round trip between legs. `WsExchange` also allows only one post in flight (SPEC-0002 H-1). |
| 6 | Live orders may have `cloid: None` | `submit_live` (`intent.cloid.clone()`) | An unknown outcome can't be reconciled or safely retried (SPEC-0002 H-2) |
| 7 | `exchange.place(...)` result `Ok(_)` is recorded as "submitted" without reading per-order statuses | `submit_live` | Rejected orders (margin, tick, min-notional) look like live orders |
| 8 | Aggressive orders (`limit_px: None`) use the **mid** as the limit | `submit_live` | An IOC at mid usually doesn't fill; a GTC at mid rests unexpectedly |
| 9 | Risk limits are all `Option`, and `None` means unlimited | `mev-risk/src/limits.rs`, `config.rs` | **Fail-open** by default, contradicting SPEC-0004 §4 |
| 10 | Risk checks each intent against the account snapshot only; pending/in-flight orders from the same cycle aren't counted | `LimitRisk::check` | Several intents in one cycle can each pass and together breach a cap |
| 11 | Every tick serializes the whole `AccountView` to JSON for the replay log | `Recorder::record` | CPU on the hot path; duplicates what SPEC-0008 records |

Items 6–10 are **safety** issues on a real-money path. They get quick fixes in task **E-0**, before anything else, even on the old engine.

## 3. Goals and non-goals

**Goals**

| # | Goal | Target (GOAL §5.2) |
|---|---|---|
| G-1 | Event-driven: a decision on **every** relevant event | No timers in the decision path, except strategy timers |
| G-2 | Internal tick-to-order latency (socket read → order bytes written) | **p50 ≤ 100 µs, p99 ≤ 1 ms** on the reference host |
| G-3 | No allocation per market event in steady state | 0 allocations per `bbo` event (measured, §17) |
| G-4 | No locks and no `.await` on the engine thread | Enforced by design (§5) and review |
| G-5 | Same engine code for `live`, `simulate`, `replay` | One engine; pluggable I/O backends (§14) |
| G-6 | Deterministic replay | Same recorded input ⇒ byte-identical action log |
| G-7 | Safe under failure | Feed gaps, exec disconnects, and unknown order outcomes fail closed (§16) |

**Non-goals:** multi-leg execution policy (SPEC-0011); new strategies; kernel-bypass networking; FPGA. Colocation and our own node are SPEC-0009.

## 4. Hot-path rules (specific to this engine)

These refine GOAL §5.1. Reviewers reject changes that break them.

| Rule | Detail |
|---|---|
| One owner | The engine thread owns all trading state (markets, account, orders, strategies, risk). Nothing else mutates it. **No `Arc<RwLock<…>>` of trading state.** |
| No `.await`, no blocking | The engine thread is a plain `std::thread`. It never awaits, never does network or disk I/O, and never takes a lock that another thread can hold for longer than a queue operation. |
| Bounded handoffs only | In: bounded channels from I/O tasks. Out: bounded channels to exec I/O and background writers. Full outbound channel ⇒ fail closed (halt new orders), never block. |
| Interned ids | Coins are `CoinId(u16)` resolved at startup (`AssetMap`); per-coin state lives in `Vec`s indexed by `CoinId`. No `String` keys or string hashing on the hot path. |
| Preallocate | Action buffers, order-wire buffers, msgpack scratch buffers, and book arrays are allocated once and reused. |
| Fixed-size books | Books are fixed arrays of the top N levels (N = 20, per SPEC-0008 V-1), not `BTreeMap`. |
| Serialization off-thread | The engine sends typed records to the persistence/metrics threads; **they** serialize. The engine never calls `serde_json`. |
| Logging | No `tracing` at INFO or above per event. Per-event data goes to counters and histograms; rare events (rejects, gaps) may log at WARN. |
| Time | The engine reads time only through `EngineClock` (§13). Strategies read `ctx.now` only. |

## 5. Architecture

```
 tokio runtime (I/O)                                   engine thread (std::thread, optional pinned core)
 ─────────────────────                                  ───────────────────────────────────────────────
 market WS conns (RawWsConn, SPEC-0008 R-3)             loop {
   read → t_recv → decode → MarketUpdate ──[market ch, bounded, lossy]──►   1. drain control/account/exec ch (lossless) first
 account WS (H-3: orderUpdates/userFills/userEvents)                         2. drain market ch (all available) → apply to state,
   read → decode → AccountUpdate ──────[acct ch, bounded, lossless]────►       mark dirty coins
 exec reader (post replies, H-1)                                             3. fire due timers
   → PostAck{req_id, statuses} ────────[acct ch]───────────────────────►     4. for each dirty coin: dispatch to interested strategies
 REST reconciler (every 30–60 s)                                                → actions → risk → order manager → batch → sign
   → Reconcile{…} ─────────────────────[acct ch]───────────────────────►     5. push signed payloads ──[exec ch]──► exec writer task
 control (kill switch file/signal/CLI)                                            (WS post, H-1) ──► socket (TCP_NODELAY)
   → Control{…} ───────────────────────[acct ch]───────────────────────►     6. push records ──► DbWriter / metrics (try_send)
                                                                             7. nothing pending? spin `spin_us`, then block on
                                                                                recv with timeout = next timer deadline
                                                                           }
```

| Piece | Runs on | Notes |
|---|---|---|
| Market ingest | tokio tasks (one per WS connection) | Decode happens here, off the engine thread. Stamp `t_recv` immediately after the socket read. |
| Account ingest | tokio task | H-3 stream; lossless |
| Exec writer/reader | tokio tasks (H-1 design) | The writer receives already-signed payloads from the engine and writes them; the reader routes replies back as `PostAck` |
| REST reconciler | tokio task | `clearinghouseState`, `openOrders`, `spotClearinghouseState` on a cadence and after reconnects; results arrive as `Reconcile` events |
| Engine | one `std::thread` | Owns state; everything in §6–§12 |
| Persistence / metrics export | existing `DbWriter` thread; a metrics-export task | Receive typed records |

**Channels:** use `crossbeam-channel` (bounded, MPMC, parks efficiently, non-blocking `try_send` from async tasks). Engine → exec uses `tokio::sync::mpsc` (`try_send` works from a non-async thread and wakes the exec task). E-2 benchmarks the handoff latency; alternatives (SPSC rings such as `rtrb` plus manual `unpark`) are only adopted if the bench shows > 10 µs p99 handoff.

**Conflation:** HL `bbo`/`l2Book` messages are full snapshots, so the engine drains **all** available market messages, applies each (cheap: overwrite a slot), and then evaluates strategies **once per dirty coin**. Under load, several updates for a coin collapse into one decision on the latest state. If the market channel is full, the producer drops the **new** message and increments `hl_engine_market_drops_total{coin}`; the next snapshot repairs state. Trades aren't snapshots: dropped trades are counted, and strategies that need a complete tape must say so in `interests()` (§8) so the planner gives them a dedicated lossless channel.

## 6. Events

```rust
pub struct Stamp {
    pub t_recv_ns: i64,     // wall clock at socket read (or recorded t_ns in replay)
    pub mono_ns: u64,       // monotonic at socket read
    pub ts_exch_ms: u64,    // venue timestamp if present, else 0
}

pub enum MarketUpdate {           // produced by ingest tasks
    Bbo   { coin: CoinId, stamp: Stamp, bid: Level, ask: Level },
    Book  { coin: CoinId, stamp: Stamp, book: BookSnapshot },        // fixed arrays
    Trades{ coin: CoinId, stamp: Stamp, trades: SmallVec<[Trade; 8]> },
    Ctx   { coin: CoinId, stamp: Stamp, ctx: AssetCtxLite },          // funding, mark, oracle, OI
    Gap   { conn: ConnId, stamp: Stamp, open: bool },                 // feed gap start/end
}

pub enum AccountUpdate {          // lossless
    OrderUpdate { stamp: Stamp, cloid: Cloid, oid: u64, status: VenueOrderStatus, filled_sz: Sz, avg_px: Px },
    Fill        { stamp: Stamp, cloid: Option<Cloid>, oid: u64, coin: CoinId, side: Side, px: Px, sz: Sz, fee: Px, liquidation: bool },
    PostAck     { stamp: Stamp, req_id: u64, result: PostResult },    // per-order statuses or error
    Reconcile   { stamp: Stamp, snapshot: AccountSnapshot },
    Funding     { stamp: Stamp, coin: CoinId, usdc: Px },
    Control     (Control),                                            // KillSwitch, Resume, Pause{strategy}, ReloadLimits
}
```

- `CoinId`, `Cloid` (`[u8; 16]`), `Level { px, sz, n }`, `BookSnapshot { bids: [Level; 20], asks: [Level; 20], n_bids: u8, n_asks: u8, time_ms }`.
- `Px`/`Sz` are `rust_decimal::Decimal` in v1. Task **E-11** may switch them to fixed-point `i64` with a per-asset scale, **only if** E-10 shows decode or evaluation over budget. Money math stays exact either way (GOAL §4.5).
- Ingest decodes straight into these types. There are no intermediate `serde_json::Value`s on the market path (today's `ws::decode` goes via `Value`; E-2 replaces it with typed borrowed-string deserialization).

## 7. Engine state

```rust
pub struct EngineState {
    pub markets: Vec<MarketSlot>,          // index = CoinId
    pub account: AccountState,             // positions, spot balances, margin; from stream + reconcile
    pub orders: OrderManager,              // §10: every live/pending order by cloid
    pub risk: RiskState,                   // §11: limits, exposure incl. in-flight, breakers, kill flag
    pub timers: TimerHeap,                 // BinaryHeap<(deadline_mono_ns, TimerId)>
    pub strategies: Vec<Box<dyn Strategy>>,
    pub routes: Routes,                    // CoinId × stream → strategy indices (precomputed)
}

pub struct MarketSlot {
    pub meta: AssetMeta,                   // asset id, sz_decimals, tick rules: precomputed for the order builder
    pub bbo: Option<(Level, Level, Stamp)>,
    pub book: Option<(BookSnapshot, Stamp)>,
    pub ctx: Option<(AssetCtxLite, Stamp)>,
    pub stale: bool,                       // set by Gap events / staleness check
}
```

Best bid/ask prefers the fresher of `bbo` and book top (SPEC-0002 H-5). A coin whose inputs are older than its tolerance, or inside a gap, is `stale`. Strategies can see that, and **risk rejects new non-reduce-only orders on stale coins**.

## 8. Strategy API v2 (synchronous)

```rust
pub trait Strategy: Send {
    fn id(&self) -> StrategyId;
    /// Coins, streams, and timers this strategy reacts to. Called once at startup.
    fn interests(&self) -> Interests;
    /// React to a market change on a coin it's interested in. Push actions into `out`.
    fn on_market(&mut self, coin: CoinId, ctx: &Ctx<'_>, out: &mut Actions);
    /// React to its own order updates and fills.
    fn on_order(&mut self, update: &OrderEvent, ctx: &Ctx<'_>, out: &mut Actions);
    /// Timer fired.
    fn on_timer(&mut self, timer: TimerId, ctx: &Ctx<'_>, out: &mut Actions) {}
}

pub struct Ctx<'a> {
    pub now: Stamp,                 // event time (replay-safe)
    pub markets: &'a [MarketSlot],  // read-only
    pub account: &'a AccountState,  // read-only
    pub orders: &'a OrderManager,   // read-only: this strategy's working orders
}

pub enum Action {
    Place(OrderIntent),             // cloid assigned by the engine if absent
    Cancel { cloid: Cloid },
    Modify { cloid: Cloid, px: Px, sz: Sz },     // one venue action instead of cancel + place
    PlaceGroup(GroupIntent),        // multi-leg: SPEC-0011
}
```

Rules: strategies are **pure and synchronous**. No I/O, no clock reads, no randomness except a seeded `DeterministicRng` (already in `mev-strategy`). `Actions` is a reusable buffer (`SmallVec` inside), cleared by the engine. Dispatch uses the precomputed `routes`, so a strategy is only called for coins it cares about.

Migration: `FundingBasis` and `MarketMaker` are already pure. E-4 ports them to v2 and deletes the `async_trait` version. `MarketMaker` switches its cancel/replace to `Modify` where possible.

## 9. Per-iteration algorithm (the loop)

```
loop:
  n = 0
  while let Ok(ev) = acct_rx.try_recv():  apply_account(ev); n += 1      // lossless first
  while let Ok(ev) = market_rx.try_recv(): apply_market(ev); n += 1      // marks dirty coins
  fire_due_timers(now_mono)                                              // pushes timer dispatches
  for coin in dirty.drain():               dispatch_market(coin)         // strategies → actions
  for ev in pending_order_events.drain():  dispatch_order(ev)
  process_actions()                        // §10–§12: risk → order manager → batch → build → sign → exec_tx.try_send
  flush_records()                          // try_send typed records to DbWriter / metrics
  if n == 0:
      spin up to `spin_us` checking both channels (default 50 µs; 0 = never spin)
      else block: select(acct_rx, market_rx) with timeout = next timer deadline
```

Ordering is deterministic: account before market, coins in `CoinId` order, strategies in registration order, actions in emission order.

## 10. Order manager and state machine

Every order the engine sends is tracked by `Cloid`. **The engine always assigns a `cloid`** (per-process 8-byte random prefix + 8-byte counter) if the strategy didn't.

| State | Entered when | Next states |
|---|---|---|
| `PendingNew` | Action accepted by risk and handed to exec | `Resting`, `Filled`, `PartiallyFilled`, `Rejected(reason)`, `Unknown` |
| `Resting` / `PartiallyFilled` | Ack / order update says resting | `Filled`, `PendingCancel`, `PendingModify`, `Cancelled` |
| `PendingCancel` / `PendingModify` | Cancel/modify sent | `Cancelled`, `Resting` (modified), `Filled` (raced), `Unknown` |
| `Filled` / `Cancelled` / `Rejected` | Terminal | — |
| `Unknown` | Exec disconnect or timeout with no ack | Resolved by the exec task querying `orderStatus` by cloid (SPEC-0002 H-2), which comes back as `Reconcile`/`OrderUpdate` |

- `PostAck` statuses are parsed per order (`resting` / `filled` / `error:<reason>`); `Rejected` carries the typed `RejectReason` (SPEC-0002 §9). This fixes §2 item 7.
- **Exposure accounting:** for risk, every order in `PendingNew`, `Resting`, `PartiallyFilled`, `PendingModify`, or `Unknown` counts at its **worst case** (full remaining size fills). This fixes §2 item 10.
- While any order on a coin is `Unknown`, new non-reduce-only orders on that coin are rejected.

## 11. Risk on the hot path

SPEC-0004 owns the rules; this section fixes the interface and the performance contract.

- `fn check(&mut self, action: &Action, ctx: &RiskCtx) -> Decision`: synchronous, O(1) per action, no allocation. Incremental per-coin and account exposure is kept up to date by the order manager, not recomputed by scanning.
- Check order (first failure wins): **kill switch** (an in-state flag, set by `Control::KillSwitch`) → breaker state → stale coin → unknown orders on coin → rate budget (§12) → per-order notional → per-coin projected exposure (confirmed + in-flight) → account margin utilization → min notional / rounding validity.
- `live` refuses to start unless every limit is explicitly set to a finite value (fixes §2 item 9; config validation in E-0).
- Cancels are never blocked by risk (they reduce risk), except by the rate budget's hard floor.

## 12. Building, batching, signing, sending

| Step | Rule |
|---|---|
| Aggressive price | For `limit_px: None` (take liquidity): IOC with limit = best opposite price × (1 ± `max_slippage_bps`/1e4), rounded to tick in the safe direction. Never the mid (fixes §2 item 8). `max_slippage_bps` is per strategy, default 10. |
| Batch | All approved actions from one iteration are coalesced: all places ⇒ **one** `order` action (bulk); all cancels by cloid ⇒ one `cancelByCloid`; all modifies ⇒ one `batchModify`. Places, cancels, and modifies go as separate posts, sent in this order: **cancels first, then modifies, then places.** |
| Build | Precomputed `AssetMeta` per `CoinId`; rounding per SPEC-0002 §6; msgpack into a reused buffer. |
| Sign | On the engine thread (budget: p50 ≤ 150 µs incl. msgpack; SPEC-0002 H-7). The nonce comes from the engine-owned `NonceManager` (single writer by construction), and persistence is write-behind (H-6). |
| Send | `exec_tx.try_send(SignedPost { req_id, payload, cloids })`. On failure (channel full or exec down): mark those orders `Rejected(LocalBackpressure)` and set a breaker (§16). |
| Rate budget | Engine-side token buckets for IP weight (1200/min shared; the engine's share is configurable) and the address budget (from `userRateLimit`, polled by the reconciler). Below `min_budget`: reject places, allow cancels. Metric `hl_rate_budget_remaining{kind}`. |
| Socket | Exec connections set **`TCP_NODELAY`** (connect the `TcpStream` ourselves, `set_nodelay(true)`, then TLS + WS handshake) and disable permessage-deflate. Connections are opened and warmed at startup, never on demand. |

## 13. Clock

`trait EngineClock { fn now(&self) -> Stamp; fn mono_ns(&self) -> u64; }`

- `LiveClock`: `SystemTime` for wall time (nonces, logs) + the **process-global monotonic clock shared with the raw socket** (`mev_hl_client::raw_ws::mono_ns`), so engine latency spans and received-frame stamps live on one monotonic timeline.
- `ReplayClock`: the time of the event being processed (from recorded `t_ns`/`mono_ns`); timers fire when replayed time passes their deadline.
- Nonces use wall time in live, and the replay clock in simulate/replay (signatures in replay are never sent).

## 14. Backends: live, simulate, replay

The engine is generic over its I/O. Only the edges change between modes.

| Mode | Market input | Account input | Exec backend | Clock |
|---|---|---|---|---|
| `observe` | live WS | — | none: the engine doesn't run strategies (current behavior kept) | live |
| `simulate` | live WS | `PaperExec` fills as `AccountUpdate`s | `PaperExec`: fills against the live book after a configurable latency `sim_latency_ms` (default: measured p50 ack latency, else 20 ms), maker fills when the book trades through | live |
| `replay` | **SPEC-0008 recorder segments** via `hl-recorder`'s reader, decoded by the same ingest decoders | `PaperExec` | `PaperExec` with the same latency model | `ReplayClock` |
| `live` | live WS | H-3 stream + reconciler | `WsExec` (H-1), REST fallback | live |

`hl replay --from … --to … --strategies … --out actions.jsonl` runs the real engine over recorded data. Its action log (every approved action with its cloid, px, and sz, plus every fill) is the determinism artifact: the same input must give a byte-identical log. This also lets SPEC-0008 study winners be re-validated with production code before going live.

The existing `PaperExecutor` in `mev-strategy/src/paper.rs` becomes the core of `PaperExec`, moved behind the exec-backend interface.

## 15. Account state and reconciliation

- The source of truth for own orders and fills is the H-3 stream (`orderUpdates`, `userFills`, `userEvents`), applied in order.
- The REST reconciler runs every 30 s, after any reconnect, and on demand (after `Unknown`s). It produces `Reconcile` snapshots; the engine diffs them against local state. On drift: fix local state, count `hl_reconcile_drift_total{kind}`, and if drift repeats 3 times within 10 min, trip the breaker.
- The 5 s `account_poller` is deleted once E-8 lands.

## 16. Failure handling

| Failure | Engine response |
|---|---|
| Market feed gap on a conn | Mark affected coins `stale`; risk rejects new non-reduce-only orders there; strategies see `stale` |
| Account stream gap | Halt new places account-wide; request an immediate reconcile; resume after a clean reconcile |
| Exec disconnect / post timeout | Affected orders → `Unknown`; halt new places; exec reconnects and resolves via `orderStatus`; resume when no `Unknown` remains |
| Outbound channel full | Reject the batch locally, trip the breaker `exec_backpressure`, alert |
| Kill switch (`SIGUSR1`, flag file, `hl panic`) | Set the kill flag; emit cancel-all for every working order; strategies get no more dispatches; multi-leg residuals follow SPEC-0011 `on_kill` |
| Engine thread panic | `panic = abort` (release profile) ⇒ process exits ⇒ the dead-man switch (`scheduleCancel`) cancels resting orders; systemd restarts; startup reconciles before trading |
| Dead-man refresh | An **engine timer** emits the `scheduleCancel` action through the same exec path (ordered with orders; SPEC-0002 H-4 policy) |

## 17. Latency instrumentation and benchmarks

**Stamps carried per decision:** `t_recv` (socket read) → `t_decoded` → `t_dequeued` (engine) → `t_decided` (strategy returned) → `t_risked` → `t_signed` → `t_handoff` (exec_tx) → `t_written` (exec writer returned from the socket write) → `t_ack` (reply received).

| Histogram (Prometheus, exported every 1 s from in-thread `hdrhistogram`s, so there's no per-event metrics-facade cost) | Span |
|---|---|
| `hl_engine_decode_seconds` | `t_decoded − t_recv` |
| `hl_engine_queue_seconds` | `t_dequeued − t_decoded` |
| `hl_engine_decide_seconds` | `t_decided − t_dequeued` |
| `hl_engine_risk_seconds` | `t_risked − t_decided` |
| `hl_engine_sign_seconds` | `t_signed − t_risked` |
| `hl_engine_handoff_seconds` | `t_written − t_signed` |
| `hl_exec_queue_seconds` | exec writer: post received → frame enqueued on the socket (excludes the reply wait; task C) |
| **`hl_engine_tick_to_order_seconds`** | **`t_written − t_recv`** (the engine-side headline; the published `hl_tick_to_order_seconds` is the live transport span, SPEC-0002 H-7) |
| `hl_engine_submit_ack_seconds` | `t_ack − t_written` (network + venue, engine-side) |
| `hl_engine_iteration_seconds`, `hl_engine_events_per_iteration`, `hl_engine_market_drops_total`, `hl_engine_idle_ratio` | loop health |

**Benchmarks (criterion, `crates/mev-bot/benches/engine.rs`):**
1. `bbo_to_action`: one `Bbo` event through apply → dispatch (a trivial threshold strategy) → risk → build → sign, excluding the socket. Must meet G-2 on the reference machine.
2. `drain_1000`: 1000 queued market events → one decision per dirty coin.
3. `zero_alloc`: a counting global allocator in a test binary asserts **0 allocations** per `Bbo` event after warm-up (G-3).
4. `replay_throughput`: events/second in `replay` (target ≥ 1M events/s, so a day replays in minutes).

Reference machine = the chosen production host type (SPEC-0008 V-4). Results are recorded in §21.

## 18. Performance engineering checklist (E-12)

E-12 can decide the code/config/profile rows here; every row that needs real
hardware is marked **deferred — needs reference host (SPEC-0008 V-4)** with the
exact experiment to run. No host numbers are invented.

| Item | Default | Decide by | E-12 decision |
|---|---|---|---|
| Pin the engine thread to a dedicated core (`core_affinity`) | on if ≥ 4 vCPUs | bench p99 with and without; check VPS steal time | **deferred — needs reference host (SPEC-0008 V-4).** No affinity code or `core_affinity` dependency exists yet, and E-12 does not add one. Experiment: on the production host type, build two variants (pin on / off) and bench `bbo_to_action` p99 and 24 h `simulate` `hl_engine_tick_to_order_seconds` p99 under live load, plus `/proc/stat` steal time; keep pinning only if p99 improves. The `pin_core` config surface lands with the E-13 wiring of the v2 loop into `hl` (nothing reads it today). |
| Spin before blocking (`spin_us`) | 50 µs | p99 vs CPU cost | **default confirmed.** `LoopConfig::default` is `spin_us: 50` (`crates/mev-engine/src/run.rs:53`) and the §19 example matches. **deferred — needs reference host (SPEC-0008 V-4).** Experiment: sweep `spin_us ∈ {0, 20, 50, 100, 250}` and record `bbo_to_action` p99 vs engine-core CPU% (and idle ratio) to pick the knee. |
| Allocator (`mimalloc`) | off | bench | **confirmed off.** No `mimalloc` dependency is added; the zero-alloc test (`crates/mev-engine/tests/zero_alloc.rs`) passes on the system allocator. **deferred — needs reference host (SPEC-0008 V-4).** Experiment: add `mimalloc` as a temporary global allocator, bench `bbo_to_action`/`replay_throughput`/`drain_1000`; adopt only on a measured win. |
| `TCP_NODELAY` on every exec and market socket | **on** | always | **decided and verified (E-6).** `set_tcp_nodelay` (`crates/mev-hl-client/src/raw_ws.rs:375`) is applied to the market `RawWsConn` (`raw_ws.rs:387`) and the exec `WsExchange` (`crates/mev-hl-client/src/ws_exchange.rs:167`); a loopback test asserts `nodelay()` is set (`raw_ws.rs:547`). Always on; no host decision needed. |
| Warm standby exec connection (fail over without a handshake) | off | measured reconnect gap | **deferred — needs reference host (SPEC-0008 V-4).** Experiment: force-drop the exec socket and measure the reconnect gap (TCP + TLS + H-1 auth/handshake) over many samples; enable a warm standby only if that gap threatens the order path under the measured p99. |
| `lto = "fat"` for release | **fat** (was `thin`) | bench | **set to `fat` in the workspace `[profile.release]`** (`Cargo.toml`): small workspace, acceptable link time, `codegen-units = 1` and `panic = "abort"` retained. **Final choice deferred — needs reference host (SPEC-0008 V-4):** bench `bbo_to_action`/`replay_throughput` thin vs fat (and note binary size / link time); revert to `thin` if the latency gain does not justify the build cost. |
| Fixed-point `Px`/`Sz` (E-11) | off | E-10 results | **off — E-11 not triggered.** E-10 measured `bbo_to_action` p50 ~34.6 µs / p99 ~38.5 µs (quick, dev), far under the G-2 budget of 100 µs / 1 ms (§21), so the decode/eval cost does not justify fixed-point. Cross-reference §23 Q-Decode-Alloc; revisit only if a reference-host run breaches the budget. |

**CI quick mode (E-12 check).** The `MEV_BENCH_QUICK=1` path is documented in the
bench file header (`crates/mev-bot/benches/engine.rs:14-27`), so no follow-up is
needed.

**Config surface (E-12 note).** No `[engine]` section is added to
`config/default.toml`: `mev-core::Config` has no engine fields and nothing
consumes `spin_us`/`pin_core` yet, so a config default there would be inert. The
surface is exposed with the E-13 wiring of the v2 loop into `hl`.

## 19. Configuration

```toml
[engine]
spin_us               = 50
pin_core              = "auto"        # "auto" | "off" | <core index>
market_channel_cap    = 65_536
account_channel_cap   = 16_384
exec_channel_cap      = 1_024
reconcile_secs        = 30
sim_latency_ms        = 20
max_slippage_bps      = 10            # default; strategies may override
rate_budget_min       = { ip_weight = 100, address = 500 }
```

## 20. Work breakdown

All tasks are **T1**, except **E-0, which is T0 fix-first** ([`docs/GOAL.md`](../docs/GOAL.md) §2.2): do it before anything else. Status: ☐ / 🔄 / ✅. Size: S ≤ ½ day, M ≤ 2 days, L ≤ 5 days.

| ID | Title | Size | Depends on | Status |
|---|---|---|---|---|
| E-0 | **T0: Safety fixes on the current engine** (before any other work or testnet run) | S | — | ✅ |
| E-1 | Core types: `CoinId` interning, `Stamp`, `MarketUpdate`, `AccountUpdate`, `Level`, `BookSnapshot`, `Cloid` | S | — | ✅ |
| E-2 | Typed ingest decoders (no `serde_json::Value`) + market/account channels; handoff bench | M | E-1, SPEC-0008 R-3 | ✅ |
| E-3 | Engine thread + loop (§9), timers, routes, spin/park | M | E-1 | ✅ |
| E-4 | Strategy API v2 (sync) + port `FundingBasis` and `MarketMaker` | M | E-3 | ✅ |
| E-5 | Order manager + state machine (§10), cloid assignment, in-flight exposure | M | E-3 | ✅ |
| E-6 | Build/batch/sign on the engine thread + `WsExec` backend (§12) incl. `TCP_NODELAY`, aggressive-price rule, rate budgets | M | E-5, SPEC-0002 H-1, H-2 | 🔄 |
| E-7 | `PaperExec` backend + `hl replay` over recorder segments; determinism test | M | E-5, SPEC-0008 R-7 | ✅ |
| E-8 | Account stream + reconciler integration; delete `account_poller` | M | E-5, SPEC-0002 H-3 | 🔄 |
| E-9 | Hot-path risk integration (§11) with SPEC-0004 K-tasks | M | E-5, SPEC-0004 K-1, K-2, K-3 | ✅ |
| E-10 | Latency stamps, histograms, benches incl. zero-alloc (§17) | M | E-6 | ✅ |
| E-11 | Fixed-point `Px`/`Sz` (**only if** E-10 shows decode/eval over budget) | L | E-10 | ✅ |
| E-12 | Performance checklist (§18), results recorded | M | E-10 | ✅ |
| E-13 | Remove the tick engine; update SPEC-0003 status; update RUNBOOK | S | E-4, E-5 | ✅ |

### Task details

**E-0 — Safety fixes on the current engine.** In `crates/mev-bot/src/engine.rs`, `mev-risk`, and `mev-core/src/config.rs`: (1) assign a `cloid` to every live order when the intent has none; (2) parse the `place` response's per-order statuses and record `resting` / `filled` / `rejected:<reason>` instead of "submitted"; (3) for `limit_px: None`, use the §12 aggressive-price rule instead of the mid; (4) `Config::validate` rejects `live` unless all four risk limits are set; (5) `LimitRisk` counts the notional of intents already approved **in the same cycle** toward the position cap. *Done when:* a unit test covers each of the five, and existing tests pass.

**E-1 — Core types.** New module `crates/mev-bot/src/engine/types.rs` (or a new `mev-engine` crate, if the dependency graph needs it: decide in the task and note why). `CoinId` is built from `AssetMap` at startup with a bidirectional map. *Done when:* types compile with docs, and a unit test round-trips `CoinId` ↔ coin name for perps, spot, and HIP-3.

**E-1 implemented (2026-09-26).** First landed as `crates/mev-bot/src/engine/types.rs`, then **moved to `crates/mev-engine/src/types.rs` in E-2** once the typed decoders needed the types beside them (see E-2's crate-decision note). Ships `CoinId`/`CoinRegistry` (dense ids from the configured universe, `from_asset_map` helper), `ConnId`, `Cloid([u8;16])`, `Level`/`BookSnapshot` (fixed `[Level; 20]`, `BOOK_DEPTH`), `Stamp`, `Px`/`Sz` = `Decimal`, `AssetCtxLite`/`AssetMetaLite`, `Side`, `VenueOrderStatus`, `PostResult`, `Control`, `AccountSnapshot`, and the §6 `MarketUpdate`/`AccountUpdate` enums. `AccountSnapshot`/`PostResult`/`Control`/`VenueOrderStatus` are minimal payload stubs to be fleshed out by E-5/E-8/E-9. Adds `smallvec` (workspace) for `Trades`; `MarketUpdate` keeps its `Book` inline (`#[allow(clippy::large_enum_variant)]`) because boxing would allocate per event.

**E-2 — Typed ingest.** Decoders for `bbo`, `l2Book`, `trades`, `activeAssetCtx`, and the H-3 account channels deserialize straight into §6 types (serde with borrowed `&str` → parse). Ingest tasks stamp `t_recv` right after the read, then `try_send`. *Done when:* golden-fixture tests pass (reuse `benches/fixtures`), decode bench ≤ ADR-0001 numbers, and the handoff bench (async `try_send` → engine thread receive) is recorded, p99 target ≤ 10 µs.

**E-2 implemented (2026-09-26).** `mev-engine`'s `src/ingest.rs`: `decode_market`/`Ingest` parse `l2Book`, `bbo`, `trades`, and `activeAssetCtx` straight into `MarketUpdate` (borrowed `&str` → `CoinId`, fixed-size `BookSnapshot`, `SmallVec` trades); `decode_account` parses the H-3 channels into `AccountUpdate`s (`orderUpdates` → `OrderUpdate`, `userFills` → `Fill` incl. snapshot, `userEvents`/`"user"` → `Fill`s plus `Funding`). Unknown coins and ack channels are skipped. Bench (`benches/ingest.rs`): `decode/l2Book` ~113 µs, `trades` ~74 µs, `ctx` ~54 µs — all below the ADR-0001 `Value`-based numbers (259 / 186 / 131 µs); `handoff/try_send` ~9 ns and `handoff/thread_latency` p50 ~211 ns, well under the 10 µs p99 target.

**Crate decision (E-2):** the engine now lives in a dedicated `mev-engine` crate (types + ingest), above `mev-hl-client` and below `mev-bot`. This supersedes E-1's "module in `mev-bot`" note: the typed decoders must sit beside the types they produce, and `mev-hl-client` cannot depend on `mev-bot`, so a crate above `mev-hl-client` is the only acyclic placement (§23 Q1). E-1's `types.rs` was moved there unchanged.

**E-3 — Engine loop.** Implement §9 on a `std::thread` with `crossbeam_channel::select!`. `TimerHeap` with `BinaryHeap`. Precomputed `Routes` from `interests()`. *Done when:* unit tests cover drain order (account before market), conflation (5 updates for one coin ⇒ 1 dispatch), timer firing order, and spin → block with timeout.

**E-3 implemented (2026-09-26).** In `mev-engine`: `channels` (bounded `Inputs`/`InputHandles`, lossy `send_market` that drops when full, lossless `send_account`, fail-closed `Outbound::try_send`), `timers` (`TimerHeap` over `BinaryHeap`, deterministic deadline-then-id order), `routes` (`Interests`/`Stream`/`Routes`, per-coin lists in registration order), `state` (`MarketSlot`/`EngineState`, best bid/ask preferring the fresher of bbo/book, dirty-coin set drained in `CoinId` order), and `run` (`EngineLoop` implementing §9 on a plain struct; `iterate` drains account → market → timers → dirty coins, then `idle` spins `spin_us` and blocks with a 250 ms cap or the next timer deadline). A `Dispatcher` trait is the seam E-4 fills with the v2 strategy dispatch; E-3 tests use a recorder. Tests cover account-before-market, conflation, timer order, routes, spin-then-block, stop-on-request, and stop-on-closed-inputs.

**E-4 — Strategy API v2.** Replace the `async_trait` `Strategy` with §8's trait. Port `FundingBasis` and `MarketMaker`; `MarketMaker` uses `Modify` for re-quotes. Keep their existing tests, adapted. *Done when:* both strategies pass their tests under v2 and no `async_trait` remains in `mev-strategy`.

**E-4 implemented (2026-09-26).** The v2 trait lives in **`mev-engine`** (`src/strategy.rs`): `Strategy` (`id`/`cost`/`interests`/`on_market`/`on_order`/`on_timer`), `Ctx`, `Actions`, `Action` (`Place`/`Cancel`/`Modify`/`PlaceGroup`), `OrderEvent`/`OrderEventKind`, `Interests`/`Stream`/`TimerId`. Rationale below (§23 Q-Layering). `FundingBasis` and `MarketMaker` moved from `mev-strategy` to **`mev-engine/src/strategies/`** (`funding.rs`, `mm.rs`) and are driven by `on_market`/`on_order`; `MarketMaker` now re-quotes a stable ladder with `Action::Modify` (one venue action per level) and falls back to cancel + place only when the ladder shape changes (inventory cap removes a side). `mev-strategy` keeps the pure building blocks (`CostModel`, `FeeRates`, views, `OrderIntent`, sizing, paper, RNG) and no longer contains `async_trait`; it gained `BookView::from_levels` so a fixed `BookSnapshot` can feed the cost model. The legacy `mev-bot` engine was ported to drive the v2 trait synchronously (it builds `MarketSlot`s/`AccountState` from its views each cycle).

**E-4 layering decision (§23 Q-Layering).** §8's trait and `Ctx` name engine types (`CoinId`, `Cloid`, `MarketSlot`, `AccountState`). `mev-engine` depends on `mev-strategy`, so a trait in `mev-strategy` would close a cycle. The trait therefore lives in `mev-engine`, and `FundingBasis`/`MarketMaker` moved there with it; `mev-strategy` remains the venue-agnostic library. The engine's own loop still reaches strategies through the E-3 `run::Dispatcher` seam, which E-5 replaces with per-strategy dispatch built from `interests()`.

**E-4 legacy-engine note.** `OrderManager`/`AccountState`/`RiskState` do not exist until E-5/E-8/E-9, so `Ctx` currently exposes `markets`, `account`, and `registry` (no `orders`); `AccountState` is a minimal `positions`/`spot`/`account_value`/`margin_used` holder here and E-5/E-8 extend it. The ported strategies reach their `CoinId`s by resolving names once at build time. The legacy `mev-bot` engine tracks `cloid → (coin, asset_id, …)` locally to turn `Action::Modify` into cancel-then-place until the E-5 order manager lands (E-13 deletes this engine).

**E-5 — Order manager.** §10 in full, plus incremental per-coin exposure (confirmed + worst-case in-flight) consumed by risk. *Done when:* table-driven tests cover every §10 transition, including races (fill arrives before ack; cancel races fill), and the exposure is exact after each.

**E-5 implemented (2026-09-26).** `mev-engine/src/orders.rs`: `OrderState` (with `is_terminal`/`is_working`/`is_unknown`), `LiveOrder` (`remaining`, `remaining_notional`), `OrderManager`, and `CloidAssigner` (wraps `mev_hl_client::CloidFactory`). The manager tracks orders by `Cloid`, routes post acks by `req_id` (`assign_req`/`on_post_ack`), applies stream order updates and incremental fills, and maintains an exact incremental per-coin worst-case in-flight notional (`pending_notional`), adjusted without rescanning orders. Terminal states are sticky and a documented §10 transition guard rejects illegal transitions (`PartiallyFilled` may not fall back to `Resting`), so a fill-before-ack resolves the order and a stale ack/cancel cannot downgrade it. `AccountState::projected_notional` gives the confirmed side; risk (E-9) combines confirmed + in-flight. Tests cover every §10 transition, the two races, exactness after each transition, per-coin `unknown_on_coin`, `on_post_ack` routing, `resting_count`, and cloid round-tripping.

**E-5b implemented (2026-09-26) — per-strategy dispatch replaces the seam.** `mev-engine/src/dispatch.rs` adds `StrategyDispatcher`, the concrete consumer of the E-3 `Dispatcher` seam: it owns the `Vec<Box<dyn Strategy>>`, aggregates each strategy's `Interests()` into the routes, and dispatches a coin/timer/account event **only to the strategies that declared it** (per-coin routing). It owns the `OrderManager`, `AccountState`, `AccountStreamState`, `RiskGate`, `AssetTable`, `CoinRegistry`, `CloidAssigner`, `ReqIds`, an optional `ExecBackend`, and a `LatencyRecorder`. The action pipeline assigns cloids, gates each action through `RiskGate`, plans cancels-first posts via `builder::plan_iteration`, dispatches through the exec backend, and fails closed on backpressure (tripping the breaker). A `cloid → StrategyId` map routes order updates and fills back to their owning strategy; `PostAck` routes statuses via `req_id`. `EngineLoop<StrategyDispatcher>` (via `EngineLoop::with_dispatcher`) is the production loop; the generic `Dispatcher` trait is retained for the E-3 tests. This closes the E-4 note's "E-5 replaces the seam" item. Tests: per-coin routing, place→gate→cloid→one bulk order (cancels first), kill-switch cancel-all + place rejection, fill/order-update delivery by owner, `PostAck` routing and `Error`→breaker, backpressure fail-closed, `apply_reconcile`, timer routing, and an `EngineLoop<StrategyDispatcher>` + `MarketMaker` smoke.

**E-6 — Build, batch, sign, send.** §12 in full. Uses the H-1 concurrent `WsExec`. Precomputed `AssetMeta`, reused buffers. *Done when:* a mock-venue test shows one iteration with 2 places + 1 cancel sends exactly 2 posts (cancel first), statuses route back to the right cloids, `TCP_NODELAY` is set (asserted on the socket), and aggressive prices round in the safe direction.

**E-6 implemented, part 1 (2026-09-26) — build/batch + socket rules.**
- `mev-engine/src/builder.rs`: `AssetTable` (precomputed `Market` per `CoinId`), `AssetMeta`, the §12 aggressive-price rule (`aggressive_limit_px`/`aggressive_limit_px_market`, safe-direction rounding), and `plan_iteration`, which coalesces one iteration's approved actions into posts in §12 order — **all cancels as one `cancelByCloid`, then all places as one bulk `order`** — assigning cloids, rounding, and dropping sub-min-notional / no-touch / unknown-cloid items with a typed `DropReason`.
- `mev-engine/src/exec.rs`: the `ExecBackend` seam (`UnsignedPost { req_id, action, cloids }`), `ReqIds`, and `dispatch`/`dispatch_batch` (fail-closed: nothing is registered on backpressure) + `apply_post_ack` to route `PostAck` statuses to the right cloids.
- `mev-hl-client`: `TCP_NODELAY` is set on the underlying `TcpStream` of both the exec (`WsExchange`) and market (`RawWsConn`) sockets via `MaybeTlsStream::get_ref()` (the real rustls inner stream, not a fallback); `WsExchange::warm()`/`is_connected()` open the connection eagerly. Tests cover cancel-first ordering / one post per kind, status routing, aggressive direction, min-notional and unknown-cloid drops, cloid alignment, fail-closed backpressure, and loopback `nodelay` assertion.

**E-6 open items (recorded, not yet done).**
1. **Signing placement.** §12 says sign on the engine thread with an engine-owned `NonceManager`. Part 1 keeps the engine signing-free (no key in `mev-engine`) and hands unsigned posts to the exec layer, where `WriteCore::prepare` signs. Whether to move signing onto the engine thread (and thus hold the agent signer in `mev-engine`) is **Q-Sign-Placement** below.
2. **`batchModify`.** The venue `batchModify` wire fields are unverified, so `plan_iteration` drops `Action::Modify` (`DropReason::ModifyUnsupported`) and `Action::PlaceGroup` (`GroupUnsupported`). Consequence: the E-4 `MarketMaker` re-quote via `Modify` is **not executable on the new engine until this lands**; the legacy tick engine still routes MM as cancel + place. Verify the wire shape and implement, or standardize modify as cancel + place.
3. **Rate budgets.** §12's engine-side token buckets (`hl_rate_budget_remaining`) are not implemented; E-9 owns the risk-side budget together with SPEC-0004 K-tasks.

**E-7 — Paper + replay.** `PaperExec` (from `paper.rs`) behind the exec backend interface, with a latency model. `hl replay` reads SPEC-0008 segments. *Done when:* replaying a fixture segment twice gives byte-identical action logs, and a `simulate` run of `FundingBasis` produces the same decisions as replay over the same recorded window (within the latency model).

**E-7 implemented, part 1 (2026-09-26) — `PaperExec` + latency + determinism.** `mev-engine/src/paper_exec.rs`: `PaperOrder`, `PaperConfig` (`latency_ms` default 20, `maker_fills` default true), and `PaperExec`, which matches takers at their `now_ms + latency_ms` deadline and fills resting ALO orders only after a later book crosses their limit (also latency-delayed), emitting `AccountUpdate::OrderUpdate`/`Fill` exactly as the live account stream would. `paper_orders_from_post`/`paper_cancels_from_post` convert a built `UnsignedPost` into paper orders/cancels (it takes the `AssetTable` because `OrderWire` carries numeric asset ids, not coins). A determinism test drives `MarketMaker` + `PaperExec` twice over a fixed, clock-free, random-free sequence and asserts identical action logs and account updates (FNV-1a fingerprint); latency-model tests cover the taker deadline, the resting cross, post-only rejection, and cancel.

**E-7 implemented, part 2 (2026-09-27) — `hl replay` over recorder segments.**
- `mev-engine/src/clock.rs`: `EngineClock` with `LiveClock` and `ReplayClock`;
  injected into `StrategyDispatcher` and `EngineLoop` (`with_clock`). All engine
  time (paper latency, risk rate budget) now goes through it, and a dirty coin
  is dispatched with the stamp of the update that marked it dirty, so
  `ctx.now.t_recv_ns` is the real event time (it was always 0 before).
- `CloidAssigner::with_prefix` pins the cloid prefix for `simulate`/`replay`
  (G-6); live keeps the random default.
- Interest-driven repeating timers: `Interests::timers_ms` is now scheduled on
  the loop's heap at the first iteration, routed to the owning strategy via
  `Dispatcher::on_timer_registered`, and re-armed at the next period after now.
- `mev-engine/src/journal.rs`: an optional `ActionSink` records every
  risk-approved action and every fill; `SharedSink` is the inspectable sink.
  Live leaves it `None`.
- `mev-recorder::reader::segments_for` enumerates `.jsonl.zst` (and `.crashed`)
  files for an inclusive UTC date range.
- `mev-bot/src/replay.rs` + `hl replay --from/--to/--rec-dir/--out` drive
  `EngineLoop<StrategyDispatcher>` + `PaperExec` over the merged segments with a
  `ReplayClock`; `gap_start`/`gap_end` map to `MarketUpdate::Gap`. The SQLite
  session path stays when `--from` is absent.
- *Done when, verified:* `replay::tests::segment_replay_is_byte_identical_across_runs`
  replays a fixture `l2Book` segment twice and asserts the journal files are
  byte-identical; `dispatch::tests::replay_is_independent_of_wall_clock_delays`
  drives the same event timeline twice, once with a wall-clock stall between
  iterations, and asserts identical journals (the property that lets `simulate`
  over live data and `replay` over the same window agree). The strict live
  `FundingBasis`-vs-replay comparison still needs the recorder deployed (R-10)
  to have a window recorded by a live `simulate` run.

**E-8 — Account stream + reconciler.** Wire H-3 and the §15 reconciler. Delete `account_poller`. *Done when:* tests cover drift detection and correction, the account-gap halt/resume, and `Unknown` resolution through a mocked `orderStatus`.

**E-8 implemented, part 1 (2026-09-26) — reconciler library + poller removal.**
- `mev-engine/src/reconcile.rs`: `Reconciler` (`build_snapshot` maps `ClearinghouseState`/`OpenOrder` → `VenueSnapshot`; `diff` → `SmallVec<[Drift; 16]>` for order-missing-on-venue / missing-locally / size+state mismatch / position / account-value; `apply_drift` corrects local state; `record_drift`/`should_trip` = the §15 3-in-10-min per-`DriftKind` breaker), and `resolve_unknown` (applies a passed-in `OrderStatusResponse` through the `OrderManager` transition guard). Pure: no I/O, no clock reads.
- `mev-engine/src/state.rs`: `AccountStreamState` implements the §15/§16 gap semantics — `on_gap` halts places + requests reconcile; reconnect alone does not resume; `on_clean_reconcile(has_unknown, drift_remaining)` resumes only when both clear.
- `mev-bot`: the 5 s `account_poller` is **deleted** and replaced by `account_reconciler` on the §15 30 s cadence (REST backstop, never on the order path).
- Tests: drift detection/correction (both sides, size/status/position), correction idempotence and per-kind isolation, the 3-in-10-min breaker (and non-trip when spread out), the account-gap halt/resume matrix, and `Unknown` → filled/cancelled/resting/not-found/not-tracked via a mocked `orderStatus`.

**E-8 remaining (partly closed by E-5b).** With `StrategyDispatcher` (E-5b) the v2 loop now owns `AccountStreamState` and a `Reconciler`-fed path: `Control::KillSwitch`/`Resume`, `OrderUpdate`/`Fill` application and owner delivery, `PostAck` routing, `Reconcile` snapshots (`apply_reconcile`), and `on_gap` (halt places until a clean reconcile). **Still outstanding:** the engine does not itself run the 30 s REST `Reconciler` (a caller task fetches the snapshot and calls `apply_reconcile`), and `AccountUpdate::Reconcile` carries only account value/margin (positions and order-state drift are not applied). `AccountUpdate` has no `Gap` variant, so the gap entry point is the explicit `AccountStreamState::on_gap()` (the market side keeps `MarketUpdate::Gap`); the `hl` account-stream task therefore does not driver the account-gap halt yet.

**E-8 live stream wired (2026-09-27).** `hl run` now runs a dedicated `account_stream` task: `orderUpdates`/`userFills`/`userEvents` on their own connection, decoded by the `mev-engine` account decoder and delivered losslessly through the account channel, so live cancels resolve and terminal orders are pruned. The REST reconciler remains the 30 s backstop. The account-gap halt on reconnect and position/open-order drift application still wait on an `AccountUpdate::Gap` variant and a richer reconcile payload (above).

**E-8 fill correctness (2026-09-27, task A).** Fills are taken from `userFills` only (the `user`/`userEvents` channel keeps funding, liquidations, and `nonUserCancel`) and de-duplicated by venue `tid`: the first-connect snapshot is recorded but not applied (it is already in the starting position), a reconnect snapshot applies only unseen tids, and live redeliveries are skipped. A `userFills` snapshot is carried as one `AccountUpdate::Fills` so the first-snapshot skip is atomic. Fills are mapped to orders through an `oid → cloid` index kept by `OrderManager` (filled by post acks and `orderUpdates`), so a live fill updates `filled_sz` and reaches the owning strategy; a fill that arrives after the order's terminal update and after pruning is still delivered from a bounded recent-route cache. The remaining fill gap is unchanged: position/open-order drift is not yet applied from the REST reconcile payload, and the account-gap halt still needs an `AccountUpdate::Gap`.

**E-9 — Risk integration.** Implement §11 against SPEC-0004's K-tasks. *Done when:* property tests show that no sequence of approved actions can push projected exposure over a cap, and the kill switch stops all new places within one iteration.

**E-9 implemented (2026-09-26).** `mev-engine/src/risk.rs`: `RiskGate` (`check`/`evaluate` + `RiskReason`), `RiskLimits`, `RiskCtx` (borrows the order manager, account state, slot, meta, rate budget and `now_ms`), `Breakers`, and `RateBudget`/`RateKind`/`RateBudgetConfig`. The §11 check order is exact: kill → breaker → stale → unknown-on-coin → rate budget → per-order notional (resize) → per-coin projected exposure = confirmed (`AccountState::projected_notional`) + worst-case in-flight (`OrderManager::pending_notional`), resize → margin utilization → min-notional/tick validity. Reduce-only bypasses stale/unknown/exposure; cancels bypass everything except the rate hard floor. **SPEC-0004 K-2** (in-flight/gross exposure toward the cap) landed in `LimitRisk` and the engine gate; the SPEC-0011 group-worst-single-leg rule is left a documented `GroupUnsupported` stub (group types do not exist yet — see §23 Q8). **SPEC-0004 K-3** landed as `mev-risk/src/kill.rs` (`KillSwitch` sticky flag, `check_flag_file` helper, `cancel_all_cloids`); the engine-gate kill check uses it. `RiskSettings.rate_budget` (`RateBudgetSettings`) added in `mev-core`. Tests: the rejection-order table, reduce-only, resize at both caps, kill-stops-all/sticky/cancel-all, rate-budget consume/guard/refill, config mapping, and a fixed-seed 5,000-iteration randomized property test asserting exposure never exceeds the cap. Approvals allocate nothing; only a rejection builds its reason string.

**E-10 — Latency instrumentation.** §17 stamps, histograms, and benches. *Done when:* the benches run in CI quick mode (warn-only thresholds), the zero-alloc test passes, and §21 has the first numbers.

**E-10 implemented (2026-09-26).** `mev-engine/src/instrument.rs`: `Stamps` (the §17 t_recv…t_ack chain with derived decode/queue/decide/risk/sign/handoff/`tick_to_order`/`submit_ack` spans), a fixed-bucket integer histogram + `LatencyRecorder` (`record`/`flush`, per-thread `&mut`, no lock on the hot path), and loop-health counters (iteration, events/iteration, market drops, idle ratio). `hdrhistogram` is **not** a workspace dependency, so a local ~4.4%-resolution histogram is used (switching to `hdrhistogram` recorded as a follow-up). The 12 §17 names were added to `mev-metrics` `names`. Benches in `crates/mev-bot/benches/engine.rs` (`bbo_to_action`, `drain_1000`, `replay_throughput`) with a `MEV_BENCH_QUICK=1` CI quick mode. `mev-engine/tests/zero_alloc.rs` is a counting-allocator test binary. First numbers recorded in §21. **Findings (honest, not hidden):** engine-side `Bbo` apply is 0 alloc/event, but the full `Ingest::decode` path allocates **1×/event** (a borrowed tag pre-parse in `decode_market`) — recorded as §23 Q9; `replay_throughput` measured ~649 k events/s vs the ≥1 M target (large `l2Book` frames dominate) — deferred to E-12. `run.rs` is not yet wired to the recorder (hooks are exposed; wiring is E-12/E-13).

**E-11 — Fixed-point (conditional).** `Px(i64)`/`Sz(i64)` with per-asset scale; parse from strings directly; convert to `Decimal` at the persistence/display edges. *Done when:* property tests show round-trip exactness vs `Decimal` for all assets in `AssetMap`, and benches show the gain (else revert and record why).

**E-11 not triggered (2026-09-26).** The task is conditional ("**only if** E-10 shows decode/eval over budget"). E-10 measured `bbo_to_action` p50 ~34.6 µs / p99 ~38.5 µs (quick, dev) — comfortably inside the G-2 budget (≤100 µs / ≤1 ms) — so the decode/eval cost does not justify fixed-point. Decision recorded in §18 and cross-referenced by §23 Q-Decode-Alloc; revisit only if a reference-host (§21) run breaches the budget. No code changed.

**E-12 — Performance checklist.** Measure each §18 item on the reference host. Keep only what helps. *Done when:* §21 has a before/after table, and the chosen defaults are in `config/default.toml`.

**E-13 — Cleanup.** Remove the tick engine and its `RwLock` plumbing; update SPEC-0003 §16, `RUNBOOK.md`, and `AGENTS.md`'s repo map. *Done when:* `grep -r "interval(Duration::from_secs(1))" crates/mev-bot` finds no decision loop, and all tests pass.

**E-13 implemented (2026-09-26)** — the legacy 1 s tick engine is gone and `mev-bot` runs `EngineLoop<StrategyDispatcher>`.
- `mev-bot/src/engine.rs` was split: the tick `Engine` (its 1 s decision loop, the async executor, `RwLock<MarketState>` decision plumbing, `TrackedOrder`, `step`/`run`/`apply_actions`) is **deleted**. Kept and reused: `EngineBuild`/`build`/`interests_to_subscriptions` (v2 strategies + subscriptions from config), `Recorder`, the replay helpers, and the view converters those need. `account_reconciler` now pushes `AccountUpdate::Reconcile` into the account channel.
- `mev-bot/src/main.rs`: `run()` builds the `CoinRegistry`/`AssetTable`, the strategies, a `RiskGate`, and a `StrategyDispatcher` (paper backend for `simulate`, an `Outbound<UnsignedPost>` exec writer for `live`, none for `observe`), then runs `EngineLoop::with_dispatcher` on a dedicated std thread. Ingest decodes WS frames with the `mev-engine` typed decoders into `handles.send_market`; a REST reconciler and the live exec writer feed `handles.send_account`. Shutdown stops the loop and joins. `observe` stays read-only.
- **Gap handling wired:** a feed gap/stream end sends `MarketUpdate::Gap{open:true}` and a (re)connect sends `open:false`; the loop marks all coins stale on open and clears a coin on its next fresh update, so the §11 stale-coin rejection is no longer inert (`run.rs` test `gap_marks_coins_stale_until_fresh_data_arrives`).
- Done-when verified: `grep -rn "interval(Duration::from_secs(1))" crates/mev-bot` finds only the 1 s dead-man tick and the 1 s staleness monitor — **no decision loop**; all tests pass.

**E-13 remaining (post-E-13 review, 2026-09-27; items 1–2 are now T0).**
1. **H-2 regression — T0 ([`docs/GOAL.md`](../docs/GOAL.md) §2.2 row 8), *fixed (2026-09-27, task B)*.** `submit_post` classifies a definitive failure (`Error::Exchange`/`NotSent`/local config) as `PostResult::Rejected` (terminal) and only `UnknownOutcome` as `Unknown`. A lost reply is reconciled by `cloid` with capped `orderStatus` retries in a spawned task; a found answer is applied only while the order is `Unknown` (via `AccountUpdate::ResolveUnknown` → `reconcile::resolve_unknown`), and a never-seen order is resolved `Rejected` after the bound (`AccountUpdate::UnknownExpired`). The `exec_error` breaker clears when no `Unknown` remains. No path resends an order.
2. **H-3 account stream not wired in `hl` — T0 ([`docs/GOAL.md`](../docs/GOAL.md) §2.2 row 11).** The REST reconciler is the wired backstop and `AccountUpdate::Reconcile` carries only `account_value`/`margin_used`; positions and order-state drift are not applied. Live cancels without per-order statuses leave orders tracked as working until H-3 lands (SPEC-0002 H-3 / SPEC-0010 E-8).
3. **Replay of new sessions** records market/reconcile events but no `Timer` events, so `hl replay` over a fresh session yielded no intents. *Fixed (2026-09-27, E-7 part 2):* the v2 driver replays recorder segments and the loop schedules `Interests::timers_ms` as repeating timers, so strategy timers fire on event time too. The SQLite `Event`-log replay path still carries only a `timer` row per decision cycle and is legacy.
4. **Exec writer stalls — T0 ([`docs/GOAL.md`](../docs/GOAL.md) §2.2 row 9), *fixed (2026-09-27, task C)*.** `WsExchange` exposes a split `enqueue` (sign + write the frame in call order, return a `ReplyHandle`) and the `hl` exec writer polls a `FuturesUnordered` in `select!` alongside `rx.recv()`. Posts are signed and enqueued in order as they arrive; only the reply wait is concurrent, and a lost reply's recovery is a spawned task. A post arriving during an in-flight reply is enqueued immediately (measured ~2 ms by the mock-writer test, sub-ms in practice; `hl_exec_queue_seconds`) instead of waiting a round trip, and a lost reply no longer stalls later posts or the kill-switch cancel-all.
5. **No kill-switch trigger — T0 ([`docs/GOAL.md`](../docs/GOAL.md) §2.2 row 10).** *Fixed (2026-09-27):* `hl run` has a 250 ms control task (SIGUSR1/SIGUSR2 + flag-file poll) that sends `Control::KillSwitch`/`Resume`; `hl panic`/`hl resume` maintain the flag file (`data/KILL`, `HL_KILL_FILE`). See SPEC-0004 K-3 part 2.
6. **H-2 re-wire and H-3 stream (2026-09-27).** *Fixed in tasks A and B:* fills come from `userFills` only, are de-duplicated by `tid` (first-connect snapshot recorded but not applied, reconnect snapshot applies unseen tids), and map to their order by `oid` so they reach the owner (row 12); definitive post errors are terminal and a never-seen order resolves `Rejected` after bounded retries (row 13, item 1 above). **Still open:** the account-gap halt on the reconnect path (`AccountStreamState::on_gap` is not yet driven from `hl`) and applying position/open-order drift from the REST reconcile snapshot (E-8 remaining below).
7. **Hot-path decode order — fixed (2026-09-27).** `hl` ingest does the typed decode and engine hand-off **first**, then offers the raw frame to a bounded sidecar. Correction to an earlier note: the `ingest_to_engine` bench measures ~615 µs for the legacy `ws::decode` over a 203-frame batch, i.e. **~3 µs per frame**, not the 130–260 µs ADR-0001 figure (that was a different/larger corpus). The bench compares the two decodes in isolation; it does **not** exercise `main.rs::ingest` or the real sidecar hand-off, so it is a relative guard, not an end-to-end measurement.
8. **Bbo subscription + `[engine]` config — fixed (2026-09-27).** `interests_to_subscriptions` maps `Stream::Bbo` to `Subscription::Bbo` (was `L2Book`); `Config::engine.spin_us` (default 50, `[engine]` in `config/default.toml`) replaces the hard-coded loop spin.

*Historical note.* An earlier revision of this spec recorded E-13 as blocked (2026-09-27). E-5b (`StrategyDispatcher`) and the `hl` port then landed, so that block is superseded by the *E-13 implemented* note above; its unblock checklist and doc edits are done, and no code was reverted. Its dependency cell was corrected to the real prerequisites (E-4, E-5b) at the same time; the E-6/E-7/E-8 remainders are tracked as *E-13 remaining* above and as [`docs/GOAL.md`](../docs/GOAL.md) §2.2 rows 8–11.

## 21. Measured results (filled in by E-10 / E-12)

| Metric | Target | Measured | Host | Date |
|---|---|---|---|---|
| `bbo_to_action` p50 / p99 | ≤ 100 µs / ≤ 1 ms | ~34.6 µs / ~38.5 µs (quick) | dev (non-reference) | 2026-09-26 |
| Handoff ingest → engine p99 | ≤ 10 µs | ~211 ns p50 (E-2) | dev | 2026-09-26 |
| Sign (single order) p50 / p99 | ≤ 150 / 500 µs | included in `bbo_to_action` (not isolated) | dev | 2026-09-26 |
| Allocations per `Bbo` event | 0 | apply 0; **full decode 1** (§23 Q9) | dev | 2026-09-26 |
| Replay throughput | ≥ 1M events/s | ~649 k events/s (below target) | dev | 2026-09-26 |
| `hl_submit_ack_seconds` p50 (live, testnet) | minimize | — (needs testnet) | | |
| `drain_1000` (1000 events → 1 decision/dirty coin) | — | ~45.8 µs/drain | dev | 2026-09-26 |

These are dev-host quick-mode numbers, **not** the reference production host
(SPEC-0008 V-4); E-12 re-measures on the reference host and records the
before/after. They were measured under the pre-E-12 `[profile.release]`
(`lto = "thin"`); E-12 changed the workspace profile to `lto = "fat"`, so the
`bbo_to_action` / `replay_throughput` / `drain_1000` rows above are **not**
directly comparable to a `fat` build and must be re-benched on the reference
host for both profiles (§18, §23 Q-E12-Reference-Host). The `hl_submit_ack_seconds`
row additionally needs a testnet round-trip; no host row is fabricated here.

## 22. Acceptance criteria

- [ ] E-0 merged before any testnet or live run.
- [ ] G-2 met on the reference host (bench + a 24 h `simulate` run's `hl_engine_tick_to_order_seconds`).
- [ ] G-3: the zero-alloc test passes.
- [ ] G-6: replay determinism test passes in CI.
- [ ] `FundingBasis` and `MarketMaker` run on the new engine in `simulate` for 24 h with no panics, no drops on the account channel, and reconciliation drift = 0.
- [ ] Every §16 failure row has a test.
- [ ] The tick engine is removed (E-13).

## 23. Open questions

1. `mev-engine` as its own crate vs a module inside `mev-bot` (E-1 decides; prefer a crate if replay tooling or benches need it without the binary).
2. Busy-spin budget on a VPS with shared cores: does pinning help, or does steal time dominate? (E-12)
3. Should signing move to a second pinned thread when one iteration emits many independent batches? Only if E-10 shows sign time dominating p99.
4. **Q-Layering (answered in E-4).** §8 puts the `Strategy` trait in `mev-strategy`, but its `Ctx`/`Action` name engine types (`CoinId`, `Cloid`, `MarketSlot`, `AccountState`) and `mev-engine` depends on `mev-strategy`. Resolved by putting the trait and the ported strategies in `mev-engine`; `mev-strategy` stays the venue-agnostic library. Revisit only if a non-engine consumer needs the trait.
5. **Q-Sign-Placement (E-6).** Does signing happen on the engine thread (§12, engine-owned `NonceManager`, agent signer inside `mev-engine`) or in the exec layer (part 1, keeps the key out of the engine crate)? Decision inputs: the sign budget (p50 ≤ 150 µs, §17), the AGENTS rule that only SPEC-0002 code holds the signer, and §23 Q3 (a second pinned signing thread). Decide in E-10 once sign latency is measured.
6. **Q-BatchModify (E-6).** Exact `batchModify` wire fields need a source; until then `Action::Modify` is dropped by the builder (E-6 open item 2) and MM cannot re-quote on the new engine.
7. **Q-Replay-Gap (E-7) — resolved 2026-09-27.** R-1/R-2/R-7 landed, so E-7 part 2 built the recorder-segment driver and `hl replay --from … --to … --out actions.jsonl` (see the E-7 note above). The SQLite-log replay remains for recorded `simulate`/`live` sessions.
8. **Q-Group-Risk (E-9).** SPEC-0004 K-2's group-worst-single-leg exposure rule and the SPEC-0011 `on_kill` residual path need the multi-leg group types, which do not exist yet; `RiskGate` returns `GroupUnsupported` for `Action::PlaceGroup`. Implement with SPEC-0011 L-tasks.
9. **Q-Decode-Alloc (E-10).** The engine-side `Bbo` apply is 0 alloc/event (G-3 satisfied for the apply path), but `Ingest::decode` allocates 1×/event: `decode_market` pre-parses a borrowed channel tag, and `serde_json`'s ignored-value handling allocates once for the nested `data` shape. Fix by dispatching from one typed borrowed envelope or scanning the tag without `serde_json`. Also `replay_throughput` is ~649 k events/s (vs ≥1 M); both are E-12/E-11 inputs.
10. **Q-Loop-Instrumentation (E-10/E-12).** `run.rs` now owns a `LatencyRecorder` and `StrategyDispatcher` fills the engine-side `Stamps` (decode/queue/decide/risk/sign spans) and records each iteration. `t_written`/`t_ack` (handoff/`tick_to_order`/`submit_ack`) stay unset until the exec layer reports them back (signing is off-thread, E-6), so those histograms are not yet emitted; the exec ack path fills them when the live exec writer lands (E-13).
11. **Q-E12-Reference-Host (E-12).** No production/reference host (SPEC-0008 V-4) exists yet, so the following are decided but unmeasured here and wait on it. **§18 rows:** (a) `core_affinity` pinning — bench p99 with/without + VPS steal time; (b) `spin_us` sweep 0/20/50/100/250 µs, p99 vs CPU%; (c) `mimalloc` — bench hot-path vs system allocator; (d) warm standby exec — measure forced-drop reconnect gap; (e) thin-vs-fat LTO — bench `bbo_to_action`/`replay_throughput`. **§21 rows:** all dev-host rows (`bbo_to_action`, handoff, sign, allocations/event, replay throughput, `drain_1000`) must be re-measured on the reference host under `lto = "fat"`; `hl_submit_ack_seconds` needs a testnet round-trip. Run the same criterion benches from `crates/mev-bot/benches/engine.rs` (and `mev-engine/tests/zero_alloc.rs`) on that host and fill the before/after table.
12. **Q-E13-Unblock (E-13, resolved).** E-5b's `StrategyDispatcher` and the `mev-bot` port landed, so the legacy 1 s tick engine is removed and `hl` runs `EngineLoop<StrategyDispatcher>` (E-13 note above). Follow-ups from the port are tracked as E-13 "remaining": the H-2 `reconcile_unknown` re-wire, the H-3 account stream, and the v2 replay driver (E-7 part 2).
