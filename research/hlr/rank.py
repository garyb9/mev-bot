"""``hlr-rank``: grade every study report and write ``RANKING.md`` (P-5, §13.6).

Scans ``research/reports/O*.md`` for the front-matter metrics P-5's
:mod:`hlr.report` writes, re-grades each study against
``research/thresholds.toml`` **at run time**, and writes the §13.6 ranking
(grouped PASS → MARGINAL → INCONCLUSIVE → FAIL, forward studies before
``HIST-PRELIM`` within each tier). Editing the thresholds and re-running
re-grades without a code change.

This is research code. It reads no keys and no network.
"""

from __future__ import annotations

import argparse
import math
import sys
from collections.abc import Mapping, Sequence
from pathlib import Path
from typing import Any

import polars as pl

from hlr.report import (
    RANKING_FILENAME,
    ReportError,
    canonical_digest,
    episode_provenance,
    is_forward,
    parse_front_matter,
    rank_studies,
    render_ranking,
    valid_backfill_sources,
)
from hlr.thresholds import ThresholdError, default_thresholds_path, load_thresholds

__all__ = ["build_ranking", "default_reports_dir", "main", "scan_reports"]


def default_reports_dir() -> Path:
    """Return the repository's ``research/reports`` directory."""
    return Path(__file__).resolve().parents[1] / "reports"


def scan_reports(reports_dir: str | Path) -> list[dict[str, Any]]:
    """Read the front-matter of every ``O*.md`` report in ``reports_dir``.

    Sorted by path so the scan itself is deterministic. Every report must declare
    its episode parquet and sha256 digest next to it; the digest is re-checked,
    provenance is re-derived from the parquet's ``source``/``fidelity`` columns
    (overriding the front-matter), and the declared episode counts and date range
    are verified. A problem with one report never aborts the run: parse and
    validation errors are recorded on ``_digest_issue`` and the report is kept so
    :func:`hlr.report.grade_study` returns INCONCLUSIVE with the reason.
    """
    directory = Path(reports_dir)
    studies: list[dict[str, Any]] = []
    for path in sorted(directory.glob("O*.md")):
        text = path.read_text(encoding="utf-8")
        if not text.lstrip().startswith("---"):
            continue
        try:
            front_matter = parse_front_matter(text)
        except (ReportError, TypeError, ValueError, AttributeError, KeyError) as err:
            studies.append(_malformed_front_matter(path, f"invalid front-matter: {err}"))
            continue
        front_matter["_report_path"] = str(path)
        try:
            front_matter["_digest_issue"] = _validate_episodes(front_matter, directory)
        except (ReportError, TypeError, ValueError, AttributeError, KeyError) as err:
            front_matter["_digest_issue"] = f"could not validate episode data: {err}"
        studies.append(front_matter)
    return studies


def _malformed_front_matter(path: Path, reason: str) -> dict[str, Any]:
    """A minimal report dict for a file whose front-matter could not be parsed."""
    return {
        "study_id": path.stem.split("-", 1)[0] or path.stem,
        "slug": "",
        "title": path.name,
        "_report_path": str(path),
        "_digest_issue": reason,
    }


def _validate_episodes(front_matter: dict[str, Any], directory: Path) -> str | None:
    """Re-check a report's declared episode data; return a problem reason or None.

    Enforces (in order): both digest fields declared, the parquet present and
    readable, the digest matching, then re-derives provenance from the parquet and
    verifies the declared per-capital episode counts and date range. Provenance
    fields are written back onto ``front_matter`` so grading uses the parquet, not
    a caller-asserted value.
    """
    digest = front_matter.get("episode_digest")
    parquet = front_matter.get("episode_parquet")
    if not digest or not parquet:
        return "report does not declare its episode data (episode_digest/episode_parquet)"
    candidate = Path(str(parquet))
    if not candidate.is_absolute():
        candidate = directory / candidate
    if not candidate.is_file():
        return f"declared episode parquet {parquet} is missing next to the report"
    try:
        frame = pl.read_parquet(candidate)
    except (OSError, pl.exceptions.PolarsError, ValueError):
        return f"episode parquet {parquet} is unreadable"
    if canonical_digest(frame) != digest:
        return "episode parquet digest does not match the report"

    provenance_issue = _merge_provenance(front_matter, frame)
    if provenance_issue is not None:
        return provenance_issue
    return _consistency_issue(front_matter, frame)


def _merge_provenance(front_matter: dict[str, Any], frame: pl.DataFrame) -> str | None:
    """Set the stricter of the declared and parquet-derived provenance.

    The front-matter is the floor: a parquet can never upgrade a study to
    forward. Forward requires **both** the declared front-matter and the parquet
    to be forward; otherwise the stricter (backfill, or unknown if either is
    unknown) label is kept, and the declared backfill sources are preserved
    (they include the robustness re-runs' provenance, which is not in the
    parquet). A malformed declared ``backfill_sources`` is a problem: the study is
    set to unknown and a reason is returned so grading is INCONCLUSIVE.
    """
    declared_sources = valid_backfill_sources(front_matter)
    if declared_sources is None:
        front_matter["data_source"] = "unknown"
        front_matter["backfill_sources"] = []
        front_matter["fidelity_class"] = None
        return "front-matter `backfill_sources` must be a list of strings"
    declared_forward = is_forward(front_matter)
    declared_source = str(front_matter.get("data_source", "unknown"))
    derived_source, derived_sources, derived_fidelity = episode_provenance([frame])
    if declared_forward and derived_source == "forward":
        data_source, backfill = "forward", ()
        fidelity = front_matter.get("fidelity_class") or derived_fidelity
    elif declared_source == "unknown" or derived_source == "unknown":
        data_source, backfill, fidelity = "unknown", (), None
    else:
        merged = sorted(set(declared_sources) | set(derived_sources))
        data_source, backfill = "backfill", tuple(merged)
        fidelity = front_matter.get("fidelity_class") or derived_fidelity
    front_matter["data_source"] = data_source
    front_matter["backfill_sources"] = list(backfill)
    front_matter["fidelity_class"] = fidelity
    return None


def _consistency_issue(front_matter: Mapping[str, Any], frame: pl.DataFrame) -> str | None:
    """Verify the parquet's date range and per-capital episode counts match."""
    declared = _string_set(front_matter.get("in_sample_dates")) | _string_set(
        front_matter.get("oos_dates")
    )
    if "date" not in frame.columns:
        return "episode parquet has no date column"
    frame_dates = {str(value) for value in frame["date"].to_list()}
    outside = sorted(frame_dates - declared)
    if outside:
        return f"episode parquet has dates outside the declared range: {outside}"

    blocks = front_matter.get("capital")
    if not isinstance(blocks, list) or not blocks:
        return None
    if "capital_usd" not in frame.columns:
        return "episode parquet has no capital_usd column"
    for block in blocks:
        if not isinstance(block, dict):
            return "front-matter capital entry is malformed"
        capital = _safe_float(block.get("capital_usd"))
        if capital is None:
            return "front-matter capital entry has a non-numeric capital_usd"
        expected = _declared_episodes(block)
        if expected is None:
            continue
        actual = frame.filter(pl.col("capital_usd").cast(pl.Float64) == capital).height
        if actual != expected:
            return (
                f"episode parquet has {actual} episodes at capital ${capital:,.0f} "
                f"but the report declares {expected}"
            )
    return None


def _safe_float(value: Any) -> float | None:
    """Return ``value`` as a finite float, or ``None`` for anything else."""
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    number = float(value)
    if math.isnan(number) or math.isinf(number):
        return None
    return number


def _string_set(value: Any) -> set[str]:
    """Return a list's items as strings; anything that is not a list is empty."""
    if not isinstance(value, list):
        return set()
    return {str(item) for item in value}


def _declared_episodes(block: Mapping[str, Any]) -> int | None:
    """Sum a capital block's declared in-sample and out-of-sample episode counts."""
    total = 0
    found = False
    for sample in ("in_sample", "oos"):
        rows = block.get(sample)
        if isinstance(rows, list) and rows and isinstance(rows[0], dict):
            value = rows[0].get("episodes")
            if isinstance(value, int) and not isinstance(value, bool):
                total += value
                found = True
    return total if found else None


def build_ranking(
    reports_dir: str | Path | None = None,
    thresholds_path: str | Path | None = None,
) -> tuple[str, int]:
    """Scan reports, grade them against the thresholds, return ``(markdown, n)``."""
    directory = Path(reports_dir) if reports_dir is not None else default_reports_dir()
    thresholds = load_thresholds(thresholds_path)
    results = rank_studies(scan_reports(directory), thresholds=thresholds)
    return render_ranking(results, thresholds), len(results)


def _build_parser() -> argparse.ArgumentParser:
    """Build the ``hlr-rank`` argument parser."""
    parser = argparse.ArgumentParser(
        prog="hlr-rank",
        description=(
            "Grade the SPEC-0008 study reports against research/thresholds.toml "
            "and write research/reports/RANKING.md."
        ),
    )
    parser.add_argument(
        "--reports-dir",
        default=None,
        metavar="PATH",
        help="directory holding O*.md reports (default: research/reports)",
    )
    parser.add_argument(
        "--thresholds",
        default=None,
        metavar="PATH",
        help=f"thresholds TOML (default: {default_thresholds_path()})",
    )
    parser.add_argument(
        "--out",
        default=None,
        metavar="PATH",
        help="output file (default: <reports-dir>/RANKING.md)",
    )
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    """CLI entry point for ``hlr-rank``."""
    args = _build_parser().parse_args(argv)
    directory = Path(args.reports_dir) if args.reports_dir is not None else default_reports_dir()
    try:
        text, count = build_ranking(directory, args.thresholds)
    except (ReportError, ThresholdError) as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2
    destination = Path(args.out) if args.out is not None else directory / RANKING_FILENAME
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_text(text, encoding="utf-8")
    print(f"wrote {destination} ({count} study report(s))")
    return 0


if __name__ == "__main__":  # pragma: no cover - exercised through the console script
    raise SystemExit(main())
