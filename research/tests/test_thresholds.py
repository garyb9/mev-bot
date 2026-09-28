"""Tests for :mod:`hlr.thresholds` (SPEC-0008 P-3, §13.6).

The committed ``research/thresholds.toml`` is loaded for the value checks; invalid
cases write a small file under ``tmp_path``. Nothing touches the network.
"""

from __future__ import annotations

from pathlib import Path

import pytest

from hlr.thresholds import (
    FAIL,
    MARGINAL,
    PASS,
    ThresholdError,
    default_thresholds_path,
    grade,
    load_thresholds,
)

_VALID = """\
[capital]
grid_usd = [10_000, 25_000]
headline_usd = 25_000
[apr]
target = 0.25
floor = 0.10
[quality]
min_episodes_per_day = 10
max_concentration = 0.40
min_coverage_pct = 0.90
robustness_buffer_multiplier = 2.0
[latency]
headline_ms = 250
[pilot]
max_capital_usd = 10_000
max_weeks = 4
max_loss_usd = 1_000
"""


def test_default_file_matches_spec() -> None:
    thresholds = load_thresholds()
    assert default_thresholds_path().is_file()
    assert thresholds.capital.grid_usd == (10_000, 25_000, 50_000, 100_000)
    assert thresholds.capital.headline_usd == 25_000
    assert thresholds.apr.target == pytest.approx(0.25)
    assert thresholds.apr.floor == pytest.approx(0.10)
    assert thresholds.quality.min_episodes_per_day == 10
    assert thresholds.quality.max_concentration == pytest.approx(0.40)
    assert thresholds.quality.min_coverage_pct == pytest.approx(0.90)
    assert thresholds.quality.robustness_buffer_multiplier == pytest.approx(2.0)
    assert thresholds.latency.headline_ms == 250
    assert thresholds.pilot.max_capital_usd == 10_000
    assert thresholds.pilot.max_weeks == 4
    assert thresholds.pilot.max_loss_usd == 1_000


@pytest.mark.parametrize(
    ("apr", "expected"),
    [
        (0.25, PASS),  # exactly target
        (0.50, PASS),
        (0.24, MARGINAL),
        (0.10, MARGINAL),  # exactly floor
        (0.099, FAIL),
        (0.0, FAIL),
        (-0.5, FAIL),
    ],
)
def test_grade_boundaries(apr: float, expected: str) -> None:
    assert grade(apr) == expected


def test_grade_uses_custom_thresholds(tmp_path: Path) -> None:
    path = tmp_path / "thresholds.toml"
    path.write_text(_VALID)
    thresholds = load_thresholds(path)
    assert grade(0.25, thresholds=thresholds) == PASS
    assert grade(0.10, thresholds=thresholds) == MARGINAL
    assert grade(0.05, thresholds=thresholds) == FAIL


def test_grade_rejects_non_number() -> None:
    with pytest.raises(ThresholdError, match="apr"):
        grade("high")  # type: ignore[arg-type]


def test_floor_above_target_is_rejected(tmp_path: Path) -> None:
    path = tmp_path / "thresholds.toml"
    path.write_text(_VALID.replace("floor = 0.10", "floor = 0.30"))
    with pytest.raises(ThresholdError, match="floor"):
        load_thresholds(path)


def test_missing_file_raises(tmp_path: Path) -> None:
    with pytest.raises(ThresholdError, match="not found"):
        load_thresholds(tmp_path / "nope.toml")


def test_missing_key_raises(tmp_path: Path) -> None:
    path = tmp_path / "thresholds.toml"
    path.write_text(_VALID.replace("headline_usd = 25_000\n", ""))
    with pytest.raises(ThresholdError, match="headline_usd"):
        load_thresholds(path)


def test_ill_typed_key_raises(tmp_path: Path) -> None:
    path = tmp_path / "thresholds.toml"
    path.write_text(_VALID.replace("target = 0.25", 'target = "high"'))
    with pytest.raises(ThresholdError, match="must be a number"):
        load_thresholds(path)


def test_out_of_range_fraction_raises(tmp_path: Path) -> None:
    path = tmp_path / "thresholds.toml"
    path.write_text(_VALID.replace("min_coverage_pct = 0.90", "min_coverage_pct = 1.5"))
    with pytest.raises(ThresholdError, match="<= 1"):
        load_thresholds(path)


def test_empty_grid_raises(tmp_path: Path) -> None:
    path = tmp_path / "thresholds.toml"
    path.write_text(_VALID.replace("grid_usd = [10_000, 25_000]", "grid_usd = []"))
    with pytest.raises(ThresholdError, match="positive integers"):
        load_thresholds(path)


def test_invalid_toml_raises(tmp_path: Path) -> None:
    path = tmp_path / "thresholds.toml"
    path.write_text("[capital\n")
    with pytest.raises(ThresholdError, match="invalid TOML"):
        load_thresholds(path)
