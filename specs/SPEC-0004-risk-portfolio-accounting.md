# SPEC-0004 — Risk, Portfolio & Accounting

**Status:** Draft
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
