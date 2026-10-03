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
| Envelope + segment writer/reader | `crates/hl-arb-recorder/src/{envelope,segment,reader}.rs` | Rust | R-1, R-2, R-7 |
| Raw WS connection (shared with the bot) | `crates/hl-arb-client/src/raw_ws.rs` | Rust | R-3 |
| HL subscription planner | `crates/hl-arb-recorder/src/planner.rs` | Rust | R-4 |
| HL REST snapshotter | `crates/hl-arb-recorder/src/sources/hl_rest.rs` | Rust | R-5 |
| `hl record` CLI + profiles | `crates/hl-arb-bot/src/main.rs`, `config/record.toml` | Rust | R-6 |
| CEX reference sources | `crates/hl-arb-recorder/src/sources/cex.rs` | Rust | R-8 |
| HyperEVM pool source | `crates/hl-arb-recorder/src/sources/evm.rs` | Rust | R-9 |
| Deployment | `deploy/recorder/`, `RUNBOOK.md` | systemd / docs | R-10 |
| Research toolkit | `research/hlr/` | Python | P-1…P-5 |
| Studies + reports | `research/studies/`, `research/reports/` | Python / Markdown | S-1…S-23 |

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

**Gap timestamps (R-8 fix2).** A `gap_start` is stamped with the wall-clock and monotonic time the disconnect was **detected** (`Envelope::gap_start_at`/`new_at`), not the time the line was written, and `gap_end` with the reopen time; `meta.gap_ms = gap_end.t_ns - gap_start.t_ns`. Envelope `seq` order is still the write order, so a `gap_start`'s `t_ns` can be **earlier** than the preceding frame's (the frame was stamped when the recorder processed it, the gap at the drop). A `shutdown` `gap_start` is unpaired by design (the run ends there). A writer stopped by the disk/mount guard also emits a `gap_start` (§6, §6.1).

**Reading a gap.** Readers close an open gap at the next `gap_start`, `gap_end`, or data envelope (`frame`, `frame_bin`, `rest`) on the same `(src, conn)`, so a restart after an unpaired `shutdown` gap is covered from its first data record rather than the whole day being missing. Rust `reader::CoverageAcc` and Python `_GapTracker` (`research/hlr/normalize.py`) implement the same rule. A final unpaired gap is closed at the last record seen on the stream; how a `drop` gap ends remains the §17 open question (#24).

### 5.4 Source ids

| `src` | What | Task |
|---|---|---|
| `hl-ws` | Hyperliquid WebSocket market data | R-3, R-4 |
| `hl-rest` | Hyperliquid `/info` snapshots | R-5 |
| `binance-usdm` | Binance USDⓈ-M futures `bookTicker` | R-8 |
| `binance-spot` | Binance spot `bookTicker` | R-8 |
| `bybit-linear` | Bybit v5 linear `orderbook.1` | R-8 |
| `hyperevm` | HyperEVM block headers + pool state | R-9 |
| `yahoo-options` | Option chains for mapped underlyings, every field (OI, volume, IV, bid, ask, last) | R-11 |
| `finsnap` | Optional: the owner's finsnap view (`/snap` derived labels), recorded for comparison only | R-11 |
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

**Manifest appends are serialized (R-2b).** Every finished segment appends one line to its day's `manifest.jsonl`, and several writer threads (one per `(src, conn)`) append to the same file with independently-opened `O_APPEND` handles. On the recorder's WSL 9p `drvfs` mount (`/mnt/e`) that is **not** atomic: the 9p client caches the file size, so two writers that open at the same instant append at the same offset and a line is lost with no error (5 of 56 finalized segments in the 6.25 h V-4 run had no manifest line). The recorder now holds a process-wide, per-path lock across the whole open→write→fsync of each append (`crates/hl-arb-recorder/src/segment.rs::manifest_append_lock`), so at most one handle appends at a time. `hl record verify` trusts the manifest: a segment whose line is missing is invisible to it and coverage is undercounted (§17 #36).

### 6.1 Mount guard (R-14)

A profile may pin recording to an external drive. `require_mount` (an absolute path) and the optional `require_mount_source` (the mount's source from `/proc/self/mountinfo`, e.g. `'E:\'`, matched case-insensitively ignoring a trailing separator) are checked at startup and re-checked before every guarded write, so a disconnected drive cannot silently send writes to the root disk. See `crates/hl-arb-recorder/src/mount_guard.rs`, `config/record-ssd.toml`, and `deploy/recorder/run-ssd.sh`.

- **Startup:** the recorder refuses to start unless `require_mount` is a real mount point on a device other than `/` and `out_dir` is strictly inside it, before and after canonicalization (a symlink or `..` cannot escape). `validate_startup` creates nothing.
- **Guarded write paths:** segment create (directory + `.partial`), segment finalize (`fsync` + rename + manifest), manifest append, crash recovery (`.partial` → `.crashed` rename + manifest), the REST paging state file, and `out_dir` creation — a single `create_dir` of a **direct child** of the verified mount, never `create_dir_all` along a path that may have become the root disk.
- **Watchdog:** the writer re-checks the mount every 2 s (`MOUNT_RECHECK_INTERVAL`) even when no segment rotates, and the async path probes on a blocking thread under a 2 s timeout; a hung probe counts as a lost mount.
- **Fail-closed:** on the first failed check the guard trips and the stream stops. The open segment is dropped **without** finalizing (finalization is path-based, so the `.partial` is left for crash recovery on the next start), and `hl record` exits non-zero. `/readyz` reports not-ready as soon as the guard trips.
- **External watchdog:** `deploy/recorder/run-ssd.sh` refuses to `start` against anything but `/mnt/e`, and its 1 s watchdog SIGTERMs (then SIGKILLs) the recorder if `/mnt/e` stops being the E: `drvfs`/9p mount.

## 7. Hyperliquid WebSocket recording

### 7.1 Protocol facts

| Fact | Value | Status |
|---|---|---|
| Mainnet WS URL | `wss://api.hyperliquid.xyz/ws` (`Network::ws_url()`) | known |
| Subscribe message | `{"method":"subscribe","subscription":{…}}` | known (in `ws.rs`) |
| App-level ping | send `{"method":"ping"}`, expect `{"channel":"pong"}` | known |
| Server idle close | closes a connection it has not **sent** a message to for ~60 s; app-level `{"method":"ping"}` (→ `{"channel":"pong"}`) resets the timer | known (V-1) |
| Limits per IP | ≤ 10 WS connections, ≤ 30 new connections/min, ≤ 1000 subscriptions, ≤ 2000 client→server messages/min | known (SPEC-0001 §5) |
| `l2Book` depth | up to 20 levels per side; optional `nSigFigs` (2–5 or `null`) and `mantissa` (1/2/5, only when `nSigFigs=5`) aggregate levels; `fast:true` returns 5 levels | known (V-1) |
| `l2Book` cadence | default 20-level snapshot pushed on change (observed ~2.4–6.6 s for BTC); `fast:true` (5 levels) observed ~0.5 s (docs state a ≥ 0.5 s push bound) | known (V-1) |
| `bbo` | pushed only when best bid/offer changes on a block | known (SPEC-0001) |
| `trades` payload | includes `users: [buyer, seller]` addresses (WS `WsTrade`; REST `recentTrades` too) | known (V-1) |
| `allMids` HIP-3 | accepts `"dex": "<name>"` to get mids for a HIP-3 dex (WS and REST); spot mids only with the first perp dex | known (V-1) |
| `activeAssetCtx` for spot | spot coins answer on channel `activeSpotAssetCtx` (data `{coin, ctx}`), not `activeAssetCtx`; the subscription coin may be `@123` or `BASE/QUOTE` | known (V-1). The recorder stores raw frames, so this only matters for research normalization. |

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
http_port = 9091                 # kept separate from the bot's 9090

[profile.default.hl]
bbo              = ["perps:all", "hip3:all", "spot:top:40", "spot:quotes"]
trades           = ["perps:top:40", "hip3:all", "spot:top:20"]
active_asset_ctx = ["perps:all", "hip3:all"]
all_mids         = true          # main dex + every HIP-3 dex
l2book           = ["BTC", "ETH", "SOL", "HYPE"]
max_subs         = 950           # ~934 requested; 50 of the 1000/IP limit as headroom
subs_per_conn    = 150
connections      = 8             # of 10; leaves 2 for ad-hoc tools / the bot
allow_truncate   = false         # refuse to drop priority-1 subscriptions

[profile.default.rest]
enabled = true
# see §8 for the request list and cadences
weight_per_min = 300             # a quarter of the 1200/IP budget

[profile.default.deribit]
enabled    = false               # R-12; public Deribit options summaries (no keys)
currencies = ["BTC", "ETH"]
base_url   = "https://www.deribit.com/api/v2"

[profile.default.cex]
binance_usdm = ["BTCUSDT", "ETHUSDT", "SOLUSDT", "HYPEUSDT"]
binance_spot = ["BTCUSDT", "ETHUSDT", "SOLUSDT"]
bybit_linear = ["BTCUSDT", "ETHUSDT", "SOLUSDT", "HYPEUSDT"]

[profile.default.hyperevm]
enabled = false                  # R-9; needs a non-public RPC (see §10)
rpc_ws  = "env:HL_EVM_WS_URL"
pools   = "config/hyperevm-pools.toml"
```

**Profile budget.** `max_subs` is a local cap, kept below the venue limit of **1000 subscriptions per IP** (§7.1) with headroom for the bot and ad-hoc tools. The shipped profile requests ~934 subscriptions against `max_subs = 950`, so nothing is dropped. Listing `hip3:all` in `l2book` instead requests ~1085 and the planner drops every priority-3 subscription, so the majors' books are not recorded. `l2Book` is the heaviest stream: at §5's 0.2–0.5 GB/day per coin raw, recording every market on every HIP-3 dex is roughly **30–75 GB/day raw**, which is why `hip3:all` is not a default. When over budget, priority-3 (`l2Book`, `allMids`) is dropped first.

### 7.4 Subscription planner (task R-4)

Input: a resolved profile. Output: a `Plan` = list of connections, each with an ordered list of subscriptions.

Algorithm (must be deterministic: same input ⇒ same plan):

1. Expand every selector into `(stream, coin)` pairs, de-duplicated, sorted by `(priority, stream, coin)`.
2. Count them. If the count is over `max_subs`, drop from the lowest priority upward: at each step, take the **highest priority number** present (3, then 2, then 1) and remove the **last** subscription with that priority in `(priority, stream, coin, dex)` sort order (`planner::plan`, `crates/hl-arb-recorder/src/planner.rs`). An explicitly named coin gets no protection against a wildcard expansion of the same stream and priority: both are ordinary subscriptions in the same sorted set, so the lexicographically last coin is the one removed. Log every dropped pair at WARN and fail if a priority-1 pair would be dropped, unless `--allow-truncate` is set.
3. Put `l2Book` subscriptions on their **own** connection(s): they are the heaviest, and isolating them keeps `bbo` latency clean.
4. Fill the remaining connections round-robin, at most `subs_per_conn` each, so no single coin's `bbo`/`trades`/`ctx` all share one socket (limits the blast radius of one bad connection).
5. Fail if the connection count exceeds `connections`.
6. Pace subscribe messages at ≤ 20 messages/s per process (all connections combined) to stay under 2000 msgs/min even during a reconnect storm. Ping messages count toward the budget.
7. Pace new connections at ≤ 1 every 3 s (≤ 20/min, under the 30/min limit).

`hl record plan --profile default` prints the plan as a table and exits without connecting. It is the planner's acceptance test.

### 7.5 Raw WS connection (task R-3)

Extract a reusable raw connection from `crates/hl-arb-client/src/ws.rs` into `raw_ws.rs`. Then rebuild `WsMarketStream` on top of it, so the bot and the recorder share one reconnect implementation.

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

Recorded as `kind:"rest"` envelopes under `src:"hl-rest"`. All requests are `POST /info`. Use `HttpInfo::info(body)` from `hl-arb-client`.

| Request body | Cadence | Weight (SPEC-0001 §5) | Why |
|---|---|---|---|
| `{"type":"meta"}` | startup + every `meta_refresh_secs` | 20 | Universe, szDecimals |
| `{"type":"perpDexs"}` | same | 20 | HIP-3 dex list |
| `{"type":"meta","dex":D}` per HIP-3 dex | same | 20 each | HIP-3 universes |
| `{"type":"spotMeta"}` | same | 20 | Spot tokens and pairs |
| `{"type":"metaAndAssetCtxs"}` | every 60 s | 20 | Funding/OI/volume for **all** perps (backs up WS ctx) |
| `{"type":"spotMetaAndAssetCtxs"}` | every 60 s | 20 | Spot volume and mid |
| `{"type":"predictedFundings"}` | every 5 min | 20 | Predicted funding for HL and CEXes (O7). Shape `[[coin, [[venue, {fundingRate, nextFundingTime, fundingIntervalHours}], …]], …]`; first perp dex only (V-1) |
| `{"type":"fundingHistory","coin":C,"startTime":T}` | once a day per coin in `active_asset_ctx`, paging forward from the last stored time | 20 + 1 per 20 items returned | Funding backfill (O7). Response `[{coin, fundingRate, premium, time(ms)}]`, ascending, max **500** items/call (V-1) |
| `{"type":"candleSnapshot","req":{"coin":C,"interval":I,"startTime":T,"endTime":E}}` for I ∈ {1m, 5m, 1h} | once a day per coin in `bbo`, paging forward from the last stored time; a one-time backfill on first run | 20 + 1 per 60 items returned | History for O10 Part E and O11. Fields `t,T,s,i,o,c,h,l,v,n`. The venue keeps only the most recent ~5000 candles per interval (V-1: observed 5182 for 1m, 5003 for 1h), so 1m history is only a few days: **start this early**. |

Budget: the snapshotter owns a token bucket of **300 weight/min** (a quarter of the 1200/IP budget, leaving the rest for the bot and tools). A request that doesn't fit waits; it is never dropped. Metric: `hl_rest_weight_used_total{src="recorder"}`.

## 9. Reference venues (task R-8)

Used by study O5 (cross-venue lead-lag) and as fair-value references in O1/O3.

| `src` | URL | Subscribe | Payload fields used | Keepalive |
|---|---|---|---|---|
| `binance-usdm` | `wss://fstream.binance.com/stream?streams=btcusdt@bookTicker/ethusdt@bookTicker/…` | streams encoded in the URL | `s, b, B, a, A`, `T` (transaction time ms), `E` (event time ms); also `e, u, ps, st` | server sends a ping frame every 3 min; pong within 10 min (unsolicited pongs allowed); 24 h connection cap. Tungstenite auto-pongs. |
| `binance-spot` | `wss://stream.binance.com:9443/stream?streams=btcusdt@bookTicker/…` | in the URL | `u, s, b, B, a, A` (no `e`/timestamps: rely on `t_ns`) | server sends a ping frame every 20 s; pong within 1 min. Tungstenite auto-pongs. |
| `bybit-linear` | `wss://stream.bybit.com/v5/public/linear` | `{"op":"subscribe","args":["orderbook.1.BTCUSDT",…]}` | `ts`/`cts` (ms), `data.b`/`data.a` (level arrays; top of book is `data.b[0]`/`data.a[0]`) | send `{"op":"ping"}` every 20 s; level 1 is snapshot-only and re-sends a snapshot after 3 s idle. |

**Verified 2026-09-28 (V-2):** URLs, payload fields, and keepalive above match the official docs and live probes (one connect + first message each). All three are reachable from the dev machine used for this check (connect 0.6–0.8 s). The intended recorder host region is not chosen yet (V-4), so geo-blocking there is untested. Symbols are lowercased for Binance URLs, uppercase for Bybit. Reuse `RawWsConn` from R-3; only the subscribe and keepalive hooks differ.

### 9.1 Options and equities sources (tasks R-11, R-12, R-13)

These feed studies **O9** and **O10**: options-informed trading of HIP-3 tokenized-stock perps and crypto bluechips. None of them is latency-critical except `equities` for O10 Part A.

| `src` | What | Cadence | Notes |
|---|---|---|---|
| `yahoo-options` | Option chains straight from Yahoo Finance's options endpoint (the same one finsnap's collector uses) for every underlying in `research/mappings/underlyings.toml` plus SPY/QQQ: per contract strike, expiry, OI, volume, **implied volatility, bid, ask, last** | every 15 min during US regular hours, plus one snapshot after the close | **Verified 2026-09-28 (V-9).** Base `GET https://query2.finance.yahoo.com/v7/finance/options/{TICKER}`; one request per expiry (`?date=<unix seconds>` for all but the first, which comes inline). Session = consent cookie from `GET https://fc.yahoo.com/` + crumb from `…/v1/test/getcrumb`; no crumb ⇒ HTTP 401 `Invalid Crumb`. Per-contract fields: `contractSymbol, strike, currency, lastPrice, change, percentChange, volume, openInterest, bid, ask, contractSize, expiration (unix s), lastTradeDate, impliedVolatility, inTheMoney`. Data is **delayed 15 min** (Yahoo's own quote pages). No documented rate limit (unofficial endpoint): finsnap paces one request per extra expiry by **300 ms** and backs off **2 s ×2** on HTTP 429 (≤ 3 retries). Unstable and against Yahoo's ToS for automated collection/storage — see §15 and §17. This bot owns its options data and computes its own positioning metrics; it never depends on finsnap to trade. |
| `finsnap` (optional) | The owner's finsnap dashboard (`../finsnap`) is a **view**, not a dependency. Two read-only uses: (1) P-7 imports its stored `option_snapshots` history once, for O9a; (2) optionally poll `GET /snap` every 5 min in US hours to record its derived labels for side-by-side comparison. | — | Nothing in this repo requires finsnap to be running. `option_snapshots` stores only `volume`/`open_interest` per (ticker, expiration, side, strike, snapshot_date) — **no IV/bid/ask/last** — and keeps only the **last 30 days**; see §15 and §17 #6/#31. |
| `deribit` | Deribit public API: `public/get_book_summary_by_currency` (`currency=BTC\|ETH`, `kind=option`) + `public/get_index_price`; add `public/get_instruments` and per-instrument `public/ticker` for the fields the summary lacks | every 60 s | **Verified 2026-09-28 (V-10).** Free, unauthenticated, reachable from this machine (~0.1 s/call). Summary fields: `instrument_name` (`BTC-<DDMMMYY>-<strike>-<C\|P>`), `mark_iv`, `open_interest`, `volume`, `underlying_price`, `mark_price`, `bid_price`/`ask_price`, `mid_price`, `last`, `high`/`low`, `volume_usd`. The summary has **no `bid_iv`/`ask_iv`, no greeks, no `index_price`**: those require `public/ticker` per instrument (or the `ticker.<instrument>` WS channel). Rate limit: non-matching default 20 req/s sustained, burst 100; `public/get_instruments` is capped at **1 req/s** sustained (burst 50); public non-authorized calls are per-IP. |
| `equities` | Streaming real-time quotes (bid/ask/last) for the underlyings of HIP-3 stock perps (e.g. TSLA, NVDA, the index behind `XYZ100`) | streaming during US hours (+ pre/post market if the provider has it) | Provider chosen in **V-11** (free feeds cover only part of the volume; consolidated feeds are paid). Check the provider's terms allow storing the data. |

**Crypto/other sources evaluated (2026-09-29, `docs/research/data-sources-2026-09-29.md` §1.3/§1.4).** Binance Vision (`data.binance.vision`) is free and usable for personal research only; its ToS text was not retrieved (**UNVERIFIED**). Deribit's public history API (all historical option/future trades, free, keyless) is the best free crypto-options backfill but its ToS was not read. Tardis costs 350–3,000 USD/mo and its ToS carries the ML-training clause (#27). CoinGecko, CoinAPI, Kaiko and CoinDesk Data were reviewed and are not needed now. Prices are 2026-09-29 list prices and change.

### 9.2 News and alternative data (recommended minimal stack)

Recommended minimal stack for news/alt data (T3; not before the gate), all free:

1. **SEC EDGAR** — free, official, second-resolution acceptance timestamps for 8-K/Form 4/13F (https://www.sec.gov/search-filings/edgar-application-programming-interfaces). The cleanest source for claims that need sub-minute timing.
2. **GDELT** — free, 15-minute files; post-2013 records are keyed by `DATEADDED` (when reported), i.e. ingest-side, so it is **not** usable for sub-minute lead/lag, only for hours-scale attention series (https://www.gdeltproject.org/data.html).
3. **Our own forward-recorded RSS/exchange announcements** — store both `published_at` (source) and `first_seen_at` (our clock); score headline text offline with **FinBERT** (ProsusAI/finbert, Apache-2.0, https://huggingface.co/ProsusAI/finbert; https://arxiv.org/abs/1908.10063).

**Timestamp discipline.** Never join on ingest time for a lead/lag claim; only sources whose `published_at` is set by the originator (EDGAR acceptance datetime, exchange feeds) can support sub-minute claims — aggregator/GDELT timestamps bias toward a false lead of price over news. Skip NewsAPI (449 USD/mo for production), CryptoPanic (paid only), X (pay-per-use) and Reddit (approval queue; no ML training on content without a separate licence) for cost/ToS reasons. Serves O10 Parts B/E and O6; T3, not before the gate. Sources and seen-dates: `docs/research/data-sources-2026-09-29.md` Part 2.

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

`hl record` serves the same HTTP endpoints as `hl run` (reuse `hl-arb-metrics`):

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
| `options_contracts` | `t_ns, t_data, underlying, expiry, strike, cp, oi, volume, iv, bid, ask, last` | `yahoo-options`; plus finsnap `option_snapshots` history via P-7 (`iv/bid/ask/last` null there). `t_data` = the data's own as-of time, which is **not** `t_ns`. finsnap retains only the **last 30 days** and only `volume`/`open_interest`/`underlying_price` (V-9). |
| `options_expiry` | `t_ns, t_data, underlying, expiry, pc_ratio, skew_score, wmean_strike, wmean_std, wall_strike, wall_side, label, call_vol, put_vol, call_oi, put_oi` | **Computed by our normalizer** from `options_contracts`, using the same formulas as finsnap (`pcRatio`, `skewScore = 0.5·(ln volRatio + ln oiRatio)`, volume-weighted strike mean/std, wall inference; see finsnap `AGENTS.md` "Options positioning") so results are comparable with what the owner sees |
| `options_strikes` | `t_ns, t_data, underlying, strike, call_vol, call_oi, put_vol, put_oi, call_iv, put_iv` | Computed from `options_contracts` (per-strike profile across expiries) |
| `deribit_options` | `t_ns, instrument, underlying, expiry, strike, cp, mark_iv, bid_iv, ask_iv, open_interest, volume, underlying_px, index_px` | `deribit`: `get_book_summary_by_currency` for `mark_iv`/`open_interest`/`volume`/`underlying_price`, `get_instruments` for `expiry`/`strike`/`cp`, and per-instrument `public/ticker` for `bid_iv`/`ask_iv`/`index_price` (the summary omits them; V-10) |
| `equity_quotes` | `t_ns, ts_exch_ms, symbol, bid_px, bid_sz, ask_px, ask_sz, last_px, session(pre/regular/post)` | `equities` |
| `bars` | `t_open_ms, interval(1s/10s/1m/5m/15m/1h/1d), venue, market, open, high, low, close, volume, n_trades, source(mid/trade/candle)` | Built from `bbo` mids and `trades`; plus HL `candleSnapshot` backfill (R-5) and stock bars (V-11 provider) for longer history |

Market naming in every table: HL perps `BTC`, HIP-3 `xyz:TSLA`, HL spot as `BASE/QUOTE` (resolved from `spotMeta`, never `@123`), CEX as `binance-usdm:BTCUSDT`.

### 13.2 Cost model (task P-3)

`research/costs.toml` holds every fee. Studies read it through `hlr.costs`; no fee is hardcoded in a study.

| Venue / market | Taker | Maker | Status |
|---|---|---|---|
| HL perp (main dex), base tier | 4.5 bps | 1.5 bps | known (SPEC-0003 §5) |
| HL spot, base tier | 7.0 bps | 4.0 bps | known (SPEC-0003 §5) |
| HL HIP-3 perps | `4.5 × scaleIfHip3 × growthModeScale` → **9.0** (no growth) or **0.9** (growth) at scale 1.0 | `1.5 × scaleIfHip3 × growthModeScale` → **3.0** / **0.3** | known (V-3): per asset from `meta(dex)` (`deployerFeeScale`, `growthMode`); `scaleIfHip3 = scale+1 if scale<1 else 2×scale`; `growthModeScale = 0.1`. See §15. |
| Binance USDⓈ-M, VIP0 | 5.0 bps | 2.0 bps | known (V-2): 0.050% / 0.020% Regular/VIP0; ×0.9 if BNB fee deduction is on |
| Bybit linear, VIP0 | 5.5 bps | 2.0 bps | known (V-2): 0.0550% / 0.0200% VIP0; rates are region-dependent |
| HyperEVM DEX swap | pool fee (per pool in `hyperevm-pools.toml`) + gas in HYPE | — | V-6 |

Spot pairs between two spot quote assets (e.g. `USDT0/USDC`) get **80% lower taker fee and 80% smaller maker rebate/volume contribution** (`scaleIfStablePair = 0.2`); that leg is 1.4 bps taker at base tier. **Aligned quote assets** (20% lower taker, 50% larger maker rebate, 20% more volume contribution) do **not** exist on mainnet per the HIP-3 deployer-actions doc (V-3; aligned status is not exposed by `spotMeta`). All values are base tier; staking/referral discounts are separate multipliers.

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
| `captured_L` for each latency `L` | `net_bps(t_start + L) / 1e4 × size_L` (USD) **if** the episode is still open at `t_start + L` with `net_bps(t_start + L) > 0`, else `0` (missed) |

**Capture sizing (clarified 2026-09-28, P-4 review).** The order is sized when the decision is made, not re-sized at the fill: `size_L = min(size_usd(t_start), size_usd(t_start + L))`, so a book that deepens after the signal cannot enlarge the order. With fill competition (§13.10) the size is `min(max_notional, max(0, displayed − traded_notional))`: the cap binds *after* the competing notional is subtracted. Every lookup at `t_start + L` is a backward as-of read (only data available at that time). `net_bps` must already include both legs' fees and `slippage_bps` at the capped size; the detector (`hlr.episodes`) applies no costs itself.

### 13.4 Latency grid

Every latency-sensitive number is reported for **L ∈ {10, 50, 100, 250, 500, 1000} ms**. `L` is the time from the first data that reveals the episode to our orders arriving at the venue. The **headline** latency is **L = 250 ms** until measurements replace it: network RTT from V-4 plus internal tick-to-order from SPEC-0002 H-7 ([`docs/GOAL.md`](../docs/GOAL.md) §5.2). Because speed is a project priority, every report also states the **minimum latency at which the study still passes** (the "latency requirement").

**Reveal delay (backfill).** Historical data carries the exchange's timestamp, not our receive time: Tardis's Tokyo collector observed HL `bbo` arriving p1 173 / p50 229 / p90 319 ms after the HL exchange time (BTC, 2026-09-01; histdata review). Backfilled studies must add the measured HL publish lag to every `L` (a study parameter, B-9) or run on `local_timestamp`; V-4 measures the lag from our host.

### 13.5 Study metrics (same columns in every report)

| Metric | Definition |
|---|---|
| `days` | Days of valid data used (need ≥ 14 for a final report; ≥ 3 for a preliminary one) |
| `coverage_pct` | Share of wall time where all inputs were valid |
| `episodes_per_day` | Median and p90 across days |
| `duration_ms` | p50 / p90 of episode durations |
| `peak_net_bps` | p50 / p90 |
| `capture_rate_L` | Share of episodes still open at `+L` |
| `capture_naive_L` / `capture_adj_L` | Naive (still-open) vs competition-adjusted capture (§13.10; the displayed size at the episode price minus what someone else traded in `[t_start, t_start+L]`). The verdict uses `capture_adj_L`. |
| `usd_per_day_L` | Σ `captured_L` / days, at the study's `max_notional` |
| `usd_per_day_L_ci90` | 90% CI of `usd_per_day_L` from a day-block bootstrap (resample days, 2,000 draws; §13.10) |
| `cells_K` | Number of (pair × cell) combinations scanned in the grid; the headline is the OOS-selected cell with no re-selection (§13.10) |
| `capital_usd` | Capital needed to run the strategy at that notional (both legs, margin at 3× unless stated). Evaluated at every point of the **capital grid** in `research/thresholds.toml` (default $10k / $25k / $50k / $100k): `max_notional` scales with capital, but capture is capped by the book size available in each episode, so APR usually falls as capital grows. |
| `apr_L` | `usd_per_day_L × 365 / capital_usd`, reported for each capital grid point |
| `apr_L_ci90` | 90% CI of `apr_L`; the lower bound gates PASS (§13.6) |
| `markout_1s` / `markout_10s` | Median mid move over +1 s / +10 s after the hypothetical fill (adverse selection; §13.10) |
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
| **PASS** | `apr_L ≥ apr.target` **and** the 90% CI lower bound of `apr_L` ≥ `apr.floor` **and** every `[quality]` criterion holds | Build it (candidate for M5) |
| **MARGINAL** | `apr.floor ≤ apr_L < apr.target` **and** every `[quality]` criterion holds | Acceptable. Build it if it's cheap to implement, stacks with a PASS strategy on shared infrastructure, or nothing passes. |
| **FAIL** | `apr_L < apr.floor`, **or** any `[quality]` criterion fails | Don't build it (re-test later if conditions change) |
| **INCONCLUSIVE** | Not enough data (`days`/`coverage_pct` too low) | Keep recording; re-run |

For fast (episode) studies the verdict uses the out-of-sample (last 40%), competition-adjusted, jittered-latency numbers (§13.10), not the in-sample point estimate; a study whose point estimate is ≥ target but whose 90% CI lower bound is < floor is **MARGINAL**, not PASS.

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
3. **Method**: which legs, the `net_bps` formula, parameters (`buffer_bps`, `max_notional`, `stale_ms`), the **pre-registration git SHA** from `research/REGISTRY.md`, anything that departs from §13
4. **Results**: the §13.5 metrics table, latency-grid table, per-day bar chart (PNG in `research/reports/img/`), top-10 episodes table, **artifact counts** (excluded vs counted), and **sensitivity rows** (clock-skew shift, jittered latency, buffer × multiplier)
5. **Sanity checks**: at least 3 of the largest episodes inspected by hand against raw frames; is each real, or a data artifact?
6. **Verdict**: PASS / MARGINAL / FAIL / INCONCLUSIVE against §13.6 (and §13.8 for slow-signal studies), the APR at each capital grid point, implementation cost S/M/L, and the main risks
7. **Reproduce**: the exact command(s) and git SHA

Backfill runs are stamped `PRELIMINARY (backfill: <sources>)` and carry the `HIST-PRELIM` qualifier (§13.11).

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

Each study below maps to one or more tasks (S-1…S-23, tiered in §14.0–14.1). Fast studies depend on P-1…P-5, slow-signal parts also on P-6, plus the data listed.

### O1 — HIP-3 / main-dex same-underlying dislocations (task S-1)

| Item | Detail |
|---|---|
| Hypothesis | The same underlying on different perp dexes (e.g. a HIP-3 equity/index perp on two dexes, or a HIP-3 crypto perp vs the main-dex perp) trades at prices that diverge by more than fees often enough to arb. |
| Data | `bbo` for `hip3:all` and main-dex perps; `ctx` (funding differs by dex); `markets` |
| Pairing | `research/mappings/underlyings.toml` maps each underlying to its HL markets: `["AVGO"]` → `hl = ["para:AVGO", "xyz:AVGO"]`, `["BTC"]` → `hl = ["BTC"]`. Built from live metadata and reviewed (V-9). Only mapped pairs are studied. **V-9 (2026-09-28):** only xyz, para, io and mkts have listed markets, so cross-dex equity pairs are few (AAOI, AVGO, CRWD, IREN, NET, RDDT, SNDK, EWY, NBIS; see §15). |
| Signal | For each ordered pair (A, B) of the same underlying: buy A at ask, sell B at bid. |
| Extra cost | Funding differential over the expected holding time (default: 1 h, since positions are unwound when prices re-converge) |
| Special checks | Oracle/mark definitions can differ across dexes. Report the persistent **basis** (rolling 1 h median of mid differences) separately, and flag pairs whose "edge" is really a stable basis rather than transient dislocations. |

### O2 — Spot triangular across stablecoin quotes (task S-2)

| Item | Detail |
|---|---|
| Hypothesis | Tokens quoted in several stablecoins on HyperCore spot (e.g. `X/USDC` and `X/USDT0`) plus the stable-vs-stable pair (e.g. `USDT0/USDC`) form triangles whose product departs from 1 by more than three spot taker fees. |
| Data | `bbo` for `spot:quotes`; `markets` (quote tokens come from `spotMeta`, never hardcoded; V-3 observed USDC/USDT0/USDH/USDE, see §15) |
| Signal | For each triangle and both directions: `product = Π (1 / ask or bid)` along the cycle; `gross_bps = (product − 1) × 1e4`; subtract the three legs' spot taker fees (the stable-vs-stable leg pays 80% less; see §13.2). |
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
| Universe | HIP-3 stock perps whose underlying has a liquid US options chain (e.g. `xyz:TSLA` ↔ TSLA; an index perp ↔ QQQ/SPY), plus BTC/ETH (Deribit). Built in V-9 into `research/mappings/underlyings.toml`, e.g. `TSLA = { hl = ["xyz:TSLA"], options = "TSLA", equity = "TSLA" }`. **V-9 (2026-09-28):** only **xyz (109 listed), para (29), io (8) and mkts (4)** have listed markets; `flx`, `vntl`, `hyna`, `km`, `abcd` and `cash` are fully delisted. The file maps 122 active HIP-3 underlyings: **89** with a US chain (or index-ETF proxy), **33** marked `no-chain`. |
| Data | `options_contracts` / `options_expiry` / `options_strikes`, `deribit_options`, `bbo` + `ctx` for the mapped perps, `equity_quotes` (the underlying's real price), `funding_hist` |
| Pre-registered signals (v1) | **P1 wall pinning**: within 2 trading days of a large expiry, if spot is more than 1σ (the expiry's strike std) from the dominant OI strike, lean toward that strike; exit at expiry. **P2 skew extreme**: `skew_score` 60-day z-score beyond ±2 ⇒ contrarian position for H; the momentum sign is also tested and both are reported. **P3 crypto IV skew** (Deribit): 25-delta risk-reversal z-score beyond ±2 ⇒ contrarian BTC/ETH perp position. **P4 market regime**: SPY/QQQ positioning label (computed by our normalizer with finsnap's formulas) as a filter on P1–P3 (trade only when the index label agrees). |
| Data history | O9a: only V-13 purchases and whatever forward collection exists — **finsnap keeps just 30 days** of `option_snapshots` (and only volume/OI, no IV), so its history cannot reach the ≥ 2-year target by itself (V-9; §17 #6). O9b: forward-collected; preliminary after ≥ 20 US trading days, final after ≥ 60 trading days **and** ≥ 30 out-of-sample signals per variant. |
| HIP-3 specifics | Stock perps trade 24/7, but the underlying and its options trade only in US hours. Oracle, funding, fees, and leverage per dex come from V-12. Entry/exit prices outside US hours must use the HL perp price only. |
| Owner prior | The owner has used options positioning in discretionary stock trading and saw it work, but it hasn't been tested quantitatively. finsnap's "context, never signal" label is a legal-style disclaimer, not a finding. O9 is the quantitative test. |
| Two stages | **O9a (history, now):** test the signals on the **real stock** (as a proxy for the HL perp, which tracks the stock during US hours), using finsnap's stored `option_snapshots` (daily, per contract; imported read-only by P-7 — but V-9 found it retains only the **last 30 days** with no IV, so V-13 purchases are needed for a real history) plus bought history if V-13 approves it, and stock bars from the V-11 provider. This can produce a verdict in days, not months. **O9b (forward):** the same pre-registered signals on the actual HL perps with HL prices, fees, and funding, confirming the edge transfers (off-hours behavior, funding drag). |

### O10 — HIP-3 stock perps vs the real stock (tasks S-10a T1: Parts A, D · S-10b T2: Part C · S-10c T3: Parts B, E)

| Item | Detail |
|---|---|
| Hypothesis | HIP-3 stock perps are priced off the real stock during US hours and trade on their own the rest of the time. That creates (A) short-lived dislocations vs the live stock price, (B) predictable convergence at the US open after nights/weekends, and (C) persistent funding/premium patterns. |
| Part A: market-hours dislocation | Fair = equity mid (`equity_quotes`) + rolling 5-min median basis. Episodes per §13.3 when the HL perp is through fair by more than HL taker + buffer. HL-only execution; a hedge in the stock needs a brokerage, so it's measured only (like O5). **Latency grid applies.** |
| Part B: open convergence | For every US open after a closed period (overnight, weekend, holiday): compare the perp's last price before 09:30 ET with the stock's opening print. Measure whether the perp's closed-hours move over- or under-shoots, overall and relative to the options-implied move (IV from `yahoo-options`). Pre-registered trade: fade closed-hours perp moves larger than k × implied move (k ∈ {1, 1.5, 2}) shortly before the open, exit after the open. §13.8 method. |
| Part C: funding and premium | Distribution of HIP-3 stock-perp funding and perp-vs-stock premium by session (regular / pre / post / closed / weekend). Report whether a carry-like pattern clears costs. |
| Part D: closed-hours lead-lag (arb-like, both directions) | When US markets are closed (nights, weekends, holidays), the only live prices are HL perps. BTC/ETH and index perps move first, and single-stock perps with high market or crypto beta (e.g. COIN, MSTR, HOOD, NVDA, TSLA) may lag. Estimate each stock perp's rolling beta to BTC and to the index perp in closed sessions; fair = stock-perp last + β × (driver return since); episodes per §13.3 when the stock perp is through fair by more than fees + buffer. Long **and** short. **Latency grid applies.** |
| Part E: closed-hours move → next-session / week-ahead edge | Pre-registered: signal = the stock perp's closed-period return (Fri US close → Mon pre-open for weekends; US close → next open for overnights), optionally scaled by the options-implied move. Trades: (i) on the perp from Sunday evening / late night into the US open (continuation vs fade, both reported); (ii) on the perp from the US open over horizons 1 d, 3 d, 5 d (weekend) or 1 d (overnight). §13.8 method. History: HL `candleSnapshot` 1h candles for HIP-3 perps since listing (R-5), plus forward data. |
| Owner intent | The owner expects an edge in weekend/overnight information for the following session/week and is willing to run a small-to-medium pilot on MARGINAL evidence (§13.6 pilot allowance). |
| Data | `bbo`, `ctx`, `bars` for HIP-3 stock perps, BTC/ETH, and index perps; `equity_quotes`; `options_*` (for Part B/E implied moves); HIP-3 oracle updates from the node (SPEC-0009, optional) |
| Prereq | V-11 + R-13 for Parts A/B; Parts C, D, E need only HL data (+ options for the implied-move variants) |

### O11 — Bollinger-band mean reversion, arb-style (tasks S-11a T1: Part A · S-11b T3: Parts B, C)

Prior: in finsnap's backtests across its whole universe, **Bollinger Reversion** (20-period SMA ± 2σ / 3σ; buy a close at or below the lower band, sell a close at or above the upper band; next-bar-open fills) had the best historical average of all 20 strategies. That was on daily bars of US ETFs. O11 tests whether the same idea (price stretched ~2σ from its rolling mean tends to revert) pays on Hyperliquid at arb-like speeds and timeframes.

| Item | Detail |
|---|---|
| Part A: spread bands (stat-arb, market-neutral; the arb version) | Apply Bollinger bands to the **log-spread** between related instruments, instead of to one price: HIP-3 cross-dex pairs (O1), spot vs perp (O3), stock perp vs real stock (O10 A), stock perp vs index perp (β-hedged), BTC vs ETH (β-hedged). Enter when the spread closes beyond ±k σ of its rolling mean; exit at the middle band (variant 1) or the opposite band (variant 2, finsnap's rule). k ∈ {2, 3}; window N ∈ {20, 50, 100} bars; bars ∈ {1 s, 10 s, 1 m, 5 m}. Both legs taker (plus a maker-entry variant: rest at the band). Extends O1/O3/O10 A: the rolling middle band absorbs a persistent basis automatically. **Latency grid applies** (for the sub-minute bars). |
| Part B: single-instrument intraday bands (directional) | finsnap's exact rule (20/2σ and 20/3σ) on HL perps (BTC, ETH, SOL, HYPE, HIP-3 stock perps), bars ∈ {1 m, 5 m, 15 m, 1 h}, variants: long-only (finsnap) and long/short. Include weekend/overnight sessions for stock perps and report them separately. §13.8 method with bar-close signals and next-bar-open fills. Costs dominate at short bars: report taker and maker-entry variants. |
| Part C: as a filter | Bollinger bandwidth (squeeze) and %B as pre-registered filters on O9 and O10 E signals (e.g. only fade a closed-hours move when %B is beyond 0/1). Reported inside those studies. |
| Data | `bars` (from `bbo`/`trades` + `candleSnapshot` backfill), `bbo` for the spread legs, `equity_quotes` (for stock-vs-perp spreads), `funding_hist` |
| History | Part B 1h: HL candle history (months). Parts A and B sub-hour: forward-recorded bars. Stock-proxy pre-check: the V-11 provider's multi-year stock minute bars can test Part B's rule on the underlying stocks right away (like O9a). |
| Pre-registration | Parameters above are the complete grid, fixed now. Report every cell; the headline is the out-of-sample result of the cell chosen on the in-sample 60%. |

### O12 — Oracle-update timing and mark-price trigger cascade (task S-12)

| Item | Detail |
|---|---|
| Hypothesis | The oracle is republished by validators every ~3 s as a stake-weighted median of 8 CEX spot prices (weights Binance 3, OKX/Bybit 2, others 1); mark price derives from it. Between ticks `oraclePx` can lag a fast CEX move, and TP/SL triggers plus some liquidations fire in a burst at/after the next tick. Trade (a) the HL perp when the oracle/book has not repriced after a > fees+buffer move on the high-weight CEXs, and (b) rest ALO to receive the post-tick burst and exit on reversion. |
| Data | `ctx` (`oraclePx`, `markPx`, funding; plus `fastAssetCtxs` for faster `markPx`/`midPx`) ⚠ verify, `bbo`, `trades`, CEX `bbo`; derived `oracle_updates (t_ns, market, oracle_px, mark_px, tick_index_est)` |
| Signal | Fast (§13.3). Fair = weighted-median CEX mid (documented weights). Episode when `sign(fair − oraclePx)` persists and `\|fair − oraclePx\| > fees + buffer`. Also an event study at inferred oracle-tick boundaries (first frame whose `oraclePx` changes); headline `L` is tick-aligned, not a flat 250 ms. |
| Latency | High (a race). §13.4 grid plus jittered latency; report the minimum passing `L`. |
| Special checks | Infer tick cadence from data (never assume exactly 3 s); `markPx` blends book state, so an `oraclePx` lag may not be tradable; CEX feeds must be fresh and clock skew < 1 ms (±25 ms sensitivity); exclude episodes within 1 s of a gap/reconnect. **Could fail:** the tradable mark reprices faster than the oracle; MMs stack ALO on the burst (prioritized over taker); the lag is tiny unless a high-weight venue gaps. |
| Prereq | V-1, R-8 |

### O13 — HIP-3 deployer-oracle stair-step / stale fallback (task S-13)

| Item | Detail |
|---|---|
| Hypothesis | HIP-3 oracles are pushed by the deployer (`setOracle`) at most once per 2.5 s ("expected every 3 s") and the mark move is clamped to 1% per update (prices clamped to 10× start-of-day); a stale mark falls back to the local mark after 10 s. When the real underlying gaps, the perp can only walk toward it at ≤ 1%/update. Trade the perp toward external fair while the clamp/staleness binds, and fade the one-way stair-step when the underlying stalls. |
| Data | `ctx` (`oraclePx`, `markPx`, `premium`), `bbo`, `equity_quotes`/CEX, `markets`; derived `hip3_oracle_updates (t_ns, dex, coin, oracle_px, mark_px, dt_since_prev)`; `perpDexs` (deployer, caps, funding multipliers) |
| Signal | Episodes (§13.3) vs `fair_external` (V-11/V-12 reference) net of the true HIP-3 deployer fee scale; detect clamp-limited moves (\|Δmark\| ≈ 1%) and time since the last update. Also a §13.8 "convergence completion" event study. |
| Latency | Medium (stair-steps persist seconds–minutes); `L` = 250 ms–1 s. |
| Special checks | Exclude `haltTrading`/settled periods; `externalPerpPx` is node-only, so infer it from the external feed; verify the clamp is on the **mark**, not the oracle; per-dex behavior is heterogeneous. **Could fail:** convergence (1%/2.5 s) is too fast to cover two taker fees; the deployer widens/stops updates exactly when it matters; funding on the lagging dex eats the edge. |
| Prereq | V-12 |

### O14 — Funding-settlement timing (hourly print) (task S-14)

| Item | Detail |
|---|---|
| Hypothesis | Funding settles hourly at 1/8 of the computed 8 h rate, which is the average premium sampled every 5 s plus a clamped interest component; the payment uses the **oracle** notional, so the next hourly print is largely known before settlement. Harvest a large known print with a short delta-managed position, and trade the predictable pre-settlement hedging flow. |
| Data | `ctx`, `funding_hist`, `bbo`, `trades`; `perpDexs` + deployer actions (multipliers, interest, clamps); derived `funding_params` |
| Signal | Event study around each hourly boundary for Δ ∈ {60, 30, 10, 3, 1} min: mid path and "enter at T−Δ, exit at T+Δ'" net of 2×taker + funding. Compare predicted (trailing premium average) vs realized (`funding_hist`); test hedging the price risk on a correlated perp. |
| Latency | Low/medium; report at `L` = 1 s. |
| Special checks | Separate funding PnL from mark-to-market and report both; check the print is not already in a persistent basis (then it is O7); bucket rates at the 4%/h cap separately (possible deployer manipulation). **Could fail:** baseline funding (~1.25 bp/h) is below 9 bp round-trip taker outside extremes; unwinding into the post-settlement crowd gives back the print; rates flip sign often. |
| Prereq | V-3, V-12 |

### O15 — Liquidation-cluster ladder (forward map) (task S-15)

| Item | Detail |
|---|---|
| Hypothesis | `clearinghouseState` exposes every address's positions and `liquidationPx`; aggregating public positions gives a per-coin liquidation ladder. When price approaches a cluster the forced flow is partly predictable: rest ALO just beyond a large cluster to receive it and exit on reversion, or lean after the cluster starts triggering and take profit on exhaustion. |
| Data | `positions (t_ns, user, dex, coin, szi, entry_px, liquidation_px, margin_used, leverage_type, account_value)` from `clearinghouseState` per watched address or node outputs; `ctx`, `bbo`, `trades` |
| Signal | Build the ladder continuously; event study keyed on "distance to next cluster" and "cluster notional / ADV". Measure overshoot, reversion speed, and PnL of (a) ALO inside the cascade and (b) taker lean, net of taker. |
| Latency | High for entering a triggered cascade; medium/low for the ALO variant. |
| Special checks | `liquidationPx` is a moving target (funding, cross-margin, top-ups): report the map's error. The public API is one address per call and WS caps at **10 users/connection**, so full coverage needs the node/S3 or the ladder is a biased sample. Separate book liquidations from backstop/HLP and ADL. **Could fail:** margin moves stale the map during volatility; the 30 s cooldown and 20%-per-block partial-liquidation rule slow cascades; HLP internalizes the flow. |
| Prereq | New public-positions/node source, V-1 |

### O16 — Public TWAP-flow prediction (task S-16)

| Item | Detail |
|---|---|
| Hypothesis | TWAP orders are public chain state: `twapStates`/`userTwapHistory`/`userTwapSliceFills` expose `executedSz`, total `sz`, `minutes`, `randomize`, `reduceOnly`, and slices are ≥ 30 s apart with "catch-up" slices up to 3× normal (3% slippage cap). Front-load risk on the side a visibly-behind TWAP must trade, or provide liquidity into the catch-up slice and fade the ending. |
| Data | `twap (t_ns, user, coin, side, total_sz, executed_sz, minutes, randomize, reduce_only, status)` from a `twapStates` watchlist or (ideally) all TWAPs from node L1/S3 `replica_cmds` ⚠ verify; `bbo`, `trades`, `book` |
| Signal | Detect large TWAPs (notional / 1 h ADV above a threshold); episode = "active TWAP behind target by X%". Measure drift over the remaining schedule, the catch-up slice, and post-completion reversion, net of taker. |
| Latency | Medium (slice cadence ≥ 30 s; the catch-up is the fast part). |
| Special checks | `randomize` (±20%) and the 3× cap blunt prediction; WS shows only the next 8 nonces/current state, so the full population needs node/S3. Distinguish TWAPs from other algorithmic flow; a TWAP that cannot fill (3% cap) is not price-insensitive forever. **Could fail:** a large visible TWAP is already priced; randomisation kills per-slice prediction; the ending may not revert. |
| Prereq | New node/TWAP source, V-8 |

### O17 — HIP-4 outcomes: digital vs Deribit + 06:00 pin (task S-17)

| Item | Detail |
|---|---|
| Hypothesis | HL has native **HIP-4 outcome markets** (fully-collateralized binaries; a daily 06:00 UTC binary settling to the HyperCore **mark**, extended to BTC/ETH/HYPE/SOL — launch date ⚠ verify). Their prices can diverge from a Deribit-implied digital beyond costs; the 06:00 mark settlement pins the underlying perp; and the merged Yes/No book can show transient locked/crossed states. |
| Data | New `outcomes` source: `outcomeMeta`, outcome `l2Book`/`bbo`/`trades` (Yes and No); `deribit_options`, HL perp `bbo`/`ctx`, `trades` |
| Signal | (a) §13.3 cross-venue vs a Deribit smile-replicated digital, minus fees (docs say zero opening/closing fees; builder fees may apply on sells ⚠ verify); (b) event study of the perp mark path and outcome decay into 06:00; (c) microstructure: `Yes_ask + No_ask < 1 − buffer` (buy both) and `Yes_bid + No_bid > 1 + buffer` (split and sell). |
| Latency | Medium; (c) is fast and capacity-limited. |
| Special checks | Settlement is to the HL **mark**, so a "mispricing" vs Deribit may be a rational basis: model it separately. Full collateral, no liquidation. Thin books: report the USD cap. Digital replication is model-sensitive: report a range and pre-register the model. **Could fail:** liquidity too thin at $25k; the mark basis exceeds the apparent edge; zero fees may end; the merged-book priority makes (c) untakeable at the same instant. |
| Prereq | New R-task for the outcomes source, V-10 |

### O18 — Cross-dex funding-differential carry (task S-18)

| Item | Detail |
|---|---|
| Hypothesis | Each perp dex has independent books, margining and **deployer-set funding parameters** (`setFundingMultipliers` 0–10, `setFundingInterestRates` ±1%/8 h, `setFundingClamps`), and HIP-3 uses a more responsive premium formula. The same underlying on two dexes can carry persistently different funding; long the cheap-funding dex / short the rich one to harvest the differential. |
| Data | `funding_hist` per coin/dex, `ctx`, `bbo`, `perpDexs` + deployer actions, `positions`/margin |
| Signal | "Open when annualized differential > X for N hours, close when < Y", grid over X/Y/N; net APR vs 2×taker + basis risk + funding drag. Report the persistent price basis separately from the funding differential. |
| Latency | Low (`L` = 1 s). |
| Special checks | Legging/divergence risk (independent books, non-atomic legs); margin is per-dex so capital may double. Verify the deployer cannot change funding params adversely while we hold (30-day cooldown on fee changes; funding-param cooldowns ⚠ verify). **Could fail:** differentials usually below two taker fees + basis noise; basis risk makes it directional; funding flips; capital is locked twice. |
| Prereq | V-3, V-12 |

### O19 — Quote-asset peg defense + true spot fee multipliers (task S-19)

| Item | Detail |
|---|---|
| Hypothesis | A permissionless spot quote asset is backed by a slashable 200k-HYPE stake (3-year lock), slashable on validator vote if `QUOTE/USDC` fails its size/band conditions for a majority of 1-second samples over three days; aligned quote assets add stronger conditions. That creates a forced peg defender. Trade a quote asset away from par, betting on defense before the slashing clock. Separately, the real fee schedule changes O2: stable-vs-stable spot pairs have **80% lower taker fees**, and aligned quote assets **20% lower taker / 50% better maker**. |
| Data | Spot `bbo` for quote assets and their base pairs, `markets`/`spotMeta`, `spotMetaAndAssetCtxs`; derived `quote_peg (t_ns, quote_token, quote_usdc_px, depth_within_band)`; aligned fee flags (⚠ `spotMeta` does not expose aligned status; see V-3 in §15) |
| Signal | §13.3 on `QUOTE/USDC` vs 1 (episode beyond a buffer) plus quote-asset triangles using the **actual per-pair fee multiplier**; hold the depeg trade to par with a time stop before the 3-day window. |
| Latency | Low/medium. |
| Special checks | Slashing is discretionary (validator vote), so the defender is incentivized, not guaranteed; a depeg can persist days; quote-asset books are thin (report size); new quote assets may have no USDC pair. This **extends O2**; the new content is the slashing mechanism and the fee correction. **Could fail:** validators may not slash; the deployer may not be able to defend; thin books cap size. |
| Prereq | V-3 |

### O20 — Portfolio-/cross-margin contagion forced flow (task S-20)

| Item | Detail |
|---|---|
| Hypothesis | Portfolio and cross margin link assets, so a loss in one can force deleveraging of an apparently unrelated asset held in the same account. With public positions, margin mode and abstraction state, the correlated forced-selling map can be estimated and the uncontaminated asset faded after the flow. |
| Data | `positions` with `leverage.type`, margin mode and abstraction (`userAbstraction`); `ctx`; node `dex_user_account_summaries` and L1 data; `bbo`, `trades` |
| Signal | Event study: large move/deleverage in asset A → abnormal move and reversion in correlated asset B held by the same addresses; episode PnL of fading B net of fees. |
| Latency | High. |
| Special checks | Attribution is the hard part: forced flow vs ordinary correlation needs position-level (node) data. Portfolio-margin eligibility is validator-restricted, limiting the universe. ADL may close the winner before the fade. **Could fail:** cascade attribution unreliable; forced flow small and absorbed by HLP; correlation dominates. |
| Prereq | Public-positions/node source, V-12 |

### O21 — HIP-2 Hyperliquidity deterministic-quote pickoff (task S-21)

| Item | Detail |
|---|---|
| Hypothesis | HIP-2 Hyperliquidity is a deterministic protocol strategy on USDC spot pairs: a recursive grid (`px_i = round(px_{i−1} × 1.003)`), refreshed on blocks ≥ 3 s since the last update, targeting ~0.3% spread. Because the grid/refresh is deterministic and the external reference moves continuously, the ladder is stale for up to ~3 s after a ≥ 0.3% move and its next levels are predictable. Pick off (or rest against) the stale tranche. |
| Data | Spot `book`/`bbo` for HIP-2 pairs, external reference (CEX/other HL pair), `markets`/`spotMeta`, `trades` |
| Signal | §13.3: episode when the external-reference-implied price is through the HL ladder by > fees; identify HL tranches by size and 0.3% spacing. |
| Latency | High (~3 s window, block-cadence refresh). |
| Special checks | HIP-2 only operates on USDC spot pairs and only updates on blocks ≥ 3 s apart; the 0.3% spread may already exceed fees + adverse selection; distinguish HIP-2 from human MM quotes (mis-attribution kills the thesis); new listings are the most dislocated and rarest. **Could fail:** 3 s refresh vs the ~0.3% round-trip edge; adverse selection; thin books; rare listings. |
| Prereq | V-1; no new source |

### O22 — Read-precompile / CoreWriter-delay asymmetry (task S-22)

| Item | Detail |
|---|---|
| Hypothesis | HyperEVM read precompiles return HyperCore state guaranteed to match the latest Core state at the EVM block's construction (oracle prices, positions, vault equity, L1 block number). CoreWriter order actions are **deliberately delayed** a few seconds, and transfers are asymmetric (EVM→Core lands in the same L1 block; Core→EVM waits for the next EVM block). Read a guaranteed-fresh Core oracle inside a contract and trade an EVM pool against it while the Core leg is delayed. |
| Data | `evm_pools`, Core spot `bbo`, oracle precompile reads, HyperEVM block/receipt data (`s3://hl-mainnet-evm-blocks/`, node `evm_block_and_receipts`), gas |
| Signal | §13.3 with the precompile oracle as fair; model the CoreWriter delay and the block the EVM tx lands in; PnL of the EVM leg plus the delayed Core leg (non-atomic). |
| Latency | Medium/high (EVM block 1 s small / 60 s large; CoreWriter delay seconds). |
| Special checks | The delay is the whole risk: the Core leg can be repriced/rejected. Precompiles cost gas and consume all gas on invalid input; priority-fee competition inside EVM blocks. Confirm precompiles are on **mainnet** (docs describe testnet) ⚠ verify. **Could fail:** non-atomicity; delay; gas; thin EVM pools; mainnet precompile availability unconfirmed. |
| Prereq | V-5, V-6, R-9, own node/RPC |

### O23 — HYPE realized-vs-implied vol carry (task S-23)

| Item | Detail |
|---|---|
| Hypothesis | HL has no native vanilla options, but HYPE has external/on-chain option markets (Derive; HyperEVM protocols such as Hypersurface, opt.fun ⚠ verify). HL funding/premium and mark-oracle deviation are a public high-frequency proxy for short-horizon HYPE realized vol. When implied vol exceeds HL realized vol by more than costs, sell options and delta-hedge with the HYPE perp; buy vol when funding/premium spikes signal cheap IV. O9 uses options for **direction**; this is a **vol-carry / relative-vol** strategy. |
| Data | `deribit_options` (HYPE), new HyperEVM options quotes, HL HYPE `bbo`/`ctx`/`bars` for realized vol, `funding_hist` as a vol signal |
| Signal | §13.8 slow-signal: pre-register the IV/RV threshold and the delta hedge; net PnL = option premium − realized variance − hedge costs − gas; report the variance risk premium by regime. |
| Latency | Low (hours–days). |
| Special checks | On-chain option spreads are wide and thin; hedging on HL is taker-heavy at short intervals and pays funding; HYPE options are not in the current recorder; needs SPEC-0004 directional risk limits (§17 Q7). **Could fail:** wide option spreads; taker-heavy hedges; gas; EVM key operational surface; no native HL options. |
| Prereq | R-11/R-12, new EVM options source, P-6, V-10; T3 gate |

### Classification-model studies (M-1…M-5) — PROPOSED

These are **proposals, not yet in the work-breakdown table** (§14); no S-tasks are added for them. They come from `docs/research/data-sources-2026-09-29.md` §3.5 and are ranked by expected value over cost. A classifier is always part of a *study*, never strategy code first (§13). M-1/M-2 are T1-adjacent and cost nothing in data; M-3/M-4/M-5 are T3 and gated by [`docs/GOAL.md`](../docs/GOAL.md) §2.1. Two findings set expectations: after costs, deep LOB models and LLM-news alpha are weak out-of-sample and generally decay (https://arxiv.org/html/2308.01915, https://arxiv.org/abs/2304.07619); classifiers are **filters, not edge creators** (https://hudsonthames.org/does-meta-labeling-add-to-signal-efficacy-triple-barrier-method/).

**Common protocol (all M-studies).** Pre-register the feature list and hyperparameter grid in the hypothesis registry (§13.13); always include a baseline (linear/logistic, or the base rule with no filter); purged walk-forward with embargo; block-bootstrap CIs on net PnL/episode and APR at the headline capital ($25k) and the §13.4 latency grid; **PASS = 95% CI lower bound > 0 net of costs AND uplift vs baseline CI > 0 AND ≥ 30 out-of-sample signals per variant**; MARGINAL/FAIL per the §13.6 APR floor/target thresholds; report the Deflated Sharpe with the true trial count.

#### M-1 — Meta-labelled take/skip filter on arb episodes (proposal; T1-adjacent, rank 1)

| Item | Detail |
|---|---|
| Hypothesis | Among episodes that pass the base rule at latency L, a secondary classifier predicts which are net-positive, raising net PnL/episode and cutting the loss tail versus taking all. |
| Features (known at detection) | net edge bps; spread and depth at both venues; time since last HL oracle/mark update; Binance-lead move size; recent volatility; hour; funding-settlement proximity. |
| Label | 1 if episode net PnL after fees/slippage at latency L is > 0 (fixed horizon = episode life, triple-barrier style: profit target, stop, time). |
| Split | Chronological purged walk-forward, ≥ 14 days recorded data (gate G1), last 30% untouched hold-out; models logistic (baseline) and GBT. |
| Cost model | Reuse P-3 (fees/slippage/latency grid). |
| Pass rule | Hold-out net PnL/episode of the filtered set exceeds the unfiltered set by CI lower bound > 0 with ≥ 30 signals; the filter must compile to a ≤ 20-parameter rule (hot-path-safe). |
| Data cost / tier | 0 data cost, effort S/M; T1-adjacent — serves GOAL §2 items 1/3 (O5/O1/O12 after P-4). |

#### M-2 — Microstructure GBT vs linear OFI for CEX-to-HL lead (proposal; T1-adjacent, rank 2)

| Item | Detail |
|---|---|
| Hypothesis | A GBT on multi-level OFI, trade imbalance, Binance-minus-HL mid basis, and depth ratios predicts the HL mid move over 100 ms–5 s beyond the linear OFI baseline (Cont, Kukanov, Stoikov 2014, https://arxiv.org/abs/1011.6402). |
| Features | multi-level OFI, trade imbalance, CEX-minus-HL mid basis, depth ratios. |
| Label | Ternary sign of HL mid change beyond (half-spread + fee) at horizon h ∈ {0.25, 1, 5} s; trade only when predicted class prob > threshold. |
| Split | Walk-forward by day, embargo = h. |
| Cost model | Taker fee tier + spread + latency grid (§13.2/§13.4). |
| Pass rule | Net APR CI lower bound > floor at latency ≥ measured p50 tick-to-order + RTT, and uplift over the linear OFI baseline > 0; otherwise record FAIL (a useful negative result). |
| Data cost / tier | 0 data cost, effort M; T1-adjacent — serves O5/O12. |

#### M-3 — Options-positioning features → daily direction of HIP-3 stock perps / bluechips (proposal; T3, rank 3)

| Item | Detail |
|---|---|
| Hypothesis | Skew, put/call volume+OI ratio, OI-wall distance, and IV minus realized vol (IV from EODHD; Deribit `mark_iv` for BTC/ETH) predict next-1d/3d direction beyond a trailing-return baseline. |
| Features | skew, put/call volume+OI ratio, OI-wall distance, IV − RV. |
| Label | Triple barrier on the daily close of the real stock (proxy) with vol-scaled barriers; then O9b forward on HL perps. |
| Split | Purged walk-forward on the EODHD window (~2.9 yr ≈ 700 days), last 6 months hold-out; underlying-clustered bootstrap. |
| Cost model | HL stock-perp fees + funding + open-to-open gap in off-hours. |
| Pass rule | Hold-out net APR CI lower bound > floor, and IC CI > 0 across ≥ 60% of tickers. |
| Data cost / tier | 29.99 USD once (V-13 EODHD add-on); power warning — few days × tickers, so pre-register ≤ 6 features and 1 label, expect MARGINAL/inconclusive unless effects are large; T3 (gated, O9a/O9b). |

#### M-4 — News/filing event study with classifier tagging (proposal; T3, rank 4)

| Item | Detail |
|---|---|
| Hypothesis | For HIP-3 stock perps, 8-K/news events arriving while the stock is closed are priced by the perp within X minutes, and the FinBERT sign (or a fine-tuned classifier) predicts the perp's 5–60 min drift beyond the initial jump. |
| Features | FinBERT sentiment sign/score, 8-K item type, event age, closed-session flag. |
| Label | Sign/size of perp return over [t+1 min, t+60 min] net of costs. |
| Split | Forward-only, chronological; ≥ 60 trading days and ≥ 30 events/variant. |
| Cost model | HL fees + spread (event study, §13.8). |
| Pass rule | Event-study CAR CI excludes 0 net of costs at latency 1 s and 30 s; baseline = keyword/8-K-item-type rules without ML. |
| Data cost / tier | 0 data cost (EDGAR + forward RSS) but forward calendar time; T3 (gated, O10 B/E and O6). |

#### M-5 — Typed-classifier regime gate benchmark (Jev/Laya) as an off-path arm (proposal; T3, rank 5, optional)

| Item | Detail |
|---|---|
| Hypothesis | A Laya (open, local) regime tag (trend/chop/high-vol) from a compact text description of state adds value over a plain vol/volume rule as a gate on M-1/M-2 or on the O11 Bollinger reversion. |
| Features | typed text description of state (regime); plain vol/volume gate as the baseline. |
| Label | Regime-conditional net PnL of the base strategy; compare with a GBT/logistic gate and no gate. |
| Split | Offline on recorded data only; same purged walk-forward as the parent study. |
| Cost model | Local GPU/CPU only; Jev excluded (closed, per-call cost, no reproducibility). |
| Pass rule | Uplift CI > 0 over the simplest non-ML gate; if it fails (the likely outcome), close it and delete the idea — expected to fail per the literature above. |
| Data cost / tier | 0 data cost; T3 (gated, optional, low EV). |

**Jev/Laya identification (secondary sources).** TypeSafe AI's closed, hosted **Jev** (70–500 ms per call; independently measured 264–276 ms) and the open **Laya** (Apache-2.0 encoder classifier on ModernBERT-large/mmBERT-base; 7–40 ms per call; ~7 ms/question batched on a T4) are typed-decision text classifiers released Sept 2026; community `jev-trade`/`jev-hyperliquid` repos run Jev on Hyperliquid perps. No published after-cost out-of-sample evidence exists (a survey of Jev finance projects found only dry-run/paper results). Latency is 70×–5,000× above the 100–250 µs tick-to-order budget ([`docs/GOAL.md`](../docs/GOAL.md) §5.2), so these can only ever be an off-path benchmark arm (M-5). The identification comes from **secondary sources** (blog posts and GitHub repos), not a paper or published weights: https://akmaier.substack.com/p/laya-jev-and-the-return-of-the-discriminative, https://gist.github.com/drillan/6916b16e8ea31a8ec36c8f59d6483150.

### 13.10 Rigor for episode studies

Fast (episode) studies (§13.3–13.6) are easy to fool: a point estimate over all days, with parameters (`buffer_bps`, `stale_ms`, `merge_ms`, pairs, grid cells) chosen while looking at the whole sample, no uncertainty, and no competition model. §13.8 already has the slow-signal equivalents (pre-registration, OOS, baselines). These rules are required for every study whose verdict rests on episodes.

| Rule | Requirement |
|---|---|
| Pre-registration | Before running on data, the study file declares markets/pairs, the full parameter grid, the headline cell rule, and the verdict metric. `research/REGISTRY.md` records the git SHA of that declaration (§13.13). Every cell tested is reported. |
| Chronological split | Days split 60/40 in time. Pair selection and parameter choice use the first 60% only; **the headline is the last 40%**. Preliminary reports (≥ 3 days) are labeled "in-sample only". |
| Uncertainty | Day-block bootstrap (resample days, 2,000 draws) → 90% CI for `usd_per_day_L` and `apr_L`. PASS requires the **CI lower bound ≥ `apr.floor`**, not just the point ≥ target (§13.6). |
| Multiple testing | A study scanning K (pair × cell) combinations reports K and applies a Holm/Bonferroni-style haircut or, simpler, requires the chosen cell to also pass on the OOS 40% with the in-sample-chosen parameters (no re-selection). The ranking uses OOS numbers. |
| Fill competition | "Episode still open at t+L" is necessary, not sufficient. Using `trades`: if the quoted size at the episode price was hit/lifted by someone else in `[t_start, t_start+L]`, capture is `max(0, displayed_sz − traded_sz)` (a queue-position-free lower bound). Report both naive and competition-adjusted capture; the verdict uses the adjusted one. |
| Latency jitter | Evaluate with `L` drawn from a distribution (default: lognormal with the grid value as median, p99 = 3× median) in addition to fixed `L`. Report both; the verdict uses jittered `L` at the headline. |
| Clock skew | Cross-venue studies (O5, O10 Part A) use local receive time `t_ns` only (one clock) and report the exchange-time skew distribution; results must not change sign under ±25 ms shifts of one feed (a sensitivity row, §13.7). |
| Adverse selection | For every captured episode, report the mid move over +1 s / +10 s after the hypothetical fill (markout). A study whose PnL is mainly positive at `t+L` but negative by +10 s markout is flagged: the "edge" may be stale-quote noise that reverts. |
| Artifact checks | Automatic, before hand checks: crossed/locked books, one-sided books, px outliers (> 5σ vs the 1 s median), stale feeds just before a gap, episodes starting within 1 s of a reconnect. Artifacts are excluded and counted. |
| Capacity | Report the `usd_per_day` vs `max_notional` curve (book-walk slippage makes it concave); the capital grid is the x-axis. A required plot. |
| Decay / regime | Per-week `usd_per_day` plot + linear trend; a negative trend with p < 0.1 is noted as a risk in the verdict. |

### 13.11 Historical backfill lane

Historical sources give **lower-fidelity preliminary** results. They can kill an idea early (an idea that fails on generous historical assumptions will not pass forward) and prioritize which studies to run first on forward data; they **cannot** PASS a fast study on their own. New verdict qualifier: **HIST-PRELIM** (with the fidelity class); it is never enough for gate G1.

| Fidelity class | Examples | Allowed use |
|---|---|---|
| H1 tick/update-level with ms stamps | Tardis full-depth or `bbo` updates; Binance/Bybit book tickers | Episode studies at L ≥ 100 ms; still HIST-PRELIM |
| H2 periodic snapshots (seconds) | HL S3 `l2Book` snapshots, `asset_ctxs` | Episode studies at L ≥ snapshot period only; duration stats censored |
| H3 bars / funding | `candleSnapshot`, `fundingHistory`, Binance klines | Slow-signal (§13.8): O7 carry, O10 C/E, O11 B |

**Kill rule.** A study that FAILs at H1/H2 fidelity under **generous** assumptions (L = snapshot period, no competition, maker fees) is deprioritized — moved to the bottom of the forward queue — not deleted.

Concrete sources (histdata review, 2026-09-28):

| Source | Access | Covers (§13.1) | Cost |
|---|---|---|---|
| Tardis free days | no key; the first day of each month, every exchange/type | HL `bbo`/`trades`/`ctx`/`book`, Binance/Bybit `bbo`, Deribit (optional); 15 free HL days (2025-07-01…2026-09-01) and 9 HIP-3 days [probed] | free; every day is paid (monthly plans from ~$350/mo, min $300) |
| HL REST `/info` | public | `funding_hist` (pages to listing, incl. HIP-3); `bars` from candles (1m ≈ 3.5 d, 1h ≈ 208 d, 4h ≈ 2.3 y, 1d all); `markets` [probed] | free |
| `hyperliquid-archive` (S3) | requester-pays; needs an AWS account | main-dex `l2Book` → `book`/`bbo`, `asset_ctxs` → `ctx`; ~monthly, no timeliness guarantee; no spot, no candles [probed/doc] | egress ~$0.09/GB (us-east-1) |
| `hl-mainnet-node-data` (S3) | requester-pays; needs an AWS account | `node_fills_by_block` → `trades` (wallets + liquidation marker), funding events in `misc_events_by_block`; ~0.8–1.0 GiB/day [probed/3p] | egress (~$0.11/GB, ap-northeast-1) |
| Hydromancer Reservoir (S3) | requester-pays; needs an AWS account | 1 s `bars` (all HL markets), fills with liquidation/ADL flags → `trades`, 1-min L2 → `book` [probed/doc] | free + egress |
| Binance `data.binance.vision` | public | CEX `trades`/`bars`/`funding_hist`; `bookTicker` ended 2024-03-30 [probed] | free |
| Bybit public + quote-saver | public | CEX `trades`; full-book `bbo` (ob200/ob500) [probed] | free |
| Deribit history | public | `deribit_options` (trade IV; deltas computed) for O9 P3 [probed] | free |
| Alpaca / Massive | account | equity `bars` for O9a, O10 B/E and the O11 stock proxy [doc/3p] | free tier |

Per-study confidence for a preliminary (HIST-PRELIM) backtest now:

| Study | Historical source(s) | Fidelity lost vs forward recording | Confidence |
|---|---|---|---|
| O1 | Tardis 9 HIP-3 days (`book_ticker` + `derivative_ticker`), `fundingHistory`, Hydromancer 1-min L2 / 1 s candles | block `bbo` is as good as ours, but only 1 day/month (biased: 3 weekends, 1 holiday); `abcd` dex missing; reveal lag not in the data | Medium (prelim) |
| O2 | Tardis 15 spot days + `spotMeta` | @N mapping for delisted/renamed pairs; quote pairs only from their listing dates | Medium |
| O3 | Tardis spot + perp days + `derivative_ticker` | no cross-day continuity of the rolling basis | Medium–high |
| O4 | — (needs R-9 forward) | — | n/a |
| O5 | Part 1: Tardis HL + Binance + Bybit `book_ticker` (15 d); Part 2: Tardis only; node fills + CEX dumps for more days | HL is block-quantized (10/50 ms grid unresolvable); the ~230 ms reveal lag dominates; Binance BBO only via Tardis | Medium (P1), low–medium (P2 ≤ 100 ms) |
| O6 | Tardis no-flag trades, or node/Hydromancer fills; Hydromancer 1 s candles; CEX `aggTrades` | mid path from 1 s trade candles is noisier than `bbo`; the +1 s horizon is marginal | Medium |
| O7 | `fundingHistory` (full) + HL candles (1h/4h/1d) + Hydromancer 1 s | `predictedFundings` has no history: the entry rule must use trailing realized funding only | High |
| O8 | — (desk research + HyperEVM forward) | — | n/a |
| O9 (P3) | Deribit `get_last_trades_by_currency_and_time` + DVOL (free, years) | trade IV, no greeks; deltas computed from IV | Medium–high |
| O10 C | `fundingHistory` for HIP-3 + Tardis `derivative_ticker` (`asset_ctxs` has no HIP-3) | hourly premium only; closed-hours oracle behavior needs V-12 | High (funding), medium (premium) |
| O10 D | Tardis HIP-3/BTC/ETH days; Hydromancer 1 s candles; HL 1m candles | episodes only on the 9 free days; sparse night prints look like lag | Medium (existence), low–medium (PnL) |
| O10 E | HL candles 4h/1h + Hydromancer 1 s; Alpaca/Massive stock opens | HIP-3 age caps history: < 30 OOS weekend signals ⇒ INCONCLUSIVE for weekend variants | Medium (overnight), low (weekend) |
| O11 A | Tardis `bbo` mids (sub-minute cells); Hydromancer/HL candles (≥ 1 m) | sub-minute cells limited to the free days; noisy spreads on illiquid legs | Medium (≥ 1 m), low–medium (1/10 s) |
| O11 B | HL REST 1h/4h/15m; Hydromancer 1m/5m; Binance 1m proxy for years | the Binance proxy ignores the HL basis/funding; trade-based opens | High (1h/15m), medium (1m/5m) |
| O11 C | inherits the O9 / O10 E data it filters | — | same as the parent study |

**Common caveats.** First-of-month days are not a random sample: report per-day results and `concentration`. 15 non-contiguous days meet the ≥ 14-day count only as PRELIM; never promote a strategy on backfill alone. Historical data has exchange time, not our reveal time: add the measured HL publish lag to `L` (§13.4, B-9) or run on `local_timestamp`.

### 13.12 Research cadence

1. Survey data (V-8+) → import via the backfill lane (B-tasks) → HIST-PRELIM runs of O7, O11 Part B, O10 Parts C/E, O3, O1 (H2/H3 fidelity) → rank.
2. Forward recorder deployed (R-10) → ≥ 3 days → preliminary forward reports for the top-ranked studies.
3. ≥ 14 days → final reports → `RANKING.md` → ADR-0002 (owner approves).
4. Weekly: re-run every registered study on new data (a scheduled `hlr` rerun job); decay plots update; the `RANKING.md` diff is reviewed.

### 13.13 Hypothesis registry

`research/REGISTRY.md` holds one row per hypothesis ever proposed: id, title, source (spec / agent / owner), status (proposed / pre-registered / running / PASS / MARGINAL / FAIL / INCONCLUSIVE / parked), the pre-registration git SHA, a report link, and a one-line reason for the status. Studies O1–O23 are listed there. Nothing is deleted: failures stay as a graveyard, so the same idea is not re-tested with fresh parameters until it passes.

---

## 14. Work breakdown

Status: ☐ not started · 🔄 in progress · ✅ done. Size: **S** ≤ ½ day, **M** ≤ 2 days, **L** ≤ 5 days (for a focused agent).

### 14.0 Priority tiers (read before picking a task)

Work is tiered so the arb core isn't starved by the directional family ([`docs/GOAL.md`](../docs/GOAL.md) §2.1).

| Tier | What | Rule |
|---|---|---|
| **T0: fix-first** | Known defects on the real-money path ([`docs/GOAL.md`](../docs/GOAL.md) §2.2). In this spec: **R-3** (stream watchdog, jitter, cancellable reconnect) | **Before everything else.** |
| **T1: arb core** (latency-first) | Recorder, research toolkit, fast dislocation / arb studies, and the HyperEVM MEV feasibility desk research | **Always pick T1 first (after T0).** |
| **T2: adjacent** | Studies that reuse T1 data (flow, carry, funding patterns) or are blocked on heavier infrastructure (HyperEVM RPC / node) | When every T1 task is done, blocked, or already taken. |
| **T3-data: directional-family data collection** | Small tasks that start the clock on forward data (options chains, Deribit) | **Allowed any time.** They're small, and calendar time is the constraint. |
| **T3: directional family** (signals held hours–days) | The slow-signal backtester and the O9, O10 B/E, and O11 B/C studies | Starts only when **both**: (1) R-10 is ✅ (recorder in production), and (2) ≥ 3 T1 studies have preliminary reports. |

Strategy code for any tier still waits for gate G1 (or an owner-approved G1.5 pilot).

### 14.1 Summary table

| ID | Title | Tier | Size | Depends on | Status |
|---|---|---|---|---|---|
| V-1 | Verify HL WS/REST facts | T1 | S | — | ✅ |
| V-2 | Verify CEX endpoints, fields, fees, reachability | T1 | S | — | ✅ |
| V-3 | Verify HIP-3 fees and the spot quote-token set | T1 | S | — | ✅ |
| V-4 | Measure latency from candidate regions; pick a host | T1 | M | R-6 (`hl probe latency`) | ☐ |
| V-5 | Verify HyperEVM facts (blocks, mempool, gas, Core↔EVM transfers) | T1 | M | — | ☐ |
| V-6 | Build the HyperEVM pool list | T2 | M | V-5 | ☐ |
| V-7 | Measure data volume per stream | T1 | S | R-6 | ☐ |
| V-8 | Check the HL public S3 archive for backfill | T1 | S | — | ☐ |
| V-9 | Options data sources (Yahoo chains, finsnap history) + HIP-3 stock universe mapping | T3-data | S | — | ✅ |
| V-10 | Verify the Deribit public API (endpoints, fields, limits, reachability) | T3-data | S | — | ✅ |
| V-11 | Choose a real-time US equities data provider (owner approves) | T1 | S | — | ☐ |
| V-12 | HIP-3 stock-perp mechanics (oracle in/out of hours, funding, fees, leverage, halts) | T1 | S | — | ☐ |
| V-13 | Historical options data: vendors, coverage, cost; owner decides whether to buy | T3-data | S | V-9 | ☐ |
| R-1 | `hl-arb-recorder` crate skeleton + envelope types | T1 | S | — | ✅ |
| R-2 | Segment writer (zstd, rotation, manifest, crash recovery, disk guard) | T1 | M | R-1 | ✅ |
| R-3 | Extract `RawWsConn` (watchdog, jitter, cancel, gap events); rebase `WsMarketStream` on it | **T0** | M | — | ✅ |
| R-4 | Subscription planner + universe selectors | T1 | M | R-1 | ✅ |
| R-5 | HL REST snapshotter with weight budget (incl. candle backfill) | T1 | M | R-1, R-2 | ✅ |
| R-6 | `hl record` / `record plan` / `probe latency` CLI, profiles, metrics, health | T1 | M | R-2, R-3, R-4, R-5 | ✅ |
| R-7 | Segment reader + `hl record inspect` / `verify` | T1 | S | R-2 | ✅ |
| R-8 | Binance/Bybit sources | T1 | S | R-3, R-6, V-2 | ✅ |
| R-9 | HyperEVM pool source | T2 | L | R-6, V-5, V-6 (+ SPEC-0009 node or a provider) | ☐ |
| R-10 | Deploy recorder (systemd, chrony, runbook, optional shipping) | T1 | M | R-6, R-7, V-4 | ☐ |
| R-11 | Options-chain sources: `yahoo-options` chains (all fields) + optional `finsnap` `/snap` poller | T3-data | S | R-5, V-9 | ☐ |
| R-12 | `deribit` options summary source | T3-data | S | R-5, V-10 | 🔄 |
| R-13 | `equities` real-time quote source | T1 | M | R-3, V-11 | ☐ |
| P-1 | `research/` scaffold + segment reader in Python | T1 | S | R-2 (format frozen) | ✅ |
| P-2 | Normalizer → Parquet tables (§13.1) | T1 | M | P-1, V-1 | ✅ |
| P-3 | Cost model module + `costs.toml` + `thresholds.toml` | T1 | S | P-1, V-2, V-3 | ✅ |
| P-4 | Episode detector + latency capture (§13.3–13.5) | T1 | M | P-2, P-3 | ✅ |
| P-5 | Report template + `RANKING.md` generator | T1 | S | P-4 | ✅ |
| P-6 | Slow-signal backtester (§13.8) | T3 | M | P-2, P-3, T3 gate | ☐ |
| P-7 | Read-only import of finsnap's `option_snapshots` history (for O9a) | T3 | S | P-2, V-9, T3 gate | ☐ |
| B-1 | Tardis free-days downloader | T1 | S | P-1 | ✅ |
| B-2 | Tardis → §13.1 normalizer with symbol mapping | T1 | M | B-1, P-1 | ✅ |
| B-3 | HL REST funding + candles backfill + daily 1m candle poller | T1 | S | P-2 | ✅ |
| B-4 | Binance/Bybit public dumps | T1 | M | P-2 | ☐ |
| B-5 | Hydromancer Reservoir (**owner AWS account**) | T1 | M | B-2, owner approval | ☐ |
| B-6 | Official HL S3 archives (**owner AWS account**; completes V-8) | T1 | M–L | owner approval | ☐ |
| B-7 | Deribit history | T1 | S–M | V-10 | ☐ |
| B-8 | Equity minute bars (**owner account**; after V-11) | T1 | S | V-11 | ☐ |
| B-9 | HIST-PRELIM report plumbing | T1 | S | P-5 | ✅ |
| S-1 | Study O1 HIP-3 dislocations | T1 | M | P-5 | ☐ |
| S-2 | Study O2 spot triangles | T1 | M | P-5 | ☐ |
| S-3 | Study O3 spot-perp dislocation | T1 | S | P-5 | ☐ |
| S-4 | Study O4 Core↔EVM | T2 | L | P-5, R-9 | ☐ |
| S-5 | Study O5 CEX lead-lag | T1 | M | P-5, R-8 | ☐ |
| S-6 | Study O6 liquidation/flow events | T2 | M | P-5 | ☐ |
| S-7 | Study O7 funding carry | T2 | M | P-5 | ☐ |
| S-8 | Study O8 HyperEVM MEV feasibility (Q1–Q4 desk research T1; Q5 pool data T2) | T1 | M | V-5 (+ V-6, R-9 for Q5) | ☐ |
| S-9 | Study O9 options positioning (O9a history on stocks; O9b forward on HL perps) | T3 | L | P-6, P-7 (O9a); + R-11, R-12, V-12, ≥ 20 trading days (O9b) | ☐ |
| S-10a | Study O10 Parts A + D: stock perp vs stock in hours; closed-hours lead-lag | T1 | M | P-4, V-12, R-13 (Part A) | ☐ |
| S-10b | Study O10 Part C: HIP-3 stock-perp funding and premium by session | T2 | S | P-4, V-12 | ☐ |
| S-10c | Study O10 Parts B + E: open convergence; weekend/overnight → next session/week | T3 | M | P-6, V-12 | ☐ |
| S-11a | Study O11 Part A: Bollinger bands on spreads (stat-arb) | T1 | M | P-4 | ☐ |
| S-11b | Study O11 Parts B + C: single-instrument bands; bands as a filter | T3 | M | P-6, R-5 candle backfill | ☐ |
| S-12 | Study O12 oracle-tick lag and mark-price trigger cascade | T1 | M | P-5, R-8 | ☐ |
| S-13 | Study O13 HIP-3 deployer-oracle stair-step / stale fallback | T1 | M | P-5, V-12 | ☐ |
| S-14 | Study O14 funding-settlement timing | T2 | S | P-5, V-3, V-12 | ☐ |
| S-15 | Study O15 liquidation-cluster ladder | T2 | L | P-5, public-positions/node source | ☐ |
| S-16 | Study O16 public TWAP-flow prediction | T2 | M | P-5, node/TWAP source | ☐ |
| S-17 | Study O17 HIP-4 outcomes: digital vs Deribit + 06:00 pin | T2 | M | P-5, outcomes source, V-10 | ☐ |
| S-18 | Study O18 cross-dex funding-differential carry | T2 | S | P-5, V-3, V-12 | ☐ |
| S-19 | Study O19 quote-asset peg defense + true fee multipliers | T1/T2 | S | P-5, V-3 | ☐ |
| S-20 | Study O20 portfolio-/cross-margin contagion forced flow | T2 | L | P-5, public-positions/node source, V-12 | ☐ |
| S-21 | Study O21 HIP-2 Hyperliquidity deterministic-quote pickoff | T2 | M | P-5 | ☐ |
| S-22 | Study O22 read-precompile / CoreWriter-delay asymmetry | T2 | L | P-5, R-9, V-5, V-6, node/RPC | ☐ |
| S-23 | Study O23 HYPE realized-vs-implied vol carry | T3 | L | P-6, R-11/R-12, EVM options source, V-10, T3 gate | ☐ |
| D-1 | `RANKING.md` + ADR-0002 + first strategy spec stub | T1 | S | all S-tasks that are feasible | ☐ |

**Critical path to "recording in production":** R-1 → R-2 → R-4/R-5 (parallel with R-3) → R-6 → V-4 → R-10. Get this done first; the research tasks can start once a few days of data exist.

**Suggested parallel lanes for multiple agents:**

| Lane | Tasks in order |
|---|---|
| A (recorder core) | R-1 → R-2 → R-5 → R-7 |
| B (connectivity) | R-3 → R-4 → R-6 → R-8 |
| C (facts) | V-1, V-2, V-3, V-8, V-5 → V-6 |
| F (T3-data, allowed early) | V-9, V-10, V-13 → R-11, R-12. Starts the clock on forward options data (O9b needs weeks). No T3 *studies* until the T3 gate. |
| G (equities for T1) | V-11, V-12 → R-13 (feeds S-10a) |
| D (ops) | V-4 → R-10 → V-7 |
| E (research, after ~3 days of data) | P-1 → P-2 → P-3 → P-4 → P-5 → T1 studies (S-1, S-2, S-3, S-5, S-8, S-10a, S-11a) → T2 → T3 after the gate |
| H (backtest now) | B-1 → B-2 → B-3 → B-9 → HIST runs of O7, O11 B, O10 C/E, O3, O1 |

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
- **Done when:** §15 has an "S3 archive" block; if usable, the importer is **B-6** (§14.2), which completes this task (don't implement it now).

#### V-9 — Options data sources + HIP-3 stock universe mapping
- **Do:** (1) Using finsnap's collector as a reference (`../finsnap/apps/backend/src/collectors/options.ts`, `yahooSession.ts`), document Yahoo's options endpoint: URL, session/crumb requirements, every field per contract (OI, volume, `impliedVolatility`, bid, ask, `lastPrice`), data delay, and safe request pacing. (2) Document finsnap's `option_snapshots` table (columns, date range, symbols) and its `/snap` options JSON, for the read-only uses in §9.1. (3) List every HIP-3 stock/index perp (`hl dexs`, `hl markets --dex …`) and map each one to its underlying stock/ETF and options symbol in `research/mappings/underlyings.toml`.
- **Done when:** §15 has the Yahoo and finsnap blocks; the mapping file covers every HIP-3 stock perp with a liquid US options chain (or marks it "no chain").

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
- **Research finding (2026-09-29, `docs/research/data-sources-2026-09-29.md` §1.2/§1.5).** Start with **Alpaca free** (IEX-only, 15-min delayed via API, real-time WS capped at 30 symbols) for the O10 Part A pre-check; only if O10 Parts A/D shows edge consider **Massive Stocks Advanced 199 USD/mo** (real-time consolidated) or **Alpaca Algo Trader Plus 99 USD/mo**. No real-time equity feed exists at ~20 USD/mo. Sources: https://alpaca.markets/data, https://massive.com/pricing. Prices are 2026-09-29 list prices and change; re-check before buying. The Do/Done-when above are unchanged.

#### V-13 — Historical options data
- **Why:** O9a can test options signals on years of history instead of waiting months for forward collection. That requires past option chains (per strike: open interest, volume, and ideally IV) that neither Yahoo nor finsnap has before finsnap's start date.
- **Do:** (1) Measure what finsnap already has: date range and symbols in its `option_snapshots` table. (2) Compare ≥ 3 vendors (e.g. ThetaData, ORATS, CBOE DataShop, Polygon.io options, Databento OPRA, historicaloptiondata.com) on: symbols (the HIP-3 underlyings + SPY/QQQ), depth of history, daily vs intraday, fields (OI, volume, IV, greeks), format, and price. (3) Recommend buy / don't buy with a cost.
- **Done when:** §15 has the finsnap coverage and the vendor table; the owner's decision is recorded.
- **Research finding (2026-09-29, `docs/research/data-sources-2026-09-29.md` §1.2/§1.5).** The cheapest route to the ≥ 2-year daily US options positioning target is the **EODHD US Options add-on at 29.99 USD/mo** (39.99 first 3 months): 6,600+ US underlyings, EOD since Q4 2023 (≈ 2.9 years today), with OI, volume, bid/ask, IV and five greeks (https://eodhd.com/lp/us-stock-options-api), plus **EODHD EOD 19.99 USD/mo** for stock bars (https://eodhd.com/pricing). Longer-history options: HistoricalData.net 199–590 USD, ThetaData 40 USD/mo (6 years). Personal-use licence; the storage/automation clause is **UNVERIFIED** and must be read before pulling. **Buying is an owner decision — no purchase has been made or authorised.** Bought data goes under `research/data/` and is never committed (§17 #27). Prices are 2026-09-29 list prices and change; re-check before buying. The Do/Done-when above are unchanged.

#### V-12 — HIP-3 stock-perp mechanics
- **Do:** For each HIP-3 dex listing stock/index perps: how the oracle/mark is set during US regular hours, pre/post market, overnight, and weekends; funding formula and cadence; fees (links to V-3); max leverage; trading halts and behavior around corporate actions (splits, dividends, earnings). Sources required.
- **Done when:** §15 has a per-dex table.

#### R-1 — `hl-arb-recorder` crate skeleton + envelope
- **Do:** Create `crates/hl-arb-recorder` (add it to the workspace `members`). Define `Envelope` (§5.1) and `Kind` (§5.3) with `serde`, plus constructors that take `t_ns`/`mono_ns` from an injectable clock (reuse the `hl_arb_core::clock::Clock` pattern; add a monotonic-ns source). `raw` is stored as `String` and serialized as a JSON string.
- **Files:** `Cargo.toml` (workspace), `crates/hl-arb-recorder/{Cargo.toml,src/lib.rs,src/envelope.rs}`.
- **Tests:** golden serialization for every kind; a round-trip test; a test that `raw` containing quotes, newlines and unicode survives the round-trip byte-exact.
- **Done when:** the crate builds, tests pass, and there are no new deps beyond `serde`, `serde_json`, and workspace crates.

#### R-2 — Segment writer
- **Do:** Implement `SegmentWriter`: a dedicated OS thread (same pattern as `hl_arb_core::db::writer::DbWriter`) that receives envelopes over a bounded `sync_channel` (capacity configurable, default 65 536), writes `segment_open` first, compresses with `zstd` (add the `zstd` crate to workspace deps), flushes every ≤ 5 s, and rotates per §6 (hour boundary or 1 GiB raw). Finalize and write the manifest per §6. Crash recovery for `.partial` per §6. Disk guard per §6. Expose `try_send(env) -> bool`; on `false` the **caller** increments `hl_rec_dropped_total` and sends a `gap_start{reason:"drop"}` as soon as the channel accepts again.
- **Tests (tempdir):** rotation at an hour boundary using an injected clock; rotation on size; the manifest line matches the file; a `.partial` left from a simulated crash becomes `.crashed` on restart; records written ≡ records read back (with R-7's reader, or a minimal decoder in the test).
- **Done when:** tests pass; a benchmark note in the commit message shows ≥ 50k envelopes/s written on the dev machine.
- **Implemented (2026-09-27, R-1 + R-2):** `crates/hl-arb-recorder` with `envelope.rs` (`Envelope`, `Kind`, `SegmentOpenMeta`, `EnvelopeClock`/`MonoClock`/`SystemEnvelopeClock`/`FixedEnvelopeClock`) and `segment.rs` (`SegmentWriter`, `SegmentConfig`, `DiskSpace`/`SystemDiskSpace` over `statvfs`). The writer is one OS thread per `(src, conn)` on a bounded `sync_channel` (default 65 536), zstd level 3, rotates on UTC hour or 1 GiB raw, finalizes/manifests/fsyncs, and recovers `.partial` → `.crashed`. Throughput measured in the test binary: ~295k envelopes/s (debug, 100k envelopes in 338.6 ms) ≥ the 50k target. New workspace deps: `zstd` (compression), `thiserror` (typed errors), `libc` (`statvfs` free space). Open questions recorded in §17 (#8–#10): writer-originated `seq`, `records`/`bytes_raw` scope relative to `segment_close`, and the disk guard being per-stream until R-6's multi-stream coordinator exists.

#### R-3 — `RawWsConn`
- **Do:** Implement §7.5 in `crates/hl-arb-client/src/raw_ws.rs`. Make the URL, subscribe payloads, and keepalive message pluggable via a small `Protocol` trait (HL / Binance / Bybit implementations come later; ship HL now). Rebuild `WsMarketStream` on top of `RawWsConn` (it decodes the `Text` events with the existing `decode`). Add `rand` for jitter if it's not already present.
- **Tests (local mock WS server):** reconnect after the server closes; resubscribe order preserved; the watchdog fires when the server goes silent; a `Gap` then `Opened` is emitted; cancellation stops a reconnect loop mid-backoff; all existing `ws.rs` tests still pass.
- **Done when:** tests pass and `hl watch BTC` still works manually.
- **Implemented (2026-09-26):** `raw_ws.rs` ships `RawWsConn`, the `Protocol` trait (+ `HlProtocol`), and `RawEvent`; `WsMarketStream` is rebased on it. Jitter uses a std-hasher PRNG rather than adding the `rand` crate (no new dependency; spreading reconnect storms does not need cryptographic randomness). `RawEvent::Opened` is yielded after each (re)connect and `Gap` carries `watchdog`/`closed`/`error`/`shutdown`. The watchdog resets on any inbound frame including `pong`.

#### R-4 — Subscription planner
- **Do:** Implement the §7.3 selectors and the §7.4 algorithm in `crates/hl-arb-recorder/src/planner.rs`. Input: profile + `AssetMap` + the ctx data needed for `top:N` (pass it in; the planner does no I/O). Output: a `Plan` with a pretty table `Display`.
- **Tests:** fixture `AssetMap`s → golden plans; the over-budget drop order; the priority-1 failure; `l2Book` isolation; determinism (shuffled input ⇒ same plan); pacer math (≤ 20 msg/s).
- **Done when:** tests pass.

#### R-5 — HL REST snapshotter
- **Do:** Implement §8 in `crates/hl-arb-recorder/src/sources/hl_rest.rs`: a task that schedules each request at its cadence through a weight token bucket (300/min default), records `rest` envelopes (raw body + `meta.req/status/latency_us`), and tracks `fundingHistory` paging state (the last fetched time per coin, persisted in a small JSON state file in `out_dir`).
- **Tests:** `wiremock` `/info` → envelopes contain the raw bodies; the token bucket delays over-budget requests; paging resumes from state after a restart.
- **Done when:** tests pass.

#### R-6 — CLI, profiles, metrics, health
- **Do:** Add the `record` (with `plan`) and `probe latency` subcommands to `crates/hl-arb-bot/src/main.rs` (move the recorder wiring into a new `crates/hl-arb-bot/src/record.rs` module to keep `main.rs` manageable). Load `config/record.toml` (create it from §7.3) via `figment`, overridable by `HL_RECORD_*` env vars. Wire planner → `RawWsConn`s (paced) → `SegmentWriter`; the REST snapshotter; the `clock` envelope task; `/healthz` `/readyz` `/metrics` with the §12.2 metrics; graceful shutdown (SIGTERM ⇒ `gap_start{shutdown}` on every conn, then finalize segments).
- **Tests:** `hl record plan` on a fixture; an integration test with mock WS + mock REST that runs ~2 s and asserts files + manifest exist and contain `segment_open`, `sub`, `frame`, `segment_close`.
- **Done when:** tests pass; a manual 10-minute mainnet run produces readable segments (`hl record inspect`) with no gaps other than startup.
- **Implemented (2026-09-27).** `crates/hl-arb-bot/src/record.rs` (+ `mod record;` and `Record`/`Probe` subcommands) wires planner → one `RawWsConn` per connection (staggered dials, initial-dial retry) → per-`(src,conn)` `SegmentWriter`; the R-5 `RestSnapshotter`; per-WS-conn `clock` envelopes; `/healthz` `/readyz` `/metrics`; SIGTERM/SIGINT ⇒ `gap_start{shutdown}` then finalize. `config/record.toml` is the §7.3 example (bot uses port 9091 to avoid colliding with 9090). The §12.2 `hl_rec_*` names are in `hl-arb-metrics`. A ~2 s mock-WS + `wiremock`-REST integration test asserts `segment_open`/`sub`/`frame`/`segment_close`; a 40 s real mainnet run produced 9 segments / 23 485 records and `hl record verify` passed. **The full 10-minute production soak and the manual sign-off are left to the operator (R-10 host).** Open questions in §17 (#18–#23); the missing `hl-arb-recorder` stats/clock/liveness APIs are the main follow-up.

#### R-7 — Segment reader + inspect/verify
- **Do:** `crates/hl-arb-recorder/src/reader.rs`: iterate the envelopes of a file (tolerates a truncated tail in `.crashed` files); merge several files by `(t_ns, conn, seq)`. Implement the `hl record inspect` and `hl record verify` outputs from §12.1.
- **Tests:** a truncated file reads up to the last full line; merge order; `seq` hole detection.
- **Done when:** tests pass.
- **Blocks:** ~~SPEC-0010 **E-7 part 2** (`hl replay` over recorder segments)~~ **unblocked and delivered (2026-09-27):** `hl-arb-bot/src/replay.rs` + `hl replay --from … --to … --out actions.jsonl` drive the v2 engine over these segments (SPEC-0010 E-7 part 2; §23 Q-Replay-Gap resolved). A follow-up added `reader::segments_for` (inclusive UTC date range, `.crashed` included) as the driver's input.

**R-4 / R-5 / R-7 implemented (2026-09-27).** `planner.rs` ships the §7.3 selectors and the §7.4 algorithm (`plan`, `Plan`/`Connection`/`Pacer`/`VolumeIndex`, deterministic golden plans, over-budget drop order, `l2Book` isolation). `sources/hl_rest.rs` ships `RestSnapshotter` with the 300/min weight bucket, raw-body `rest` envelopes, and persisted `fundingHistory`/`candleSnapshot` paging; because `HttpInfo` discards the raw text, R-5 uses a local `RawInfoClient` (reqwest) rather than changing `hl-arb-client`. `reader.rs` ships `read_envelopes` (truncated-tail tolerant), `merge_segments`, and the pure `inspect`/`verify` analyses. The `hl record` CLI, profiles, metrics, and health are **R-6**, still open. Open questions recorded in §17 (#11–#17).

#### R-8 — CEX sources
- **Do:** `Protocol` implementations for `binance-usdm`, `binance-spot`, `bybit-linear` per §9 (confirmed by V-2). Add a `[cex]` section to the profile. Each venue gets its own `src` directory.
- **Tests:** mock-server tests for the subscribe format and the Bybit ping cadence.
- **Done when:** tests pass; a manual 10-minute run shows ticker frames for every configured symbol.
- **Implemented (2026-09-29).** `crates/hl-arb-recorder/src/sources/cex.rs` ships the three `Protocol`/source implementations; `crates/hl-arb-bot/src/record.rs` spawns one per non-empty `[profile.default.cex]` list, each with its own `(src, src)` `SegmentWriter` under the mount guard. CEX liveness is registered with the readiness monitor but is **non-gating**: a stale reference feed logs a WARN and never takes the recorder out of `/readyz`. The 10-minute run against the real hosts (the Done-when above) is still to do.

#### R-9 — HyperEVM pool source
- **Do:** Implement §10 with Alloy (`alloy` provider with the `ws` feature, `sol!` for `IUniswapV2Pair.getReserves`, `IUniswapV3Pool.slot0/liquidity`, and `Multicall3.aggregate3`). One multicall per new block, at that block number. Handle reconnects by emitting gaps (reuse the envelope kinds). Add `enabled=false` by default.
- **Tests:** unit tests for multicall encoding and decoding with fixed vectors; a mock JSON-RPC test for the per-block flow.
- **Done when:** tests pass; a manual 10-minute run against the configured RPC records one state snapshot per block for every pool in the list.

#### R-11 — Options-chain sources
- **Do:** (1) `yahoo-options`: fetch full option chains (every expiry) for the mapped underlyings + SPY/QQQ on the §9.1 cadence, gated to US market hours with an exchange calendar, with V-9's session handling and pacing; record raw responses as `rest` envelopes. (2) Optional `finsnap` poller of `GET /snap` (`HL_FINSNAP_URL`, disabled by default) for comparison. Reuse R-5's scheduling; back off on errors; never fail the recorder if either source is down (emit gaps).
- **Tests:** `wiremock` → envelopes for both; the market-hours gate on a fixed-clock test (weekday / weekend / holiday); a session-refresh test for the Yahoo crumb.
- **Done when:** tests pass; a 1-day run shows the expected snapshot count per underlying.

#### R-12 — `deribit` source
- **Do:** Poll the §9.1 Deribit endpoints every 60 s for the configured currencies; record `rest` envelopes under `src:"deribit"`. Respect V-10's rate limits.
- **Tests:** `wiremock` → envelopes; cadence.
- **Done when:** tests pass; a 1-hour run shows 60 snapshots per currency.
- **Implemented (2026-09-29).** `crates/hl-arb-recorder/src/sources/deribit.rs` polls `public/get_book_summary_by_currency` and `public/get_index_price` per currency every 60 s and records the raw body as `rest` envelopes; `record.rs` wires it behind `[profile.default.deribit].enabled`, default off (config/record.toml). The 1-hour run (the Done-when above) is still to do. §17 #33 names the per-instrument `ticker`/`get_instruments` fan-out this source does not poll.

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

#### P-7 — Import finsnap history (read-only)
- **Do:** `uv run hlr-import-finsnap --dsn …`: read finsnap's `option_snapshots` (read-only DB user or a dump) and write `options_contracts` partitions with `iv/bid/ask/last` null and `t_data` = the snapshot date at the US close. Never write to finsnap.
- **Tests:** a fixture dump → expected rows; re-running is idempotent.
- **Done when:** tests pass; §15 records the imported date range and symbols.

#### B-1 — Tardis free-days downloader
- **Do:** `uv run hlr-backfill tardis-free --from DATE --to DATE --symbols FILE`: download the Tardis first-of-month CSVs via plain GET from `datasets.tardis.dev/v1/{exchange}/{type}/{YYYY}/{MM}/01/{SYMBOL}.csv.gz` (no key; HEAD/range requests fail). HL: `book_ticker`, `quotes`, `trades`, `derivative_ticker`, `book_snapshot_5`/`_25`; `binance-futures`/`bybit`: `book_ticker`/`trades`; `deribit` off by default (11.7 GB/day). Retries, a local cache under `research/data/backfill/`, and checksums in a manifest. Never load keys.
- **Files:** `research/hlr/backfill/tardis_free.py`, tests.
- **Tests:** a mocked HTTP layer returns fixture CSVs → files land with the expected manifest and checksums; a missing month is recorded as a gap, not a crash; re-running is idempotent.
- **Done when:** tests pass; a real download of one free HL day (BTC `book_ticker`) is described in the commit message (size, rows).

#### B-2 — Tardis → §13.1 normalizer
- **Do:** Map Tardis rows into the §13.1 tables: `book_ticker` → `bbo` (`venue` = `hl`/`binance-usdm`/`bybit-linear`; `ts_exch_ms` = `timestamp`, `t_ns` = `local_timestamp`); `quotes` (pre-2025-06-26) → `bbo` tagged `venue="hl-book"`; `trades` → `trades` (buyer/seller null); `derivative_ticker` → `ctx` (oracle = `index_price`, premium null); `book_snapshot_*` → `book`. Symbol mapping: `XYZ:TSLA` → `xyz:TSLA`, `@N` → `BASE/QUOTE` via `spotMeta`, CEX → `binance-usdm:BTCUSDT`. Synthesize `gaps` for every non-sampled interval and Tardis incident windows. Add a `source` column (`tardis-free`).
- **Files:** `research/hlr/backfill/tardis_normalize.py`; `research/mappings/` additions.
- **Tests:** fixture rows per type → expected rows and venue tags; `XYZ:TSLA`/`@N` mapping; synthesized gaps cover every missing day; idempotent per partition.
- **Done when:** tests pass; one free HL day normalizes into `bbo`/`trades`/`ctx`/`book` with row counts in the commit message.

#### B-3 — HL REST funding + candles backfill and daily 1m poller
- **Do:** One-shot `fundingHistory` for every perp and every HIP-3 dex (page by 500 until caught up) into `funding_hist`; `candleSnapshot` for 1d/4h/1h (max depth) plus 15m/5m/1m (rolling) into `bars(source="candle")`; a `meta`/`spotMeta`/`perpDexs` snapshot into `markets`. Drop `n==0` pre-launch candles. Add a daily job (cron or the R-5 snapshotter) that appends 1m/5m candles so the rolling window stops expiring.
- **Files:** `research/hlr/backfill/hl_rest.py`; snapshotter hook or `deploy/` cron.
- **Tests:** paging stops on an empty page; `n==0` rows dropped; re-running appends only new candles.
- **Done when:** tests pass; `funding_hist` covers BTC from 2023-05 and `xyz:TSLA` from 2025-11; the daily poller is scheduled and its first appended day is recorded.

#### B-4 — Binance/Bybit public dumps
- **Do:** Import Binance `data.binance.vision` `aggTrades`/`trades`, 1m `klines`, monthly `fundingRate`, `metrics`, and `bookTicker` (2023-05-16…2024-03-30 only); Bybit `public.bybit.com/trading/` trades and quote-saver `ob200`/`ob500` (reconstruct top of book from snapshot + deltas, ms timestamps). Write `trades`/`bars`/`bbo`/`funding_hist` with a CEX `venue`.
- **Files:** `research/hlr/backfill/cex_dumps.py`.
- **Tests:** fixture archives → expected rows; the book reconstruction from a snapshot + delta matches a hand-checked top of book.
- **Done when:** tests pass; one day per venue imported with row counts in the commit message.

#### B-5 — Hydromancer Reservoir (owner AWS account)
- **Do:** With owner-provided AWS credentials (read by the AWS SDK from the environment, never logged), list the `hydromancer-reservoir` prefixes, estimate egress before downloading, then import 1 s candles → `bars(1s)`, fills → `trades` (buyer/seller + a new optional `trades.flags` for liquidation/ADL), and 1-min L2 → `book(venue="hl-book-1m")`. Document the prefixes/schemas found.
- **Files:** `research/hlr/backfill/hydromancer.py`.
- **Tests:** a fixture Parquet file → expected rows; the importer refuses to start without credentials and never prints them (log-scrub test).
- **Done when:** tests pass; the owner's AWS account is noted in §15 and one day of 1 s candles + fills is imported with row counts.

#### B-6 — Official HL S3 archives (owner AWS account; completes V-8)
- **Do:** With owner-provided AWS credentials, estimate egress and document which months exist (the §15 "S3 archive" block). Import `hl-mainnet-node-data/node_fills_by_block` (plus `node_fills` before 2025-07-27): dedupe by `tid` into taker-side `trades` with the liquidation marker. `misc_events_by_block` → funding events. `hyperliquid-archive` `asset_ctxs` → `ctx` and `market_data/l2Book` → `book`/`bbo(hl-book)`. Add a per-month egress-cap flag.
- **Files:** `research/hlr/backfill/hl_s3.py`.
- **Tests:** fixture envelopes → expected rows; `tid` dedupe yields one row per trade; the cap aborts before exceeding the budget.
- **Done when:** tests pass; §15's S3 block lists the confirmed buckets, paths, data types, formats, update lag, and cost; one sample day per data type is imported.

#### B-7 — Deribit history (for O9 P3)
- **Do:** Page `history.deribit.com/api/v2/public/get_last_trades_by_currency_and_time` (`currency=BTC|ETH`, `kind=option`) into `deribit_options` (trade IV as `mark_iv`; `bid_iv`/`ask_iv` null; deltas computed), and import DVOL OHLC as a daily series. Respect V-10's rate limits.
- **Files:** `research/hlr/backfill/deribit.py`.
- **Tests:** fixture pages → expected rows and continuation handling; rate-limit backoff.
- **Done when:** tests pass; the DVOL start date and the imported option-trade range are recorded.

#### B-8 — Equity minute bars (after V-11)
- **Do:** Using the V-11 provider (owner account), import historical equity minute bars into `bars(venue="equity")` for the mapped underlyings (O10 Parts B/E and the O11 stock proxy). Alpaca free historical SIP (the `end` parameter ≥ 15 min old) or Massive free.
- **Files:** `research/hlr/backfill/equities.py`.
- **Tests:** fixture JSON → expected rows; the key (if any) is never logged.
- **Done when:** tests pass; one symbol's minute bars for the longest available history are imported with the range in the commit message.

#### B-9 — HIST-PRELIM report plumbing
- **Do:** Teach the study/report machinery that a run is on backfill: `days` and `coverage_pct` are computed over sampled days only (non-sampled intervals are gaps); a `--data-source backfill` flag stamps "PRELIMINARY (backfill: <sources>)" into the §13.7 report and sets the verdict qualifier `HIST-PRELIM` (§13.11); add the reveal-lag adjustment (`L += measured HL publish lag`) as a study parameter.
- **Files:** `research/hlr/report.py`, `research/hlr/__main__` flags.
- **Tests:** a synthetic sparse day set → `coverage_pct` counts only sampled days; the report header contains `HIST-PRELIM`; the lag adjustment shifts every `L` in the report.
- **Done when:** tests pass; `hlr-rank` groups `HIST-PRELIM` studies below the forward ones.

#### S-1 … S-23 — Studies
- **Do:** Implement the study in `research/studies/o{n}_{slug}.py` (entry point `uv run python -m studies.o{n}_{slug} --from … --to …`) following its §13.9 block exactly, and write the report via P-5. Run it first as **preliminary** (≥ 3 days of data), then **final** (≥ 14 days). O9, the §13.8 parts of O10, and O11 Part B use the data requirements stated in their blocks.
- **Done when:** the final report exists with all §13.7 sections, including the hand-checked sanity section; the verdict is stated; the report front-matter feeds `RANKING.md`.

#### D-1 — Decision
- **Do:** Regenerate `RANKING.md`. Write `specs/decisions/0002-first-arb-strategy.md` (context, the ranking table, the decision, capital and hurdle as set by the owner, rejected alternatives with one-line reasons, consequences for M5/M6/M7). If a study passes (or is the best MARGINAL), create a stub for the first strategy spec at the next free number (`specs/SPEC-0010-<name>.md` or later) with Purpose / Goals / Legging model / Open questions filled from the report. **The owner approves ADR-0002; an agent only drafts it.**
- **Done when:** ADR-0002 is drafted and marked "Proposed", and `docs/GOAL.md` §7 is updated to match.

## 15. Verified facts (filled in by V-tasks)

| Fact | Value | Source | Date | Task |
|---|---|---|---|---|
| HL WS idle-close window | Server closes a connection it has not **sent** a message to for ~60 s; app `{"method":"ping"}` → `{"channel":"pong"}` resets it. Live: unpinged conn closed at 59.8 s (code 1000 "Inactive"); pinged every 20 s stayed open > 84 s. | docs timeouts-and-heartbeats + observed live 2026-09-28 (`wss://api.hyperliquid.xyz/ws`) | 2026-09-28 | V-1 |
| HL `l2Book` depth / params / cadence | Depth ≤ 20 levels/side (observed 20/20); `fast:true` → 5. `nSigFigs` ∈ {2,3,4,5,null}; `mantissa` ∈ {1,2,5} only when `nSigFigs=5`. Cadence: default pushed on change, observed 2.4–6.6 s for BTC; `fast:true` observed ~0.5 s. | docs websocket/subscriptions + observed live 2026-09-28 | 2026-09-28 | V-1 |
| HL `trades.users` present | Yes. WS `WsTrade.users: [buyer, seller]`; REST `recentTrades` also carries `users`. | docs websocket/subscriptions + observed live 2026-09-28 (WS `trades` and `recentTrades`) | 2026-09-28 | V-1 |
| HL `allMids` with `dex` | Yes, WS and REST. `{"type":"allMids","dex":"xyz"}` returned 126 `xyz:*` mids; default dex includes spot mids only for the first perp dex. | docs websocket/subscriptions + info-endpoint + observed live 2026-09-28 | 2026-09-28 | V-1 |
| HL spot ctx channel name | `activeSpotAssetCtx`, data `{coin, ctx}` (SpotAssetCtx). Both `@1` and `PURR/USDC` subscriptions answered on it. | docs websocket/subscriptions + observed live 2026-09-28 | 2026-09-28 | V-1 |
| HL `predictedFundings` shape | `[[coin, [[venue, {fundingRate, nextFundingTime, fundingIntervalHours}], …]], …]`; venues seen `BinPerp`, `HlPerp`, `BybitPerp`; first perp dex only. | docs info-endpoint + observed live 2026-09-28 | 2026-09-28 | V-1 |
| HL `fundingHistory` shape / max / weight | `[{coin, fundingRate, premium, time(ms)}]`, ascending from `startTime`; max **500** items/call (request from `startTime=0` returned the earliest 500); weight `20 + 1 per 20 items returned`. | docs info-endpoint + rate-limits + observed live 2026-09-28 | 2026-09-28 | V-1 |
| HL `candleSnapshot` shape / max / weight | `[{t,T,s,i,o,c,h,l,v,n}]` (`t`/`T` ms; prices/`v` strings; `n` number). Only the most recent ~5000 candles per interval are retained and a call returns at most that (observed 1m = 5182 over ~3.6 d, 1h = 5003 over ~208 d); `startTime` honored within retention; weight `20 + 1 per 60 items returned`. | docs info-endpoint + rate-limits + observed live 2026-09-28 | 2026-09-28 | V-1 |
| HL `spotMetaAndAssetCtxs` row alignment | Response `[meta, ctxs]`; `ctxs` is indexed by the spot pair `index` (`meta.universe[].index`), **not** by position in `meta.universe`: 330 universe rows vs 885 ctx rows (indices 0–884); `ctxs[i].coin` matched pair `index == i` with 0 mismatches. Join on `universe[].index`, never array position. | observed live 2026-09-28 (POST /info `{"type":"spotMetaAndAssetCtxs"}`) | 2026-09-28 | V-1 |
| `chronyc -c tracking` column order | `RefID, RefName, Stratum, RefTime, SystemTime, LastOffset, RMSOffset, Frequency, ResidualFreq, Skew, RootDelay, RootDispersion, UpdateInterval, LeapStatus` (SystemTime is column **4**, not 3). | chrony 4.5 `chronyc(1)` + chrony `client.c` `process_cmd_tracking` | 2026-09-28 | V-1 |
| Binance/Bybit endpoints + fields | Confirmed. `binance-usdm` `wss://fstream.binance.com/stream?streams=<sym-lower>@bookTicker` (combined; payload wrapped `{"stream","data"}`), data `{e,u,s,ps,b,B,a,A,T,E,st}` with `T`/`E` in ms. `binance-spot` `wss://stream.binance.com:9443/stream?streams=<sym-lower>@bookTicker`, data `{u,s,b,B,a,A}` (no `e`/timestamps). `bybit-linear` `wss://stream.bybit.com/v5/public/linear`, subscribe `{"op":"subscribe","args":["orderbook.1.BTCUSDT"]}`; level 1 is snapshot-only, re-sent after 3 s idle, `{topic,ts,type,data:{s,b[[px,sz]],a[[px,sz]],u,seq},cts}`. All three reachable from the dev machine 2026-09-28 (one connect + first message each; connects 0.6–0.8 s). Host-region reachability untested (V-4 pending). | docs (Binance Connect + Individual Symbol Book Ticker; Bybit Connect + Orderbook) + observed live 2026-09-28 (WS connect to each endpoint + first message) | 2026-09-28 | V-2 |
| Binance/Bybit VIP0 fees | Binance USDⓈ-M Regular/VIP0: taker **0.050%**, maker **0.020%** (5.0/2.0 bps); ×0.9 (0.045%/0.018%) if BNB fee deduction is on. Bybit linear VIP0: taker **0.0550%**, maker **0.0200%** (5.5/2.0 bps); Bybit notes actual rates are region-dependent. | https://www.binance.com/en/support/faq/detail/360033544231, https://www.binance.com/en/fee/futureFee, https://www.bybit.com/en/help-center/article/Trading-Fee-Structure | 2026-09-28 | V-2 |
| HIP-3 fee model + per-dex values | `taker = base(4.5 bps) × scaleIfHip3 × growthModeScale × (1−referral)`; `maker = 1.5 bps × scaleIfHip3 × growthModeScale` (positive maker only); `scaleIfHip3 = scale+1 if scale<1 else 2×scale`; `growthModeScale = 0.1` when growth mode is on. `deployerFeeScale` ∈ [0,3] ([0,10) in growth), set per asset. Live 2026-09-28 `meta(dex)`: xyz, flx, vntl, km, abcd, cash, mkts, io use scale 1.0 → 9.0/3.0 bps (0.9/0.3 bps in growth mode); hyna scale 0.1111 → 5.0/1.67 bps; para scale 0.5 → 6.75/2.25 bps (one asset). ~75% of HIP-3 listings are in growth mode. Aligned-quote collateral scaling does not apply (no aligned quote assets on mainnet). Would change O1/O10 verdicts. | https://hyperliquid.gitbook.io/hyperliquid-docs/trading/fees, https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/hip-3-deployer-actions + observed live 2026-09-28 (`POST /info {"type":"meta","dex":"<dex>"}`) | 2026-09-28 | V-3 |
| Spot quote tokens | Live 2026-09-28 `spotMeta` quote tokens actually used: **USDC** (index 0, canonical; 313 pairs), **USDT0** (268; 5 pairs), **USDH** (360; 11), **USDE** (235; 1); there is no `USDT` token. Every token exposes `deployerTradingFeeShare` (all 0.0 for these); it redirects the deployer's cut and does not change the user's fee, and quote-token deployers cannot set it. Permissionless quote assets need 8 wei / 2 sz decimals, zero deployer share, 200k HYPE staked (slashable; USDC/USDT exempt) and peg/liquidity conditions; aligned quote assets (AQAv1) need 1M HYPE staked total. | https://hyperliquid.gitbook.io/hyperliquid-docs/hypercore/permissionless-spot-quote-assets, https://hyperliquid.gitbook.io/hyperliquid-docs/hypercore/aligned-quote-assets + observed live 2026-09-28 (`POST /info {"type":"spotMeta"}`) | 2026-09-28 | V-3 |
| Latency by region (p50/p90) | | | | V-4 |
| HyperEVM blocks / gas / mempool / transfers | Fast blocks ~1 s / 3 M gas, slow ~1 min / 30 M gas; EVM→Core transfers land in the same L1 block, Core→EVM waits for the next EVM block. Two on-chain L1 mempools, next 8 nonces/address, pruned > 1 day; priority fees (gossip and order) are **burned**. | https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/hyperevm/dual-block-architecture, https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/hyperevm/interaction-timings, https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/priority-fees | 2026-09-28 | V-5 (ideas review, verify) |
| Data volume per stream (MB/h raw, zst) | | | | V-7 |
| S3 archive availability | Requester-pays: `hyperliquid-archive` (`market_data` L2 book, `asset_ctxs`; ~monthly; no spot, no candles), `hl-mainnet-node-data` (`node_fills_by_block`, `explorer_blocks`, `replica_cmds`, `misc_events_by_block`), `hl-mainnet-evm-blocks`. | https://hyperliquid.gitbook.io/hyperliquid-docs/historical-data, https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/hyperevm/raw-hyperevm-block-data | 2026-09-28 | V-8 (ideas review, verify) |
| Recorder production start date + host | | | | R-10 |
| Yahoo options endpoint (fields, session, delay, pacing) | Base `GET https://query2.finance.yahoo.com/v7/finance/options/{TICKER}`; one request per expiry (`?date=<unix s>` for all but the first, which is inline). Session cookie from `GET https://fc.yahoo.com/` + crumb from `GET https://query2.finance.yahoo.com/v1/test/getcrumb`; missing crumb ⇒ HTTP 401 `Invalid Crumb`. Per contract: `contractSymbol, strike, currency, lastPrice, change, percentChange, volume, openInterest, bid, ask, contractSize, expiration (unix s), lastTradeDate, impliedVolatility, inTheMoney`; quote block has `regularMarketPrice`/`regularMarketTime`/`exchangeDataDelayedBy`. Yahoo's own quote pages state **options data is delayed 15 minutes**. No documented rate limit (unofficial endpoint): finsnap spaces one request per extra expiry by **300 ms** and backs off **2 s ×2** on HTTP 429 (≤ 3 retries). Yahoo's ToS forbids automated collection and storage/redistribution without prior written permission (see §17 #31). | https://finance.yahoo.com/quote/QQQ/options (15-min delay), https://legal.yahoo.com/us/en/yahoo/terms/otos/index.html (ToS) + observed live 2026-09-28 (SPY chain: cookie+crumb, `?date=` paging, no-crumb 401) | 2026-09-28 | V-9 |
| finsnap `option_snapshots` + `/snap` shape | Postgres `option_snapshots(ticker text, expiration date, side text, strike float8, snapshot_date date, volume int, open_interest int, underlying_price float8, fetched_at timestamptz)`, PK (ticker, expiration, side, strike, snapshot_date); **no IV/bid/ask/last**; retention **30 days** (`OPTION_SNAPSHOT_RETENTION_DAYS`), rows deleted on every upsert. Default `OPTIONS_SYMBOLS` = 28 index/sector/commodity/bond ETFs (SPY, QQQ, DIA, IWM, EEM, EFA, sector SPDRs, SMH, XPH, GLD, SLV, USO, TLT, LQD, HYG, VXX, IBIT); no single stocks (a 24 h user search widens the universe, not the options list). `GET /snap` options block = `{price, expirations:[{date, pcRatio, calls/puts:{totalVolume, totalOI, weightedMeanStrike, weightedStdStrike}, insight:{label, skewScore, dominantSide, volRatio, oiRatio, wallStrike, distanceToSpotAbs, distanceToSpotPct, nearSpotCluster}}], strikeProfile:[{strike, callVolume, putVolume, callOI, putOI, total}]}`. | finsnap `apps/backend/src/db/migrations.ts`, `src/storage/optionsStore.ts`, `src/constants/cache.ts`, `src/config.ts`, `src/analyzers/types.ts`, `AGENTS.md` "Options positioning" | 2026-09-28 | V-9 |
| HIP-3 stock/index universe + `underlyings.toml` | Live `perpDexs` = `null` (main) + 10 dexes: xyz, flx, vntl, hyna, km, abcd, cash, para, mkts, io. **Only xyz (109 listed), para (29), io (8), mkts (4) have non-delisted markets**; flx, vntl, hyna, km, abcd, cash are fully delisted (`isDelisted:true`, empty `l2Book`, stale `allMids`). `research/mappings/underlyings.toml` maps 122 active HIP-3 underlyings + BTC/ETH: **89** with a US chain (or index-ETF proxy: SP500/US500→SPY, USTECH→QQQ, SMALL2000→IWM), **33** `no-chain` (foreign/private/uncertain). Active on more than one dex: AAOI, AVGO, CRWD, IREN, NET, RDDT, SNDK, EWY, NBIS, UNITREE, DRAM. Much smaller than the multi-dex universe O1/O9 assumed. | observed live 2026-09-28 (`POST /info {"type":"perpDexs"}`, `{"type":"meta","dex":D}`, `l2Book`, `allMids`) + `research/mappings/underlyings.toml` | 2026-09-28 | V-9 |
| Deribit public API (endpoints, fields, limits) | Base `https://www.deribit.com/api/v2` (JSON-RPC over HTTP GET, no auth). `public/get_book_summary_by_currency?currency=BTC\|ETH&kind=option`: `instrument_name` (`BTC-<DDMMMYY>-<strike>-<C\|P>`), `mark_iv`, `open_interest`, `volume`, `underlying_price`, `mark_price`, `bid_price`/`ask_price`, `mid_price`, `last`, `high`/`low`, `volume_usd` — **no `bid_iv`/`ask_iv`/greeks/`index_price`**. `public/get_instruments?currency=BTC&kind=option&expired=false`: `strike`, `option_type`, `expiration_timestamp` (ms), `tick_size`, `contract_size`, `min_trade_amount`, `state`. `public/ticker?instrument_name=…`: `mark_iv`, `bid_iv`, `ask_iv`, `greeks{delta,gamma,vega,theta,rho}`, `open_interest`, `index_price`, `underlying_price`, `last_price`, `stats.volume`. `public/get_index_price?index_name=btc_usd\|eth_usd`: `index_price`. Live 2026-09-28 from this machine: 944 BTC / 796 ETH option summaries, BTC index 83051.2, ETH 2665.33, ~0.09–0.11 s/call, unauthenticated. Limits: non-matching 20 req/s sustained / 100 burst; `public/get_instruments` **1 req/s** sustained / 50 burst; public calls are per-IP. | https://docs.deribit.com/api-reference/market-data/public-get_book_summary_by_currency, https://docs.deribit.com/api-reference/market-data/public-get_instruments, https://docs.deribit.com/api-reference/market-data/public-ticker, https://docs.deribit.com/api-reference/market-data/public-get_index_price, https://docs.deribit.com/articles/rate-limits + observed live 2026-09-28 | 2026-09-28 | V-10 |
| Equities provider comparison + owner approval | | | | V-11 |
| HIP-3 stock-perp mechanics per dex | | | | V-12 |
| finsnap `option_snapshots` coverage; options-history vendors; owner decision | | | | V-13 |
| finsnap history imported (range, symbols) | | | | P-7 |
| Spot quote-asset fee multipliers | Spot pairs between two spot quote assets (`isStablePair`, e.g. `USDT0/USDC`) get **80% lower taker fee and 80% smaller maker rebates/volume contribution** (`scaleIfStablePair = 0.2`) → 1.4 bps taker at base. Aligned quote assets (AQAv1) get 20% lower taker, 50% larger maker rebate, 20% more volume contribution; AQAv2 has no fee benefit. The HIP-3 deployer-actions doc states there are currently **no aligned quote assets on mainnet** (aligned status is not exposed by `spotMeta`), so the aligned discount does not apply today. Changes O2's triangle threshold. | https://hyperliquid.gitbook.io/hyperliquid-docs/trading/fees, https://hyperliquid.gitbook.io/hyperliquid-docs/hypercore/aligned-quote-assets, https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/hip-3-deployer-actions | 2026-09-28 | V-3 |
| Funding mechanics (ideas review) | Paid hourly at 1/8 of the computed 8 h rate; premium sampled every 5 s; cap **4%/hour**; payment = `position_size × oracle_price × rate` (oracle notional, not mark). O14 depends on this. | https://hyperliquid.gitbook.io/hyperliquid-docs/trading/funding | 2026-09-28 | V-3 (ideas review, verify) |
| `perpDexs` funding fields (ideas review) | Returns `assetToStreamingOiCap` and `assetToFundingMultiplier`; deployers can set multipliers 0–10, interest ±1%/8 h, and clamps. | https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/info-endpoint/perpetuals, https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/hip-3-deployer-actions | 2026-09-28 | V-3 (ideas review, verify) |
| HL documented latency (ideas review) | Co-located median end-to-end 0.2 s, p99 0.9 s; ALO/cancel end-to-end ~380 ms (~2 blocks); write priority ≈45 ms per 1 bp; read gossip priority ≈25 ms per slot. | https://hyperliquid.gitbook.io/hyperliquid-docs/hypercore/overview, https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/optimizing-latency, https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/priority-fees | 2026-09-28 | V-4 (ideas review, verify) |
| HIP-4 outcome markets (ideas review) | Native fully-collateralized binaries; a daily 06:00 UTC binary settling to the HyperCore **mark** (multi-outcome not in the initial release). The launch date and the extension to BTC/ETH/HYPE/SOL are third-party ⚠ verify. | https://hyperliquid.gitbook.io/hyperliquid-docs/hyperliquid-improvement-proposals-hips/hip-4-outcome-markets | 2026-09-28 | V-12 (ideas review, verify) |
| New WS channels (ideas review) | `fastAssetCtxs` (base64 + raw-DEFLATE, `markPx`/`midPx`, first message a snapshot), `allDexsAssetCtxs`, `allDexsClearinghouseState`, and `twapStates` are documented. | https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/websocket/subscriptions | 2026-09-28 | V-1 (ideas review, verify) |
| `noop` action (ideas review) | Exists to invalidate an in-flight nonce; billed at the base rate, unlike a stale `expiresAfter` (5×). Relevant to SPEC-0002, not market data. | https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/exchange-endpoint | 2026-09-28 | SPEC-0002 (ideas review, verify) |

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
6. **Historical options data** (V-13). Buying past option chains lets O9a test years of history now. The owner decides after V-13 shows finsnap's existing coverage and vendor prices. **V-9 (2026-09-28):** finsnap retains only the **last 30 days** and stores only volume/OI/underlying price (no IV/bid/ask), so its coverage is far below O9a's ≥ 2-year target; V-13 vendors are the only route to real history. **Research note (2026-09-29, `docs/research/data-sources-2026-09-29.md`):** the same 30-day limit (and the Yahoo ToS risk in #31) is what makes the paid route attractive — the cheapest licensed ≥ 2-year daily history is the EODHD US Options add-on at 29.99 USD/mo (EOD since Q4 2023). Buying is an owner decision; no purchase has been made or authorised. Details in V-13's §14.2 finding.
7. **Directional risk limits.** O9/O10 Part B strategies carry market risk that the arb strategies don't; if one passes, SPEC-0004 needs per-strategy stop-loss and volatility-scaled sizing before it goes live.
8. **Writer-originated `seq` (R-1/R-2).** §5.1 defines `seq` as the per-`conn` data counter, but `segment_open`/`segment_close` are produced by the writer, not the caller. The writer currently stamps `segment_open.seq` from the first data envelope and `segment_close.seq` from the last, so a naive hole detector sees no downward jump. Confirm this is the intended reading.
9. **`records`/`bytes_raw` scope (R-2).** §6 lists `records`/`bytes_raw` without saying whether they include the `segment_close` line. Current choice: `segment_close.meta` counts lines **before** close; the manifest's `records` counts **total file lines including close** (so `records` equals the decoded line count).
10. **Per-stream disk guard (R-2).** §6 says stop the "lowest-priority streams"; with one `SegmentWriter` per `(src, conn)` the guard stops only its own stream and emits `gap_start{reason:"disk"}`. A cross-stream coordinator (priority ordering) lands with R-6.
11. ~~**`*:top:N` volume source (R-4).**~~ **Resolved 2026-09-28 (V-1):** `spotMetaAndAssetCtxs` returns `[meta, ctxs]` where `ctxs` is indexed by the spot pair `index` (from `meta.universe[].index`), not by position in `meta.universe`; observed 330 universe rows vs 885 ctx rows with `ctxs[i].coin` matching pair `index == i` (0 mismatches). `record.rs` already matches by `market.index == ctx position`, so it is correct; its comment saying rows "align positionally" should read "by pair index". Perps keep the positional `meta.universe`↔`asset_ctxs` join (which is aligned).
12. **`spot:quotes` needs `SpotMeta` (R-4).** `AssetMap` does not expose token indices, so `spot:quotes` requires the caller to pass `SpotMeta` (else `PlannerError::MissingSpotMeta`). Confirm this is acceptable for R-6.
13. ~~**`fundingHistory` paging shape (R-5).**~~ **Resolved 2026-09-28 (V-1):** the response is `[{coin, fundingRate, premium, time(ms)}]`, ascending from `startTime`, and a call returns at most **500** items (a request from `startTime=0` returned the earliest 500), so `next startTime = max(time)+1` and "caught up at now − 60 s" are both correct. The weight is `20 + 1 per 20 items returned` (docs rate limits), not flat 20. **Code follow-up:** R-5 charges a flat weight 20 for `fundingHistory`; add the per-20-items surcharge once the item count is known.
14. ~~**`candleSnapshot` paging (R-5).**~~ **Resolved 2026-09-28 (V-1):** fields are `t` (open ms), `T` (close ms), `s`, `i`, `o`, `c`, `h`, `l`, `v`, `n`; only the most recent ~5000 candles per interval are retained and a call returns at most that (observed 1m = 5182, 1h = 5003), so the 6 h page window is well inside the cap. The weight is `20 + 1 per 60 items returned`, not `20 + 20·(candles/60)`. **Code follow-up:** fix `candle_weight` (and its test), which overestimates by ~20×.
15. ~~**`predictedFundings` (R-5)** is recorded raw but not parsed (shape ⚠ V-1).~~ **Resolved 2026-09-28 (V-1):** shape is `[[coin, [[venue, {fundingRate, nextFundingTime, fundingIntervalHours}], …]], …]` with venues `BinPerp`/`HlPerp`/`BybitPerp`; first perp dex only. Recording raw is sufficient; no code change needed.
16. **`inspect` gap duration (R-7)** counts paired `gap_start`/`gap_end` only; an open gap (crashed tail, no `gap_end`) increments the gap count but contributes no duration (`crashed_files` is reported separately).
17. ~~**`verify` coverage (R-7).**~~ **Resolved 2026-09-27:** coverage is the union of each segment's `[first_t_ns, last_t_ns]` per `(src,conn)`, minus paired gaps and clipped to the day. An unpaired `gap_start` is closed at the last record on the stream (see #24); the writer's `segment_open`/`segment_close` lines do not define a segment's span.
18. **`SegmentWriter` stats missing (R-6).** The writer exposes no counters, so `hl_rec_bytes_raw_total`, `hl_rec_bytes_zst_total`, `hl_rec_channel_depth`, and `hl_rec_segment_rotations_total` (names added) are not yet emitted. Add a `SegmentWriter::stats()` / shared `Arc<SegmentStats>`.
19. **Writer liveness (R-6).** `/healthz` cannot tell whether the segment-writer thread is alive (no API); it returns OK while the HTTP server runs. Add a liveness flag/API.
20. **`clock` envelope on `hl-rest` (R-6).** `RestSnapshotter` owns its `seq` and has no clock hook, and a second `(hl-rest, hl-rest)` producer would collide, so `clock` is emitted per `hl-ws` connection only. Needs a clock hook or a shared sequence.
21. **Subscribe pacing inside `RawWsConn` (R-6).** §7.4 step 6 (≤ 20 msg/s, ≤ 1 dial/3 s on reconnect) cannot be enforced from R-6: `RawWsConn` sends all subscriptions/resubscribes in one burst with no incremental subscribe hook. Connection-level pacing and initial-dial retry are in R-6. Its `hl_ws_*` metrics also label `src="hl"` with no `conn` (§7.5 mismatch).
22. **`hl record plan` opens metadata REST calls (R-6).** §12.1 says "no sockets opened", but resolving universe selectors needs `/info` metadata. It opens no WS/recording sockets; confirm the wording or accept the metadata calls.
23. ~~**`chronyc` column order (R-6/V-1).**~~ **Resolved 2026-09-28 (V-1):** `chronyc -c tracking` CSV order is `RefID, RefName, Stratum, RefTime, SystemTime, LastOffset, RMSOffset, Frequency, ResidualFreq, Skew, RootDelay, RootDispersion, UpdateInterval, LeapStatus`. `Stratum` is column 2 (correct in `record.rs`), but `SystemTime` is column **4**, not 3, so the current parser reads the RefTime epoch as the offset. **Code follow-up:** read column 4.
24. **How a `drop` gap ends (R-2/R-7).** `gap_start{reason:"drop"}` is emitted after a bounded-channel overflow but never gets a matching `gap_end`, so a naive coverage calculation would mark the rest of the stream missing. `verify` currently treats an unpaired `gap_start` as running to the last record on the stream (the conservative reading). Define when a drop gap ends, and whether `verify` should instead bound it (for example, one flush interval) or ignore it.
25. **AWS account for requester-pays buckets (owner).** B-5/B-6 and the V-8 sample download need an AWS account. Expected cost is a few dollars per month of fills, ~$100+ for a full year. The owner must create and hold the credentials; agents never should.
26. **Tardis subscription (owner).** Only if the preliminary HIST-PRELIM results look promising. The cheapest route to "every day" HL `bbo` for 4 months is a monthly Academic/Solo Perpetuals plan (~$350–1,200/mo; minimum $300); otherwise rely on our own recorder.
27. **Data licenses.** Tardis, Hydromancer and the HL S3 terms for storing data for research are unstated; only SonarX publishes an explicit (CC0) license. Keep everything under `research/data/` (never committed). **Research note (2026-09-29, `docs/research/data-sources-2026-09-29.md` §1.4):** Tardis's ToS (https://docs.tardis.dev/legal/terms-of-service) prohibits using the data to train or validate ML models without a separate agreement, which directly affects the classifier studies (M-1…M-5); internal/research/personal use and task-specific statistical models are allowed per the report, but the ML clause is the one to read. Deribit's ToS was not read (**UNVERIFIED**).
28. ~~**June-2026 `l2Book` throttling.**~~ **Resolved 2026-09-28 (V-1):** §15's `l2Book` row records the default 20-level push at 2.4–6.6 s (now ~5 s) and `fast:true` 5 levels at ~0.5 s; no re-check needed.
29. **Claims to verify (ideas review).** Not applied to spec facts unless the claim cites an official docs URL; those are in §15 tagged "(ideas review, verify)". Remaining bullets:
    - HIP-4 outcome-market launch date and the BTC/ETH/HYPE/SOL extension are third-party ⚠ verify (the docs describe the primitive, not the date); gates O17.
    - HyperEVM read precompiles on **mainnet** (docs describe testnet) ⚠ verify; gates O22.
    - HYPE on-chain option venues (Derive, Hypersurface, opt.fun, D2 HYPE++ vault) are third-party ⚠ verify; gates O23.
    - Whether HL's June-2026 WS throttling also changed the official S3 archive cadence is unverified.
    - ~~Binance/Bybit VIP0 fees~~ **Resolved 2026-09-28 (V-2):** 5.0/2.0 bps (Binance USDⓈ-M) and 5.5/2.0 bps (Bybit linear) at VIP0; see §15. `predictedFundings` timing/weights remain open (V-1 settled the shape; O7).
    - The docs state HyperCore order sequencing but not an explicit EVM block ordering rule; keep O8 Q2 open (source: `hyperliquid.gitbook.io/hyperliquid-docs/hypercore/order-book`).
    - §B claims already settled by V-1 (trades.users, `l2Book` fast mode, `candleSnapshot` retention, spot ctx channel, WS idle close, `chronyc` columns): no action; see §15.
30. **`markets.asset_id` convention (B-3).** §13.1 does not say how `asset_id` is numbered across universes. B-3 (`research/hlr/backfill/hl_rest.py`, `build_markets`) stores the index **within the universe it was read from** (main perp dex, each HIP-3 dex, or spot), so `asset_id` is unique only together with the dex/kind. Studies must join on the market name, not `asset_id`. If a global id is ever needed, adopt HL's own asset-id offsets (verify against the docs first).
31. **Yahoo options ToS, delay and reliability (V-9).** Yahoo's ToS forbids automated collection ("access or collect data … using any automated means … without our express, prior permission") and reproduction/redistribution without written permission, so storing Yahoo option chains for research/trading is a legal risk; the endpoint is also unofficial (cookie+crumb session, 15-min delayed, can change without notice). Before R-11 collects for O9b, either confirm with the owner that personal research storage is acceptable or replace Yahoo with a licensed source (ties into V-11/V-13). Sources: https://legal.yahoo.com/us/en/yahoo/terms/otos/index.html, https://finance.yahoo.com/quote/QQQ/options. **Research note (2026-09-29, secondary sources):** Cboe's delayed-quotes page states automated download is prohibited and IPs are blocked (secondary), and Yahoo's ToS forbids automated access (secondary); both push away from free web endpoints for R-11/O9b. A licensed alternative at ~30 USD/mo (EODHD Options add-on) is recorded in V-13's §14.2 finding and #6.
32. **Uncertain `underlyings.toml` entries and index proxies (V-9).** The file marks 33 active HIP-3 underlyings `no-chain` because their identity or optionability is unconfirmed (ANSEM, ANTH, BIRD, BOT, CBRS, CXMT, DRAM, GIGADEV, HYUNDAI, JP225, KIOXIA, KR200, LYTE, MINIMAX, NCLD, OAI, OURA, PURRDAT, QNT, SHAZ, SHEIN, SKHX, SKHY, SMSN, SNXX, SOFTBANK, SPCX, STRC, TREAD, UNITREE, USBOND, XYZ100, ZHIPU). The index-ETF proxies (SP500/US500→SPY, USTECH→QQQ, SMALL2000→IWM) and the foreign-name identifications should be reviewed by the owner; a wrong mapping silently biases O9/O10.
33. **R-12 Deribit field sources (V-10).** `get_book_summary_by_currency` omits `bid_iv`/`ask_iv`, greeks and `index_price`, so R-12 cannot fill §13.1's `deribit_options` row from it alone: add `get_instruments` (expiry/strike/cp, capped at 1 req/s) and per-instrument `ticker` (or the `ticker.<instrument>` WS channel). Deribit's terms for storing market data are unstated (see #27).
34. **Grading edge cases (P-5 review, 2026-09-28).** Chosen readings, all conservative: coverage below `min_coverage_pct` grades **FAIL** (a failed `[quality]` gate), not INCONCLUSIVE; a missing, failed or non-positive buffered-cost re-run (`robustness_buffer_multiplier`) grades **FAIL**; a `preliminary` result is capped at **MARGINAL**; the headline latency rounds **up** to the next grid value (above the grid → INCONCLUSIVE) and the headline capital must match a run exactly; a report without the `adj_jitter` variant is INCONCLUSIVE; `data_source` is derived from the input tables' `source` column (anything but pure recorder data → HIST-PRELIM).
35. **Accepted limits of P-5 grading (review #4, 2026-09-29).** (a) Hand-edited report front-matter metrics and provenance are trusted: the digest only guards data drift, not metric edits; closing this needs the bootstrap seed and draws saved, or the robustness frames' `source` stored in the parquet. (b) Studies built on the `bars` table carry source mid/trade/candle and can never be graded forward (forward is decided by `source == "recorder"` only). (c) Episode provenance is taken from the row at `t_start`. A malformed report never aborts `hlr-rank`; it grades INCONCLUSIVE with a reason.
36. **`verify` trusts manifests; orphan segments (R-2b/V-4).** `hl record verify` enumerates the manifest (`reader::verify`), so a finished segment whose manifest line was lost is invisible to it and coverage is undercounted. The 9p/drvfs append race that caused this is now serialized per manifest path (R-2b, §6), but a **manifest-vs-disk check** (list segments on disk, flag those with no manifest line) and repair of the **5 orphan segments** from the V-4 run are open follow-ups.
37. **V-4 / V-7 measured results (2026-09-29, external-SSD run on the pre-V-7b profile).** 6.25 h, 22.3 M records, 774 MB compressed on disk; `hl-ws`+`hl-rest` ~117 MB/h compressed and ~1.2 GB/h raw (10.5× compression); 24 connection opens over 8 connections. Measured **before** the `l2Book`/`allMids` fix, i.e. with every priority-3 subscription dropped; the corrected profile (majors' `l2Book` only) is estimated at +0.2–0.5 GB/day compressed. Recorded here rather than in §15 because the reference region/host (V-4's actual Done-when) is still unset.
