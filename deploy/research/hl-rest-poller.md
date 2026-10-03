# HL REST funding/candle poller (SPEC-0008 B-3)

The `hlr-hl-rest` console script (in `research/`) backfills, from Hyperliquid's
**public** `POST /info` endpoint (no keys, ever):

* `fundingHistory` → `funding_hist` for every main-dex perp and every HIP-3 dex;
* `candleSnapshot` for 1d/4h/1h (max depth) plus 15m/5m/1m (rolling) →
  `bars(source="candle")`, dropping `n == 0` pre-launch candles;
* `meta`/`spotMeta`/`perpDexs` → `markets`.

## One-shot backfill (run once, first)

```sh
cd research
uv run hlr-hl-rest backfill --out research/data/parquet
```

Resumable/idempotent: each `(table, day)` partition is merge-deduped, and
`research/data/parquet/hl_rest_state.json` records the last funding time and
candle open time per stream. Re-running writes only new rows. `--full` ignores
the resume points (still no duplicates). `--weight-per-min N` (default 300, a
quarter of HL's 1200/IP/min budget) caps the token bucket; the client adds the
documented per-item surcharge (1 per 20 funding items, 1 per 60 candles) and
backs off on 429/5xx. No request is ever dropped, only delayed.

## Daily poller (the scheduled job)

The venue keeps only the most recent ~5000 candles per interval, so the rolling
1m window expires in a few days. `poll` refreshes the market snapshot, catches
funding up, and appends 1m/5m candles:

```sh
cd research
uv run hlr-hl-rest poll --out research/data/parquet
```

Install this as a daily cron entry **on the research host** (nothing is
installed by this repo; copy the line deliberately):

```cron
# SPEC-0008 B-3: append rolling HL 1m/5m candles + funding once a day.
17 3 * * *  cd /srv/hl-arb-bot/research && /usr/bin/uv run hlr-hl-rest poll --out data/parquet >> /var/log/hlr-hl-rest.log 2>&1
```

`uv sync` must have been run once in `research/` first. The log line prints the
tables and the number of new rows, and the day range of the candle days touched
(the "first appended day" record).

First smoke run (2026-09-28, public data, scratch out-dir): BTC funding from
2023-05-12, `xyz:TSLA` funding from 2025-11-13, BTC 1h candles for the last day
(25 rows), BTC market snapshot (858 markets: 234 perp / 294 HIP-3 / 330 spot).
The first `poll` appended BTC 1m candles for days 2026-09-25…2026-09-28 and 5m
for 2026-09-11…2026-09-28.
