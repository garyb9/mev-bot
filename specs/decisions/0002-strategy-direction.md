# ADR-0002 — Strategy direction

**Status:** Proposed (PROVISIONAL — decision pending owner) · **Date:** 2026-10-03 ·
**Spec:** SPEC-0008 §13 (gate G1)

> This ADR is provisional. The studies below are preliminary (about four days of
> forward recorder data, two of them out-of-sample), several inputs are missing
> (competition model, funding charged, real fee tier), and no strategy has been
> through a full ≥ 14-day gate. The owner decides.

## Context

SPEC-0008 is the evidence gate for what we trade (GOAL §4.1, gate G1). The
recorder has run since 2026-09-29 and the research toolkit (P-2…P-5, B-1…B-3,
B-9) is built, so the first preliminary studies have run. Their verdicts are in
`research/reports/prelim-2026-10-03/` (written by the orchestrator from subagent
hand-backs; UNVERIFIED items are marked) and the fee break-even work is in the
same directory.

## Preliminary evidence

| Study | Question | Preliminary verdict |
|---|---|---|
| S-1 (O1) | HIP-3 cross-dex dislocations | INCONCLUSIVE, leans MARGINAL — thin, ~$10/day |
| S-2 (O2) | Stablecoin spot triangles | FAIL — best OOS APR 8.6%, CI low 0.4% |
| S-3 (O3) | Spot vs perp | INCONCLUSIVE, leans FAIL — majors ~0 episodes, thin alts are dust |
| S-5 (O5) | CEX lead-lag / stale quotes | FAIL — lead-lag is real (~0.5 s) but gross ~4 bps vs a 9 bps taker round trip |
| S-11a (O11a) | Bollinger bands on spreads | FAIL — 0 of 792 cells positive |
| S-12 (O12) | Oracle-tick lag | INCONCLUSIVE — not testable at 1 Hz `ctx` |
| S-19 (O19) | Quote-asset peg defense | FAIL — never beyond the round-trip cost |

**Finding: no strategy has positive net edge at the base fee tier.** Every
number is a generous upper bound: the competition model (`compete_usd`) is
absent and funding is not charged, so a FAIL is robust while a pass would not
be. The lead-lag signal is real; the obstacle is the venue round trip.

## Options (none chosen yet)

1. **Fee-aware maker with a CEX signal.** Quote ALO on HL and skew/cancel on
   Binance moves (lead-lag ~0.5 s). A differentiator only if fills can be
   modelled honestly: recorded trades + a conservative queue model. The address
   rate limit (1 request per USDC traded, 10k buffer) constrains quoting.
2. **Funding carry / cross-venue funding differential.** Delta-neutral spot+perp
   or cross-dex funding; a low-risk pilot that also proves the stack end to end.
3. **HIP-3 growth-mode legs.** Growth mode cuts protocol taker fees ~90%
   (~0.45–0.9 bps), which could move S-1/S-5; the para deployer fee scale
   (0.5 vs observed 1.0) must be resolved first.

## Kill tests (cheapest first, no live funds)

These gate each option and come from the product review (PROD-001..008); the
full text of the five bets is in the reviewer's hand-back summarised in the
preliminary reports.

1. **Funding carry:** trailing-60-day net APR ≥ 10% with a positive confidence
   interval for at least one coin; kill otherwise.
2. **Growth-mode HIP-3:** no growth-mode asset has a gross lag edge > 2× its
   round-trip cost or touch depth > $5k → kill.
3. **Maker with CEX signal:** needs the competition model and a queue model;
   net markout ≤ 0 in-sample and out-of-sample → kill.
4. **HIP-3 stock perps (non-latency):** sign-hit rate net of 9 bps < 55% or
   t < 2 → kill.
5. **Liquidation/ADL flow:** net post-event drift after 9 bps < 0 on all events
   → kill (and its data is not yet available).

The thesis demo (PROD): a one-page net-markout report for one asset (maker fills
simulated from recorded trades with a conservative queue, split by CEX-signal
state, real fee scenario). Positive in-sample and out-of-sample means the maker
thesis has legs; otherwise the direction is carry plus directional trading.

## Decision

**Pending owner.** No option is selected. Until one bet clears its kill test,
new engine/hot-path scope is frozen and effort goes to the recorder days and the
kill tests above.

## Consequences

- The M5 "first strategy" spec stays unwritten until this ADR is accepted.
- S-1/S-2/S-3/S-5/S-11a/S-12/S-19 stay open in SPEC-0008 pending ≥ 14 days and a
  final report; the PRELIM annotations are not the gate.
- Facts that must be verified before any option is built: the HL fee tier, the
  `para` deployer fee scale and `scaleIfHip3` formula, the `scheduleCancel`
  volume gate, and the recorded-trades competition model.
