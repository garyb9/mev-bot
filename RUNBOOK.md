# Runbook

Operational procedures for the `hl` bot. See [SPEC-0006](specs/SPEC-0006-deployment-observability-runbooks.md)
for the full design.

> Current status: the v2 event-driven engine (`EngineLoop`) is wired into `hl`.
> Some procedures reference SPEC-0004 components that are not wired yet; those
> are marked _(pending)_.

## Start / stop / restart

```sh
# Start (observe is the safe default)
hl run --mode observe

# Start against testnet
hl run --mode observe --network testnet

# Stop gracefully (SIGTERM or Ctrl-C): disarms the dead-man's switch,
# flushes metrics, closes the DB, and exits 0.
kill -TERM "$(pgrep -f 'hl run')"
```

A graceful shutdown must log `shutdown complete` and exit `0`. A non-zero exit
or missing log means the runbook below applies.

## Health checks

| Endpoint | Meaning |
|---|---|
| `GET /healthz` | process liveness (also reflects breaker state) |
| `GET /readyz` | `200` when all required feeds are fresh; `503` otherwise |
| `GET /metrics` | Prometheus scrape |

## Markets & watchlist

```sh
# Discover markets (read-only): perps by default, HIP-3 via --dex, spot via --spot
hl markets BTC
hl markets --dex xyz TSLA
hl dexs

# Edit the persisted watchlist (validated against live metadata before saving)
hl select BTC ETH SOL
hl select --add xyz:TSLA          # HIP-3 is dex-qualified
hl select --remove ETH
hl select                         # print the current list

# Override for a single run without persisting
hl run --coins BTC,SOL
```

The watchlist is stored at `HL_WATCHLIST_PATH` (default `data/watchlist.txt`,
one coin per line, `#` comments allowed). Precedence: `--coins` > persisted file
> config/env default. Unknown or delisted coins fail fast before any subscription.

## Dry-run an order / inspect an account

```sh
# Build + sign an order WITHOUT submitting (needs HL_AGENT_PRIVATE_KEY).
# Prints the rounded wire order, nonce, signature, and the /exchange envelope.
HL_AGENT_PRIVATE_KEY=0x... hl order BTC --side buy --sz 0.123456 --px 61000.7
HL_AGENT_PRIVATE_KEY=0x... hl order BTC --side sell --sz 0.5 --px 50000 \
    --tif alo --reduce-only --cloid 0x0123456789abcdef0123456789abcdef

# Read-only account snapshot (positions, margin, open orders).
hl account 0xYourAddress
```

`hl order` never posts, in any mode. Invalid orders (bad tick/lot, below the
$10 minimum notional) are rejected locally before signing. The nonce
high-water mark is persisted in SQLite (`meta.nonce.last`) so a restart cannot
reuse a nonce.

## Transports & dead-man's switch

- Writes default to the **WebSocket `post`** transport (`WsExchange`); REST
  `POST /exchange` (`HttpExchange`) is the fallback. Both share the same signed
  envelope, nonce state, and SQLite high-water mark.
- In `live`, `scheduleCancel` is armed on start with TTL
  `HL_SCHEDULE_CANCEL_TTL_MS` (default 30s). A 1 s task checks the engine's
  resting-order count and refreshes the switch once less than half the TTL
  remains. A crash, stall, or loss of connectivity therefore cancels resting
  orders after the TTL. Graceful shutdown disarms it explicitly.
- Watch `hl_deadman_armed` (1 while armed), `hl_deadman_refreshes_total`, and
  `hl_deadman_failures_total`. Failure to refresh within the window is an alert.

> Open item: the live testnet round-trip (place a far-from-mid ALO order,
> confirm in `openOrders`, cancel) is wired but not yet exercised — it needs a
> funded, agent-approved testnet account.

## Go live (checklist)

1. Confirm the agent wallet is approved (Hyperliquid UI → Settings → API).
2. Set `HL_ACCOUNT_ADDRESS` and `HL_AGENT_PRIVATE_KEY` (agent key only).
3. Set `HL_MODE=live` and `HL_LIVE_CONFIRM=YES`.
4. Confirm `readyz` is `200` and metrics show fresh feeds.
5. Verify the dead-man's switch is armed: log line `dead-man switch armed` and
   `hl_deadman_armed 1`.
6. Start with small size; watch PnL and reject metrics.

## Kill switch

- **Manual _(pending)_:** the design (SPEC-0004 K-3) is `SIGUSR1`, the
  configured flag file, or `hl panic` (which writes the file). None of these
  are wired into `hl` yet; the `Control::KillSwitch` path the dead-man task uses
  is the only kill trigger today.
- **Expected action:** cancel all resting orders + halt new risk. Flattening is
  opt-in.
- **Verify:** open orders go to zero; `/healthz` reflects halt; logs record the
  trip.

## Nonce errors

Symptom: `live` submissions rejected with a stale/duplicate nonce. Fix:

1. Ensure only one process uses the agent wallet.
2. Confirm NTP/chrony is healthy (`timedatectl`); the clock must not regress.
3. Restart the bot; the nonce state machine advances past the last value.
4. If it persists, rotate to a fresh agent wallet.

## Key rotation

1. Create a new agent wallet in the Hyperliquid UI and approve it.
2. Update `HL_AGENT_PRIVATE_KEY`, restart the bot in `observe`, verify signing.
3. Revoke the old agent wallet in the UI.

The master key is never placed on the host.

## Replay a recorded session

`simulate` and `live` record their inputs to the `events` table. Today `hl run`
records only market-feed frames (no `Timer`/`Account`/`Fill` rows), so a freshly
recorded session has no decision cycles and `hl replay` yields zero intents.
Replay re-drives the configured strategies from the recorded log with no network
or clock, and prints an FNV-1a-64 fingerprint over the emitted placements:

```sh
# Replay the most recent session (prints events=, intents=, fingerprint=)
cargo run -p mev-bot -- replay

# Replay a specific session id
cargo run -p mev-bot -- replay --session 12 --db data/hlbot.db
```

Identical logs must yield an identical fingerprint; a change means the strategy
is non-deterministic (a bug — see SPEC-0003 §10). Replay never dials the network.
The v2 replay driver that records `Timer`/`Fill` so new sessions replay is
SPEC-0010 E-7 part 2 (blocked on SPEC-0008 R-7).

## Recorder (market data, SPEC-0008)

The recorder (`hl record`) is a separate, low-risk process: it never loads keys
and never places orders. It writes raw Hyperliquid market data to rotating zstd
segments under `data/rec/` and serves the same `/healthz` `/readyz` `/metrics`
endpoints as the bot (default port `9091`).

```sh
# Start (profile `default` from config/record.toml). Runs until SIGTERM/Ctrl-C.
hl record

# Start against testnet, or a named profile
hl record --network testnet
hl record --profile default

# Print the resolved subscription plan and exit; opens no recording sockets.
# Use it to prove the plan stays within the HL WS limits (SPEC-0008 §7.4).
hl record plan

# Stop gracefully: emits `gap_start{shutdown}` on every connection, finalizes
# each open segment, and writes the manifests.
kill -TERM "$(pgrep -f 'hl record')"
```

Configuration lives in `config/record.toml` (profile shape in SPEC-0008 §7.3).
Override any value with `HL_RECORD_*` env vars using `__` for nesting, e.g.
`HL_RECORD_PROFILE__DEFAULT__NETWORK=testnet`.

### Health

| Endpoint | Meaning |
|---|---|
| `GET /healthz` | process liveness |
| `GET /readyz` | `200` when every planned WS connection is connected and fed within the watchdog and REST data is fresh |
| `GET /metrics` | Prometheus scrape (`hl_rec_*`, `hl_ws_*`) |

### Coverage and integrity

```sh
# Inspect one or more segment files (or a directory, walked recursively):
# record counts by src/conn/kind/channel, first/last time, gaps, seq holes.
hl record inspect data/rec/mainnet

# Check a day's manifest against the files on disk and report coverage %.
hl record verify --date 2026-09-27
```

### Disk full / rotation / shipping

- Segments rotate at the top of every UTC hour or at 1 GiB uncompressed,
  whichever comes first (SPEC-0008 §6).
- When free disk drops below `min_free_gb` (default 20) the affected stream
  stops and emits `gap_start{reason:"disk"}`; the recorder never crashes on a
  full disk.
- Retention (`retain_days`, default 30) only deletes shipped segments, and
  shipping is optional in v1 (SPEC-0008 R-10).
- A segment that was cut off by a crash appears as `*.jsonl.zst.crashed` and is
  readable up to its last complete line; it is reported by `hl record verify`.

### Latency probing (SPEC-0008 V-4)

```sh
# TCP connect, TLS handshake, WS ping->pong, and /info allMids RTT (p50/p90/max)
hl probe latency --count 50
```

## Database (SQLite)

- Path: `HL_DB_PATH` (default `data/hlbot.db`), WAL mode.
- Backup: copy the DB with SQLite's backup API/`VACUUM INTO` (WAL-safe) on a
  cadence; snapshot before upgrades _(pending SPEC-0004)_.
- Restore drill on a schedule; a restore must not require re-deriving PnL.

## Reconciliation drift

Symptom: local positions/orders differ from the exchange. Action:

1. A background REST task refreshes the account snapshot every 30 s and feeds it
   to the engine through the account channel (SPEC-0010 §15).
2. In `hl` today that `AccountUpdate::Reconcile` carries only
   `account_value`/`margin_used`; position and order-state drift is not yet
   applied until the H-3 account stream is wired (SPEC-0010 E-13 remaining).
   Treat position/order drift as a manual intervention until it lands.
3. Resolve drift before resuming.

## Failed deploy / rollback

1. Stop the running instance gracefully.
2. Check out the last green tag/commit and rebuild.
3. Restore the pre-upgrade DB snapshot if the schema changed.
4. Start in `observe`, confirm `readyz`, then re-enable the previous mode.
