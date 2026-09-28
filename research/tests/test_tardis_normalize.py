"""Tests for :mod:`hlr.backfill.tardis_normalize` and :mod:`hlr.tables`.

No test touches the network: every fixture is a tiny gzipped CSV written under
``tmp_path``. The Tardis column layouts and microsecond timestamps mirror the
real 2026-09-01 BTC files verified while building B-2.
"""

from __future__ import annotations

import datetime as dt
import gzip
from pathlib import Path

import polars as pl
import pytest

from hlr import tables
from hlr.backfill.tardis_normalize import (
    DAY_NS,
    MarketMappingError,
    NormalizeError,
    normalize,
    spot_index_map,
    tardis_market,
)

_EXCHANGE = "hyperliquid"
_DAY = "2026-09-01"

_BOOK_TICKER_HEADER = (
    "exchange,symbol,timestamp,local_timestamp,ask_amount,ask_price,bid_price,bid_amount"
)
_QUOTES_HEADER = _BOOK_TICKER_HEADER
_TRADES_HEADER = "exchange,symbol,timestamp,local_timestamp,id,side,price,amount"
_DERIV_HEADER = (
    "exchange,symbol,timestamp,local_timestamp,funding_timestamp,funding_rate,"
    "predicted_funding_rate,open_interest,last_price,index_price,mark_price"
)
_BOOK5_HEADER = (
    "exchange,symbol,timestamp,local_timestamp,"
    "asks[0].price,asks[0].amount,bids[0].price,bids[0].amount,"
    "asks[1].price,asks[1].amount,bids[1].price,bids[1].amount,"
    "asks[2].price,asks[2].amount,bids[2].price,bids[2].amount,"
    "asks[3].price,asks[3].amount,bids[3].price,bids[3].amount,"
    "asks[4].price,asks[4].amount,bids[4].price,bids[4].amount"
)


def _write_csv(
    root: Path,
    data_type: str,
    symbol: str,
    header: str,
    rows: list[str],
    *,
    day: str = _DAY,
    exchange: str = _EXCHANGE,
) -> Path:
    """Write a tiny gzipped Tardis-shaped CSV and return its path."""
    path = root / exchange / data_type / day / f"{symbol}.csv.gz"
    path.parent.mkdir(parents=True, exist_ok=True)
    text = "\n".join([header, *rows]) + "\n"
    with gzip.open(path, "wb") as handle:
        handle.write(text.encode())
    return path


def _read(out: Path, table: str, day: str = _DAY) -> pl.DataFrame:
    """Load a whole ``(table, day)`` partition with one glob."""
    files = sorted((out / table / f"date={day}").glob("tardis-free*.parquet"))
    assert files, f"no {table} partition for {day}"
    return pl.read_parquet(files)


# --------------------------------------------------------------------------
# Per-type normalization and venue tags
# --------------------------------------------------------------------------


def test_book_ticker_maps_to_bbo(tmp_path: Path) -> None:
    raw = tmp_path / "raw"
    _write_csv(
        raw,
        "book_ticker",
        "BTC",
        _BOOK_TICKER_HEADER,
        ["hyperliquid,BTC,1788220800019000,1788220801849918,2.6,78575,78574,10.0"],
    )
    out = tmp_path / "out"

    report = normalize(raw, out, dt.date(2026, 9, 1), dt.date(2026, 9, 1))

    assert report.rows_for("bbo") == 1
    frame = _read(out, "bbo")
    assert frame.columns == tables.columns("bbo")
    row = frame.row(0, named=True)
    assert row == {
        "t_ns": 1788220801849918000,  # local_timestamp (µs) * 1000
        "ts_exch_ms": 1788220800019,  # timestamp (µs) // 1000
        "venue": "hl",
        "market": "BTC",
        "bid_px": 78574.0,
        "bid_sz": 10.0,
        "ask_px": 78575.0,
        "ask_sz": 2.6,
        "source": "tardis-free",
        "fidelity": "H1",
    }


def test_quotes_tag_hl_book(tmp_path: Path) -> None:
    raw = tmp_path / "raw"
    _write_csv(
        raw,
        "quotes",
        "BTC",
        _QUOTES_HEADER,
        ["hyperliquid,BTC,1788220799550000,1788220801052681,3.9,78575,78574,9.4"],
    )
    out = tmp_path / "out"

    normalize(raw, out, dt.date(2026, 9, 1), dt.date(2026, 9, 1))

    row = _read(out, "bbo").row(0, named=True)
    assert row["venue"] == "hl-book"
    assert row["market"] == "BTC"


def test_trades_fields_and_null_wallets(tmp_path: Path) -> None:
    raw = tmp_path / "raw"
    _write_csv(
        raw,
        "trades",
        "BTC",
        _TRADES_HEADER,
        ["hyperliquid,BTC,1788220800095000,1788220801941368,140510975437187,buy,78575,0.59978"],
    )
    out = tmp_path / "out"

    normalize(raw, out, dt.date(2026, 9, 1), dt.date(2026, 9, 1))

    frame = _read(out, "trades")
    assert frame.columns == tables.columns("trades")
    row = frame.row(0, named=True)
    assert row["venue"] == "hl"
    assert row["market"] == "BTC"
    assert row["side"] == "buy"
    assert row["px"] == 78575.0
    assert row["sz"] == 0.59978
    assert row["tid"] == "140510975437187"
    assert row["hash"] is None
    assert row["buyer"] is None
    assert row["seller"] is None


def test_derivative_ticker_maps_to_ctx(tmp_path: Path) -> None:
    raw = tmp_path / "raw"
    _write_csv(
        raw,
        "derivative_ticker",
        "BTC",
        _DERIV_HEADER,
        ["hyperliquid,BTC,1788220801676184,1788220801676184,,0.0000125,,37614.69,,78581,78573"],
    )
    out = tmp_path / "out"

    normalize(raw, out, dt.date(2026, 9, 1), dt.date(2026, 9, 1))

    frame = _read(out, "ctx")
    assert frame.columns == tables.columns("ctx")
    assert "venue" not in frame.columns
    row = frame.row(0, named=True)
    assert row["market"] == "BTC"
    assert row["funding"] == 1.25e-05
    assert row["open_interest"] == 37614.69
    assert row["oracle_px"] == 78581.0  # index_price is the oracle
    assert row["mark_px"] == 78573.0
    assert row["mid_px"] is None
    assert row["premium"] is None
    assert row["day_ntl_vlm"] is None


def test_book_snapshot_folds_wide_to_long(tmp_path: Path) -> None:
    raw = tmp_path / "raw"
    # Level 4 is empty (blank ask and bid), so it must not appear.
    _write_csv(
        raw,
        "book_snapshot_5",
        "BTC",
        _BOOK5_HEADER,
        [
            (
                "hyperliquid,BTC,1788220799550000,1788220801052681,"
                "78575,3.9,78574,9.4,78576,1.1,78573,0.0001,"
                "78577,0.5,78572,0.0001,78578,0.5,78571,0.0001,,,"
            )
        ],
    )
    out = tmp_path / "out"

    report = normalize(raw, out, dt.date(2026, 9, 1), dt.date(2026, 9, 1))

    # 4 present levels (0..3) x 2 sides; level 4 is dropped.
    assert report.rows_for("book") == 8
    frame = _read(out, "book")
    assert frame.columns == tables.columns("book")
    assert frame["venue"].unique().to_list() == ["hl-book"]
    assert frame["n"].null_count() == frame.height
    top = frame.filter((pl.col("side") == "ask") & (pl.col("level") == 0)).row(
        0, named=True
    )
    assert top["px"] == 78575.0
    assert top["sz"] == 3.9
    assert top["ts_exch_ms"] == 1788220799550
    levels = frame.filter(pl.col("side") == "bid")["level"].sort().to_list()
    assert levels == [0, 1, 2, 3]


def test_cex_venue_and_market_prefix(tmp_path: Path) -> None:
    raw = tmp_path / "raw"
    _write_csv(
        raw,
        "book_ticker",
        "BTCUSDT",
        _BOOK_TICKER_HEADER,
        ["binance-futures,BTCUSDT,1704067200002131,1704067200002131,2.7,42283.59,42283.58,9.0"],
        exchange="binance-futures",
    )
    _write_csv(
        raw,
        "trades",
        "ETHUSDT",
        _TRADES_HEADER,
        ["bybit,ETHUSDT,1704067200000000,1704067200001000,abc,sell,2200.5,1.5"],
        exchange="bybit",
    )
    out = tmp_path / "out"

    normalize(raw, out, dt.date(2026, 9, 1), dt.date(2026, 9, 1))

    bbo = _read(out, "bbo").row(0, named=True)
    assert bbo["venue"] == "binance-usdm"
    assert bbo["market"] == "binance-usdm:BTCUSDT"
    trades = _read(out, "trades").row(0, named=True)
    assert trades["venue"] == "bybit-linear"
    assert trades["market"] == "bybit-linear:ETHUSDT"


# --------------------------------------------------------------------------
# Symbol mapping
# --------------------------------------------------------------------------


@pytest.mark.parametrize(
    ("symbol", "exchange", "expected"),
    [
        ("BTC", "hyperliquid", "BTC"),
        ("kPEPE", "hyperliquid", "kPEPE"),
        ("XYZ-TSLA", "hyperliquid", "xyz:TSLA"),
        ("IO-SPX", "hyperliquid", "io:SPX"),
        ("BTCUSDT", "binance-futures", "binance-usdm:BTCUSDT"),
        ("BTCUSDT", "bybit", "bybit-linear:BTCUSDT"),
    ],
)
def test_tardis_market_mapping(symbol: str, exchange: str, expected: str) -> None:
    assert tardis_market(symbol, exchange) == expected


def test_spot_known_symbol_from_b1_fallback() -> None:
    # B-1's KNOWN_SPOT_SYMBOLS maps HYPE/USDC <-> @107; B-2 inverts it.
    assert tardis_market("@107", "hyperliquid") == "HYPE/USDC"


def test_spot_index_map_from_spotmeta() -> None:
    meta = {
        "tokens": [
            {"index": 0, "name": "USDC"},
            {"index": 1, "name": "PURR"},
            {"index": 150, "name": "HYPE"},
            {"index": 235, "name": "USDE"},
        ],
        "universe": [
            {"index": 107, "name": "@107", "tokens": [150, 0]},
            {"index": 108, "name": "@108", "tokens": [235, 0]},
            {"index": 0, "name": "PURR/USDC", "tokens": [1, 0]},
        ],
    }
    mapping = spot_index_map(meta)
    assert mapping["@107"] == "HYPE/USDC"
    assert mapping["@108"] == "USDE/USDC"
    assert mapping["@0"] == "PURR/USDC"
    assert tardis_market("@108", "hyperliquid", spot_symbols=mapping) == "USDE/USDC"


def test_unknown_spot_symbol_raises() -> None:
    with pytest.raises(MarketMappingError, match="spot"):
        tardis_market("@999", "hyperliquid")


def test_unknown_exchange_raises() -> None:
    with pytest.raises(MarketMappingError, match="unknown Tardis exchange"):
        tardis_market("BTC", "kraken")


def test_unmapped_spot_in_tree_fails_closed(tmp_path: Path) -> None:
    raw = tmp_path / "raw"
    _write_csv(
        raw,
        "book_ticker",
        "@999",
        _BOOK_TICKER_HEADER,
        ["hyperliquid,@999,1788220800019000,1788220801849918,2.6,1,1,10.0"],
    )
    with pytest.raises(MarketMappingError, match="spot"):
        normalize(raw, tmp_path / "out", dt.date(2026, 9, 1), dt.date(2026, 9, 1))


def test_load_spot_meta_roundtrip(tmp_path: Path) -> None:
    from hlr.backfill.tardis_normalize import load_spot_meta

    path = tmp_path / "spotMeta.json"
    path.write_text(
        '{"tokens":[{"index":0,"name":"USDC"},{"index":150,"name":"HYPE"}],'
        '"universe":[{"index":107,"name":"@107","tokens":[150,0]}]}'
    )
    assert load_spot_meta(path)["universe"][0]["index"] == 107
    with pytest.raises(NormalizeError, match="spotMeta"):
        load_spot_meta(tmp_path / "missing.json")


# --------------------------------------------------------------------------
# CLI
# --------------------------------------------------------------------------


def test_cli_main_normalizes_and_returns_zero(tmp_path: Path) -> None:
    from hlr.backfill.tardis_normalize import main

    raw = tmp_path / "raw"
    out = tmp_path / "out"
    _write_csv(
        raw,
        "trades",
        "BTC",
        _TRADES_HEADER,
        ["hyperliquid,BTC,1788220800095000,1788220801941368,1,buy,78575,0.1"],
    )

    code = main(
        [
            "--from",
            "2026-09-01",
            "--to",
            "2026-09-01",
            "--in",
            str(raw),
            "--out",
            str(out),
        ]
    )

    assert code == 0
    assert _read(out, "trades").height == 1


def test_cli_main_reads_expect_streams(tmp_path: Path) -> None:
    from hlr.backfill.tardis_normalize import main

    raw = tmp_path / "raw"
    out = tmp_path / "out"
    _write_csv(
        raw,
        "trades",
        "BTC",
        _TRADES_HEADER,
        ["hyperliquid,BTC,1788220800095000,1788220801941368,1,buy,78575,0.1"],
    )
    streams = tmp_path / "streams.txt"
    streams.write_text("hyperliquid/book_ticker/BTC\n")

    code = main(
        [
            "--from",
            "2026-09-01",
            "--to",
            "2026-09-01",
            "--in",
            str(raw),
            "--out",
            str(out),
            "--expect-streams",
            str(streams),
        ]
    )

    assert code == 0
    assert _read(out, "gaps")["conn"].to_list() == ["hyperliquid/book_ticker/BTC"]


def test_cli_main_rejects_a_reversed_range(tmp_path: Path) -> None:
    from hlr.backfill.tardis_normalize import main

    code = main(
        [
            "--from",
            "2026-09-02",
            "--to",
            "2026-09-01",
            "--in",
            str(tmp_path / "raw"),
            "--out",
            str(tmp_path / "out"),
        ]
    )

    assert code == 2


# --------------------------------------------------------------------------
# Gaps
# --------------------------------------------------------------------------


def test_gaps_cover_every_missing_day(tmp_path: Path) -> None:
    raw = tmp_path / "raw"
    _write_csv(
        raw,
        "book_ticker",
        "BTC",
        _BOOK_TICKER_HEADER,
        ["hyperliquid,BTC,1788220800019000,1788220801849918,2.6,78575,78574,10.0"],
    )
    out = tmp_path / "out"

    report = normalize(raw, out, dt.date(2026, 9, 1), dt.date(2026, 9, 3))

    # One partition per requested day; day 1 is covered, days 2 and 3 are not.
    assert report.gap_files == 3
    assert report.gap_rows == 2
    assert _read(out, "gaps", "2026-09-01").height == 0
    for day in ("2026-09-02", "2026-09-03"):
        gap = _read(out, "gaps", day)
        assert gap.height == 1
        row = gap.row(0, named=True)
        assert row["src"] == "tardis-free"
        assert row["conn"] == "hyperliquid/book_ticker/BTC"
        assert row["reason"] == "unsampled"
        start = (dt.date.fromisoformat(day) - dt.date(1970, 1, 1)).days * DAY_NS
        assert row["start_ns"] == start
        assert row["end_ns"] == start + DAY_NS


def test_expected_streams_flag_a_fully_absent_source(tmp_path: Path) -> None:
    raw = tmp_path / "raw"
    _write_csv(
        raw,
        "book_ticker",
        "BTC",
        _BOOK_TICKER_HEADER,
        ["hyperliquid,BTC,1788220800019000,1788220801849918,2.6,78575,78574,10.0"],
    )
    out = tmp_path / "out"

    # Without the expected stream, ETH is unknown and never flagged.
    normalize(raw, out, dt.date(2026, 9, 1), dt.date(2026, 9, 1))
    conns = _read(out, "gaps")["conn"].to_list()
    assert conns == []

    # With it, the same day gains a gap row for ETH.
    report = normalize(
        raw,
        out,
        dt.date(2026, 9, 1),
        dt.date(2026, 9, 1),
        expected_streams=[("hyperliquid", "book_ticker", "ETH")],
    )
    assert report.gap_rows == 1
    assert _read(out, "gaps")["conn"].to_list() == ["hyperliquid/book_ticker/ETH"]


def test_expected_streams_validation_is_fail_closed(tmp_path: Path) -> None:
    with pytest.raises(NormalizeError, match="unknown exchange"):
        normalize(
            tmp_path / "raw",
            tmp_path / "out",
            dt.date(2026, 9, 1),
            dt.date(2026, 9, 1),
            expected_streams=[("kraken", "book_ticker", "BTC")],
        )
    with pytest.raises(NormalizeError, match="unsupported data type"):
        normalize(
            tmp_path / "raw",
            tmp_path / "out",
            dt.date(2026, 9, 1),
            dt.date(2026, 9, 1),
            expected_streams=[("hyperliquid", "options_chain", "BTC")],
        )


def test_parse_expected_streams(tmp_path: Path) -> None:
    from hlr.backfill.tardis_normalize import parse_expected_streams

    path = tmp_path / "streams.txt"
    path.write_text(
        "# a comment\n\nhyperliquid/book_ticker/BTC\nbybit/trades/ETHUSDT\n"
    )
    assert parse_expected_streams(path) == [
        ("hyperliquid", "book_ticker", "BTC"),
        ("bybit", "trades", "ETHUSDT"),
    ]
    path.write_text("hyperliquid/book_ticker\n")
    with pytest.raises(NormalizeError, match="exchange/data_type/symbol"):
        parse_expected_streams(path)


def test_new_data_clears_a_stale_gap(tmp_path: Path) -> None:
    raw = tmp_path / "raw"
    out = tmp_path / "out"
    header = _BOOK_TICKER_HEADER
    row = "hyperliquid,BTC,1788220800019000,1788220801849918,2.6,78575,78574,10.0"

    # First run: day 2 has no file, so it is a gap.
    _write_csv(raw, "book_ticker", "BTC", header, [row], day="2026-09-01")
    normalize(raw, out, dt.date(2026, 9, 1), dt.date(2026, 9, 2))
    assert _read(out, "gaps", "2026-09-02").height == 1

    # Add day 2's file and re-run: the stale gap must be overwritten empty.
    _write_csv(raw, "book_ticker", "BTC", header, [row], day="2026-09-02")
    report = normalize(raw, out, dt.date(2026, 9, 1), dt.date(2026, 9, 2))
    assert report.gap_rows == 0
    assert _read(out, "gaps", "2026-09-02").height == 0


# --------------------------------------------------------------------------
# Partitioning, idempotency, skips
# --------------------------------------------------------------------------


def test_multiple_symbols_one_day_one_glob(tmp_path: Path) -> None:
    raw = tmp_path / "raw"
    _write_csv(
        raw,
        "book_ticker",
        "BTC",
        _BOOK_TICKER_HEADER,
        ["hyperliquid,BTC,1788220800019000,1788220801849918,2.6,78575,78574,10.0"],
    )
    _write_csv(
        raw,
        "book_ticker",
        "XYZ-TSLA",
        _BOOK_TICKER_HEADER,
        ["hyperliquid,XYZ-TSLA,1788220800019000,1788220801849918,2.6,300,299,10.0"],
    )
    out = tmp_path / "out"

    normalize(raw, out, dt.date(2026, 9, 1), dt.date(2026, 9, 1))

    # One part per input stream, loaded together by the day glob.
    parts = sorted(
        path.name for path in (out / "bbo" / "date=2026-09-01").glob("tardis-free*.parquet")
    )
    assert parts == ["tardis-free.hyperliquid.book_ticker.BTC.parquet",
                     "tardis-free.hyperliquid.book_ticker.XYZ-TSLA.parquet"]
    assert set(_read(out, "bbo")["market"]) == {"BTC", "xyz:TSLA"}


def test_rerun_drops_a_stale_stream_part(tmp_path: Path) -> None:
    raw = tmp_path / "raw"
    _write_csv(
        raw,
        "book_ticker",
        "BTC",
        _BOOK_TICKER_HEADER,
        ["hyperliquid,BTC,1788220800019000,1788220801849918,2.6,78575,78574,10.0"],
    )
    eth = _write_csv(
        raw,
        "book_ticker",
        "XYZ-TSLA",
        _BOOK_TICKER_HEADER,
        ["hyperliquid,XYZ-TSLA,1788220800019000,1788220801849918,2.6,300,299,10.0"],
    )
    out = tmp_path / "out"

    normalize(raw, out, dt.date(2026, 9, 1), dt.date(2026, 9, 1))
    assert set(_read(out, "bbo")["market"]) == {"BTC", "xyz:TSLA"}

    eth.unlink()
    normalize(raw, out, dt.date(2026, 9, 1), dt.date(2026, 9, 1))
    assert set(_read(out, "bbo")["market"]) == {"BTC"}
    parts = list((out / "bbo" / "date=2026-09-01").glob("tardis-free*.parquet"))
    assert len(parts) == 1


def test_rerun_is_idempotent_per_partition(tmp_path: Path) -> None:
    raw = tmp_path / "raw"
    _write_csv(
        raw,
        "book_ticker",
        "BTC",
        _BOOK_TICKER_HEADER,
        ["hyperliquid,BTC,1788220800019000,1788220801849918,2.6,78575,78574,10.0"],
    )
    _write_csv(
        raw,
        "trades",
        "BTC",
        _TRADES_HEADER,
        ["hyperliquid,BTC,1788220800095000,1788220801941368,1,buy,78575,0.1"],
    )
    out = tmp_path / "out"

    normalize(raw, out, dt.date(2026, 9, 1), dt.date(2026, 9, 1))
    first = {table: _read(out, table) for table in ("bbo", "trades")}
    normalize(raw, out, dt.date(2026, 9, 1), dt.date(2026, 9, 1))

    for table, frame in first.items():
        assert _read(out, table).equals(frame)
        files = list((out / table).rglob("*.parquet"))
        assert len(files) == 1
    assert list(out.rglob("*.tmp")) == []


def test_unsupported_type_is_skipped_not_crashing(tmp_path: Path) -> None:
    raw = tmp_path / "raw"
    _write_csv(
        raw,
        "incremental_book_L2",
        "BTC",
        "exchange,symbol,timestamp,local_timestamp,is_snapshot,side,price,amount",
        ["hyperliquid,BTC,1,2,true,bid,1,1"],
    )
    out = tmp_path / "out"

    report = normalize(raw, out, dt.date(2026, 9, 1), dt.date(2026, 9, 1))

    assert report.input_files == 0
    assert len(report.skipped) == 1


def test_missing_root_yields_no_files(tmp_path: Path) -> None:
    report = normalize(
        tmp_path / "does-not-exist",
        tmp_path / "out",
        dt.date(2026, 9, 1),
        dt.date(2026, 9, 1),
    )
    assert report.input_files == 0


def test_reversed_range_raises(tmp_path: Path) -> None:
    with pytest.raises(NormalizeError, match="is after"):
        normalize(
            tmp_path,
            tmp_path / "out",
            dt.date(2026, 9, 2),
            dt.date(2026, 9, 1),
        )
