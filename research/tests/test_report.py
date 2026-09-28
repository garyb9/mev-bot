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
    main,
    parse_front_matter,
    render_front_matter,
    render_report,
    sampled_coverage_pct,
    top_episodes,
)
from hlr.thresholds import FAIL, MARGINAL, PASS, load_thresholds

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


def passing_episodes(
    days: int = 20,
    per_day: int = 12,
    *,
    mixed: bool = False,
    fidelity: str = "H1",
    net: float = 5.0,
    latencies: tuple[int, ...] = (100, 250, 500),
) -> pl.DataFrame:
    """A synthetic study dense enough to clear every §13.6 gate, with provenance."""
    times: list[int] = []
    net_bps: list[float] = []
    size: list[float] = []
    compete: list[float] = []
    sources: list[str] = []
    fidelities: list[str] = []
    for index in range(days):
        for slot in range(per_day):
            base = BASE_NS + index * DAY + slot * 10_000 * MS
            times += [base, base + 400 * MS, base + 2_000 * MS, base + 2_001 * MS]
            net_bps += [net, net, -1.0, -1.0]
            size += [5_000.0, 5_000.0, 5_000.0, 5_000.0]
            compete += [0.0, 0.0, 0.0, 0.0]
            source = "tardis" if mixed and index % 2 else "recorder"
            sources += [source, source, source, source]
            fidelities += [fidelity, fidelity, fidelity, fidelity]
    frame = pl.DataFrame(
        {
            "t_ns": times,
            "net_bps": net_bps,
            "size_usd": size,
            "compete_usd": compete,
            "source": sources,
            "fidelity": fidelities,
        }
    )
    config = EpisodeConfig(latencies_ms=latencies, jitter=True, jitter_seed=0)
    return detect_episodes(
        frame,
        net_bps="net_bps",
        size_usd="size_usd",
        config=config,
        compete_usd="compete_usd",
    )


def zero_capture(frame: pl.DataFrame, latency_ms: int) -> pl.DataFrame:
    """Zero every capture column at one latency (simulates the edge gone by then)."""
    columns = [
        name
        for name in frame.columns
        if name.startswith("captured") and name.endswith(f"_{latency_ms}")
    ]
    return frame.with_columns([pl.lit(0.0).alias(name) for name in columns])


def test_detect_episodes_carries_provenance() -> None:
    episodes = passing_episodes(days=1, per_day=1)
    assert "source" in episodes.columns
    assert set(episodes["source"].to_list()) == {"recorder"}
    assert set(episodes["fidelity"].to_list()) == {"H1"}
    # An input without the provenance columns stays without them.
    assert "source" not in synthetic_episodes([0]).columns


def test_recorder_study_with_fidelity_can_reach_pass_end_to_end(tmp_path: Path) -> None:
    episodes = passing_episodes(fidelity="H1")
    intervals = passing_episodes(fidelity="H1", net=3.0)  # genuinely different re-run
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
        robustness_runs={25_000.0: intervals},
        all_dates=[day(i) for i in range(20)],
        latencies_ms=(100, 250, 500),
        episode_parquet="O1-pass-episodes.parquet",
        reports_dir=reports,
    )
    (reports / "O1-pass.md").write_text(text)
    scanned = scan_reports(reports)
    assert scanned[0]["_digest_issue"] is None
    assert scanned[0]["data_source"] == "forward"
    assert scanned[0]["fidelity_class"] == "H1"
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


def test_fidelity_is_informational() -> None:
    episodes = passing_episodes(fidelity="H2")
    intervals = passing_episodes(fidelity="H2", net=3.0)
    values = build(
        episodes,
        days=20,
        all_dates=[day(i) for i in range(20)],
        latencies_ms=(100, 250, 500),
        max_notional={25_000.0: 5_000.0},
        robustness_runs={25_000.0: intervals},
    )
    assert values["data_source"] == "forward"
    assert values["fidelity_class"] == "H2"


def test_episode_date_outside_all_dates_raises() -> None:
    episodes = synthetic_episodes([0, 1, 2, 3, 4])
    with pytest.raises(ReportError, match="not in all_dates"):
        build(
            episodes,
            days=3,
            all_dates=[day(i) for i in range(3)],
            latencies_ms=(100, 250),
        )


# --------------------------------------------------------------------------
# B-9: HIST-PRELIM report plumbing
# --------------------------------------------------------------------------


def test_sparse_day_set_coverage_counts_only_sampled_days() -> None:
    """Coverage is the mean over the sampled days, not the whole calendar."""
    sampled = [day(0), day(30), day(60)]
    coverage = sampled_coverage_pct(
        sampled, {day(0): 1.0, day(30): 0.5, day(60): 1.0}
    )
    assert coverage == pytest.approx(2.5 / 3)
    # Three days sampled across a 61-day span still score 1.0 when fully valid:
    # the 58 unsampled days are gaps, not missing data.
    assert sampled_coverage_pct(sampled, {d: 1.0 for d in sampled}) == pytest.approx(1.0)

    episodes = synthetic_episodes([0, 30, 60])
    front_matter = build(
        episodes,
        days=3,
        all_dates=sampled,
        coverage_pct=None,
        day_coverage={day(0): 1.0, day(30): 0.5, day(60): 0.75},
        latencies_ms=(100, 250),
    )
    assert front_matter["days"] == 3
    assert front_matter["coverage_pct"] == pytest.approx(2.25 / 3)


def test_backfill_report_header_stamps_preliminary() -> None:
    episodes = passing_episodes(days=5, per_day=3, mixed=True)
    text = build_report(
        study_id="O2",
        slug="backfill",
        title="Backfill study",
        hypothesis="h",
        implementation_cost="M",
        days=5,
        coverage_pct=0.99,
        capital_runs={25_000.0: episodes},
        max_notional={25_000.0: 5_000.0},
        all_dates=[day(i) for i in range(5)],
        latencies_ms=(100, 250, 500),
    )
    assert "PRELIMINARY (backfill: tardis)" in text
    assert "HIST-PRELIM" in text

    # A forced backfill with no derivable source still stamps the lane.
    forced = build_report(
        study_id="O3",
        slug="forced",
        title="Forced backfill",
        hypothesis="h",
        implementation_cost="M",
        days=5,
        coverage_pct=0.99,
        capital_runs={25_000.0: synthetic_episodes([0, 1, 2, 3, 4])},
        max_notional={25_000.0: 5_000.0},
        all_dates=[day(i) for i in range(5)],
        latencies_ms=(100, 250),
        data_source="backfill",
    )
    assert "PRELIMINARY (backfill: unspecified)" in forced
    assert "HIST-PRELIM" in forced


def test_reveal_lag_zero_leaves_report_identical() -> None:
    episodes = synthetic_episodes([0, 1, 2, 3, 4])
    plain = build(episodes)
    zero = build(episodes, reveal_lag_ms=0)
    assert render_front_matter(plain) == render_front_matter(zero)
    assert render_report(plain) == render_report(zero)


def test_negative_or_non_numeric_reveal_lag_rejected() -> None:
    episodes = synthetic_episodes([0, 1, 2, 3, 4])
    for bad in (-1, "fast", 1.5, float("nan")):
        with pytest.raises(ReportError, match="reveal_lag_ms"):
            build(episodes, reveal_lag_ms=bad)


def test_sampled_day_with_no_episodes_counts_in_days_and_lowers_coverage() -> None:
    episodes = synthetic_episodes([0, 1, 2, 3])  # day 4 was sampled but had none
    sampled = [day(i) for i in range(5)]
    front_matter = build(
        episodes,
        days=5,
        all_dates=sampled,
        coverage_pct=None,
        day_coverage={day(0): 1.0, day(1): 1.0, day(2): 1.0, day(3): 1.0, day(4): 0.0},
        latencies_ms=(100, 250),
    )
    assert front_matter["days"] == 5
    assert front_matter["coverage_pct"] == pytest.approx(4.0 / 5.0)
    assert day(4).isoformat() in front_matter["in_sample_dates"] + front_matter["oos_dates"]


def test_day_coverage_rejects_unknown_sampled_day() -> None:
    sampled = [day(0), day(1)]
    with pytest.raises(ReportError, match="unsampled"):
        sampled_coverage_pct(sampled, {day(0): 1.0, day(1): 1.0, day(9): 0.0})
    with pytest.raises(ReportError, match="missing sampled day"):
        sampled_coverage_pct(sampled, {day(0): 1.0})
    episodes = synthetic_episodes([0, 1])
    with pytest.raises(ReportError, match="unsampled"):
        build(
            episodes,
            days=2,
            all_dates=sampled,
            coverage_pct=None,
            day_coverage={day(0): 1.0, day(1): 1.0, day(9): 0.0},
            latencies_ms=(100, 250),
        )


def test_reveal_lag_grades_data_computed_at_l_plus_lag() -> None:
    """A lag must move the graded cell to L+lag data, never relabel lower data."""
    latencies = (250, 480)
    episodes = zero_capture(passing_episodes(latencies=latencies), 480)
    robust = zero_capture(passing_episodes(net=3.0, latencies=latencies), 480)
    kwargs = {
        "days": 20,
        "all_dates": [day(i) for i in range(20)],
        "latencies_ms": (250,),
        "max_notional": {25_000.0: 5_000.0},
        "robustness_runs": {25_000.0: robust},
    }
    thresholds = load_thresholds()

    # L = 250 is positive, so without a lag the study is a PASS.
    unlagged = build(episodes, **kwargs)
    assert grade_study(unlagged, thresholds=thresholds).verdict == PASS

    # With a +230 ms lag the label 250 is graded on the data computed at 480,
    # which is zero: FAIL. The lag can only ever worsen (or hold) the verdict.
    lagged = build(episodes, reveal_lag_ms=230, **kwargs)
    assert lagged["latency_grid_ms"] == [250]
    assert lagged["effective_latency_grid_ms"] == [480]
    assert lagged["effective_headline_latency_ms"] == 480
    row = next(row for row in lagged["capital"][0]["oos"] if row["latency_ms"] == 250)
    assert row["effective_latency_ms"] == 480
    assert row["usd_per_day"] == pytest.approx(0.0)
    result = grade_study(lagged, thresholds=thresholds)
    assert result.verdict == FAIL
    assert result.headline_latency_ms == 250  # the label, not the effective latency
    assert result.apr == pytest.approx(0.0)


def test_reveal_lag_rejected_when_effective_columns_missing() -> None:
    """A table computed only at L cannot be lagged; refuse instead of relabelling."""
    episodes = passing_episodes(latencies=(250,))  # no 480 columns
    with pytest.raises(ReportError, match="effective"):
        build(
            episodes,
            days=20,
            all_dates=[day(i) for i in range(20)],
            latencies_ms=(250,),
            reveal_lag_ms=230,
            max_notional={25_000.0: 5_000.0},
        )


def test_top_episodes_use_effective_latency() -> None:
    latencies = (250, 480)
    episodes = zero_capture(passing_episodes(latencies=latencies), 480)
    kwargs = {
        "days": 20,
        "all_dates": [day(i) for i in range(20)],
        "latencies_ms": (250,),
        "max_notional": {25_000.0: 5_000.0},
    }
    unlagged = build(episodes, **kwargs)
    lagged = build(episodes, reveal_lag_ms=230, **kwargs)
    assert any(row["captured"] > 0 for row in unlagged["sections"]["top_episodes"])
    assert lagged["effective_headline_latency_ms"] == 480
    assert lagged["sections"]["top_episodes"]
    assert all(
        row["captured"] == pytest.approx(0.0) for row in lagged["sections"]["top_episodes"]
    )


def test_report_cli_rejects_negative_lag(tmp_path: Path) -> None:
    parquet = tmp_path / "O1-episodes.parquet"
    synthetic_episodes([0, 1, 2, 3, 4]).write_parquet(parquet)
    out = tmp_path / "O1.md"
    code = main(
        [
            "--study-id",
            "O1",
            "--slug",
            "s",
            "--title",
            "t",
            "--episodes",
            str(parquet),
            "--capital-usd",
            "25000",
            "--max-notional",
            "10000",
            "--coverage-pct",
            "0.99",
            "--reveal-lag-ms",
            "-1",
            "--out",
            str(out),
        ]
    )
    assert code != 0
    assert not out.exists()


def test_report_cli_backfill_requires_all_dates(tmp_path: Path) -> None:
    parquet = tmp_path / "O2-episodes.parquet"
    synthetic_episodes([0, 1, 2, 3, 4]).write_parquet(parquet)
    code = main(
        [
            "--study-id",
            "O2",
            "--slug",
            "backfill",
            "--title",
            "t",
            "--episodes",
            str(parquet),
            "--capital-usd",
            "25000",
            "--max-notional",
            "10000",
            "--coverage-pct",
            "0.99",
            "--data-source",
            "backfill",
            "--out",
            str(tmp_path / "O2.md"),
        ]
    )
    assert code != 0
    assert not (tmp_path / "O2.md").exists()


def test_report_cli_rejects_lag_without_effective_columns(tmp_path: Path) -> None:
    parquet = tmp_path / "O3-episodes.parquet"
    synthetic_episodes([0, 1, 2, 3, 4]).write_parquet(parquet)
    code = main(
        [
            "--study-id",
            "O3",
            "--slug",
            "lag",
            "--title",
            "t",
            "--episodes",
            str(parquet),
            "--capital-usd",
            "25000",
            "--max-notional",
            "10000",
            "--coverage-pct",
            "0.99",
            "--all-dates",
            "2024-01-01,2024-01-02,2024-01-03,2024-01-04,2024-01-05",
            "--reveal-lag-ms",
            "150",
            "--out",
            str(tmp_path / "O3.md"),
        ]
    )
    assert code != 0  # default grid's effective columns are absent from the table


def test_report_cli_builds_backfill_report(tmp_path: Path) -> None:
    parquet = tmp_path / "O2-episodes.parquet"
    synthetic_episodes([0, 1, 2, 3, 4]).write_parquet(parquet)
    out = tmp_path / "O2.md"
    code = main(
        [
            "--study-id",
            "O2",
            "--slug",
            "backfill",
            "--title",
            "t",
            "--episodes",
            str(parquet),
            "--capital-usd",
            "25000",
            "--max-notional",
            "10000",
            "--coverage-pct",
            "0.99",
            "--data-source",
            "backfill",
            "--all-dates",
            "2024-01-01,2024-01-02,2024-01-03,2024-01-04,2024-01-05",
            "--latency-ms",
            "100",
            "--headline-latency-ms",
            "100",
            "--reveal-lag-ms",
            "150",
            "--out",
            str(out),
        ]
    )
    assert code == 0
    text = out.read_text()
    assert "PRELIMINARY (backfill:" in text
    assert "HIST-PRELIM" in text
    # Label 100 is graded on the data computed at its effective latency 250.
    assert '"latency_grid_ms": [' in text
    assert '"effective_latency_grid_ms": [' in text
    assert '"effective_headline_latency_ms": 250' in text
    assert '"reveal_lag_ms": 150' in text
