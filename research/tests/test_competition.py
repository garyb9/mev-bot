"""Tests for :mod:`hlr.competition` (ST-1, SPEC-0008 §13.10).

All fixtures are tiny synthetic frames built in memory (plus one small zstd
segment in ``tmp_path`` for the reader glue); no real recording is read and
nothing is written under ``data/`` or ``research/data/``.
"""

from __future__ import annotations

import datetime as dt
from pathlib import Path

import orjson
import polars as pl
import pytest
import zstandard

from hlr.competition import (
    CHUNK_NS,
    COMPETE_COLUMN,
    GAP_OVERLAP_COLUMN,
    TRADE_COLUMNS,
    CompetitionError,
    estimate_competition,
    iter_recorded_trades,
    trades_from_envelopes,
)

#: A small base timestamp so ns arithmetic is easy to read.
T0 = 1_700_000_000_000_000_000
#: One hour in ns.
_NS_H = 3_600_000_000_000


def episode(
    *,
    coin: str = "BTC",
    t_start: int = T0,
    t_end: int = T0 + 1_000,
    side: str = "buy",
    px: float = 100.0,
    sz: float | None = 1_000.0,
    notional: float | None = None,
) -> dict:
    """One episode row with the ST-1 opportunity columns."""
    row: dict = {"coin": coin, "t_start": t_start, "t_end": t_end, "side": side, "px": px}
    if sz is not None:
        row["sz"] = sz
    if notional is not None:
        row["notional"] = notional
    return row


def trade(
    *,
    ts_ns: int = T0,
    coin: str = "BTC",
    px: float = 100.0,
    sz: float = 1.0,
    side: str = "buy",
    tid: str | None = "1",
) -> dict:
    """One public-trade row."""
    return {"ts_ns": ts_ns, "coin": coin, "px": px, "sz": sz, "side": side, "tid": tid}


def episodes_frame(*rows: dict) -> pl.DataFrame:
    return pl.DataFrame(list(rows))


def trades_frame(*rows: dict) -> pl.DataFrame:
    if not rows:
        return pl.DataFrame(schema=TRADE_COLUMNS)
    return pl.DataFrame(list(rows))


# --------------------------------------------------------------------------
# Window boundaries
# --------------------------------------------------------------------------


def test_window_boundaries_are_inclusive() -> None:
    eps = episodes_frame(episode(t_start=T0 + 1_000, t_end=T0 + 2_000, sz=1_000.0))
    trades = trades_frame(
        trade(ts_ns=T0 + 999, sz=10.0, tid="before"),
        trade(ts_ns=T0 + 1_000, sz=1.0, tid="start"),
        trade(ts_ns=T0 + 2_000 + 100, sz=2.0, tid="end"),
        trade(ts_ns=T0 + 2_001 + 100, sz=10.0, tid="after"),
    )

    out = estimate_competition(eps, trades, latency_ns=100)

    # Only the exact-start and exact-end-plus-latency trades count.
    assert out[COMPETE_COLUMN].to_list() == [(1.0 + 2.0) * 100.0]


def test_strictly_before_and_after_do_not_count() -> None:
    eps = episodes_frame(episode(t_start=T0 + 1_000, t_end=T0 + 2_000, sz=1_000.0))
    trades = trades_frame(
        trade(ts_ns=T0 + 999, sz=100.0, tid="before"),
        trade(ts_ns=T0 + 2_001, sz=100.0, tid="after"),
    )

    out = estimate_competition(eps, trades, latency_ns=0)

    assert out[COMPETE_COLUMN].to_list() == [0.0]


# --------------------------------------------------------------------------
# Side
# --------------------------------------------------------------------------


def test_only_opportunity_side_counts() -> None:
    eps = episodes_frame(
        episode(side="buy", sz=1_000.0),
        episode(side="sell", sz=1_000.0),
    )
    trades = trades_frame(
        trade(side="buy", sz=1.0, tid="b"),
        trade(side="sell", sz=5.0, tid="s"),
    )

    out = estimate_competition(eps, trades)

    # The buy episode counts the buy trade; the sell episode the sell trade.
    assert out[COMPETE_COLUMN].to_list() == [100.0, 500.0]


def test_unknown_episode_side_raises() -> None:
    eps = episodes_frame(episode(side="sideways"))
    with pytest.raises(CompetitionError, match="side"):
        estimate_competition(eps, trades_frame(trade()))


# --------------------------------------------------------------------------
# Price
# --------------------------------------------------------------------------


def test_worse_price_does_not_count_and_through_price_does() -> None:
    eps = episodes_frame(episode(side="buy", px=100.0, sz=1_000.0))
    trades = trades_frame(
        trade(px=101.0, sz=10.0, tid="worse"),
        trade(px=100.0, sz=1.0, tid="at"),
        trade(px=99.0, sz=1.0, tid="through"),
    )

    out = estimate_competition(eps, trades)

    assert out[COMPETE_COLUMN].to_list() == [100.0 + 99.0]


def test_sell_worse_price_does_not_count() -> None:
    eps = episodes_frame(episode(side="sell", px=100.0, sz=1_000.0))
    trades = trades_frame(
        trade(side="sell", px=99.0, sz=10.0, tid="worse"),
        trade(side="sell", px=100.0, sz=1.0, tid="at"),
        trade(side="sell", px=101.0, sz=2.0, tid="through"),
    )

    out = estimate_competition(eps, trades)

    assert out[COMPETE_COLUMN].to_list() == [100.0 + 202.0]


# --------------------------------------------------------------------------
# Cap, floor, empty
# --------------------------------------------------------------------------


def test_compete_is_capped_at_quoted_notional_and_zero_without_trades() -> None:
    eps = episodes_frame(
        episode(sz=0.5, px=100.0),  # quoted 50
        episode(coin="ETH", sz=100.0),  # no trades
    )
    trades = trades_frame(trade(sz=1.0, px=100.0, tid="big"))

    out = estimate_competition(eps, trades)

    assert out[COMPETE_COLUMN].to_list() == [50.0, 0.0]


def test_notional_column_is_authoritative() -> None:
    eps = episodes_frame(episode(sz=1_000.0, notional=30.0))
    trades = trades_frame(trade(sz=1.0, px=100.0, tid="big"))

    out = estimate_competition(eps, trades)

    assert out[COMPETE_COLUMN].to_list() == [30.0]


def test_duplicate_tid_counts_once_distinct_tids_twice() -> None:
    eps = episodes_frame(episode(sz=1_000.0))
    trades = trades_frame(
        trade(sz=1.0, tid="t1"),
        trade(sz=1.0, tid="t1"),
        trade(sz=1.0, tid="t2"),
    )

    out = estimate_competition(eps, trades)

    assert out[COMPETE_COLUMN].to_list() == [200.0]


# --------------------------------------------------------------------------
# Gaps
# --------------------------------------------------------------------------


def test_gap_overlap_is_flagged_and_compete_is_a_lower_bound() -> None:
    eps = episodes_frame(
        episode(t_start=T0 + 1_000, t_end=T0 + 2_000),
        episode(t_start=T0 + 3_000, t_end=T0 + 4_000),
    )
    gaps = pl.DataFrame(
        {"start_ns": [T0 + 1_500], "end_ns": [T0 + 1_600], "src": ["hl-ws"]}
    )
    trades = trades_frame(trade(ts_ns=T0 + 1_100, sz=1.0, tid="before-gap"))

    out = estimate_competition(eps, trades, gaps=gaps)

    assert out[GAP_OVERLAP_COLUMN].to_list() == [True, False]
    assert out[COMPETE_COLUMN].to_list() == [100.0, 0.0]


def test_gap_src_filter_matches_only_the_feed() -> None:
    eps = episodes_frame(episode(t_start=T0 + 1_000, t_end=T0 + 2_000))
    gaps = pl.DataFrame(
        {"start_ns": [T0 + 1_500], "end_ns": [T0 + 1_600], "src": ["binance-usdm"]}
    )

    out = estimate_competition(eps, trades_frame(trade()), gaps=gaps)

    assert out[GAP_OVERLAP_COLUMN].to_list() == [False]


# --------------------------------------------------------------------------
# Empty inputs
# --------------------------------------------------------------------------


def test_empty_episodes_return_typed_empty_frame() -> None:
    empty_eps = pl.DataFrame(
        schema={
            "coin": pl.String,
            "t_start": pl.Int64,
            "t_end": pl.Int64,
            "side": pl.String,
            "px": pl.Float64,
            "sz": pl.Float64,
        }
    )

    out = estimate_competition(empty_eps, trades_frame())

    assert out.height == 0
    assert out.schema[COMPETE_COLUMN] == pl.Float64
    assert out.schema[GAP_OVERLAP_COLUMN] == pl.Boolean


def test_empty_trades_yield_zero_competition() -> None:
    eps = episodes_frame(episode())

    out = estimate_competition(eps, trades_frame())

    assert out[COMPETE_COLUMN].to_list() == [0.0]
    assert out[GAP_OVERLAP_COLUMN].to_list() == [False]


# --------------------------------------------------------------------------
# Normalizer
# --------------------------------------------------------------------------


def _frame_env(
    seq: int,
    t_ns: int,
    *,
    kind: str = "frame",
    raw: str | None = None,
    meta: dict | None = None,
) -> dict:
    env: dict = {
        "v": 1,
        "src": "hl-ws",
        "conn": "hl-ws-01",
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


def _trades_raw(*trades_data: dict) -> str:
    return orjson.dumps({"channel": "trades", "data": list(trades_data)}).decode()


def test_trades_from_envelopes_extracts_dedupes_and_ignores_gaps() -> None:
    data = [
        {"coin": "BTC", "side": "B", "px": "100", "sz": "1", "tid": 1, "time": 1},
        {"coin": "BTC", "side": "B", "px": "100", "sz": "1", "tid": 1, "time": 2},
        {"coin": "BTC", "side": "A", "px": "99", "sz": "2", "tid": 2, "time": 3},
    ]
    bbo = orjson.dumps({"channel": "bbo", "data": {"coin": "BTC"}}).decode()
    envelopes = [
        _frame_env(0, T0, kind="segment_open", meta={}),
        _frame_env(1, T0, raw=bbo),
        _frame_env(2, T0 + 1, kind="gap_start", meta={"reason": "close"}),
        _frame_env(3, T0 + 2, raw=_trades_raw(*data)),
        _frame_env(4, T0 + 3, kind="gap_end", meta={"gap_ms": 1}),
    ]

    chunks = list(trades_from_envelopes(envelopes))

    assert len(chunks) == 1
    frame = chunks[0]
    assert frame.columns == list(TRADE_COLUMNS)
    assert frame.height == 2  # the duplicate tid=1 is dropped
    assert frame["tid"].to_list() == ["1", "2"]
    assert frame["side"].to_list() == ["buy", "sell"]
    assert frame["coin"].to_list() == ["BTC", "BTC"]


def test_trades_from_envelopes_chunks_by_hour_and_dedupes_across_chunks() -> None:
    envelopes = [
        _frame_env(1, T0, raw=_trades_raw({"coin": "BTC", "side": "B", "px": "1", "sz": "1", "tid": 5})),
        _frame_env(2, T0 + _NS_H, raw=_trades_raw({"coin": "BTC", "side": "B", "px": "1", "sz": "1", "tid": 5})),
        _frame_env(3, T0 + _NS_H + 1, raw=_trades_raw({"coin": "BTC", "side": "A", "px": "1", "sz": "1", "tid": 6})),
    ]

    chunks = list(trades_from_envelopes(envelopes, chunk_ns=CHUNK_NS))

    assert [frame.height for frame in chunks] == [1, 1]
    assert chunks[0]["tid"].to_list() == ["5"]
    # The replayed tid=5 is dropped; the new tid=6 is kept.
    assert chunks[1]["tid"].to_list() == ["6"]


def test_trades_from_envelopes_rejects_bad_parameters() -> None:
    with pytest.raises(CompetitionError, match="chunk_ns"):
        list(trades_from_envelopes([], chunk_ns=0))
    with pytest.raises(CompetitionError, match="max_tids"):
        list(trades_from_envelopes([], max_tids=0))


def _write_segment(root: Path, day: str, hour: str, t_ns: int, envelopes: list[dict]) -> None:
    """Write a tiny finished ``.jsonl.zst`` segment under the §6 layout."""
    path = root / "hl-ws" / day / hour / f"hl-ws-01-{t_ns}.jsonl.zst"
    path.parent.mkdir(parents=True, exist_ok=True)
    body = b"".join(orjson.dumps(env) + b"\n" for env in envelopes)
    path.write_bytes(zstandard.ZstdCompressor(level=3).compress(body))


def test_iter_recorded_trades_reads_a_segment(tmp_path: Path) -> None:
    day = dt.date(2026, 9, 1)
    day_ns = (day - dt.date(1970, 1, 1)).days * 86_400_000_000_000
    t_ns = day_ns + 5_000_000_000  # 00:00:05 UTC
    root = tmp_path / "rec"
    envelopes = [
        _frame_env(0, t_ns, kind="segment_open", meta={}),
        _frame_env(1, t_ns, raw=_trades_raw({"coin": "BTC", "side": "B", "px": "100", "sz": "3", "tid": 7})),
    ]
    _write_segment(root, "2026-09-01", "00", t_ns, envelopes)

    chunks = list(iter_recorded_trades(root, "2026-09-01", "2026-09-01"))

    assert len(chunks) == 1
    assert chunks[0]["px"].to_list() == [100.0]
    assert chunks[0]["sz"].to_list() == [3.0]
    assert chunks[0]["ts_ns"].to_list() == [t_ns]
