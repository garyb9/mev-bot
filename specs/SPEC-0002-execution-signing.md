# SPEC-0002 — Execution, Signing & Account

**Status:** Implemented (M2) — open item: live testnet round-trip (gated on a funded, agent-approved testnet account). M2.5 hardening follow-ups: §17.
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
- **Verified 2026-09-26** ([Nonces and API wallets](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/nonces-and-api-wallets)): the venue stores the **100 highest nonces per signer** (the agent address for an API wallet). A new nonce must be (1) larger than the **smallest** nonce in that set, (2) never used before, and (3) within `(T − 2 days, T + 1 day)` of the block timestamp `T`. This supersedes the earlier vague "recent window"; the manager's default future-drift guard (60 s) is intentionally stricter than the venue's `T + 1 day`.
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
- Cancels are cheap and more rate-lenient; reconcile unknown outcomes via `orderStatus`. **Verified 2026-09-26** ([Info endpoint](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/info-endpoint)): `orderStatus.oid` accepts **either** a `u64` order id **or** a 16-byte hex `cloid` string, so an unknown order can be resolved by its client id without ever resending it (H-2). The response is nested — `{"status":"order","order":{"order":{…},"status":<state>,"statusTimestamp":…}}` for a hit, `{"status":"unknownOid"}` for a miss — and the inner `status` is the state (`open`/`filled`/`canceled`/rejected classes). The current `OrderStatusResponse` in `mev-hl-client/src/types.rs` models the flat shape and must be corrected for H-2 (see §18).
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
- **Verified 2026-09-26** ([Exchange endpoint](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/exchange-endpoint)): the scheduled `time` must be **at least 5 s** in the future; omitting `time` removes the schedule; the trigger count increments only when the scheduled time arrives and cancels all open orders, with a **max of 10 triggers/day**, reset at **00:00 UTC**. Refreshing before the deadline re-arms without incrementing the trigger count, so a live heartbeat is not what burns the daily limit — but every arm/refresh still spends **address rate-limit budget** (H-4 arms only while orders rest for this reason).

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

- [x] Signatures recover to the configured agent address on mainnet and testnet (golden vectors + official-SDK recovery).
- [ ] **OPEN** — Testnet order round-trip succeeds end-to-end; no nonce reuse observed. Blocked on a funded, agent-approved testnet account; the path is wired and mock-tested only.
- [x] Local rounding rejects invalid orders before any network call.
- [x] `observe` mode makes zero `/exchange` requests.
- [ ] Build + msgpack + sign of a single order within the [`docs/GOAL.md`](../docs/GOAL.md) §5.2 budget (p50 ≤ 150 µs, p99 ≤ 500 µs) on the reference machine (task H-7). *(Supersedes the original 10 ms target.)*

## 16. Resolved decisions

1. **Transport** — default is **WebSocket post**; REST `/exchange` remains a fallback. Both are benchmarked in SPEC-0001 and the default can be re-decided by measurement.
2. **Agent provisioning** — enroll the agent via the Hyperliquid UI so the master key stays off-host; an optional guided one-time CLI command supports later re-approvals without persisting the master key.
3. **Autonomy** — engine-driven by default (`HL_AUTONOMY=auto`); `confirm` mode exists for supervised runs.
4. **Dead-man's switch** — armed by default in `live` via `scheduleCancel`, TTL `HL_SCHEDULE_CANCEL_TTL_MS` (default 30s), refreshed on heartbeat, disarmed on graceful shutdown.

## 17. Follow-ups — M2.5 execution hardening + latency baseline

Found in the post-M2 review (2026-09-26). **H-1, H-2, H-4, and H-9 are T0 fix-first** ([`docs/GOAL.md`](../docs/GOAL.md) §2.2): do them before any other work. All of these must land before any strategy trades (M4), and they sit on the latency hot path, so [`docs/GOAL.md`](../docs/GOAL.md) §5 applies to every one. Format and rules match SPEC-0008 §0 and §14: do the tasks in dependency order, tick the status in the same commit as the code, and write ambiguities into §18 instead of guessing.

| ID | Title | Tier | Size | Depends on | Status |
|---|---|---|---|---|---|
| H-1 | Concurrent WS `post` (reader task + pending map) | **T0** | M | — | ✅ |
| H-2 | Mandatory `cloid` + unknown-outcome reconciliation | **T0** | M | H-1 | ☐ |
| H-3 | Account stream (`orderUpdates`, `userFills`, `userEvents`) | T1 | M | SPEC-0008 R-3 | ☐ |
| H-4 | Dead-man's switch policy (arm only when needed; fail closed) | **T0** | S | H-1 | ☐ |
| H-5 | Apply `bbo` to `MarketState` | T1 | S | — | ☐ |
| H-6 | Nonce persistence off the hot path | T1 | S | H-9 | ☐ |
| H-7 | Latency instrumentation + sign/submit benchmarks | T1 | M | H-1 | ☐ |
| H-8 | `simulate` without keys (ephemeral signer) | T1 | S | — | ☐ |
| H-9 | Verify the HL nonce and `scheduleCancel` rules | **T0** | S | — | ✅ |
| H-10 | Testnet round-trip (the open §15 item) | T1 | S | **all T0 fixes** (GOAL §2.2), owner-provided testnet key | ☐ |

**H-1 — Concurrent WS `post`.** Today `WsExchange::post` holds the socket mutex while it waits for the reply, so only one request is ever in flight. Replace this with: one writer handle (a channel into a socket-owning task); one reader task that routes `channel:"post"` replies by `id` to a `HashMap<u64, oneshot::Sender>`; a semaphore capping in-flight posts at 100 (the venue limit); a per-request timeout (default 5 s) that returns a typed `Error::UnknownOutcome`. On socket loss, fail every pending request with `UnknownOutcome`, reconnect (reuse `RawWsConn` from SPEC-0008 R-3 once it exists), and keep the connection warm with app-level pings. *Done when:* a mock-venue test sends 50 concurrent orders with shuffled reply order and each caller gets its own reply; a dropped socket fails pending calls with `UnknownOutcome`; the dead-man refresh no longer blocks order sends.

**H-2 — Mandatory `cloid` + reconciliation.** Every order gets a `cloid` (generate a 16-byte id from a per-process random prefix + counter when the caller doesn't supply one). On `UnknownOutcome`, query `orderStatus` by `cloid` (⚠ verify that `orderStatus` accepts a cloid; H-9) and resolve to resting/filled/rejected/not-found. **Never** resend an order without doing this first. *Done when:* `wiremock`/mock-WS tests cover each resolution, and there is no code path that retries an order blindly.

**H-3 — Account stream.** Add `Subscription::{OrderUpdates, UserFills, UserEvents}{user}` and typed `StreamEvent` variants. Handle the `isSnapshot` first message of `userFills`. These go on a **separate, lossless** channel from market data (SPEC-0001 §8), and on reconnect the snapshot resyncs state. *Done when:* golden-fixture decode tests pass for every channel (fixtures captured from testnet or the docs), and a reconnect test shows the snapshot applied.

**H-4 — Dead-man's switch policy.** (a) Arm only while at least one resting order exists, and disarm when none remain; each `scheduleCancel` spends address rate-limit budget (a 30 s TTL refreshed every 15 s is ~5.8k requests/day against a 10k + 1-per-USDC-traded budget). (b) Default TTL 120 s, refreshed at half the TTL. (c) If arming or refreshing fails, set a sticky **trading-halt** flag that the risk engine reads (fail closed); today the task logs and returns while live continues. (d) Expose remaining address budget by polling `userRateLimit` every 60 s as a metric. *Done when:* unit tests cover arm/disarm on the resting-order count and halt on failure.

**H-5 — `bbo` into `MarketState`.** `MarketState::apply` currently ignores `StreamEvent::Bbo`. Store the latest BBO per coin with its receive time, and expose `best_bid/ask` that prefer the fresher of `bbo` and the `l2Book` top. Subscribe `bbo` for watchlist coins in `ingest`. *Done when:* tests show that the fresher source wins.

**H-6 — Nonce persistence off the hot path.** `WriteCore::prepare` currently writes the nonce to SQLite synchronously before every send. Once H-9 confirms the nonce rule, move persistence to write-behind (via `DbWriter`, at most once per second), relying on `max(now_ms, restored + 1)` + the future-drift guard for crash safety. Document the argument in §5. *Done when:* a restart test (kill before the write-behind flush) still never produces a nonce ≤ any previously sent nonce under a monotonic clock; H-7 shows the latency drop.

**H-7 — Latency instrumentation + benchmarks.** Add histograms for each [`docs/GOAL.md`](../docs/GOAL.md) §5.2 stage (`hl_decode_seconds`, `hl_sign_seconds`, `hl_submit_ack_seconds{transport}`, `hl_tick_to_order_seconds`) using a cheap monotonic clock. Add a criterion bench `benches/sign.rs` (build + msgpack + EIP-712 sign of one order and a batch of 10) and a mock-WS `post` round-trip bench. Record results in ADR-0001's "Not yet measured" section. *Done when:* the benches run in CI quick mode and the numbers are recorded.

**H-8 — `simulate` without keys.** SPEC-0000 §6 says `simulate` needs no keys, but `WriteCore::new` rejects `DryRun` without a signer. In `simulate` with no key, generate an ephemeral random signer (never persisted, logged as such). *Done when:* `hl run --mode simulate` starts with no key, and a test covers it.

**H-9 — Verify HL rules.** From the official docs, confirm: (1) the nonce rule (believed: the venue keeps the 100 highest nonces per signer; a new nonce must be above the smallest of them, unused, and within roughly (now − 2 days, now + 1 day)); (2) the `scheduleCancel` rules (minimum lead time; daily trigger limit; any eligibility requirement); (3) whether `orderStatus` accepts a `cloid`. Record each answer with its source in §5/§12, and fix any spec text that is wrong. *Done when:* each fact has a source link and date.

**H-10 — Testnet round-trip.** Owner-run, or run by an agent only with an owner-provided **testnet** key (never a mainnet key). Place a far-from-mid ALO order, see it in `openOrders` and in `orderUpdates` (H-3), cancel it by `cloid`, and record submit→ack latency (H-7). Closes the open §15 item.

## 18. Open questions (M2.5)

1. H-4 default TTL (120 s) vs strategy needs; market-making may want a shorter TTL with more budget.
2. `OrderStatusResponse` (`mev-hl-client/src/types.rs`) models a flat `{"status":"order","order":{OpenOrder}}` shape, but the venue returns a nested `{"status":"order","order":{"order":{…},"status":<state>,"statusTimestamp":…}}` (verified 2026-09-26, see §9). H-2 must correct the type and its `is_filled`/resolution logic before using it for reconciliation.
