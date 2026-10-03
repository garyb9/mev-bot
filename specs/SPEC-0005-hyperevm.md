# SPEC-0005 — HyperEVM DEX Sources & Executor

**Status:** Draft (deferred — designed now, built after HyperCore)
**Depends on:** SPEC-0000, SPEC-0001, SPEC-0002, SPEC-0004
**Blocks:** nothing in v1

## 1. Purpose

Scope the second venue: DEX arbitrage on **HyperEVM** (chain 999). This spec exists so the platform is shaped for it from the start (shared Alloy stack, shared risk engine, shared simulation), while the HyperCore path ships first and is unaffected.

## 2. Goals

- Reuse the existing Alloy/revm/Rust stack — no new language or framework.
- Discover and track HyperEVM DEX pools; price routes offline; simulate atomically.
- An on-chain executor with hard safety guards (slippage, min profit, pause).
- Optional flashloan module (own-capital first).
- Full integration with SPEC-0004 risk and SPEC-0002-style submission.

## 3. Non-goals

- Replacing or changing the HyperCore execution path.
- Cross-chain bridging strategies beyond the native HyperCore↔HyperEVM transfer.
- Being live in v1.

## 4. HyperEVM context (facts to design around)

- **Chain IDs:** 999 (mainnet), 998 (testnet). Gas paid in **HYPE** (unlike HyperCore, which is gas-free).
- **Blocks:** a fast block roughly every **1s** (2M gas) and a large block roughly every **1min** (30M gas).
- **Read precompiles** (from `0x…0800`) expose HyperCore state (perp positions, spot balances, oracle prices, L1 block number) and are guaranteed consistent with HyperCore at block construction — usable as an oracle without external price feeds.
- **CoreWriter** system contract (`0x3333…`) sends actions from HyperEVM to HyperCore. Actions sent this way are **deliberately delayed a few seconds** on-chain — so it is *not* a low-latency execution path. Low-latency action stays on `/exchange` (SPEC-0002).
- Reference interfaces: `L1Read.sol` (reads) and `CoreWriter.sol` (writes).

## 5. Architecture

- New crate `hl-arb-hyperevm`: Alloy provider, pool sources, state cache, revm fork simulator, executor bindings.
- **Sources** implement a common trait (factory discovery → pool registry → state updates via logs/multicall), mirroring the SPEC-0001 pattern for consistency.
- **Simulation** uses a local **revm** fork of HyperEVM state as a pre-submit gate (bit-exact), with `eth_call` + state override as an alternative.
- **Submission** goes through the risk engine (SPEC-0004) and a submitter (private bundle/relay if available, else the mempool).

## 6. Data & pool discovery

- Discover pools from factory events (`PairCreated`/`PoolCreated`) at startup; keep a registry updated from new blocks/logs.
- Seed state with batched Multicall3 reads; update incrementally from `Sync`/swap logs.
- Reorg handling is mandatory given fast blocks: keep a short reorg buffer and re-derive affected pools.
- Protocol coverage (v1 of this spec): Uniswap-V2-style constant product first; V3 concentrated liquidity and Curve-style stables are follow-ups.
- DEX/protocol addresses are configuration, not hardcoded (chain 999 list to be confirmed).

## 7. Strategy

- **Cyclic/triangular** path search across pools (graph, negative-cycle detection), with optimal input sizing (closed form for V2; ternary search for nonlinear curves).
- Edge is **net of gas (HYPE)**, pool fees, and slippage estimated by walking/quoting actual pool state.
- Opportunities below the net-edge buffer are discarded, consistent with SPEC-0003.

## 8. Execution & executor contract

- Solidity executor (Foundry project, `contracts/`), design principles:
  - **Own-capital first**: atomic round-trip funded by the caller; a flashloan variant is optional and later.
  - Guards: `slippageBps`/`minProfit` per route, `Ownable2Step`, `Pausable`, `ReentrancyGuard`, SafeERC20.
  - Profit paid to the owner; emits a `TradeExecuted` event for accounting.
  - No admin functions that can move user funds unexpectedly; recover functions owner-only.
- Optional flashloan adapter (e.g. HyperLend) behind the same interface, enabled only after own-capital path is proven.
- Every candidate trade is **simulated via revm** (revert → discard) before submission.

## 9. Latency & infra

- HyperEVM is EVM, so submission latency is RPC/builder-bound; use private relay/bundle access if available, else standard RPC with gas bidding.
- Because CoreWriter is delayed, any strategy that needs Core liquidity atomically is out of scope; keep Core and EVM interactions as separate, deliberate steps (including the native transfer between environments).

## 10. Safety

- Same SPEC-0004 risk engine and kill switch apply; the EVM submitter additionally checks simulation success, deadline, and min-profit.
- Fail-closed: no simulation result ⇒ no submit.
- Gas cost is part of the edge model; unprofitable-after-gas trades are rejected before signing.

## 11. Testing

- Foundry unit/fork tests for the executor (happy path, revert on min-profit, slippage, reentrancy, pause).
- revm simulation parity vs live quoters on forks.
- Offline pricing/route tests with recorded pool state.
- End-to-end dry-run on testnet (998) before any mainnet enablement.

## 12. Acceptance criteria

- Pool registry and state stay consistent with on-chain state across reorgs in replay.
- Simulated routes match on-chain results within tolerance.
- Executor reverts rather than executing a trade below `minProfit`.
- Zero impact on the HyperCore path when disabled.

## 13. Resolved decisions

1. **Deferred** until after HyperCore is live; spec'd now to shape the platform.
2. **Reuse** Alloy + revm; own-capital execution first, flashloans optional/later.
3. **CoreWriter is not a latency path**; Core execution stays on `/exchange`.

## 14. Open questions

1. Which HyperEVM DEXes/protocols to support first (V2-style only, or V3 too) and their chain-999 addresses.
2. Flashloan provider choice and whether it's worth it at our capital scale.
3. Whether HyperEVM arb is meaningfully profitable versus focusing effort on HyperCore market-making.
