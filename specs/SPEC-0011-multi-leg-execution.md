# SPEC-0011 — Multi-Leg Execution & Hedging

**Status:** Draft
**Milestone:** M4/M5, **Tier T1** ([`docs/GOAL.md`](../docs/GOAL.md) §2.1). Every T1 arb has ≥ 2 legs.
**Depends on:** SPEC-0010 (engine, order manager E-5, build/batch/sign E-6), SPEC-0004 (risk K-tasks).
**Blocks:** M5 for any multi-leg strategy (O1, O2, O3, O10 A, O11 A, and the funding pilot's spot/perp pair).

---

## 0. How to use this spec

Same rules as SPEC-0010 §0. Pick a task from §14. Facts marked **⚠ verify** need a source (task L-V) before you rely on them.

## 1. Purpose

Provide **one generic executor** for trades that need several orders to fill together: arbitrage, stat-arb spreads, triangles, spot/perp basis. It should:

1. get all legs filled as fast as possible,
2. keep **unhedged exposure** (the position left when some legs filled and others didn't) small and short-lived, with hard limits, and
3. make legging cost and risk **measurable**, so research and live results can be compared.

Strategies describe *what* to trade (a group intent). This spec decides *how*.

## 2. Terms

| Term | Meaning |
|---|---|
| **Leg** | One order in a group: market, side, size, price rule |
| **Group** | The set of legs that together form one trade, with a shared `group_id` |
| **Primary leg** | The leg most likely to fail (thinnest book / least liquid). It goes first when order matters. |
| **Hedge legs** | The other legs, which neutralize the primary's exposure |
| **Hedge ratio** | Quantity relationship between legs: 1:1 notional for the same underlying, β for stat-arb, cycle-derived for triangles |
| **Residual** | Net exposure (in USD, per underlying) after the fills so far, relative to a fully balanced group |
| **Legging window** | Time from the first fill to the group being balanced (residual ≈ 0) |
| **Repair** | Extra orders that remove a residual, either by hedging it or by unwinding what filled |

## 3. Venue facts that shape the design

| Fact | Status |
|---|---|
| A bulk `order` action carries many orders under **one signature and one round trip** | known (SPEC-0002 §7) |
| Orders in one bulk action are processed **in sequence, not atomically**: each can fill or fail independently, in the order sent | ⚠ verify (L-V) |
| IOC orders fill what they can at or better than the limit, and cancel the rest | known |
| ALO (post-only) is rejected if it would cross | known |
| Min order notional $10; size rounded to `szDecimals`; price tick rules per asset | known (SPEC-0002 §6) |
| Address rate limit counts **each order** in a batch | known (SPEC-0002 §10) |
| Spot and perp live in different balances (spot tokens vs perp margin); moving USDC between them is a separate user-signed action | ⚠ verify how this applies to our account type (L-V) |
| HIP-3 perps margin in their own collateral; cross-dex positions don't net | ⚠ verify (L-V) |

The key consequence: **there is no atomic multi-leg order on HyperCore**. Every multi-leg trade carries legging risk, so it has to be bounded by design.

## 4. Group intent (what strategies emit)

```rust
pub struct GroupIntent {
    pub strategy: StrategyId,
    pub group_id: GroupId,              // assigned by the engine if 0
    pub legs: SmallVec<[LegIntent; 4]>,
    pub style: ExecStyle,               // §5
    pub expected_edge_bps: Decimal,     // at decision time, net of fees (SPEC-0008 §13.2 model)
    pub min_edge_bps: Decimal,          // abort / don't send if re-priced edge falls below this
    pub max_notional_usd: Decimal,      // cap for the whole group
    pub rationale: String,              // audit only (goes to the persistence thread)
}

pub struct LegIntent {
    pub coin: CoinId,
    pub side: Side,
    pub ratio: Decimal,                 // quantity per unit of group size (hedge ratio)
    pub role: LegRole,                  // Primary | Hedge
    pub price: LegPrice,                // Touch{ max_slippage_bps } | Limit(Px) | Passive{ offset_ticks }
}
```

Strategies don't set sizes directly. The executor sizes the group (§6).

## 5. Execution styles

| Style | How it works | Legging risk | Fees | Use when |
|---|---|---|---|---|
| **`TakerBatch`** (default for fast arb) | All legs IOC in **one bulk `order` action**, primary first in the batch. One round trip. | Medium: a leg can miss (the price moved, or thin liquidity) | taker × all legs | The episode is short (SPEC-0008 `competition_hint` = latency-competitive), and all legs are liquid |
| **`PrimaryThenHedge`** | Send the primary leg IOC alone. On its ack/fill, immediately send the hedge legs IOC, sized to the **filled** primary quantity. | Low: hedges are sized to actual fills | taker × all legs | The primary leg is thin or often misses; one extra round trip is affordable |
| **`MakerPrimary`** | Rest the primary as ALO at a price that still leaves ≥ `min_edge_bps` if hedged at the current touch. On every (partial) fill, hedge that quantity IOC at once. Re-price or cancel the primary when the edge re-computed at the touch drops below `min_edge_bps`. | Higher: adverse selection on the resting order, plus the hedge-slippage window | maker on primary, taker on hedges | Slower dislocations (e.g. stat-arb spreads, O11 A); fee savings matter |

The style is chosen by the strategy and justified by its study (the SPEC-0008 report says which style its numbers assume). Default: `TakerBatch`.

## 6. Sizing

1. For each leg, walk its current book (SPEC-0010 `MarketSlot`) up to its price limit: `Touch{max_slippage_bps}` → the fillable quantity `q_i` at acceptable prices.
2. Group size `G = min_i(q_i / ratio_i)`, capped by `max_notional_usd`, by risk headroom (SPEC-0004: projected exposure incl. worst-case legging), and by any per-strategy cap.
3. Each leg quantity = `G × ratio_i`, rounded **down** to its lot. If any leg's notional is < $10 (min order), or rounding changes the hedge ratio by more than `ratio_tolerance` (default 1%), shrink `G` or skip the group.
4. Re-price the edge with the rounded sizes and the walked prices. If it's below `min_edge_bps`, don't send (count `hl_group_skipped_total{reason="edge"}`).

Sizing is synchronous and allocation-free on the engine thread. Target: ≤ 20 µs for a 3-leg group.

## 7. Group lifecycle (state machine)

| State | Meaning | Transitions |
|---|---|---|
| `Sized` | Sized and re-priced; passes edge check | → `RiskChecked` or `Skipped` |
| `RiskChecked` | Group-level risk approved (§9) | → `Sent` |
| `Sent` | Legs handed to exec (all at once, or the primary only, by style) | → `Balanced`, `Residual`, `Failed` |
| `Balanced` | All fills consistent with the ratios (residual ≤ `balance_tolerance_usd`, default $5) | → `Closed` |
| `Residual` | Some legs filled, others didn't or only partly | → `Repairing` |
| `Repairing` | Repair orders out (§8) | → `Balanced`, `Unwinding`, `Stuck` |
| `Unwinding` | Reversing filled legs to flatten | → `Closed`, `Stuck` |
| `Failed` | Nothing filled | → `Closed` |
| `Stuck` | Repair/unwind limits exhausted and residual still open | Trips the breaker, alerts, halts the strategy; a human decides |
| `Closed` | Terminal: final PnL and legging metrics recorded | — |

Per-leg orders use SPEC-0010's order state machine. A leg in `Unknown` counts as **fully filled** for residual purposes until resolved (worst case).

## 8. Repair policy

When a group enters `Residual`:

1. **Hedge first.** Send IOC orders on the leg(s) that under-filled, for the missing quantity, with a wider cap `repair_slippage_bps` (default 2 × the leg's `max_slippage_bps`). If the leg is too thin, hedge on the **most liquid equivalent** market allowed by the strategy (e.g. the main-dex perp for a HIP-3 residual), if configured.
2. **Unwind if hedging fails.** After `max_repair_attempts` (default 2), or if the repair would cost more than `max_repair_cost_bps` (default 20 bps of residual notional), reverse the filled legs IOC to flatten.
3. **Hard limits** (checked every iteration while a residual is open):
   - `max_unhedged_usd` (default $2,000; must be ≤ the SPEC-0004 per-strategy cap): if the residual exceeds it, skip straight to unwind.
   - `max_unhedged_ms` (default 2,000 ms): if the residual is still open, unwind at `repair_slippage_bps × 2`.
4. If unwind fails too → `Stuck` (breaker, alert, strategy halted).
5. While a strategy has any group in `Residual`/`Repairing`/`Unwinding`, it cannot open new groups on the same underlying.

Every repair records its cost (bps and USD) against the group, so realized edge = expected edge − fees − slippage − repair cost.

## 9. Risk integration

- Risk checks the **group as a whole** before `Sent`:
  - net projected exposure after all legs fill (should be ~0 for neutral groups);
  - **worst-case single-leg exposure**: the largest exposure if only one leg fills (this is what legging can actually leave you with). It must fit the per-coin and per-strategy caps;
  - the rate budget covers all legs (and one repair round).
- Residuals count as real exposure in SPEC-0004's exposure accounting.
- **Kill switch:** cancel all resting legs, then apply `on_kill` for open residuals:
  - `neutralize` (default): run repair (hedge or unwind) under the hard limits, then halt. Leaving a legged residual open is itself unmanaged risk.
  - `leave`: halt without touching residuals (for manual handling).

## 10. Scope limits (v1)

- **HyperCore legs only** (main-dex perps, spot, HIP-3). CEX, equities, and HyperEVM legs are out of scope. `LegIntent` gets a `venue` field later.
- Non-atomic inventory arb across HyperCore ↔ HyperEVM (O4) needs pre-positioned inventory and transfers. That's a later spec if O4 passes.

## 11. Configuration

```toml
[execution.groups]               # defaults; strategies may override per style
balance_tolerance_usd = 5
ratio_tolerance       = 0.01
repair_slippage_bps   = 20
max_repair_attempts   = 2
max_repair_cost_bps   = 20
max_unhedged_usd      = 2_000
max_unhedged_ms       = 2_000
on_kill               = "neutralize"   # "neutralize" | "leave"
```

## 12. Metrics

| Metric | Labels |
|---|---|
| `hl_group_total{outcome}` | balanced / repaired / unwound / failed / skipped / stuck |
| `hl_group_send_to_balanced_seconds` | strategy, style |
| `hl_group_legging_window_seconds` (first fill → balanced) | strategy, style |
| `hl_group_residual_usd_max` | strategy |
| `hl_group_repair_cost_bps` | strategy, style |
| `hl_group_edge_bps{kind="expected"\|"realized"}` | strategy, style |
| `hl_group_leg_miss_total` | strategy, coin |

Every group is also persisted (group row + leg orders + fills + repair orders) for attribution (SPEC-0004 K-6).

## 13. Testing

- **Scripted venue simulator** (extends `PaperExec`): a scenario lists per-leg outcomes (fill, partial x%, miss, reject, delay, unknown then resolved). Table-driven tests for every §7 transition and §8 branch.
- **Property tests:** for random scenarios, (a) the residual never exceeds `max_unhedged_usd` for longer than one iteration without an unwind being sent; (b) once `Closed`, the net position is within `balance_tolerance_usd` of flat, unless `Stuck`; (c) no new group on an underlying with an open residual.
- **Replay:** O1/O3 fixtures through `hl replay` with `TakerBatch` vs `PrimaryThenHedge`; compare realized-edge distributions (feeds the style choice).

## 14. Work breakdown

All tasks are **T1**.

| ID | Title | Size | Depends on | Status |
|---|---|---|---|---|
| L-V | Verify §3 venue facts (bulk-order sequencing, spot/perp balances, HIP-3 margin) | S | — | ☐ |
| L-1 | `GroupIntent` / `LegIntent` types + `Action::PlaceGroup` | S | SPEC-0010 E-1 | ☐ |
| L-2 | Group sizing (§6) with book walks and rounding | M | L-1, SPEC-0010 E-5 | ☐ |
| L-3 | Group state machine (§7) on top of the order manager | M | L-1, SPEC-0010 E-5 | ☐ |
| L-4 | Styles: `TakerBatch`, `PrimaryThenHedge` | M | L-3, SPEC-0010 E-6 | ☐ |
| L-5 | Repair / unwind policy + hard limits (§8) | M | L-3 | ☐ |
| L-6 | Group-level risk (§9) + kill-switch `on_kill` | M | L-3, SPEC-0004 K-2, K-3 | ☐ |
| L-7 | Scripted venue simulator + property tests (§13) | M | L-4, L-5 | ☐ |
| L-8 | `MakerPrimary` style | M | L-7 | ☐ |
| L-9 | Metrics + persistence of groups (§12) | S | L-3 | ☐ |
| L-10 | Port `FundingBasis` spot/perp entry and exit to `PlaceGroup` | S | L-4 | ☐ |

**Task details**

- **L-V:** Answer each ⚠ in §3 from the official docs and, where possible, a testnet experiment (owner-provided testnet key only). *Done when:* §15 has each answer with a source.
- **L-1:** Types in the engine crate; `Actions` accepts `PlaceGroup`. *Done when:* the types compile with docs; a unit test builds a 3-leg triangle intent.
- **L-2:** §6. *Done when:* tests cover thin-leg limiting, lot rounding breaking the ratio (shrink or skip), the min-notional skip, and re-priced edge below `min_edge_bps` ⇒ skip; the bench is ≤ 20 µs for 3 legs.
- **L-3:** §7, driven by order-manager events. *Done when:* table-driven tests cover every transition, including `Unknown` legs counted as filled.
- **L-4:** `TakerBatch` builds one bulk action with the primary first; `PrimaryThenHedge` sends the hedge on the primary's fill, sized to the filled quantity. *Done when:* mock-venue tests show one post for `TakerBatch`, and hedge sizes that match partial primary fills for `PrimaryThenHedge`.
- **L-5:** §8 incl. hard limits by size and time. *Done when:* tests cover hedge success, hedge failure → unwind, `max_unhedged_ms` expiry, `max_unhedged_usd` breach, and `Stuck` tripping the breaker.
- **L-6:** §9. *Done when:* a group whose worst-case single leg breaches a cap is rejected; kill switch + `neutralize` flattens residuals in the simulator.
- **L-7:** §13. *Done when:* property tests run in CI (bounded case count).
- **L-8:** `MakerPrimary` with re-pricing and fill-by-fill hedging. *Done when:* simulator tests cover partial maker fills hedged incrementally, and cancel when the edge disappears.
- **L-9:** §12 metrics + a `groups` table (migration in `hl-arb-core/src/db.rs`). *Done when:* metrics appear in `/metrics` in `simulate`, and rows are written for each group.
- **L-10:** `FundingBasis` enters and exits through a spot/perp `PlaceGroup` (`PrimaryThenHedge`, spot as primary). *Done when:* its tests pass, and a `simulate` run shows balanced groups.

## 15. Verified facts (filled in by L-V)

| Fact | Answer | Source / date |
|---|---|---|
| Bulk `order` processing: sequential? atomic? | | |
| Spot vs perp balances for our account type | | |
| HIP-3 margin/collateral and netting | | |

## 16. Acceptance criteria

- [ ] L-7 property tests pass in CI.
- [ ] In `simulate` over ≥ 24 h, every group ends `Closed` (none `Stuck`), and `hl_group_residual_usd_max` stays ≤ `max_unhedged_usd`.
- [ ] Replay comparison of `TakerBatch` vs `PrimaryThenHedge` for at least one T1 study, recorded in that study's report.
- [ ] `FundingBasis` uses groups (L-10).

## 17. Open questions

1. Default `max_unhedged_usd` / `max_unhedged_ms` per strategy class. The defaults here are conservative placeholders; set them from study data (episode durations, book depth).
2. Should repair ever hedge on a *different venue* (e.g. a CEX) once cross-venue legs exist? Out of scope for v1.
