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
- `approveAgent` is a **user-signed** action performed once by the **master account** to enroll the agent. Recommended: do it via the Hyperliquid UI (Settings → API), keeping the master key off this host entirely. Optional: a guided one-time CLI command for later re-approvals that reads the master key from a prompt (never persisted, never logged).
- The agent key lives on the bot host; the master key does not.
- **One agent wallet per bot instance** (nonce isolation). Multiple instances sharing an agent will emit out-of-order nonces and reject each other.

## 5. Nonce state machine

- Nonce = **millisecond timestamp**, strictly increasing per agent wallet, within a recent window. The venue validates but never assigns it; nonces need only be strictly increasing, **not contiguous** (gaps are harmless).
- A single writer owns the nonce; the value is generated as `max(now_ms, last_nonce + 1)` and persisted before send (crash-safe). Single-writer serialization is enforced by the executor's lock.
- **Self-healing:**
  - *Stale/duplicate/recent-window rejection* → resync to the wall clock (`last = max(last, now) + 1`) and retry; orders are idempotent via `cloid` (§7).
  - *Future drift* — if `last_nonce` has run more than a guard (default 60s, `max_future_drift`) ahead of the clock (corruption or a backwards clock step), the value cannot have been accepted by the venue, so it is **reclaimed** and reset to the wall clock. Reset reason is tagged in `hl_nonce_resets_total`.
  - *Manual* → an operator escape hatch force-resyncs to the wall clock.
- Clock discipline: NTP/chrony required; the machine must not run backwards. A backwards jump advances the nonce rather than reusing it.
- Unit-tested as a state machine (monotonicity, same-millisecond bursts, restart recovery, clock regression, future-drift reclaim, reject resync).

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

- Two transports behind the trait: **WebSocket post** (default; max 100 simultaneous in-flight posts, lower latency) and **REST `POST /exchange`** (fallback, simpler, stateless). The SPEC-0001 harness measures submit latency over both; the default ships as WS and can be re-decided by measurement.
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

## 12. Modes, autonomy & safety

### Modes

- `observe`: **zero** `/exchange` calls (asserted in tests).
- `simulate`: build + sign (optional) but **never submit**; responses are simulated.
- `live`: submit; gated by SPEC-0000 (`HL_LIVE_CONFIRM=YES`, keys present).

### Execution autonomy

Orthogonal to the mode above; applies only in `live`.

- `HL_AUTONOMY=auto` (**default**): the engine submits trades on its own once risk checks pass. Required for an unattended trading/MEV bot.
- `HL_AUTONOMY=confirm`: the engine emits a proposed trade and waits for explicit human approval (CLI/TTY or an approval channel) before submitting. For supervised/manual runs and debugging.
- Every decision/approval is logged with the sizing rationale; `auto` never bypasses risk limits (SPEC-0004).

### Dead-man's switch (`scheduleCancel`)

- In `live`, arm `scheduleCancel` with a TTL and refresh it on a heartbeat.
- If the process stalls, loses connectivity, or crashes, the exchange auto-cancels resting orders after the TTL — a dead bot cannot leave stale orders exposed.
- TTL is configurable (`HL_SCHEDULE_CANCEL_TTL_MS`, default 30s), refreshed with margin below the TTL, and explicitly disarmed on graceful shutdown.
- Armed status is a metric and a `/healthz` input; failure to refresh within the window raises an alert.

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

## 16. Resolved decisions

1. **Transport** — default is **WebSocket post**; REST `/exchange` remains a fallback. Both are benchmarked in SPEC-0001 and the default can be re-decided by measurement.
2. **Agent provisioning** — enroll the agent via the Hyperliquid UI so the master key stays off-host; an optional guided one-time CLI command supports later re-approvals without persisting the master key.
3. **Autonomy** — engine-driven by default (`HL_AUTONOMY=auto`); `confirm` mode exists for supervised runs.
4. **Dead-man's switch** — armed by default in `live` via `scheduleCancel`, TTL `HL_SCHEDULE_CANCEL_TTL_MS` (default 30s), refreshed on heartbeat, disarmed on graceful shutdown.
