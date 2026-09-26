# SPEC-0002 — Execution, Signing & Account

**Status:** Draft
**Depends on:** SPEC-0000, SPEC-0001
**Blocks:** SPEC-0003 (strategy), SPEC-0004 (risk)

## 1. Purpose

Define how the system signs and submits HyperCore actions: the agent-wallet EIP-712 signing model, the nonce state machine, the order builder with tick/lot rounding, the action catalog, the exchange client trait/transports, response handling, and account-state reconciliation. This spec owns all *writes* to Hyperliquid.

## 2. Goals

- Correct, deterministic EIP-712 signing for L1 actions and user-signed actions.
- A nonce state machine that cannot reuse or regress a nonce per agent wallet.
- An order builder that respects per-market tick/lot/notional rules so requests don't silently fail.
- Idempotent, observable submission with clear rejection handling.
- No master/withdraw key on the host; agent wallet only.

## 3. Non-goals

- Market data/streams (SPEC-0001).
- Strategy/expected-value math (SPEC-0003).
- Portfolio-level risk limits and accounting (SPEC-0004).

## 4. Signing model

Hyperliquid has two signing flows. Both send `{ action, nonce, signature: { r, s, v } }` (plus optional `vaultAddress` / `expiresAfter`) to `POST /exchange`; `v` is 27/28.

### 4.1 L1 actions (trading) — phantom agent

Used for orders, cancels, modifies, leverage/margin updates.

1. Serialize the `action` to **msgpack**.
2. Keccak-256 hash it; fold in `nonce`, `vaultAddress` (if any), and `expiresAfter` (if any) → **connectionId**.
3. Construct a phantom `Agent` struct `{ source, connectionId }` where `source` = `"a"` (mainnet) or `"b"` (testnet).
4. EIP-712 sign with domain `{ name: "Exchange", version: "1", chainId: 1337, verifyingContract: 0x0 }`, primary type `Agent`.
5. Send the original `action` + signature + nonce. Validators recover the signer, then verify `connectionId` matches the action hash.

Notes: `chainId 1337` is hardcoded and independent of the wallet's network. A valid-looking signature rejected is almost always a chain-id or nonce mismatch.

### 4.2 User-signed actions

Used for account/security and fund movements (`approveAgent`, `approveBuilderFee`, `usdClassTransfer`, `withdraw`, transfers, staking). The action fields are placed directly into typed data — no phantom agent / no action hash.

- Domain `{ name: "HyperliquidSignTransaction", version: "1", chainId: <signatureChainId>, verifyingContract: 0x0 }`.
- `signatureChainId` is carried in the action (hex, e.g. Arbitrum `0x66eee`).

### 4.3 Agent wallet lifecycle

- The UI calls it an "API wallet"; it's a separate keypair authorized by the master account and **cannot withdraw**.
- `approveAgent` is a **user-signed** action performed once (by the master key, off this host or via a documented one-time flow).
- The agent key lives on the bot host; the master key does not.
- **One agent wallet per bot instance** (nonce isolation). Multiple instances sharing an agent will emit out-of-order nonces and reject each other.

## 5. Nonce state machine

- Nonce = **millisecond timestamp**, strictly increasing per agent wallet, within a recent window.
- A single writer owns the nonce; the value is generated as `max(now_ms, last_nonce + 1)` and persisted before send (crash-safe).
- On rejection indicating a stale/duplicate/recent-window violation: re-read time, advance past `last_nonce`, and retry the action (orders are idempotent via `cloid`, §7).
- Clock discipline: NTP/chrony required; the machine must not run backwards. A backwards jump is detected and the nonce is advanced rather than reused.
- Unit-tested as a state machine (monotonicity, crash/restart recovery, concurrent-send serialization).

## 6. Order model & rounding

Orders reference assets by **numeric index**, not ticker (resolved via SPEC-0001 `AssetMap`).

- Side: `A` (ask/sell) / `B` (bid/buy); `reduceOnly` flag.
- Time-in-force: `Alo` (post-only/add-liquidity), `Gtc`, `Ioc`; trigger orders with `triggerPx`, `isMarket`, `tpsl`.
- `cloid`: optional 16-byte hex client order id; used for idempotency and lookup.
- **Rounding is mandatory** and derived from metadata:
  - Prices to the market's tick (significant-figure rules) and sizes to `szDecimals`.
  - Enforce minimum notional and max leverage; reject locally with a precise error before sending.
  - Property tests assert round-trips never exceed limits and always produce valid tick/lot values.

## 7. Action catalog

| Action | Signing | Purpose |
|---|---|---|
| `order` (single/bulk) | L1 | place orders; `grouping` na/normalTpsl/positionTpsl |
| `cancel` / `cancelByCloid` | L1 | cancel by oid or cloid |
| `modify` / `batchModify` | L1 | amend price/size |
| `scheduleCancel` | L1 | dead-man's switch (auto-cancel after time) |
| `updateLeverage` | L1 | set cross/isolated leverage |
| `updateIsolatedMargin` | L1 | adjust isolated margin |
| `usdClassTransfer` | user | move USDC spot ↔ perp |
| `approveAgent` | user | authorize the agent wallet (one-time) |
| `approveBuilderFee` | user | optional builder fee approval |

Exact field names per action are confirmed against current docs during implementation; wire types are golden-fixture tested.

## 8. Exchange client & transports

`ExchangeApi` trait in `mev-hl-client` (sibling to `InfoApi`):

```rust
#[async_trait]
pub trait ExchangeApi {
    async fn place(&self, req: OrderRequest) -> Result<OrderResponse>;
    async fn cancel(&self, req: CancelRequest) -> Result<ActionResponse>;
    async fn modify(&self, req: ModifyRequest) -> Result<ActionResponse>;
    async fn schedule_cancel(&self, at_ms: u64) -> Result<ActionResponse>;
    async fn update_leverage(&self, asset: u32, cross: bool, lev: u32) -> Result<ActionResponse>;
}
```

- Two transports behind the trait: **REST `POST /exchange`** (default, simple) and **WebSocket post** (max 100 in-flight; lower latency). The SPEC-0001 harness also measures submit latency over both; default chosen by measurement.
- Only SPEC-0002 code may hold the agent signer; it's injected at construction and never logged.

## 9. Response handling & errors

- Exchange responses carry a status; order results include `resting` / `filled` / `error` with per-order status strings (`badAloPxRejected`, `iocCancelRejected`, `perpMarginRejected`, `minTradeNtlRejected`, `tickRejected`, `reduceOnlyRejected`, `oracleRejected`, …). These are mapped to typed errors, never string-matched ad hoc.
- **Never blind-retry an order** — retries are idempotent via `cloid`; a retry of the same logical order reuses the cloid or is suppressed.
- Cancels are cheap and more rate-lenient; reconcile unknown outcomes via `orderStatus`.
- All rejects are metric-tagged by status for later analysis.

## 10. Rate & address limits

- Respect IP weight (1200/min, §SPEC-0001) and the **address-based** limit: ~1 request per 1 USDC traded, initial 10,000-request buffer, then 1 request/10s when limited; cancels get a larger allowance.
- A batched order of `n` counts as `1` for IP but `n` for the address budget.
- Open-order limit: default 1000 + 1 per 5M USDC volume (cap 5000); the builder avoids exceeding it.

## 11. Account state

Reads (via `InfoApi`): `clearinghouseState` (positions/margin), `spotClearinghouseState`, `openOrders`, `userFills`, `orderStatus`, `userRateLimit`, `userFees` (effective fee rates feed SPEC-0003's EV model).

- Reconcile on startup and periodically; the executor never assumes a fill without either a stream `orderUpdates`/`userFills` event or an `orderStatus` check.
- Open orders and positions are the source of truth for risk (SPEC-0004).

## 12. Modes & safety

- `observe`: **zero** `/exchange` calls (asserted in tests).
- `simulate`: build + sign (optional) but **never submit**; responses are simulated.
- `live`: submit; gated by SPEC-0000 (`HL_LIVE_CONFIRM=YES`, keys present).
- Optional `scheduleCancel` dead-man's switch is armed in `live` so orders auto-cancel if the bot dies.

## 13. Observability

Metrics: submit latency histogram (by transport), order rejects by status, nonce errors, open-order count, fill rate, cancel latency, and slippage vs intent. A nonce error or an unexpected reject rate is an alert.

## 14. Testing

- EIP-712 known-vector tests: sign → recover the agent address; both L1 and user-signed flows.
- msgpack golden vectors for representative actions (hash stability).
- Rounding property tests (tick/lot/notional/leverage).
- Nonce state-machine tests (monotonic, restart, clock regression).
- `wiremock` for `/exchange` (success, resting, each reject class, 429).
- **Testnet round-trip** (once keys exist): place a far-from-mid post-only order, confirm it appears in `openOrders`, cancel it. Exercises signing, nonce, and rounding together.

## 15. Acceptance criteria

- Signatures recover to the configured agent address on mainnet and testnet.
- Testnet order round-trip succeeds end-to-end; no nonce reuse observed.
- Local rounding rejects invalid orders before any network call.
- `observe` mode makes zero `/exchange` requests.
- Sub-10 ms p50 (local) for build+sign of a single order on the reference machine (target, measured in SPEC-0001 harness).

## 16. Open questions

1. Default transport: REST vs WS post (decided by the SPEC-0001 submit-latency benchmark).
2. Agent wallet provisioning flow to document (one-time `approveAgent` off-host vs a guided command).
3. Whether to arm `scheduleCancel` by default in `live`.
