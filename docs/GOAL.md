# GOAL

> **Every agent and contributor reads this file first.** If a task you are
> given conflicts with this document, stop and flag it instead of guessing.

## 1. The goal in one sentence

Build an **automated arbitrage / MEV-style trading system for Hyperliquid**
(HyperCore first, HyperEVM where it pays) where **every strategy that trades
real money is justified by edge measured from recorded data, net of all costs**.

## 2. What "MEV / arb on Hyperliquid" actually means

Hyperliquid is not Ethereum. Read this before proposing any strategy.

| Venue | What exists | What does **not** exist |
|---|---|---|
| **HyperCore** (the L1 order books: perps, spot, HIP-3 perps) | A fully on-chain central limit order book with block-by-block sequencing. Edge comes from **price dislocations between markets**, **being faster than other traders**, and **predictable flow** (liquidations, funding, oracle updates). | A public mempool you can see and reorder. **No sandwiching, no classic backrunning** of other users' orders. |
| **HyperEVM** (chain 999, an EVM that sits next to HyperCore) | DEX pools whose prices drift from HyperCore prices; classic EVM arbitrage. Possibly backrunning, **if** pending transactions are visible (unverified — see SPEC-0008 study O8). | Atomic HyperCore execution from a contract: `CoreWriter` actions are **deliberately delayed** by a few seconds. |

So in this project, "MEV/arb" means, in rough order of expected fit:

1. **Dislocations inside HyperCore.** The same underlying on several HIP-3 dexes or the main dex; spot pairs across stablecoin quotes; spot vs perp.
2. **HyperCore ↔ HyperEVM.** The same token priced differently on a HyperCore spot book and on a HyperEVM DEX. You hold inventory on both sides and trade each leg separately (non-atomic).
3. **Cross-venue latency.** Binance/Bybit move first and HyperCore quotes lag.
4. **Flow events.** Liquidation cascades, oracle-update timing, funding settlement.
5. **Carry.** Funding/basis. This is not arb; it is a low-risk **pilot** used to prove the stack end to end.

Which of these we actually build is **decided by data** (SPEC-0008), not by
preference.

## 3. How we measure success

| Kind | Metric | Target |
|---|---|---|
| North star | Net realized PnL after fees, funding, gas, and slippage | Positive, and above the ADR-0002 hurdle (set by the owner) |
| Evidence | Every live strategy has a research report showing net-positive edge at a realistic latency | 100% of live strategies |
| Safety | Loss-of-funds incidents caused by bugs (wrong size, stuck orders, runaway loops) | **Zero** |
| Safety | Orders sent without passing the risk engine | **Zero** |
| Operability | Recorder and bot uptime; feed staleness | ≥ 99% of time with fresh feeds |

Capital allocation and the numeric profit hurdle are owner decisions. They are
recorded in `specs/decisions/0002-*.md` once SPEC-0008 finishes.

## 4. Principles (non-negotiable)

1. **Evidence before strategy.** No strategy code goes live without a
   SPEC-0008 study showing net-positive edge. "It should work" is not evidence.
2. **Safe by default.** The default mode is `observe`. `live` requires explicit
   configuration plus `HL_LIVE_CONFIRM=YES`. Agents **never** run `live` and
   never handle mainnet keys.
3. **Strategies propose, risk disposes.** Every order goes through the
   fail-closed risk engine (SPEC-0004).
4. **Spec first.** Behavior is specified in `specs/` before it is built. If
   reality differs from a spec, update the spec in its own commit.
5. **Exact money math.** `rust_decimal` in every production money path; never
   `f64`. (Research code in `research/` may use floats.)
6. **Record everything.** Raw market data and every decision/order/fill are
   persisted, so any result can be replayed and audited.
7. **Measure latency; don't assume it.** Report latency-sensitive results at
   several latency assumptions.

## 5. Non-goals

- Ethereum mainnet MEV (retired; see `legacy/`).
- Sandwiching or anything that harms retail users by front-running their orders.
- Polymarket (parked, SPEC-0007).
- Building a strategy because it is interesting rather than because data says it pays.

## 6. Roadmap

Spec numbers are **stable identifiers, not an order**. This table is the order.

| # | Milestone | Spec(s) | Status | Depends on |
|---|---|---|---|---|
| M0 | Platform: config, modes, observability, CI | SPEC-0000 | ✅ done | — |
| M1 | HyperCore market data client, local state, benchmark | SPEC-0001, ADR-0001 | ✅ done | M0 |
| M2 | Execution: signing, nonce, order builder, transports, dead-man switch | SPEC-0002 | ✅ done (testnet round-trip open) | M1 |
| M2.5 | **Execution hardening**: concurrent WS post, account stream, mandatory `cloid`, dead-man policy, stream watchdog | SPEC-0002 §17 | ⏳ next | M2 |
| **M3** | **Market-data recorder + opportunity research → ADR-0002** | **SPEC-0008** | ⏳ **now** (runs in parallel with M2.5) | M1 |
| M4 | Engine + risk + persistence; **funding-carry pilot** at small size | SPEC-0003 (A), SPEC-0004 | planned | M2.5, M3 recorder |
| M5 | **First arb strategy**, the one chosen by ADR-0002 | SPEC-0009 (written after ADR-0002) | planned | M3 gate, M4 |
| M6 | Market-making | SPEC-0003 (B) | only if research supports it | M4 |
| M7 | HyperEVM sources + executor | SPEC-0005 | only if studies O4/O8 pass | M3 gate, M4 |
| M8 | Production ops: deploy, alerting, backups | SPEC-0006 | runs alongside M4+ | — |

**Why M3 comes before the funding strategy:** recorded data builds up over
calendar time. Every day the recorder isn't running is a day of evidence lost.
The recorder depends only on M1 (market data), so it can start immediately.

## 7. What to work on right now

1. **SPEC-0008 Phase V and Phase R.** Verify facts, then build the recorder
   and deploy it to a low-latency host.
2. **SPEC-0002 §17** (M2.5 hardening), in parallel.
3. Then SPEC-0008 Phases P and S (research toolkit and studies), which end in
   ADR-0002.

## 8. Decision gates

| Gate | Question | Evidence required | Recorded in |
|---|---|---|---|
| G1 (end of M3) | Which arb do we build first, or none? | SPEC-0008 study reports + `RANKING.md` covering ≥ 14 days of data | ADR-0002 |
| G2 (before any `live`) | Is the stack safe with real money? | Testnet round-trip, kill-switch drill, risk property tests, pilot in `simulate` | SPEC-0004/0006 checklists |
| G3 (before scaling size) | Does realized edge match researched edge? | ≥ 7 days live at small size; realized vs expected within tolerance | Strategy spec |

## 9. Glossary

| Term | Meaning |
|---|---|
| HyperCore | Hyperliquid's L1 trading engine (perp/spot order books). No gas for trading. |
| HyperEVM | EVM chain (id 999) running alongside HyperCore; gas paid in HYPE. |
| HIP-3 | Builder-deployed perp dexes; coins are dex-qualified, e.g. `xyz:TSLA`. |
| Agent / API wallet | A key the master account authorizes to trade. It **cannot withdraw**. The only key on the host. |
| bps | Basis points; 1 bps = 0.01%. |
| Edge (gross / net) | Price advantage before / after fees, slippage, funding, gas, and a safety buffer. |
| Episode | A contiguous stretch of time during which an opportunity is net-positive (SPEC-0008). |
| Leg / legging risk | One side of a multi-market trade; the risk that one leg fills and the other doesn't. |
| `observe` / `simulate` / `live` | Execution modes: no orders / signed but never sent / real orders. |
| Recorder | The M3 process that stores raw market data for research and replay (SPEC-0008). |
