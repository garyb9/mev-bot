# SPEC-0008 — Market-Data Recorder & Opportunity Research

**Status:** Draft
**Milestone:** M3 (see [`docs/GOAL.md`](../docs/GOAL.md) §7). Runs in parallel with M2.5 and comes **before** the funding pilot (M4).
**Depends on:** SPEC-0000, SPEC-0001 (market data client). Execution (SPEC-0002) is **not** required.
**Blocks:** ADR-0002 (which strategy to build first), the first strategy spec (next free number, SPEC-0010+), and the go/no-go for SPEC-0003 (B) and SPEC-0005.

---

## 0. How to use this spec (read first if you are an implementing agent)

1. Read [`docs/GOAL.md`](../docs/GOAL.md) and [`AGENTS.md`](../AGENTS.md).
2. Find your task in **§14 Work breakdown**. Every task has an ID (`V-1`, `R-3`, `P-2`, `S-4`, …), its dependencies, the files to touch, and a **Done when** checklist.
3. Do not start a task until all of its dependencies are ✅ in the §14 table.
4. Do only what the task says. If something is ambiguous or wrong, write it under **§16 Open questions** and stop. Do not invent behavior.
5. When you finish, tick the task's status in §14 in the **same commit** as the code. This is the one allowed exception to "no mixed spec+code commits".
6. Facts marked **⚠ verify (V-n)** are believed true but unconfirmed. Do not rely on them until the matching V-task is ✅; that task records the confirmed value in **§15 Verified facts**.

---

## 1. Purpose

Answer one question with data: **which arbitrage / MEV-style opportunities on Hyperliquid are big enough, frequent enough, and slow enough for us to capture profitably?**

The spec has two halves:

- **Part A — Recorder.** A long-running, low-risk process (no keys, no orders) that stores **raw** market data from Hyperliquid (plus reference venues and HyperEVM) to compressed files, with accurate timestamps and explicit gap markers.
- **Part B — Research.** A toolkit and eight fixed-method studies (O1–O8) that turn recorded data into ranked, comparable opportunity reports. They end in a written decision (**ADR-0002**).

## 2. Goals

| # | Goal | Measured by |
|---|---|---|
| G-1 | Record raw frames 24/7 with no silent data loss | Every disconnect or drop produces a `gap` record; uptime ≥ 99% |
| G-2 | Timestamps precise enough for latency studies | Local receive time in ns (wall + monotonic), exchange time where the venue provides it, clock offset logged |
| G-3 | Stay within Hyperliquid rate limits by construction | Subscription planner refuses plans over budget; REST weight metered |
| G-4 | Research is reproducible | Same raw files + same code ⇒ identical report numbers |
| G-5 | Opportunities are comparable | Every study uses the same episode definition, cost model, latency grid, and report template |
| G-6 | A clear decision | ADR-0002 names the first strategy (or "none yet") with the numbers behind it |

## 3. Non-goals

- Placing, signing, or simulating orders. The recorder never loads keys.
- Building strategies. Studies measure opportunities; strategies come in their own spec after ADR-0002.
- Replacing the SQLite event log (SPEC-0004 §9). That log stores **the bot's own session inputs and decisions**. The recorder stores **raw market data at research scale** in files (see §5.2 for why).
- Real-time dashboards. Prometheus metrics for recorder health are enough.
- Machine learning. Studies use transparent, rule-based detection.

## 4. Architecture overview

```
                    ┌──────────────────────────── recorder host (Tokyo) ─────────────────────────────┐
                    │                                                                                  │
  HL WS (≤10 conns) ─┼─► RawWsConn ×N ─┐                                                             │
  HL REST /info     ─┼─► RestSnapshotter ┤                                                             │
  Binance/Bybit WS  ─┼─► RawWsConn ×M ─┼─► bounded channel ─► SegmentWriter thread (per source) ─► data/rec/…/*.jsonl.zst
  HyperEVM RPC (WS) ─┼─► EvmPoolSource ─┘        (drop ⇒ gap record, never block the socket)            │
                    │                                                                                  │
                    │   /metrics  /healthz  /readyz   (hl record)                                        │
                    └──────────────────────────────────────────────────────────────────────────────────┘
                                                        │  rsync / object storage
                                                        ▼
            research/ (Python):  read segments ─► normalize ─► Parquet tables ─► studies O1..O8 ─► reports ─► RANKING.md ─► ADR-0002
```

Components and where they live:

| Component | Location | Language | Task |
|---|---|---|---|
| Envelope + segment writer/reader | `crates/mev-recorder/src/{envelope,segment,reader}.rs` | Rust | R-1, R-2, R-7 |
| Raw WS connection (shared with the bot) | `crates/mev-hl-client/src/raw_ws.rs` | Rust | R-3 |
| HL subscription planner | `crates/mev-recorder/src/planner.rs` | Rust | R-4 |
| HL REST snapshotter | `crates/mev-recorder/src/sources/hl_rest.rs` | Rust | R-5 |
| `hl record` CLI + profiles | `crates/mev-bot/src/main.rs`, `config/record.toml` | Rust | R-6 |
| CEX reference sources | `crates/mev-recorder/src/sources/cex.rs` | Rust | R-8 |
| HyperEVM pool source | `crates/mev-recorder/src/sources/evm.rs` | Rust | R-9 |
| Deployment | `deploy/recorder/`, `RUNBOOK.md` | systemd / docs | R-10 |
| Research toolkit | `research/hlr/` | Python | P-1…P-5 |
| Studies + reports | `research/studies/`, `research/reports/` | Python / Markdown | S-1…S-10 |

**Decision — Python for research.** Research uses Python 3.12 + `uv` + `polars` (+ `duckdb` where SQL is easier). Rationale: much faster iteration for analysis, and research code never touches money paths. Production stays Rust. Research code must not be imported by, or deployed with, the bot.

---

# Part A — Recorder

## 5. Record format

### 5.1 Envelope (one JSON object per line)

Every line in a segment file is one **envelope**:

```json
{"v":1,"src":"hl-ws","conn":"hl-ws-02","seq":184467,"t_ns":1790401234567890123,"mono_ns":98765432109876,"kind":"frame","raw":"{\"channel\":\"bbo\",\"data\":{...}}"}
```

| Field | Type | Required | Meaning |
|---|---|---|---|
| `v` | u8 | yes | Envelope schema version. Starts at `1`. Bump on any breaking change. |
| `src` | string | yes | Source id (§5.4 table). |
| `conn` | string | yes | Connection id within the source, e.g. `hl-ws-02`. Stable for the process lifetime. REST uses `hl-rest`. |
| `seq` | u64 | yes | Per-`conn` counter, starting at 0 at process start, +1 for **every** envelope on that conn. Gaps in `seq` mean dropped envelopes. |
| `t_ns` | i64 | yes | Local wall-clock receive time, ns since Unix epoch (`SystemTime`). Taken **immediately** after the socket read returns, before any parsing. |
| `mono_ns` | u64 | yes | Local monotonic time in ns (`Instant` relative to a process-start anchor). Use it for durations; `t_ns` can jump if the clock is adjusted. |
| `kind` | string | yes | One of the §5.3 kinds. |
| `raw` | string | for `frame`, `rest` | The **exact** text received, unmodified. Never re-serialize it. |
| `meta` | object | for non-frame kinds | Kind-specific fields (§5.3). |

Rules:

- The recorder **does not decode** frames on the hot path. It only timestamps them and forwards the raw text. (It may peek at `"channel"` for metrics; that is optional.)
- Binary WS frames are stored with `kind:"frame_bin"` and `raw` = base64.
- Everything is UTF-8, `\n`-terminated, one envelope per line.

### 5.2 Why files, not SQLite

Rough volume: one `l2Book` stream is about 1–3 KB every ~0.5 s ⇒ ~0.2–0.5 GB/day per coin uncompressed. Recording books for dozens of coins plus bbo/trades for hundreds means tens of GB/day raw. Append-only compressed files rotate, ship, and delete cheaply. SQLite remains the store for the bot's own state (SPEC-0004). Task **V-7** measures the real volume.

### 5.3 Envelope kinds

| `kind` | Written when | `meta` fields |
|---|---|---|
| `frame` | Every text frame received | — |
| `frame_bin` | Every binary frame received | — |
| `rest` | Every REST response from the snapshotter | `{"req": <request JSON>, "status": <http status>, "latency_us": <u64>}` |
| `sub` | Right after a subscribe message is sent | `{"sub": <subscription JSON>}` |
| `conn_open` | Socket connected (initial or reconnect) | `{"url": str, "attempt": u32}` |
| `gap_start` | Socket error/close, watchdog timeout, **or** a dropped envelope | `{"reason": "close"\|"error"\|"watchdog"\|"drop"\|"shutdown", "detail": str}` |
| `gap_end` | After reconnect **and** all resubscribes are sent | `{"gap_ms": u64}` |
| `clock` | Every 60 s per source | `{"chrony_offset_ns": i64\|null, "chrony_stratum": u8\|null}` |
| `segment_open` | First line of every segment file | `{"host": str, "git_sha": str, "profile": str, "recorder_version": str}` |
| `segment_close` | Last line of every cleanly closed segment | `{"records": u64, "bytes_raw": u64}` |

**Invariant:** research code treats every interval between a `gap_start` and the next `gap_end` on the same `conn` as **missing data** for every subscription on that conn. A segment that doesn't end with `segment_close` was cut off by a crash; its last `t_ns` counts as a `gap_start`.

### 5.4 Source ids

| `src` | What | Task |
|---|---|---|
| `hl-ws` | Hyperliquid WebSocket market data | R-3, R-4 |
| `hl-rest` | Hyperliquid `/info` snapshots | R-5 |
| `binance-usdm` | Binance USDⓈ-M futures `bookTicker` | R-8 |
| `binance-spot` | Binance spot `bookTicker` | R-8 |
| `bybit-linear` | Bybit v5 linear `orderbook.1` | R-8 |
| `hyperevm` | HyperEVM block headers + pool state | R-9 |
| `finsnap` | Options positioning snapshots from the owner's finsnap service | R-11 |
| `deribit` | Deribit crypto options summaries (IV, OI, volume) | R-12 |
| `equities` | Real-time US equity quotes for HIP-3 stock-perp underlyings | R-13 |
| `hl-node` | Output files of our own node (SPEC-0009) | SPEC-0009 N-6 |

## 6. Storage layout, rotation, and manifest

```
data/rec/
  {network}/                      mainnet | testnet
    {src}/                        hl-ws | hl-rest | binance-usdm | …
      {YYYY-MM-DD}/               UTC date of the segment's first record
        {HH}/                     UTC hour
          {conn}-{start_t_ns}.jsonl.zst            finished segment
          {conn}-{start_t_ns}.jsonl.zst.partial    segment being written
      {YYYY-MM-DD}/manifest.jsonl                   one line per finished segment
```

| Rule | Value |
|---|---|
| Compression | zstd, level 3 (configurable), streaming encoder |
| Rotation | At the top of every UTC hour **or** when the uncompressed size passes 1 GiB, whichever comes first |
| Finalize | Write `segment_close`, finish the zstd frame, `fsync`, rename `.partial` → final, append a manifest line |
| Crash recovery | On startup, any leftover `.partial` is renamed to `…jsonl.zst.crashed` and listed in the manifest with `"crashed": true`. It is **not** deleted: the zstd stream is readable up to the last full frame. |
| Manifest line | `{"file": str, "src": str, "conn": str, "first_t_ns": i64, "last_t_ns": i64, "records": u64, "bytes_raw": u64, "bytes_zst": u64, "crashed": bool}` |
| Flush cadence | Flush the zstd encoder at least every 5 s, so a crash loses ≤ 5 s |
| Disk guard | If free disk space < `min_free_gb` (default 20), stop the **lowest-priority** streams first (§7.3), emit `gap_start{reason:"disk"}`, and alert. Never crash on a full disk. |
| Retention | Local: delete segments older than `retain_days` (default 30) **only after** they are shipped (R-10). Shipping is optional in v1. |

`data/` is already git-ignored. Do not commit recordings.

## 7. Hyperliquid WebSocket recording

### 7.1 Protocol facts

| Fact | Value | Status |
|---|---|---|
| Mainnet WS URL | `wss://api.hyperliquid.xyz/ws` (`Network::ws_url()`) | known |
| Subscribe message | `{"method":"subscribe","subscription":{…}}` | known (in `ws.rs`) |
| App-level ping | send `{"method":"ping"}`, expect `{"channel":"pong"}` | known |
| Server idle close | closes a connection that has sent nothing for ~60 s | ⚠ verify (V-1) |
| Limits per IP | ≤ 10 WS connections, ≤ 30 new connections/min, ≤ 1000 subscriptions, ≤ 2000 client→server messages/min | known (SPEC-0001 §5) |
| `l2Book` depth | up to 20 levels per side; optional `nSigFigs` (2–5) and `mantissa` params aggregate levels | ⚠ verify (V-1) |
| `l2Book` cadence | a snapshot per block when the book changed, roughly every ≥ 0.5 s | ⚠ verify (V-1) |
| `bbo` | pushed only when best bid/offer changes on a block | known (SPEC-0001) |
| `trades` payload | includes `users: [buyer, seller]` addresses | ⚠ verify (V-1) |
| `allMids` HIP-3 | accepts `"dex": "<name>"` to get mids for a HIP-3 dex | ⚠ verify (V-1) |
| `activeAssetCtx` for spot | spot coins (`@123`) may answer on a different channel name (e.g. `activeSpotAssetCtx`) | ⚠ verify (V-1). The recorder stores raw frames, so this only matters for research normalization. |

### 7.2 Streams to record

| Stream | Subscription JSON | Research use | Priority (§7.3) |
|---|---|---|---|
| `bbo` | `{"type":"bbo","coin":C}` | Top of book for every arb study | 1 (highest) |
| `trades` | `{"type":"trades","coin":C}` | Flow, liquidations, fill realism | 2 |
| `activeAssetCtx` | `{"type":"activeAssetCtx","coin":C}` | Funding, mark, oracle, OI | 2 |
| `allMids` | `{"type":"allMids"}` and `{"type":"allMids","dex":D}` per HIP-3 dex | Cheap cross-market sanity checks | 3 |
| `l2Book` | `{"type":"l2Book","coin":C}` (no aggregation) | Depth, slippage and size estimates | 3 |

Not recorded in v1: `candle` (derivable from trades), `fastAssetCtxs` (revisit if V-1 shows it's materially faster), and any user-specific channel.

### 7.3 Universes and profiles

Recording is driven by a **profile** in `config/record.toml`. Coins are chosen by **universe selectors**, resolved at startup (and on every metadata refresh) using the existing `AssetMap`/`MarketSelector`:

| Selector | Resolves to |
|---|---|
| `"BTC"`, `"xyz:TSLA"`, `"@107"`, `"PURR/USDC"` | That exact market (validated; unknown ⇒ startup error) |
| `perps:all` | Every non-delisted main-dex perp |
| `perps:top:N` | Top N main-dex perps by `dayNtlVlm` from `metaAndAssetCtxs` |
| `hip3:all` | Every market on every dex from `perpDexs` |
| `hip3:<dex>` | Every market on one HIP-3 dex |
| `spot:all` | Every spot pair from `spotMeta` |
| `spot:top:N` | Top N spot pairs by `dayNtlVlm` from `spotMetaAndAssetCtxs` |
| `spot:quotes` | Every spot pair whose base token trades against **more than one** quote token (the O2 universe) |

Example `config/record.toml` (task R-6 creates this file):

```toml
[profile.default]
network = "mainnet"
out_dir = "data/rec"
zstd_level = 3
min_free_gb = 20
retain_days = 30
meta_refresh_secs = 300          # re-resolve universes, pick up new listings

[profile.default.hl]
bbo              = ["perps:all", "hip3:all", "spot:top:40", "spot:quotes"]
trades           = ["perps:top:40", "hip3:all", "spot:top:20"]
active_asset_ctx = ["perps:all", "hip3:all"]
all_mids         = true          # main dex + every HIP-3 dex
l2book           = ["BTC", "ETH", "SOL", "HYPE", "hip3:all"]
max_subs         = 900           # keep 100 of the 1000 as headroom
subs_per_conn    = 150
connections      = 8             # of 10; leaves 2 for ad-hoc tools / the bot

[profile.default.rest]
enabled = true
# see §8 for the request list and cadences

[profile.default.cex]
binance_usdm = ["BTCUSDT", "ETHUSDT", "SOLUSDT", "HYPEUSDT"]
binance_spot = ["BTCUSDT", "ETHUSDT", "SOLUSDT"]
bybit_linear = ["BTCUSDT", "ETHUSDT", "SOLUSDT", "HYPEUSDT"]

[profile.default.hyperevm]
enabled = false                  # R-9; needs a non-public RPC (see §10)
rpc_ws  = "env:HL_EVM_WS_URL"
pools   = "config/hyperevm-pools.toml"
```

### 7.4 Subscription planner (task R-4)

Input: a resolved profile. Output: a `Plan` = list of connections, each with an ordered list of subscriptions.

Algorithm (must be deterministic: same input ⇒ same plan):

1. Expand every selector into `(stream, coin)` pairs, de-duplicated, sorted by `(priority, stream, coin)`.
2. Count them. If the count is over `max_subs`, **drop from the lowest priority upward** (priority 3 first, then 2) until the plan fits. Log every dropped pair at WARN and fail if a priority-1 pair would be dropped, unless `--allow-truncate` is set.
3. Put `l2Book` subscriptions on their **own** connection(s): they are the heaviest, and isolating them keeps `bbo` latency clean.
4. Fill the remaining connections round-robin, at most `subs_per_conn` each, so no single coin's `bbo`/`trades`/`ctx` all share one socket (limits the blast radius of one bad connection).
5. Fail if the connection count exceeds `connections`.
6. Pace subscribe messages at ≤ 20 messages/s per process (all connections combined) to stay under 2000 msgs/min even during a reconnect storm. Ping messages count toward the budget.
7. Pace new connections at ≤ 1 every 3 s (≤ 20/min, under the 30/min limit).

`hl record plan --profile default` prints the plan as a table and exits without connecting. It is the planner's acceptance test.

### 7.5 Raw WS connection (task R-3)

Extract a reusable raw connection from `crates/mev-hl-client/src/ws.rs` into `raw_ws.rs`. Then rebuild `WsMarketStream` on top of it, so the bot and the recorder share one reconnect implementation.

| Behavior | Requirement |
|---|---|
| Output | Yields `RawEvent::{Text{t_ns, mono_ns, text}, Binary{…}, Opened{attempt}, Gap{reason, detail}}` |
| Heartbeat | Send app `ping` every 30 s. If no inbound message of **any** kind (including `pong`) arrives for `watchdog` (default 45 s), emit `Gap{watchdog}` and reconnect. |
| Backoff | Exponential from 500 ms, cap 30 s, **full jitter** (`sleep = rand(0, backoff)`). Attempt counter resets after 60 s of healthy connection. |
| Resubscribe | After reconnect, resend every subscription in original order through the §7.4 pacer, then emit `gap_end`. |
| Cancellation | Accepts a `CancellationToken`/shutdown future. On shutdown, emit `Gap{shutdown}` and close cleanly. The current `reconnect()` loop can't be cancelled; fix that. |
| Errors | Never panics. No `unwrap`/`expect` on socket paths. |
| Metrics | `hl_ws_reconnects_total{src,conn,reason}`, `hl_ws_connected{src,conn}`, `hl_ws_msgs_total{src,conn}` |

Existing `ws.rs` tests must keep passing after the refactor, and new tests use a local mock WS server (the pattern already in `ws_exchange.rs` tests).

## 8. Hyperliquid REST snapshots (task R-5)

Recorded as `kind:"rest"` envelopes under `src:"hl-rest"`. All requests are `POST /info`. Use `HttpInfo::info(body)` from `mev-hl-client`.

| Request body | Cadence | Weight (SPEC-0001 §5) | Why |
|---|---|---|---|
| `{"type":"meta"}` | startup + every `meta_refresh_secs` | 20 | Universe, szDecimals |
| `{"type":"perpDexs"}` | same | 20 | HIP-3 dex list |
| `{"type":"meta","dex":D}` per HIP-3 dex | same | 20 each | HIP-3 universes |
| `{"type":"spotMeta"}` | same | 20 | Spot tokens and pairs |
| `{"type":"metaAndAssetCtxs"}` | every 60 s | 20 | Funding/OI/volume for **all** perps (backs up WS ctx) |
| `{"type":"spotMetaAndAssetCtxs"}` | every 60 s | 20 | Spot volume and mid |
| `{"type":"predictedFundings"}` | every 5 min | 20 | Predicted funding for HL and CEXes (O7) ⚠ verify shape (V-1) |
| `{"type":"fundingHistory","coin":C,"startTime":T}` | once a day per coin in `active_asset_ctx`, paging forward from the last stored time | 20 + per-item surcharge | Funding backfill (O7) |
| `{"type":"candleSnapshot","req":{"coin":C,"interval":I,"startTime":T,"endTime":E}}` for I ∈ {1m, 5m, 1h} | once a day per coin in `bbo`, paging forward from the last stored time; a one-time backfill on first run | 20 + 20 per 60 candles | History for O10 Part E and O11. The venue keeps only the most recent ~5000 candles per interval ⚠ verify (V-1), so 1m history is only a few days: **start this early**. |

Budget: the snapshotter owns a token bucket of **300 weight/min** (a quarter of the 1200/IP budget, leaving the rest for the bot and tools). A request that doesn't fit waits; it is never dropped. Metric: `hl_rest_weight_used_total{src="recorder"}`.

## 9. Reference venues (task R-8)

Used by study O5 (cross-venue lead-lag) and as fair-value references in O1/O3.

| `src` | URL | Subscribe | Payload fields used | Keepalive |
|---|---|---|---|---|
| `binance-usdm` | `wss://fstream.binance.com/stream?streams=btcusdt@bookTicker/ethusdt@bookTicker/…` | streams encoded in the URL | `s, b, B, a, A, T` (transaction time), `E` (event time) | server pings; reply with pong (tungstenite does this automatically) |
| `binance-spot` | `wss://stream.binance.com:9443/stream?streams=btcusdt@bookTicker/…` | in the URL | `s, b, B, a, A` (no exchange timestamp: rely on `t_ns`) | same |
| `bybit-linear` | `wss://stream.bybit.com/v5/public/linear` | `{"op":"subscribe","args":["orderbook.1.BTCUSDT",…]}` | `ts`, `data.b`, `data.a` | send `{"op":"ping"}` every 20 s |

All three ⚠ verify (V-2): URLs, field names, and whether each venue is reachable from the chosen host region. Symbols are lowercased for Binance URLs. Reuse `RawWsConn` from R-3; only the subscribe and keepalive hooks differ.

### 9.1 Options and equities sources (tasks R-11, R-12, R-13)

These feed studies **O9** and **O10**: options-informed trading of HIP-3 tokenized-stock perps and crypto bluechips. None of them is latency-critical except `equities` for O10 Part A.

| `src` | What | Cadence | Notes |
|---|---|---|---|
| `finsnap` | The owner's finsnap service (`../finsnap`, TypeScript, Yahoo Finance data): `GET /snap` JSON with per-expiry options positioning (`pcRatio`, `skewScore`, weighted-mean strike, wall inference, labels) and the per-strike profile | every 5 min during US regular hours, plus one snapshot after the close | Recorded raw as `rest` envelopes. Yahoo data is **delayed** ⚠ verify how much (V-9). finsnap today keeps only strike/volume/OI and drops IV/bid/ask; cross-repo task **F-1** adds them. finsnap's default options universe is ETFs (SPY, QQQ, sectors, IBIT), so single stocks behind HIP-3 perps must be added (F-1). |
| `deribit` | Deribit public API: `public/get_book_summary_by_currency` (`currency=BTC\|ETH`, `kind=option`) + `public/get_index_price` | every 60 s | Free and real-time; includes mark IV, OI, volume, and underlying price per instrument. The right options source for BTC/ETH (much better than Yahoo/IBIT). ⚠ verify endpoints, fields, and limits (V-10). |
| `equities` | Streaming real-time quotes (bid/ask/last) for the underlyings of HIP-3 stock perps (e.g. TSLA, NVDA, the index behind `XYZ100`) | streaming during US hours (+ pre/post market if the provider has it) | Provider chosen in **V-11** (free feeds cover only part of the volume; consolidated feeds are paid). Check the provider's terms allow storing the data. |

## 10. HyperEVM pool state (task R-9)

Only needed for studies O4/O8. **Start R-9 only after V-5 and V-6 are ✅.**

| Item | Requirement |
|---|---|
| RPC | Not the public `rpc.hyperliquid.xyz/evm` (100 req/min is too low). Use our own node ([SPEC-0009](SPEC-0009-own-node.md), preferred) or a provider, configured by env `HL_EVM_WS_URL`. |
| Blocks | Subscribe `newHeads`; write each header as `kind:"frame"` with `raw` = the JSON header. |
| Pool list | `config/hyperevm-pools.toml`: `[[pool]] address, dex, kind = "v2"\|"v3", token0, token1, fee_bps`. Produced by V-6. Never hardcode addresses in Rust. |
| State | For every new block, one `Multicall3` `eth_call` at that block number reading `getReserves()` (v2) or `slot0()` + `liquidity()` (v3) for every pool. Write the raw JSON-RPC response as a `kind:"rest"` envelope with `meta.req` = the call and `meta.block` = the block number. |
| Dual blocks | HyperEVM has small fast blocks (~1 s) and large slow blocks (~1 min) ⚠ verify (V-5). Record a `block_kind` if the header exposes it. |
| Library | `alloy` provider + `sol!` bindings (Alloy is the project standard; **no ethers**). |

## 11. Clock discipline

| Requirement | Detail |
|---|---|
| Time sync | Host runs `chrony` against reliable NTP sources (the cloud provider's time service if available). |
| Logging | The `clock` envelope every 60 s includes the chrony offset if `chronyc -c tracking` is available; otherwise `null`. |
| Use in research | One-way latency ≈ `t_ns − exchange_time`, valid only while the chrony offset is small (< 1 ms). Report the offset alongside any latency number. |

## 12. CLI, health, metrics, deployment

### 12.1 CLI (task R-6, R-7)

| Command | Behavior |
|---|---|
| `hl record [--profile NAME] [--network …]` | Run the recorder until SIGTERM. Never loads keys. Exits non-zero on config errors. |
| `hl record plan [--profile NAME]` | Print the resolved subscription plan (per connection: stream, coin, priority) plus totals vs limits. No sockets opened. |
| `hl record inspect PATH…` | For segment files: record counts by `src/conn/kind/channel`, first/last time, gaps (count + total ms), `seq` holes. |
| `hl record verify --date YYYY-MM-DD` | Check the manifest against the files on disk (sizes, counts, `.crashed`), and report coverage % per stream for that day. |
| `hl probe latency [--count 20]` | Measure TCP connect time, TLS handshake, WS ping→pong RTT, and `/info` `allMids` RTT to Hyperliquid. Print p50/p90/max. Used by V-4 to pick a region. |

### 12.2 Health and metrics

`hl record` serves the same HTTP endpoints as `hl run` (reuse `mev-metrics`):

- `/healthz`: process alive and the segment writer thread alive.
- `/readyz`: every planned connection is connected and has received data within its watchdog window.
- `/metrics`:

| Metric | Type | Labels |
|---|---|---|
| `hl_rec_records_total` | counter | `src, conn, kind` |
| `hl_rec_bytes_raw_total` / `hl_rec_bytes_zst_total` | counter | `src` |
| `hl_rec_dropped_total` | counter | `src, conn` |
| `hl_rec_gap_seconds_total` | counter | `src, conn, reason` |
| `hl_rec_channel_depth` | gauge | `src` |
| `hl_rec_segment_rotations_total` | counter | `src` |
| `hl_rec_disk_free_bytes` | gauge | — |
| `hl_rec_clock_offset_ns` | gauge | — |
| `hl_ws_*`, `hl_rest_weight_used_total` | as §7.5 / §8 | — |

### 12.3 Deployment (task R-10)

| Item | Requirement |
|---|---|
| Region | Chosen by V-4 measurements. Expected: Tokyo (AWS `ap-northeast-1` or equivalent), ⚠ verify (V-4). Record the measurements in §15. |
| Host | 2+ vCPU, 4+ GB RAM, disk ≥ 30 days × V-7 daily volume × 1.5. |
| Process | systemd unit `deploy/recorder/hl-recorder.service`: `Restart=always`, `RestartSec=5`, non-root user, `WorkingDirectory` with `data/`. |
| Time | `chrony` enabled and verified. |
| Shipping (optional v1) | Daily `rclone`/`aws s3 sync` of finished segments + manifests to object storage, then local retention applies. |
| Runbook | Add a "Recorder" section to `RUNBOOK.md`: start/stop, check health, check coverage (`hl record verify`), disk full, rotate/ship. |

---

# Part B — Opportunity research

## 13. Research method (shared by every study)

Every study **must** use these definitions, so studies can be ranked against each other.

### 13.1 Normalized tables (task P-2)

`research/hlr/normalize.py` turns segments into Parquet, partitioned by `date`. All prices and sizes are `float64` here (research only). Timestamps are `int64` ns (`t_ns`) plus `ts_exch_ms` where the venue provides one.

| Table | Columns | Built from |
|---|---|---|
| `bbo` | `t_ns, ts_exch_ms, venue, market, bid_px, bid_sz, ask_px, ask_sz` | HL `bbo`; HL `l2Book` level 0 (tagged `venue="hl-book"`); Binance/Bybit tickers |
| `book` | `t_ns, ts_exch_ms, venue, market, side, level, px, sz, n` | HL `l2Book` |
| `trades` | `t_ns, ts_exch_ms, venue, market, side, px, sz, tid, hash, buyer, seller` | HL `trades` (`buyer`/`seller` null if absent) |
| `ctx` | `t_ns, market, funding, open_interest, oracle_px, mark_px, mid_px, premium, day_ntl_vlm` | HL `activeAssetCtx` + REST `metaAndAssetCtxs` |
| `funding_hist` | `time_ms, market, funding_rate, premium` | REST `fundingHistory` |
| `markets` | `snapshot_t_ns, market, kind(perp/spot/hip3), dex, base, quote, asset_id, sz_decimals, max_leverage` | REST meta snapshots |
| `gaps` | `src, conn, start_ns, end_ns, reason` | `gap_start`/`gap_end`, crashed segments, `seq` holes |
| `evm_pools` | `t_ns, block, pool, reserve0, reserve1, sqrt_price_x96, liquidity, tick` | R-9 records |
| `options_expiry` | `t_ns, t_data, underlying, expiry, pc_ratio, skew_score, wmean_strike, wmean_std, wall_strike, wall_side, label, call_vol, put_vol, call_oi, put_oi` | `finsnap` (`t_data` = the data's own as-of time, which is **not** `t_ns`) |
| `options_strikes` | `t_ns, t_data, underlying, strike, call_vol, call_oi, put_vol, put_oi, call_iv, put_iv` (IVs null until F-1) | `finsnap` strike profile |
| `deribit_options` | `t_ns, instrument, underlying, expiry, strike, cp, mark_iv, bid_iv, ask_iv, open_interest, volume, underlying_px, index_px` | `deribit` |
| `equity_quotes` | `t_ns, ts_exch_ms, symbol, bid_px, bid_sz, ask_px, ask_sz, last_px, session(pre/regular/post)` | `equities` |
| `bars` | `t_open_ms, interval(1s/10s/1m/5m/15m/1h/1d), venue, market, open, high, low, close, volume, n_trades, source(mid/trade/candle)` | Built from `bbo` mids and `trades`; plus HL `candleSnapshot` backfill (R-5) and stock bars (V-11 provider) for longer history |

Market naming in every table: HL perps `BTC`, HIP-3 `xyz:TSLA`, HL spot as `BASE/QUOTE` (resolved from `spotMeta`, never `@123`), CEX as `binance-usdm:BTCUSDT`.

### 13.2 Cost model (task P-3)

`research/costs.toml` holds every fee. Studies read it through `hlr.costs`; no fee is hardcoded in a study.

| Venue / market | Taker | Maker | Status |
|---|---|---|---|
| HL perp (main dex), base tier | 4.5 bps | 1.5 bps | known (SPEC-0003 §5) |
| HL spot, base tier | 7.0 bps | 4.0 bps | known (SPEC-0003 §5) |
| HL HIP-3 perps | ? | ? | ⚠ verify (V-3): deployer-set / multiplier |
| Binance USDⓈ-M, VIP0 | 5.0 bps | 2.0 bps | ⚠ verify (V-2) |
| Bybit linear, VIP0 | 5.5 bps | 2.0 bps | ⚠ verify (V-2) |
| HyperEVM DEX swap | pool fee (per pool in `hyperevm-pools.toml`) + gas in HYPE | — | V-6 |

Additional cost terms:

| Term | Definition |
|---|---|
| `buffer_bps` | Safety margin, default **2 bps**, configurable per study |
| Slippage | Walk the `book` table for the intended size when available. Otherwise assume the size is capped at top-of-book size (no walk). |
| Funding | Held positions accrue `funding_rate × notional` per hourly settlement (sign by side) |
| Gas (EVM) | `gas_used × gas_price` in HYPE × HYPE/USDC mid at that time |
| Transfer (Core↔EVM) | Fixed cost + delay from V-5; used by O4 only |

Default execution assumption is **taker on every leg** (conservative). A study may add a maker-leg variant, but must also report taker-taker.

### 13.3 Episode definition (task P-4)

An **opportunity signal** is a time series `net_bps(t)` for a candidate trade (e.g. "buy X at ask, sell Y at bid"):

```
gross_bps(t) = (sell_px(t) − buy_px(t)) / ref_mid(t) × 1e4
net_bps(t)   = gross_bps(t) − Σ fee_bps(legs) − buffer_bps − slippage_bps(size)
size(t)      = min(top-of-book size of each leg at t), capped at max_notional
```

An **episode** is a maximal interval `[t_start, t_end)` where `net_bps(t) > 0` **and** every input feed is valid:

- Feed validity: the last update of each input is newer than `stale_ms` (default 2000) **and** `t` is not inside any `gaps` interval for that input's connection.
- Episodes separated by < `merge_ms` (default 50) merge into one.

Per episode, record:

| Field | Meaning |
|---|---|
| `t_start, t_end, duration_ms` | Timing |
| `peak_net_bps`, `start_net_bps` | Size of the edge |
| `size_usd_at_start` | Capturable notional at `t_start` |
| `captured_L` for each latency `L` | `net_bps(t_start + L) × size_usd(t_start + L)` **if** the episode is still open at `t_start + L`, else `0` (missed) |

### 13.4 Latency grid

Every latency-sensitive number is reported for **L ∈ {10, 50, 100, 250, 500, 1000} ms**. `L` is the time from the first data that reveals the episode to our orders arriving at the venue. The **headline** latency is **L = 250 ms** until measurements replace it: network RTT from V-4 plus internal tick-to-order from SPEC-0002 H-7 ([`docs/GOAL.md`](../docs/GOAL.md) §5.2). Because speed is a project priority, every report also states the **minimum latency at which the study still passes** (the "latency requirement").

### 13.5 Study metrics (same columns in every report)

| Metric | Definition |
|---|---|
| `days` | Days of valid data used (need ≥ 14 for a final report; ≥ 3 for a preliminary one) |
| `coverage_pct` | Share of wall time where all inputs were valid |
| `episodes_per_day` | Median and p90 across days |
| `duration_ms` | p50 / p90 of episode durations |
| `peak_net_bps` | p50 / p90 |
| `capture_rate_L` | Share of episodes still open at `+L` |
| `usd_per_day_L` | Σ `captured_L` / days, at the study's `max_notional` |
| `capital_usd` | Capital needed to run the strategy at that notional (both legs, margin at 3× unless stated). Evaluated at every point of the **capital grid** in `research/thresholds.toml` (default $10k / $25k / $50k / $100k): `max_notional` scales with capital, but capture is capped by the book size available in each episode, so APR usually falls as capital grows. |
| `apr_L` | `usd_per_day_L × 365 / capital_usd`, reported for each capital grid point |
| `best_capital_usd` | The grid point with the highest `usd_per_day_L` that still meets the APR floor |
| `concentration` | Share of total PnL from the single best day (robustness; > 50% is a red flag) |
| `competition_hint` | p50 duration < 100 ms ⇒ "latency-competitive"; > 2 s ⇒ "slow / capacity-bound" |
| `latency_requirement_ms` | Largest `L` in the grid at which the study still passes §13.6 (or "none"). Tells us how fast we must be. |

### 13.6 Scoring and the go/no-go rule

**The profit bar is a range, not a single number, and it is adjustable.** Every threshold lives in `research/thresholds.toml` (created by task P-3). Studies and `hlr-rank` read it at run time; changing the file and re-running `uv run hlr-rank` re-grades every study without touching code. Owner decisions (2026-09-26): **capital is small to medium** (the $10k–$100k grid), **target APR 25%**, **floor APR 10%**.

```toml
# research/thresholds.toml: owner-adjustable; re-run `uv run hlr-rank` after editing
[capital]
grid_usd     = [10_000, 25_000, 50_000, 100_000]   # small → medium
headline_usd = 25_000                               # the capital used for the headline verdict

[apr]
target = 0.25    # ≥ target ⇒ PASS
floor  = 0.10    # floor ≤ APR < target ⇒ MARGINAL; < floor ⇒ FAIL

[quality]
min_episodes_per_day = 10
max_concentration    = 0.40
min_coverage_pct     = 0.90
robustness_buffer_multiplier = 2.0   # usd_per_day must stay > 0 with buffer_bps × this

[latency]
headline_ms = 250   # replace with measured (V-4 + H-7) when known
```

Each study gets one verdict, evaluated at the headline latency **and** the headline capital (and also reported for every capital grid point):

| Verdict | Rule | Meaning |
|---|---|---|
| **PASS** | `apr_L ≥ apr.target` **and** every `[quality]` criterion holds | Build it (candidate for M5) |
| **MARGINAL** | `apr.floor ≤ apr_L < apr.target` **and** every `[quality]` criterion holds | Acceptable. Build it if it's cheap to implement, stacks with a PASS strategy on shared infrastructure, or nothing passes. |
| **FAIL** | `apr_L < apr.floor`, **or** any `[quality]` criterion fails | Don't build it (re-test later if conditions change) |
| **INCONCLUSIVE** | Not enough data (`days`/`coverage_pct` too low) | Keep recording; re-run |

**Pilot allowance.** The owner may approve a **time-boxed, small-capital live pilot** for a study that is MARGINAL, or INCONCLUSIVE with a positive point estimate, when the owner has an independent prior (e.g. discretionary trading experience). This matches GOAL §9 gate G1.5. A pilot needs: an ADR entry naming the study and the cap; capital ≤ `[pilot].max_capital_usd`; duration ≤ `[pilot].max_weeks`; a hard stop at `[pilot].max_loss_usd`; and the G2 safety gate already passed. Pilot results are fed back into the study report as realized-vs-expected.

```toml
# research/thresholds.toml (continued)
[pilot]
max_capital_usd = 10_000
max_weeks       = 4
max_loss_usd    = 1_000
```

The **ranking score** is `usd_per_day_L × (1 − concentration)` at the headline capital, with ties broken by implementation cost (S/M/L from the report). `RANKING.md` lists every study (every verdict) grouped PASS → MARGINAL → INCONCLUSIVE → FAIL, with its metrics row, score, and a per-capital-grid APR column, so the owner can see at a glance how each opportunity scales.

### 13.7 Report template (task P-5)

Every study writes `research/reports/O{n}-{slug}.md` with exactly these sections:

1. **Hypothesis** (one paragraph)
2. **Data**: date range, tables, markets, `coverage_pct`, gaps excluded
3. **Method**: which legs, the `net_bps` formula, parameters (`buffer_bps`, `max_notional`, `stale_ms`), anything that departs from §13
4. **Results**: the §13.5 metrics table, latency-grid table, per-day bar chart (PNG in `research/reports/img/`), top-10 episodes table
5. **Sanity checks**: at least 3 of the largest episodes inspected by hand against raw frames; is each real, or a data artifact?
6. **Verdict**: PASS / MARGINAL / FAIL / INCONCLUSIVE against §13.6 (and §13.8 for slow-signal studies), the APR at each capital grid point, implementation cost S/M/L, and the main risks
7. **Reproduce**: the exact command(s) and git SHA

### 13.8 Slow-signal method (task P-6; used by O9, O10 Part B, and optionally O6/O7)

Episodes (§13.3) fit fast dislocations. **Directional signals held for hours or days** use this method instead. It follows the no-lookahead discipline finsnap already uses.

| Step | Rule |
|---|---|
| Availability | A signal computed from data with as-of time `t_data` can only be acted on at `t_avail = max(t_ns received, t_data + data_delay)`, where `data_delay` comes from V-9/V-10 (e.g. Yahoo delay). Never use data before it was available. |
| Entry | Taker at the HL `bbo` at `t_avail + 1 s` (latency is irrelevant at this horizon). Skip the signal if the HL feed is invalid then. |
| Horizons | Exit taker at each of **H ∈ {1 h, 4 h, 1 d, 3 d}** after entry (report all), or at the signal's own exit rule if it has one. |
| Costs | 2 × HL taker fee (HIP-3 fees from V-3/V-12) + funding accrued over the hold (from `ctx`/`funding_hist`) + `buffer_bps`. |
| Sizing | Fixed fraction of capital per signal (default 20%, leverage ≤ 2×), at each capital grid point; overlapping positions share capital. |
| Pre-registration | Every signal variant is written in the study file **before** it's run on data. All tested variants are reported, including failures. No tuning on the full sample. |
| Out-of-sample | Split chronologically 60% / 40%. Parameters (if any) are fixed on the first 60%. The **headline results are the last 40%**. |
| Baselines | (a) same entry times with a random direction (1000 shuffles → p-value); (b) buy-and-hold of the same perp over the same periods; (c) the signal delayed by one extra snapshot (detects leakage). |
| Metrics | `n_signals`, hit rate, mean and median net return per trade (bps), per-trade t-stat, daily-aggregated Sharpe (annualized), max drawdown, worst trade, max adverse excursion, `apr` at each capital grid point, `concentration` (share of PnL from the best single week). |
| Verdict | Same PASS / MARGINAL / FAIL / INCONCLUSIVE tiers and APR range as §13.6 (on out-of-sample results), **plus**: ≥ 30 out-of-sample signals, t-stat ≥ 2, beats the random-direction baseline at p ≤ 0.05, and the delayed-signal variant doesn't beat the real one (otherwise suspect leakage). |

### 13.9 The studies

Each study below is one task (S-1…S-10). All depend on P-1…P-5, plus the data listed.

### O1 — HIP-3 / main-dex same-underlying dislocations (task S-1)

| Item | Detail |
|---|---|
| Hypothesis | The same underlying on different perp dexes (e.g. a HIP-3 equity/index perp on two dexes, or a HIP-3 crypto perp vs the main-dex perp) trades at prices that diverge by more than fees often enough to arb. |
| Data | `bbo` for `hip3:all` and main-dex perps; `ctx` (funding differs by dex); `markets` |
| Pairing | `research/mappings/underlyings.toml` maps each market to an underlying id, e.g. `BTC = ["BTC", "<dex>:BTC"]`, `TSLA = ["xyz:TSLA", "<dex2>:TSLA"]`. Built by hand from `hl markets --dex …` listings **and reviewed**. Only mapped pairs are studied. |
| Signal | For each ordered pair (A, B) of the same underlying: buy A at ask, sell B at bid. |
| Extra cost | Funding differential over the expected holding time (default: 1 h, since positions are unwound when prices re-converge) |
| Special checks | Oracle/mark definitions can differ across dexes. Report the persistent **basis** (rolling 1 h median of mid differences) separately, and flag pairs whose "edge" is really a stable basis rather than transient dislocations. |

### O2 — Spot triangular across stablecoin quotes (task S-2)

| Item | Detail |
|---|---|
| Hypothesis | Tokens quoted in several stablecoins on HyperCore spot (e.g. `X/USDC` and `X/USDT0`) plus the stable-vs-stable pair (e.g. `USDT0/USDC`) form triangles whose product departs from 1 by more than three spot taker fees. |
| Data | `bbo` for `spot:quotes`; `markets` (quote tokens come from `spotMeta`, never hardcoded ⚠ verify the quote-token set in V-3) |
| Signal | For each triangle and both directions: `product = Π (1 / ask or bid)` along the cycle; `gross_bps = (product − 1) × 1e4`; subtract 3 × spot taker. |
| Size | Minimum top-of-book notional across the 3 legs |
| Special checks | Spot books can be thin: report the size distribution; episodes under the $10 minimum order notional don't count. |

### O3 — HyperCore spot vs perp instantaneous basis (task S-3)

| Item | Detail |
|---|---|
| Hypothesis | The HL spot mid (e.g. `UBTC/USDC`, `HYPE/USDC`) and the HL perp mid for the same underlying occasionally dislocate beyond fees and their normal basis. |
| Data | `bbo` for the mapped spot/perp pairs (use `underlyings.toml`), `ctx` |
| Signal | Buy the cheap side, sell the rich side (taker both), relative to the rolling 1 h median basis (the dislocation is the part **beyond** the normal basis). |
| Note | Distinct from O7 carry: O3 is short-lived dislocation, O7 is funding income. |

### O4 — HyperCore spot ↔ HyperEVM DEX (task S-4)

| Item | Detail |
|---|---|
| Hypothesis | Tokens that trade on both HyperCore spot and HyperEVM DEX pools (HYPE first) dislocate beyond fees + gas. With pre-positioned inventory on both sides, each leg can be traded independently (non-atomic). |
| Data | `bbo` for the Core spot pair; `evm_pools` for the mapped pools; gas prices from block headers |
| Signal | Buy on the cheaper venue, sell on the richer. EVM leg price = constant-product quote for the size (v2), or tick-walk (v3); cost = pool fee + gas. |
| Latency | EVM leg inclusion delay: next small block (~1 s ⚠ V-5). Evaluate `captured_L` using the Core price at `t + L` and the EVM pool state **in the block our tx would land in**. |
| Inventory | Report how often inventory would need rebalancing across Core↔EVM, and its cost/delay (V-5 transfer facts). |
| Prereq | V-5, V-6, R-9 |

### O5 — CEX-lead latency arb (task S-5)

| Item | Detail |
|---|---|
| Hypothesis | Binance/Bybit perp prices lead HL perp prices by tens to hundreds of ms. HL quotes that are stale relative to CEX fair value can be taken profitably. |
| Data | `bbo` for HL perps + `binance-usdm` + `bybit-linear` for BTC, ETH, SOL, HYPE |
| Part 1 — lead/lag | Resample mids to a 10 ms grid; cross-correlate log returns at lags −2 s…+2 s; report the peak lag and correlation per pair and per day. |
| Part 2 — stale quotes | Fair HL price = CEX mid + rolling 5-min median basis (HL − CEX). Episode when HL ask < fair − fees − buffer (buy) or HL bid > fair + fees + buffer (sell). HL taker fee only; the hedge on CEX is **out of scope for execution**, so report both "unhedged, exit on HL at +5 s" PnL and "hedged at CEX taker" PnL. |
| Note | This study is the most latency-sensitive; the latency grid is essential. It also measures our structural latency disadvantage (network path from the V-4 host). |

### O6 — Liquidation and large-flow events (task S-6)

| Item | Detail |
|---|---|
| Hypothesis | Large aggressive bursts (often liquidations) push HL prices away from fair value, and prices partly revert within seconds, so providing liquidity after a burst, or fading it, is profitable. |
| Data | `trades`, `bbo`, `ctx` (OI drops), CEX `bbo` as the fair reference |
| Event detection | A burst = same-side aggressive volume in 1 s ≥ k × rolling 1 h p99 of 1 s volume (k = 3 default) **and** mid move ≥ 10 bps. If V-1 confirms `users` in trades, tag bursts whose taker address is a known liquidator/HLP address (list in `research/mappings/addresses.toml`, built in V-1). |
| Measurement | Mid path relative to CEX fair at +1, +5, +30, +60 s after the event. PnL of a taker fade entering at `+L` and exiting at each horizon, net of 2 × taker. |
| Note | This is a signal study; its "episodes" are events. Use §13.5 metrics with one fade trade per event. |

### O7 — Funding carry baseline (task S-7)

| Item | Detail |
|---|---|
| Hypothesis | Delta-neutral spot-long/perp-short (or the reverse) on HL earns funding above round-trip costs. This validates or kills the M4 pilot, and sets a **risk-free-ish hurdle** the arb strategies must beat. |
| Data | `funding_hist` (backfilled as far as REST allows), `ctx`, `bbo` for spot/perp pairs, REST `predictedFundings` |
| Method | For each coin with a spot leg: simulate "enter when the trailing 24 h avg funding annualizes above X%, exit when it falls below Y% for N consecutive hours" over a grid of X/Y/N. Round-trip cost from `costs.toml` (maker and taker variants). Report net APR, max drawdown of the spread, and time in position. |
| Extra | Report the HL vs Binance/Bybit funding differential from `predictedFundings` (cross-venue carry, measured only; not built). |
| Not latency-sensitive | Report at L = 1000 ms only. |

### O8 — HyperEVM MEV feasibility (task S-8)

| Item | Detail |
|---|---|
| Hypothesis | HyperEVM allows profitable backrunning or block-state arbitrage between DEX pools. |
| Nature | Mostly **desk research + small measurements**, answering the questions below. It produces a report with the standard verdict. |
| Questions | (1) Are pending txs visible (public mempool, node gossip, per-RPC)? (2) How is order within a block decided (priority fee, arrival, other)? (3) Are there private relays/builders? (4) Priority-fee economics: burned or paid to someone? (5) Pool-vs-pool dislocations between HyperEVM DEXes per block (use `evm_pools`, same §13 method with pool fees + gas). |
| Prereq | V-5, V-6, and R-9 for question (5) |

### O9 — Options positioning → HIP-3 stock perps and crypto bluechips (task S-9)

The owner's thesis: tokenized-stock perps on HIP-3 dexes (and BTC/ETH on the main dex) move with the broad market and with their options chains, so options positioning can tell us which way to lean. This is a **directional, slow-signal** strategy family, not arb, and it's judged with the §13.8 method.

| Item | Detail |
|---|---|
| Hypothesis | Options positioning in the underlying (put/call imbalance, skew, open-interest walls near expiry; for crypto, Deribit IV and skew) predicts the underlying's direction or pinning over hours to days, and trading the matching HL perp captures that net of fees and funding. |
| Universe | HIP-3 stock perps whose underlying has a liquid US options chain (e.g. `xyz:TSLA` ↔ TSLA; an index perp ↔ QQQ/SPY), plus BTC/ETH (Deribit). Built in V-9 into `research/mappings/underlyings.toml`, e.g. `TSLA = { hl = ["xyz:TSLA"], options = "TSLA", equity = "TSLA" }`. |
| Data | `options_expiry`, `options_strikes` (finsnap), `deribit_options`, `bbo` + `ctx` for the mapped perps, `equity_quotes` (the underlying's real price), `funding_hist` |
| Pre-registered signals (v1) | **P1 wall pinning**: within 2 trading days of a large expiry, if spot is more than 1σ (the expiry's strike std) from the dominant OI strike, lean toward that strike; exit at expiry. **P2 skew extreme**: `skew_score` 60-day z-score beyond ±2 ⇒ contrarian position for H; the momentum sign is also tested and both are reported. **P3 crypto IV skew** (Deribit): 25-delta risk-reversal z-score beyond ±2 ⇒ contrarian BTC/ETH perp position. **P4 market regime**: SPY/QQQ positioning label (finsnap) as a filter on P1–P3 (trade only when the index label agrees). |
| Data history | O9a: as much history as finsnap's `option_snapshots` + V-13 purchases provide (target ≥ 2 years for a final O9a report). O9b: forward-collected; preliminary after ≥ 20 US trading days, final after ≥ 60 trading days **and** ≥ 30 out-of-sample signals per variant. |
| HIP-3 specifics | Stock perps trade 24/7, but the underlying and its options trade only in US hours. Oracle, funding, fees, and leverage per dex come from V-12. Entry/exit prices outside US hours must use the HL perp price only. |
| Owner prior | The owner has used options positioning in discretionary stock trading and saw it work, but it hasn't been tested quantitatively. finsnap's "context, never signal" label is a legal-style disclaimer, not a finding. O9 is the quantitative test. |
| Two stages | **O9a (history, now):** test the signals on the **real stock** (as a proxy for the HL perp, which tracks the stock during US hours), using finsnap's stored `option_snapshots` (daily, per contract, since finsnap started collecting) plus bought history if V-13 approves it, and stock bars from the V-11 provider. This can produce a verdict in days, not months. **O9b (forward):** the same pre-registered signals on the actual HL perps with HL prices, fees, and funding, confirming the edge transfers (off-hours behavior, funding drag). |

### O10 — HIP-3 stock perps vs the real stock (task S-10)

| Item | Detail |
|---|---|
| Hypothesis | HIP-3 stock perps are priced off the real stock during US hours and trade on their own the rest of the time. That creates (A) short-lived dislocations vs the live stock price, (B) predictable convergence at the US open after nights/weekends, and (C) persistent funding/premium patterns. |
| Part A: market-hours dislocation | Fair = equity mid (`equity_quotes`) + rolling 5-min median basis. Episodes per §13.3 when the HL perp is through fair by more than HL taker + buffer. HL-only execution; a hedge in the stock needs a brokerage, so it's measured only (like O5). **Latency grid applies.** |
| Part B: open convergence | For every US open after a closed period (overnight, weekend, holiday): compare the perp's last price before 09:30 ET with the stock's opening print. Measure whether the perp's closed-hours move over- or under-shoots, overall and relative to the options-implied move (IV from F-1 or Deribit-style straddle pricing). Pre-registered trade: fade closed-hours perp moves larger than k × implied move (k ∈ {1, 1.5, 2}) shortly before the open, exit after the open. §13.8 method. |
| Part C: funding and premium | Distribution of HIP-3 stock-perp funding and perp-vs-stock premium by session (regular / pre / post / closed / weekend). Report whether a carry-like pattern clears costs. |
| Part D: closed-hours lead-lag (arb-like, both directions) | When US markets are closed (nights, weekends, holidays), the only live prices are HL perps. BTC/ETH and index perps move first, and single-stock perps with high market or crypto beta (e.g. COIN, MSTR, HOOD, NVDA, TSLA) may lag. Estimate each stock perp's rolling beta to BTC and to the index perp in closed sessions; fair = stock-perp last + β × (driver return since); episodes per §13.3 when the stock perp is through fair by more than fees + buffer. Long **and** short. **Latency grid applies.** |
| Part E: closed-hours move → next-session / week-ahead edge | Pre-registered: signal = the stock perp's closed-period return (Fri US close → Mon pre-open for weekends; US close → next open for overnights), optionally scaled by the options-implied move. Trades: (i) on the perp from Sunday evening / late night into the US open (continuation vs fade, both reported); (ii) on the perp from the US open over horizons 1 d, 3 d, 5 d (weekend) or 1 d (overnight). §13.8 method. History: HL `candleSnapshot` 1h candles for HIP-3 perps since listing (R-5), plus forward data. |
| Owner intent | The owner expects an edge in weekend/overnight information for the following session/week and is willing to run a small-to-medium pilot on MARGINAL evidence (§13.6 pilot allowance). |
| Data | `bbo`, `ctx`, `bars` for HIP-3 stock perps, BTC/ETH, and index perps; `equity_quotes`; `options_*` (for Part B/E implied moves); HIP-3 oracle updates from the node (SPEC-0009, optional) |
| Prereq | V-11 + R-13 for Parts A/B; Parts C, D, E need only HL data (+ options for the implied-move variants) |

### O11 — Bollinger-band mean reversion, arb-style (task S-11)

Prior: in finsnap's backtests across its whole universe, **Bollinger Reversion** (20-period SMA ± 2σ / 3σ; buy a close at or below the lower band, sell a close at or above the upper band; next-bar-open fills) had the best historical average of all 20 strategies. That was on daily bars of US ETFs. O11 tests whether the same idea (price stretched ~2σ from its rolling mean tends to revert) pays on Hyperliquid at arb-like speeds and timeframes.

| Item | Detail |
|---|---|
| Part A: spread bands (stat-arb, market-neutral; the arb version) | Apply Bollinger bands to the **log-spread** between related instruments, instead of to one price: HIP-3 cross-dex pairs (O1), spot vs perp (O3), stock perp vs real stock (O10 A), stock perp vs index perp (β-hedged), BTC vs ETH (β-hedged). Enter when the spread closes beyond ±k σ of its rolling mean; exit at the middle band (variant 1) or the opposite band (variant 2, finsnap's rule). k ∈ {2, 3}; window N ∈ {20, 50, 100} bars; bars ∈ {1 s, 10 s, 1 m, 5 m}. Both legs taker (plus a maker-entry variant: rest at the band). Extends O1/O3/O10 A: the rolling middle band absorbs a persistent basis automatically. **Latency grid applies** (for the sub-minute bars). |
| Part B: single-instrument intraday bands (directional) | finsnap's exact rule (20/2σ and 20/3σ) on HL perps (BTC, ETH, SOL, HYPE, HIP-3 stock perps), bars ∈ {1 m, 5 m, 15 m, 1 h}, variants: long-only (finsnap) and long/short. Include weekend/overnight sessions for stock perps and report them separately. §13.8 method with bar-close signals and next-bar-open fills. Costs dominate at short bars: report taker and maker-entry variants. |
| Part C: as a filter | Bollinger bandwidth (squeeze) and %B as pre-registered filters on O9 and O10 E signals (e.g. only fade a closed-hours move when %B is beyond 0/1). Reported inside those studies. |
| Data | `bars` (from `bbo`/`trades` + `candleSnapshot` backfill), `bbo` for the spread legs, `equity_quotes` (for stock-vs-perp spreads), `funding_hist` |
| History | Part B 1h: HL candle history (months). Parts A and B sub-hour: forward-recorded bars. Stock-proxy pre-check: the V-11 provider's multi-year stock minute bars can test Part B's rule on the underlying stocks right away (like O9a). |
| Pre-registration | Parameters above are the complete grid, fixed now. Report every cell; the headline is the out-of-sample result of the cell chosen on the in-sample 60%. |

---

## 14. Work breakdown

Status: ☐ not started · 🔄 in progress · ✅ done. Size: **S** ≤ ½ day, **M** ≤ 2 days, **L** ≤ 5 days (for a focused agent).

### 14.1 Summary table

| ID | Title | Size | Depends on | Status |
|---|---|---|---|---|
| V-1 | Verify HL WS/REST facts | S | — | ☐ |
| V-2 | Verify CEX endpoints, fields, fees, reachability | S | — | ☐ |
| V-3 | Verify HIP-3 fees and the spot quote-token set | S | — | ☐ |
| V-4 | Measure latency from candidate regions; pick a host | M | R-6 (`hl probe latency`) | ☐ |
| V-5 | Verify HyperEVM facts (blocks, mempool, gas, Core↔EVM transfers) | M | — | ☐ |
| V-6 | Build the HyperEVM pool list | M | V-5 | ☐ |
| V-7 | Measure data volume per stream | S | R-6 | ☐ |
| V-8 | Check the HL public S3 archive for backfill | S | — | ☐ |
| V-9 | finsnap integration + HIP-3 stock universe mapping | S | — | ☐ |
| V-10 | Verify the Deribit public API (endpoints, fields, limits, reachability) | S | — | ☐ |
| V-11 | Choose a real-time US equities data provider (owner approves) | S | — | ☐ |
| V-12 | HIP-3 stock-perp mechanics (oracle in/out of hours, funding, fees, leverage, halts) | S | — | ☐ |
| V-13 | Historical options data: vendors, coverage, cost; owner decides whether to buy | S | V-9 | ☐ |
| F-1 | **finsnap repo:** keep IV/bid/ask/last in options data; add HIP-3 underlyings to the options universe | M | V-9 | ☐ |
| R-1 | `mev-recorder` crate skeleton + envelope types | S | — | ☐ |
| R-2 | Segment writer (zstd, rotation, manifest, crash recovery, disk guard) | M | R-1 | ☐ |
| R-3 | Extract `RawWsConn` (watchdog, jitter, cancel, gap events); rebase `WsMarketStream` on it | M | — | ☐ |
| R-4 | Subscription planner + universe selectors | M | R-1 | ☐ |
| R-5 | HL REST snapshotter with weight budget | M | R-1, R-2 | ☐ |
| R-6 | `hl record` / `record plan` / `probe latency` CLI, profiles, metrics, health | M | R-2, R-3, R-4, R-5 | ☐ |
| R-7 | Segment reader + `hl record inspect` / `verify` | S | R-2 | ☐ |
| R-8 | Binance/Bybit sources | S | R-3, R-6, V-2 | ☐ |
| R-9 | HyperEVM pool source | L | R-6, V-5, V-6 | ☐ |
| R-10 | Deploy recorder (systemd, chrony, runbook, optional shipping) | M | R-6, R-7, V-4 | ☐ |
| R-11 | `finsnap` poller source | S | R-5, V-9 | ☐ |
| R-12 | `deribit` options summary source | S | R-5, V-10 | ☐ |
| R-13 | `equities` real-time quote source | M | R-3, V-11 | ☐ |
| P-1 | `research/` scaffold + segment reader in Python | S | R-2 (format frozen) | ☐ |
| P-2 | Normalizer → Parquet tables (§13.1) | M | P-1, V-1 | ☐ |
| P-3 | Cost model module + `costs.toml` | S | P-1, V-2, V-3 | ☐ |
| P-4 | Episode detector + latency capture (§13.3–13.5) | M | P-2, P-3 | ☐ |
| P-5 | Report template + `RANKING.md` generator | S | P-4 | ☐ |
| P-6 | Slow-signal backtester (§13.8) | M | P-2, P-3 | ☐ |
| S-1 | Study O1 HIP-3 dislocations | M | P-5 | ☐ |
| S-2 | Study O2 spot triangles | M | P-5 | ☐ |
| S-3 | Study O3 spot-perp dislocation | S | P-5 | ☐ |
| S-4 | Study O4 Core↔EVM | L | P-5, R-9 | ☐ |
| S-5 | Study O5 CEX lead-lag | M | P-5, R-8 | ☐ |
| S-6 | Study O6 liquidation/flow events | M | P-5 | ☐ |
| S-7 | Study O7 funding carry | M | P-5 | ☐ |
| S-8 | Study O8 HyperEVM MEV feasibility | M | V-5, V-6 (R-9 for Q5) | ☐ |
| S-9 | Study O9 options positioning (O9a history on stocks now; O9b forward on HL perps) | L | P-6, V-9 (O9a); + R-11, R-12, V-12, ≥ 20 trading days (O9b) | ☐ |
| S-10 | Study O10 HIP-3 stock perps vs the real stock (Parts A–E) | L | P-4, P-6, V-12 (R-13 for A/B) | ☐ |
| S-11 | Study O11 Bollinger mean reversion (spread + single-instrument) | M | P-4, P-6, R-5 candle backfill | ☐ |
| D-1 | `RANKING.md` + ADR-0002 + first strategy spec stub | S | all S-tasks that are feasible | ☐ |

**Critical path to "recording in production":** R-1 → R-2 → R-4/R-5 (parallel with R-3) → R-6 → V-4 → R-10. Get this done first; the research tasks can start once a few days of data exist.

**Suggested parallel lanes for multiple agents:**

| Lane | Tasks in order |
|---|---|
| A (recorder core) | R-1 → R-2 → R-5 → R-7 |
| B (connectivity) | R-3 → R-4 → R-6 → R-8 |
| C (facts) | V-1, V-2, V-3, V-8, V-5 → V-6 |
| F (options & equities) | V-9, V-10, V-11, V-12, V-13 → F-1 (in `../finsnap`) → R-11, R-12, R-13 → start collecting early: O9b needs weeks of forward data; O9a can start as soon as P-6 exists |
| D (ops) | V-4 → R-10 → V-7 |
| E (research, after ~3 days of data) | P-1 → P-2 → P-3 → P-4 → P-5 → S-* |

### 14.2 Task details

Every task also has these implicit **Done when** items: `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace` pass (Rust tasks), or `uv run ruff check` and `uv run pytest` pass (Python tasks); the §14.1 status is updated; and one conventional commit per task (e.g. `feat(recorder): segment writer with zstd rotation`).

#### V-1 — Verify HL WS/REST facts
- **Do:** Using the official Hyperliquid API docs **and** a live connection (`hl watch`, or a small throwaway script in the scratchpad, not committed), confirm or correct every "⚠ verify (V-1)" row in §7.1 and §8: idle-close window, `l2Book` depth/params/cadence, `trades.users`, `allMids` with `dex`, the spot ctx channel name, the `predictedFundings` response shape. Also build `research/mappings/addresses.toml` with any documented system addresses (HLP, liquidator), each with a source link.
- **Files:** `specs/SPEC-0008-…md` §15, `research/mappings/addresses.toml`.
- **Done when:** every V-1 row in §15 has a value, a source (doc URL or "observed live on DATE"), and the date; any spec text that turned out wrong is corrected.

#### V-2 — Verify CEX endpoints, fields, fees, reachability
- **Do:** Confirm the §9 URLs, subscribe formats, field names, keepalive rules, and the VIP0 fees in §13.2. From the intended host region (or state that it's unknown), confirm the endpoints are reachable and not geo-blocked.
- **Done when:** the §15 rows are filled in with sources; §9/§13.2 are corrected where needed.

#### V-3 — Verify HIP-3 fees and the spot quote-token set
- **Do:** Find how HIP-3 taker/maker fees are set (per dex? a multiplier on base?), and record the values for every dex listed by `hl dexs`. List every spot quote token currently used in `spotMeta` (e.g. USDC, USDT0, others) using `hl markets --spot`.
- **Done when:** the §15 rows are filled in; `research/costs.toml` values are ready for P-3.

#### V-4 — Measure latency and pick a host
- **Do:** Run `hl probe latency --count 50` from at least 2 candidate regions (Tokyo required; one other, e.g. Singapore or Frankfurt). Record p50/p90 for each measurement. Recommend a region and provider.
- **Done when:** §15 has a latency table per region, and §12.3 is updated with the chosen host.

#### V-5 — Verify HyperEVM facts
- **Do:** From the docs plus measurement, answer: block types and cadence (small/large blocks), gas pricing (base fee, priority fee, burned or paid), mempool visibility, how to move HYPE and other tokens between Core and EVM (system addresses, delay, cost), and whether `CoreWriter` delays apply to transfers. Every answer needs a source.
- **Done when:** §15 has a HyperEVM block; §10 and §13.9 O4/O8 are corrected where needed.

#### V-6 — Build the HyperEVM pool list
- **Do:** Identify the main HyperEVM DEXes and, for each token that also trades on HyperCore spot (HYPE first), the deepest pools. Write `config/hyperevm-pools.toml` (schema in §10) with source links as comments. Check each address on-chain with an `eth_call` (`token0()`, `token1()`, `fee()`, or `getReserves()`).
- **Done when:** the file exists with ≥ 5 pools, every one verified on-chain, and the verification commands are noted in the task commit message.

#### V-7 — Measure data volume
- **Do:** Run the recorder with the default profile for ≥ 6 hours and report MB/hour raw and compressed per `src` and per channel (use `hl record inspect`). Extrapolate per day and per 30 days.
- **Done when:** §15 has the volume table; the §12.3 disk sizing is confirmed or updated.

#### V-8 — Check the HL public S3 archive
- **Do:** Hyperliquid is believed to publish historical data to a requester-pays S3 bucket (e.g. `s3://hyperliquid-archive/…`, possibly also node data buckets) ⚠ unverified. Confirm the bucket(s), the paths, the data types (l2Book snapshots? asset ctxs? fills?), the formats (lz4?), the update lag, and the cost. Download one sample file per data type into the scratchpad and describe it.
- **Done when:** §15 has an "S3 archive" block; if usable, add a follow-up task `P-6 backfill importer` to §14 (don't implement it now).

#### V-9 — finsnap integration + HIP-3 stock universe mapping
- **Do:** Read `../finsnap` (its `AGENTS.md`, `apps/backend/src/collectors/options.ts`, `analyzers/options.ts`, the `/snap` route). Document the `/snap` JSON shape for options (field names, as-of timestamps), the data delay of the Yahoo source, how to run finsnap next to the recorder (Docker Compose), and its options universe (`OPTIONS_SYMBOLS`). List every HIP-3 stock/index perp (`hl dexs`, `hl markets --dex …`), and map each one to its underlying stock/ETF and options symbol in `research/mappings/underlyings.toml`.
- **Done when:** §15 has the finsnap block; the mapping file covers every HIP-3 stock perp with a liquid US options chain (or marks it "no chain").

#### V-10 — Verify the Deribit public API
- **Do:** Confirm the §9.1 endpoints, response fields (`mark_iv`, `open_interest`, `volume`, `underlying_price`, `instrument_name` format), rate limits, and reachability from the recorder host.
- **Done when:** §15 row filled in with sources.

#### V-11 — Choose a real-time US equities data provider
- **Staged plan (owner-approved direction, 2026-09-26):** start free, pay when a study shows it matters.
  - **Stage 0 (now, free):** Yahoo via finsnap for slow/daily work (delayed, unofficial); Deribit for crypto options.
  - **Stage 1 (free real-time):** a free real-time WebSocket feed. The leading candidate is Alpaca's free market-data tier (real-time, but IEX exchange only, a small share of total volume) ⚠ verify. Or the owner's **brokerage API**, if it offers real-time quotes and option chains to account holders (e.g. Schwab, Interactive Brokers, Tradier) ⚠ verify which broker the owner uses.
  - **Stage 2 (paid, when O10 Part A or O11 Part A needs full-market prices):** a consolidated (SIP) real-time feed, e.g. Alpaca's paid tier, Polygon.io, or Databento. Prices and tiers ⚠ verify.
- **Do:** For each candidate, check coverage (IEX-only vs consolidated), real-time vs delayed, pre/post-market and overnight-session coverage, WebSocket support, historical minute bars (years available: needed by O9a/O11 stock-proxy checks), cost, and whether the terms allow storing data for research. Recommend a Stage 1 choice and a Stage 2 choice.
- **Done when:** §15 has the comparison; the owner has approved the Stage 1 choice (record the approval date).

#### V-13 — Historical options data
- **Why:** O9a can test options signals on years of history instead of waiting months for forward collection. That requires past option chains (per strike: open interest, volume, and ideally IV) that neither Yahoo nor finsnap has before finsnap's start date.
- **Do:** (1) Measure what finsnap already has: date range and symbols in its `option_snapshots` table. (2) Compare ≥ 3 vendors (e.g. ThetaData, ORATS, CBOE DataShop, Polygon.io options, Databento OPRA, historicaloptiondata.com) on: symbols (the HIP-3 underlyings + SPY/QQQ), depth of history, daily vs intraday, fields (OI, volume, IV, greeks), format, and price. (3) Recommend buy / don't buy with a cost.
- **Done when:** §15 has the finsnap coverage and the vendor table; the owner's decision is recorded.

#### V-12 — HIP-3 stock-perp mechanics
- **Do:** For each HIP-3 dex listing stock/index perps: how the oracle/mark is set during US regular hours, pre/post market, overnight, and weekends; funding formula and cadence; fees (links to V-3); max leverage; trading halts and behavior around corporate actions (splits, dividends, earnings). Sources required.
- **Done when:** §15 has a per-dex table.

#### F-1 — finsnap: richer options data (in the `../finsnap` repo)
- **Do:** In the finsnap repo, following **its** `AGENTS.md`: keep `impliedVolatility`, `bid`, `ask`, `lastPrice` per contract in the options collector/store (the Yahoo response already carries them; `normalizeContracts` drops them today), expose them in the `/snap` options payload, and add the HIP-3 underlyings from V-9 to the options universe. Commit in the finsnap repo, not here.
- **Done when:** finsnap tests pass, and `/snap` shows IVs for the new symbols. Record the finsnap commit SHA in §15.

#### R-1 — `mev-recorder` crate skeleton + envelope
- **Do:** Create `crates/mev-recorder` (add it to the workspace `members`). Define `Envelope` (§5.1) and `Kind` (§5.3) with `serde`, plus constructors that take `t_ns`/`mono_ns` from an injectable clock (reuse the `mev_core::clock::Clock` pattern; add a monotonic-ns source). `raw` is stored as `String` and serialized as a JSON string.
- **Files:** `Cargo.toml` (workspace), `crates/mev-recorder/{Cargo.toml,src/lib.rs,src/envelope.rs}`.
- **Tests:** golden serialization for every kind; a round-trip test; a test that `raw` containing quotes, newlines and unicode survives the round-trip byte-exact.
- **Done when:** the crate builds, tests pass, and there are no new deps beyond `serde`, `serde_json`, and workspace crates.

#### R-2 — Segment writer
- **Do:** Implement `SegmentWriter`: a dedicated OS thread (same pattern as `mev_core::db::writer::DbWriter`) that receives envelopes over a bounded `sync_channel` (capacity configurable, default 65 536), writes `segment_open` first, compresses with `zstd` (add the `zstd` crate to workspace deps), flushes every ≤ 5 s, and rotates per §6 (hour boundary or 1 GiB raw). Finalize and write the manifest per §6. Crash recovery for `.partial` per §6. Disk guard per §6. Expose `try_send(env) -> bool`; on `false` the **caller** increments `hl_rec_dropped_total` and sends a `gap_start{reason:"drop"}` as soon as the channel accepts again.
- **Tests (tempdir):** rotation at an hour boundary using an injected clock; rotation on size; the manifest line matches the file; a `.partial` left from a simulated crash becomes `.crashed` on restart; records written ≡ records read back (with R-7's reader, or a minimal decoder in the test).
- **Done when:** tests pass; a benchmark note in the commit message shows ≥ 50k envelopes/s written on the dev machine.

#### R-3 — `RawWsConn`
- **Do:** Implement §7.5 in `crates/mev-hl-client/src/raw_ws.rs`. Make the URL, subscribe payloads, and keepalive message pluggable via a small `Protocol` trait (HL / Binance / Bybit implementations come later; ship HL now). Rebuild `WsMarketStream` on top of `RawWsConn` (it decodes the `Text` events with the existing `decode`). Add `rand` for jitter if it's not already present.
- **Tests (local mock WS server):** reconnect after the server closes; resubscribe order preserved; the watchdog fires when the server goes silent; a `Gap` then `Opened` is emitted; cancellation stops a reconnect loop mid-backoff; all existing `ws.rs` tests still pass.
- **Done when:** tests pass and `hl watch BTC` still works manually.

#### R-4 — Subscription planner
- **Do:** Implement the §7.3 selectors and the §7.4 algorithm in `crates/mev-recorder/src/planner.rs`. Input: profile + `AssetMap` + the ctx data needed for `top:N` (pass it in; the planner does no I/O). Output: a `Plan` with a pretty table `Display`.
- **Tests:** fixture `AssetMap`s → golden plans; the over-budget drop order; the priority-1 failure; `l2Book` isolation; determinism (shuffled input ⇒ same plan); pacer math (≤ 20 msg/s).
- **Done when:** tests pass.

#### R-5 — HL REST snapshotter
- **Do:** Implement §8 in `crates/mev-recorder/src/sources/hl_rest.rs`: a task that schedules each request at its cadence through a weight token bucket (300/min default), records `rest` envelopes (raw body + `meta.req/status/latency_us`), and tracks `fundingHistory` paging state (the last fetched time per coin, persisted in a small JSON state file in `out_dir`).
- **Tests:** `wiremock` `/info` → envelopes contain the raw bodies; the token bucket delays over-budget requests; paging resumes from state after a restart.
- **Done when:** tests pass.

#### R-6 — CLI, profiles, metrics, health
- **Do:** Add the `record` (with `plan`) and `probe latency` subcommands to `crates/mev-bot/src/main.rs` (move the recorder wiring into a new `crates/mev-bot/src/record.rs` module to keep `main.rs` manageable). Load `config/record.toml` (create it from §7.3) via `figment`, overridable by `HL_RECORD_*` env vars. Wire planner → `RawWsConn`s (paced) → `SegmentWriter`; the REST snapshotter; the `clock` envelope task; `/healthz` `/readyz` `/metrics` with the §12.2 metrics; graceful shutdown (SIGTERM ⇒ `gap_start{shutdown}` on every conn, then finalize segments).
- **Tests:** `hl record plan` on a fixture; an integration test with mock WS + mock REST that runs ~2 s and asserts files + manifest exist and contain `segment_open`, `sub`, `frame`, `segment_close`.
- **Done when:** tests pass; a manual 10-minute mainnet run produces readable segments (`hl record inspect`) with no gaps other than startup.

#### R-7 — Segment reader + inspect/verify
- **Do:** `crates/mev-recorder/src/reader.rs`: iterate the envelopes of a file (tolerates a truncated tail in `.crashed` files); merge several files by `(t_ns, conn, seq)`. Implement the `hl record inspect` and `hl record verify` outputs from §12.1.
- **Tests:** a truncated file reads up to the last full line; merge order; `seq` hole detection.
- **Done when:** tests pass.

#### R-8 — CEX sources
- **Do:** `Protocol` implementations for `binance-usdm`, `binance-spot`, `bybit-linear` per §9 (confirmed by V-2). Add a `[cex]` section to the profile. Each venue gets its own `src` directory.
- **Tests:** mock-server tests for the subscribe format and the Bybit ping cadence.
- **Done when:** tests pass; a manual 10-minute run shows ticker frames for every configured symbol.

#### R-9 — HyperEVM pool source
- **Do:** Implement §10 with Alloy (`alloy` provider with the `ws` feature, `sol!` for `IUniswapV2Pair.getReserves`, `IUniswapV3Pool.slot0/liquidity`, and `Multicall3.aggregate3`). One multicall per new block, at that block number. Handle reconnects by emitting gaps (reuse the envelope kinds). Add `enabled=false` by default.
- **Tests:** unit tests for multicall encoding and decoding with fixed vectors; a mock JSON-RPC test for the per-block flow.
- **Done when:** tests pass; a manual 10-minute run against the configured RPC records one state snapshot per block for every pool in the list.

#### R-11 — `finsnap` poller
- **Do:** A source that polls the configured finsnap base URL (`HL_FINSNAP_URL`, profile `[finsnap]` section with the cadence from §9.1, gated to US market hours using an exchange calendar) and records raw responses as `rest` envelopes under `src:"finsnap"`. Reuse the R-5 scheduling code; no weight budget, but back off on errors.
- **Tests:** `wiremock` → envelopes; the market-hours gate on a fixed-clock test (weekday / weekend / holiday).
- **Done when:** tests pass; a 1-day run shows the expected snapshot count.

#### R-12 — `deribit` source
- **Do:** Poll the §9.1 Deribit endpoints every 60 s for the configured currencies; record `rest` envelopes under `src:"deribit"`. Respect V-10's rate limits.
- **Tests:** `wiremock` → envelopes; cadence.
- **Done when:** tests pass; a 1-hour run shows 60 snapshots per currency.

#### R-13 — `equities` source
- **Do:** Implement the V-11 provider as a `RawWsConn` `Protocol` (auth from env, never logged), subscribed to the mapped underlyings; record frames under `src:"equities"`. If the provider only offers REST, poll at its fastest allowed rate and note it.
- **Tests:** mock-server subscribe/auth tests; keys absent from logs.
- **Done when:** tests pass; a 1-session run records quotes for every mapped symbol.

#### R-10 — Deploy
- **Do:** Add `deploy/recorder/hl-recorder.service` and `deploy/recorder/README.md` (host setup: user, dirs, chrony, build/copy the binary, env file with `HL_EVM_WS_URL` if used, enable the service). Optional: `deploy/recorder/ship.sh` for daily sync + retention. Add the "Recorder" section to `RUNBOOK.md`.
- **Done when:** the recorder has run ≥ 48 h on the chosen host with `/readyz` green ≥ 99% of the time, and `hl record verify` shows ≥ 99% coverage for priority-1 streams. Record the start date in §15.

#### P-1 — Research scaffold
- **Do:** Create `research/` with `pyproject.toml` (Python 3.12; deps: `polars`, `duckdb`, `zstandard`, `orjson`, `matplotlib`, `tomli`/stdlib `tomllib`; dev: `pytest`, `ruff`), managed with `uv`. Package `research/hlr/` with `io.py`: `iter_envelopes(path)` and `iter_frames(root, src, date_from, date_to)` following the §5/§6 format (tolerates `.crashed` tails). Add `research/README.md` with setup and a "never import this from production" note. Add `research/data/` to `.gitignore` (the parquet output).
- **Tests:** read a small fixture segment (generated by a Rust test from R-2, or created in Python with `zstandard`), including a truncated one.
- **Done when:** `uv run pytest` passes.

#### P-2 — Normalizer
- **Do:** `hlr/normalize.py` + `uv run hlr-normalize --from DATE --to DATE`: build the §13.1 tables into `research/data/parquet/{table}/date=YYYY-MM-DD/*.parquet`. Idempotent (re-running a date overwrites that partition). Uses the V-1 facts for channel names and shapes.
- **Tests:** fixture frames for each channel → expected rows; gap extraction including `seq` holes and crashed segments.
- **Done when:** tests pass; running it on one real day finishes and `SELECT count(*)` per table looks plausible (numbers in the commit message).

#### P-3 — Cost model
- **Do:** `research/costs.toml` (values from §13.2 as confirmed by V-2/V-3), `research/thresholds.toml` (exactly the §13.6 block), and `hlr/costs.py` with `fee_bps(venue, market_kind, liquidity="taker")`, `buffer_bps`, gas helpers, and a book-walk `slippage_bps(book_levels, side, usd)`.
- **Tests:** fee lookup for each venue/kind; slippage on a synthetic book equals the hand-computed value.
- **Done when:** tests pass.

#### P-4 — Episode detector and latency capture
- **Do:** `hlr/episodes.py`: given aligned input series (as-of joined on `t_ns`), feed-validity masks (from `stale_ms` + `gaps`), and a `net_bps`/`size` function, produce the episode table (§13.3) and the §13.5 metrics for the §13.4 latency grid. Operate on polars frames; no Python loops over rows in the hot part (use `join_asof`, `rle`/run-id tricks).
- **Tests:** synthetic series with known episodes (including ones that cross a gap, merge within `merge_ms`, or end before `+L`) → exact expected episodes and `captured_L`.
- **Done when:** tests pass; a runtime note for one real day of BTC bbo is in the commit message.

#### P-5 — Report template and ranking
- **Do:** `hlr/report.py`: render a §13.7 report skeleton with the metrics table, latency table, per-day chart, and top-10 episodes, given a study's episode table + parameters. `uv run hlr-rank` scans `research/reports/O*.md` front-matter (YAML block with the metrics) and writes `research/reports/RANKING.md` sorted by the §13.6 score.
- **Tests:** rendering on a synthetic episode table; ranking order.
- **Done when:** tests pass.

#### P-6 — Slow-signal backtester
- **Do:** `hlr/signals.py`: implement §13.8 (availability rule, entry/exit at horizons, costs incl. funding, sizing on the capital grid, 60/40 out-of-sample split, the three baselines, metrics, verdict). Studies call it with a DataFrame of pre-registered signals `(t_data, market, direction, variant, exit_rule?)`.
- **Tests:** synthetic price paths with known returns → exact PnL; the availability rule blocks lookahead (a signal with `t_data + delay` in the future is never filled early); the random-baseline p-value on a known-null signal is not significant.
- **Done when:** tests pass.

#### S-1 … S-11 — Studies
- **Do:** Implement the study in `research/studies/o{n}_{slug}.py` (entry point `uv run python -m studies.o{n}_{slug} --from … --to …`) following its §13.9 block exactly, and write the report via P-5. Run it first as **preliminary** (≥ 3 days of data), then **final** (≥ 14 days). O9, the §13.8 parts of O10, and O11 Part B use the data requirements stated in their blocks.
- **Done when:** the final report exists with all §13.7 sections, including the hand-checked sanity section; the verdict is stated; the report front-matter feeds `RANKING.md`.

#### D-1 — Decision
- **Do:** Regenerate `RANKING.md`. Write `specs/decisions/0002-first-arb-strategy.md` (context, the ranking table, the decision, capital and hurdle as set by the owner, rejected alternatives with one-line reasons, consequences for M5/M6/M7). If a study passes (or is the best MARGINAL), create a stub for the first strategy spec at the next free number (`specs/SPEC-0010-<name>.md` or later) with Purpose / Goals / Legging model / Open questions filled from the report. **The owner approves ADR-0002; an agent only drafts it.**
- **Done when:** ADR-0002 is drafted and marked "Proposed", and `docs/GOAL.md` §7 is updated to match.

## 15. Verified facts (filled in by V-tasks)

| Fact | Value | Source | Date | Task |
|---|---|---|---|---|
| HL WS idle-close window | | | | V-1 |
| HL `l2Book` depth / params / cadence | | | | V-1 |
| HL `trades.users` present | | | | V-1 |
| HL `allMids` with `dex` | | | | V-1 |
| HL spot ctx channel name | | | | V-1 |
| HL `predictedFundings` shape | | | | V-1 |
| Binance/Bybit endpoints + fields | | | | V-2 |
| Binance/Bybit VIP0 fees | | | | V-2 |
| HIP-3 fee model + per-dex values | | | | V-3 |
| Spot quote tokens | | | | V-3 |
| Latency by region (p50/p90) | | | | V-4 |
| HyperEVM blocks / gas / mempool / transfers | | | | V-5 |
| Data volume per stream (MB/h raw, zst) | | | | V-7 |
| S3 archive availability | | | | V-8 |
| Recorder production start date + host | | | | R-10 |
| finsnap `/snap` options shape, data delay, run mode | | | | V-9 |
| Deribit endpoints, fields, limits | | | | V-10 |
| Equities provider comparison + owner approval | | | | V-11 |
| HIP-3 stock-perp mechanics per dex | | | | V-12 |
| finsnap `option_snapshots` coverage; options-history vendors; owner decision | | | | V-13 |
| finsnap F-1 commit SHA | | | | F-1 |

## 16. Acceptance criteria

- [ ] The recorder runs ≥ 14 consecutive days on the production host with ≥ 99% coverage on priority-1 streams (`hl record verify`), and every outage is visible as a gap.
- [ ] `hl record plan` proves the default profile stays within every HL WS limit in §7.1.
- [ ] REST weight used by the recorder never exceeds its 300/min budget (metric evidence).
- [ ] The normalizer + studies reproduce identical numbers when re-run on the same files (checked for at least one study).
- [ ] Studies O1, O2, O3, O5, O6, O7, O9a, O10, O11 have final reports; O4, O8, and O9b have final reports **or** a documented reason they are blocked or still collecting (e.g. no RPC access, not enough trading days).
- [ ] `RANKING.md` exists, and ADR-0002 is drafted with a decision.
- [ ] No keys are read by any recorder or research code path (grep-verified in review).

## 17. Open questions

1. ~~Profit hurdle and capital~~ **Resolved 2026-09-26:** small-to-medium capital ($10k–$100k grid), APR target 25% and floor 10%, all adjustable in `research/thresholds.toml` (§13.6).
2. **Shipping and backup** of recordings (bucket, retention). Optional in v1.
3. **HyperEVM RPC provider** for R-9 (commercial provider vs own node), and budget.
4. **Should O5's CEX hedge ever be built?** Holding CEX accounts is a new operational surface (keys, KYC, transfers). Measured only for now.
5. **Maker-leg variants** (fill-probability modeling) are deferred until a taker-taker study looks promising.
6. **Historical options data** (V-13). Buying past option chains lets O9a test years of history now. The owner decides after V-13 shows finsnap's existing coverage and vendor prices.
7. **Directional risk limits.** O9/O10 Part B strategies carry market risk that the arb strategies don't; if one passes, SPEC-0004 needs per-strategy stop-loss and volatility-scaled sizing before it goes live.
