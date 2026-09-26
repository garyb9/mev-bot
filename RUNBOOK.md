# Runbook

Operational procedures for the `hl` bot. See [SPEC-0006](specs/SPEC-0006-deployment-observability-runbooks.md)
for the full design.

> Current status: platform scaffold. Some procedures reference components that
> arrive with SPEC-0001/0002; those are marked _(pending)_.

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

## Go live (checklist)

1. Confirm the agent wallet is approved (Hyperliquid UI → Settings → API).
2. Set `HL_ACCOUNT_ADDRESS` and `HL_AGENT_PRIVATE_KEY` (agent key only).
3. Set `HL_MODE=live` and `HL_LIVE_CONFIRM=YES`.
4. Confirm `readyz` is `200` and metrics show fresh feeds.
5. Verify the dead-man's switch is armed in logs/metrics _(pending SPEC-0002)_.
6. Start with small size; watch PnL and reject metrics.

## Kill switch

- **Manual:** send `SIGUSR1`, drop the flag file, or run `hl panic` _(pending)_.
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

## Database (SQLite)

- Path: `HL_DB_PATH` (default `data/hlbot.db`), WAL mode.
- Backup: copy the DB with SQLite's backup API/`VACUUM INTO` (WAL-safe) on a
  cadence; snapshot before upgrades _(pending SPEC-0004)_.
- Restore drill on a schedule; a restore must not require re-deriving PnL.

## Reconciliation drift

Symptom: local positions/orders differ from the exchange. Action:

1. The bot resyncs automatically on reconnect/periodically.
2. If drift persists, it halts new risk and alerts.
3. Diagnose with the reconciliation metrics; resolve before resuming.

## Failed deploy / rollback

1. Stop the running instance gracefully.
2. Check out the last green tag/commit and rebuild.
3. Restore the pre-upgrade DB snapshot if the schema changed.
4. Start in `observe`, confirm `readyz`, then re-enable the previous mode.
