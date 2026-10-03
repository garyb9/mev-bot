"""Public-trades competition estimate for episode studies (ST-1, SPEC-0008 §13.10).

The day-3 preliminary studies had no competition model: the recorder writes HL
``trades`` frames but nothing turned them into a per-episode ``compete_usd``, so
every "competition-adjusted" number equalled the naive (generous) one. This
module closes that gap. It is self-contained research code: it never edits
:mod:`hlr.normalize`, :mod:`hlr.episodes` or :mod:`hlr.costs`, loads no keys and
does no network I/O.

It has two halves.

**Normalizer.** :func:`trades_from_envelopes` turns recorded HL ``trades``
envelope records into the public-trades table below, streaming one bounded chunk
per UTC hour (peak memory is a chunk, never a day). :func:`iter_recorded_trades`
glues it to :func:`hlr.io.iter_frames` for a date range. HL replays its recent
trades on resubscribe/reconnect, so rows are deduplicated by ``tid`` (the
bounded recent-``tid`` window in :func:`trades_from_envelopes`); envelopes that
are not ``frame`` records are ignored, which is what "ignore gap windows" means
here -- a ``gap_start``/``gap_end`` envelope can never fabricate a trade. The
recorded ``trades`` parquet table from :mod:`hlr.normalize` is left untouched by
this task.

Public-trades table columns (exact names)::

    ts_ns : Int64   local receive time of the frame, ns since the epoch
    coin  : String  HL coin as recorded (perps unchanged; no @N -> BASE/QUOTE)
    px    : Float64 trade price
    sz    : Float64 trade size (base units)
    side  : String  aggressor side, "buy" or "sell"
    tid   : String  HL trade id

**Estimator.** :func:`estimate_competition` adds ``compete_usd`` and
``gap_overlap`` to an episode table. Given an episode (a market, the window
``[t_start, t_end]`` in ns, the opportunity ``side`` -- the aggressor side we
would be -- and the stale quoted ``px`` plus a quoted ``sz``/``notional``), it
sums the USD notional of *other* traders' executions that:

* are on the same market and the same side (a buy-side opportunity is competed
  by trades that lifted the ask; a sell-side opportunity by trades that hit the
  bid) -- the wrong side never counts;
* reached the stale quote: a buy counts only at ``trade_px <= quote_px``, a sell
  only at ``trade_px >= quote_px`` -- a trade at a worse price (it never got to
  the quote) does not count;
* have ``t_start <= ts_ns <= t_end + latency_ns`` (inclusive at both ends);
* are deduplicated by ``tid`` (a replayed trade counts once).

The result is ``min(quoted_notional, sum)``, never negative, and ``0`` when
there is no qualifying trade. ``gap_overlap`` is ``True`` when the episode
window intersects a recorded gap for the trades feed; a gapped episode's
``compete_usd`` is then a **lower bound** (trades inside the gap cannot exist),
so callers must flag it rather than drop or impute it.

Two deliberate readings, both flagged for the spec (which is terse here):

* The window is ``[t_start, t_end + latency_ns]``, per the ST-1 task, whereas
  SPEC-0008 §13.10 words fill competition as ``[t_start, t_start + L]``. The
  end-anchored window is the conservative (larger) one and is what is
  implemented.
* Trades are timestamped with the local receive time ``t_ns`` of their frame so
  the estimator and :mod:`hlr.episodes` (which builds its timeline on frame
  ``t_ns``) share one clock. No reveal-lag correction is applied.

Research only: never imported by, or deployed with, the trading bot.
"""

from __future__ import annotations

import bisect
import os
from collections import deque
from collections.abc import Iterable, Iterator, Mapping
from typing import Any

import orjson
import polars as pl

from hlr.io import iter_frames

__all__ = [
    "CHUNK_NS",
    "COMPETE_COLUMN",
    "DAY_NS",
    "DEFAULT_MAX_TIDS",
    "GAP_OVERLAP_COLUMN",
    "HL_WS_SOURCE",
    "HOUR_NS",
    "TRADES_CHANNEL",
    "TRADE_COLUMNS",
    "CompetitionError",
    "estimate_competition",
    "iter_recorded_trades",
    "trades_from_envelopes",
]

#: Nanoseconds per hour: the normalizer's streaming chunk.
HOUR_NS = 3_600 * 1_000_000_000
#: Nanoseconds per UTC day.
DAY_NS = 24 * HOUR_NS
#: Default chunk size (one UTC hour) for :func:`trades_from_envelopes`.
CHUNK_NS = HOUR_NS

#: The HL WS channel whose frames hold public trades.
TRADES_CHANNEL = "trades"
#: The recorder source id that carries HL WS frames.
HL_WS_SOURCE = "hl-ws"

#: The added competition columns.
COMPETE_COLUMN = "compete_usd"
GAP_OVERLAP_COLUMN = "gap_overlap"

#: Recent-``tid`` window: enough to catch a replay at the next few reconnects
#: while keeping the dedupe set bounded.
DEFAULT_MAX_TIDS = 250_000

#: Exact public-trades table schema.
TRADE_COLUMNS: dict[str, pl.DataType] = {
    "ts_ns": pl.Int64,
    "coin": pl.String,
    "px": pl.Float64,
    "sz": pl.Float64,
    "side": pl.String,
    "tid": pl.String,
}


class CompetitionError(ValueError):
    """Malformed inputs: a missing column, an invalid parameter, a bad dtype."""


# --------------------------------------------------------------------------
# Normalizer
# --------------------------------------------------------------------------


def _float(value: Any) -> float | None:
    """Parse a JSON scalar as float, or ``None`` when absent/unparseable."""
    if value is None:
        return None
    try:
        return float(value)
    except (TypeError, ValueError):
        return None


def _side(value: Any) -> str | None:
    """Normalize an HL aggressor side to ``buy``/``sell`` (``None`` if unknown)."""
    if value is None:
        return None
    text = str(value)
    if text in ("B", "b", "buy"):
        return "buy"
    if text in ("A", "a", "sell"):
        return "sell"
    return None


def _trades_frame(rows: list[tuple[Any, ...]]) -> pl.DataFrame:
    """Build one typed public-trades frame from row tuples."""
    return pl.DataFrame(rows, schema=list(TRADE_COLUMNS.items()), orient="row")


def trades_from_envelopes(
    envelopes: Iterable[Mapping[str, Any]],
    *,
    chunk_ns: int = CHUNK_NS,
    max_tids: int = DEFAULT_MAX_TIDS,
) -> Iterator[pl.DataFrame]:
    """Stream recorded HL ``trades`` envelopes into hourly public-trades chunks.

    Each yielded frame is bounded to one ``chunk_ns`` bucket of the frame receive
    time (default one hour) and carries :data:`TRADE_COLUMNS`. Only ``frame``
    envelopes are read; ``gap_start``/``gap_end``/``rest``/``sub``/... are
    ignored, so a gap can never fabricate a trade. A frame whose ``raw`` is not
    a ``trades`` channel, or a trade missing ``coin``/``px``/``sz``, is skipped.

    HL replays recent trades on resubscribe/reconnect. To count a replayed
    ``tid`` once while staying bounded, the last ``max_tids`` ids are remembered
    (a rolling set): a duplicate inside that window is dropped, a duplicate that
    falls out of it is not. A trade without a ``tid`` is always kept.
    """
    if chunk_ns <= 0:
        raise CompetitionError(f"chunk_ns must be > 0, got {chunk_ns}")
    if max_tids <= 0:
        raise CompetitionError(f"max_tids must be > 0, got {max_tids}")

    seen: set[str] = set()
    recent: deque[str] = deque()
    buffer: list[tuple[Any, ...]] = []
    current_bucket: int | None = None

    for env in envelopes:
        if not isinstance(env, Mapping) or env.get("kind") != "frame":
            continue
        t_ns = env.get("t_ns")
        raw = env.get("raw")
        if not isinstance(t_ns, int) or not isinstance(raw, str):
            continue
        try:
            frame = orjson.loads(raw)
        except orjson.JSONDecodeError:
            continue
        if not isinstance(frame, Mapping) or frame.get("channel") != TRADES_CHANNEL:
            continue
        data = frame.get("data")
        if not isinstance(data, list):
            continue

        bucket = t_ns // chunk_ns
        if current_bucket is None:
            current_bucket = bucket
        elif bucket != current_bucket:
            if buffer:
                yield _trades_frame(buffer)
                buffer = []
            current_bucket = bucket

        for trade in data:
            if not isinstance(trade, Mapping):
                continue
            coin = trade.get("coin")
            px = _float(trade.get("px"))
            sz = _float(trade.get("sz"))
            if coin is None or px is None or sz is None:
                continue
            raw_tid = trade.get("tid")
            tid = None if raw_tid is None else str(raw_tid)
            if tid is not None:
                if tid in seen:
                    continue
                seen.add(tid)
                recent.append(tid)
                if len(recent) > max_tids and recent:
                    seen.discard(recent.popleft())
            buffer.append((t_ns, str(coin), px, sz, _side(trade.get("side")), tid))

    if buffer:
        yield _trades_frame(buffer)


def iter_recorded_trades(
    root: str | os.PathLike[str],
    date_from: str,
    date_to: str,
    *,
    src: str = HL_WS_SOURCE,
    chunk_ns: int = CHUNK_NS,
    max_tids: int = DEFAULT_MAX_TIDS,
) -> Iterator[pl.DataFrame]:
    """Stream a date range's recorded HL trades as hourly chunks.

    ``root``/``date_from``/``date_to`` follow :func:`hlr.io.iter_frames`: the
    recorder network directory and an inclusive UTC ``YYYY-MM-DD`` range. The
    result is the :func:`trades_from_envelopes` generator over that source, so no
    whole day (let alone the range) is ever held in memory by this function.
    """
    yield from trades_from_envelopes(
        iter_frames(root, src, date_from, date_to),
        chunk_ns=chunk_ns,
        max_tids=max_tids,
    )


# --------------------------------------------------------------------------
# Estimator
# --------------------------------------------------------------------------


def _require_columns(frame: pl.DataFrame, columns: Iterable[str]) -> None:
    """Raise :class:`CompetitionError` when ``frame`` lacks a column."""
    missing = [name for name in columns if name not in frame.columns]
    if missing:
        raise CompetitionError(f"frame is missing column(s): {', '.join(missing)}")


def _episode_side(value: Any) -> str:
    """Normalize an episode's opportunity side, raising on an unknown value."""
    side = _side(value)
    if side is None:
        raise CompetitionError(f"episode side must be buy/sell (B/A), got {value!r}")
    return side


def _union_gaps(gaps: pl.DataFrame, src: str | None) -> list[tuple[int, int]]:
    """Return the unioned ``(start_ns, end_ns)`` gap intervals, sorted by start."""
    _require_columns(gaps, ["start_ns", "end_ns"])
    subset = gaps
    if src is not None and "src" in gaps.columns:
        subset = subset.filter(pl.col("src") == src)
    intervals = sorted(
        (int(row["start_ns"]), int(row["end_ns"]))
        for row in subset.select("start_ns", "end_ns").iter_rows(named=True)
        if row["start_ns"] is not None and row["end_ns"] is not None
    )
    unioned: list[tuple[int, int]] = []
    for start, end in intervals:
        if end < start:
            start, end = end, start
        if unioned and start <= unioned[-1][1]:
            prev_start, prev_end = unioned[-1]
            unioned[-1] = (prev_start, max(prev_end, end))
        else:
            unioned.append((start, end))
    return unioned


def _overlaps(union_gaps: list[tuple[int, int]], low_ns: int, high_ns: int) -> bool:
    """Whether ``[low_ns, high_ns]`` intersects any unioned gap interval."""
    if not union_gaps:
        return False
    starts = [start for start, _ in union_gaps]
    index = bisect.bisect_right(starts, high_ns) - 1
    return index >= 0 and union_gaps[index][1] >= low_ns


def estimate_competition(
    episodes: pl.DataFrame,
    trades: pl.DataFrame,
    *,
    latency_ns: int = 0,
    gaps: pl.DataFrame | None = None,
    gap_src: str | None = HL_WS_SOURCE,
    coin_col: str = "coin",
    start_col: str = "t_start",
    end_col: str = "t_end",
    side_col: str = "side",
    price_col: str = "px",
    size_col: str | None = "sz",
    notional_col: str | None = "notional",
    trade_ts_col: str = "ts_ns",
    trade_coin_col: str = "coin",
    trade_price_col: str = "px",
    trade_size_col: str = "sz",
    trade_side_col: str = "side",
    trade_tid_col: str = "tid",
) -> pl.DataFrame:
    """Add per-episode ``compete_usd`` and ``gap_overlap`` columns.

    ``episodes`` needs the opportunity market (``coin_col``), window
    (``start_col``, ``end_col``, both inclusive ns), ``side_col`` (aggressor side
    we would be) and ``price_col`` (the stale quote), plus a quoted size
    (``size_col``) or notional (``notional_col``; when both are given the
    notional is authoritative). ``trades`` is the public-trades table from
    :func:`trades_from_envelopes` (``ts_ns``/``coin``/``px``/``sz``/``side``/
    ``tid``). ``latency_ns`` extends the window to ``end + latency_ns``.

    The returned frame preserves the input columns and row order and appends
    :data:`COMPETE_COLUMN` (Float64, capped at the quoted notional and never
    negative) and :data:`GAP_OVERLAP_COLUMN` (Boolean). ``gaps`` is the ``gaps``
    table (``start_ns``/``end_ns``, optionally ``src``); when given, an episode
    whose window intersects one of the feed's gaps is flagged, because its
    ``compete_usd`` is then only a lower bound. An empty input yields a frame
    with the same columns and typed zero-row competition columns.
    """
    if latency_ns < 0:
        raise CompetitionError(f"latency_ns must be >= 0, got {latency_ns}")
    _require_columns(
        episodes, [coin_col, start_col, end_col, side_col, price_col]
    )
    has_notional = notional_col is not None and notional_col in episodes.columns
    has_size = size_col is not None and size_col in episodes.columns
    if not has_notional and not has_size:
        raise CompetitionError(
            "episodes need a quoted size column or a notional column"
        )
    _require_columns(
        trades,
        [trade_ts_col, trade_coin_col, trade_price_col, trade_size_col, trade_side_col],
    )

    px_expr = pl.col(price_col).cast(pl.Float64, strict=False)
    size_expr = (
        pl.col(size_col).cast(pl.Float64, strict=False) * px_expr
        if has_size and size_col is not None
        else None
    )
    if has_notional and notional_col is not None:
        notional_expr = pl.col(notional_col).cast(pl.Float64, strict=False)
        quoted_expr = (
            pl.coalesce([notional_expr, size_expr]) if size_expr is not None else notional_expr
        )
    elif size_expr is not None:
        quoted_expr = size_expr
    else:  # pragma: no cover - guarded above
        raise CompetitionError("episodes need a quoted size or notional column")

    episode = episodes.select(
        pl.col(coin_col).alias("_coin"),
        pl.col(start_col).cast(pl.Int64, strict=False).alias("_start"),
        pl.col(end_col).cast(pl.Int64, strict=False).alias("_end"),
        pl.col(side_col).alias("_side"),
        px_expr.alias("_px"),
        quoted_expr.alias("_quoted"),
    )

    wanted = {coin for coin in episode["_coin"].to_list() if coin is not None}
    buckets: dict[Any, tuple[list[int], list[float], list[float], list[str]]] = {}
    seen: set[str] = set()
    if wanted and trades.height:
        for row in trades.iter_rows(named=True):
            coin = row[trade_coin_col]
            if coin not in wanted:
                continue
            raw_tid = row[trade_tid_col]
            px = row[trade_price_col]
            sz = row[trade_size_col]
            if px is None or sz is None:
                continue
            tid = None if raw_tid is None else str(raw_tid)
            if tid is not None:
                if tid in seen:
                    continue
                seen.add(tid)
            ts = int(row[trade_ts_col])
            trade_side = _side(row[trade_side_col])
            ts_list, px_list, sz_list, side_list = buckets.setdefault(
                coin, ([], [], [], [])
            )
            ts_list.append(ts)
            px_list.append(float(px))
            sz_list.append(float(sz))
            side_list.append(trade_side or "")

    ordered: dict[Any, tuple[list[int], list[float], list[float], list[str]]] = {}
    for coin, (ts_list, px_list, sz_list, side_list) in buckets.items():
        order = sorted(range(len(ts_list)), key=ts_list.__getitem__)
        ordered[coin] = (
            [ts_list[i] for i in order],
            [px_list[i] for i in order],
            [sz_list[i] for i in order],
            [side_list[i] for i in order],
        )

    union_gaps = _union_gaps(gaps, gap_src) if gaps is not None else []

    compete: list[float] = []
    overlap: list[bool] = []
    for row in episode.iter_rows(named=True):
        start = row["_start"]
        end = row["_end"]
        quote_px = row["_px"]
        quoted = row["_quoted"]
        side = row["_side"]
        if start is None or end is None:
            raise CompetitionError("episode start/end must be non-null integers")
        window_hi = int(end) + latency_ns
        overlap.append(_overlaps(union_gaps, int(start), window_hi))
        bucket = ordered.get(row["_coin"])
        if bucket is None or quote_px is None or quoted is None or quoted <= 0:
            compete.append(0.0)
            continue
        agg_side = _episode_side(side)
        ts_list, px_list, sz_list, side_list = bucket
        lo = bisect.bisect_left(ts_list, int(start))
        hi = bisect.bisect_right(ts_list, window_hi)
        total = 0.0
        for index in range(lo, hi):
            if side_list[index] != agg_side:
                continue
            trade_px = px_list[index]
            if agg_side == "buy":
                if trade_px > quote_px:
                    continue
            elif trade_px < quote_px:
                continue
            total += trade_px * sz_list[index]
        compete.append(max(0.0, min(float(quoted), total)))

    return episodes.with_columns(
        pl.Series(COMPETE_COLUMN, compete, dtype=pl.Float64),
        pl.Series(GAP_OVERLAP_COLUMN, overlap, dtype=pl.Boolean),
    )
