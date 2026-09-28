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
import sys
from collections.abc import Mapping, Sequence
from pathlib import Path
from typing import Any

import polars as pl

from hlr.report import (
    RANKING_FILENAME,
    ReportError,
    canonical_digest,
    parse_front_matter,
    rank_studies,
    render_ranking,
)
from hlr.thresholds import ThresholdError, default_thresholds_path, load_thresholds

__all__ = ["build_ranking", "default_reports_dir", "main", "scan_reports"]


def default_reports_dir() -> Path:
    """Return the repository's ``research/reports`` directory."""
    return Path(__file__).resolve().parents[1] / "reports"


def scan_reports(reports_dir: str | Path) -> list[dict[str, Any]]:
    """Read the front-matter of every ``O*.md`` report in ``reports_dir``.

    Sorted by path so the scan itself is deterministic. If a report names its
    episode parquet and that file sits next to it, the digest is re-checked and a
    mismatch is recorded so :func:`hlr.report.grade_study` returns INCONCLUSIVE.
    A malformed report raises :class:`hlr.report.ReportError` naming the file.
    """
    directory = Path(reports_dir)
    studies: list[dict[str, Any]] = []
    for path in sorted(directory.glob("O*.md")):
        text = path.read_text(encoding="utf-8")
        if not text.lstrip().startswith("---"):
            continue
        try:
            front_matter = parse_front_matter(text)
        except ReportError as err:
            raise ReportError(f"{path}: {err}") from err
        front_matter["_report_path"] = str(path)
        front_matter["_digest_mismatch"] = _digest_mismatch(front_matter, directory)
        studies.append(front_matter)
    return studies


def _digest_mismatch(front_matter: Mapping[str, Any], directory: Path) -> bool:
    """Whether the report's named episode parquet is present and does not match.

    An absent parquet cannot be checked, so it is not a mismatch; a present but
    unreadable one is.
    """
    parquet = front_matter.get("episode_parquet")
    if not parquet:
        return False
    candidate = Path(str(parquet))
    if not candidate.is_absolute():
        candidate = directory / candidate
    if not candidate.is_file():
        return False
    try:
        frame = pl.read_parquet(candidate)
    except (OSError, pl.exceptions.PolarsError, ValueError):
        return True
    return canonical_digest(frame) != front_matter.get("episode_digest")


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
