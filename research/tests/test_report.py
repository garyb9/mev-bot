"""Tests for :mod:`hlr.report` (SPEC-0008 P-5, §13.5–§13.7).

The rendering tests build a synthetic episode table through the P-4 detector so
the report sees the exact columns a study would produce. Nothing touches the
network or ``research/data/``.
"""

from __future__ import annotations

import datetime as _dt
from pathlib import Path

import polars as pl
import pytest

from hlr.episodes import EpisodeConfig, detect_episodes
from hlr.rank import scan_reports
from hlr.report import (
    HIST_PRELIM,
    ReportError,
    build_front_matter,
    build_report,
    canonical_digest,
    episode_digest,
    grade_study,
    parse_front_matter,
    render_front_matter,
    render_report,
    top_episodes,
)
from hlr.thresholds import MARGINAL, PASS, load_thresholds

MS = 1_000_000
DAY = 86_400_000 * MS  # milliseconds per day -> nanoseconds
#: Nanoseconds at 2024-01-01T00:00:00Z, so episodes land on the ``all_dates`` days.
BASE_NS = int(_dt.datetime(2024, 1, 1, tzinfo=_dt.UTC).timestamp()) * 1_000_000_000


def day(index: int) -> _dt.date:
    return _dt.date(2024, 1, 1) + _dt.timedelta(days=index)


def synthetic_episodes(day_indices: list[int]) -> pl.DataFrame:
    """One clean, ~2 s episode per day, with a competition column for adj_jitter."""
    times: list[int] = []
    net: list[float] = []
    size: list[float] = []
    compete: list[float] = []
    for index in day_indices:
        base = BASE_NS + index * DAY
        times += [base, base + 400 * MS, base + 2_000 * MS, base + 2_001 * MS]
        net += [5.0, 5.0, -1.0, -1.0]
        size += [1_000.0, 1_000.0, 1_000.0, 1_000.0]
        compete += [0.0, 0.0, 0.0, 0.0]
    frame = pl.DataFrame(
        {"t_ns": times, "net_bps": net, "size_usd": size, "compete_usd": compete}
    )
    config = EpisodeConfig(latencies_ms=(100, 250), jitter=True, jitter_seed=0)
    return detect_episodes(
        frame,
        net_bps="net_bps",
        size_usd="size_usd",
        config=config,
        compete_usd="compete_usd",
    )


def test_detector_emits_headline_columns() -> None:
    episodes = synthetic_episodes([0, 1, 2, 3, 4])
    for column in (
        "captured_250",
        "captured_adj_jitter_250",
        "open_jitter_250",
        "date",
    ):
        assert column in episodes.columns
    assert episodes.height == 5


def test_front_matter_round_trips() -> None:
    episodes = synthetic_episodes([0, 1, 2, 3, 4])
    front_matter = build_front_matter(
        study_id="O1",
        slug="hip3",
        title="HIP-3 dislocations",
        hypothesis="Same underlying, two dexes.",
        implementation_cost="M",
        days=5,
        coverage_pct=0.99,
        capital_runs={25_000.0: episodes},
        max_notional={25_000.0: 10_000.0},
        all_dates=[day(i) for i in range(5)],
        latencies_ms=(100, 250),
    )
    text = render_front_matter(front_matter)
    assert text.startswith("---\n") and text.rstrip().endswith("---")
    assert parse_front_matter(text + "\nbody\n") == front_matter


def test_build_report_has_all_seven_sections() -> None:
    episodes = synthetic_episodes([0, 1, 2, 3, 4])
    text = build_report(
        study_id="O1",
        slug="hip3",
        title="HIP-3 dislocations",
        hypothesis="Same underlying, two dexes.",
        implementation_cost="M",
        days=5,
        coverage_pct=0.99,
        capital_runs={25_000.0: episodes},
        max_notional={25_000.0: 10_000.0},
        all_dates=[day(i) for i in range(5)],
        latencies_ms=(100, 250),
        sections={"data": {"range": "2024-01-01..05"}, "reproduce": "uv run python -m studies.o1"},
    )
    for heading in (
        "## 1. Hypothesis",
        "## 2. Data",
        "## 3. Method",
        "## 4. Results",
        "## 5. Sanity checks",
        "## 6. Verdict",
        "## 7. Reproduce",
    ):
        assert heading in text
    assert "### Latency grid" in text
    assert "### Capital grid" in text
    assert "### Top-10 episodes" in text
    parsed = parse_front_matter(text)
    assert parsed["study_id"] == "O1"


def test_front_matter_carries_oos_and_in_sample_rows() -> None:
    episodes = synthetic_episodes([0, 1, 2, 3, 4])
    front_matter = build_front_matter(
        study_id="O1",
        slug="hip3",
        title="HIP-3 dislocations",
        hypothesis="Same underlying, two dexes.",
        implementation_cost="M",
        days=5,
        coverage_pct=0.99,
        capital_runs={25_000.0: episodes},
        max_notional={25_000.0: 10_000.0},
        all_dates=[day(i) for i in range(5)],
        latencies_ms=(100, 250),
    )
    block = front_matter["capital"][0]
    assert block["in_sample"] and block["oos"]
    assert {row["latency_ms"] for row in block["oos"]} == {100, 250}
    assert {row["capture_variant"] for row in block["oos"]} >= {"naive", "adj_jitter"}
    row = next(
        row
        for row in block["oos"]
        if row["latency_ms"] == 250 and row["capture_variant"] == "adj_jitter"
    )
    assert row["usd_per_day"] > 0.0
    assert "apr" in row and "apr_ci90_lo" in row


def test_render_report_requires_front_matter() -> None:
    episodes = synthetic_episodes([0])
    front_matter = build_front_matter(
        study_id="O1",
        slug="hip3",
        title="t",
        hypothesis="h",
        implementation_cost="M",
        days=1,
        coverage_pct=1.0,
        capital_runs={25_000.0: episodes},
        max_notional={25_000.0: 10_000.0},
        all_dates=[day(0)],
        latencies_ms=(250,),
    )
    text = render_report(front_matter)
    assert text.startswith("---")


def test_top_episodes_sorted_by_capture() -> None:
    episodes = synthetic_episodes([0, 1, 2])
    rows = top_episodes(episodes, 250, "adj_jitter", n=10)
    captured = [row["captured"] for row in rows]
    assert captured == sorted(captured, reverse=True)
    assert len(rows) == 3


def build(episodes: pl.DataFrame, **overrides: object) -> dict[str, object]:
    """Build an O1/O1 front-matter over five days, with overridable inputs."""
    arguments: dict[str, object] = {
        "study_id": "O1",
        "slug": "hip3",
        "title": "t",
        "hypothesis": "h",
        "implementation_cost": "M",
        "days": 5,
        "coverage_pct": 0.99,
        "capital_runs": {25_000.0: episodes},
        "max_notional": {25_000.0: 10_000.0},
        "all_dates": [day(i) for i in range(5)],
        "latencies_ms": (100, 250),
    }
    arguments.update(overrides)
    return build_front_matter(**arguments)  # type: ignore[arg-type]


def test_provenance_derived_from_source_column() -> None:
    recorder = synthetic_episodes([0, 1, 2, 3, 4]).with_columns(
        pl.lit("recorder").alias("source")
    )
    assert build(recorder)["data_source"] == "forward"

    backfill = synthetic_episodes([0, 1, 2, 3, 4]).with_columns(
        pl.lit("tardis").alias("source")
    )
    front_matter = build(backfill)
    assert front_matter["data_source"] == "backfill"
    assert front_matter["backfill_sources"] == ["tardis"]

    # No source column means provenance cannot be proven forward.
    assert build(synthetic_episodes([0, 1, 2, 3, 4]))["data_source"] == "unknown"


def test_digest_is_stable_and_content_sensitive() -> None:
    episodes = synthetic_episodes([0, 1, 2])
    digest = episode_digest({25_000.0: episodes})
    assert digest == episode_digest({25_000.0: episodes.clone()})
    assert canonical_digest(episodes.select(sorted(episodes.columns))) == canonical_digest(
        episodes
    )
    assert episode_digest({25_000.0: synthetic_episodes([0, 1, 2, 3])}) != digest
    assert build(episodes)["episode_digest"] == digest


def test_max_notional_is_required_and_asserted() -> None:
    episodes = synthetic_episodes([0, 1, 2, 3, 4])
    with pytest.raises(ReportError, match="max_notional"):
        build(episodes, max_notional={})
    with pytest.raises(ReportError, match="max_notional"):
        build(episodes, max_notional={25_000.0: 500.0})


def test_missing_date_column_raises() -> None:
    episodes = synthetic_episodes([0, 1, 2, 3, 4]).drop("date")
    with pytest.raises(ReportError, match="date"):
        build(episodes)


def passing_episodes(days: int = 20, per_day: int = 12, *, mixed: bool = False) -> pl.DataFrame:
    """A synthetic study dense enough to clear every §13.6 gate, with provenance."""
    times: list[int] = []
    net: list[float] = []
    size: list[float] = []
    compete: list[float] = []
    sources: list[str] = []
    for index in range(days):
        for slot in range(per_day):
            base = BASE_NS + index * DAY + slot * 10_000 * MS
            times += [base, base + 400 * MS, base + 2_000 * MS, base + 2_001 * MS]
            net += [5.0, 5.0, -1.0, -1.0]
            size += [5_000.0, 5_000.0, 5_000.0, 5_000.0]
            compete += [0.0, 0.0, 0.0, 0.0]
            source = "tardis" if mixed and index % 2 else "recorder"
            sources += [source, source, source, source]
    frame = pl.DataFrame(
        {
            "t_ns": times,
            "net_bps": net,
            "size_usd": size,
            "compete_usd": compete,
            "source": sources,
        }
    )
    config = EpisodeConfig(latencies_ms=(100, 250, 500), jitter=True, jitter_seed=0)
    return detect_episodes(
        frame,
        net_bps="net_bps",
        size_usd="size_usd",
        config=config,
        compete_usd="compete_usd",
    )


def test_detect_episodes_carries_provenance() -> None:
    episodes = passing_episodes(days=1, per_day=1)
    assert "source" in episodes.columns
    assert set(episodes["source"].to_list()) == {"recorder"}
    # An input without the provenance columns stays without them.
    assert "source" not in synthetic_episodes([0]).columns


def test_recorder_study_can_reach_pass_end_to_end(tmp_path: Path) -> None:
    episodes = passing_episodes()
    reports = tmp_path / "reports"
    text = build_report(
        study_id="O1",
        slug="pass",
        title="Pass",
        hypothesis="h",
        implementation_cost="M",
        days=20,
        coverage_pct=0.99,
        capital_runs={25_000.0: episodes},
        max_notional={25_000.0: 5_000.0},
        robustness_runs={25_000.0: episodes},
        all_dates=[day(i) for i in range(20)],
        latencies_ms=(100, 250, 500),
        episode_parquet="O1-pass-episodes.parquet",
        reports_dir=reports,
    )
    (reports / "O1-pass.md").write_text(text)
    scanned = scan_reports(reports)
    assert scanned[0]["_digest_issue"] is None
    assert scanned[0]["data_source"] == "forward"
    assert grade_study(scanned[0], thresholds=load_thresholds()).verdict == PASS


def test_mixed_provenance_cannot_pass_end_to_end() -> None:
    episodes = passing_episodes(mixed=True)
    values = build(
        episodes,
        days=20,
        all_dates=[day(i) for i in range(20)],
        latencies_ms=(100, 250, 500),
        max_notional={25_000.0: 5_000.0},
        robustness_runs={25_000.0: episodes},
    )
    assert values["data_source"] == "backfill"
    result = grade_study(values, thresholds=load_thresholds())
    assert result.verdict == MARGINAL
    assert result.qualifier is not None and result.qualifier.startswith(HIST_PRELIM)


def test_provenance_checks_robustness_tables() -> None:
    episodes = synthetic_episodes([0, 1, 2, 3, 4]).with_columns(
        pl.lit("recorder").alias("source")
    )
    without_source = synthetic_episodes([0, 1, 2, 3, 4])
    values = build(episodes, robustness_runs={25_000.0: without_source})
    assert values["data_source"] == "unknown"
