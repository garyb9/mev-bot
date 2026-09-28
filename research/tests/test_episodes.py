"""Tests for :mod:`hlr.episodes` (SPEC-0008 P-4, §13.3–§13.5, §13.10 hooks).

Every frame is hand-built with known answers; nothing touches the network or
``research/data/``. Times are nanoseconds and the helpers name the milliseconds.
"""

from __future__ import annotations

import datetime as _dt
import math

import polars as pl
import pytest

from hlr.episodes import (
    BPS_SCALE,
    EpisodeConfig,
    EpisodeError,
    Feed,
    daily_capture,
    day_block_bootstrap_ci,
    detect_episodes,
    episode_metrics,
    feed_validity,
    jittered_latency_ms,
    oos_split,
)

MS = 1_000_000
MIDNIGHT = 86_400 * MS
DAY1 = _dt.date(1970, 1, 1)
DAY2 = _dt.date(1970, 1, 2)
MC = pytest.approx


def ms(value: float) -> int:
    return int(value * MS)


def frame(
    times: list[int],
    net: list[float],
    size: list[float] | None = None,
    **extra: list[object],
) -> pl.DataFrame:
    data: dict[str, list[object]] = {
        "t_ns": times,
        "net_bps": net,
        "size_usd": size if size is not None else [100.0] * len(times),
    }
    data.update(extra)
    return pl.DataFrame(data)


def episode_rows(episodes: pl.DataFrame) -> list[dict[str, object]]:
    return episodes.iter_rows(named=True)


# --------------------------------------------------------------------------
# §13.3 detection
# --------------------------------------------------------------------------


def test_one_clear_episode_has_exact_fields() -> None:
    f = frame([ms(0), ms(1), ms(2), ms(3), ms(4)], [-1.0, 5.0, 6.0, -1.0, -1.0])
    cfg = EpisodeConfig(merge_ms=0, latencies_ms=(1, 2, 5))
    ep = detect_episodes(f, net_bps="net_bps", size_usd="size_usd", config=cfg)
    (row,) = episode_rows(ep)
    assert row["t_start"] == ms(1)
    assert row["t_end"] == ms(3)  # first row where the edge is no longer > 0
    assert row["duration_ms"] == 2
    assert row["peak_net_bps"] == MC(6.0)
    assert row["start_net_bps"] == MC(5.0)
    assert row["size_usd_at_start"] == MC(100.0)
    assert row["date"] == DAY1
    assert row["open_1"] is True
    assert row["captured_1"] == MC(6.0 * 100.0 * BPS_SCALE)  # re-read at t_start + 1 ms
    assert row["open_2"] is False  # t_end is exclusive
    assert row["captured_2"] == MC(0.0)


def test_no_episode_when_below_cost() -> None:
    f = frame([ms(0), ms(1), ms(2), ms(3)], [-1.0, -0.1, -5.0, 0.0])
    ep = detect_episodes(f, net_bps="net_bps", size_usd="size_usd")
    assert ep.height == 0
    assert "captured_10" in ep.columns


def test_boundary_exactly_at_threshold_is_not_an_episode() -> None:
    # net_bps > 0 is strict: exactly zero does not open an episode.
    at_zero = frame([ms(0), ms(1), ms(2)], [0.0, 0.0, 0.0])
    assert detect_episodes(at_zero, net_bps="net_bps", size_usd="size_usd").height == 0
    above = frame([ms(0), ms(1), ms(2)], [0.0, 1e-9, 0.0])
    ep = detect_episodes(above, net_bps="net_bps", size_usd="size_usd")
    assert ep.height == 1
    assert ep["t_start"][0] == ms(1)


def test_edge_that_vanishes_before_latency_captures_zero() -> None:
    f = frame([ms(0), ms(1), ms(2), ms(3)], [0.0, 5.0, -1.0, -1.0])
    cfg = EpisodeConfig(merge_ms=0, latencies_ms=(1,))
    ep = detect_episodes(f, net_bps="net_bps", size_usd="size_usd", config=cfg)
    (row,) = episode_rows(ep)
    assert row["t_end"] == ms(2)
    assert row["open_1"] is False
    assert row["captured_1"] == MC(0.0)


def test_edge_is_reevaluated_at_latency_not_taken_from_start() -> None:
    # The edge is 5 bps at t_start but only 2 bps at t_start+1 ms.
    f = frame([ms(0), ms(1), ms(2), ms(3)], [0.0, 5.0, 2.0, -1.0])
    cfg = EpisodeConfig(merge_ms=0, latencies_ms=(1,))
    ep = detect_episodes(f, net_bps="net_bps", size_usd="size_usd", config=cfg)
    (row,) = episode_rows(ep)
    assert row["captured_1"] == MC(2.0 * 100.0 * BPS_SCALE)


def test_episodes_within_merge_ms_merge() -> None:
    f = frame(
        [ms(0), ms(1), ms(2), ms(3), ms(4), ms(5), ms(6)],
        [5.0, 5.0, -1.0, -1.0, 5.0, 5.0, -1.0],
        [1.0] * 7,
    )
    merged = detect_episodes(
        f,
        net_bps="net_bps",
        size_usd="size_usd",
        config=EpisodeConfig(merge_ms=5, latencies_ms=(1,)),
    )
    (row,) = episode_rows(merged)
    assert row["t_start"] == ms(0)
    assert row["t_end"] == ms(6)
    assert row["peak_net_bps"] == MC(5.0)
    assert row["start_net_bps"] == MC(5.0)

    split = detect_episodes(
        f,
        net_bps="net_bps",
        size_usd="size_usd",
        config=EpisodeConfig(merge_ms=1, latencies_ms=(1,)),
    )
    assert split.height == 2


def test_do_not_merge_across_invalid_separator() -> None:
    f = frame(
        [ms(0), ms(1), ms(2), ms(3), ms(4), ms(5), ms(6)],
        [5.0, 5.0, -1.0, -1.0, 5.0, 5.0, -1.0],
        [1.0] * 7,
        valid=[True, True, False, False, True, True, True],
    )
    ep = detect_episodes(
        f,
        net_bps="net_bps",
        size_usd="size_usd",
        valid="valid",
        config=EpisodeConfig(merge_ms=50, latencies_ms=(1,)),
    )
    assert ep.height == 2
    assert ep["t_start"].to_list() == [ms(0), ms(4)]


def test_invalid_feed_breaks_the_episode() -> None:
    f = frame([ms(0), ms(1), ms(2), ms(3)], [5.0, 5.0, 5.0, 5.0], valid=[True, True, False, True])
    ep = detect_episodes(
        f,
        net_bps="net_bps",
        size_usd="size_usd",
        valid="valid",
        config=EpisodeConfig(merge_ms=0, latencies_ms=(1,)),
    )
    assert ep.height == 2
    (first, second) = episode_rows(ep)
    assert first["t_start"] == ms(0)
    assert first["t_end"] == ms(2)
    assert second["t_start"] == ms(3)


def test_size_is_capped_at_max_notional() -> None:
    f = frame([ms(0), ms(1), ms(2)], [0.0, 5.0, -1.0], [1_000.0, 1_000.0, 1_000.0])
    ep = detect_episodes(
        f,
        net_bps="net_bps",
        size_usd="size_usd",
        config=EpisodeConfig(merge_ms=0, latencies_ms=(0,), max_notional=250.0),
    )
    (row,) = episode_rows(ep)
    assert row["size_usd_at_start"] == MC(250.0)
    assert row["captured_0"] == MC(5.0 * 250.0 * BPS_SCALE)


def test_expressions_are_accepted_in_place_of_columns() -> None:
    f = frame([ms(0), ms(1), ms(2)], [0.0, 5.0, -1.0])
    ep = detect_episodes(
        f,
        net_bps=pl.col("net_bps"),
        size_usd=pl.col("size_usd") * 2,
        config=EpisodeConfig(merge_ms=0, latencies_ms=(1,)),
    )
    (row,) = episode_rows(ep)
    assert row["size_usd_at_start"] == MC(200.0)


# --------------------------------------------------------------------------
# §13.3 day boundary and no lookahead
# --------------------------------------------------------------------------


def test_episode_across_midnight_stays_one_and_is_dated_by_start() -> None:
    f = frame(
        [MIDNIGHT - ms(2), MIDNIGHT - ms(1), MIDNIGHT, MIDNIGHT + ms(1), MIDNIGHT + ms(2)],
        [0.0, 5.0, 5.0, 5.0, -1.0],
    )
    ep = detect_episodes(
        f,
        net_bps="net_bps",
        size_usd="size_usd",
        config=EpisodeConfig(merge_ms=0, latencies_ms=(1, 3)),
    )
    (row,) = episode_rows(ep)
    assert row["t_start"] == MIDNIGHT - ms(1)
    assert row["t_end"] == MIDNIGHT + ms(2)
    assert row["date"] == DAY1
    assert row["duration_ms"] == 3
    # The +1 ms probe lands after midnight and reads the next day's state.
    assert row["open_1"] is True
    assert row["captured_1"] == MC(5.0 * 100.0 * BPS_SCALE)
    assert row["open_3"] is False


def test_capture_at_latency_never_reads_a_later_row() -> None:
    # A huge edge at t=10 ms must not inflate the capture probed at t=2 ms.
    f = frame([ms(0), ms(1), ms(2), ms(10)], [0.0, 5.0, 5.0, 1000.0], [1.0] * 4)
    ep = detect_episodes(
        f,
        net_bps="net_bps",
        size_usd="size_usd",
        config=EpisodeConfig(merge_ms=0, latencies_ms=(1,)),
    )
    (row,) = episode_rows(ep)
    assert row["t_start"] == ms(1)
    assert row["peak_net_bps"] == MC(1000.0)
    assert row["captured_1"] == MC(5.0 * 1.0 * BPS_SCALE)


def test_appending_future_rows_does_not_change_past_episodes() -> None:
    base = frame([ms(0), ms(1), ms(2), ms(3)], [0.0, 5.0, 6.0, -1.0], [1.0] * 4)
    extended = pl.concat(
        [base, frame([ms(4), ms(5)], [1000.0, 1000.0], [1.0] * 2)],
    )
    cfg = EpisodeConfig(merge_ms=0, latencies_ms=(1,))
    first = detect_episodes(base, net_bps="net_bps", size_usd="size_usd", config=cfg)
    second = detect_episodes(extended, net_bps="net_bps", size_usd="size_usd", config=cfg)
    assert first.height == 1
    assert second.height == 2
    for column in first.columns:
        assert first[column].to_list() == second[column].to_list()[:1]


def test_probe_past_the_stale_window_is_not_captured() -> None:
    # The episode runs to 5000 ms but the last state is at 1 ms; a probe at
    # +3000 ms would be acting on a feed already older than stale_ms.
    f = frame([ms(0), ms(1), ms(5000)], [5.0, 5.0, -1.0], [100.0] * 3)
    ep = detect_episodes(
        f,
        net_bps="net_bps",
        size_usd="size_usd",
        config=EpisodeConfig(merge_ms=0, latencies_ms=(1000, 3000), stale_ms=2000),
    )
    (row,) = episode_rows(ep)
    # t_end is capped at the last state (1 ms) + stale_ms (2000 ms).
    assert row["t_end"] == ms(2001)
    assert row["duration_ms"] == 2001
    assert row["open_1000"] is True
    assert row["captured_1000"] == MC(5.0 * 100.0 * BPS_SCALE)
    assert row["open_3000"] is False
    assert row["captured_3000"] == MC(0.0)


def test_lazy_frame_and_custom_time_column_are_supported() -> None:
    f = frame([ms(0), ms(1), ms(2)], [0.0, 5.0, -1.0]).rename({"t_ns": "ts"})
    ep = detect_episodes(
        f.lazy(),
        net_bps="net_bps",
        size_usd="size_usd",
        config=EpisodeConfig(merge_ms=0, latencies_ms=(0,)),
        time_col="ts",
    )
    (row,) = episode_rows(ep)
    assert row["t_start"] == ms(1)
    assert row["captured_0"] == MC(5.0 * 100.0 * BPS_SCALE)


# --------------------------------------------------------------------------
# §13.3 feed validity (stale_ms + gaps)
# --------------------------------------------------------------------------


def test_feed_validity_uses_freshness_and_gaps() -> None:
    f = pl.DataFrame(
        {
            "t_ns": [ms(0), ms(1000), ms(2500), ms(4000)],
            "last_a": [ms(0), ms(0), ms(0), ms(4000)],
            "last_b": [ms(0), ms(1000), ms(2500), ms(4000)],
        }
    )
    gaps = pl.DataFrame(
        {
            "src": ["hl-ws", "hl-ws"],
            "conn": ["bbo", "bbo"],
            "start_ns": [ms(1500), 0],
            "end_ns": [ms(2000), 0],
        }
    )
    out = feed_validity(
        f,
        [Feed("a", "last_a", "hl-ws", "bbo")],
        stale_ms=2000,
        gaps=gaps,
    )
    assert out["a_valid"].to_list() == [True, True, False, True]
    assert out["valid"].to_list() == out["a_valid"].to_list()


def test_feed_validity_gap_invalidates_even_when_fresh() -> None:
    # t = 1600 ms is inside the [1500, 2000) gap although the feed is fresh.
    f = pl.DataFrame({"t_ns": [ms(1600)], "last_a": [ms(1500)]})
    gaps = pl.DataFrame(
        {
            "src": ["hl-ws"],
            "conn": ["bbo"],
            "start_ns": [ms(1500)],
            "end_ns": [ms(2000)],
        }
    )
    out = feed_validity(
        f,
        [Feed("a", "last_a", "hl-ws", "bbo")],
        stale_ms=5000,
        gaps=gaps,
    )
    assert out["valid"].to_list() == [False]
    # At exactly end_ns the gap is over.
    at_end = pl.DataFrame({"t_ns": [ms(2000)], "last_a": [ms(2000)]})
    closed = feed_validity(
        at_end,
        [Feed("a", "last_a", "hl-ws", "bbo")],
        stale_ms=5000,
        gaps=gaps,
    )
    assert closed["valid"].to_list() == [True]


def test_feed_validity_requires_all_feeds() -> None:
    f = pl.DataFrame(
        {"t_ns": [ms(10)], "last_a": [ms(0)], "last_b": [ms(0)]},
    )
    out = feed_validity(
        f,
        [Feed("a", "last_a"), Feed("b", "last_b")],
        stale_ms=5,
    )
    assert out["a_valid"].to_list() == [False]
    assert out["valid"].to_list() == [False]


def test_missing_column_raises() -> None:
    with pytest.raises(EpisodeError, match="missing column"):
        detect_episodes(frame([ms(0)], [1.0]), net_bps="nope", size_usd="size_usd")


def test_gap_splits_an_otherwise_contiguous_episode() -> None:
    times = [ms(0), ms(1), ms(2), ms(3), ms(4), ms(5)]
    f = frame(times, [5.0, 5.0, 5.0, 5.0, 5.0, -1.0], [100.0] * 6, last_a=times, last_b=times)
    gaps = pl.DataFrame(
        {
            "src": ["hl-ws"],
            "conn": ["bbo"],
            "start_ns": [ms(2)],
            "end_ns": [ms(4)],
        }
    )
    valid = feed_validity(
        f,
        [Feed("a", "last_a", "hl-ws", "bbo"), Feed("b", "last_b")],
        stale_ms=10,
        gaps=gaps,
    )
    ep = detect_episodes(
        valid,
        net_bps="net_bps",
        size_usd="size_usd",
        valid="valid",
        config=EpisodeConfig(merge_ms=1, latencies_ms=(1,)),
    )
    assert ep["t_start"].to_list() == [ms(0), ms(4)]
    assert ep["t_end"].to_list() == [ms(2), ms(5)]


# --------------------------------------------------------------------------
# §13.10 fill competition hook
# --------------------------------------------------------------------------


def test_compete_usd_reduces_capture_and_never_goes_negative() -> None:
    f = frame(
        [ms(0), ms(1), ms(2), ms(3)],
        [0.0, 5.0, 6.0, -1.0],
        [100.0] * 4,
        compete=[0.0, 10.0, 20.0, 0.0],
    )
    ep = detect_episodes(
        f,
        net_bps="net_bps",
        size_usd="size_usd",
        config=EpisodeConfig(merge_ms=0, latencies_ms=(1, 2)),
        compete_usd="compete",
    )
    (row,) = episode_rows(ep)
    # Probe at t=2 ms: 10 + 20 = 30 USD traded by others in [1 ms, 2 ms].
    assert row["competed_1"] == MC(30.0)
    assert row["captured_1"] == MC(6.0 * 100.0 * BPS_SCALE)
    assert row["captured_adj_1"] == MC(6.0 * (100.0 - 30.0) * BPS_SCALE)
    # Probe past t_end is closed, so both captures are zero.
    assert row["captured_adj_2"] == MC(0.0)


def test_compete_larger_than_size_floors_at_zero() -> None:
    f = frame([ms(0), ms(1), ms(2)], [0.0, 5.0, -1.0], [10.0] * 3, compete=[0.0, 500.0, 0.0])
    ep = detect_episodes(
        f,
        net_bps="net_bps",
        size_usd="size_usd",
        config=EpisodeConfig(merge_ms=0, latencies_ms=(1,)),
        compete_usd="compete",
    )
    (row,) = episode_rows(ep)
    assert row["captured_adj_1"] == MC(0.0)


# --------------------------------------------------------------------------
# §13.10 markout hook
# --------------------------------------------------------------------------


def test_markout_reports_signed_mid_move() -> None:
    # Mid rises 100 -> 101 in the 1 s after the fill; long direction => +100 bps.
    f = frame(
        [ms(0), ms(1), ms(1001), ms(1002)],
        [0.0, 5.0, 5.0, -1.0],
        [1.0] * 4,
        mid=[100.0, 100.0, 101.0, 101.0],
        dir=[1.0] * 4,
    )
    ep = detect_episodes(
        f,
        net_bps="net_bps",
        size_usd="size_usd",
        config=EpisodeConfig(
            merge_ms=0,
            latencies_ms=(1,),
            markout_horizons_s=(1,),
        ),
        mid="mid",
        direction="dir",
    )
    (row,) = episode_rows(ep)
    assert row["markout_1_1s"] == MC(100.0)


def test_markout_is_null_when_the_horizon_state_is_stale() -> None:
    # The 1 s horizon is 100 ms stale (stale_ms=100), so the markout is null.
    f = frame(
        [ms(0), ms(1), ms(50)],
        [0.0, 5.0, 5.0],
        [1.0] * 3,
        mid=[100.0, 100.0, 101.0],
        dir=[1.0] * 3,
    )
    ep = detect_episodes(
        f,
        net_bps="net_bps",
        size_usd="size_usd",
        config=EpisodeConfig(
            merge_ms=0,
            stale_ms=100,
            latencies_ms=(1,),
            markout_horizons_s=(1,),
        ),
        mid="mid",
        direction="dir",
    )
    (row,) = episode_rows(ep)
    assert row["markout_1_1s"] is None


def test_markout_is_signed_by_direction() -> None:
    f = frame(
        [ms(0), ms(1), ms(1001)],
        [0.0, 5.0, 5.0],
        [1.0] * 3,
        mid=[100.0, 100.0, 101.0],
        dir=[-1.0] * 3,
    )
    ep = detect_episodes(
        f,
        net_bps="net_bps",
        size_usd="size_usd",
        config=EpisodeConfig(
            merge_ms=0, latencies_ms=(1,), markout_horizons_s=(1,)
        ),
        mid="mid",
        direction="dir",
    )
    (row,) = episode_rows(ep)
    assert row["markout_1_1s"] == MC(-100.0)


# --------------------------------------------------------------------------
# §13.10 latency jitter hook
# --------------------------------------------------------------------------


def test_jittered_latency_is_deterministic_and_centred() -> None:
    assert jittered_latency_ms(250, 5, seed=7) == jittered_latency_ms(250, 5, seed=7)
    draws = sorted(jittered_latency_ms(250, 50000, seed=3))
    assert all(value > 0 for value in draws)
    median = draws[len(draws) // 2]
    p99 = draws[int(0.99 * len(draws))]
    assert median == MC(250.0, rel=0.02)
    assert p99 == MC(750.0, rel=0.07)


def test_jitter_columns_are_produced_when_enabled() -> None:
    f = frame([ms(0), ms(1), ms(2), ms(9)], [0.0, 5.0, 5.0, -1.0], [100.0] * 4)
    ep = detect_episodes(
        f,
        net_bps="net_bps",
        size_usd="size_usd",
        config=EpisodeConfig(merge_ms=0, latencies_ms=(1,), jitter=True, jitter_seed=1),
    )
    assert "captured_jitter_1" in ep.columns
    assert "open_jitter_1" in ep.columns


# --------------------------------------------------------------------------
# §13.5 / §13.10 metrics
# --------------------------------------------------------------------------


def test_episode_metrics_roll_up_per_latency() -> None:
    f = frame([ms(0), ms(1), ms(2), ms(9), ms(10), ms(11)], [0, 5, 6, -1, 4, -1], [100.0] * 6)
    ep = detect_episodes(
        f,
        net_bps="net_bps",
        size_usd="size_usd",
        config=EpisodeConfig(merge_ms=0, latencies_ms=(1, 5)),
    )
    table = episode_metrics(ep, days=1, latencies_ms=(1, 5), all_dates=[DAY1], capital_usd=10_000.0)
    assert table.height == 2
    first = table.row(0, named=True)
    assert first["latency_ms"] == 1
    assert first["capture_variant"] == "naive"
    assert first["episodes"] == 2
    assert first["capture_rate"] == MC(0.5)  # probe +5 ms lands past the first t_end
    assert first["usd_per_day"] == MC(first["capture_usd"])
    assert first["usd_per_day_ci90_lo"] <= first["usd_per_day"] <= first["usd_per_day_ci90_hi"]
    assert first["apr"] == MC(first["usd_per_day"] * 365.0 / 10_000.0)


def test_metrics_counts_zero_episode_days() -> None:
    ep = detect_episodes(
        frame([ms(0), ms(1), ms(2)], [0.0, 5.0, -1.0]),
        net_bps="net_bps",
        size_usd="size_usd",
        config=EpisodeConfig(merge_ms=0, latencies_ms=(1,)),
    )
    table = episode_metrics(ep, days=2, latencies_ms=(1,), all_dates=[DAY1, DAY2])
    row = table.row(0, named=True)
    assert row["episodes_per_day"] == MC(0.5)
    assert row["episodes_per_day_p50"] == MC(0.5)


def test_daily_capture_groups_by_start_day() -> None:
    f = frame(
        [MIDNIGHT - ms(1), MIDNIGHT, MIDNIGHT + ms(2), MIDNIGHT + ms(9)],
        [0.0, 5.0, -1.0, -1.0],
    )
    ep = detect_episodes(
        f,
        net_bps="net_bps",
        size_usd="size_usd",
        config=EpisodeConfig(merge_ms=0, latencies_ms=(1,)),
    )
    daily = daily_capture(ep, 1, all_dates=[DAY1, DAY2])
    assert daily["date"].to_list() == [DAY1, DAY2]
    assert daily["episodes"].to_list() == [1, 0]
    assert daily["captured_usd"].to_list()[1] == MC(0.0)


def test_bootstrap_ci_is_deterministic_and_contains_mean() -> None:
    constant = day_block_bootstrap_ci([2.0, 2.0, 2.0], draws=200, seed=0)
    assert constant == (MC(2.0), MC(2.0))
    lo, hi = day_block_bootstrap_ci([0.0, 10.0], draws=500, seed=1)
    assert lo <= 5.0 <= hi
    assert (lo, hi) == day_block_bootstrap_ci([0.0, 10.0], draws=500, seed=1)
    empty_lo, empty_hi = day_block_bootstrap_ci([])
    assert math.isnan(empty_lo) and math.isnan(empty_hi)


def test_empty_episode_table_has_typed_columns() -> None:
    cfg = EpisodeConfig(merge_ms=0, latencies_ms=(10, 250), jitter=True)
    ep = detect_episodes(pl.DataFrame({"t_ns": []}), net_bps=pl.lit(0.0), size_usd=pl.lit(0.0), config=cfg)
    assert ep.height == 0
    assert ep.schema["open_10"] == pl.Boolean
    assert ep.schema["captured_jitter_250"] == pl.Float64


def test_capture_is_sized_at_start_when_the_book_grows() -> None:
    # The book is 100 USD at t_start and 300 USD at t_start+1 ms; the order we
    # could have placed at t_start caps the capture at 100.
    f = frame(
        [ms(0), ms(1), ms(2), ms(3)],
        [0.0, 5.0, 5.0, -1.0],
        [0.0, 100.0, 300.0, 300.0],
    )
    ep = detect_episodes(
        f,
        net_bps="net_bps",
        size_usd="size_usd",
        config=EpisodeConfig(merge_ms=0, latencies_ms=(1,)),
    )
    (row,) = episode_rows(ep)
    assert row["size_usd_at_start"] == MC(100.0)
    assert row["captured_1"] == MC(5.0 * 100.0 * BPS_SCALE)


def test_cap_is_applied_after_competition() -> None:
    # displayed 100k, cap 10k, competed 20k -> adjusted size 10k (not 0).
    f = frame(
        [ms(0), ms(1), ms(2)],
        [0.0, 5.0, -1.0],
        [0.0, 100_000.0, 100_000.0],
        compete=[0.0, 20_000.0, 0.0],
    )
    ep = detect_episodes(
        f,
        net_bps="net_bps",
        size_usd="size_usd",
        config=EpisodeConfig(merge_ms=0, latencies_ms=(0,), max_notional=10_000.0),
        compete_usd="compete",
    )
    (row,) = episode_rows(ep)
    assert row["size_usd_at_start"] == MC(10_000.0)
    assert row["captured_0"] == MC(5.0 * 10_000.0 * BPS_SCALE)
    assert row["captured_adj_0"] == MC(5.0 * 10_000.0 * BPS_SCALE)


def test_open_excludes_a_merged_dip() -> None:
    # The runs merge (sep 1 ms) but the merged interval holds net_bps == 0 at the
    # +2 ms probe, so it is not an open (capturable) probe.
    f = frame([ms(0), ms(1), ms(2), ms(3), ms(4), ms(5)], [5.0, 5.0, 0.0, 5.0, 5.0, -1.0])
    ep = detect_episodes(
        f,
        net_bps="net_bps",
        size_usd="size_usd",
        config=EpisodeConfig(merge_ms=5, latencies_ms=(2,)),
    )
    (row,) = episode_rows(ep)
    assert row["t_start"] == ms(0)
    assert row["t_end"] == ms(5)
    assert row["open_2"] is False
    assert row["captured_2"] == MC(0.0)


def test_metrics_exposes_every_capture_variant() -> None:
    f = frame(
        [ms(0), ms(1), ms(2), ms(3)],
        [0.0, 5.0, 5.0, -1.0],
        [100.0] * 4,
        compete=[0.0, 10.0, 0.0, 0.0],
        mid=[100.0, 100.0, 100.0, 101.0],
        dir=[1.0] * 4,
    )
    ep = detect_episodes(
        f,
        net_bps="net_bps",
        size_usd="size_usd",
        config=EpisodeConfig(merge_ms=0, latencies_ms=(1,), jitter=True),
        compete_usd="compete",
        mid="mid",
        direction="dir",
    )
    table = episode_metrics(ep, days=1, all_dates=[DAY1], latencies_ms=(1,))
    assert set(table["capture_variant"].to_list()) == {"naive", "adj", "jitter", "adj_jitter"}
    headline = table.filter(pl.col("capture_variant") == "adj_jitter").row(0, named=True)
    assert headline["usd_per_day"] == MC(
        float(ep["captured_adj_jitter_1"].sum()) / 1.0
    )


def test_zero_episodes_metrics_are_zero_not_absent() -> None:
    ep = detect_episodes(
        frame([ms(0), ms(1), ms(2)], [-1.0, -2.0, -3.0]),
        net_bps="net_bps",
        size_usd="size_usd",
        config=EpisodeConfig(merge_ms=0, latencies_ms=(1,)),
    )
    table = episode_metrics(ep, days=1, all_dates=[DAY1], capital_usd=10_000.0, latencies_ms=(1,))
    assert table.height == 1
    row = table.row(0, named=True)
    assert row["episodes"] == 0
    assert row["capture_rate"] == MC(0.0)
    assert row["usd_per_day"] == MC(0.0)
    assert row["apr"] == MC(0.0)
    assert row["usd_per_day_ci90_lo"] == MC(0.0)
    assert row["usd_per_day_ci90_hi"] == MC(0.0)


def test_all_dates_is_required_for_metrics_and_daily_capture() -> None:
    ep = detect_episodes(
        frame([ms(0), ms(1), ms(2)], [0.0, 5.0, -1.0]),
        net_bps="net_bps",
        size_usd="size_usd",
        config=EpisodeConfig(merge_ms=0, latencies_ms=(1,)),
    )
    with pytest.raises(EpisodeError, match="all_dates"):
        episode_metrics(ep, days=1)
    with pytest.raises(EpisodeError, match="all_dates"):
        daily_capture(ep, 1)


def test_all_dates_length_must_match_days() -> None:
    ep = detect_episodes(
        frame([ms(0), ms(1), ms(2)], [0.0, 5.0, -1.0]),
        net_bps="net_bps",
        size_usd="size_usd",
        config=EpisodeConfig(merge_ms=0, latencies_ms=(1,)),
    )
    with pytest.raises(EpisodeError, match="all_dates"):
        episode_metrics(ep, days=2, all_dates=[DAY1])


def test_episode_dates_must_be_in_all_dates() -> None:
    ep = detect_episodes(
        frame([ms(0), ms(1), ms(2)], [0.0, 5.0, -1.0]),
        net_bps="net_bps",
        size_usd="size_usd",
        config=EpisodeConfig(merge_ms=0, latencies_ms=(1,)),
    )
    with pytest.raises(EpisodeError, match="not in all_dates"):
        episode_metrics(ep, days=1, all_dates=[DAY2])


@pytest.mark.parametrize(
    "gaps",
    [
        [(0, 1000), (100, 200)],  # nested
        [(0, 1000), (400, 600)],  # the later gap ends before the earlier one
    ],
)
def test_overlapping_gaps_are_unioned(gaps: list[tuple[int, int]]) -> None:
    f = pl.DataFrame({"t_ns": [ms(700)], "last_a": [ms(700)]})
    gap_frame = pl.DataFrame(
        {
            "src": ["hl-ws"] * len(gaps),
            "conn": ["bbo"] * len(gaps),
            "start_ns": [ms(start) for start, _ in gaps],
            "end_ns": [ms(end) for _, end in gaps],
        }
    )
    out = feed_validity(
        f,
        [Feed("a", "last_a", "hl-ws", "bbo")],
        stale_ms=5000,
        gaps=gap_frame,
    )
    assert out["valid"].to_list() == [False]


def test_oos_split_is_chronological() -> None:
    dates = [_dt.date(2026, 1, day) for day in range(1, 11)]
    in_sample, oos = oos_split(list(reversed(dates)))
    assert in_sample == dates[:6]
    assert oos == dates[6:]
    assert oos_split([dates[0]]) == ([dates[0]], [])
    with pytest.raises(EpisodeError, match="frac"):
        oos_split(dates, frac=1.0)


# --------------------------------------------------------------------------
# Config validation
# --------------------------------------------------------------------------


@pytest.mark.parametrize(
    ("kwargs", "match"),
    [
        ({"stale_ms": 0}, "stale_ms"),
        ({"merge_ms": -1}, "merge_ms"),
        ({"max_notional": -1.0}, "max_notional"),
        ({"latencies_ms": ()}, "latencies_ms"),
    ],
)
def test_invalid_config_raises(kwargs: dict[str, object], match: str) -> None:
    with pytest.raises(EpisodeError, match=match):
        detect_episodes(
            frame([ms(0)], [1.0]),
            net_bps="net_bps",
            size_usd="size_usd",
            config=EpisodeConfig(**kwargs),
        )
