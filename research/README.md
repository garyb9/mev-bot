# hlr — Hyperliquid recorder research toolkit

Python toolkit for turning the raw market-data segments written by the recorder
(`hl record`, SPEC-0008 Part A) into research tables, studies, and reports
(SPEC-0008 Part B).

> **Research code only.** `research/` is never imported by, linked into, or
> deployed with the trading bot. The bot is Rust; production money paths never
> depend on anything in this directory. Do not add this package as a dependency
> of any workspace crate.

## Setup

Requires Python 3.12 and [`uv`](https://docs.astral.sh/uv/).

```sh
cd research
uv sync          # create research/.venv and install deps from uv.lock
```

No keys, no `.env`, no network access is needed to run the reader or the tests.
Recordings live under `research/data/` (git-ignored) after you copy or sync them
from the recorder host; nothing in this directory writes to `data/`.

## Data terms

Fetched third-party market data (for example Tardis free days, Binance/Bybit
public dumps) is governed by the vendor's terms of service. **Do not
redistribute or commit it.** Derived data lives under the git-ignored
`research/data/`; never check it in. See [`docs/REFERENCES.md`](../docs/REFERENCES.md)
and SPEC-0008 §17 for the per-vendor terms.

## Layout

| Path | What |
|---|---|
| `hlr/io.py` | `iter_envelopes(path)`, `iter_frames(root, src, date_from, date_to)` — read segment files (`*.jsonl.zst`, `*.jsonl.zst.crashed`) in the SPEC-0008 §5/§6 format |
| `hlr/tables.py` | Shared §13.1 Parquet table schemas |
| `hlr/normalize.py` | Envelope → §13.1 Parquet tables (P-2); console script `hlr-normalize` |
| `hlr/costs.py` / `hlr/thresholds.py` | Cost model and study thresholds (P-3) |
| `hlr/episodes.py` | Episode detector + latency capture (P-4) |
| `hlr/competition.py` | `compete_usd` estimate from recorded public trades (ST-1) |
| `hlr/report.py` / `hlr/rank.py` | Report template and `RANKING.md` generator (P-5) |
| `hlr/backfill/tardis.py` | Tardis free-day downloader (B-1); console script `hlr-tardis-fetch` |
| `hlr/backfill/tardis_normalize.py` | Tardis rows → §13.1 tables (B-2); console script `hlr-tardis-normalize` |
| `hlr/backfill/hl_rest.py` | HL REST funding/candle backfill + daily poller (B-3); console script `hlr-hl-rest` |
| `tests/` | Pytest suite; builds zstd fixtures in `tmp_path` (no committed binaries) |

## Usage

```python
from hlr.io import SegmentError, iter_envelopes, iter_frames

# Finished segment: a damaged stream raises SegmentError instead of silently
# dropping data.
for env in iter_envelopes("data/rec/mainnet/hl-ws/2026-01-01/00/hl-ws-01-....jsonl.zst"):
    print(env["kind"], env["t_ns"])

# Crashed segment: a truncated tail stops cleanly at the last complete line.
for env in iter_envelopes(".../hl-ws-01-....jsonl.zst.crashed"):
    ...

# Every envelope for one source over an inclusive UTC date range.
root = "data/rec/mainnet"  # the network directory from SPEC-0008 §6
for env in iter_frames(root, "hl-ws", "2026-01-01", "2026-01-03"):
    ...
```

A `SegmentError` from a finished segment means the recorder reported the file as
complete but the bytes are truncated or corrupted: surface it, don't skip it
(SPEC-0008 G-1/G-4).

## Public-trades competition estimate (`hlr.competition`)

Episode studies had no competition model: `compete_usd` was 0 and every
"adjusted" number equalled the naive (generous) one. `hlr/competition.py`
supplies one from the recorded HL public `trades` stream (which the recorder
already writes; only the §13.1 `trades` parquet table is produced by
`hlr.normalize`).

```python
import polars as pl

from hlr.competition import estimate_competition, iter_recorded_trades

# Stream a range's HL trades as hourly public-trades frames (never a whole day).
chunks = iter_recorded_trades("data/rec/mainnet", "2026-10-01", "2026-10-01")
trades = pl.concat(chunks)  # ts_ns, coin, px, sz, side, tid

# Add per-episode compete_usd / gap_overlap to an episode table.
episodes = estimate_competition(episodes, trades, latency_ns=250_000_000, gaps=gaps)
```

`compete_usd` is the USD notional of *other* traders' executions that were on
the episode's opportunity side (a buy episode is competed by trades that lifted
the ask), reached the stale quote price (buy: `px <= quote`; sell: `px >=
quote`), and fall in the episode window `[t_start, t_end + latency_ns]`
(inclusive at both ends). It is capped at the episode's quoted notional
(`notional`, else `sz * quote_px`), never negative, and 0 without qualifying
trades. Duplicate `tid`s (HL replays trades on resubscribe/reconnect) count
once. `gap_overlap` is `True` when the window intersects a recorded gap for the
feed, so its `compete_usd` is a lower bound; the caller flags it rather than
dropping or imputing. Memory is bounded: the normalizer yields one hour at a
time and the estimator only keeps the trades of the requested coins.

Two spec gaps are flagged (see `hlr/competition.py`): the window is end-anchored
at `t_end + latency_ns` per ST-1, while SPEC-0008 §13.10 words fill competition
as `[t_start, t_start + L]`; and trades use local receive time `t_ns` (one clock
with `hlr.episodes`), not the venue/`time` field.

## HL REST backfill (B-3)

`hlr/backfill/hl_rest.py` (console script `hlr-hl-rest`) backfills
`funding_hist`, `bars(source="candle")` and `markets` from Hyperliquid's public
`POST /info` endpoint. No keys are ever used. It is resumable and idempotent
(merge-dedupe per date partition, state in `hl_rest_state.json`) and meters the
documented REST weight through a token bucket with backoff.

```sh
# Run these from research/ (as in Setup above); --out is relative to the cwd,
# so use data/parquet here. Run from the repo root with --out research/data/parquet.
uv run hlr-hl-rest backfill --out data/parquet   # one-shot
uv run hlr-hl-rest poll --out data/parquet       # scheduled daily 1m/5m
```

See [`../deploy/research/hl-rest-poller.md`](../deploy/research/hl-rest-poller.md)
for the daily cron entry (nothing is installed automatically).

## Tests

```sh
uv run pytest
uv run ruff check
```

If the machine exports an unrelated `PYTHONPATH` (for example a ROS
installation) whose packages register auto-loaded pytest plugins, isolate the
run:

```sh
PYTEST_DISABLE_PLUGIN_AUTOLOAD=1 PYTHONPATH= uv run pytest
```
