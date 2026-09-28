"""Tests for :mod:`hlr.normalize` (SPEC-0008 P-2).

Fixtures are recorder-style segments built in ``tmp_path`` with ``zstandard``
and ``orjson`` (the P-1/R-1 writer format); no binary fixture is committed and
nothing is written under ``data/`` or ``research/data/``.
"""

from __future__ import annotations

import datetime as dt
from pathlib import Path

import orjson
import polars as pl
import pytest
import zstandard

from hlr import tables
from hlr.normalize import (
    DAY_NS,
    MarketMappingError,
    NormalizeError,
    main,
    normalize,
    recorder_market,
    spot_index_map,
)

_DAY = "2026-09-01"
_DAY2 = "2026-09-02"
_T0 = (dt.date(2026, 9, 1) - dt.date(1970, 1, 1)).days * DAY_NS


def _t(hour: int = 0, second: int = 0) -> int:
    """A nanosecond timestamp on 2026-09-01 UTC at ``hour``:``second``."""
    return _T0 + hour * 3_600 * 1_000_000_000 + second * 1_000_000_000


def envelope(
    seq: int,
    t_ns: int,
    *,
    src: str = "hl-ws",
    conn: str = "hl-ws-01",
    kind: str = "frame",
    raw: str | None = None,
    meta: dict | None = None,
) -> dict:
    """One envelope dict in the SPEC-0008 §5.1 shape."""
    env: dict = {
        "v": 1,
        "src": src,
        "conn": conn,
        "seq": seq,
        "t_ns": t_ns,
        "mono_ns": seq,
        "kind": kind,
    }
    if raw is not None:
        env["raw"] = raw
    if meta is not None:
        env["meta"] = meta
    return env


def frame_env(seq: int, t_ns: int, frame: dict, **kwargs) -> dict:
    """An envelope whose ``raw`` is the JSON text of ``frame``."""
    return envelope(seq, t_ns, raw=orjson.dumps(frame).decode(), **kwargs)


def rest_env(seq: int, t_ns: int, body: object, req: dict, **kwargs) -> dict:
    """An envelope for one ``hl-rest`` response."""
    return envelope(
        seq,
        t_ns,
        src="hl-rest",
        conn="hl-rest",
        kind="rest",
        raw=orjson.dumps(body).decode(),
        meta={"req": req, "status": 200, "latency_us": 1},
        **kwargs,
    )


def _lines(envelopes: list[dict]) -> bytes:
    return b"".join(orjson.dumps(env) + b"\n" for env in envelopes)


def _compress(raw: bytes) -> bytes:
    return zstandard.ZstdCompressor(level=3).compress(raw)


def write_segment(
    root: Path,
    src: str,
    day: str,
    hour: str,
    conn: str,
    envelopes: list[dict],
) -> Path:
    """Write a finished ``.jsonl.zst`` segment under the §6 layout."""
    path = root / src / day / hour / f"{conn}-{envelopes[0]['t_ns']}.jsonl.zst"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(_compress(_lines(envelopes)))
    return path


def write_crashed_segment(
    root: Path,
    src: str,
    day: str,
    hour: str,
    conn: str,
    frames: list[list[dict]],
) -> Path:
    """Write a ``.crashed`` segment: complete frames then a truncated tail."""
    path = root / src / day / hour / f"{conn}-{frames[0][0]['t_ns']}.jsonl.zst.crashed"
    path.parent.mkdir(parents=True, exist_ok=True)
    compressed = [_compress(_lines(frame)) for frame in frames]
    assert len(compressed) >= 2
    head = b"".join(compressed[:-1])
    tail = compressed[-1]
    path.write_bytes(head + tail[: len(tail) // 2])
    return path


def read(out: Path, table: str, day: str = _DAY) -> pl.DataFrame:
    """Load a whole ``(table, day)`` partition with one glob."""
    directory = out / table / f"date={day}"
    files = sorted(directory.glob("recorder*.parquet")) if directory.is_dir() else []
    assert files, f"no recorder {table} partition for {day}"
    return pl.read_parquet(files)


# --------------------------------------------------------------------------
# WS channels
# --------------------------------------------------------------------------


def test_bbo_channel_writes_hl_bbo(tmp_path: Path) -> None:
    root, out = tmp_path / "rec", tmp_path / "out"
    frame = {
        "channel": "bbo",
        "data": {
            "coin": "BTC",
            "time": 1_788_220_800_019,
            "bbo": [{"px": "78574", "sz": "10", "n": 2}, {"px": "78575", "sz": "2.6", "n": 3}],
        },
    }
    write_segment(root, "hl-ws", _DAY, "00", "hl-ws-01", [frame_env(1, _t(), frame)])

    report = normalize(root, out, dt.date(2026, 9, 1), dt.date(2026, 9, 1), sources=("hl-ws",))

    assert report.rows_for("bbo") == 1
    frame_out = read(out, "bbo")
    assert frame_out.columns == tables.columns("bbo")
    row = frame_out.row(0, named=True)
    assert row["t_ns"] == _t()
    assert row["ts_exch_ms"] == 1_788_220_800_019
    assert row["venue"] == "hl"
    assert row["market"] == "BTC"
    assert (row["bid_px"], row["bid_sz"], row["ask_px"], row["ask_sz"]) == (
        78574.0,
        10.0,
        78575.0,
        2.6,
    )
    assert row["source"] == "recorder"
    assert row["fidelity"] == "H1"


def test_bbo_missing_side_is_null(tmp_path: Path) -> None:
    root, out = tmp_path / "rec", tmp_path / "out"
    frame = {
        "channel": "bbo",
        "data": {"coin": "ETH", "time": 5, "bbo": [None, {"px": "2", "sz": "1", "n": 1}]},
    }
    write_segment(root, "hl-ws", _DAY, "00", "hl-ws-01", [frame_env(1, _t(), frame)])

    normalize(root, out, dt.date(2026, 9, 1), dt.date(2026, 9, 1), sources=("hl-ws",))

    row = read(out, "bbo").row(0, named=True)
    assert row["bid_px"] is None and row["bid_sz"] is None
    assert row["ask_px"] == 2.0


def test_l2book_writes_book_and_level0_bbo(tmp_path: Path) -> None:
    root, out = tmp_path / "rec", tmp_path / "out"
    frame = {
        "channel": "l2Book",
        "data": {
            "coin": "BTC",
            "time": 1_788_220_800_019,
            "levels": [
                [{"px": "100", "sz": "1.5", "n": 2}, {"px": "99", "sz": "3", "n": 1}],
                [{"px": "101", "sz": "2", "n": 4}],
            ],
        },
    }
    write_segment(root, "hl-ws", _DAY, "00", "hl-ws-01", [frame_env(1, _t(), frame)])

    report = normalize(root, out, dt.date(2026, 9, 1), dt.date(2026, 9, 1), sources=("hl-ws",))

    assert report.rows_for("book") == 3  # 2 bids + 1 ask
    book = read(out, "book")
    assert book.columns == tables.columns("book")
    assert book["venue"].unique().to_list() == ["hl-book"]
    assert book["fidelity"].unique().to_list() == ["H2"]
    top_bid = book.filter((pl.col("side") == "bid") & (pl.col("level") == 0)).row(0, named=True)
    assert (top_bid["px"], top_bid["sz"], top_bid["n"]) == (100.0, 1.5, 2)
    assert top_bid["ts_exch_ms"] == 1_788_220_800_019

    bbo = read(out, "bbo").row(0, named=True)
    assert bbo["venue"] == "hl-book"
    assert bbo["market"] == "BTC"
    assert (bbo["bid_px"], bbo["ask_px"], bbo["ask_sz"]) == (100.0, 101.0, 2.0)


def test_trades_batch_fields_and_users(tmp_path: Path) -> None:
    root, out = tmp_path / "rec", tmp_path / "out"
    frame = {
        "channel": "trades",
        "data": [
            {
                "coin": "BTC",
                "side": "B",
                "px": "78575",
                "sz": "0.5",
                "time": 1_788_220_800_100,
                "tid": 42,
                "users": ["0xbuyer", "0xseller"],
            },
            {"coin": "BTC", "side": "A", "px": "78574", "sz": "1.0", "time": 1_788_220_800_101},
        ],
    }
    write_segment(root, "hl-ws", _DAY, "00", "hl-ws-01", [frame_env(1, _t(), frame)])

    report = normalize(root, out, dt.date(2026, 9, 1), dt.date(2026, 9, 1), sources=("hl-ws",))

    assert report.rows_for("trades") == 2
    trades = read(out, "trades")
    assert trades.columns == tables.columns("trades")
    first = trades.sort("ts_exch_ms").row(0, named=True)
    assert first["side"] == "buy"
    assert first["px"] == 78575.0 and first["sz"] == 0.5
    assert first["tid"] == "42"
    assert first["hash"] is None
    assert first["buyer"] == "0xbuyer" and first["seller"] == "0xseller"
    assert first["t_ns"] == _t()
    second = trades.sort("ts_exch_ms").row(1, named=True)
    assert second["side"] == "sell"
    assert second["buyer"] is None and second["seller"] is None


def test_active_asset_ctx_writes_ctx(tmp_path: Path) -> None:
    root, out = tmp_path / "rec", tmp_path / "out"
    frame = {
        "channel": "activeAssetCtx",
        "data": {
            "coin": "BTC",
            "ctx": {
                "funding": "0.0000125",
                "openInterest": "37614.69",
                "prevDayPx": "78500",
                "dayNtlVlm": "1000000",
                "premium": "0.0001",
                "oraclePx": "78581",
                "markPx": "78573",
                "midPx": "78575",
            },
        },
    }
    write_segment(root, "hl-ws", _DAY, "00", "hl-ws-01", [frame_env(1, _t(), frame)])

    normalize(root, out, dt.date(2026, 9, 1), dt.date(2026, 9, 1), sources=("hl-ws",))

    row = read(out, "ctx").row(0, named=True)
    assert row["market"] == "BTC"
    assert row["funding"] == 1.25e-05
    assert row["open_interest"] == 37614.69
    assert row["oracle_px"] == 78581.0
    assert row["mark_px"] == 78573.0
    assert row["mid_px"] == 78575.0
    assert row["premium"] == 0.0001
    assert row["day_ntl_vlm"] == 1000000.0
    assert row["fidelity"] == "H2"


def test_hip3_coin_is_passthrough(tmp_path: Path) -> None:
    root, out = tmp_path / "rec", tmp_path / "out"
    frame = {
        "channel": "bbo",
        "data": {"coin": "xyz:TSLA", "time": 1, "bbo": [{"px": "300", "sz": "1", "n": 1}, None]},
    }
    write_segment(root, "hl-ws", _DAY, "00", "hl-ws-01", [frame_env(1, _t(), frame)])

    normalize(root, out, dt.date(2026, 9, 1), dt.date(2026, 9, 1), sources=("hl-ws",))

    assert read(out, "bbo").row(0, named=True)["market"] == "xyz:TSLA"


def test_allmids_and_unknown_channels_are_skipped(tmp_path: Path) -> None:
    root, out = tmp_path / "rec", tmp_path / "out"
    mids = {"channel": "allMids", "data": {"mids": {"BTC": "1"}}}
    write_segment(root, "hl-ws", _DAY, "00", "hl-ws-01", [frame_env(1, _t(), mids)])

    report = normalize(root, out, dt.date(2026, 9, 1), dt.date(2026, 9, 1), sources=("hl-ws",))

    assert report.rows_for("bbo") == 0
    assert report.skipped_envelopes == 1


# --------------------------------------------------------------------------
# REST
# --------------------------------------------------------------------------


def test_rest_meta_and_asset_ctxs_writes_ctx(tmp_path: Path) -> None:
    root, out = tmp_path / "rec", tmp_path / "out"
    body = [
        {"universe": [{"name": "BTC"}, {"name": "ETH"}]},
        [
            {"funding": "0.01", "openInterest": "1", "oraclePx": "2", "markPx": "3"},
            {"funding": "0.02", "openInterest": "4", "oraclePx": "5", "markPx": "6"},
        ],
    ]
    write_segment(
        root,
        "hl-rest",
        _DAY,
        "00",
        "hl-rest",
        [rest_env(1, _t(), body, {"type": "metaAndAssetCtxs"})],
    )

    report = normalize(root, out, dt.date(2026, 9, 1), dt.date(2026, 9, 1), sources=("hl-rest",))

    assert report.rows_for("ctx") == 2
    ctx = read(out, "ctx").sort("market")
    assert ctx["market"].to_list() == ["BTC", "ETH"]
    assert ctx["funding"].to_list() == [0.01, 0.02]
    row = ctx.row(0, named=True)
    assert row["fidelity"] == "H2"


# --------------------------------------------------------------------------
# Symbol mapping
# --------------------------------------------------------------------------


def test_spot_meta_resolves_at_index_to_base_quote(tmp_path: Path) -> None:
    root, out = tmp_path / "rec", tmp_path / "out"
    spot_meta = {
        "tokens": [{"index": 0, "name": "USDC"}, {"index": 150, "name": "HYPE"}],
        "universe": [{"index": 107, "name": "@107", "tokens": [150, 0]}],
    }
    bbo = {
        "channel": "bbo",
        "data": {"coin": "@107", "time": 1, "bbo": [{"px": "40", "sz": "1", "n": 1}, None]},
    }
    write_segment(
        root,
        "hl-rest",
        _DAY,
        "00",
        "hl-rest",
        [rest_env(1, _t(), spot_meta, {"type": "spotMeta"})],
    )
    write_segment(root, "hl-ws", _DAY, "00", "hl-ws-01", [frame_env(1, _t(), bbo)])

    normalize(root, out, dt.date(2026, 9, 1), dt.date(2026, 9, 1))

    assert read(out, "bbo").row(0, named=True)["market"] == "HYPE/USDC"


def test_unknown_at_index_fails_closed(tmp_path: Path) -> None:
    root, out = tmp_path / "rec", tmp_path / "out"
    bbo = {
        "channel": "bbo",
        "data": {"coin": "@999", "time": 1, "bbo": [{"px": "1", "sz": "1", "n": 1}, None]},
    }
    write_segment(root, "hl-ws", _DAY, "00", "hl-ws-01", [frame_env(1, _t(), bbo)])

    with pytest.raises(MarketMappingError, match="spot"):
        normalize(root, out, dt.date(2026, 9, 1), dt.date(2026, 9, 1), sources=("hl-ws",))


def test_spot_index_map_and_recorder_market_helpers() -> None:
    meta = {
        "tokens": [{"index": 0, "name": "USDC"}, {"index": 1, "name": "PURR"}],
        "universe": [{"index": 0, "name": "PURR/USDC", "tokens": [1, 0]}],
    }
    mapping = spot_index_map(meta)
    assert mapping["@0"] == "PURR/USDC"
    assert mapping["@107"] == "HYPE/USDC"  # fallback
    assert recorder_market("@0", spot_symbols=mapping) == "PURR/USDC"
    assert recorder_market("BTC") == "BTC"
    assert recorder_market("xyz:TSLA") == "xyz:TSLA"
    assert recorder_market("PURR/USDC") == "PURR/USDC"


# --------------------------------------------------------------------------
# Gaps
# --------------------------------------------------------------------------


def _bbo_frame(coin: str = "BTC") -> dict:
    return {
        "channel": "bbo",
        "data": {"coin": coin, "time": 1, "bbo": [{"px": "1", "sz": "1", "n": 1}, None]},
    }


def test_explicit_gap_start_end(tmp_path: Path) -> None:
    root, out = tmp_path / "rec", tmp_path / "out"
    envelopes = [
        frame_env(1, _t(second=1), _bbo_frame()),
        envelope(2, _t(second=2), kind="gap_start", meta={"reason": "close", "detail": "x"}),
        envelope(3, _t(second=5), kind="gap_end", meta={"gap_ms": 3000}),
        frame_env(4, _t(second=6), _bbo_frame()),
    ]
    write_segment(root, "hl-ws", _DAY, "00", "hl-ws-01", envelopes)

    report = normalize(root, out, dt.date(2026, 9, 1), dt.date(2026, 9, 1), sources=("hl-ws",))

    assert report.gap_rows == 1
    gap = read(out, "gaps").row(0, named=True)
    assert gap["src"] == "hl-ws"
    assert gap["conn"] == "hl-ws-01"
    assert gap["start_ns"] == _t(second=2)
    assert gap["end_ns"] == _t(second=5)
    assert gap["reason"] == "close"
    assert gap["source"] == "recorder"


def test_seq_hole_becomes_a_gap(tmp_path: Path) -> None:
    root, out = tmp_path / "rec", tmp_path / "out"
    envelopes = [
        frame_env(1, _t(second=1), _bbo_frame()),
        frame_env(3, _t(second=4), _bbo_frame()),
    ]
    write_segment(root, "hl-ws", _DAY, "00", "hl-ws-01", envelopes)

    normalize(root, out, dt.date(2026, 9, 1), dt.date(2026, 9, 1), sources=("hl-ws",))

    gap = read(out, "gaps").row(0, named=True)
    assert gap["reason"] == "seq"
    assert gap["start_ns"] == _t(second=1)
    assert gap["end_ns"] == _t(second=4)


def test_unpaired_gap_start_is_closed_by_next_data(tmp_path: Path) -> None:
    root, out = tmp_path / "rec", tmp_path / "out"
    envelopes = [
        frame_env(1, _t(second=1), _bbo_frame()),
        envelope(2, _t(second=2), kind="gap_start", meta={"reason": "drop", "detail": "full"}),
        frame_env(3, _t(second=4), _bbo_frame()),
    ]
    write_segment(root, "hl-ws", _DAY, "00", "hl-ws-01", envelopes)

    normalize(root, out, dt.date(2026, 9, 1), dt.date(2026, 9, 1), sources=("hl-ws",))

    gap = read(out, "gaps").row(0, named=True)
    assert gap["reason"] == "drop"
    assert gap["start_ns"] == _t(second=2)
    assert gap["end_ns"] == _t(second=4)


def test_crashed_segment_tail_becomes_a_gap_to_range_end(tmp_path: Path) -> None:
    root, out = tmp_path / "rec", tmp_path / "out"
    complete = [
        envelope(0, _t(second=0), kind="segment_open", meta={"host": "h"}),
        frame_env(1, _t(second=1), _bbo_frame()),
        frame_env(2, _t(second=2), _bbo_frame()),
    ]
    write_crashed_segment(
        root, "hl-ws", _DAY, "00", "hl-ws-01", [complete, [frame_env(3, _t(second=3), _bbo_frame())]]
    )

    normalize(root, out, dt.date(2026, 9, 1), dt.date(2026, 9, 1), sources=("hl-ws",))

    # The crashed tail starts at the last surviving record and runs to day end.
    gap = read(out, "gaps").row(0, named=True)
    assert gap["reason"] == "crash"
    assert gap["start_ns"] == _t(second=2)
    assert gap["end_ns"] == _t(second=0) + DAY_NS


def test_crash_then_resume_is_closed_at_next_segment_data(tmp_path: Path) -> None:
    root, out = tmp_path / "rec", tmp_path / "out"
    first = [envelope(0, _t(second=0), kind="segment_open", meta={}), frame_env(1, _t(second=1), _bbo_frame())]
    write_crashed_segment(root, "hl-ws", _DAY, "00", "hl-ws-01", [first, [frame_env(9, _t(second=9), _bbo_frame())]])
    second = [
        envelope(0, _t(hour=1), kind="segment_open", meta={}),
        envelope(1, _t(hour=1, second=1), kind="segment_close", meta={"records": 1}),
        frame_env(2, _t(hour=1, second=2), _bbo_frame()),
    ]
    write_segment(root, "hl-ws", _DAY, "01", "hl-ws-01", second)

    normalize(root, out, dt.date(2026, 9, 1), dt.date(2026, 9, 1), sources=("hl-ws",))

    gap = read(out, "gaps").row(0, named=True)
    assert gap["reason"] == "crash"
    assert gap["start_ns"] == _t(second=1)
    assert gap["end_ns"] == _t(hour=1, second=2)


# --------------------------------------------------------------------------
# Partitions, idempotency, streaming
# --------------------------------------------------------------------------


def test_rows_partition_by_t_ns_day(tmp_path: Path) -> None:
    root, out = tmp_path / "rec", tmp_path / "out"
    day2 = _T0 + DAY_NS
    write_segment(
        root,
        "hl-ws",
        _DAY,
        "00",
        "hl-ws-01",
        [frame_env(1, _t(second=1), _bbo_frame()), frame_env(2, day2 + 1, _bbo_frame("ETH"))],
    )

    normalize(root, out, dt.date(2026, 9, 1), dt.date(2026, 9, 2), sources=("hl-ws",))

    assert read(out, "bbo", _DAY).height == 1
    assert read(out, "bbo", _DAY2).row(0, named=True)["market"] == "ETH"


def test_rerun_is_idempotent_and_leaves_no_tmp(tmp_path: Path) -> None:
    root, out = tmp_path / "rec", tmp_path / "out"
    write_segment(
        root, "hl-ws", _DAY, "00", "hl-ws-01", [frame_env(1, _t(second=1), _bbo_frame())]
    )
    write_segment(
        root,
        "hl-rest",
        _DAY,
        "00",
        "hl-rest",
        [rest_env(1, _t(), [{"universe": [{"name": "BTC"}]}, [{"funding": "1"}]], {"type": "metaAndAssetCtxs"})],
    )

    normalize(root, out, dt.date(2026, 9, 1), dt.date(2026, 9, 1))
    first = {table: read(out, table) for table in ("bbo", "ctx")}
    normalize(root, out, dt.date(2026, 9, 1), dt.date(2026, 9, 1))

    for table, frame in first.items():
        assert read(out, table).equals(frame)
        assert len(list((out / table).rglob("*.parquet"))) == 1
    assert list(out.rglob("*.tmp")) == []
    assert not (out / ".normalize-tmp").exists()


def test_rerun_drops_a_stale_stream_part(tmp_path: Path) -> None:
    root, out = tmp_path / "rec", tmp_path / "out"
    write_segment(
        root, "hl-ws", _DAY, "00", "hl-ws-01", [frame_env(1, _t(second=1), _bbo_frame())]
    )
    normalize(root, out, dt.date(2026, 9, 1), dt.date(2026, 9, 1), sources=("hl-ws",))
    parts = list((out / "bbo" / f"date={_DAY}").glob("recorder*.parquet"))
    assert [p.name for p in parts] == ["recorder.hl-ws.hl-ws-01.bbo.00000.parquet"]

    # A new run with only a different conn must remove the old part.
    root2 = tmp_path / "rec2"
    write_segment(
        root2,
        "hl-ws",
        _DAY,
        "00",
        "hl-ws-99",
        [frame_env(1, _t(second=1), _bbo_frame(), conn="hl-ws-99")],
    )
    normalize(root2, out, dt.date(2026, 9, 1), dt.date(2026, 9, 1), sources=("hl-ws",))

    parts = list((out / "bbo" / f"date={_DAY}").glob("recorder*.parquet"))
    assert [p.name for p in parts] == ["recorder.hl-ws.hl-ws-99.bbo.00000.parquet"]


def test_stale_normalize_tmp_is_removed_and_ignored(tmp_path: Path) -> None:
    root, out = tmp_path / "rec", tmp_path / "out"
    write_segment(
        root, "hl-ws", _DAY, "00", "hl-ws-01", [frame_env(1, _t(second=1), _bbo_frame())]
    )
    # A crashed run left junk in the staging dir, including a fake part.
    stale = out / ".normalize-tmp" / "bbo" / f"date={_DAY}"
    stale.mkdir(parents=True)
    (stale / "recorder.hl-ws.hl-ws-01.bbo.00009.parquet").write_bytes(b"not parquet")
    (out / ".normalize-tmp" / "leftover.txt").write_text("junk")

    report = normalize(root, out, dt.date(2026, 9, 1), dt.date(2026, 9, 1), sources=("hl-ws",))

    assert report.rows_for("bbo") == 1
    assert not (out / ".normalize-tmp").exists()
    parts = [p.name for p in (out / "bbo" / f"date={_DAY}").glob("recorder*.parquet")]
    assert parts == ["recorder.hl-ws.hl-ws-01.bbo.00000.parquet"]
    assert read(out, "bbo").height == 1


def test_symlinked_normalize_tmp_target_is_not_touched(tmp_path: Path) -> None:
    root, out = tmp_path / "rec", tmp_path / "out"
    outside = tmp_path / "outside"
    outside.mkdir()
    sentinel = outside / "keep.txt"
    sentinel.write_text("keep")
    out.mkdir()
    (out / ".normalize-tmp").symlink_to(outside, target_is_directory=True)
    # A same-named directory outside the output root must also survive.
    sibling = tmp_path / ".normalize-tmp"
    sibling.mkdir()
    (sibling / "keep.txt").write_text("keep")
    write_segment(
        root, "hl-ws", _DAY, "00", "hl-ws-01", [frame_env(1, _t(second=1), _bbo_frame())]
    )

    normalize(root, out, dt.date(2026, 9, 1), dt.date(2026, 9, 1), sources=("hl-ws",))

    assert sentinel.read_text() == "keep"
    assert not (out / ".normalize-tmp").is_symlink()
    assert not (out / ".normalize-tmp").exists()
    assert (sibling / "keep.txt").read_text() == "keep"


def test_normalize_tmp_inner_symlink_is_unlinked_not_followed(tmp_path: Path) -> None:
    root, out = tmp_path / "rec", tmp_path / "out"
    outside = tmp_path / "outside"
    outside.mkdir()
    sentinel = outside / "keep.txt"
    sentinel.write_text("keep")
    stale = out / ".normalize-tmp"
    stale.mkdir(parents=True)
    (stale / "escape").symlink_to(outside, target_is_directory=True)
    write_segment(
        root, "hl-ws", _DAY, "00", "hl-ws-01", [frame_env(1, _t(second=1), _bbo_frame())]
    )

    normalize(root, out, dt.date(2026, 9, 1), dt.date(2026, 9, 1), sources=("hl-ws",))

    assert sentinel.read_text() == "keep"
    assert not stale.exists()


def test_rerun_clears_a_stale_tmp_and_stays_idempotent(tmp_path: Path) -> None:
    root, out = tmp_path / "rec", tmp_path / "out"
    write_segment(
        root, "hl-ws", _DAY, "00", "hl-ws-01", [frame_env(1, _t(second=1), _bbo_frame())]
    )
    normalize(root, out, dt.date(2026, 9, 1), dt.date(2026, 9, 1), sources=("hl-ws",))
    first = read(out, "bbo")

    # Simulate a crash between runs: recreate the staging dir.
    (out / ".normalize-tmp").mkdir()
    (out / ".normalize-tmp" / "junk.parquet").write_bytes(b"junk")
    normalize(root, out, dt.date(2026, 9, 1), dt.date(2026, 9, 1), sources=("hl-ws",))

    assert read(out, "bbo").equals(first)
    assert not (out / ".normalize-tmp").exists()
    assert list(out.rglob("*.tmp")) == []


def test_small_flush_writes_one_bounded_part_per_batch(tmp_path: Path) -> None:
    root, out = tmp_path / "rec", tmp_path / "out"
    envelopes = [frame_env(seq, _t(second=seq), _bbo_frame()) for seq in range(1, 11)]
    write_segment(root, "hl-ws", _DAY, "00", "hl-ws-01", envelopes)

    # flush_rows=1 forces one bounded part per row instead of one part per
    # stream, which is what keeps a huge stream from being held in memory.
    report = normalize(
        root, out, dt.date(2026, 9, 1), dt.date(2026, 9, 1), sources=("hl-ws",), flush_rows=1
    )

    assert report.rows_for("bbo") == 10
    parts = list((out / "bbo" / f"date={_DAY}").glob("recorder*.parquet"))
    assert len(parts) == 10
    assert read(out, "bbo").height == 10
    assert list(out.rglob("*.tmp")) == []


def test_gap_spanning_midnight_is_clipped_into_each_day(tmp_path: Path) -> None:
    root, out = tmp_path / "rec", tmp_path / "out"
    envelopes = [
        frame_env(1, _t(hour=23, second=0), _bbo_frame()),
        envelope(2, _t(hour=23, second=1), kind="gap_start", meta={"reason": "error"}),
        frame_env(3, _T0 + DAY_NS + 60, _bbo_frame()),
    ]
    write_segment(root, "hl-ws", _DAY, "23", "hl-ws-01", envelopes)

    normalize(root, out, dt.date(2026, 9, 1), dt.date(2026, 9, 2), sources=("hl-ws",))

    day1 = read(out, "gaps", _DAY)
    day2 = read(out, "gaps", _DAY2)
    assert day1.row(0, named=True)["end_ns"] == _T0 + DAY_NS
    assert day2.row(0, named=True)["start_ns"] == _T0 + DAY_NS


# --------------------------------------------------------------------------
# CLI
# --------------------------------------------------------------------------


def test_cli_normalizes_and_returns_zero(tmp_path: Path) -> None:
    root, out = tmp_path / "rec", tmp_path / "out"
    write_segment(
        root, "hl-ws", _DAY, "00", "hl-ws-01", [frame_env(1, _t(second=1), _bbo_frame())]
    )
    code = main(
        [
            "--from",
            _DAY,
            "--to",
            _DAY,
            "--in",
            str(root),
            "--out",
            str(out),
            "--sources",
            "hl-ws",
        ]
    )
    assert code == 0
    assert read(out, "bbo").height == 1


def test_cli_rejects_a_reversed_range(tmp_path: Path) -> None:
    code = main(
        ["--from", "2026-09-02", "--to", "2026-09-01", "--in", str(tmp_path), "--out", str(tmp_path)]
    )
    assert code == 2


def test_cli_rejects_an_unknown_source(tmp_path: Path) -> None:
    code = main(
        ["--from", _DAY, "--to", _DAY, "--in", str(tmp_path), "--out", str(tmp_path), "--sources", "kraken"]
    )
    assert code == 2


def test_reversed_range_raises() -> None:
    with pytest.raises(NormalizeError, match="is after"):
        normalize(
            "irrelevant",
            "irrelevant",
            dt.date(2026, 9, 2),
            dt.date(2026, 9, 1),
        )
