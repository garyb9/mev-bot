#!/usr/bin/env python3
"""Warn-only criterion benchmark regression check (SPEC-0000 C-3, SPEC-0002 H-7).

Reads the criterion `new` estimates under `target/criterion/**/new/estimates.json`
and compares each benchmark's mean (ns) against a baseline committed at
`benches/baseline.json`. A benchmark slower than the baseline by more than the
threshold (default 15%) produces a GitHub `::warning::` annotation and a row in
the step summary. The build is **never** failed on a regression: this process
exits non-zero only if it cannot read the results at all (e.g. the benches did
not run), which means the bench job itself failed to build or run.

Only the Python standard library is used.

Baselines are host-specific. The committed baseline was generated on the
GitHub Actions runner (see the ``host`` field in ``benches/baseline.json``), and
a local dev host is usually faster, so a local run can appear faster than the
baseline. Re-measure on the intended host class before treating a delta as a
regression.

Each bench target is run under its own absolute ``CRITERION_HOME`` because some
benchmark ids collide across binaries (for example ``decode/l2Book`` exists in
both the client and engine benches) and a shared output directory would let the
last writer overwrite the first. Reproduce the baseline on any host with:

    export MEV_BENCH_QUICK=1
    root="$PWD/target/criterion"
    CRITERION_HOME="$root/hl-decode"         cargo bench -p hl-arb-client --bench decode -- --quick
    CRITERION_HOME="$root/hl-sign"           cargo bench -p hl-arb-client --bench sign   -- --quick
    CRITERION_HOME="$root/engine-ingest"     cargo bench -p hl-arb-engine    --bench ingest -- --quick
    CRITERION_HOME="$root/bot-engine"        cargo bench -p hl-arb-bot       --bench engine -- --quick
    CRITERION_HOME="$root/recorder-segment"  cargo bench -p hl-arb-recorder  --bench segment -- --quick
    python3 scripts/bench_check.py --write-baseline

Usage:
    python3 scripts/bench_check.py [--criterion-dir DIR] [--baseline FILE]
                                   [--threshold 0.15] [--label NAME]
    python3 scripts/bench_check.py --write-baseline [--baseline FILE]
    python3 scripts/bench_check.py --self-test
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import sys
import tempfile
from datetime import datetime, timezone
from pathlib import Path

DEFAULT_THRESHOLD = 0.15


def discover(criterion_dir: Path) -> dict[str, float]:
    """Return ``{bench_id: mean_ns}`` from every ``new/estimates.json`` file."""
    if not criterion_dir.is_dir():
        raise SystemExit(
            f"error: criterion dir not found: {criterion_dir} (did the benches run?)"
        )
    out: dict[str, float] = {}
    for estimates in sorted(criterion_dir.rglob("estimates.json")):
        if estimates.parent.name != "new":
            continue
        bench_id = estimates.parent.parent.relative_to(criterion_dir).as_posix()
        try:
            data = json.loads(estimates.read_text(encoding="utf-8"))
            mean = float(data["mean"]["point_estimate"])
        except (OSError, ValueError, KeyError, TypeError) as exc:
            raise SystemExit(f"error: cannot read {estimates}: {exc}")
        out[bench_id] = mean
    if not out:
        raise SystemExit(f"error: no criterion estimates under {criterion_dir}")
    return out


def load_baseline(path: Path) -> dict[str, float]:
    """Load a baseline, accepting the wrapped ``{"benches": {...}}`` form."""
    if not path.is_file():
        return {}
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
        benches = data.get("benches", data) if isinstance(data, dict) else {}
        return {str(k): float(v) for k, v in benches.items()}
    except (OSError, ValueError, KeyError, TypeError) as exc:
        raise SystemExit(f"error: cannot read baseline {path}: {exc}")


def compare(
    current: dict[str, float], baseline: dict[str, float], threshold: float
) -> list[dict]:
    """Compare each current bench to the baseline; mark regressions."""
    rows = []
    for bench_id in sorted(current):
        now = current[bench_id]
        base = baseline.get(bench_id)
        if base is None or base <= 0:
            rows.append(
                {
                    "id": bench_id,
                    "baseline": None,
                    "current": now,
                    "delta": None,
                    "regressed": False,
                }
            )
        else:
            delta = (now - base) / base
            rows.append(
                {
                    "id": bench_id,
                    "baseline": base,
                    "current": now,
                    "delta": delta,
                    "regressed": delta > threshold,
                }
            )
    return rows


def fmt_ns(ns: float) -> str:
    """Human-readable duration for a nanosecond count."""
    if ns < 1_000:
        return f"{ns:.0f} ns"
    if ns < 1_000_000:
        return f"{ns / 1_000:.1f} µs"
    if ns < 1_000_000_000:
        return f"{ns / 1_000_000:.2f} ms"
    return f"{ns / 1_000_000_000:.2f} s"


def render(rows: list[dict], threshold: float, label: str) -> tuple[str, list[str]]:
    """Return a markdown report and the list of GitHub warning annotations."""
    regressions = [r for r in rows if r["regressed"]]
    new = [r for r in rows if r["baseline"] is None]
    lines = [
        "### Bench check: quick mode vs committed baseline",
        "",
        f"- host: `{label}`",
        f"- threshold: {threshold * 100:.0f}% slower (warn-only, never fails the build)",
        f"- benchmarks: {len(rows)} ({len(regressions)} regressed, {len(new)} without baseline)",
        "",
        "| Benchmark | Baseline | Current | Change | Status |",
        "|---|---:|---:|---:|---|",
    ]
    for row in rows:
        if row["baseline"] is None:
            lines.append(
                f"| `{row['id']}` | — | {fmt_ns(row['current'])} | — | new |"
            )
        else:
            delta = row["delta"]
            sign = "+" if delta >= 0 else ""
            status = "REGRESSION" if row["regressed"] else "ok"
            lines.append(
                f"| `{row['id']}` | {fmt_ns(row['baseline'])} | {fmt_ns(row['current'])} "
                f"| {sign}{delta * 100:.1f}% | {status} |"
            )
    warnings = [
        "::warning::bench {id} is {pct}% slower than baseline ({base} -> {now})".format(
            id=r["id"],
            pct=f"{r['delta'] * 100:.1f}",
            base=fmt_ns(r["baseline"]),
            now=fmt_ns(r["current"]),
        )
        for r in regressions
    ]
    return "\n".join(lines) + "\n", warnings


def write_baseline(path: Path, current: dict[str, float], label: str) -> None:
    """Write the current means as the new baseline, with host metadata."""
    payload = {
        "generated_at": datetime.now(timezone.utc).isoformat(timespec="seconds"),
        "host": label,
        "quick_mode": True,
        "benches": {k: round(v, 3) for k, v in sorted(current.items())},
    }
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
    print(f"wrote baseline: {path} ({len(current)} benches, host={label})")


def self_test() -> int:
    """Build a fake criterion tree and assert one regression is flagged."""
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp) / "criterion"

        def put(bench: str, ns: float) -> None:
            target = root / bench / "new"
            target.mkdir(parents=True, exist_ok=True)
            (target / "estimates.json").write_text(
                json.dumps({"mean": {"point_estimate": ns}}), encoding="utf-8"
            )
            # A stray non-`new` estimate must be ignored.
            base = root / bench / "base"
            base.mkdir(parents=True, exist_ok=True)
            (base / "estimates.json").write_text(
                json.dumps({"mean": {"point_estimate": 1.0}}), encoding="utf-8"
            )

        put("sign_one_order", 100_000.0)  # unchanged
        put("bbo_to_action", 90_000.0)  # faster
        put("group/decode_send_dispatch", 240_000.0)  # 20% slower
        baseline = {
            "sign_one_order": 100_000.0,
            "bbo_to_action": 100_000.0,
            "group/decode_send_dispatch": 200_000.0,
        }

        rows = compare(discover(root), baseline, DEFAULT_THRESHOLD)
        assert len(rows) == 3, rows
        regressed = [r["id"] for r in rows if r["regressed"]]
        assert regressed == ["group/decode_send_dispatch"], regressed

        markdown, warnings = render(rows, DEFAULT_THRESHOLD, "self-test")
        assert len(warnings) == 1, warnings
        assert "group/decode_send_dispatch" in warnings[0], warnings
        assert "REGRESSION" in markdown
        print(markdown)
        # Keep the `::warning::` token out of CI stdout: the runner would turn
        # this expected self-test line into a real annotation on the job.
        print(f"expected warning: {warnings[0].removeprefix('::warning::')}")
        print("self-test: PASS")

        # A written baseline round-trips through load_baseline.
        out = Path(tmp) / "baseline.json"
        write_baseline(out, discover(root), "self-test")
        reloaded = load_baseline(out)
        assert reloaded == discover(root), (reloaded, discover(root))
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument(
        "--criterion-dir", type=Path, default=Path("target/criterion")
    )
    parser.add_argument("--baseline", type=Path, default=Path("benches/baseline.json"))
    parser.add_argument("--threshold", type=float, default=DEFAULT_THRESHOLD)
    parser.add_argument(
        "--label", default=os.environ.get("BENCH_LABEL", platform.node() or "unknown")
    )
    parser.add_argument("--write-baseline", action="store_true")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args(argv)

    if args.self_test:
        return self_test()

    current = discover(args.criterion_dir)
    if args.write_baseline:
        write_baseline(args.baseline, current, args.label)
        return 0

    baseline = load_baseline(args.baseline)
    rows = compare(current, baseline, args.threshold)
    markdown, warnings = render(rows, args.threshold, args.label)
    print(markdown)
    for warning in warnings:
        print(warning)
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a", encoding="utf-8") as handle:
            handle.write(markdown + "\n")
    if not baseline:
        print(f"note: no baseline at {args.baseline}; nothing to compare")
    return 0


if __name__ == "__main__":
    sys.exit(main())
