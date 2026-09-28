"""Study reports and ``RANKING.md`` (task P-5, SPEC-0008 §13.5–§13.7, §13.10–§13.11).

A study turns its episode table into a §13.7 report and a machine-readable block
of metrics; :mod:`hlr.rank` scans those reports, re-grades every study against
``research/thresholds.toml`` at run time, and writes ``RANKING.md``. This module
owns both halves so the CLI is a thin wrapper.

The §13.7 report is markdown with a front-matter block::

    ---
    { ...the metrics, as JSON... }
    ---
    # O1 — ...

The front-matter is **JSON**, which is a strict subset of YAML 1.2, so the block
is valid YAML without adding a YAML dependency (research already depends on
``orjson``; the P-5 task adds no new ones). :func:`parse_front_matter` reads it
back with ``orjson``. The block carries the raw metrics (per capital, per sample
split, per latency, per capture variant), never a stored verdict, so editing
``thresholds.toml`` and re-running ``uv run hlr-rank`` re-grades every study.

Grading follows §13.6: on the out-of-sample 60/40 split, using the §13.10
headline variant (``adj_jitter`` = competition-adjusted + jittered latency) at
the threshold's headline latency and headline capital. PASS needs the APR point
at or above the target **and** the 90% CI lower bound at or above the floor;
quality gates (episodes/day, concentration, coverage) must also hold. Naive
numbers are shown only as context.

False-PASS paths are closed: the headline cell must be exactly ``adj_jitter`` at
the smallest grid latency >= the threshold headline (INCONCLUSIVE otherwise),
and the headline capital must have been run (no nearest-match); data provenance
is derived from the tables' ``source`` column (anything but ``recorder`` is
backfill), a ``HIST-PRELIM`` report, a preliminary report, and a report whose
buffered-cost robustness re-run does not stay positive are all capped at
MARGINAL; and a report whose episode parquet no longer matches its stored
sha256 digest is INCONCLUSIVE.

This is research code. It is never imported by, or deployed with, the bot; it
reads no keys and no network.
"""

from __future__ import annotations

import datetime as _dt
import hashlib
import math
from collections.abc import Mapping, Sequence
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import orjson
import polars as pl

from hlr.episodes import (
    DEFAULT_LATENCIES_MS,
    HEADLINE_VARIANT,
    EpisodeError,
    daily_capture,
    episode_metrics,
    oos_split,
)
from hlr.thresholds import (
    FAIL,
    MARGINAL,
    PASS,
    Thresholds,
    load_thresholds,
)

__all__ = [
    "HIST_PRELIM",
    "INCONCLUSIVE",
    "RANKING_FILENAME",
    "GradeResult",
    "ReportError",
    "build_front_matter",
    "build_report",
    "canonical_digest",
    "canonical_episodes",
    "episode_digest",
    "grade_study",
    "parse_front_matter",
    "rank_studies",
    "render_front_matter",
    "render_ranking",
    "render_report",
    "top_episodes",
    "write_daily_chart",
]

#: The §13.6 verdict tier for too little data (not produced by ``thresholds.grade``).
INCONCLUSIVE = "INCONCLUSIVE"
#: The §13.11 qualifier for a result from the historical backfill lane.
HIST_PRELIM = "HIST-PRELIM"
#: The generated ranking file name inside the reports directory.
RANKING_FILENAME = "RANKING.md"

#: Fence that opens and closes the front-matter block.
_FENCE = "---"
#: Report-section order for :func:`render_report` (§13.7).
_SECTION_KEYS = ("data", "method", "artifacts", "sensitivity", "sanity", "reproduce")#: ``captured``/``open`` column formats per capture variant (mirrors ``hlr.episodes``).
_CAPTURED_FORMAT = {
    "naive": "captured_{lat}",
    "adj": "captured_adj_{lat}",
    "jitter": "captured_jitter_{lat}",
    "adj_jitter": "captured_adj_jitter_{lat}",
}
_OPEN_FORMAT = {
    "naive": "open_{lat}",
    "adj": "open_{lat}",
    "jitter": "open_jitter_{lat}",
    "adj_jitter": "open_jitter_{lat}",
}
#: §13.5 minimum valid days for a final vs a preliminary report.
_MIN_DAYS_FINAL = 14
_MIN_DAYS_PRELIM = 3
#: Sort order for the verdict tiers and implementation costs.
_VERDICT_ORDER = {PASS: 0, MARGINAL: 1, INCONCLUSIVE: 2, FAIL: 3}
_IMPL_ORDER = {"S": 0, "M": 1, "L": 2}


class ReportError(ValueError):
    """A report is malformed: bad front-matter, missing field, bad metric shape."""


@dataclass(frozen=True)
class GradeResult:
    """One study's recomputed §13.6 verdict and the numbers behind it."""

    study_id: str
    slug: str
    title: str
    verdict: str
    qualifier: str | None
    score: float
    implementation_cost: str
    headline_latency_ms: int | None
    headline_capital_usd: float | None
    headline_variant: str | None
    apr: float | None
    apr_ci90_lo: float | None
    usd_per_day: float | None
    robust_usd_per_day: float | None
    naive_apr: float | None
    naive_usd_per_day: float | None
    in_sample_apr: float | None
    episodes_per_day: float | None
    concentration: float | None
    coverage_pct: float | None
    days: int
    preliminary: bool
    apr_by_capital: tuple[tuple[float, float | None, float | None], ...] = ()
    reasons: tuple[str, ...] = field(default_factory=tuple)

    def sort_key(self) -> tuple[object, ...]:
        """Deterministic §13.6 ordering: tier, forward-before-hist, score, cost, id."""
        hist = 0 if self.qualifier is None else 1
        cost = _IMPL_ORDER.get(self.implementation_cost, 99)
        return (_VERDICT_ORDER[self.verdict], hist, -self.score, cost, self.study_id)


# --------------------------------------------------------------------------
# Front matter
# --------------------------------------------------------------------------


def render_front_matter(front_matter: Mapping[str, Any]) -> str:
    """Serialize ``front_matter`` as a fenced JSON (YAML-subset) block."""
    payload = {key: value for key, value in front_matter.items() if not key.startswith("_")}
    body = orjson.dumps(
        payload, option=orjson.OPT_SORT_KEYS | orjson.OPT_INDENT_2
    ).decode("utf-8")
    return f"{_FENCE}\n{body}\n{_FENCE}\n"


def parse_front_matter(text: str) -> dict[str, Any]:
    """Parse the leading fenced front-matter block out of a report ``text``.

    The block is JSON (a strict YAML 1.2 subset, see the module docstring). Raises
    :class:`ReportError` when the fences are missing, the JSON is invalid, or the
    block is not an object.
    """
    lines = text.splitlines()
    if not lines or lines[0].strip() != _FENCE:
        raise ReportError("report has no `---` front-matter block")
    end = None
    for index in range(1, len(lines)):
        if lines[index].strip() == _FENCE:
            end = index
            break
    if end is None:
        raise ReportError("report front-matter block is not closed with `---`")
    raw = "\n".join(lines[1:end])
    try:
        parsed = orjson.loads(raw)
    except orjson.JSONDecodeError as err:
        raise ReportError(f"invalid front-matter JSON: {err}") from err
    if not isinstance(parsed, dict):
        raise ReportError("front-matter must be a JSON object")
    return parsed


def build_front_matter(
    *,
    study_id: str,
    slug: str,
    title: str,
    hypothesis: str,
    implementation_cost: str,
    days: int,
    coverage_pct: float,
    capital_runs: Mapping[float, pl.DataFrame],
    max_notional: Mapping[float, float],
    all_dates: Sequence[_dt.date],
    prereg_sha: str = "",
    cells_k: int | None = None,
    preliminary: bool = False,
    fidelity_class: str | None = None,
    robustness_runs: Mapping[float, pl.DataFrame] | None = None,
    episode_parquet: str | None = None,
    reports_dir: str | Path | None = None,
    headline_latency_ms: int = 250,
    headline_capital_usd: float | None = None,
    latencies_ms: Sequence[int] = DEFAULT_LATENCIES_MS,
    capture_variants: Sequence[str] | None = None,
    bootstrap_draws: int = 2000,
    bootstrap_seed: int = 0,
    sections: Mapping[str, Any] | None = None,
) -> dict[str, Any]:
    """Compute the §13.5 metrics and assemble a study's report front-matter.

    ``capital_runs`` maps each capital-grid point (USD) to the episode table the
    study detected with that point's ``max_notional`` (required, and asserted
    against ``size_usd_at_start``). The episodes are split chronologically 60/40
    (:func:`hlr.episodes.oos_split`); both samples get one metrics row per
    ``(latency, capture_variant)`` with per-cell ``concentration`` and, when
    ``robustness_runs`` is given, a ``usd_per_day_robust`` from the buffered-cost
    re-run.

    Data provenance is derived, never asserted by the caller: if any input table
    carries a non-``recorder`` ``source`` column the study is stamped
    ``backfill``, which makes the verdict ``HIST-PRELIM`` (§13.11). The canonical
    episode digest (and optional parquet path) let ``hlr-rank`` detect a report
    that no longer matches its data.
    """
    if implementation_cost not in _IMPL_ORDER:
        raise ReportError(
            f"implementation_cost must be S, M or L, got {implementation_cost!r}"
        )
    if not capital_runs:
        raise ReportError("capital_runs is empty; a study has at least one capital point")
    dates = _clean_dates(all_dates, "all_dates")
    if days != len(dates):
        raise ReportError(f"days ({days}) must equal len(all_dates) ({len(dates)})")
    in_sample, out_sample = oos_split(dates)
    if days != len(in_sample) + len(out_sample):
        raise ReportError("days must equal in-sample plus out-of-sample days")
    sorted_capitals = sorted(float(c) for c in capital_runs)
    data_source, backfill_sources = _provenance(capital_runs)

    capital_blocks: list[dict[str, Any]] = []
    for capital in sorted_capitals:
        key = _as_key(capital_runs, capital)
        episodes = capital_runs[key]
        if key not in max_notional:
            raise ReportError(f"missing max_notional for capital ${capital:,.0f}")
        max_not = _as_float(max_notional[key])
        sizes = episodes["size_usd_at_start"] if "size_usd_at_start" in episodes.columns else None
        largest = sizes.max() if sizes is not None else None
        if largest is not None and float(largest) > max_not:
            raise ReportError(
                f"episode size_usd_at_start {float(largest):.2f} exceeds "
                f"max_notional {max_not:.2f} at capital ${capital:,.0f}"
            )
        robust = robustness_runs.get(key) if robustness_runs else None
        capital_blocks.append(
            {
                "capital_usd": capital,
                "max_notional": max_not,
                "in_sample": _sample_rows(
                    episodes,
                    in_sample,
                    capital_usd=capital,
                    latencies_ms=latencies_ms,
                    capture_variants=capture_variants,
                    bootstrap_draws=bootstrap_draws,
                    bootstrap_seed=bootstrap_seed,
                    robust_episodes=robust,
                ),
                "oos": _sample_rows(
                    episodes,
                    out_sample,
                    capital_usd=capital,
                    latencies_ms=latencies_ms,
                    capture_variants=capture_variants,
                    bootstrap_draws=bootstrap_draws,
                    bootstrap_seed=bootstrap_seed,
                    robust_episodes=robust,
                ),
            }
        )

    digest = episode_digest(capital_runs)
    if episode_parquet is not None and reports_dir is not None:
        destination = Path(reports_dir) / episode_parquet
        destination.parent.mkdir(parents=True, exist_ok=True)
        canonical_episodes(capital_runs).write_parquet(destination)

    merged = _default_sections()
    if sections:
        merged.update(sections)
    available_variants = sorted(
        {str(row.get("capture_variant")) for block in capital_blocks for row in block["oos"]}
    )
    headline_cap = _nearest_capital_of(
        capital_runs, float(headline_capital_usd)
        if headline_capital_usd is not None
        else sorted_capitals[0]
    )
    if not merged.get("top_episodes"):
        run = capital_runs[_as_key(capital_runs, headline_cap)]
        merged["top_episodes"] = top_episodes(
            run, _ceiling_latency(latencies_ms, headline_latency_ms) or max(latencies_ms),
            HEADLINE_VARIANT,
        )

    return _json_safe(
        {
            "spec": "SPEC-0008",
            "study_id": study_id,
            "slug": slug,
            "title": title,
            "hypothesis": hypothesis,
            "implementation_cost": implementation_cost,
            "days": days,
            "coverage_pct": float(coverage_pct),
            "prereg_sha": prereg_sha,
            "cells_K": cells_k,
            "preliminary": bool(preliminary),
            "data_source": data_source,
            "backfill_sources": list(backfill_sources),
            "fidelity_class": fidelity_class,
            "episode_digest": digest,
            "episode_parquet": episode_parquet,
            "headline_latency_ms": int(headline_latency_ms),
            "headline_variant": HEADLINE_VARIANT,
            "headline_capital_usd": headline_cap,
            "latency_grid_ms": [int(lat) for lat in latencies_ms],
            "capture_variants": list(capture_variants)
            if capture_variants
            else available_variants,
            "in_sample_dates": [day.isoformat() for day in in_sample],
            "oos_dates": [day.isoformat() for day in out_sample],
            "capital": capital_blocks,
            "sections": merged,
        }
    )


def build_report(
    *,
    daily: pl.DataFrame | None = None,
    img_dir: str | Path | None = None,
    **front_matter_kwargs: Any,
) -> str:
    """Build a full §13.7 report from a study's episode tables and parameters.

    This is the convenience wrapper the studies call: it builds the front-matter
    with :func:`build_front_matter`, optionally writes the required per-day bar
    chart into ``img_dir``, and renders the markdown skeleton.
    """
    front_matter = build_front_matter(**front_matter_kwargs)
    if daily is not None and img_dir is not None:
        name = f"{front_matter['study_id']}-{front_matter['slug']}-daily.png"
        write_daily_chart(daily, Path(img_dir) / name, title=front_matter["title"])
        front_matter["sections"]["image"] = f"img/{name}"
    return render_report(front_matter)


# --------------------------------------------------------------------------
# Grading
# --------------------------------------------------------------------------


def grade_study(
    front_matter: Mapping[str, Any],
    *,
    thresholds: Thresholds | None = None,
) -> GradeResult:
    """Recompute one study's verdict against the thresholds (§13.6, §13.10).

    Runs entirely off the front-matter metrics, so a thresholds edit changes the
    verdict without regenerating the report. Never raises for a zero-episode
    study: it grades FAIL and stays in the ranking.
    """
    th = thresholds if thresholds is not None else load_thresholds()
    study_id = str(front_matter.get("study_id") or "?")
    slug = str(front_matter.get("slug") or "")
    title = str(front_matter.get("title") or "")
    implementation_cost = str(front_matter.get("implementation_cost") or "M")
    days = _as_int(front_matter.get("days"), "days")
    preliminary = bool(front_matter.get("preliminary", False))
    forward = _is_forward(front_matter)
    hist = not forward
    qualifier = _qualifier(front_matter) if hist else None

    _check_day_counts(front_matter, days)

    reasons: list[str] = []
    capital_runs = _capital_runs(front_matter)
    lat = _ceiling_latency(_latency_grid(front_matter, th), th.latency.headline_ms)
    run = _exact_run(capital_runs, float(th.capital.headline_usd))
    cap = float(run.get("capital_usd")) if run is not None else None
    oos = run.get("oos", []) if run else []
    variant = (
        HEADLINE_VARIANT
        if lat is not None and _find_row(oos, lat, HEADLINE_VARIANT) is not None
        else None
    )
    row = _find_row(oos, lat, variant) if variant else None

    concentration = _optional_float(row.get("concentration")) if row else None
    robust = _optional_float(row.get("usd_per_day_robust")) if row else None
    coverage = _optional_float(front_matter.get("coverage_pct"))

    apr = _optional_float(row.get("apr")) if row else None
    apr_ci_lo = _optional_float(row.get("apr_ci90_lo")) if row else None
    usd_per_day = _optional_float(row.get("usd_per_day")) if row else None
    naive_row = _find_row(oos, lat, "naive") if oos and lat is not None else None
    naive_apr = _optional_float(naive_row.get("apr")) if naive_row else None
    naive_usd = _optional_float(naive_row.get("usd_per_day")) if naive_row else None
    is_row = (
        _find_row(run.get("in_sample", []), lat, variant) if run and variant else None
    )
    in_apr = _optional_float(is_row.get("apr")) if is_row else None

    score = (
        (usd_per_day or 0.0) * (1.0 - concentration)
        if concentration is not None
        else 0.0
    )
    apr_by_capital = _apr_by_capital(capital_runs, th, lat)

    min_days = _MIN_DAYS_PRELIM if preliminary else _MIN_DAYS_FINAL
    verdict: str
    if days < min_days:
        verdict = INCONCLUSIVE
        reasons.append(f"only {days} valid day(s); need >= {min_days} for this report")
    elif bool(front_matter.get("_digest_mismatch")):
        verdict = INCONCLUSIVE
        reasons.append("episode parquet digest does not match the report")
    elif lat is None:
        verdict = INCONCLUSIVE
        reasons.append(
            f"no grid latency >= headline {th.latency.headline_ms} ms; report is not gradeable"
        )
    elif run is None:
        verdict = INCONCLUSIVE
        reasons.append(
            f"headline capital ${th.capital.headline_usd:,} was not run; no nearest match"
        )
    elif variant is None:
        verdict = INCONCLUSIVE
        reasons.append(
            f"headline variant `{HEADLINE_VARIANT}` is missing at L = {lat} ms; "
            "only the §13.10 headline cell may be graded"
        )
    elif row is None:
        verdict = FAIL
        reasons.append("no out-of-sample metrics for the headline cell")
    else:
        verdict = _grade_cell(
            apr=apr,
            apr_ci_lo=apr_ci_lo,
            concentration=concentration,
            coverage=coverage,
            episodes_per_day=_optional_float(row.get("episodes_per_day_p50")),
            hist=hist,
            preliminary=preliminary,
            robust_usd_per_day=robust,
            thresholds=th,
            reasons=reasons,
        )

    return GradeResult(
        study_id=study_id,
        slug=slug,
        title=title,
        verdict=verdict,
        qualifier=qualifier,
        score=score,
        implementation_cost=implementation_cost,
        headline_latency_ms=lat,
        headline_capital_usd=cap,
        headline_variant=variant,
        apr=apr,
        apr_ci90_lo=apr_ci_lo,
        usd_per_day=usd_per_day,
        robust_usd_per_day=robust,
        naive_apr=naive_apr,
        naive_usd_per_day=naive_usd,
        in_sample_apr=in_apr,
        episodes_per_day=_optional_float(row.get("episodes_per_day_p50")) if row else None,
        concentration=concentration,
        coverage_pct=coverage,
        days=days,
        preliminary=preliminary,
        apr_by_capital=apr_by_capital,
        reasons=tuple(reasons),
    )


def _grade_cell(
    *,
    apr: float | None,
    apr_ci_lo: float | None,
    concentration: float | None,
    coverage: float | None,
    episodes_per_day: float | None,
    hist: bool,
    preliminary: bool,
    robust_usd_per_day: float | None,
    thresholds: Thresholds,
    reasons: list[str],
) -> str:
    """Apply the §13.6 quality gates and APR tier, then the PASS caps.

    Quality failures (episodes/day, concentration, coverage) are FAIL. A PASS also
    requires the robustness re-run to stay positive, and is capped at MARGINAL for
    a HIST-PRELIM report, a preliminary report, or a non-robust one.
    """
    quality_ok = True
    if episodes_per_day is None:
        quality_ok = False
        reasons.append("missing episodes/day")
    elif episodes_per_day < thresholds.quality.min_episodes_per_day:
        quality_ok = False
        reasons.append(
            f"episodes/day {episodes_per_day:.1f} < "
            f"{thresholds.quality.min_episodes_per_day}"
        )
    if concentration is None:
        quality_ok = False
        reasons.append("missing concentration")
    elif concentration > thresholds.quality.max_concentration:
        quality_ok = False
        reasons.append(
            f"concentration {concentration:.2f} > {thresholds.quality.max_concentration}"
        )
    if coverage is None:
        quality_ok = False
        reasons.append("missing coverage_pct")
    elif coverage < thresholds.quality.min_coverage_pct:
        quality_ok = False
        reasons.append(
            f"coverage {coverage:.3f} < {thresholds.quality.min_coverage_pct}"
        )

    if apr is None or apr_ci_lo is None:
        reasons.append("missing APR / CI")
        return FAIL
    if not quality_ok:
        return FAIL
    if apr < thresholds.apr.floor:
        reasons.append(f"APR {apr:.3f} < floor {thresholds.apr.floor}")
        return FAIL
    if apr >= thresholds.apr.target and apr_ci_lo >= thresholds.apr.floor:
        if hist:
            reasons.append("HIST-PRELIM cannot PASS (§13.11)")
            return MARGINAL
        if preliminary:
            reasons.append("preliminary report cannot PASS; needs >= 14 valid days")
            return MARGINAL
        if robust_usd_per_day is None or robust_usd_per_day <= 0.0:
            reasons.append(
                "robustness check failed or missing; usd_per_day at buffered costs "
                "must stay > 0"
            )
            return MARGINAL
        return PASS
    reasons.append(
        f"APR {apr:.3f} with CI lower bound {apr_ci_lo:.3f} below target "
        f"{thresholds.apr.target}"
    )
    return MARGINAL


def rank_studies(
    studies: Sequence[Mapping[str, Any]],
    *,
    thresholds: Thresholds | None = None,
) -> list[GradeResult]:
    """Grade and deterministically sort studies for ``RANKING.md`` (§13.6)."""
    th = thresholds if thresholds is not None else load_thresholds()
    return sorted(
        (grade_study(study, thresholds=th) for study in studies),
        key=lambda result: result.sort_key(),
    )


# --------------------------------------------------------------------------
# Rendering
# --------------------------------------------------------------------------


def render_report(front_matter: Mapping[str, Any]) -> str:
    """Render the §13.7 report skeleton from a study's front-matter."""
    fm = dict(front_matter)
    sections = _default_sections()
    sections.update(fm.get("sections") or {})
    verdict = grade_study(fm)
    headline = _headline_block(fm, verdict)

    lines: list[str] = [render_front_matter(fm).rstrip("\n"), ""]
    lines.append(f"# {fm.get('study_id')} — {fm.get('title')}")
    if verdict.qualifier:
        lines += ["", f"> **PRELIMINARY ({verdict.qualifier})**"]
    lines += ["", "## 1. Hypothesis", "", str(fm.get("hypothesis") or "")]
    lines += ["", "## 2. Data"]
    lines += _render_kv(_section_map(sections, "data"))
    lines += ["", "## 3. Method"]
    lines += _render_kv(_section_map(sections, "method"))
    lines += [
        "",
        (
            f"- Headline cell: L = {verdict.headline_latency_ms} ms, "
            f"capital = ${verdict.headline_capital_usd:,.0f}, "
            f"variant = `{verdict.headline_variant}`"
        ),
        f"- Pre-registration git SHA: `{fm.get('prereg_sha') or ''}`",
        f"- Cells scanned (K): {fm.get('cells_K')}",
    ]
    lines += ["", "## 4. Results"]
    lines += _render_results(fm, verdict, headline, sections)
    lines += ["", "## 5. Sanity checks"]
    lines += _render_bullets(_section_list(sections, "sanity"))
    lines += ["", "## 6. Verdict"]
    lines += _render_verdict(verdict, fm)
    lines += ["", "## 7. Reproduce", "", f"```sh\n{sections.get('reproduce', '')}\n```", ""]
    return "\n".join(lines)


def render_ranking(
    results: Sequence[GradeResult],
    thresholds: Thresholds,
) -> str:
    """Render ``RANKING.md`` from graded studies and the live thresholds."""
    grid = list(thresholds.capital.grid_usd)
    lines: list[str] = [
        "# Strategy ranking (SPEC-0008 §13.6)",
        "",
        (
            "Generated by `uv run hlr-rank`. Every study is graded at run time from "
            "its report front-matter against `research/thresholds.toml`; edit the "
            "thresholds and re-run to re-grade without touching code."
        ),
        "",
        f"- Headline latency: **{thresholds.latency.headline_ms} ms**",
        f"- Headline capital: **${thresholds.capital.headline_usd:,}**",
        (
            f"- APR target / floor: **{thresholds.apr.target:.0%} / "
            f"{thresholds.apr.floor:.0%}**"
        ),
        (
            f"- Quality: episodes/day >= {thresholds.quality.min_episodes_per_day}, "
            f"concentration <= {thresholds.quality.max_concentration}, "
            f"coverage >= {thresholds.quality.min_coverage_pct:.0%}"
        ),
        "",
        (
            "The graded columns use the §13.10 headline variant "
            "(`adj_jitter`: competition-adjusted + jittered); naive is shown only as "
            "context. `HIST-PRELIM` can never PASS (§13.11) and is grouped below the "
            "forward studies."
        ),
        "",
    ]
    if not results:
        lines += ["_No study reports yet._", ""]
        return "\n".join(lines)

    lines += ["## Overview", "", *_ranking_table(results, grid)]
    for verdict in (PASS, MARGINAL, INCONCLUSIVE, FAIL):
        group = [result for result in results if result.verdict == verdict]
        if not group:
            continue
        lines += ["", f"## {verdict} ({len(group)})", "", *_ranking_table(group, grid)]
    return "\n".join(lines)


def _ranking_table(results: Sequence[GradeResult], grid: Sequence[int]) -> list[str]:
    headers = [
        "Study",
        "Title",
        "Verdict",
        "Score",
        "APR (OOS)",
        "CI lo",
        "Naive APR",
        "Impl",
        "ep/day",
        "conc",
        "days",
        "Qualifier",
    ] + [f"APR @ ${cap:,}" for cap in grid]
    rows: list[list[str]] = []
    for result in results:
        by_capital = {int(cap): (apr, lo) for cap, apr, lo in result.apr_by_capital}
        rows.append(
            [
                result.study_id,
                result.title,
                result.verdict,
                f"{result.score:,.2f}",
                _fmt_pct(result.apr),
                _fmt_pct(result.apr_ci90_lo),
                _fmt_pct(result.naive_apr),
                result.implementation_cost,
                _fmt_num(result.episodes_per_day, 1),
                _fmt_num(result.concentration, 2),
                str(result.days),
                result.qualifier or "",
            ]
            + [_fmt_pct(by_capital.get(int(cap), (None, None))[0]) for cap in grid]
        )
    return _markdown_table(headers, rows)


def _render_results(
    fm: Mapping[str, Any],
    verdict: GradeResult,
    headline: Mapping[str, Any] | None,
    sections: Mapping[str, Any],
) -> list[str]:
    lines = ["", "### §13.5 metrics (headline cell, in-sample vs out-of-sample)"]
    if headline is None:
        lines += ["", "_No metrics for the headline cell._"]
    else:
        headers = ["Metric", "In-sample", "Out-of-sample"]
        ins = headline["in_sample"]
        oos = headline["oos"]
        metrics = [
            ("APR", _fmt_pct(ins.get("apr")), _fmt_pct(oos.get("apr"))),
            (
                "APR 90% CI lower",
                _fmt_pct(ins.get("apr_ci90_lo")),
                _fmt_pct(oos.get("apr_ci90_lo")),
            ),
            ("usd/day", _fmt_usd(ins.get("usd_per_day")), _fmt_usd(oos.get("usd_per_day"))),
            (
                "usd/day 90% CI",
                _fmt_range(ins.get("usd_per_day_ci90_lo"), ins.get("usd_per_day_ci90_hi")),
                _fmt_range(oos.get("usd_per_day_ci90_lo"), oos.get("usd_per_day_ci90_hi")),
            ),
            (
                "episodes/day p50",
                _fmt_num(ins.get("episodes_per_day_p50"), 1),
                _fmt_num(oos.get("episodes_per_day_p50"), 1),
            ),
            (
                "episodes/day p90",
                _fmt_num(ins.get("episodes_per_day_p90"), 1),
                _fmt_num(oos.get("episodes_per_day_p90"), 1),
            ),
            (
                "duration p50/p90 ms",
                _fmt_pair(ins, "duration_ms_p50", "duration_ms_p90"),
                _fmt_pair(oos, "duration_ms_p50", "duration_ms_p90"),
            ),
            (
                "peak net bps p50/p90",
                _fmt_pair(ins, "peak_net_bps_p50", "peak_net_bps_p90"),
                _fmt_pair(oos, "peak_net_bps_p50", "peak_net_bps_p90"),
            ),
            (
                "capture rate",
                _fmt_pct(ins.get("capture_rate")),
                _fmt_pct(oos.get("capture_rate")),
            ),
            (
                "markout 1s / 10s bps",
                _fmt_pair(ins, "markout_1s_median", "markout_10s_median"),
                _fmt_pair(oos, "markout_1s_median", "markout_10s_median"),
            ),
        ]
        lines += [""] + _markdown_table(headers, [[m, i, o] for m, i, o in metrics])

    lines += ["", "### Latency grid (out-of-sample; naive shown as context)"]
    lines += ["", *_latency_table(fm, verdict)]
    lines += ["", "### Capital grid (out-of-sample)"]
    lines += ["", *_capital_table(fm, verdict)]
    lines += ["", "### Top-10 episodes"]
    top = _section_list(sections, "top_episodes")
    lines += ["", *_episodes_table(top)]
    lines += ["", "### Artifact counts"]
    lines += _render_kv(_section_map(sections, "artifacts"))
    lines += ["", "### Sensitivity"]
    lines += ["", *_records_table(_section_list(sections, "sensitivity"))]
    if sections.get("image"):
        lines += ["", f"![Per-day captured USD]({sections['image']})"]
    return lines


def _latency_table(fm: Mapping[str, Any], verdict: GradeResult) -> list[str]:
    run = _run_for_capital(fm, verdict.headline_capital_usd)
    oos = run.get("oos", []) if run else []
    variant = verdict.headline_variant or HEADLINE_VARIANT
    headers = [
        "L (ms)",
        "episodes",
        "capture_rate",
        "usd/day (naive)",
        "usd/day (adj_jitter)",
        "CI lo",
        "APR (adj_jitter)",
        "markout 1s",
        "markout 10s",
    ]
    rows: list[list[str]] = []
    for lat in _latency_grid(fm, None):
        row = _find_row(oos, lat, variant)
        naive = _find_row(oos, lat, "naive")
        if row is None and naive is None:
            continue
        rows.append(
            [
                str(lat),
                str(_get_int(row or naive, "episodes")),
                _fmt_pct(_get(row, "capture_rate")),
                _fmt_usd(_get(naive, "usd_per_day")),
                _fmt_usd(_get(row, "usd_per_day")),
                _fmt_usd(_get(row, "usd_per_day_ci90_lo")),
                _fmt_pct(_get(row, "apr")),
                _fmt_num(_get(row, "markout_1s_median"), 2),
                _fmt_num(_get(row, "markout_10s_median"), 2),
            ]
        )
    if not rows:
        return ["_No latency rows._"]
    return _markdown_table(headers, rows)


def _capital_table(fm: Mapping[str, Any], verdict: GradeResult) -> list[str]:
    headers = ["Capital (USD)", "max notional", "usd/day (OOS)", "APR (OOS)", "APR CI lo", "Concentration"]
    rows: list[list[str]] = []
    lat = verdict.headline_latency_ms
    for block in _capital_runs(fm):
        row = (
            _find_row(block.get("oos", []), lat, HEADLINE_VARIANT)
            if lat is not None
            else None
        )
        rows.append(
            [
                f"{_as_float(block.get('capital_usd')):,.0f}",
                _fmt_usd(block.get("max_notional")),
                _fmt_usd(_get(row, "usd_per_day")),
                _fmt_pct(_get(row, "apr")),
                _fmt_pct(_get(row, "apr_ci90_lo")),
                _fmt_num(_get(row, "concentration"), 2),
            ]
        )
    if not rows:
        return ["_No capital rows._"]
    return _markdown_table(headers, rows)


def _episodes_table(top: Sequence[Mapping[str, Any]]) -> list[str]:
    if not top:
        return ["_No episodes recorded._"]
    preferred = [
        "t_start",
        "date",
        "duration_ms",
        "peak_net_bps",
        "size_usd_at_start",
        "captured",
    ]
    keys = [key for key in preferred if any(key in row for row in top)]
    if not keys:
        keys = [key for key in top[0] if not key.startswith("open")][:6]
    headers = [key.replace("_", " ") for key in keys]
    rows = [[_fmt_cell(row.get(key)) for key in keys] for row in top]
    return _markdown_table(headers, rows)


def _render_verdict(verdict: GradeResult, fm: Mapping[str, Any]) -> list[str]:
    label = verdict.verdict if not verdict.qualifier else f"{verdict.verdict} ({verdict.qualifier})"
    lines = [
        "",
        (
            f"**{label}** — score {verdict.score:,.2f} "
            f"(implementation cost {verdict.implementation_cost})."
        ),
        "",
        (
            f"- OOS APR {_fmt_pct(verdict.apr)} "
            f"(90% CI lower {_fmt_pct(verdict.apr_ci90_lo)}), "
            f"naive context {_fmt_pct(verdict.naive_apr)}"
        ),
        f"- In-sample APR {_fmt_pct(verdict.in_sample_apr)}",
        (
            f"- Episodes/day {_fmt_num(verdict.episodes_per_day, 1)}, "
            f"concentration {_fmt_num(verdict.concentration, 2)}, "
            f"coverage {_fmt_pct(verdict.coverage_pct)}"
        ),
        (
            f"- Robustness (usd/day at buffer × multiplier): "
            f"{_fmt_usd(verdict.robust_usd_per_day)}"
        ),
    ]
    if verdict.apr_by_capital:
        grid = ", ".join(
            f"${cap:,.0f}: {_fmt_pct(apr)}" for cap, apr, _ in verdict.apr_by_capital
        )
        lines.append(f"- APR by capital grid: {grid}")
    if verdict.reasons:
        lines += ["", "Reasons:"] + [f"- {reason}" for reason in verdict.reasons]
    return lines


# --------------------------------------------------------------------------
# Metrics helpers
# --------------------------------------------------------------------------


def _sample_rows(
    episodes: pl.DataFrame,
    dates: Sequence[_dt.date],
    *,
    capital_usd: float,
    latencies_ms: Sequence[int],
    capture_variants: Sequence[str] | None,
    bootstrap_draws: int,
    bootstrap_seed: int,
    robust_episodes: pl.DataFrame | None = None,
) -> list[dict[str, Any]]:
    """One sample split's §13.5 rows, or an empty list for an empty split.

    The split is by the ``date`` column, so a frame without one cannot be split
    and raises instead of silently mixing in-sample rows into the out-of-sample
    verdict. Each row carries its own ``concentration`` and, when a buffered-cost
    re-run is supplied, ``usd_per_day_robust``.
    """
    if not dates:
        return []
    if "date" not in episodes.columns:
        raise ReportError(
            "episode table has no `date` column; cannot split in-sample/out-of-sample"
        )
    filtered = episodes.filter(pl.col("date").is_in(list(dates)))
    metrics = episode_metrics(
        filtered,
        days=len(dates),
        all_dates=dates,
        latencies_ms=latencies_ms,
        capture_variants=capture_variants,
        capital_usd=capital_usd,
        bootstrap_draws=bootstrap_draws,
        bootstrap_seed=bootstrap_seed,
    ).to_dicts()
    robust_by_cell = _robust_by_cell(
        robust_episodes,
        dates,
        latencies_ms=latencies_ms,
        capture_variants=capture_variants,
        capital_usd=capital_usd,
        bootstrap_draws=bootstrap_draws,
        bootstrap_seed=bootstrap_seed,
    )
    for row in metrics:
        latency = int(row["latency_ms"])
        variant = str(row["capture_variant"])
        row["concentration"] = _concentration(filtered, dates, latency, variant)
        row["usd_per_day_robust"] = robust_by_cell.get((latency, variant))
    return metrics


def _robust_by_cell(
    robust_episodes: pl.DataFrame | None,
    dates: Sequence[_dt.date],
    *,
    latencies_ms: Sequence[int],
    capture_variants: Sequence[str] | None,
    capital_usd: float,
    bootstrap_draws: int,
    bootstrap_seed: int,
) -> dict[tuple[int, str], float | None]:
    """The buffered-cost ``usd_per_day`` per ``(latency, variant)`` cell."""
    if robust_episodes is None or not dates:
        return {}
    if "date" not in robust_episodes.columns:
        raise ReportError("robustness table has no `date` column")
    filtered = robust_episodes.filter(pl.col("date").is_in(list(dates)))
    rows = episode_metrics(
        filtered,
        days=len(dates),
        all_dates=dates,
        latencies_ms=latencies_ms,
        capture_variants=capture_variants,
        capital_usd=capital_usd,
        bootstrap_draws=bootstrap_draws,
        bootstrap_seed=bootstrap_seed,
    ).to_dicts()
    return {
        (int(row["latency_ms"]), str(row["capture_variant"])): _optional_float(
            row.get("usd_per_day")
        )
        for row in rows
    }


def _concentration(
    episodes: pl.DataFrame,
    dates: Sequence[_dt.date],
    latency_ms: int,
    variant: str,
) -> float | None:
    """Best day's share of captured USD at one cell (§13.6).

    Returns ``None`` when the cell cannot be computed (missing capture column,
    no dates), so the grader treats it as a failed quality gate rather than a
    fake zero.
    """
    if not dates:
        return None
    try:
        daily = daily_capture(episodes, latency_ms, all_dates=dates, variant=variant)
    except EpisodeError:
        return None
    total = float(daily["captured_usd"].sum())
    if total <= 0.0:
        return 0.0
    return float(daily["captured_usd"].max()) / total


# --------------------------------------------------------------------------
# Provenance and digest
# --------------------------------------------------------------------------


def canonical_episodes(capital_runs: Mapping[float, pl.DataFrame]) -> pl.DataFrame:
    """The canonical combined episode table: every run tagged with ``capital_usd``.

    Column order is normalized so the same runs always produce the same frame (and
    therefore the same :func:`canonical_digest`), regardless of mapping order.
    """
    frames: list[pl.DataFrame] = []
    for capital in sorted(float(c) for c in capital_runs):
        frame = capital_runs[_as_key(capital_runs, capital)]
        frames.append(frame.with_columns(pl.lit(capital).alias("capital_usd")))
    if not frames:
        return pl.DataFrame()
    combined = pl.concat(frames, how="diagonal_relaxed")
    return combined.select(sorted(combined.columns))


def canonical_digest(frame: pl.DataFrame) -> str:
    """A sha256 over the canonically ordered rows of ``frame``.

    This is the content hash stored in the front-matter and re-checked by
    ``hlr-rank``; it is independent of parquet metadata and mapping order.
    """
    columns = sorted(frame.columns)
    if frame.height == 0:
        payload = "|".join(columns)
    else:
        view = frame.select(columns).sort(columns)
        payload = "\n".join(repr(tuple(row)) for row in view.iter_rows())
    return hashlib.sha256(payload.encode("utf-8")).hexdigest()


def episode_digest(capital_runs: Mapping[float, pl.DataFrame]) -> str:
    """Canonical sha256 of a study's per-capital episode tables."""
    return canonical_digest(canonical_episodes(capital_runs))


def _provenance(capital_runs: Mapping[float, pl.DataFrame]) -> tuple[str, tuple[str, ...]]:
    """Derive ``(data_source, backfill_sources)`` from table ``source`` columns.

    A ``recorder`` source is the forward lane; any other source (or no source
    column at all, which cannot be proven forward) is conservatively backfill.
    """
    found_column = False
    sources: set[str] = set()
    for frame in capital_runs.values():
        if "source" in frame.columns:
            found_column = True
            sources.update(str(value) for value in frame["source"].unique().to_list())
    if not found_column:
        return "unknown", ()
    non_recorder = tuple(sorted(source for source in sources if source != "recorder"))
    if non_recorder:
        return "backfill", non_recorder
    return "forward", ()


def top_episodes(
    episodes: pl.DataFrame,
    latency_ms: int,
    variant: str = HEADLINE_VARIANT,
    *,
    n: int = 10,
) -> list[dict[str, Any]]:
    """The ``n`` episodes with the largest captured USD at ``(latency, variant)``."""
    if episodes.height == 0:
        return []
    captured_col = _CAPTURED_FORMAT.get(variant, _CAPTURED_FORMAT[HEADLINE_VARIANT]).format(
        lat=latency_ms
    )
    if captured_col not in episodes.columns:
        captured_col = _CAPTURED_FORMAT["naive"].format(lat=latency_ms)
    if captured_col not in episodes.columns:
        return []
    wanted = ["t_start", "date", "duration_ms", "peak_net_bps", "size_usd_at_start"]
    columns = [col for col in wanted if col in episodes.columns]
    ranked = (
        episodes.sort(captured_col, descending=True, nulls_last=True)
        .head(n)
        .select([*columns, pl.col(captured_col).alias("captured")])
    )
    return ranked.to_dicts()


# --------------------------------------------------------------------------
# Picking helpers
# --------------------------------------------------------------------------


def _capital_runs(front_matter: Mapping[str, Any]) -> list[dict[str, Any]]:
    """Return the front-matter ``capital`` blocks, raising on a malformed shape."""
    blocks = front_matter.get("capital")
    if not isinstance(blocks, list) or not blocks:
        raise ReportError("front-matter has no non-empty `capital` list")
    if not all(isinstance(block, dict) for block in blocks):
        raise ReportError("every `capital` entry must be an object")
    return blocks


def _find_row(
    rows: Sequence[Mapping[str, Any]], latency_ms: int | None, variant: str
) -> Mapping[str, Any] | None:
    """The row matching ``(latency_ms, variant)`` exactly, else ``None``."""
    if latency_ms is None:
        return None
    for row in rows:
        if (
            _as_int(row.get("latency_ms"), "latency_ms") == int(latency_ms)
            and str(row.get("capture_variant")) == variant
        ):
            return row
    return None


def _apr_by_capital(
    capital_runs: Sequence[Mapping[str, Any]],
    thresholds: Thresholds,
    latency_ms: int | None,
) -> tuple[tuple[float, float | None, float | None], ...]:
    """Per-capital-grid ``(capital, APR, APR CI lo)`` at the headline cell.

    A grid point the study did not run has no row and reports ``None`` — it is
    never approximated from a neighbouring capital.
    """
    out: list[tuple[float, float | None, float | None]] = []
    for cap in thresholds.capital.grid_usd:
        run = _exact_run(capital_runs, float(cap))
        row = (
            _find_row(run.get("oos", []), latency_ms, HEADLINE_VARIANT)
            if run is not None
            else None
        )
        out.append(
            (
                float(cap),
                _optional_float(row.get("apr")) if row else None,
                _optional_float(row.get("apr_ci90_lo")) if row else None,
            )
        )
    return tuple(out)


def _headline_block(
    front_matter: Mapping[str, Any], verdict: GradeResult
) -> dict[str, Any] | None:
    """The in-sample and out-of-sample headline rows, for the metrics table."""
    if verdict.headline_capital_usd is None or verdict.headline_latency_ms is None:
        return None
    run = _run_for_capital(front_matter, verdict.headline_capital_usd)
    if run is None:
        return None
    in_row = _find_row(run.get("in_sample", []), verdict.headline_latency_ms, HEADLINE_VARIANT)
    oos_row = _find_row(run.get("oos", []), verdict.headline_latency_ms, HEADLINE_VARIANT)
    if in_row is None and oos_row is None:
        return None
    return {"in_sample": in_row or {}, "oos": oos_row or {}}


def _run_for_capital(
    front_matter: Mapping[str, Any], capital: float | None
) -> Mapping[str, Any] | None:
    if capital is None:
        return None
    return _exact_run(_capital_runs(front_matter), float(capital))


def _latency_grid(front_matter: Mapping[str, Any], thresholds: Thresholds | None) -> list[int]:
    """The study's latency grid, or the default §13.4 grid as a fallback."""
    grid = front_matter.get("latency_grid_ms")
    if isinstance(grid, list) and grid:
        return [int(value) for value in grid]
    if thresholds is not None:
        return [thresholds.latency.headline_ms]
    return list(DEFAULT_LATENCIES_MS)


def _ceiling_latency(options: Sequence[int], value: int) -> int | None:
    """Smallest ``options`` value >= ``value``, or ``None`` when none exists.

    A study cannot be graded at a latency faster than the grid it evaluated, so
    the headline latency is rounded *up* to the next grid point; if it is above
    the whole grid the report is INCONCLUSIVE.
    """
    at_or_above = sorted({int(item) for item in options if int(item) >= int(value)})
    return at_or_above[0] if at_or_above else None


def _exact_run(
    capital_runs: Sequence[Mapping[str, Any]], capital: float
) -> Mapping[str, Any] | None:
    """The run whose capital equals ``capital``; no nearest-match fallback."""
    for block in capital_runs:
        if _as_float(block.get("capital_usd")) == float(capital):
            return block
    return None


def _nearest_capital_of(
    capital_runs: Mapping[float, pl.DataFrame], capital: float
) -> float:
    """Nearest key of a ``capital -> episodes`` mapping (display only, ties lower)."""
    ordered = sorted(float(key) for key in capital_runs)
    return min(ordered, key=lambda item: (abs(item - capital), item))


def _check_day_counts(front_matter: Mapping[str, Any], days: int) -> None:
    """Verify ``days`` equals the in-sample plus out-of-sample day lists (§13.5)."""
    in_dates = front_matter.get("in_sample_dates")
    oos_dates = front_matter.get("oos_dates")
    if in_dates is None and oos_dates is None:
        return
    total = len(in_dates or []) + len(oos_dates or [])
    if total != days:
        raise ReportError(
            f"days ({days}) must equal in_sample_dates + oos_dates ({total})"
        )


def _is_forward(front_matter: Mapping[str, Any]) -> bool:
    """Whether the front-matter proves forward provenance (§13.11).

    Only ``data_source == "forward"`` exactly, with no backfill sources and no
    fidelity class, counts as forward; anything else is HIST-PRELIM.
    """
    return (
        str(front_matter.get("data_source", "unknown")) == "forward"
        and not (front_matter.get("backfill_sources") or [])
        and front_matter.get("fidelity_class") is None
    )


def _qualifier(front_matter: Mapping[str, Any]) -> str:
    """The §13.11 qualifier, with the fidelity class and backfill sources."""
    parts: list[str] = []
    if front_matter.get("fidelity_class"):
        parts.append(str(front_matter["fidelity_class"]))
    parts.extend(str(source) for source in (front_matter.get("backfill_sources") or []))
    return f"{HIST_PRELIM} ({'; '.join(parts)})" if parts else HIST_PRELIM


# --------------------------------------------------------------------------
# Formatting helpers
# --------------------------------------------------------------------------


def _markdown_table(headers: Sequence[str], rows: Sequence[Sequence[str]]) -> list[str]:
    lines = [
        "| " + " | ".join(headers) + " |",
        "| " + " | ".join("---" for _ in headers) + " |",
    ]
    lines += ["| " + " | ".join(cells) + " |" for cells in rows]
    return lines


def _records_table(records: Sequence[Mapping[str, Any]]) -> list[str]:
    if not records:
        return ["_None._"]
    keys = list(records[0].keys())
    return _markdown_table(
        [key.replace("_", " ") for key in keys],
        [[_fmt_cell(record.get(key)) for key in keys] for record in records],
    )


def _render_kv(values: Mapping[str, Any]) -> list[str]:
    if not values:
        return ["", "_None._"]
    return [""] + [f"- {key}: {_fmt_cell(value)}" for key, value in values.items()]


def _render_bullets(values: Sequence[Any]) -> list[str]:
    if not values:
        return ["", "_None._"]
    return [""] + [f"- {value}" for value in values]


def _section_map(sections: Mapping[str, Any], key: str) -> Mapping[str, Any]:
    value = sections.get(key)
    return value if isinstance(value, Mapping) else {}


def _section_list(sections: Mapping[str, Any], key: str) -> list[Any]:
    value = sections.get(key)
    return list(value) if isinstance(value, list) else []


def _default_sections() -> dict[str, Any]:
    return {
        "data": {},
        "method": {},
        "artifacts": {},
        "sensitivity": [],
        "sanity": [],
        "top_episodes": [],
        "reproduce": "",
    }


def _json_safe(value: Any) -> Any:
    """Recursively make a value JSON/YAML-safe (finite floats, ISO dates, str keys).

    Keeps the in-memory front-matter identical to the serialized one, so it
    round-trips exactly: NaN/inf become ``None`` (``orjson``'s ``null``) and dates
    become ISO strings.
    """
    if isinstance(value, Mapping):
        return {str(key): _json_safe(item) for key, item in value.items()}
    if isinstance(value, (list, tuple)):
        return [_json_safe(item) for item in value]
    if isinstance(value, bool) or value is None:
        return value
    if isinstance(value, float):
        return None if math.isnan(value) or math.isinf(value) else value
    if isinstance(value, (_dt.datetime, _dt.date)):
        return value.isoformat()
    return value


def _fmt_cell(value: Any) -> str:
    if value is None:
        return "—"
    if isinstance(value, bool):
        return "yes" if value else "no"
    if isinstance(value, float):
        if math.isnan(value) or math.isinf(value):
            return "—"
        return f"{value:,.4g}"
    if isinstance(value, _dt.date):
        return value.isoformat()
    return str(value)


def _get(row: Mapping[str, Any] | None, key: str) -> Any:
    return row.get(key) if row else None


def _get_int(row: Mapping[str, Any] | None, key: str) -> int:
    return _as_int(row.get(key), key) if row else 0


def _fmt_pct(value: Any) -> str:
    number = _optional_float(value)
    if number is None:
        return "—"
    return f"{number * 100:.1f}%"


def _fmt_usd(value: Any) -> str:
    number = _optional_float(value)
    if number is None:
        return "—"
    return f"${number:,.2f}"


def _fmt_num(value: Any, digits: int) -> str:
    number = _optional_float(value)
    if number is None:
        return "—"
    return f"{number:.{digits}f}"


def _fmt_range(lo: Any, hi: Any) -> str:
    low = _fmt_usd(lo)
    high = _fmt_usd(hi)
    if low == "—" and high == "—":
        return "—"
    return f"{low} … {high}"


def _fmt_pair(row: Mapping[str, Any], low_key: str, high_key: str) -> str:
    return f"{_fmt_num(row.get(low_key), 2)} / {_fmt_num(row.get(high_key), 2)}"


# --------------------------------------------------------------------------
# Validation helpers
# --------------------------------------------------------------------------


def _clean_dates(dates: Sequence[_dt.date], where: str) -> list[_dt.date]:
    values = list(dates)
    if not values:
        raise ReportError(f"{where} must not be empty")
    if len(set(values)) != len(values):
        raise ReportError(f"{where} must not contain duplicates")
    return values


def _optional_float(value: Any) -> float | None:
    """Return ``value`` as a finite float, or ``None`` for missing/non-finite."""
    if value is None or isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    number = float(value)
    if math.isnan(number) or math.isinf(number):
        return None
    return number


def _as_float(value: Any) -> float:
    number = _optional_float(value)
    if number is None:
        raise ReportError(f"expected a number, got {value!r}")
    return number


def _as_int(value: Any, where: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int):
        raise ReportError(f"{where} must be an integer, got {value!r}")
    return value


def _as_key(runs: Mapping[float, pl.DataFrame], capital: float) -> float:
    """Find the mapping key matching ``capital`` without float-equality surprises."""
    for key in runs:
        if float(key) == capital:
            return key
    raise ReportError(f"no capital run for ${capital:,.0f}")


def write_daily_chart(daily: pl.DataFrame, path: Path, *, title: str = "") -> Path:
    """Write the §13.7 per-day captured-USD bar chart to ``path`` (PNG)."""
    if daily.height == 0 or "captured_usd" not in daily.columns:
        raise ReportError("daily frame needs a non-empty `captured_usd` column")
    import matplotlib

    matplotlib.use("Agg")
    import matplotlib.pyplot as plt

    path.parent.mkdir(parents=True, exist_ok=True)
    dates = [str(day) for day in daily["date"].to_list()]
    values = [float(value) for value in daily["captured_usd"].to_list()]
    figure, axes = plt.subplots(figsize=(max(6.0, len(dates) * 0.3), 3.0))
    axes.bar(range(len(dates)), values, color="#3b6ea5")
    axes.set_ylabel("captured USD/day")
    axes.set_title(title or "captured USD per day")
    step = max(1, len(dates) // 12)
    axes.set_xticks(range(0, len(dates), step))
    axes.set_xticklabels(dates[::step], rotation=90, fontsize=6)
    figure.tight_layout()
    figure.savefig(path, dpi=100)
    plt.close(figure)
    return path
