"""The §13.1 normalized-table schemas (SPEC-0008), shared by every normalizer.

The recorder-segment normalizer (P-2) and the historical-backfill normalizer
(B-2) both write the same logical tables, so their column names and polars dtypes
live here once. Every physical partition also carries two provenance columns:

* ``source`` — where the rows came from (SPEC-0008 §13.11), e.g. ``tardis-free``
  for the Tardis backfill (B-2) or ``recorder`` for forward segments (P-2); and
* ``fidelity`` — the §13.11 fidelity class (``H1`` tick/update-level, ``H2``
  periodic snapshots, ``H3`` bars/funding).

The §13.1 columns are exactly as specified; only ``source`` and ``fidelity`` are
appended. All prices and sizes are ``float64`` (research only, SPEC-0008 §13.1);
timestamps are int64 nanoseconds plus ``ts_exch_ms`` where the venue provides an
exchange timestamp. ``bars`` is the one table whose §13.1 columns already include
``source(mid/trade/candle)``; that column doubles as its provenance value
(``candle`` for the B-3 REST candle backfill), so it is not duplicated.

Physical layout. A normalizer writes
``{root}/{table}/date=YYYY-MM-DD/{source}*.parquet``: one part file per input
stream (B-2 names them ``{source}.{exchange}.{data_type}.{symbol}.parquet``) so
peak memory stays at one stream instead of a whole day. A reader loads a full
day with a single glob, ``{root}/{table}/date=DAY/{source}*.parquet`` (which
also matches a single-file ``{source}.parquet`` partition).
"""

from __future__ import annotations

import polars as pl

__all__ = [
    "BASE_SCHEMAS",
    "FIDELITY_COLUMN",
    "FIDELITY_H1",
    "FIDELITY_H2",
    "FIDELITY_H3",
    "SOURCE_COLUMN",
    "columns",
    "empty_frame",
    "schema",
]

#: Provenance column holding the data source id (SPEC-0008 §13.11).
SOURCE_COLUMN = "source"

#: Provenance column holding the §13.11 fidelity class.
FIDELITY_COLUMN = "fidelity"

#: §13.11 fidelity classes. ``H1`` = tick/update-level with ms stamps; ``H2`` =
#: periodic snapshots (seconds); ``H3`` = bars / funding.
FIDELITY_H1 = "H1"
FIDELITY_H2 = "H2"
FIDELITY_H3 = "H3"

#: The §13.1 columns and dtypes for each table this toolkit writes, in order.
#: ``source`` and ``fidelity`` are added by :func:`schema`, not listed here.
BASE_SCHEMAS: dict[str, dict[str, pl.DataType]] = {
    "bbo": {
        "t_ns": pl.Int64,
        "ts_exch_ms": pl.Int64,
        "venue": pl.String,
        "market": pl.String,
        "bid_px": pl.Float64,
        "bid_sz": pl.Float64,
        "ask_px": pl.Float64,
        "ask_sz": pl.Float64,
    },
    "book": {
        "t_ns": pl.Int64,
        "ts_exch_ms": pl.Int64,
        "venue": pl.String,
        "market": pl.String,
        "side": pl.String,
        "level": pl.Int32,
        "px": pl.Float64,
        "sz": pl.Float64,
        "n": pl.Int64,
    },
    "trades": {
        "t_ns": pl.Int64,
        "ts_exch_ms": pl.Int64,
        "venue": pl.String,
        "market": pl.String,
        "side": pl.String,
        "px": pl.Float64,
        "sz": pl.Float64,
        "tid": pl.String,
        "hash": pl.String,
        "buyer": pl.String,
        "seller": pl.String,
    },
    "ctx": {
        "t_ns": pl.Int64,
        "market": pl.String,
        "funding": pl.Float64,
        "open_interest": pl.Float64,
        "oracle_px": pl.Float64,
        "mark_px": pl.Float64,
        "mid_px": pl.Float64,
        "premium": pl.Float64,
        "day_ntl_vlm": pl.Float64,
    },
    "gaps": {
        "src": pl.String,
        "conn": pl.String,
        "start_ns": pl.Int64,
        "end_ns": pl.Int64,
        "reason": pl.String,
    },
    "funding_hist": {
        "time_ms": pl.Int64,
        "market": pl.String,
        "funding_rate": pl.Float64,
        "premium": pl.Float64,
    },
    "markets": {
        "snapshot_t_ns": pl.Int64,
        "market": pl.String,
        "kind": pl.String,
        "dex": pl.String,
        "base": pl.String,
        "quote": pl.String,
        "asset_id": pl.Int64,
        "sz_decimals": pl.Int64,
        "max_leverage": pl.Int64,
    },
    # ``bars`` already has the §13.1 ``source(mid/trade/candle)`` column; for this
    # table that column *is* the provenance marker, so ``schema`` does not append
    # a second ``source`` (see below). ``fidelity`` is still appended.
    "bars": {
        "t_open_ms": pl.Int64,
        "interval": pl.String,
        "venue": pl.String,
        "market": pl.String,
        "open": pl.Float64,
        "high": pl.Float64,
        "low": pl.Float64,
        "close": pl.Float64,
        "volume": pl.Float64,
        "n_trades": pl.Int64,
        "source": pl.String,
    },
}


def columns(table: str) -> list[str]:
    """Return the §13.1 columns of ``table`` plus the provenance columns."""
    return list(schema(table))


def schema(table: str) -> dict[str, pl.DataType]:
    """Return the full polars schema of ``table``, in column order.

    The §13.1 columns come first, then ``source`` and ``fidelity``. The one
    exception is ``bars``: its §13.1 ``source(mid/trade/candle)`` column already
    carries the provenance value (§13.11), so only ``fidelity`` is appended and
    ``source`` appears once.
    """
    base = dict(BASE_SCHEMAS[table])
    if SOURCE_COLUMN not in base:
        base[SOURCE_COLUMN] = pl.String
    base[FIDELITY_COLUMN] = pl.String
    return base


def empty_frame(table: str) -> pl.DataFrame:
    """Return a zero-row :class:`polars.DataFrame` with ``table``'s schema.

    Used to overwrite a stale partition (for example an old ``gaps`` file) with
    an explicit "nothing here" file rather than deleting it, keeping writes
    atomic and idempotent.
    """
    return pl.DataFrame(schema=schema(table))
