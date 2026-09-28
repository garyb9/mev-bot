"""Recorder segments → §13.1 parquet tables (SPEC-0008 §13.1, task P-2).

Reads the raw segments written by ``hl record`` (SPEC-0008 Part A) through
:mod:`hlr.io` and writes the shared §13.1 parquet tables under
``research/data/parquet/{table}/date=YYYY-MM-DD/{source}*.parquet``, reusing the
schemas and provenance columns from :mod:`hlr.tables`:

* ``hl-ws`` ``bbo`` → ``bbo`` (``venue="hl"``)
* ``hl-ws`` ``l2Book`` → ``book`` (``venue="hl-book"``) and its level 0 → ``bbo`` (``venue="hl-book"``)
* ``hl-ws`` ``trades`` → ``trades``
* ``hl-ws`` ``activeAssetCtx`` → ``ctx``
* ``hl-rest`` ``metaAndAssetCtxs`` → ``ctx``
* ``gap_start``/``gap_end``, crashed segments and ``seq`` holes → ``gaps``

Every row carries ``source="recorder"`` and the §13.11 ``fidelity`` class:
``H1`` for the update-level ``bbo``/``trades`` streams, ``H2`` for the
second-cadence ``l2Book``/``ctx`` snapshots (the S3-archive classes), ``H1`` for
gaps (matching B-2).

Scope. Only streams the recorder actually writes are normalized. The HL REST
``fundingHistory``/``candleSnapshot``/``meta``/``spotMeta`` snapshots and the
CEX/options/equity sources are not mapped here: the funding, candle and market
tables belong to B-3/B-4 and the CEX sources (R-8) are not implemented yet.
``spotMeta`` responses are still read — not written — to resolve spot ``@N`` to
``BASE/QUOTE`` so an index never leaks into a table (§13.1).

Symbol naming follows §13.1: HL perps keep their name (``BTC``), HIP-3 markets
keep the dex prefix (``xyz:TSLA``) and spot pairs are resolved to
``BASE/QUOTE`` through the recorded ``spotMeta`` (never ``@N``); an unresolvable
spot symbol raises rather than writing an index.

Memory. Envelopes are streamed one at a time; rows accumulate in a bounded
buffer and each full batch is written straight to its partition as an atomic
``.tmp`` → final rename. Peak RSS is set by the batch size, not by the day: a
stream too large for one parquet batch becomes several part files rather than
one merged file, because parquet cannot be appended and concatenating chunks for
a merge makes polars' ``sink_parquet`` materialize the whole stream. A reader
still loads a stream with one glob (``{source}*.parquet``). Re-running a
partition overwrites its own ``recorder*`` parts and leaves the Tardis (B-2)
parts untouched.

Research only: never imported by, or deployed with, the trading bot. It reads no
keys and no network.
"""

from __future__ import annotations

import argparse
import datetime as _dt
import os
import shutil
import sys
from collections.abc import Iterable, Mapping, Sequence
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import orjson
import polars as pl

from hlr import tables
from hlr.io import SegmentError, iter_frames

__all__ = [
    "DAY_NS",
    "DEFAULT_SOURCES",
    "FIDELITY_WS",
    "KNOWN_SPOT_SYMBOLS",
    "MARKET_TABLES",
    "SOURCE",
    "MarketMappingError",
    "NormalizeError",
    "NormalizeReport",
    "TableCount",
    "main",
    "normalize",
    "recorder_market",
    "spot_index_map",
]

#: SPEC-0008 §13.11 source tag for rows derived from our own recorder segments.
SOURCE = "recorder"

#: Sources this normalizer understands, in processing order. ``hl-rest`` is read
#: first so a recorded ``spotMeta`` populates the ``@N`` → ``BASE/QUOTE`` map
#: before any ``hl-ws`` frame needs it.
DEFAULT_SOURCES: tuple[str, ...] = ("hl-rest", "hl-ws")

#: The §13.1 tables P-2 writes (the rest belong to B-3/B-4 and later tasks).
MARKET_TABLES: tuple[str, ...] = ("bbo", "book", "trades", "ctx", "gaps")

#: §13.11 fidelity of the update-level WS streams (``bbo``, ``trades``).
FIDELITY_WS = tables.FIDELITY_H1

#: §13.11 fidelity of the second-cadence snapshot streams (``l2Book``, ``ctx``).
FIDELITY_SNAPSHOT = tables.FIDELITY_H2

#: One UTC day in nanoseconds.
DAY_NS = 86_400 * 1_000_000_000

#: Fallback ``@N`` → ``BASE/QUOTE`` map (mirrors B-1's ``KNOWN_SPOT_SYMBOLS``):
#: ``@107`` = HYPE/USDC on 2026-09-28. A recorded ``spotMeta`` supersedes it.
KNOWN_SPOT_SYMBOLS: dict[str, str] = {"@107": "HYPE/USDC"}

#: Parquet row-group size for a written part.
_ROW_GROUP = 250_000

#: Buffered rows across every stream before a batch is written; bounded memory
#: knob (peak RSS scales with this, not with the day).
_FLUSH_ROWS = 100_000


class NormalizeError(Exception):
    """Base class for every error this module raises."""


class MarketMappingError(NormalizeError):
    """A recorder coin cannot be mapped to a SPEC-0008 §13.1 market name."""


@dataclass(frozen=True)
class TableCount:
    """Row count of one written ``(table, date)`` partition."""

    table: str
    day: _dt.date
    rows: int
    path: Path


@dataclass
class NormalizeReport:
    """Outcome of one :func:`normalize` run (for the CLI summary and tests)."""

    envelopes: int
    skipped_envelopes: int
    tables: list[TableCount] = field(default_factory=list)
    gap_files: int = 0
    gap_rows: int = 0

    def rows_for(self, table: str) -> int:
        """Total rows written for ``table`` across every date partition."""
        return sum(entry.rows for entry in self.tables if entry.table == table)


# --------------------------------------------------------------------------
# Symbol mapping
# --------------------------------------------------------------------------


def spot_index_map(spot_meta: Mapping[str, Any] | None) -> dict[str, str]:
    """Build an ``@N → BASE/QUOTE`` map from a Hyperliquid ``spotMeta`` response.

    Pairs are recovered from each universe entry's base/quote token indices via
    the ``tokens`` array. An entry whose ``name`` is already a ``BASE/QUOTE``
    string keeps that name. :data:`KNOWN_SPOT_SYMBOLS` is always included as a
    fallback so a recorder day without a ``spotMeta`` refresh still resolves the
    most common pair.
    """
    mapping = dict(KNOWN_SPOT_SYMBOLS)
    if not spot_meta:
        return mapping
    tokens = {
        token["index"]: token["name"]
        for token in spot_meta.get("tokens", [])
        if isinstance(token, Mapping) and "index" in token and "name" in token
    }
    for entry in spot_meta.get("universe", []):
        if not isinstance(entry, Mapping):
            continue
        index = entry.get("index")
        pair = entry.get("tokens")
        name = entry.get("name")
        mapped: str | None = None
        if isinstance(pair, (list, tuple)) and len(pair) >= 2:
            base = tokens.get(pair[0])
            quote = tokens.get(pair[1])
            if base is not None and quote is not None:
                mapped = f"{base}/{quote}"
        if isinstance(name, str) and name and not name.startswith("@"):
            mapped = name
        if index is not None and mapped is not None:
            mapping[f"@{index}"] = mapped
    return mapping


def recorder_market(
    coin: str,
    *,
    spot_symbols: Mapping[str, str] | None = None,
) -> str:
    """Map a recorder coin to the §13.1 market name.

    * HL perps are unchanged (``BTC``, ``kPEPE``).
    * HIP-3 perps are unchanged (``xyz:TSLA``).
    * HL spot ``@N`` is resolved through ``spot_symbols`` (recorded ``spotMeta``
      plus :data:`KNOWN_SPOT_SYMBOLS`); an unknown index raises rather than
      leaking an ``@N`` into the data. A ``BASE/QUOTE`` name passes through.
    """
    name = coin.strip()
    if not name:
        raise MarketMappingError("empty recorder coin")
    if not name.startswith("@"):
        return name
    table = dict(KNOWN_SPOT_SYMBOLS)
    if spot_symbols:
        table.update(spot_symbols)
    mapped = table.get(name)
    if mapped is None:
        raise MarketMappingError(
            f"no BASE/QUOTE known for spot `{name}`; pass --spot-meta or "
            "record an hl-rest spotMeta refresh"
        )
    return mapped


# --------------------------------------------------------------------------
# Gap extraction
# --------------------------------------------------------------------------


@dataclass
class _GapTracker:
    """Per-``(src, conn)`` gap state across one normalize run.

    Consumes every envelope of a connection in path order and produces closed
    gap intervals:

    * an explicit ``gap_start`` … ``gap_end`` pair;
    * an unpaired ``gap_start`` (for example ``drop``/``shutdown``), closed at
      the next data envelope;
    * a crash: a ``segment_open`` with no preceding ``segment_close`` starts a
      gap at the crashed segment's last data time (SPEC-0008 §5.3);
    * a ``seq`` hole: two consecutive counters more than one apart.
    """

    src: str
    conn: str
    gaps: list[tuple[int, int, str]] = field(default_factory=list)
    _open_start: int | None = None
    _open_reason: str = ""
    _segment_open: bool = False
    _prev_data_ns: int | None = None
    _last_t_ns: int | None = None
    _last_seq: int | None = None

    def _close(self, end_ns: int) -> None:
        """Close an open gap at ``end_ns`` (never before its start)."""
        if self._open_start is None:
            return
        self.gaps.append((self._open_start, max(end_ns, self._open_start), self._open_reason))
        self._open_start = None
        self._open_reason = ""

    def _open(self, start_ns: int, reason: str) -> None:
        """Open a gap, closing any previous one at the same instant."""
        if self._open_start is not None:
            self._close(start_ns)
        self._open_start = start_ns
        self._open_reason = reason

    def observe(self, env: Mapping[str, Any]) -> None:
        """Update the tracker from one envelope."""
        kind = env.get("kind")
        t_ns = env.get("t_ns")
        if not isinstance(t_ns, int):
            return
        if kind == "segment_open":
            if self._segment_open and self._open_start is None and self._prev_data_ns is not None:
                # The previous segment on this connection never closed: crash.
                self._open(self._prev_data_ns, "crash")
            self._segment_open = True
            self._last_t_ns = t_ns
            return
        if kind == "segment_close":
            self._segment_open = False
            self._last_t_ns = t_ns
            return

        seq = env.get("seq")
        if isinstance(seq, int):
            if self._last_seq is not None and seq > self._last_seq + 1 and self._open_start is None:
                start = self._last_t_ns if self._last_t_ns is not None else t_ns
                self._open(start, "seq")
            self._last_seq = seq

        if kind in ("gap_start", "gap_end"):
            meta = env.get("meta")
            if kind == "gap_start":
                reason = meta.get("reason") if isinstance(meta, Mapping) else None
                self._open(t_ns, str(reason) if reason else "gap")
            else:
                self._close(t_ns)
            self._last_t_ns = t_ns
            return

        if kind in ("frame", "frame_bin", "rest"):
            # Data resumed: an unpaired gap (drop/shutdown/crash) ends here.
            self._close(t_ns)
            self._prev_data_ns = t_ns
            self._last_t_ns = t_ns
            return

        self._last_t_ns = t_ns

    def finish(self, range_end_ns: int) -> None:
        """Close a still-open gap at the end of the requested range."""
        if self._segment_open and self._open_start is None and self._prev_data_ns is not None:
            self._open(self._prev_data_ns, "crash")
        if self._open_start is not None:
            self._close(range_end_ns)


# --------------------------------------------------------------------------
# Bounded parquet writer (one bounded part file per flush)
# --------------------------------------------------------------------------


def _sanitize(text: str) -> str:
    """Make ``text`` safe for a filename component."""
    return "".join(ch if ch.isalnum() or ch in "-_." else "_" for ch in text)


def _part_name(table: str, src: str, conn: str, channel: str, index: int) -> str:
    """Part filename for one bounded batch, globbable as ``{SOURCE}*.parquet``.

    A stream is written as one file per bounded batch (``index``) rather than a
    single file per stream: parquet cannot be appended, and re-reading many
    chunks to concatenate them makes polars' ``sink_parquet`` materialize the
    whole stream, which would break the bounded-memory rule. A reader still
    loads a stream with one glob (``{SOURCE}*.parquet``).
    """
    if table == "gaps":
        return f"{SOURCE}.{_sanitize(src)}.{_sanitize(conn)}.{index:05d}.parquet"
    return (
        f"{SOURCE}.{_sanitize(src)}.{_sanitize(conn)}.{_sanitize(channel)}."
        f"{index:05d}.parquet"
    )


class _ParquetSink:
    """Bounded writer: buffer ``flush_rows`` rows, then write one atomic part."""

    def __init__(
        self,
        out_dir: Path,
        *,
        flush_rows: int = _FLUSH_ROWS,
    ) -> None:
        self.out_dir = out_dir
        self.flush_rows = max(1, flush_rows)
        # key = (table, date_iso, src, conn, channel) -> buffered rows
        self._buffers: dict[tuple[str, str, str, str, str], list[tuple[Any, ...]]] = {}
        self._part_index: dict[tuple[str, str, str, str, str], int] = {}
        self._produced: dict[str, set[str]] = {}
        self._rows: dict[tuple[str, str], int] = {}
        self._buffered = 0

    def add(
        self,
        table: str,
        day: str,
        src: str,
        conn: str,
        channel: str,
        row: tuple[Any, ...],
    ) -> None:
        """Buffer one row for ``(table, day, src, conn, channel)``."""
        key = (table, day, src, conn, channel)
        bucket = self._buffers.get(key)
        if bucket is None:
            bucket = self._buffers[key] = []
        bucket.append(row)
        self._buffered += 1
        if self._buffered >= self.flush_rows:
            self.flush()

    def flush(self) -> None:
        """Write every non-empty buffer as its own bounded part file."""
        for key, rows in self._buffers.items():
            if not rows:
                continue
            self._write_part(key, rows)
            rows.clear()
        self._buffered = 0

    def _write_part(self, key: tuple[str, str, str, str, str], rows: list[tuple[Any, ...]]) -> None:
        table, day, src, conn, channel = key
        index = self._part_index.get(key, 0)
        self._part_index[key] = index + 1
        name = _part_name(table, src, conn, channel, index)
        final = self.out_dir / table / f"date={day}" / name
        _write_frame_atomic(table, rows, final)
        self._produced.setdefault(f"{table}|{day}", set()).add(name)
        self._rows[(table, day)] = self._rows.get((table, day), 0) + len(rows)

    def finalize(self) -> tuple[list[TableCount], dict[str, set[str]]]:
        """Flush the tail and return the partition counts and produced part names."""
        self.flush()
        counts = [
            TableCount(table, _dt.date.fromisoformat(day), rows, self.out_dir / table / f"date={day}")
            for (table, day), rows in sorted(self._rows.items())
        ]
        return counts, self._produced


def _write_frame_atomic(table: str, rows: list[tuple[Any, ...]], final: Path) -> None:
    """Write one bounded part with the exact table schema, atomically renamed."""
    final.parent.mkdir(parents=True, exist_ok=True)
    tmp = final.with_name(final.name + ".tmp")
    try:
        frame = pl.DataFrame(rows, schema=list(tables.schema(table).items()), orient="row")
        frame.write_parquet(tmp, compression="zstd", row_group_size=_ROW_GROUP)
        os.replace(tmp, final)
    except BaseException:
        tmp.unlink(missing_ok=True)
        raise


def _drop_stale_parts(directory: Path, keep: set[str]) -> None:
    """Remove ``recorder*`` parts of a rewritten partition that are not ``keep``."""
    if not directory.is_dir():
        return
    for existing in directory.glob(f"{SOURCE}*.parquet"):
        if existing.name not in keep:
            existing.unlink()


# --------------------------------------------------------------------------
# Row helpers
# --------------------------------------------------------------------------


def _day_start_ns(day: _dt.date) -> int:
    """Unix nanoseconds at 00:00:00 UTC on ``day`` (integer-exact)."""
    return (day - _dt.date(1970, 1, 1)).days * DAY_NS


def _date_of_ns(t_ns: int) -> _dt.date:
    """The UTC date of a nanosecond Unix timestamp."""
    return _dt.date(1970, 1, 1) + _dt.timedelta(days=t_ns // DAY_NS)


def _iter_days(first: _dt.date, last: _dt.date) -> Iterable[_dt.date]:
    """Yield every UTC day in the inclusive range ``[first, last]``."""
    day = first
    while day <= last:
        yield day
        day += _dt.timedelta(days=1)


def _f(obj: Any, key: str) -> float | None:
    """Read ``obj[key]`` as float64, or ``None`` when absent/null/unparseable."""
    if not isinstance(obj, Mapping):
        return None
    value = obj.get(key)
    if value is None:
        return None
    try:
        return float(value)
    except (TypeError, ValueError):
        return None


def _i(obj: Any, key: str) -> int | None:
    """Read ``obj[key]`` as int64, or ``None`` when absent/null/unparseable."""
    value = _f(obj, key)
    return None if value is None else int(value)


def _s(value: Any) -> str | None:
    """Stringify a JSON scalar, mapping null to ``None``."""
    if value is None:
        return None
    return str(value)


def _trade_side(value: Any) -> str | None:
    """Normalize an HL trade aggressor side to ``buy``/``sell`` (like B-2)."""
    if value is None:
        return None
    text = str(value)
    if text in ("B", "b", "buy"):
        return "buy"
    if text in ("A", "a", "sell"):
        return "sell"
    return text


# --------------------------------------------------------------------------
# Frame and REST decoding
# --------------------------------------------------------------------------


def _handle_frame(
    raw: str,
    env: Mapping[str, Any],
    src: str,
    conn: str,
    sink: _ParquetSink,
    spot_symbols: Mapping[str, str],
    skipped: list[int],
) -> None:
    """Route one ``hl-ws`` text frame to its §13.1 table(s)."""
    try:
        frame = orjson.loads(raw)
    except orjson.JSONDecodeError:
        skipped[0] += 1
        return
    if not isinstance(frame, Mapping):
        skipped[0] += 1
        return
    channel = frame.get("channel")
    data = frame.get("data")
    t_ns = env["t_ns"]
    day = _date_of_ns(t_ns).isoformat()

    if channel == "bbo" and isinstance(data, Mapping):
        market = recorder_market(str(data.get("coin", "")), spot_symbols=spot_symbols)
        levels = data.get("bbo") or []
        bid = levels[0] if isinstance(levels, (list, tuple)) and len(levels) > 0 else None
        ask = levels[1] if isinstance(levels, (list, tuple)) and len(levels) > 1 else None
        sink.add(
            "bbo",
            day,
            src,
            conn,
            "bbo",
            (
                t_ns,
                _i(data, "time"),
                "hl",
                market,
                _f(bid, "px"),
                _f(bid, "sz"),
                _f(ask, "px"),
                _f(ask, "sz"),
                SOURCE,
                FIDELITY_WS,
            ),
        )
        return

    if channel == "l2Book" and isinstance(data, Mapping):
        market = recorder_market(str(data.get("coin", "")), spot_symbols=spot_symbols)
        levels = data.get("levels") or [[], []]
        bids = levels[0] if isinstance(levels, (list, tuple)) and len(levels) > 0 else []
        asks = levels[1] if isinstance(levels, (list, tuple)) and len(levels) > 1 else []
        for side, book_side in (("bid", bids), ("ask", asks)):
            for level, entry in enumerate(book_side):
                sink.add(
                    "book",
                    day,
                    src,
                    conn,
                    "l2book",
                    (
                        t_ns,
                        _i(data, "time"),
                        "hl-book",
                        market,
                        side,
                        level,
                        _f(entry, "px"),
                        _f(entry, "sz"),
                        _i(entry, "n"),
                        SOURCE,
                        FIDELITY_SNAPSHOT,
                    ),
                )
        bid = bids[0] if isinstance(bids, (list, tuple)) and len(bids) > 0 else None
        ask = asks[0] if isinstance(asks, (list, tuple)) and len(asks) > 0 else None
        sink.add(
            "bbo",
            day,
            src,
            conn,
            "l2book",
            (
                t_ns,
                _i(data, "time"),
                "hl-book",
                market,
                _f(bid, "px"),
                _f(bid, "sz"),
                _f(ask, "px"),
                _f(ask, "sz"),
                SOURCE,
                FIDELITY_SNAPSHOT,
            ),
        )
        return

    if channel == "trades" and isinstance(data, (list, tuple)):
        for trade in data:
            if not isinstance(trade, Mapping):
                skipped[0] += 1
                continue
            market = recorder_market(str(trade.get("coin", "")), spot_symbols=spot_symbols)
            users = trade.get("users") or []
            buyer = users[0] if isinstance(users, (list, tuple)) and len(users) > 0 else None
            seller = users[1] if isinstance(users, (list, tuple)) and len(users) > 1 else None
            sink.add(
                "trades",
                day,
                src,
                conn,
                "trades",
                (
                    t_ns,
                    _i(trade, "time"),
                    "hl",
                    market,
                    _trade_side(trade.get("side")),
                    _f(trade, "px"),
                    _f(trade, "sz"),
                    _s(trade.get("tid")),
                    _s(trade.get("hash")),
                    _s(buyer),
                    _s(seller),
                    SOURCE,
                    FIDELITY_WS,
                ),
            )
        return

    if channel == "activeAssetCtx" and isinstance(data, Mapping):
        market = recorder_market(str(data.get("coin", "")), spot_symbols=spot_symbols)
        ctx = data.get("ctx")
        sink.add(
            "ctx",
            day,
            src,
            conn,
            "activeassetctx",
            (
                t_ns,
                market,
                _f(ctx, "funding"),
                _f(ctx, "openInterest"),
                _f(ctx, "oraclePx"),
                _f(ctx, "markPx"),
                _f(ctx, "midPx"),
                _f(ctx, "premium"),
                _f(ctx, "dayNtlVlm"),
                SOURCE,
                FIDELITY_SNAPSHOT,
            ),
        )
        return

    # allMids / activeSpotAssetCtx / user channels / unknown: no §13.1 table.
    skipped[0] += 1


def _handle_rest(
    raw: str,
    env: Mapping[str, Any],
    sink: _ParquetSink,
    spot_symbols: dict[str, str],
    skipped: list[int],
) -> None:
    """Route one ``hl-rest`` response; only ``metaAndAssetCtxs`` writes rows."""
    meta = env.get("meta")
    if not isinstance(meta, Mapping) or meta.get("status") != 200:
        skipped[0] += 1
        return
    req = meta.get("req")
    req_type = req.get("type") if isinstance(req, Mapping) else None
    try:
        parsed = orjson.loads(raw)
    except orjson.JSONDecodeError:
        skipped[0] += 1
        return

    if req_type == "spotMeta" and isinstance(parsed, Mapping):
        spot_symbols.update(spot_index_map(parsed))
        return
    if req_type == "spotMetaAndAssetCtxs" and isinstance(parsed, (list, tuple)) and parsed:
        inner = parsed[0]
        if isinstance(inner, Mapping):
            spot_symbols.update(spot_index_map(inner))
        return
    if req_type == "metaAndAssetCtxs" and isinstance(parsed, (list, tuple)) and len(parsed) >= 2:
        meta_obj, ctxs = parsed[0], parsed[1]
        universe = meta_obj.get("universe") if isinstance(meta_obj, Mapping) else None
        if not isinstance(universe, (list, tuple)) or not isinstance(ctxs, (list, tuple)):
            skipped[0] += 1
            return
        conn = str(env.get("conn", "hl-rest"))
        t_ns = env["t_ns"]
        day = _date_of_ns(t_ns).isoformat()
        for asset, ctx in zip(universe, ctxs):
            if not isinstance(asset, Mapping):
                continue
            market = recorder_market(str(asset.get("name", "")), spot_symbols=spot_symbols)
            sink.add(
                "ctx",
                day,
                "hl-rest",
                conn,
                "metaandassetcxs",
                (
                    t_ns,
                    market,
                    _f(ctx, "funding"),
                    _f(ctx, "openInterest"),
                    _f(ctx, "oraclePx"),
                    _f(ctx, "markPx"),
                    _f(ctx, "midPx"),
                    _f(ctx, "premium"),
                    _f(ctx, "dayNtlVlm"),
                    SOURCE,
                    FIDELITY_SNAPSHOT,
                ),
            )
        return

    # predictedFundings / fundingHistory / candleSnapshot / meta / perpDexs:
    # raw is recorded but the funding/candle/market tables belong to B-3.
    skipped[0] += 1


def _gap_rows(
    trackers: Iterable[_GapTracker],
    date_from: _dt.date,
    date_to: _dt.date,
) -> list[tuple[str, str, int, int, str, str, str]]:
    """Clip every closed gap to the UTC days it overlaps.

    A gap that spans midnight appears (clipped) in each day's partition, so a
    study that loads one day always sees the whole missing interval.
    """
    rows: list[tuple[str, str, int, int, str, str, str]] = []
    first = _day_start_ns(date_from)
    last = _day_start_ns(date_to) + DAY_NS
    for tracker in trackers:
        for start, end, reason in tracker.gaps:
            if end < first or start > last:
                continue
            day = max(start, first)
            while day <= min(end, last):
                day_start = _day_start_ns(_date_of_ns(day))
                day_end = day_start + DAY_NS
                clipped_start = max(start, day_start)
                clipped_end = min(end, day_end)
                if clipped_start < clipped_end or (start == end and clipped_start == clipped_end):
                    rows.append(
                        (
                            tracker.src,
                            tracker.conn,
                            clipped_start,
                            max(clipped_end, clipped_start),
                            reason,
                            SOURCE,
                            FIDELITY_WS,
                        )
                    )
                day = day_end
    return rows


# --------------------------------------------------------------------------
# Driver
# --------------------------------------------------------------------------


def normalize(
    root: str | os.PathLike[str],
    out_dir: str | os.PathLike[str],
    date_from: _dt.date,
    date_to: _dt.date,
    *,
    spot_symbols: Mapping[str, str] | None = None,
    sources: Sequence[str] = DEFAULT_SOURCES,
    flush_rows: int = _FLUSH_ROWS,
) -> NormalizeReport:
    """Normalize recorder segments under ``root`` into ``out_dir``.

    ``root`` is the recorder's network directory (``data/rec/{network}``). Each
    ``(table, day, src, conn, channel)`` stream is written as one part per
    bounded batch, named ``{source}.{src}.{conn}.{channel}.{NNNNN}.parquet`` in
    the day partition, so a reader loads a partition with
    ``{table}/date=DAY/{source}*.parquet``. Rows are streamed through a bounded
    buffer (``flush_rows``) and each batch is written atomically; peak RSS is
    bounded by the batch, not by the day. ``gaps`` partitions are synthesized
    for every day in the range. A re-run overwrites its own parts and removes a
    part that no longer exists; other-source (B-2) parts are untouched.
    """
    if date_from > date_to:
        raise NormalizeError(
            f"--from {date_from.isoformat()} is after --to {date_to.isoformat()}"
        )
    out_root = Path(out_dir)

    sink = _ParquetSink(out_root, flush_rows=flush_rows)
    trackers: dict[tuple[str, str], _GapTracker] = {}
    skipped = [0]
    envelope_count = 0
    spot_map: dict[str, str] = dict(spot_symbols or {})

    day_from = date_from.isoformat()
    day_to = date_to.isoformat()
    for src in sources:
        try:
            envelopes = iter_frames(root, src, day_from, day_to)
            for env in envelopes:
                envelope_count += 1
                conn = str(env.get("conn", src))
                tracker = trackers.get((src, conn))
                if tracker is None:
                    tracker = trackers[(src, conn)] = _GapTracker(src, conn)
                tracker.observe(env)
                kind = env.get("kind")
                raw = env.get("raw")
                if kind == "frame" and isinstance(raw, str):
                    _handle_frame(raw, env, src, conn, sink, spot_map, skipped)
                elif kind == "rest" and isinstance(raw, str):
                    _handle_rest(raw, env, sink, spot_map, skipped)
        except SegmentError:
            raise
        except ValueError as err:
            raise NormalizeError(str(err)) from err

    range_end_ns = _day_start_ns(date_to) + DAY_NS
    for tracker in trackers.values():
        tracker.finish(range_end_ns)

    gap_rows = _gap_rows(trackers.values(), date_from, date_to)
    for row in gap_rows:
        sink.add("gaps", _date_of_ns(row[2]).isoformat(), row[0], row[1], "", row)

    counts, produced = sink.finalize()
    for day in _iter_days(date_from, date_to):
        for table in MARKET_TABLES:
            directory = out_root / table / f"date={day.isoformat()}"
            _drop_stale_parts(directory, produced.get(f"{table}|{day.isoformat()}", set()))

    shutil.rmtree(out_root / ".normalize-tmp", ignore_errors=True)
    gap_files = sum(
        len(names) for key, names in produced.items() if key.startswith("gaps|")
    )
    return NormalizeReport(
        envelopes=envelope_count,
        skipped_envelopes=skipped[0],
        tables=counts,
        gap_files=gap_files,
        gap_rows=len(gap_rows),
    )


# --------------------------------------------------------------------------
# CLI
# --------------------------------------------------------------------------


def _parse_date(value: str) -> _dt.date:
    """Parse a ``YYYY-MM-DD`` UTC date, raising :class:`NormalizeError`."""
    try:
        return _dt.date.fromisoformat(value.strip())
    except ValueError as err:
        raise NormalizeError(f"invalid date `{value}` (use YYYY-MM-DD)") from err


def _load_spot_meta(path: str | os.PathLike[str]) -> dict[str, Any]:
    """Read a Hyperliquid ``spotMeta`` JSON response from ``path``."""
    try:
        data = orjson.loads(Path(path).read_bytes())
    except (OSError, orjson.JSONDecodeError) as exc:
        raise NormalizeError(f"cannot read spotMeta JSON `{path}`: {exc}") from exc
    if not isinstance(data, dict) or "universe" not in data or "tokens" not in data:
        raise NormalizeError(f"`{path}` is not a Hyperliquid spotMeta response")
    return data


def _parse_sources(raw: str) -> list[str]:
    """Split a comma-separated ``--sources`` list, order-preserving."""
    sources = [part.strip() for part in raw.split(",") if part.strip()]
    if not sources:
        raise NormalizeError("--sources is empty")
    unknown = [src for src in sources if src not in DEFAULT_SOURCES]
    if unknown:
        raise NormalizeError(
            f"unknown source(s) {', '.join(unknown)}; known: {', '.join(DEFAULT_SOURCES)}"
        )
    return sources


def _build_parser() -> argparse.ArgumentParser:
    """Build the ``hlr-normalize`` argument parser."""
    parser = argparse.ArgumentParser(
        prog="hlr-normalize",
        description=(
            "Normalize recorder segments into the SPEC-0008 §13.1 parquet "
            "tables (source=recorder)."
        ),
    )
    parser.add_argument(
        "--from", dest="date_from", required=True, metavar="YYYY-MM-DD", help="first UTC day"
    )
    parser.add_argument(
        "--to", dest="date_to", required=True, metavar="YYYY-MM-DD", help="last UTC day"
    )
    parser.add_argument(
        "--in",
        dest="in_dir",
        default="data/rec/mainnet",
        help="recorder network root (default: data/rec/mainnet)",
    )
    parser.add_argument(
        "--out",
        dest="out_dir",
        default="research/data/parquet",
        help="parquet output root (default: research/data/parquet)",
    )
    parser.add_argument(
        "--sources",
        default=",".join(DEFAULT_SOURCES),
        help=f"comma-separated sources (default: {','.join(DEFAULT_SOURCES)})",
    )
    parser.add_argument(
        "--spot-meta",
        default=None,
        metavar="PATH",
        help="optional HL spotMeta JSON used to resolve @N → BASE/QUOTE",
    )
    return parser


def _print_report(report: NormalizeReport) -> None:
    """Print the run summary to stdout."""
    for entry in sorted(report.tables, key=lambda e: (e.table, e.day)):
        print(f"{entry.table} {entry.day.isoformat()}: {entry.rows} rows -> {entry.path}")
    print(
        f"{report.envelopes} envelope(s), {report.skipped_envelopes} skipped; "
        f"bbo={report.rows_for('bbo')} book={report.rows_for('book')} "
        f"trades={report.rows_for('trades')} ctx={report.rows_for('ctx')} "
        f"gaps={report.rows_for('gaps')} rows"
    )


def main(argv: Sequence[str] | None = None) -> int:
    """CLI entry point for ``hlr-normalize``."""
    parser = _build_parser()
    args = parser.parse_args(argv)
    try:
        spot_symbols: dict[str, str] = {}
        if args.spot_meta is not None:
            spot_symbols = spot_index_map(_load_spot_meta(args.spot_meta))
        report = normalize(
            args.in_dir,
            args.out_dir,
            _parse_date(args.date_from),
            _parse_date(args.date_to),
            spot_symbols=spot_symbols,
            sources=_parse_sources(args.sources),
        )
    except (NormalizeError, SegmentError) as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2
    _print_report(report)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
