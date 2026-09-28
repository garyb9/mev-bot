"""Tardis → §13.1 normalizer (SPEC-0008 §13.1, §13.11, task B-2).

Maps the CSVs B-1 downloads (``research/data/raw/tardis/…``) into the shared
§13.1 parquet tables under ``research/data/parquet/{table}/date=YYYY-MM-DD/``,
one file per ``(table, date, source)`` so recorder- and Tardis-derived
partitions can coexist:

* ``book_ticker`` → ``bbo`` (venue ``hl`` / ``binance-usdm`` / ``bybit-linear``)
* ``quotes`` → ``bbo`` tagged ``venue="hl-book"``
* ``trades`` → ``trades`` (``buyer``/``seller``/``hash`` null)
* ``derivative_ticker`` → ``ctx`` (``oracle_px = index_price``; the other
  HL-empty fields are null)
* ``book_snapshot_5`` / ``book_snapshot_25`` → long-form ``book``

Tardis timestamps are microseconds since epoch: ``t_ns = local_timestamp *
1000`` and ``ts_exch_ms = timestamp // 1000`` (verified against the real file
headers, 2026-09-01).

Every partition is written with the ``source`` column (``tardis-free``, the
SPEC-0008 §13.11 tag) and the ``fidelity`` column (``H1``). Rows are read with
polars' lazy CSV scanner, reshaped by vectorized expressions, and sunk to
parquet with the streaming engine: no row ever becomes a Python object and no
per-row Python loop runs. The wide book snapshots are reshaped by projecting one
narrow frame per ``(side, level)`` and concatenating, which the optimizer folds
onto a single cached scan — all streaming-supported, so peak memory stays flat
as a file or a multi-symbol day grows. Writes go to a ``.tmp`` file that is
atomically renamed into place, so re-running a ``(table, date, source)``
overwrites only that file.

``gaps`` rows are synthesized for every ``(stream, day)`` in the requested range
that has no downloaded file (the Tardis free lane only has one day per month).
Only streams with at least one file in the range are known by default; pass
``--expect-streams`` (lines ``exchange/data_type/symbol``) to also flag a source
that is entirely absent.

Research only: never imported by, or deployed with, the trading bot. It reads
no keys and no network.
"""

from __future__ import annotations

import argparse
import datetime as _dt
import os
import sys
from collections import defaultdict
from collections.abc import Iterable, Iterator, Mapping, Sequence
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import orjson
import polars as pl

from hlr import tables
from hlr.backfill.tardis import (
    HIP3_DEXES,
    KNOWN_SPOT_SYMBOLS,
    TardisError,
    parse_boundary,
)

__all__ = [
    "DATA_TYPE_TABLE",
    "DAY_NS",
    "EXCHANGES",
    "FIDELITY",
    "SOURCE",
    "MarketMappingError",
    "NormalizeError",
    "NormalizeReport",
    "TableCount",
    "TardisFile",
    "build_frame",
    "discover_files",
    "load_spot_meta",
    "main",
    "normalize",
    "normalize_bbo",
    "normalize_book",
    "normalize_ctx",
    "normalize_trades",
    "parse_expected_streams",
    "scan_tardis_csv",
    "spot_index_map",
    "synthesize_gaps",
    "tardis_market",
    "tardis_venue",
]

#: SPEC-0008 §13.11 source tag for rows imported through B-1's free Tardis lane.
SOURCE = "tardis-free"

#: Every Tardis data type B-2 handles is update-level with microsecond stamps.
FIDELITY = tables.FIDELITY_H1

#: Exchanges present in the raw tree and their §5.4/§13.1 venue tags.
EXCHANGES: frozenset[str] = frozenset({"hyperliquid", "binance-futures", "bybit"})

_VENUE_BY_EXCHANGE: dict[str, str] = {
    "binance-futures": "binance-usdm",
    "bybit": "bybit-linear",
}

#: Data type → §13.1 table. Types not listed here are skipped (and reported),
#: not treated as errors: B-1 may cache ``incremental_book_L2`` for later work.
DATA_TYPE_TABLE: dict[str, str] = {
    "book_ticker": "bbo",
    "quotes": "bbo",
    "trades": "trades",
    "derivative_ticker": "ctx",
    "book_snapshot_5": "book",
    "book_snapshot_25": "book",
}

#: Number of levels in the Tardis book-snapshot datasets.
_BOOK_LEVELS: dict[str, int] = {"book_snapshot_5": 5, "book_snapshot_25": 25}

#: Prefix of the compressed CSV datasets.
_SUFFIX = ".csv.gz"

#: One UTC day in nanoseconds.
DAY_NS = 86_400 * 1_000_000_000

#: Parquet row-group size; keeps memory bounded without tiny groups.
_ROW_GROUP = 250_000


class NormalizeError(Exception):
    """Base class for every error this module raises."""


class MarketMappingError(NormalizeError):
    """A Tardis symbol cannot be mapped to a SPEC-0008 market name."""


@dataclass(frozen=True)
class TardisFile:
    """One raw Tardis CSV to normalize."""

    exchange: str
    data_type: str
    day: _dt.date
    symbol: str
    path: Path


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

    input_files: int
    skipped: list[Path] = field(default_factory=list)
    tables: list[TableCount] = field(default_factory=list)
    gap_files: int = 0
    gap_rows: int = 0

    def rows_for(self, table: str) -> int:
        """Total rows written for ``table`` across every date partition."""
        return sum(entry.rows for entry in self.tables if entry.table == table)


# --------------------------------------------------------------------------
# Symbol and venue mapping
# --------------------------------------------------------------------------


def _default_spot_symbols() -> dict[str, str]:
    """Invert B-1's known ``BASE/QUOTE → @N`` table to ``@N → BASE/QUOTE``."""
    return {tardis: market for market, tardis in KNOWN_SPOT_SYMBOLS.items()}


def spot_index_map(spot_meta: Mapping[str, Any] | None) -> dict[str, str]:
    """Build a ``@N → BASE/QUOTE`` map from a Hyperliquid ``spotMeta`` response.

    ``spotMeta`` names most pairs by their raw index (``@107``); the pair is
    recovered from the universe entry's base/quote token indices through the
    ``tokens`` array. B-1's :data:`KNOWN_SPOT_SYMBOLS` is the fallback, so the
    map is usable even without a ``spotMeta`` snapshot.
    """
    mapping = _default_spot_symbols()
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
        if index is None or not isinstance(pair, Sequence) or len(pair) < 2:
            continue
        base = tokens.get(pair[0])
        quote = tokens.get(pair[1])
        if base is None or quote is None:
            continue
        name = entry.get("name")
        if isinstance(name, str) and name and not name.startswith("@"):
            mapping[f"@{index}"] = name
        else:
            mapping[f"@{index}"] = f"{base}/{quote}"
    return mapping


def tardis_market(
    symbol: str,
    exchange: str,
    *,
    spot_symbols: Mapping[str, str] | None = None,
) -> str:
    """Map a Tardis symbol to the §13.1 market name (the inverse of B-1).

    * HL perps are unchanged (``BTC``, ``kPEPE``).
    * HIP-3 ``DEX-COIN`` becomes ``dex:COIN`` (``XYZ-TSLA`` → ``xyz:TSLA``).
    * HL spot ``@N`` is resolved to ``BASE/QUOTE`` through ``spot_symbols``
      (B-1's table plus any ``spotMeta``-derived map); an unknown index raises
      :class:`MarketMappingError` rather than leaking an ``@N`` into the data.
    * CEX symbols gain their venue prefix (``BTCUSDT`` →
      ``binance-usdm:BTCUSDT``).
    """
    if exchange not in EXCHANGES:
        raise MarketMappingError(f"unknown Tardis exchange `{exchange}`")
    name = symbol.strip()
    if not name:
        raise MarketMappingError("empty Tardis symbol")

    if exchange != "hyperliquid":
        return f"{_VENUE_BY_EXCHANGE[exchange]}:{name}"

    if name.startswith("@"):
        table = _default_spot_symbols()
        if spot_symbols:
            table.update(spot_symbols)
        mapped = table.get(name)
        if mapped is None:
            raise MarketMappingError(
                f"no BASE/QUOTE known for Tardis spot `{name}`; pass --spot-meta "
                "built from the HL spotMeta response"
            )
        return mapped

    if "-" in name:
        dex, _, coin = name.partition("-")
        if dex.lower() in HIP3_DEXES and coin:
            return f"{dex.lower()}:{coin}"

    return name


def tardis_venue(exchange: str, data_type: str) -> str:
    """Return the §13.1 ``venue`` tag for a Tardis exchange/data-type pair."""
    if exchange == "hyperliquid":
        return "hl-book" if data_type in _BOOK_LEVELS or data_type == "quotes" else "hl"
    return _VENUE_BY_EXCHANGE[exchange]


# --------------------------------------------------------------------------
# Lazy CSV readers and per-table normalizers
# --------------------------------------------------------------------------


def scan_tardis_csv(path: str | os.PathLike[str], data_type: str) -> pl.LazyFrame:
    """Lazily scan one Tardis CSV, pinning the numeric/timestamp column types.

    Pinning the schema (rather than inferring it from the first rows) keeps the
    scan streaming and stops an all-empty leading column from being typed as a
    string.
    """
    overrides: dict[str, pl.DataType] = {
        "timestamp": pl.Int64,
        "local_timestamp": pl.Int64,
    }
    if data_type in ("book_ticker", "quotes"):
        overrides |= {
            "ask_amount": pl.Float64,
            "ask_price": pl.Float64,
            "bid_price": pl.Float64,
            "bid_amount": pl.Float64,
        }
    elif data_type == "trades":
        overrides |= {"id": pl.String, "price": pl.Float64, "amount": pl.Float64}
    elif data_type == "derivative_ticker":
        overrides |= {
            "funding_timestamp": pl.Int64,
            "funding_rate": pl.Float64,
            "predicted_funding_rate": pl.Float64,
            "open_interest": pl.Float64,
            "last_price": pl.Float64,
            "index_price": pl.Float64,
            "mark_price": pl.Float64,
        }
    elif data_type in _BOOK_LEVELS:
        for level in range(_BOOK_LEVELS[data_type]):
            for prefix in ("asks", "bids"):
                overrides[f"{prefix}[{level}].price"] = pl.Float64
                overrides[f"{prefix}[{level}].amount"] = pl.Float64
    return pl.scan_csv(
        path, schema_overrides=overrides, missing_columns="insert"
    )


def _t_ns() -> pl.Expr:
    """Local receive time in ns (Tardis ``local_timestamp`` is µs since epoch)."""
    return (pl.col("local_timestamp") * 1000).cast(pl.Int64)


def _ts_exch_ms() -> pl.Expr:
    """Exchange time in ms (Tardis ``timestamp`` is µs since epoch)."""
    return (pl.col("timestamp") // 1000).cast(pl.Int64)


def _null(dtype: pl.DataType) -> pl.Expr:
    """A typed null literal, so an empty §13.1 column still has its dtype."""
    return pl.lit(None, dtype=dtype)


def normalize_bbo(
    lf: pl.LazyFrame, *, venue: str, market: str, source: str, fidelity: str
) -> pl.LazyFrame:
    """Normalize a Tardis ``book_ticker``/``quotes`` frame into ``bbo``."""
    return lf.select(
        _t_ns().alias("t_ns"),
        _ts_exch_ms().alias("ts_exch_ms"),
        pl.lit(venue).alias("venue"),
        pl.lit(market).alias("market"),
        pl.col("bid_price").cast(pl.Float64).alias("bid_px"),
        pl.col("bid_amount").cast(pl.Float64).alias("bid_sz"),
        pl.col("ask_price").cast(pl.Float64).alias("ask_px"),
        pl.col("ask_amount").cast(pl.Float64).alias("ask_sz"),
        pl.lit(source).alias("source"),
        pl.lit(fidelity).alias("fidelity"),
    ).select(tables.columns("bbo"))


def normalize_trades(
    lf: pl.LazyFrame, *, venue: str, market: str, source: str, fidelity: str
) -> pl.LazyFrame:
    """Normalize a Tardis ``trades`` frame; the HL wallet fields are null."""
    return lf.select(
        _t_ns().alias("t_ns"),
        _ts_exch_ms().alias("ts_exch_ms"),
        pl.lit(venue).alias("venue"),
        pl.lit(market).alias("market"),
        pl.col("side").cast(pl.String).alias("side"),
        pl.col("price").cast(pl.Float64).alias("px"),
        pl.col("amount").cast(pl.Float64).alias("sz"),
        pl.col("id").cast(pl.String).alias("tid"),
        _null(pl.String).alias("hash"),
        _null(pl.String).alias("buyer"),
        _null(pl.String).alias("seller"),
        pl.lit(source).alias("source"),
        pl.lit(fidelity).alias("fidelity"),
    ).select(tables.columns("trades"))


def normalize_ctx(
    lf: pl.LazyFrame, *, market: str, source: str, fidelity: str
) -> pl.LazyFrame:
    """Normalize a Tardis ``derivative_ticker`` frame into ``ctx``.

    ``index_price`` is the oracle; ``mid_px``, ``premium`` and ``day_ntl_vlm``
    have no Tardis source and are null (the HL feed carries no ``last_price``).
    """
    return lf.select(
        _t_ns().alias("t_ns"),
        pl.lit(market).alias("market"),
        pl.col("funding_rate").cast(pl.Float64).alias("funding"),
        pl.col("open_interest").cast(pl.Float64).alias("open_interest"),
        pl.col("index_price").cast(pl.Float64).alias("oracle_px"),
        pl.col("mark_price").cast(pl.Float64).alias("mark_px"),
        _null(pl.Float64).alias("mid_px"),
        _null(pl.Float64).alias("premium"),
        _null(pl.Float64).alias("day_ntl_vlm"),
        pl.lit(source).alias("source"),
        pl.lit(fidelity).alias("fidelity"),
    ).select(tables.columns("ctx"))


def normalize_book(
    lf: pl.LazyFrame,
    *,
    venue: str,
    market: str,
    source: str,
    fidelity: str,
    levels: int,
) -> pl.LazyFrame:
    """Normalize a wide Tardis book snapshot into the long-form ``book`` table.

    Each ``(side, level)`` pair is projected as its own narrow lazy frame, and
    the frames are concatenated. Polars' optimizer folds the projections onto a
    single cached CSV scan, and ``select``/``concat``/``filter`` are all
    streaming, so ``sink_parquet(engine="streaming")`` keeps memory flat as the
    file grows (unlike the previous struct/``explode`` plan, which the streaming
    engine silently ran in memory). No per-row Python work runs. Empty levels
    are dropped; Tardis has no order count, so ``n`` is null.
    """
    meta = [
        _t_ns().alias("t_ns"),
        _ts_exch_ms().alias("ts_exch_ms"),
        pl.lit(venue).alias("venue"),
        pl.lit(market).alias("market"),
        pl.lit(source).alias("source"),
        pl.lit(fidelity).alias("fidelity"),
    ]
    projections: list[pl.LazyFrame] = []
    for level in range(levels):
        for side, prefix in (("ask", "asks"), ("bid", "bids")):
            projections.append(
                lf.select(
                    *meta,
                    pl.lit(side, dtype=pl.String).alias("side"),
                    pl.lit(level, dtype=pl.Int32).alias("level"),
                    pl.col(f"{prefix}[{level}].price").cast(pl.Float64).alias("px"),
                    pl.col(f"{prefix}[{level}].amount").cast(pl.Float64).alias("sz"),
                )
            )
    return (
        pl.concat(projections, how="vertical", rechunk=False)
        .filter(pl.col("px").is_not_null() & pl.col("sz").is_not_null())
        .with_columns(_null(pl.Int64).alias("n"))
        .select(tables.columns("book"))
    )


def build_frame(
    file: TardisFile,
    *,
    source: str,
    fidelity: str,
    spot_symbols: Mapping[str, str] | None,
) -> pl.LazyFrame:
    """Build the normalized lazy frame for one raw file, in its table's schema."""
    table = DATA_TYPE_TABLE[file.data_type]
    market = tardis_market(file.symbol, file.exchange, spot_symbols=spot_symbols)
    venue = tardis_venue(file.exchange, file.data_type)
    lf = scan_tardis_csv(file.path, file.data_type)
    if table == "bbo":
        return normalize_bbo(
            lf, venue=venue, market=market, source=source, fidelity=fidelity
        )
    if table == "trades":
        return normalize_trades(
            lf, venue=venue, market=market, source=source, fidelity=fidelity
        )
    if table == "ctx":
        return normalize_ctx(lf, market=market, source=source, fidelity=fidelity)
    return normalize_book(
        lf,
        venue=venue,
        market=market,
        source=source,
        fidelity=fidelity,
        levels=_BOOK_LEVELS[file.data_type],
    )


# --------------------------------------------------------------------------
# Discovery, writing, gaps
# --------------------------------------------------------------------------


def discover_files(
    root: str | os.PathLike[str], date_from: _dt.date, date_to: _dt.date
) -> tuple[list[TardisFile], list[Path]]:
    """Find the raw Tardis CSVs for ``[date_from, date_to]`` under ``root``.

    Returns the selected files and the paths skipped because their exchange or
    data type is unsupported (or their directory is not a date). A missing
    ``root`` yields nothing.
    """
    base = Path(root)
    selected: list[TardisFile] = []
    skipped: list[Path] = []
    if not base.is_dir():
        return selected, skipped
    for path in sorted(base.glob(f"*/*/*/*{_SUFFIX}")):
        rel = path.relative_to(base).parts
        if len(rel) != 4:
            skipped.append(path)
            continue
        exchange, data_type, day_text, filename = rel
        if exchange not in EXCHANGES or data_type not in DATA_TYPE_TABLE:
            skipped.append(path)
            continue
        try:
            day = _dt.date.fromisoformat(day_text)
        except ValueError:
            skipped.append(path)
            continue
        if not (date_from <= day <= date_to):
            continue
        selected.append(
            TardisFile(
                exchange=exchange,
                data_type=data_type,
                day=day,
                symbol=filename[: -len(_SUFFIX)],
                path=path,
            )
        )
    return selected, skipped


def _atomic_sink(lf: pl.LazyFrame, path: Path) -> None:
    """Sink ``lf`` to ``path`` via a ``.tmp`` file and an atomic rename."""
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(path.name + ".tmp")
    try:
        lf.sink_parquet(
            tmp,
            compression="zstd",
            mkdir=True,
            row_group_size=_ROW_GROUP,
            engine="streaming",
        )
        os.replace(tmp, path)
    except BaseException:
        tmp.unlink(missing_ok=True)
        raise


def _part_name(source: str, file: TardisFile) -> str:
    """Part filename for one input stream, safe to glob with ``{source}*``."""
    symbol = file.symbol.replace("/", "_").replace(os.sep, "_").replace("\\", "_")
    return f"{source}.{file.exchange}.{file.data_type}.{symbol}.parquet"


def _drop_stale_parts(directory: Path, source: str, keep: set[str]) -> None:
    """Remove leftover ``{source}*`` parts of a rewritten partition."""
    for existing in directory.glob(f"{source}*.parquet"):
        if existing.name not in keep:
            existing.unlink()


def _parquet_rows(path: Path) -> int:
    """Row count from a parquet file's metadata (no full read)."""
    return pl.scan_parquet(path).select(pl.len()).collect().item()


def _day_start_ns(day: _dt.date) -> int:
    """Unix nanoseconds at 00:00:00 UTC on ``day`` (integer-exact)."""
    return (day - _dt.date(1970, 1, 1)).days * DAY_NS


def _iter_days(first: _dt.date, last: _dt.date) -> Iterator[_dt.date]:
    """Yield every UTC day in the inclusive range ``[first, last]``."""
    day = first
    while day <= last:
        yield day
        day += _dt.timedelta(days=1)


def _validate_stream(stream: tuple[str, str, str], origin: str) -> None:
    """Reject an expected stream with an unknown exchange or data type."""
    exchange, data_type, _symbol = stream
    if exchange not in EXCHANGES:
        raise NormalizeError(f"{origin}: unknown exchange `{exchange}`")
    if data_type not in DATA_TYPE_TABLE:
        raise NormalizeError(f"{origin}: unsupported data type `{data_type}`")


def synthesize_gaps(
    files: Sequence[TardisFile],
    out_dir: str | os.PathLike[str],
    date_from: _dt.date,
    date_to: _dt.date,
    *,
    expected_streams: Iterable[tuple[str, str, str]] | None = None,
    source: str = SOURCE,
    fidelity: str = FIDELITY,
) -> tuple[int, int]:
    """Write one ``gaps`` partition per requested day, over every known stream.

    A *stream* is a ``(exchange, data_type, symbol)``. Streams seen anywhere in
    the selected raw files are always covered; ``expected_streams`` adds streams
    that should exist but have **no file at all** in the requested range, so a
    completely missing source is flagged rather than silently absent (each
    expected stream is validated against the known exchanges/data types,
    fail-closed). For each day in ``[date_from, date_to]`` any stream with no
    file that day becomes one gap row covering the whole UTC day, under
    ``gaps/date=DAY/{source}.parquet``. Days with full coverage still get a
    (zero-row) partition, so a stale gap file is overwritten rather than left
    behind. Returns ``(files_written, rows_written)``.
    """
    present = {(f.exchange, f.data_type, f.symbol, f.day) for f in files}
    streams = {(f.exchange, f.data_type, f.symbol) for f in files}
    for stream in expected_streams or ():
        _validate_stream(stream, "expected stream")
        streams.add(stream)
    root = Path(out_dir)
    files_written = rows_written = 0
    for day in _iter_days(date_from, date_to):
        conns: list[str] = []
        starts: list[int] = []
        for exchange, data_type, symbol in sorted(streams):
            if (exchange, data_type, symbol, day) in present:
                continue
            conns.append(f"{exchange}/{data_type}/{symbol}")
            starts.append(_day_start_ns(day))
        if conns:
            count = len(conns)
            frame = pl.DataFrame(
                {
                    "src": [source] * count,
                    "conn": conns,
                    "start_ns": starts,
                    "end_ns": [start + DAY_NS for start in starts],
                    "reason": ["unsampled"] * count,
                    "source": [source] * count,
                    "fidelity": [fidelity] * count,
                },
                schema=tables.schema("gaps"),
            )
        else:
            frame = tables.empty_frame("gaps")
        path = root / "gaps" / f"date={day.isoformat()}" / f"{source}.parquet"
        _atomic_sink(frame.lazy(), path)
        files_written += 1
        rows_written += frame.height
    return files_written, rows_written


def normalize(
    root: str | os.PathLike[str],
    out_dir: str | os.PathLike[str],
    date_from: _dt.date,
    date_to: _dt.date,
    *,
    spot_symbols: Mapping[str, str] | None = None,
    expected_streams: Iterable[tuple[str, str, str]] | None = None,
    source: str = SOURCE,
    fidelity: str = FIDELITY,
) -> NormalizeReport:
    """Normalize every raw Tardis file under ``root`` into ``out_dir``.

    Each input file (one exchange/data-type/symbol stream) becomes its own part
    file in the day partition, named
    ``{source}.{exchange}.{data_type}.{symbol}.parquet``. Reading a whole day is
    one glob: ``{table}/date=DAY/{source}*.parquet``. Per-stream parts keep peak
    memory to one stream (a day-level ``concat`` would decompress every symbol
    into memory at once, since polars' gzip reader buffers each file), and each
    part is overwritten atomically by name. When a ``(table, day)`` is rewritten,
    any part for a stream no longer present in the raw tree is removed, so a
    re-run matches the current raw set. ``gaps`` partitions are then synthesized
    over the streams seen in the range plus any ``expected_streams`` (see
    :func:`synthesize_gaps`). A symbol that cannot be mapped raises
    :class:`MarketMappingError` (fail closed).
    """
    if date_from > date_to:
        raise NormalizeError(
            f"--from {date_from.isoformat()} is after --to {date_to.isoformat()}"
        )
    files, skipped = discover_files(root, date_from, date_to)

    out_root = Path(out_dir)
    parts: dict[tuple[str, _dt.date], list[Path]] = defaultdict(list)
    rows: dict[tuple[str, _dt.date], int] = defaultdict(int)
    for raw in files:
        table = DATA_TYPE_TABLE[raw.data_type]
        frame = build_frame(
            raw, source=source, fidelity=fidelity, spot_symbols=spot_symbols
        )
        path = out_root / table / f"date={raw.day.isoformat()}" / _part_name(source, raw)
        _atomic_sink(frame, path)
        parts[(table, raw.day)].append(path)
        rows[(table, raw.day)] += _parquet_rows(path)

    counts: list[TableCount] = []
    for (table, day), written in sorted(parts.items()):
        _drop_stale_parts(written[0].parent, source, {path.name for path in written})
        counts.append(TableCount(table, day, rows[(table, day)], written[0].parent))

    gap_files, gap_rows = synthesize_gaps(
        files,
        out_root,
        date_from,
        date_to,
        expected_streams=expected_streams,
        source=source,
        fidelity=fidelity,
    )
    return NormalizeReport(
        input_files=len(files),
        skipped=skipped,
        tables=counts,
        gap_files=gap_files,
        gap_rows=gap_rows,
    )


# --------------------------------------------------------------------------
# CLI
# --------------------------------------------------------------------------


def parse_expected_streams(path: str | os.PathLike[str]) -> list[tuple[str, str, str]]:
    """Read ``exchange/data_type/symbol`` lines into expected-stream tuples.

    Blank lines and ``#`` comments are ignored. A malformed line raises
    :class:`NormalizeError`; the exchange/data-type pair is validated later by
    :func:`synthesize_gaps`.
    """
    try:
        text = Path(path).read_text(encoding="utf-8")
    except OSError as exc:
        raise NormalizeError(f"cannot read expected-streams file `{path}`: {exc}") from exc
    streams: list[tuple[str, str, str]] = []
    for lineno, line in enumerate(text.splitlines(), start=1):
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        parts = stripped.split("/")
        if len(parts) != 3 or not all(parts):
            raise NormalizeError(
                f"`{path}` line {lineno}: expected exchange/data_type/symbol"
            )
        streams.append((parts[0], parts[1], parts[2]))
    return streams


def load_spot_meta(path: str | os.PathLike[str]) -> dict[str, Any]:
    """Read a Hyperliquid ``spotMeta`` JSON response from ``path``."""
    try:
        data = orjson.loads(Path(path).read_bytes())
    except (OSError, orjson.JSONDecodeError) as exc:
        raise NormalizeError(f"cannot read spotMeta JSON `{path}`: {exc}") from exc
    if not isinstance(data, dict) or "universe" not in data or "tokens" not in data:
        raise NormalizeError(f"`{path}` is not a Hyperliquid spotMeta response")
    return data


def _build_parser() -> argparse.ArgumentParser:
    """Build the ``hlr-tardis-normalize`` argument parser."""
    parser = argparse.ArgumentParser(
        prog="hlr-tardis-normalize",
        description=(
            "Normalize B-1's raw Tardis CSVs into the SPEC-0008 §13.1 parquet "
            "tables (HIST-PRELIM, source=tardis-free)."
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
        default="research/data/raw/tardis",
        help="raw Tardis root (default: research/data/raw/tardis)",
    )
    parser.add_argument(
        "--out",
        dest="out_dir",
        default="research/data/parquet",
        help="parquet output root (default: research/data/parquet)",
    )
    parser.add_argument(
        "--spot-meta",
        default=None,
        metavar="PATH",
        help="optional HL spotMeta JSON used to resolve @N → BASE/QUOTE",
    )
    parser.add_argument(
        "--expect-streams",
        default=None,
        metavar="PATH",
        help=(
            "optional file of exchange/data_type/symbol lines to flag as gaps "
            "even when they have no file in the range"
        ),
    )
    return parser


def _print_report(report: NormalizeReport) -> None:
    """Print the run summary to stdout."""
    for path in report.skipped:
        print(f"skipped (unsupported): {path}", file=sys.stderr)
    for entry in sorted(report.tables, key=lambda e: (e.table, e.day)):
        print(f"{entry.table} {entry.day.isoformat()}: {entry.rows} rows -> {entry.path}")
    print(
        f"{report.input_files} file(s): "
        f"bbo={report.rows_for('bbo')} trades={report.rows_for('trades')} "
        f"ctx={report.rows_for('ctx')} book={report.rows_for('book')} rows; "
        f"{report.gap_files} gap partition(s), {report.gap_rows} gap rows"
    )


def main(argv: Sequence[str] | None = None) -> int:
    """CLI entry point for ``hlr-tardis-normalize``."""
    parser = _build_parser()
    args = parser.parse_args(argv)
    try:
        date_from = parse_boundary(args.date_from)
        date_to = parse_boundary(args.date_to)
        spot_symbols = None
        if args.spot_meta is not None:
            spot_symbols = spot_index_map(load_spot_meta(args.spot_meta))
        expected_streams = None
        if args.expect_streams is not None:
            expected_streams = parse_expected_streams(args.expect_streams)
        report = normalize(
            args.in_dir,
            args.out_dir,
            date_from,
            date_to,
            spot_symbols=spot_symbols,
            expected_streams=expected_streams,
        )
    except (NormalizeError, TardisError) as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2
    _print_report(report)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
