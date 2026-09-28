"""Owner-adjustable study thresholds (task P-3, SPEC-0008 §13.6).

Loads and validates ``research/thresholds.toml`` and exposes :func:`grade`, the
APR tiering every study uses. The file is the single source of truth for the
capital grid, the APR target/floor, the quality gates, the headline latency, and
the pilot caps, so the owner can re-grade every study by editing data and
re-running ``uv run hlr-rank``.

:func:`grade` implements the APR part of the §13.6 verdict only. The full verdict
also requires the ``[quality]`` criteria and the 90% CI lower bound, which the
report/rank code applies using these thresholds.

This is research code. It is never imported by, or deployed with, the bot.
"""

from __future__ import annotations

import tomllib
from dataclasses import dataclass
from functools import lru_cache
from pathlib import Path
from typing import Any

__all__ = [
    "AprThresholds",
    "CapitalThresholds",
    "LatencyThresholds",
    "PilotThresholds",
    "QualityThresholds",
    "ThresholdError",
    "Thresholds",
    "default_thresholds_path",
    "grade",
    "load_thresholds",
]

#: The §13.6 verdict tiers (APR part).
PASS = "PASS"
MARGINAL = "MARGINAL"
FAIL = "FAIL"


class ThresholdError(ValueError):
    """``thresholds.toml`` is missing, malformed, or has an out-of-range value."""


@dataclass(frozen=True)
class CapitalThresholds:
    """The capital grid the studies evaluate and its headline point."""

    grid_usd: tuple[int, ...]
    headline_usd: int


@dataclass(frozen=True)
class AprThresholds:
    """The APR profit bar: ``floor <= APR < target`` is MARGINAL."""

    target: float
    floor: float


@dataclass(frozen=True)
class QualityThresholds:
    """The non-APR §13.6 gates a study must also pass."""

    min_episodes_per_day: int
    max_concentration: float
    min_coverage_pct: float
    robustness_buffer_multiplier: float


@dataclass(frozen=True)
class LatencyThresholds:
    """The headline latency used for the verdict (ms)."""

    headline_ms: int


@dataclass(frozen=True)
class PilotThresholds:
    """The §13.6 time-boxed live-pilot caps."""

    max_capital_usd: int
    max_weeks: int
    max_loss_usd: int


@dataclass(frozen=True)
class Thresholds:
    """The validated contents of ``research/thresholds.toml``."""

    capital: CapitalThresholds
    apr: AprThresholds
    quality: QualityThresholds
    latency: LatencyThresholds
    pilot: PilotThresholds


def default_thresholds_path() -> Path:
    """Return the repository's ``research/thresholds.toml`` path."""
    return Path(__file__).resolve().parents[1] / "thresholds.toml"


def load_thresholds(path: str | Path | None = None) -> Thresholds:
    """Load and validate ``thresholds.toml``.

    ``path`` defaults to :func:`default_thresholds_path`. A missing file, invalid
    TOML, missing/ill-typed key, or out-of-range value raises
    :class:`ThresholdError` naming the file and key.
    """
    path = Path(path) if path is not None else default_thresholds_path()
    raw = _read_toml(path)
    where = str(path)
    capital = _table(raw, "capital", where)
    apr = _table(raw, "apr", where)
    quality = _table(raw, "quality", where)
    latency = _table(raw, "latency", where)
    pilot = _table(raw, "pilot", where)

    target = _number(apr, "target", where, min_value=0.0)
    floor = _number(apr, "floor", where, min_value=0.0)
    if floor > target:
        raise ThresholdError(f"{where}:apr `floor` ({floor}) must not exceed `target` ({target})")

    return Thresholds(
        capital=CapitalThresholds(
            grid_usd=_positive_int_list(capital, "grid_usd", where),
            headline_usd=_positive_int(capital, "headline_usd", where),
        ),
        apr=AprThresholds(target=target, floor=floor),
        quality=QualityThresholds(
            min_episodes_per_day=_positive_int(quality, "min_episodes_per_day", where),
            max_concentration=_fraction(quality, "max_concentration", where),
            min_coverage_pct=_fraction(quality, "min_coverage_pct", where),
            robustness_buffer_multiplier=_number(
                quality, "robustness_buffer_multiplier", where, min_value=0.0
            ),
        ),
        latency=LatencyThresholds(
            headline_ms=_positive_int(latency, "headline_ms", where),
        ),
        pilot=PilotThresholds(
            max_capital_usd=_positive_int(pilot, "max_capital_usd", where),
            max_weeks=_positive_int(pilot, "max_weeks", where),
            max_loss_usd=_positive_int(pilot, "max_loss_usd", where),
        ),
    )


def grade(apr: float, *, thresholds: Thresholds | None = None) -> str:
    """Grade an APR against the §13.6 bar: PASS, MARGINAL, or FAIL.

    ``apr >= target`` is PASS; ``floor <= apr < target`` is MARGINAL; anything
    below the floor is FAIL. Boundaries are inclusive on the lower tier:
    ``apr == target`` is PASS and ``apr == floor`` is MARGINAL.
    """
    thresholds = thresholds if thresholds is not None else _default_thresholds()
    if isinstance(apr, bool) or not isinstance(apr, (int, float)):
        raise ThresholdError(f"apr must be a number, got {type(apr).__name__}")
    value = float(apr)
    if value >= thresholds.apr.target:
        return PASS
    if value >= thresholds.apr.floor:
        return MARGINAL
    return FAIL


@lru_cache(maxsize=1)
def _default_thresholds() -> Thresholds:
    """Load the repository's ``thresholds.toml`` once per process."""
    return load_thresholds()


def _read_toml(path: Path) -> dict[str, Any]:
    """Read a TOML file, mapping I/O and parse failures to :class:`ThresholdError`."""
    try:
        with open(path, "rb") as handle:
            parsed = tomllib.load(handle)
    except FileNotFoundError as err:
        raise ThresholdError(f"thresholds file not found: {path}") from err
    except OSError as err:
        raise ThresholdError(f"cannot read thresholds file {path}: {err}") from err
    except tomllib.TOMLDecodeError as err:
        raise ThresholdError(f"invalid TOML in thresholds file {path}: {err}") from err
    if not isinstance(parsed, dict):
        raise ThresholdError(f"thresholds file {path} is not a TOML table")
    return parsed


def _table(raw: dict[str, Any], key: str, where: str) -> dict[str, Any]:
    """Return ``raw[key]`` as a TOML table or raise :class:`ThresholdError`."""
    value = raw.get(key)
    if value is None:
        raise ThresholdError(f"{where}: missing table `{key}`")
    if not isinstance(value, dict):
        raise ThresholdError(f"{where}: `{key}` must be a table")
    return value


def _number(
    table: dict[str, Any], key: str, where: str, *, min_value: float
) -> float:
    """Return ``table[key]`` as a float no smaller than ``min_value``."""
    if key not in table:
        raise ThresholdError(f"{where}: missing key `{key}`")
    value = table[key]
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise ThresholdError(f"{where}: `{key}` must be a number, got {type(value).__name__}")
    result = float(value)
    if result < min_value:
        raise ThresholdError(f"{where}: `{key}` must be >= {min_value}, got {result}")
    return result


def _positive_int(table: dict[str, Any], key: str, where: str) -> int:
    """Return ``table[key]`` as an int > 0."""
    if key not in table:
        raise ThresholdError(f"{where}: missing key `{key}`")
    value = table[key]
    if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
        raise ThresholdError(f"{where}: `{key}` must be a positive integer, got {value!r}")
    return value


def _positive_int_list(table: dict[str, Any], key: str, where: str) -> tuple[int, ...]:
    """Return ``table[key]`` as a non-empty list of positive ints."""
    if key not in table:
        raise ThresholdError(f"{where}: missing key `{key}`")
    value = table[key]
    valid = (
        isinstance(value, list)
        and len(value) > 0
        and all(
            isinstance(item, int) and not isinstance(item, bool) and item > 0
            for item in value
        )
    )
    if not valid:
        raise ThresholdError(f"{where}: `{key}` must be a non-empty list of positive integers")
    return tuple(value)


def _fraction(table: dict[str, Any], key: str, where: str) -> float:
    """Return ``table[key]`` as a float in ``[0, 1]``."""
    value = _number(table, key, where, min_value=0.0)
    if value > 1.0:
        raise ThresholdError(f"{where}: `{key}` must be <= 1, got {value}")
    return value
