"""Tests for :mod:`hlr.report` grading and :mod:`hlr.rank` (SPEC-0008 P-5, §13.6).

Grades are driven by hand-built front-matter so every boundary is exact; the
re-grade and end-to-end tests write reports under ``tmp_path`` and re-read them.
Nothing touches the network or ``research/data/``.
"""

from __future__ import annotations

import datetime as _dt
from pathlib import Path
from typing import Any

import polars as pl
import pytest

from hlr.rank import build_ranking, main, scan_reports
from hlr.report import (
    HIST_PRELIM,
    INCONCLUSIVE,
    ReportError,
    _check_day_counts,
    canonical_episodes,
    episode_digest,
    grade_study,
    rank_studies,
    render_front_matter,
    render_ranking,
    render_report,
)
from hlr.thresholds import FAIL, MARGINAL, PASS, Thresholds, load_thresholds

_TEMPLATE = """\
[capital]
grid_usd = [10_000, 25_000, 50_000, 100_000]
headline_usd = {headline_usd}
[apr]
target = {target}
floor = {floor}
[quality]
min_episodes_per_day = {min_episodes_per_day}
max_concentration = {max_concentration}
min_coverage_pct = {min_coverage_pct}
robustness_buffer_multiplier = 2.0
[latency]
headline_ms = {headline_ms}
[pilot]
max_capital_usd = 10_000
max_weeks = 4
max_loss_usd = 1_000
"""


def thresholds_file(tmp_path: Path, **overrides: Any) -> Path:
    values = {
        "headline_usd": 25_000,
        "target": 0.25,
        "floor": 0.10,
        "min_episodes_per_day": 10,
        "max_concentration": 0.40,
        "min_coverage_pct": 0.90,
        "headline_ms": 250,
    }
    values.update(overrides)
    path = tmp_path / "thresholds.toml"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(_TEMPLATE.format(**values))
    return path


def thresholds(tmp_path: Path, **overrides: Any) -> Thresholds:
    return load_thresholds(thresholds_file(tmp_path, **overrides))


def metric_row(
    latency: int,
    variant: str,
    *,
    apr: float,
    ci_lo: float,
    usd: float = 100.0,
    epd: float = 20.0,
    concentration: float | None = 0.20,
    robust: float | None = 80.0,
) -> dict[str, Any]:
    return {
        "latency_ms": latency,
        "capture_variant": variant,
        "apr": apr,
        "apr_ci90_lo": ci_lo,
        "apr_ci90_hi": apr + 0.05,
        "usd_per_day": usd,
        "usd_per_day_robust": robust,
        "usd_per_day_ci90_lo": usd * 0.5,
        "usd_per_day_ci90_hi": usd * 1.5,
        "episodes": int(epd * 20),
        "episodes_per_day": epd,
        "episodes_per_day_p50": epd,
        "episodes_per_day_p90": epd * 1.5,
        "capture_rate": 1.0,
        "duration_ms_p50": 500.0,
        "duration_ms_p90": 900.0,
        "peak_net_bps_p50": 5.0,
        "peak_net_bps_p90": 8.0,
        "markout_1s_median": 0.0,
        "markout_10s_median": -1.0,
        "competition_hint": "mixed",
        "concentration": concentration,
    }


def make_fm(
    study_id: str = "O1",
    *,
    apr: float = 0.30,
    ci_lo: float = 0.30,
    usd: float = 100.0,
    epd: float = 20.0,
    concentration: float | None = 0.20,
    robust: float | None = 80.0,
    coverage: float = 0.99,
    days: int = 20,
    preliminary: bool = False,
    data_source: str = "forward",
    backfill_sources: list[str] | None = None,
    fidelity_class: str | None = None,
    implementation_cost: str = "M",
    latency: int = 250,
    latencies: tuple[int, ...] | None = None,
    capital: float = 25_000.0,
    naive_apr: float = 0.0,
    variants: tuple[str, ...] = ("naive", "adj_jitter"),
    headline_variant: str = "adj_jitter",
    title: str = "Synthetic study",
) -> dict[str, Any]:
    grid = latencies if latencies is not None else (latency,)
    oos: list[dict[str, Any]] = []
    in_sample: list[dict[str, Any]] = []
    for lat in grid:
        for variant in variants:
            headline = variant == headline_variant
            row_apr = apr if headline else naive_apr
            row_ci = ci_lo if headline else naive_apr
            usd_value = usd if headline else usd * 0.5
            row_robust = robust if headline else None
            oos.append(
                metric_row(
                    lat,
                    variant,
                    apr=row_apr,
                    ci_lo=row_ci,
                    usd=usd_value,
                    epd=epd,
                    concentration=concentration,
                    robust=row_robust,
                )
            )
            in_sample.append(
                metric_row(
                    lat,
                    variant,
                    apr=row_apr,
                    ci_lo=row_ci,
                    usd=usd_value,
                    epd=epd,
                    concentration=concentration,
                    robust=row_robust,
                )
            )
    in_days = max(0, days - max(1, days - int(days * 0.6)))
    date_strings = [f"2024-01-{index + 1:02d}" for index in range(days)]
    return {
        "spec": "SPEC-0008",
        "study_id": study_id,
        "slug": study_id.lower(),
        "title": title,
        "hypothesis": "h",
        "implementation_cost": implementation_cost,
        "days": days,
        "coverage_pct": coverage,
        "prereg_sha": "",
        "cells_K": 1,
        "preliminary": preliminary,
        "data_source": data_source,
        "backfill_sources": list(backfill_sources)
        if backfill_sources is not None
        else (["tardis"] if data_source == "backfill" else []),
        "fidelity_class": fidelity_class
        if fidelity_class is not None
        else ("H1" if data_source == "backfill" else None),
        "headline_latency_ms": latency,
        "headline_variant": headline_variant,
        "headline_capital_usd": capital,
        "latency_grid_ms": list(grid),
        "capture_variants": list(variants),
        "in_sample_dates": date_strings[:in_days],
        "oos_dates": date_strings[in_days:],
        "capital": [
            {
                "capital_usd": capital,
                "max_notional": 10_000.0,
                "in_sample": in_sample,
                "oos": oos,
            }
        ],
        "sections": {},
    }


def attach_episode_file(
    front_matter: dict[str, Any], directory: Path, *, source: str | None = None
) -> str:
    """Write a parquet whose counts and dates match ``front_matter`` and set its digest."""
    directory.mkdir(parents=True, exist_ok=True)
    parquet_name = f"{front_matter['study_id']}-{front_matter['slug']}-episodes.parquet"
    declared_values = list(front_matter.get("in_sample_dates", [])) + list(
        front_matter.get("oos_dates", [])
    )
    declared = [_dt.date.fromisoformat(value) for value in declared_values]
    resolved = source if source is not None else front_matter.get("data_source")
    runs: dict[float, pl.DataFrame] = {}
    for block in front_matter["capital"]:
        capital = float(block["capital_usd"])
        expected = 0
        for sample in ("in_sample", "oos"):
            rows = block.get(sample) or []
            if rows:
                expected += int(rows[0].get("episodes", 0))
        dates = [declared[index % len(declared)] for index in range(expected)]
        frame = pl.DataFrame({"date": dates})
        if resolved in ("forward", "recorder"):
            frame = frame.with_columns(pl.lit("recorder").alias("source"))
        elif resolved in ("backfill", "tardis"):
            frame = frame.with_columns(pl.lit("tardis").alias("source"))
        runs[capital] = frame
    front_matter["episode_parquet"] = parquet_name
    front_matter["episode_digest"] = episode_digest(runs)
    canonical_episodes(runs).write_parquet(directory / parquet_name)
    return parquet_name


def write_reports(directory: Path, *front_matters: dict[str, Any]) -> None:
    directory.mkdir(parents=True, exist_ok=True)
    for front_matter in front_matters:
        attach_episode_file(front_matter, directory)
        text = render_front_matter(front_matter) + "\n# body\n"
        (directory / f"{front_matter['study_id']}-{front_matter['slug']}.md").write_text(text)


# --------------------------------------------------------------------------
# Grading boundaries
# --------------------------------------------------------------------------


@pytest.mark.parametrize(
    ("apr", "ci_lo", "expected"),
    [
        (0.50, 0.50, PASS),
        (0.25, 0.10, PASS),  # point == target, CI lower == floor
        (0.30, 0.08, MARGINAL),  # point >= target but CI lower < floor
        (0.20, 0.20, MARGINAL),
        (0.10, 0.10, MARGINAL),  # point == floor
        (0.099, 0.05, FAIL),
        (0.0, 0.0, FAIL),
    ],
)
def test_grading_boundaries(tmp_path: Path, apr: float, ci_lo: float, expected: str) -> None:
    result = grade_study(make_fm(apr=apr, ci_lo=ci_lo), thresholds=thresholds(tmp_path))
    assert result.verdict == expected


@pytest.mark.parametrize(
    "overrides",
    [
        {"epd": 5.0},  # episodes/day below the gate
        {"concentration": 0.5},  # concentration above the gate
        {"coverage": 0.5},  # coverage below the gate
        {"concentration": None},  # concentration could not be computed
    ],
)
def test_quality_failure_is_fail(tmp_path: Path, overrides: dict[str, Any]) -> None:
    result = grade_study(
        make_fm(apr=0.9, ci_lo=0.9, **overrides), thresholds=thresholds(tmp_path)
    )
    assert result.verdict == FAIL


def test_too_few_days_is_inconclusive(tmp_path: Path) -> None:
    result = grade_study(make_fm(days=10), thresholds=thresholds(tmp_path))
    assert result.verdict == INCONCLUSIVE


def test_preliminary_cannot_pass(tmp_path: Path) -> None:
    result = grade_study(
        make_fm(days=3, preliminary=True, apr=0.5, ci_lo=0.5), thresholds=thresholds(tmp_path)
    )
    assert result.verdict == MARGINAL
    assert any("preliminary" in reason for reason in result.reasons)


@pytest.mark.parametrize("robust", [None, 0.0, -1.0])
def test_non_robust_is_fail(tmp_path: Path, robust: float | None) -> None:
    result = grade_study(
        make_fm(apr=0.5, ci_lo=0.5, robust=robust), thresholds=thresholds(tmp_path)
    )
    assert result.verdict == FAIL
    assert any("robustness" in reason for reason in result.reasons)


# --------------------------------------------------------------------------
# Headline variant and HIST-PRELIM
# --------------------------------------------------------------------------


def test_grades_headline_variant_not_naive(tmp_path: Path) -> None:
    passing = grade_study(
        make_fm(apr=0.30, ci_lo=0.30, naive_apr=0.01), thresholds=thresholds(tmp_path)
    )
    assert passing.verdict == PASS
    assert passing.apr == pytest.approx(0.30)
    assert passing.naive_apr == pytest.approx(0.01)

    failing = grade_study(
        make_fm(apr=0.05, ci_lo=0.05, naive_apr=0.50), thresholds=thresholds(tmp_path)
    )
    assert failing.verdict == FAIL
    assert failing.apr == pytest.approx(0.05)
    assert failing.naive_apr == pytest.approx(0.50)


def test_missing_headline_variant_is_inconclusive(tmp_path: Path) -> None:
    result = grade_study(
        make_fm(variants=("naive",), headline_variant="naive", apr=0.90, ci_lo=0.90),
        thresholds=thresholds(tmp_path),
    )
    assert result.verdict == INCONCLUSIVE
    assert any("adj_jitter" in reason for reason in result.reasons)


def test_hist_prelim_cannot_pass(tmp_path: Path) -> None:
    result = grade_study(
        make_fm(apr=0.60, ci_lo=0.60, data_source="backfill"),
        thresholds=thresholds(tmp_path),
    )
    assert result.verdict == MARGINAL
    assert result.qualifier is not None
    assert result.qualifier.startswith(HIST_PRELIM)
    assert any("HIST-PRELIM" in reason for reason in result.reasons)


def test_forward_backfill_sources_demote_but_fidelity_does_not(tmp_path: Path) -> None:
    th = thresholds(tmp_path)
    assert grade_study(make_fm(apr=0.6, ci_lo=0.6), thresholds=th).verdict == PASS
    assert (
        grade_study(make_fm(apr=0.6, ci_lo=0.6, backfill_sources=["tardis"]), thresholds=th).verdict
        == MARGINAL
    )
    assert (
        grade_study(make_fm(apr=0.6, ci_lo=0.6, data_source="unknown"), thresholds=th).verdict
        == MARGINAL
    )
    # A fidelity class is informational and does not demote a recorder study.
    assert (
        grade_study(make_fm(apr=0.6, ci_lo=0.6, fidelity_class="H2"), thresholds=th).verdict
        == PASS
    )


def test_hist_prelim_can_still_fail(tmp_path: Path) -> None:
    result = grade_study(
        make_fm(apr=0.05, ci_lo=0.05, data_source="backfill"),
        thresholds=thresholds(tmp_path),
    )
    assert result.verdict == FAIL


# --------------------------------------------------------------------------
# Ranking order, zero episodes, determinism
# --------------------------------------------------------------------------


def test_ranking_order_tiers(tmp_path: Path) -> None:
    th = thresholds(tmp_path)
    passing = make_fm("O1", apr=0.5, ci_lo=0.5)
    marginal = make_fm("O2", apr=0.2, ci_lo=0.2)
    inconclusive = make_fm("O3", days=5)
    failing = make_fm("O4", apr=0.05, ci_lo=0.05)
    ranked = rank_studies([failing, inconclusive, marginal, passing], thresholds=th)
    assert [result.study_id for result in ranked] == ["O1", "O2", "O3", "O4"]
    assert [result.verdict for result in ranked] == [PASS, MARGINAL, INCONCLUSIVE, FAIL]


def test_hist_prelim_groups_below_forward(tmp_path: Path) -> None:
    th = thresholds(tmp_path)
    forward = make_fm("O1", apr=0.2, ci_lo=0.2, title="forward")
    backfill = make_fm(
        "O2", apr=0.2, ci_lo=0.2, data_source="backfill", title="backfill"
    )
    ranked = rank_studies([backfill, forward], thresholds=th)
    assert [result.study_id for result in ranked] == ["O1", "O2"]
    both = [result.verdict for result in ranked]
    assert both == [MARGINAL, MARGINAL]


def test_score_then_impl_cost_tiebreak(tmp_path: Path) -> None:
    th = thresholds(tmp_path)
    cheap = make_fm("O1", apr=0.5, ci_lo=0.5, usd=100.0, concentration=0.2, implementation_cost="S")
    pricey = make_fm("O2", apr=0.5, ci_lo=0.5, usd=100.0, concentration=0.2, implementation_cost="L")
    ranked = rank_studies([pricey, cheap], thresholds=th)
    assert [result.study_id for result in ranked] == ["O1", "O2"]
    assert ranked[0].score == pytest.approx(80.0)


def test_zero_episode_study_scores_fail_not_missing(tmp_path: Path) -> None:
    th = thresholds(tmp_path)
    empty = make_fm("O9", apr=0.0, ci_lo=0.0, usd=0.0, epd=0.0, concentration=0.0)
    ranked = rank_studies([empty], thresholds=th)
    assert len(ranked) == 1
    assert ranked[0].verdict == FAIL
    assert ranked[0].score == pytest.approx(0.0)
    text = render_ranking(ranked, th)
    assert "O9" in text
    assert "FAIL" in text


def test_headline_latency_rounds_up_to_grid(tmp_path: Path) -> None:
    th = thresholds(tmp_path, headline_ms=250)
    result = grade_study(make_fm(latencies=(100, 500)), thresholds=th)
    assert result.headline_latency_ms == 500


def test_headline_latency_above_grid_is_inconclusive(tmp_path: Path) -> None:
    th = thresholds(tmp_path, headline_ms=2_000)
    result = grade_study(make_fm(latencies=(100, 500)), thresholds=th)
    assert result.verdict == INCONCLUSIVE
    assert any("grid latency" in reason for reason in result.reasons)


def test_headline_capital_must_be_exact(tmp_path: Path) -> None:
    th = thresholds(tmp_path, headline_usd=30_000)
    result = grade_study(make_fm(capital=25_000.0), thresholds=th)
    assert result.verdict == INCONCLUSIVE
    assert result.headline_capital_usd is None
    assert any("headline capital" in reason for reason in result.reasons)


def test_days_must_equal_sample_dates(tmp_path: Path) -> None:
    front_matter = make_fm(days=20)
    front_matter["oos_dates"] = front_matter["oos_dates"][:-1]
    result = grade_study(front_matter, thresholds=thresholds(tmp_path))
    assert result.verdict == INCONCLUSIVE
    assert any("in_sample_dates" in reason for reason in result.reasons)


def test_check_day_counts_rejects_bad_splits(tmp_path: Path) -> None:
    del tmp_path  # the validator does not need thresholds
    missing = make_fm(days=20)
    del missing["oos_dates"]
    with pytest.raises(ReportError, match="oos_dates"):
        _check_day_counts(missing, 20)

    duplicated = make_fm(days=20)
    duplicated["oos_dates"] = duplicated["oos_dates"][:-1] + [duplicated["oos_dates"][0]]
    with pytest.raises(ReportError, match="duplicate"):
        _check_day_counts(duplicated, 20)

    # One date in both lists, but the total still equals `days`: hits overlap.
    overlapping = make_fm(days=20)
    overlapping["oos_dates"] = [
        overlapping["in_sample_dates"][-1],
        *overlapping["oos_dates"][1:],
    ]
    with pytest.raises(ReportError, match="overlap"):
        _check_day_counts(overlapping, 20)

    reversed_split = make_fm(days=20)
    reversed_split["in_sample_dates"], reversed_split["oos_dates"] = (
        reversed_split["oos_dates"],
        reversed_split["in_sample_dates"],
    )
    with pytest.raises(ReportError, match="chronological"):
        _check_day_counts(reversed_split, 20)


def test_one_bad_report_does_not_abort_ranking(tmp_path: Path) -> None:
    th = thresholds(tmp_path)
    good = make_fm("O1", apr=0.5, ci_lo=0.5)
    bad = make_fm("O2", apr=0.5, ci_lo=0.5)
    bad["oos_dates"] = []  # empty list -> INCONCLUSIVE, not an exception
    ranked = rank_studies([bad, good], thresholds=th)
    assert [result.study_id for result in ranked] == ["O1", "O2"]
    assert ranked[0].verdict == PASS
    assert ranked[1].verdict == INCONCLUSIVE
    assert ranked[1].reasons


def test_malformed_capital_block_is_inconclusive(tmp_path: Path) -> None:
    th = thresholds(tmp_path)
    front_matter = make_fm(days=20)
    front_matter["capital"][0]["capital_usd"] = "not a number"
    result = grade_study(front_matter, thresholds=th)
    assert result.verdict == INCONCLUSIVE
    assert any("capital_usd" in reason for reason in result.reasons)


def test_rank_survives_malformed_capital_block(tmp_path: Path) -> None:
    reports = tmp_path / "reports"
    front_matter = make_fm("O1", apr=0.5, ci_lo=0.5)
    attach_episode_file(front_matter, reports)  # digest matches the parquet
    front_matter["capital"][0]["capital_usd"] = "not a number"
    (reports / "O1-o1.md").write_text(render_front_matter(front_matter) + "\n# body\n")
    result = grade_study(scan_reports(reports)[0], thresholds=thresholds(tmp_path))
    assert result.verdict == INCONCLUSIVE


def test_render_report_when_headline_capital_not_run(tmp_path: Path) -> None:
    front_matter = make_fm(capital=30_000.0)  # default headline capital is 25k
    result = grade_study(front_matter, thresholds=thresholds(tmp_path))
    assert result.verdict == INCONCLUSIVE
    assert result.headline_capital_usd is None
    text = render_report(front_matter)  # must not raise on a None headline capital
    assert "INCONCLUSIVE" in text


# --------------------------------------------------------------------------
# End-to-end: re-grade, scan, CLI, determinism
# --------------------------------------------------------------------------


def test_regrade_after_thresholds_change(tmp_path: Path) -> None:
    reports = tmp_path / "reports"
    write_reports(reports, make_fm("O1", apr=0.20, ci_lo=0.20))
    loose = thresholds_file(tmp_path, target=0.25, floor=0.10)
    strict = thresholds_file(tmp_path / "strict", target=0.25, floor=0.22)
    first = rank_studies(scan_reports(reports), thresholds=load_thresholds(loose))
    second = rank_studies(scan_reports(reports), thresholds=load_thresholds(strict))
    assert first[0].verdict == MARGINAL
    assert second[0].verdict == FAIL

    text_a, count_a = build_ranking(reports, loose)
    text_b, count_b = build_ranking(reports, strict)
    assert count_a == count_b == 1
    assert text_a != text_b


def test_regrade_after_headline_latency_change(tmp_path: Path) -> None:
    front_matter = make_fm(latencies=(100, 500), apr=0.50, ci_lo=0.50)
    block = front_matter["capital"][0]
    for row in [*block["oos"], *block["in_sample"]]:
        if row["latency_ms"] == 500 and row["capture_variant"] == "adj_jitter":
            row["apr"] = 0.05
            row["apr_ci90_lo"] = 0.05
    fast = grade_study(front_matter, thresholds=thresholds(tmp_path, headline_ms=100))
    slow = grade_study(front_matter, thresholds=thresholds(tmp_path, headline_ms=500))
    assert fast.headline_latency_ms == 100
    assert fast.verdict == PASS
    assert slow.headline_latency_ms == 500
    assert slow.verdict == FAIL


def test_rank_digest_mismatch_is_inconclusive(tmp_path: Path) -> None:
    reports = tmp_path / "reports"
    write_reports(reports, make_fm("O1", apr=0.5, ci_lo=0.5))
    th = thresholds(tmp_path)
    assert grade_study(scan_reports(reports)[0], thresholds=th).verdict != INCONCLUSIVE

    # Change only the declared digest; the parquet (counts, dates) is untouched.
    front_matter = make_fm("O1", apr=0.5, ci_lo=0.5)
    attach_episode_file(front_matter, reports)
    front_matter["episode_digest"] = "0" * 64
    (reports / "O1-o1.md").write_text(render_front_matter(front_matter) + "\n# body\n")
    result = grade_study(scan_reports(reports)[0], thresholds=th)
    assert result.verdict == INCONCLUSIVE
    assert any("digest" in reason for reason in result.reasons)


def test_missing_declared_parquet_is_inconclusive(tmp_path: Path) -> None:
    reports = tmp_path / "reports"
    write_reports(reports, make_fm("O1", apr=0.5, ci_lo=0.5))
    (reports / "O1-o1-episodes.parquet").unlink()
    assert grade_study(scan_reports(reports)[0], thresholds=thresholds(tmp_path)).verdict == (
        INCONCLUSIVE
    )


def test_missing_digest_fields_is_inconclusive(tmp_path: Path) -> None:
    reports = tmp_path / "reports"
    reports.mkdir()
    front_matter = make_fm("O1", apr=0.5, ci_lo=0.5)  # no digest, no parquet
    (reports / "O1-o1.md").write_text(render_front_matter(front_matter) + "\n# body\n")
    assert grade_study(scan_reports(reports)[0], thresholds=thresholds(tmp_path)).verdict == (
        INCONCLUSIVE
    )


def test_parquet_provenance_overrides_front_matter(tmp_path: Path) -> None:
    reports = tmp_path / "reports"
    front_matter = make_fm("O1", apr=0.60, ci_lo=0.60, data_source="forward")
    attach_episode_file(front_matter, reports, source="tardis")
    (reports / "O1-o1.md").write_text(render_front_matter(front_matter) + "\n# body\n")
    scanned = scan_reports(reports)
    assert scanned[0]["data_source"] == "backfill"
    result = grade_study(scanned[0], thresholds=thresholds(tmp_path))
    assert result.verdict == MARGINAL
    assert result.qualifier is not None and result.qualifier.startswith(HIST_PRELIM)


def test_rank_never_upgrades_declared_provenance(tmp_path: Path) -> None:
    reports = tmp_path / "reports"
    # Declared backfill (e.g. a tardis robustness re-run) but a recorder parquet.
    front_matter = make_fm(
        "O1",
        apr=0.60,
        ci_lo=0.60,
        data_source="backfill",
        backfill_sources=["tardis"],
        fidelity_class="H1",
    )
    attach_episode_file(front_matter, reports, source="recorder")
    (reports / "O1-o1.md").write_text(render_front_matter(front_matter) + "\n# body\n")
    scanned = scan_reports(reports)
    assert scanned[0]["data_source"] == "backfill"
    assert "tardis" in scanned[0]["backfill_sources"]
    result = grade_study(scanned[0], thresholds=thresholds(tmp_path))
    assert result.verdict != PASS
    assert result.qualifier is not None and result.qualifier.startswith(HIST_PRELIM)


def test_parquet_episode_count_mismatch_is_inconclusive(tmp_path: Path) -> None:
    reports = tmp_path / "reports"
    front_matter = make_fm("O1", apr=0.5, ci_lo=0.5)
    attach_episode_file(front_matter, reports)
    front_matter["capital"][0]["oos"][0]["episodes"] += 1
    (reports / "O1-o1.md").write_text(render_front_matter(front_matter) + "\n# body\n")
    assert grade_study(scan_reports(reports)[0], thresholds=thresholds(tmp_path)).verdict == (
        INCONCLUSIVE
    )


def test_scan_ignores_non_study_files(tmp_path: Path) -> None:
    reports = tmp_path / "reports"
    write_reports(reports, make_fm("O1"))
    (reports / "README.md").write_text("notes")
    (reports / "RANKING.md").write_text("# ranking")
    (reports / "O2x.md").write_text("not a report")
    studies = scan_reports(reports)
    assert [study["study_id"] for study in studies] == ["O1"]


def test_deterministic_output(tmp_path: Path) -> None:
    reports = tmp_path / "reports"
    write_reports(
        reports,
        make_fm("O2", apr=0.2, ci_lo=0.2),
        make_fm("O1", apr=0.5, ci_lo=0.5),
    )
    first, _ = build_ranking(reports, thresholds_file(tmp_path))
    second, _ = build_ranking(reports, thresholds_file(tmp_path))
    assert first == second


def test_main_writes_ranking(tmp_path: Path) -> None:
    reports = tmp_path / "reports"
    write_reports(reports, make_fm("O1", apr=0.5, ci_lo=0.5))
    out = tmp_path / "out" / "RANKING.md"
    code = main(
        [
            "--reports-dir",
            str(reports),
            "--thresholds",
            str(thresholds_file(tmp_path)),
            "--out",
            str(out),
        ]
    )
    assert code == 0
    assert out.is_file()
    assert "O1" in out.read_text()
