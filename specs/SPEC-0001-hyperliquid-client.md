# SPEC-0001 — Hyperliquid Client & Market Data

**Status:** Draft
**Depends on:** SPEC-0000
**Blocks:** SPEC-0002 (execution), SPEC-0003 (strategy)

## 1. Purpose

Define the HyperCore connectivity layer: REST `/info` client, WebSocket connection manager, market-data model, subscription strategy, reconnection/reconciliation, and the backend-selection benchmark. This spec owns *reading* Hyperliquid; order submission/signing lives in SPEC-0002 behind a sibling trait.

## 2. Goals

- Lowest practical latency for real-time data via WebSocket, with a resilient reconnect path.
- A correct, allocation-conscious local market state (books, mids, asset context, trades).
- Rate-limit compliance by construction (budgeted REST, no polling loops).
- A backend-swappable client trait so the implementation can be chosen by measurement.
- No deprecated dependencies (no `ethers`).

## 3. Non-goals

- Signing and order placement (SPEC-0002).
- Strategy/expected-value math (SPEC-0003).
- HyperEVM data (SPEC-0005).

## 4. Endpoints

| Network | REST | WebSocket |
|---|---|---|
| Mainnet | `https://api.hyperliquid.xyz/info`, `/exchange` | `wss://api.hyperliquid.xyz/ws` |
| Testnet | `https://api.hyperliquid-testnet.xyz/info`, `/exchange` | `wss://api.hyperliquid-testnet.xyz/ws` |

All REST is `POST` with a JSON body carrying a `type` field. `/info` is unauthenticated; `/exchange` requires signing (SPEC-0002).

## 5. Rate limits & budgeting

Per IP (from official docs):

- **REST:** shared aggregate of **1200 weight/minute**.
  - `exchange`: `1 + floor(batch_length / 40)`.
  - `info` weight **2**: `l2Book`, `allMids`, `clearinghouseState`, `orderStatus`, `spotClearinghouseState`, `exchangeStatus`.
  - `info` weight **60**: `userRole`.
  - `info` weight **20**: everything else documented.
  - Per-item surcharge for paginated reads: +20 per 20 items (`recentTrades`, `userFills*`, `fundingHistory`, `userFunding`, `historicalOrders`, `twap*`, …); `candleSnapshot` +20 per 60 items.
- **WebSocket:** max **10 connections**, max **30 new connections/min**, max **1000 subscriptions**, max **10 unique users** across user-specific subs, max **2000 outbound messages/min** across all connections, max **100 simultaneous in-flight post messages**.
- **Public EVM RPC** `rpc.hyperliquid.xyz/evm`: 100 req/min (use a commercial provider for EVM work later).

**Budget enforcement:** a token-bucket per endpoint class in the REST client; requests are rejected/queued rather than sent over budget. Market data is **stream-first**; REST is only used for metadata and reconciliation.

## 6. Market metadata

- Fetch `meta` (perps) and `spotMeta` (spot) at startup and on a low-frequency refresh (e.g. hourly / on reconnect).
- Build an `AssetMap`: `coin` ↔ numeric asset index, `szDecimals`, `maxLeverage`, tick size, and spot pair index (`@{index}` / `PURR/USDC` conventions).
- Respect the coin naming split: perps by name (`BTC`), HIP-3 by `dex:coin` (`xyz:XYZ100`), spot by `@index` or `PURR/USDC`.
- Rounding of prices/sizes to tick/lot is enforced in SPEC-0002 (the order builder), using this metadata.

### Market selection, watchlist & CLI

- Default watchlist = a small set of bluechips: perps `BTC`, `ETH`, `SOL` (extensible). Spot legs are resolved to their L1 names at runtime (e.g. mainnet `UBTC/USDC` for BTC) — never hardcoded.
- The watchlist is a first-class, persisted config object, overridable per run via CLI. Adding markets later is data, not code.
- A `MarketSelector` validates each coin against `meta`/`spotMeta` (name, index, `szDecimals`) before any subscription is created; unknown/remapped coins fail fast with a clear error.
- CLI subcommands (read commands require no keys):

| Command | Purpose |
|---|---|
| `markets [--perp\|--spot] [--search <q>]` | list/search markets from `meta`/`spotMeta` |
| `book <coin> [--levels N]` | live top-of-book / mid snapshot |
| `watch <coin...>` | stream book/bbo/trades/ctx for selected coins |
| `select <coin...>` / `select --add\|--remove` | edit and persist the watchlist |
| `run [--mode observe\|simulate\|live] [--coins ...]` | launch the bot with the watchlist (CLI override optional) |
| `config show` | print resolved config with secrets redacted |

`run` defaults to `observe`. This delivers the "pick pairs → search → select → launch" flow.

## 7. Subscriptions & local state

Streams consumed (subset per active strategy):

| Channel | Use | Notes |
|---|---|---|
| `allMids` | fast mid for all coins | weight 2 if via REST; prefer WS |
| `l2Book` | full book snapshots | pushed per block, min ~0.5s cadence; levels = `[bids, asks]`, `{px,sz,n}` |
| `bbo` | best bid/offer | sent only when BBO changes on a block |
| `trades` | tape / last price | array per message |
| `activeAssetCtx` | mark/mid/funding/OI per coin | drives funding/basis strategy |
| `fastAssetCtxs` | fast markPx/midPx | base64 + raw-DEFLATE (RFC 1951, `wbits=-15`), JSON |
| `candles` | interval candles | strategy warmup |
| `userFills` / `userEvents` / `orderUpdates` | account/execution state | SPEC-0002/0004, requires user |

**Local state model:**
- `OrderBook { bids: BTreeMap<Decimal, Level>, asks: BTreeMap<Decimal, Level>, time }`, cached best bid/ask/mid/spread.
- `BookStore` keyed by coin, with a monotonic `updated_at` and a `stale` flag.
- Trades ring buffer (bounded) for recent prints.
- `AssetCtx` map (mark, mid, funding, OI) with timestamps.
- `l2Book` is a full snapshot per message (not deltas), so replacement is safe; `bbo` updates are applied incrementally.
- **Staleness:** each feed tracks last-update time; `/readyz` requires all subscribed feeds fresh within their tolerance (e.g. l2Book < 2s).

## 8. Connection manager

- One supervisor owning a pool of ≤10 WS connections; subscriptions are distributed to stay within limits and to isolate critical feeds.
- **Reconnect:** on close/error, exponential backoff with jitter; on reconnect, re-subscribe and treat the first message per channel as the authoritative snapshot. Missed data is recovered from the snapshot ack and, where required, a targeted `/info` reconciliation.
- **Heartbeat:** send an application-level `ping` every **30 s** and expect a `pong`; a missed pong triggers reconnect. Additionally, a **data watchdog** reconnects if a subscribed feed receives nothing for its tolerance window. Confirm the server's exact idle-close window against a live connection and record it in the decision log.
- **Backpressure:** decode on the I/O task but publish through a bounded channel; if a consumer lags, drop the oldest market-data message (never block the socket). Account/execution feeds are lossless and use a separate channel.
- No `unwrap` on socket/parse paths; malformed frames are counted and skipped, not fatal.

## 9. Client abstraction (backend-swappable)

Traits in `mev-hl-client`:

```rust
#[async_trait]
pub trait InfoApi {
    async fn meta(&self) -> Result<Meta>;
    async fn spot_meta(&self) -> Result<SpotMeta>;
    async fn all_mids(&self) -> Result<Mids>;
    async fn l2_book(&self, coin: &Coin) -> Result<BookSnapshot>;
    async fn asset_ctx(&self, coin: &Coin) -> Result<AssetCtx>;
    // account reads (openOrders, clearinghouseState, userFills, ...)
}

#[async_trait]
pub trait MarketStream {
    async fn subscribe(&mut self, subs: &[Subscription]) -> Result<()>;
    async fn next(&mut self) -> Result<StreamEvent>;
}
```

`StreamEvent` is a typed enum (`Book`, `Bbo`, `Trade`, `AssetCtx`, `Mids`, `UserFill`, `OrderUpdate`, …). Implementations plug in behind these traits; the default is selected by the benchmark (§10).

## 10. Backend benchmark spike (decision gate)

**Candidates**

| ID | Backend | Notes |
|---|---|---|
| A | `hyperliquid_rust_sdk` 0.6 | Official; pulls deprecated `ethers 2.x` + `tokio-tungstenite 0.20` |
| B | `mev-hl-client` custom | Alloy + `fastwebsockets` + `rmp-serde` |
| C | Alloy-native community SDK | e.g. `hypersdk`; **optional, time-boxed** — only pursued if the custom path stalls |

**Method (`criterion` + a WS load harness, fixed hardware, ≥3 runs)**

1. **WS decode:** messages/sec and p50/p95/p99 decode latency for `l2Book` and `trades` streams (use recorded fixtures for determinism, plus one live run).
2. **Sign latency:** build + EIP-712 phantom-agent sign of a representative order (msgpack hash → EIP-712, chain id 1337).
3. **End-to-end submit:** wall time build→sign→POST `/exchange` (testnet).
4. **Reconnect:** time from forced socket drop to resumed fresh state.
5. **Footprint:** binary size, `cargo tree` size, compile time, memory.

**Decision rule:** any candidate that is incorrect (signature/rounding mismatch) is disqualified. Among correct candidates, pick the fastest p99 on (1)–(3); tie-break on dependency hygiene (no `ethers`) then maintenance. Result recorded in `specs/decisions/0001-hl-client-backend.md`.

**Anti-goal:** don't optimize to a specific vendor prematurely — the trait boundary keeps the choice reversible.

## 11. Observability

Metrics: msgs/sec per channel, decode latency histogram, book staleness seconds, connected sockets/subscriptions, reconnect count and duration, REST weight consumed/min, dropped-due-to-backpressure count, parse errors. All tagged by network and backend ID.

## 12. Testing

- Wire types: golden JSON fixtures from the docs for every channel.
- Decoder: fuzz/`proptest` malformed frames → `Err`, never panic.
- `fastAssetCtxs`: base64 + raw-DEFLATE decode unit tests, including the documented sample payload.
- Connection manager: mock WS server (drop/reconnect, snapshot ack, out-of-order, duplicate).
- `wiremock` for `/info` (rate-limit 429, pagination).
- Benchmark harness runs in CI in "quick" mode and fully on demand.

## 13. Acceptance criteria

- Runs in `observe` mode for ≥1 hour with no unhandled panic and subscription continuity across at least one forced reconnect.
- Local book state matches a fresh `/info` `l2Book` snapshot within tolerance after reconciliation.
- REST weight usage stays under budget with stream-first data.
- `/readyz` flips to not-ready when a feed goes stale, and recovers.
- Benchmark report produced; default backend chosen and documented in the decision record.

## 14. Resolved decisions

1. **Watchlist** — start with bluechips (`BTC`, `ETH`, `SOL`) and a persisted, CLI-editable watchlist; adding markets later is data-only. CLI supports list/search/select/watch/run (`observe` default).
2. **Mark/funding source** — `activeAssetCtx` is the primary source of truth (it carries funding); `fastAssetCtxs` is an optional latency supplement for mark/mid only, added later if measured lag justifies it.
3. **Heartbeat** — 30 s application ping + pong check, plus a per-feed data watchdog; record the observed server idle-close window in the decision log.
4. **Benchmark candidates** — custom vs official is the core comparison; the Alloy-native community SDK is an optional, time-boxed third candidate.

Remaining to verify empirically (no open product question): exact server idle-close interval and the `fastAssetCtxs` lag delta.
