# SPEC-0004 — Risk, Portfolio & Accounting

**Status:** Partially implemented (per-order limit gate in `mev-risk/src/limits.rs`). Remaining work in §16.
**Depends on:** SPEC-0000, SPEC-0001, SPEC-0002
**Blocks:** SPEC-0003 intents reaching live execution

## 1. Purpose

Define the risk engine that gates every `OrderIntent`, the portfolio/margin model, PnL accounting and attribution, reconciliation with the exchange, and the kill switch / circuit breakers. Strategies propose; risk disposes.

## 2. Goals

- Hard, fail-closed limits enforced **before** any submit.
- Liquidation-distance guard and leverage/exposure caps that cannot be bypassed.
- One source of truth for positions, balances, and open orders, kept reconciled.
- Per-strategy and per-coin PnL attribution, realized vs unrealized.
- Fast, observable kill switch and automatic circuit breakers.

## 3. Non-goals

- Strategy signals/EV (SPEC-0003).
- Signing/submission mechanics (SPEC-0002).

## 4. Position in the pipeline

```
Strategy -> OrderIntent -> [RiskEngine.check] -> Approve | Resize | Reject(reason) -> Execution
                                  ^
                          Portfolio + Limits + AccountView
```

`check` is **synchronous and deterministic** (no network), so it is unit-testable and fast. Any unknown/error condition ⇒ Reject (fail closed).

## 5. Limits (configurable)

**Account-level**
- Max account leverage; max gross and net notional.
- Max margin utilization (% of account value).
- **Min liquidation distance** (maintenance-margin buffer).
- Max daily loss (realized + unrealized) and max drawdown → trip circuit breaker.

**Per-coin**
- Max position notional; max order size; max open orders.

**Per-strategy**
- Capital/margin budget; max concurrent positions.

**Rate**
- Max orders/min; max notional traded per hour.

**Correlated exposure** (optional, v2): aggregate beta/notional across correlated bluechips.

## 6. Pre-trade checks

Ordered, fail-closed checks: kill-switch state → account loss/drawdown breaker → leverage/exposure → per-coin caps → margin & liquidation distance → rate limits → sizing validity. Result is `Approve`, `Resize(≤)`, or `Reject(reason)`; the reason is metric-tagged and logged with the intent's `rationale`.

## 7. Margin & liquidation guard

- Track margin health from `clearinghouseState`: account value, total margin used, maintenance margin used, withdrawable.
- Maintain a configurable buffer above maintenance margin; below it, halt new risk and optionally reduce.
- Handle cross vs isolated margin explicitly.
- Never let a strategy's allocation exceed its margin budget or push the account near liquidation.

## 8. Portfolio state

- Positions/margin (perp), spot balances, and open orders, updated from `orderUpdates`/`userFills` streams plus periodic `/info` reconciliation.
- **Reconciliation:** compare local state to exchange state on a cadence and on reconnect; on drift, resync; if drift persists, halt new risk and alert.
- Open orders are tracked by `cloid`/`oid` for idempotent cancel/replace.

## 9. Accounting & PnL

- **Realized**: fills with `closedPnl`, fees (`fee`, `builderFee`), funding (`userFundings`), maker rebates.
- **Unrealized**: mark-to-market from `AssetCtx`/`clearinghouseState`.
- **Attribution**: per strategy and per coin; equity curve over time.
- Fee accounting uses live effective rates (SPEC-0003) so PnL matches actual costs.

### Persistence — SQLite (primary from day one)

- **Engine:** SQLite via `rusqlite` (`bundled`, no system dependency), **WAL** journal mode.
- **Access model:** a single dedicated **writer task** (actor) receives events over a bounded channel, keeping DB writes serialized and off the latency-critical path; reads use a small connection pool.
- **Schema (insert-only event tables + snapshot tables):**
  - `orders` (intents, submitted, cancels, rejects, cloid/oid, rationale)
  - `fills` (px, sz, fee, builderFee, closedPnl, tid)
  - `funding` (hourly payments), `ledger` (transfers)
  - `positions_snapshot`, `equity_snapshot`, `open_orders_snapshot`
  - `risk_events` (limit checks, breaker trips, kill-switch state)
  - `reconciliation` (local vs exchange diffs)
- **Migrations:** versioned and applied at startup (e.g. `refinery`/embedded migrations).
- **Config:** DB path via `HL_DB_PATH` (default `./data/hlbot.db`); durability via WAL + `synchronous=NORMAL`.
- **Uses:** PnL/attribution queries, equity curve, reconciliation audit, and deterministic **replay/backtest** input.

## 10. Kill switch & circuit breakers

**Manual**
- Signal (e.g. `SIGUSR1`), a watched flag file, and a CLI `panic` command.
- Actions: cancel all resting orders (and `scheduleCancel`), optionally flatten positions (configurable), halt new risk.

**Automatic triggers**
- Daily loss / drawdown breach, margin-health breach, feed staleness, reconciliation failure, order-reject spike, nonce errors.

**Behavior**
- Default action is **cancel + halt**; flatten is opt-in.
- State is sticky until explicitly cleared; every trip is logged, metered, and surfaced on `/healthz`.

## 11. Observability

Metrics: gross/net exposure, margin utilization, liquidation distance, realized/unrealized PnL per strategy/coin, limit utilization, breaker trips, kill-switch state, reconciliation drift. Alerts on any breaker or breach.

## 12. Testing

- Property tests for every limit (boundary approve/resize/reject).
- Fail-closed tests (unknown state ⇒ reject).
- Drawdown/loss/margin breach triggers.
- Reconciliation drift detection and recovery.
- Replay: risk decisions and PnL are deterministic and match recorded sessions.

## 13. Acceptance criteria

- No intent reaches execution without an `Approve`/`Resize` decision.
- A breached limit trips the breaker and halts new risk within one decision cycle.
- Local state converges to exchange state after simulated drift.
- PnL attribution reconciles to exchange-reported fills/funding within tolerance.

## 14. Resolved decisions

1. **Fail-closed** risk checks, synchronous and deterministic.
2. **Kill switch** default action = cancel-all + halt; flatten is opt-in.
3. **Persistence** = **SQLite** (`rusqlite`, bundled, WAL) as the primary store from day one, written by a single dedicated writer task; insert-only event tables plus snapshots; versioned migrations.
4. **Attribution** at strategy and coin granularity.

## 15. Open questions

1. Exact default numeric limits (leverage cap, margin-utilization cap, daily-loss limit) — set conservatively for v1, tune later.
2. Whether automatic flatten should ever be enabled by default (current recommendation: no).
3. Corporate-action/HIP-3 asset handling for exposure aggregation.

## 16. Follow-ups: risk hardening (K-tasks)

Found in the 2026-09-26 review of the live path (SPEC-0010 §2). The hot-path interface and performance contract are in SPEC-0010 §11; this section owns the **rules**. Status: ☐ / 🔄 / ✅.

| ID | Title | Tier | Size | Depends on | Status |
|---|---|---|---|---|---|
| K-1 | Fail-closed defaults: `live` refuses to start without explicit finite limits; conservative defaults for `simulate` | **T0** | S | — | ✅ |
| K-2 | Exposure incl. in-flight orders (worst case), and group worst-single-leg exposure (SPEC-0011 §9) | T1 | M | SPEC-0010 E-5 | 🔄 |
| K-3 | Kill switch: `SIGUSR1`, flag file, `hl panic`; cancel-all + halt; SPEC-0011 `on_kill` for residuals; sticky until cleared | T1 | M | SPEC-0010 E-3 | 🔄 |
| K-4 | Circuit breakers: daily loss, drawdown, reject-rate spike, nonce errors, stale feeds, reconciliation drift, exec backpressure | T1 | M | K-3, SPEC-0010 E-8 | 🔄 |
| K-5 | Rate budgets as risk inputs: IP weight + address budget (`userRateLimit`); cancels always allowed above a hard floor | T1 | S | SPEC-0010 E-6 | ✅ |
| K-6 | PnL & attribution: realized (fills, fees, funding, rebates), unrealized (mark), per strategy / coin / group, written via `DbWriter` | T1 | M | SPEC-0010 E-8, SPEC-0011 L-9 | ☐ |
| K-7 | Property tests for every limit + breaker (boundary approve/resize/reject; unknown state ⇒ reject) | T1 | M | K-1…K-5 | ☐ |
| K-8 | Directional-strategy limits: per-strategy stop-loss, volatility-scaled sizing, max holding time, overnight/weekend exposure caps for HIP-3 stock perps | T3 | M | only when a T3 study passes or a G1.5 pilot is approved | ☐ |

**K-1:** `Config::validate` in `live` requires `max_order_notional_usd`, `max_position_notional_usd`, `max_open_orders`, `max_margin_utilization_bps`, `max_daily_loss_usd`, and `max_unhedged_usd` to all be set. `simulate` uses conservative defaults. **Implemented:** the conservative values live in `RiskSettings::conservative_defaults()` (code), applied by `Config::load` only when `mode != live`, rather than in `config/default.toml`; the config file is mode-agnostic, so putting them there would let a `live` run inherit them and weaken the explicit-limits gate. Defaults: order $2.5k, position $25k, 50 open orders, 50% margin utilization, daily loss $500, unhedged $5k. *Done when:* tests cover live-without-limits ⇒ startup error, and every default is finite.

**K-2:** Per-coin projected exposure = confirmed position + Σ worst-case fills of `PendingNew`/`Resting`/`PartiallyFilled`/`PendingModify`/`Unknown` orders, maintained incrementally by the order manager. Groups are checked on net **and** worst single-leg exposure. *Done when:* a property test shows no approved sequence can exceed a cap.

**K-2 implemented, part 1 (2026-09-26, via SPEC-0010 E-9).** The per-coin rule is live: `mev_engine::risk::RiskGate` checks projected = `AccountState::projected_notional` (confirmed) + `OrderManager::pending_notional` (worst-case in-flight), resizing to the cap; `mev-risk`'s `LimitRisk` also counts resting in-flight gross toward the same cap. A fixed-seed randomized property test shows no approved sequence exceeds the cap. **Remaining:** the group net / worst-single-leg rule needs the SPEC-0011 group types (`RiskGate` returns `GroupUnsupported` for `Action::PlaceGroup`); build it with the SPEC-0011 L-tasks (SPEC-0010 §23 Q-Group-Risk).

**K-3:** The kill flag lives in engine state and is checked first in every risk check. Triggers: `SIGUSR1`, the existence of `HL_KILL_FILE` (default `data/KILL`, polled every 250 ms by a control task), and `hl panic` (writes the file). Action: cancel every working order, then `on_kill` policy (SPEC-0011 §9), then halt. Clearing needs `hl resume` **and** deleting the file. *Done when:* an end-to-end test in `simulate` shows all orders cancelled and no new places within one iteration of each trigger.

**K-3 implemented, part 1 (2026-09-26, via SPEC-0010 E-9).** `mev-risk/src/kill.rs` ships the sticky `KillSwitch` (set/clear/is_active), the `check_flag_file(path)` helper (so a control task can poll `HL_KILL_FILE` off the hot path), and `cancel_all_cloids(&OrderManager)`; `RiskGate` checks the kill flag first and refuses all new places in one iteration. **Remaining:** the `SIGUSR1` handler and `hl panic`/`hl resume` CLI wiring, the 250 ms control-task file poll, and the `simulate` end-to-end test — schedule with E-13's `mev-bot` cleanup (the trigger surface is legacy-engine code).

**K-4:** Each breaker has a threshold in config, a metric (`hl_breaker_trips_total{breaker}`), and a sticky state shown on `/healthz`. Default action: halt new places (cancels allowed). *Done when:* each breaker has a test that trips it.

**K-4 partial (2026-09-26).** The engine-side `RiskGate` has a sticky `Breakers` latch that is checked after the kill switch and halts new places (cancels allowed), and the reconciler's 3-in-10-min drift breaker (SPEC-0010 §15) feeds it. The remaining breaker classes (daily loss, drawdown, reject-rate spike, nonce errors, stale feeds, exec backpressure) and the `hl_breaker_trips_total` metric/`/healthz` surface are still open.

**K-5:** Implement SPEC-0010 §12's budgets as a risk check. *Done when:* tests show places rejected below `rate_budget_min` while cancels pass.

**K-5 implemented (2026-09-26, via SPEC-0010 E-9).** `mev_engine::risk::RateBudget` (IP-weight + address token buckets, caller-supplied time refill) rejects places below `rate_budget_min`/`*_min` and allows cancels above the hard floor; `RiskBudgetSettings` in `mev-core` config; tests cover consume/guard/refill and cancel-pass behaviour. The `hl_rate_budget_remaining{kind}` metric export and the live `userRateLimit` poll (reconciler) are still to be wired (SPEC-0010 E-10/E-12/E-13).

**K-6:** Tables per §9 (extend the existing `fills`/`funding`/`positions_snapshot`; add `groups`). Daily PnL rollup query. *Done when:* a `simulate` day reconciles computed realized PnL against the paper executor's ledger to the cent.

**K-7:** `proptest` suites under `crates/mev-risk/tests/`. *Done when:* they run in CI.

**K-8:** Only built when needed (T3 gate). *Done when:* SPEC-0004 §5 gains a "Directional strategies" block, with tests.
