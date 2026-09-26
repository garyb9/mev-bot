# SPEC-0003 — Strategy Engine

**Status:** Partially implemented. Strategies A (funding/basis) and B (market-making) now run on [SPEC-0010](SPEC-0010-event-driven-engine.md)'s v2 `EngineLoop<StrategyDispatcher>`; the legacy 1 s tick engine was removed (E-13, see §16).
**Depends on:** SPEC-0000, SPEC-0001, SPEC-0002
**Blocks:** SPEC-0004 (risk consumes intents)

## 1. Purpose

Define the pluggable strategy engine and the first two strategies: **delta-neutral funding/basis** (first) and **market-making** (second). The engine turns market data and account state into typed `OrderIntent`s, with a shared cost/edge model so every trade is justified net of fees, funding, and slippage.

## 2. Goals

- One `Strategy` trait; new strategies are plug-ins, not forks.
- Shared, auditable cost model: fees, funding carry, slippage — net edge or no trade.
- Deterministic: identical event stream ⇒ identical intents (enables replay/backtest).
- Bluechip watchlist (`BTC`, `ETH`, `SOL`), modular to extend.
- Capital allocation across concurrently running strategies.

## 3. Non-goals

- Signing/submission (SPEC-0002).
- Portfolio risk limits and hard stops (SPEC-0004) — strategies *propose*; risk *disposes*.
- HyperEVM / cross-venue arb (SPEC-0005).

## 4. Architecture

Event-driven loop: market events (book/ctx/bbo) + account events (fills/order updates) + timers feed `on_event`; the strategy returns intents that flow through risk (SPEC-0004) to execution (SPEC-0002).

```rust
#[async_trait]
pub trait Strategy {
    fn id(&self) -> StrategyId;
    fn subscriptions(&self) -> Vec<Subscription>;
    fn timers(&self) -> Vec<Duration>;
    async fn on_event(&mut self, ctx: &StrategyContext) -> Result<Vec<OrderIntent>>;
    async fn on_fill(&mut self, fill: &Fill) -> Result<()>;
}

pub struct OrderIntent {
    pub coin: Coin,
    pub side: Side,
    pub limit_px: Option<Decimal>, // None = market/aggressive
    pub size: Decimal,
    pub tif: TimeInForce,          // Alo | Gtc | Ioc
    pub reduce_only: bool,
    pub rationale: String,         // logged for audit
}
```

`OrderIntent` is exchange-agnostic and fully described (no raw action bytes). Strategies never sign or submit.

## 5. Shared building blocks

- **`MarketView`** — books/mids/bbo, per-coin `AssetCtx` (mark, mid, funding, OI), recent trades; built from SPEC-0001 state.
- **`AccountView`** — perp positions/margin, spot balances, open orders, effective fee rates from `userFees`.
- **Cost & edge model** (`net_edge = gross_edge − fees − funding_carry − expected_slippage − buffer`):
  - **Fees:** effective maker/taker rates from `userFees` (base: perp 0.045% taker / 0.015% maker; spot 0.07% / 0.04%).
  - **Slippage:** estimated by walking the local book for the intended size (not a flat assumption).
  - **Funding carry:** projected from `AssetCtx.funding`, integrated over the expected holding horizon.
  - **Buffer:** configurable safety margin (`HL_MIN_EDGE_BPS`).
- **Sizing:** per-strategy sizer honoring min notional, tick/lot, and SPEC-0004 caps; refuses sub-edge trades.
- Every intent records signal time, decision time, and expected edge for later attribution.

## 6. Strategy A — Delta-neutral funding/basis (first)

**Thesis:** Hyperliquid pays funding **hourly**. When funding is positive, longs pay shorts; holding spot long + perp short collects funding while price risk nets out.

**Entry rule:** expected net funding over the planned horizon exceeds round-trip cost + buffer.

- Two legs, both on Hyperliquid initially: buy spot, short equal-delta perp (reverse when funding is negative).
- Prefer maker orders: break-even round-trip ≈ **0.11%** (spot buy 0.04% + perp short 0.015% + spot sell 0.04% + perp close 0.015%). Enter only when projected funding clears this plus buffer.
- Spot leg uses the correct L1 name (e.g. `UBTC/USDC`), resolved at runtime from `spotMeta`.
- **Delta neutrality:** size legs to equal delta; rebalance when drift exceeds a threshold (e.g. 1% of notional) or margin health degrades.
- **Exit:** funding falls below threshold for N consecutive settlements, basis converges, or a risk trigger (SPEC-0004) fires.
- Accrue realized funding from `userFundings`; track against projected.

**Why it fits:** low directional risk, clear EV math, hourly settlement allows early exit; it's a yield strategy, so capacity and fee control matter.

## 7. Strategy B — Market-making (second)

**Thesis:** earn the spread plus maker rebates in range-bound conditions with zero HyperCore gas.

- Quote a ladder around mid on selected bluechips; capture spread and maker rebate (base perp maker 0.015%; rebates key by maker-volume share).
- **Inventory management:** skew quotes against accumulated inventory; hard inventory caps; widen/withdraw quotes with volatility.
- **Refresh:** cancel/replace on book changes within rate limits; batch cancels; use WS post transport.
- **Adverse selection & liquidation guard:** pull quotes on fast moves; never let inventory approach margin limits.
- Maker rebate and fill quality tracked per coin.

## 8. Decision loop & scheduling

- Event-driven (book/ctx/fill), plus strategy timers (e.g. funding countdown, quote refresh).
- Deterministic ordering of events; no wall-clock reads inside strategy math beyond the injected `Clock`.
- Conflicting intents across strategies are resolved by the allocator (§9) before risk.

## 9. Capital allocation

- Per-strategy capital/margin budgets; prioritize by net edge; prevent overlapping/opposing orders on the same coin.
- Global caps are enforced by SPEC-0004; the allocator only distributes within them.

## 10. Backtesting & replay

- Record market/account events to the SQLite store (SPEC-0004) and export fixtures; replay drives strategies deterministically. **Implemented:** `Recorder` writes each `Event::{Market,Account,Timer}`; `hl replay [--session <id>]` re-drives the configured strategies offline and prints an FNV-1a-64 intent fingerprint (covered by `replay_is_deterministic`).
- `simulate` mode fills intents against the live book (or recorded book) and reports expected-vs-realized edge.
- Metrics from replay feed strategy tuning without live capital.

## 11. Observability

Per-strategy metrics: signals emitted, intents accepted/rejected (by reason), fills, expected vs realized edge (bps), funding captured, inventory/delta, quote uptime (MM). All intents carry a `rationale` for audit.

## 12. Testing

- Unit: net-edge math, fee/funding/slippage, sizer, break-even.
- Determinism: same fixture ⇒ byte-identical intent list.
- Funding strategy: correct entry/exit on synthetic funding paths; refuses trades below break-even.
- MM: quotes skew with inventory; respects caps; pulls on volatility.
- Replay integration against recorded sessions.

## 13. Acceptance criteria

- Funding strategy enters only when projected funding exceeds round-trip cost + buffer, on fixtures.
- Replay is deterministic across runs and machines.
- MM inventory stays within configured caps in replay, with quote-pull on volatility spikes.
- Every intent is net-positive at decision time under the shared cost model, or carries a documented exception.

## 14. Resolved decisions

1. **Order of delivery** — funding/basis first, market-making second.
2. **Venue scope** — both legs on Hyperliquid initially; cross-venue is a later, separate spec.
3. **Watchlist** — bluechips `BTC`, `ETH`, `SOL`; persisted and CLI-extensible (SPEC-0001).
4. **Cost discipline** — no trade without positive net edge (configurable buffer); fees come from live `userFees`.

## 15. Open questions

1. Funding horizon assumption (how many hourly settlements to project) and exit hysteresis.
2. MM quote cadence and ladder depth per coin (tune via replay).
3. Whether funding/basis should also use Hyperliquidity Provider (HLP) or other yield legs — out of scope for v1.

## 16. Implementation status & deltas (2026-09-26)

What exists in code, and where it departs from this spec. SPEC-0010 is the source of truth for the engine loop and the strategy API from here on. E-13 (2026-09-26) removed the legacy 1 s tick `Engine`; `hl` now runs `EngineLoop<StrategyDispatcher>` and `mev-bot/src/engine.rs` keeps only config/strategy building, the `Recorder`, the replay helpers, and the REST `account_reconciler`.

| Area | Code | Delta vs this spec | Resolution |
|---|---|---|---|
| Strategy trait | `mev-engine/src/strategy.rs`: sync `Strategy` returning `Vec<Action>` | §8's event-driven loop now runs in SPEC-0010's `EngineLoop` | The v1 async trait in `mev-strategy` is retired (SPEC-0010 E-4) |
| Actions | `mev-engine/src/strategy.rs`: `Action::{Place, Cancel}` | No `Modify`, no multi-leg group | SPEC-0010 §8 adds `Modify`; SPEC-0011 adds `PlaceGroup` |
| `OrderIntent` | `intent.rs`: adds `strategy`, `cloid`, `signal_ms`, `decision_ms` | `cloid` optional | The engine always assigns a cloid (SPEC-0010 §10, E-0) |
| Decision loop | `mev-bot/src/main.rs` runs `EngineLoop<StrategyDispatcher>` on a std thread; `engine.rs` no longer holds a loop | Event-driven; §8 satisfied | E-13 removed the 1 s tick `Engine` (2026-09-26) |
| Cost / edge model | `cost.rs`, `size.rs` | Matches §5 | Keep; research uses the same fee table (SPEC-0008 §13.2) |
| Strategy A: funding/basis | `mev-engine/src/strategies/funding.rs` | Legs sent as independent orders | Port to `PlaceGroup` (SPEC-0011 L-10) |
| Strategy B: market-making | `mev-engine/src/strategies/mm.rs` (inventory skew, cancel/replace) | Built ahead of research (allowed: GOAL §2.1); cancel+place instead of modify | Port to v2 with `Modify` (SPEC-0010 E-4). **Doesn't trade live** without G1/G1.5. |
| Paper execution | `mev-engine/src/paper_exec.rs` (`PaperExec`, wired in `simulate`) | Fills against book updates after a configured latency, not on a 1 s tick | SPEC-0010 §14 |
| Capital allocation (§9) | — | Not implemented | After SPEC-0010; needed only once ≥ 2 strategies run live |
| Recording (§10) | `mev-bot/src/engine.rs` (`Recorder`), `mev-core/src/db.rs` | `hl run` records only `Event::Market` — no `Timer`/`Account`/`Fill` — so a fresh session has no decision cycles | Feed the v2 event bus; record `Timer`/`Fill` with the v2 replay driver (E-7 part 2) |
| Replay (§10) | `mev-bot/src/engine.rs` (`replay_events`, `replay`), `hl replay` | Deterministic (same log ⇒ same FNV-1a-64 intent fingerprint), but it is a standalone driver (not `EngineLoop`) that runs decisions only on recorded `Timer` rows, which `hl run` no longer writes | Keep as the backtest spine; v2 replay driver is E-7 part 2 (blocked on SPEC-0008 R-7) |

**Follow-ups recorded by E-13 (SPEC-0010 E-13 "remaining"):** the H-2 `reconcile_unknown` path (`orderStatus`-by-cloid) is not re-wired — exec errors now fail closed as `PostResult::Error`; and the H-3 account stream is not wired in `hl`, so the REST reconciler's `AccountUpdate::Reconcile` carries only `account_value`/`margin_used` (positions and order-state drift are not applied).
