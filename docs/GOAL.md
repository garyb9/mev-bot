# GOAL

> **Every agent and contributor reads this file first.** If a task you are
> given conflicts with this document, stop and flag it instead of guessing.

## 1. The goal in one sentence

Build a **fast, automated arbitrage / MEV-style trading system for
Hyperliquid** (HyperCore first, HyperEVM where it pays). It must be **as low
latency as we can make it**, and **every strategy that trades real money must
be justified by edge measured from recorded data, net of all costs**.

There are two strategy families, both running on the same fast infrastructure:

1. **Arbitrage / MEV-style** (primary): short-lived price dislocations where
   speed decides who captures the edge.
2. **Options-informed directional trading** (secondary): using options-market
   positioning (option chains this bot records itself, plus Deribit) to trade
   **HIP-3 tokenized-stock perps** and crypto bluechips over hours to days.

**This repo is where we trade.** The owner's `finsnap` project is a view (a
dashboard). It's a source of ideas and of past options snapshots, never a
runtime dependency.

When two designs are equally correct and equally safe, **choose the faster
one**. Speed never overrides correctness or safety (§4), but it outranks
convenience, elegance, and dependency preferences. (For the slower
directional family, data quality and no-lookahead discipline matter more than
microseconds, but it still runs on the same low-latency stack.)

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
6. **HIP-3 tokenized stocks vs the real stock.** Stock perps trade 24/7 while the stock and its options trade only in US hours. That gives market-hours dislocations vs the live stock price, lead-lag vs BTC/index perps while US markets are closed, and weekend/overnight information that may carry into the next session or week (SPEC-0008 O10).
7. **Mean reversion, arb-style.** Bollinger bands (finsnap's best historical strategy) applied to the *spread* between related instruments (market-neutral stat-arb) and to single HL perps on short timeframes (SPEC-0008 O11).
8. **Options-informed direction** (the second family). Options positioning (put/call imbalance, skew, open-interest walls; IV for crypto) as a signal for HIP-3 stock perps and BTC/ETH (SPEC-0008 O9). It is directional, so it carries market risk and needs its own risk limits before it can go live.

Which of these we actually build is **decided by data** (SPEC-0008), not by
preference. If you have a new idea, add it as a study there first.

### 2.1 Priority tiers

To keep the arb core from being starved by everything else, all work is tiered.
Agents always pick the highest tier available: **T0 before everything**.
Full task lists: the T0 list below, then SPEC-0008 §14.0–14.1.

| Tier | What | Items | When |
|---|---|---|---|
| **T0: fix-first** | Known bugs and unsafe defaults on the real-money path, found in reviews | The §2.2 list | **Before any other work, and before any testnet or live run** |
| **T1: arb core** (latency-first) | Recorder, research toolkit, fast dislocation / arb studies, HyperEVM MEV feasibility (desk research), execution hardening (M2.5), **event-driven engine (SPEC-0010), multi-leg execution (SPEC-0011), risk hardening (SPEC-0004 K-1…K-7), CI (SPEC-0000 C-tasks)** | Items 1, 3, 6 (in-hours + closed-hours lead-lag), 7 (spread bands); study O8 Q1–Q4 | **First, always** |
| **T2: adjacent** | Reuses T1 data, or waits on heavier infrastructure | Items 2 (needs HyperEVM RPC / node), 4, 5; HIP-3 funding patterns | When T1 is done, blocked, or taken |
| **T3-data** | Starting the clock on forward options data | Options-chain and Deribit recording | **Any time** (small, time-sensitive) |
| **T3: directional family** | Signals held hours to days | Item 8; open convergence and weekend → week-ahead (item 6); single-instrument Bollinger (item 7) | After the recorder is in production **and** ≥ 3 T1 studies have preliminary reports |

### 2.2 T0 fix-first list

These are defects, not features. Each has a precise task in its spec (the spec
holds the status). An agent starting work checks this list first. If any item
is open and its dependencies are met, the agent does it before any T1 task.

| # | Fix | Why it's T0 | Task |
|---|---|---|---|
| 1 | Every live order gets a `cloid`; parse per-order exchange statuses (no "submitted" for rejected orders); no mid-price limit for aggressive orders; count same-cycle intents toward caps | Wrong or unrecoverable orders with real money | [SPEC-0010 E-0](../specs/SPEC-0010-event-driven-engine.md) |
| 2 | `live` refuses to start without explicit, finite risk limits (today every limit defaults to unlimited) | Risk is fail-open by default | [SPEC-0004 K-1](../specs/SPEC-0004-risk-portfolio-accounting.md) |
| 3 | Concurrent WS `post`: stop holding the socket lock while waiting for a reply | Only one order in flight; a dead-man refresh blocks orders and cancels | [SPEC-0002 H-1](../specs/SPEC-0002-execution-signing.md) |
| 4 | Unknown order outcomes are reconciled by `cloid` before anything is resent | A dropped socket after send leaves orders in an unknown state | [SPEC-0002 H-2](../specs/SPEC-0002-execution-signing.md) |
| 5 | Dead-man switch: halt trading if arming/refreshing fails; arm only while orders rest; stop burning the address rate-limit budget | Live trading continues unprotected after an arm failure; the budget runs out in ~2 days | [SPEC-0002 H-4](../specs/SPEC-0002-execution-signing.md) |
| 6 | Verify the venue's nonce and `scheduleCancel` rules | Items 4 and 5, and the nonce design, rest on unverified assumptions | [SPEC-0002 H-9](../specs/SPEC-0002-execution-signing.md) |
| 7 | Market stream: pong/data watchdog, jittered backoff, cancellable reconnect | Silent stale feeds; reconnect storms; shutdown can hang | [SPEC-0008 R-3](../specs/SPEC-0008-recorder-and-opportunity-research.md) |
| 8 | A lost reply (`PostResult::Error`) is recorded as a terminal `Rejected`, dropping the order from exposure and the dead-man resting count, instead of `Unknown` resolved by `orderStatus` by `cloid` | A live order can be resting yet invisible to risk and the dead-man switch | [SPEC-0002 H-2](../specs/SPEC-0002-execution-signing.md), [SPEC-0010 E-13](../specs/SPEC-0010-event-driven-engine.md) |
| 9 | The exec writer awaits each post, so only one order is ever in flight (undoes H-1; violates §5.1) | A place waits a full round trip behind its cancel | [SPEC-0002 H-1](../specs/SPEC-0002-execution-signing.md), [SPEC-0010 E-6](../specs/SPEC-0010-event-driven-engine.md) |
| 10 | No kill-switch trigger exists (no `SIGUSR1`, no flag-file poll, no `hl panic`/`hl resume`), though the RUNBOOK documents them | Gate G2 requires a kill-switch drill; there is no way to trip it | [SPEC-0004 K-3](../specs/SPEC-0004-risk-portfolio-accounting.md), [SPEC-0010 §16](../specs/SPEC-0010-event-driven-engine.md) |
| 11 | The H-3 account stream is not wired into `hl`, so live cancels never resolve, finished orders are never pruned, and `resting_count()` rescans every order each iteration | Live order state drifts, the dead-man count is wrong, and per-iteration cost grows without bound | [SPEC-0002 H-3](../specs/SPEC-0002-execution-signing.md), [SPEC-0010 E-8](../specs/SPEC-0010-event-driven-engine.md) |

Order: 6 (quick) and 1 + 2 first (small, and they touch the same files);
then 3 → 4 → 5; 7 in parallel. Rows 8–11 were added in the post-E-13 review
(2026-09-27): 8 and 11 come first (a real order can be invisible to risk),
then 9, then 10; all before any testnet or live run. New defects found in
later reviews are added here.

Strategy code for any tier waits for gate G1, or an owner-approved G1.5 pilot.
The funding pilot (M4) is a stack-prover, not a tier. Strategy code, including
market-making (M6), **may be written ahead of research** and revised later.
Tiers set priority, not permission. What research gates is **trading**: no
strategy goes live without G1 or a G1.5 pilot.

## 3. How we measure success

| Kind | Metric | Target |
|---|---|---|
| North star | Net realized PnL after fees, funding, gas, and slippage | Positive; APR at or above the floor, aiming for the target (below) |
| Evidence | Every live strategy has a research report showing net-positive edge at a realistic latency | 100% of live strategies |
| **Speed** | Internal tick-to-order latency (frame read → order bytes written to the socket) | p50 ≤ 100 µs, p99 ≤ 1 ms (see §5) |
| **Speed** | Network path: order sent → venue acknowledgement | As low as the best measured host allows; tracked per release, never allowed to regress |
| Safety | Loss-of-funds incidents caused by bugs (wrong size, stuck orders, runaway loops) | **Zero** |
| Safety | Orders sent without passing the risk engine | **Zero** |
| Operability | Recorder and bot uptime; feed staleness | ≥ 99% of time with fresh feeds |

**Owner decisions (2026-09-26), adjustable at any time in
`research/thresholds.toml` (SPEC-0008 §13.6):**

| Setting | Value | Meaning |
|---|---|---|
| Capital | **Small to medium**: evaluated at $10k / $25k / $50k / $100k; headline $25k | Opportunities are judged at the sizes we'll actually trade |
| APR target | **25%** | At or above ⇒ PASS: build it |
| APR floor | **10%** | Between floor and target ⇒ MARGINAL: acceptable, built if cheap or nothing better; below ⇒ FAIL |

The final choice of what to build is recorded in ADR-0002
(`specs/decisions/0002-*.md`) when SPEC-0008's studies are done.

## 4. Principles (non-negotiable)

1. **Evidence before strategy.** No strategy code goes live without a
   SPEC-0008 study showing net-positive edge. "It should work" is not evidence.
   The one exception is an owner-approved **pilot** (gate G1.5): small,
   capped, time-boxed, with a hard loss limit.
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
7. **Latency is a feature, and it is measured.** Every hot-path change comes
   with a benchmark or a latency metric, and a regression is treated as a
   bug. Research reports results at several latency assumptions; production
   reports measured latency. See §5.

## 5. Latency first

### 5.1 Hot-path rules

The **hot path** is everything between "a market-data frame arrives" and
"order bytes are written to the socket": decode → state update → strategy →
risk check → order build → sign → send.

| Rule | Why |
|---|---|
| No disk I/O, database writes, or blocking calls on the hot path. Hand persistence and logging to background tasks over bounded channels (drop, never block). | A single `fsync` or lock wait costs more than the entire compute budget |
| No `tracing` at INFO or above per event on the hot path; use counters/histograms and sampled DEBUG logs | Formatting and I/O are slow |
| Avoid allocation per event: reuse buffers, prefer fixed-size types and pre-sized collections | The allocator causes tail latency |
| No `Mutex` held across `.await` on the hot path; one owner per piece of state (single-threaded engine / actor) | Lock contention causes p99 spikes |
| Keep connections persistent, warm, and pre-authenticated; no connecting on demand | A TCP/TLS handshake costs milliseconds |
| Allow many in-flight order requests on WS `post` (never one at a time) | Serializing sends queues orders behind each other |
| Pre-compute whatever can be pre-computed (asset ids, rounding rules, signer state) | Moves work off the critical moment |
| Decode only what the strategy needs; skip or lazily parse the rest | Decode is the largest compute cost (ADR-0001) |
| Deploy close to the venue (region chosen by measurement, SPEC-0008 V-4). Consider running our own node if it gives faster data. | Network distance dominates everything else |
| The recorder and research tooling run as **separate processes** and never share the trading hot path | Research must never slow down trading |

### 5.2 Latency budget (initial targets; refine by measurement)

| Stage | Target p50 | Target p99 | Measured by |
|---|---|---|---|
| WS frame → decoded event | ≤ 20 µs | ≤ 100 µs | `decode` bench + runtime histogram |
| State update + strategy decision | ≤ 30 µs | ≤ 200 µs | runtime histogram |
| Risk check | ≤ 10 µs | ≤ 50 µs | unit bench + runtime histogram |
| Order build + msgpack + EIP-712 sign | ≤ 150 µs | ≤ 500 µs | `sign` bench + runtime histogram |
| Write to socket | ≤ 20 µs | ≤ 100 µs | runtime histogram |
| **Total internal tick-to-order** | **≤ 100–250 µs** | **≤ 1 ms** | end-to-end span |
| Network RTT to the venue | minimize | — | `hl probe latency`, per region |

When a target is missed, either make it faster or update this table with a
written reason (in the same commit).

## 6. Non-goals

- Ethereum mainnet MEV (retired; see `legacy/`).
- Sandwiching or anything that harms retail users by front-running their orders.
- Polymarket (parked, SPEC-0007).
- Building a strategy because it is interesting rather than because data says it pays.

## 7. Roadmap

Spec numbers are **stable identifiers, not an order**. This table is the order.

| # | Milestone | Spec(s) | Status | Depends on |
|---|---|---|---|---|
| M0 | Platform: config, modes, observability, CI | SPEC-0000 | ✅ done | — |
| M1 | HyperCore market data client, local state, benchmark | SPEC-0001, ADR-0001 | ✅ done | M0 |
| M2 | Execution: signing, nonce, order builder, transports, dead-man switch | SPEC-0002 | ✅ done (testnet round-trip open) | M1 |
| M2.5 | **Execution hardening + latency baseline**: concurrent WS post, account stream, mandatory `cloid`, dead-man policy, stream watchdog, tick-to-order instrumentation, sign/submit benchmarks | SPEC-0002 §17 | 🔄 H-1…H-4, H-9 ✅; H-5…H-8 open; H-10 (testnet round-trip) open | M2 |
| **M3** | **Market-data recorder + opportunity research → ADR-0002** | **SPEC-0008** | ⏳ **now** (runs in parallel with M2.5) | M1 |
| M4 | **Event-driven engine** + risk hardening + multi-leg execution; **funding-carry pilot** at small size | SPEC-0010, SPEC-0004 §16, SPEC-0011, SPEC-0003 (A) | 🔄 v2 event engine runs `hl` (E-13); E-6/E-7/E-8 remainders, K-tasks, SPEC-0011 open | M2.5, M3 recorder (for replay) |
| M3.5 | **Own non-validator node in Tokyo**: fastest data, local EVM RPC, richer data (fills, order statuses, L4 book) | SPEC-0009 | later (starts when its §2 triggers fire) | M3 recorder in production |
| M5 | **First strategy**, the one chosen by ADR-0002 (arb, or the options-informed family if it ranks higher) | new spec, next free number (SPEC-0010+), written after ADR-0002 | planned | M3 gate, M4 |
| M6 | Market-making | SPEC-0003 (B) | only if research supports it | M4 |
| M7 | HyperEVM sources + executor | SPEC-0005 | only if studies O4/O8 pass | M3 gate, M4, M3.5 (EVM RPC) |
| M8 | Production ops: deploy, alerting, backups | SPEC-0006 | runs alongside M4+ | — |

**Why M3 comes before the funding strategy:** recorded data builds up over
calendar time. Every day the recorder isn't running is a day of evidence lost.
The recorder depends only on M1 (market data), so it can start immediately.

## 8. What to work on right now

Follow the tiers (§2.1):

0. **T0 fix-first (§2.2), before anything else:** the four defects added in
   the post-E-13 review (rows 8–11), then SPEC-0002 H-10 (testnet round-trip).
   No testnet or live run until every §2.2 item is ✅.
1. **T1: SPEC-0008 recorder** (R-4…R-7, then V-4/V-7, R-10) deployed to a
   low-latency host, **and SPEC-0002 §17** remainder (H-5…H-8), in parallel.
   The recorder is the evidence pipeline and the clock for gate G1 (≥ 14 days
   of data).
2. **T3-data, small and early:** V-9, V-10, V-13, R-11, R-12. Options
   history needs calendar time, so start its clock now.
3. **T1 engine:** SPEC-0010 E-6/E-7/E-8 remainders, then SPEC-0011 L-tasks and
   SPEC-0004 K-tasks. SPEC-0000 C-tasks are small and can run any time.
4. **T1 research:** P-1…P-5, then studies O1, O2, O3, O5, O8 (desk), O10
   A+D, O11 A.
5. **T2** studies, then **T3** research once its gate opens. Everything ends
   in ADR-0002.

## 9. Decision gates

| Gate | Question | Evidence required | Recorded in |
|---|---|---|---|
| G1 (end of M3) | Which strategy do we build first, or none? | SPEC-0008 study reports + `RANKING.md` covering ≥ 14 days of data | ADR-0002 |
| G1.5 (optional pilot) | Run a MARGINAL / promising-but-unproven strategy with real money to learn? | Owner prior + study report; capital ≤ pilot cap, ≤ 4 weeks, hard loss limit (SPEC-0008 §13.6); G2 passed first | ADR entry |
| G2 (before any `live`) | Is the stack safe **and fast** with real money? | **Every T0 fix (§2.2) ✅**, testnet round-trip, kill-switch drill, risk property tests, pilot in `simulate`, measured tick-to-order within the §5.2 budget | SPEC-0004/0006 checklists |
| G3 (before scaling size) | Does realized edge match researched edge? | ≥ 7 days live at small size; realized vs expected within tolerance | Strategy spec |

## 10. Glossary

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
| HIP-3 stock perp | A perp on a builder-deployed dex that tracks a stock or index (e.g. `xyz:TSLA`). Trades 24/7; the real stock doesn't. |
| Options positioning | What the options market is betting on: put/call volume and OI balance, skew, strikes with large OI ("walls"), implied volatility. |
| finsnap | The owner's separate dashboard project (`../finsnap`): a **view**. Its options formulas and past snapshots are reused here; it is never a runtime dependency. |
| Tier (T1/T2/T3) | Work priority: T1 arb core first; T3 directional family gated (§2.1). |
| Non-validator node | Our own copy of the Hyperliquid chain that follows the network without validating (SPEC-0009). |
| PASS / MARGINAL / FAIL | Study verdicts against the APR target/floor and quality checks (SPEC-0008 §13.6). |
