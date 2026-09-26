# SPEC-0007 — Polymarket (Parked)

**Status:** Parked (do not implement)
**Depends on:** SPEC-0000, SPEC-0004
**Blocks:** nothing

## 1. Purpose

Record Polymarket as a considered-but-deferred source, with enough context that we can pick it up cleanly later. It is deliberately **not** part of the current build.

## 2. Why deferred

Polymarket is a fundamentally different venue from the HyperCore-first plan:

- **Chain:** Polygon (not Hyperliquid), so a separate provider, collateral (USDC), and submission path.
- **Market type:** binary prediction markets, not perps/spot order books; a different pricing, inventory, and risk model.
- **Architecture:** off-chain CLOB with EIP-712-signed orders and on-chain settlement; different from HyperCore's agent-wallet L1 actions.
- **Edge:** mostly complementary-outcome arbitrage (`YES + NO < 1`), long-tail and capacity-limited.

It shares little with the HyperCore stack, so mixing it into v1 would dilute focus for unclear gain.

## 3. Rough architecture (if unparked)

- New crate `mev-polymarket`: Gamma market reads, CLOB REST + WebSocket, EIP-712 order signing (Polygon), collateral/allowance handling.
- Strategy plug-in (SPEC-0003 trait): complement arb and cross-market consistency.
- Risk via the shared SPEC-0004 engine (separate account/limits).
- Persistence via the shared SQLite store (SPEC-0004).
- Rust crates exist in the ecosystem (e.g. `polymarket`, `polymarket-hft`) if a client path is preferred over custom.

## 4. Decision criteria to unpark

All of:

1. HyperCore funding/basis and market-making are live, stable, and profitable.
2. Spare engineering capacity and capital that isn't better spent deepening HyperCore/HyperEVM.
3. A data-backed edge validated (complement mispricing frequency vs fees/latency).

## 5. Open questions

1. Is the edge durable after fees/gas and against faster participants?
2. Custom client vs existing Rust crate (and its maintenance/dep hygiene).
3. Whether it warrants its own risk budget or a separate process entirely.
