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

## Layout

| Path | What |
|---|---|
| `hlr/io.py` | `iter_envelopes(path)`, `iter_frames(root, src, date_from, date_to)` — read segment files (`*.jsonl.zst`, `*.jsonl.zst.crashed`) in the SPEC-0008 §5/§6 format |
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
