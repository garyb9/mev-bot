"""Cost model for the SPEC-0008 studies (task P-3, SPEC-0008 §13.2).

Every fee lives in ``research/costs.toml`` as data, so a study never hardcodes
one. This module loads and validates that file (no network, no keys) and exposes
the small API the studies use:

* :func:`taker_bps` / :func:`maker_bps` — the base fee for a ``(venue, market)``
  leg, including the stable-pair scaling for HL spot;
* :func:`hip3_fee_bps` — the HIP-3 perp formula, exactly as SPEC-0008 §13.2
  defines it;
* :func:`round_trip_bps` — two taker legs plus the safety buffer.

``market`` is named as SPEC-0008 §13.1: an HL perp is ``BTC``, a HIP-3 perp is
``xyz:TSLA``, an HL spot pair is ``BASE/QUOTE`` (for example ``USDT0/USDC``), and
a CEX market is ``BTCUSDT``. Values are floats: research is exempt from
``rust_decimal`` (SPEC-0008 §13.1).

This is research code. It is never imported by, or deployed with, the bot.
"""

from __future__ import annotations

import tomllib
from dataclasses import dataclass, replace
from functools import lru_cache
from pathlib import Path
from typing import Any

__all__ = [
    "BINANCE_USDM",
    "BYBIT_LINEAR",
    "HL_HIP3",
    "HL_PERP",
    "HL_SPOT",
    "CostError",
    "Costs",
    "FeeSchedule",
    "Hip3Fees",
    "default_costs_path",
    "hip3_fee_bps",
    "load_costs",
    "maker_bps",
    "round_trip_bps",
    "taker_bps",
]

#: Venue ids. HL perp matches the §13.1 ``venue="hl"`` tag.
HL_PERP = "hl"
HL_SPOT = "hl-spot"
HL_HIP3 = "hl-hip3"
BINANCE_USDM = "binance-usdm"
BYBIT_LINEAR = "bybit-linear"

_TAKER = "taker"
_MAKER = "maker"


class CostError(ValueError):
    """``costs.toml`` is missing, malformed, or references an unknown venue.

    Raised with the offending file and key in the message so a bad edit fails
    loudly instead of silently mis-pricing a study.
    """


@dataclass(frozen=True)
class FeeSchedule:
    """A base-tier two-sided fee schedule in bps, with its provenance.

    ``stable_pair_scale`` / ``quote_assets`` are set for HL spot only;
    ``bnb_discount_multiplier`` is set for Binance USDM only.
    """

    taker_bps: float
    maker_bps: float
    source: str
    verified: str
    stable_pair_scale: float | None = None
    quote_assets: tuple[str, ...] = ()
    bnb_discount_multiplier: float | None = None


@dataclass(frozen=True)
class Hip3Fees:
    """The HIP-3 fee formula inputs and the V-3 observed per-dex fee scales."""

    base_taker_bps: float
    base_maker_bps: float
    growth_mode_scale: float
    growth_mode_share: float
    dexes: dict[str, float]
    source: str
    verified: str


@dataclass(frozen=True)
class Costs:
    """The validated contents of ``research/costs.toml``."""

    buffer_bps: float
    hl: FeeSchedule
    hl_spot: FeeSchedule
    binance_usdm: FeeSchedule
    bybit_linear: FeeSchedule
    hip3: Hip3Fees


def default_costs_path() -> Path:
    """Return the repository's ``research/costs.toml`` path."""
    return Path(__file__).resolve().parents[1] / "costs.toml"


def load_costs(path: str | Path | None = None) -> Costs:
    """Load and validate ``costs.toml``.

    ``path`` defaults to :func:`default_costs_path`. A missing file, invalid
    TOML, missing/ill-typed key, or empty required list raises
    :class:`CostError` naming the file and key.
    """
    path = Path(path) if path is not None else default_costs_path()
    raw = _read_toml(path, "costs")
    where = str(path)
    buffer_bps = _number(raw, "buffer_bps", where, min_value=0.0)
    hl = _parse_fee(raw, "hl", where)
    hl_spot = _parse_spot(raw, where)
    binance_usdm = _parse_binance(raw, where)
    bybit_linear = _parse_fee(raw, "bybit-linear", where)
    hip3 = _parse_hip3(raw, where)
    return Costs(
        buffer_bps=buffer_bps,
        hl=hl,
        hl_spot=hl_spot,
        binance_usdm=binance_usdm,
        bybit_linear=bybit_linear,
        hip3=hip3,
    )


def hip3_fee_bps(
    side: str,
    deployer_fee_scale: float,
    growth_mode: bool,
    *,
    costs: Costs | None = None,
) -> float:
    """Return the HIP-3 perp fee in bps for one leg (SPEC-0008 §13.2).

    ``side`` is ``"taker"`` or ``"maker"``. The formula is::

        scaleIfHip3 = scale + 1        if scale < 1
                      = 2 × scale      otherwise
        fee = base × scaleIfHip3 × (growth_mode_scale if growth_mode else 1)

    so ``scale=1.0`` gives 9.0/3.0 bps (0.9/0.3 in growth mode), ``scale=0.5``
    gives 6.75/2.25, and the observed ``hyna`` scale 0.1111 gives 5.0/1.67.
    """
    costs = costs if costs is not None else _default_costs()
    if side not in (_TAKER, _MAKER):
        raise CostError(f"side must be `taker` or `maker`, got `{side}`")
    scale = _float_value(deployer_fee_scale, "deployer_fee_scale")
    if scale < 0.0:
        raise CostError(f"deployer_fee_scale must be >= 0, got {scale}")
    scale_if_hip3 = scale + 1.0 if scale < 1.0 else 2.0 * scale
    factor = scale_if_hip3 * (costs.hip3.growth_mode_scale if growth_mode else 1.0)
    base = costs.hip3.base_taker_bps if side == _TAKER else costs.hip3.base_maker_bps
    return base * factor


def taker_bps(
    venue: str,
    market: str,
    *,
    bnb_discount: bool = False,
    costs: Costs | None = None,
) -> float:
    """Return the taker fee in bps for one ``(venue, market)`` leg.

    ``bnb_discount`` applies the Binance BNB ×0.9 multiplier; it is only valid
    for :data:`BINANCE_USDM` (else :class:`CostError`). HIP-3 legs use the
    conservative no-growth fee; call :func:`hip3_fee_bps` directly for growth.
    """
    return _fee_bps(venue, market, _TAKER, bnb_discount=bnb_discount, costs=costs)


def maker_bps(
    venue: str,
    market: str,
    *,
    bnb_discount: bool = False,
    costs: Costs | None = None,
) -> float:
    """Return the maker fee in bps for one ``(venue, market)`` leg.

    Same rules as :func:`taker_bps`; ``bnb_discount`` is Binance-only.
    """
    return _fee_bps(venue, market, _MAKER, bnb_discount=bnb_discount, costs=costs)


def round_trip_bps(
    leg_a: tuple[str, str],
    leg_b: tuple[str, str],
    *,
    buffer_bps: float | None = None,
    costs: Costs | None = None,
) -> float:
    """Return two taker legs plus the safety buffer, in bps.

    Each leg is a ``(venue, market)`` pair (SPEC-0008 §13.3, default execution
    assumption is taker on every leg). ``buffer_bps`` defaults to the value in
    ``costs.toml`` (2 bps).
    """
    costs = costs if costs is not None else _default_costs()
    leg_a_bps = taker_bps(leg_a[0], leg_a[1], costs=costs)
    leg_b_bps = taker_bps(leg_b[0], leg_b[1], costs=costs)
    buffer = costs.buffer_bps if buffer_bps is None else _float_value(buffer_bps, "buffer_bps")
    return leg_a_bps + leg_b_bps + buffer


@lru_cache(maxsize=1)
def _default_costs() -> Costs:
    """Load the repository's ``costs.toml`` once per process."""
    return load_costs()


def _fee_bps(
    venue: str,
    market: str,
    side: str,
    *,
    bnb_discount: bool,
    costs: Costs | None,
) -> float:
    """Shared venue dispatch for :func:`taker_bps` and :func:`maker_bps`."""
    costs = costs if costs is not None else _default_costs()
    if venue == HL_PERP:
        return _side_fee(costs.hl, side)
    if venue == HL_SPOT:
        base = _side_fee(costs.hl_spot, side)
        if _is_stable_pair(market, costs.hl_spot):
            scale = costs.hl_spot.stable_pair_scale
            if scale is None:
                raise CostError("hl-spot is missing `stable_pair_scale`")
            base *= scale
        return base
    if venue == HL_HIP3:
        dex = _hip3_dex(market)
        scale = costs.hip3.dexes.get(dex)
        if scale is None:
            raise CostError(f"unknown HIP-3 dex `{dex}` in market `{market}`")
        return hip3_fee_bps(side, scale, growth_mode=False, costs=costs)
    if venue == BINANCE_USDM:
        if not bnb_discount:
            return _side_fee(costs.binance_usdm, side)
        multiplier = costs.binance_usdm.bnb_discount_multiplier
        if multiplier is None:
            raise CostError("binance-usdm is missing `bnb_discount_multiplier`")
        return _side_fee(costs.binance_usdm, side) * multiplier
    if bnb_discount:
        raise CostError(f"bnb_discount applies only to `{BINANCE_USDM}`, not `{venue}`")
    if venue == BYBIT_LINEAR:
        return _side_fee(costs.bybit_linear, side)
    raise CostError(f"unknown venue `{venue}`")


def _side_fee(schedule: FeeSchedule, side: str) -> float:
    """Return one side of a plain fee schedule."""
    return schedule.taker_bps if side == _TAKER else schedule.maker_bps


def _is_stable_pair(market: str, schedule: FeeSchedule) -> bool:
    """Whether a ``BASE/QUOTE`` spot market is a pair of two quote assets."""
    if "/" not in market:
        raise CostError(f"spot market `{market}` must be `BASE/QUOTE`")
    base, quote = market.split("/", 1)
    return base in schedule.quote_assets and quote in schedule.quote_assets


def _hip3_dex(market: str) -> str:
    """Return the dex prefix of a HIP-3 market name such as ``xyz:TSLA``."""
    if ":" not in market:
        raise CostError(f"HIP-3 market `{market}` must be `dex:COIN`")
    return market.split(":", 1)[0]


def _read_toml(path: Path, label: str) -> dict[str, Any]:
    """Read a TOML file, mapping I/O and parse failures to :class:`CostError`."""
    try:
        with open(path, "rb") as handle:
            parsed = tomllib.load(handle)
    except FileNotFoundError as err:
        raise CostError(f"{label} file not found: {path}") from err
    except OSError as err:
        raise CostError(f"cannot read {label} file {path}: {err}") from err
    except tomllib.TOMLDecodeError as err:
        raise CostError(f"invalid TOML in {label} file {path}: {err}") from err
    if not isinstance(parsed, dict):
        raise CostError(f"{label} file {path} is not a TOML table")
    return parsed


def _parse_fee(raw: dict[str, Any], section: str, where: str) -> FeeSchedule:
    """Parse one ``[section]`` with ``taker_bps``/``maker_bps`` and provenance."""
    table = _table(raw, section, where)
    origin = f"{where}:{section}"
    return FeeSchedule(
        taker_bps=_number(table, "taker_bps", origin, min_value=0.0),
        maker_bps=_number(table, "maker_bps", origin, min_value=0.0),
        source=_string(table, "source", origin),
        verified=_string(table, "verified", origin),
    )


def _parse_spot(raw: dict[str, Any], where: str) -> FeeSchedule:
    """Parse ``[hl-spot]``, adding the stable-pair scale and quote set."""
    schedule = _parse_fee(raw, "hl-spot", where)
    table = _table(raw, "hl-spot", where)
    return replace(
        schedule,
        stable_pair_scale=_number(table, "stable_pair_scale", where, min_value=0.0),
        quote_assets=_string_list(table, "quote_assets", where),
    )


def _parse_binance(raw: dict[str, Any], where: str) -> FeeSchedule:
    """Parse ``[binance-usdm]``, adding the BNB fee-deduction multiplier."""
    schedule = _parse_fee(raw, "binance-usdm", where)
    table = _table(raw, "binance-usdm", where)
    return replace(
        schedule,
        bnb_discount_multiplier=_number(
            table, "bnb_discount_multiplier", where, min_value=0.0
        ),
    )


def _parse_hip3(raw: dict[str, Any], where: str) -> Hip3Fees:
    """Parse ``[hip3]`` and its ``[hip3.dexes]`` deployer-fee-scale table."""
    table = _table(raw, "hip3", where)
    origin = f"{where}:hip3"
    dexes_table = _table(table, "dexes", origin)
    dexes: dict[str, float] = {}
    for dex, value in dexes_table.items():
        dexes[dex] = _float_value(value, f"{origin}.dexes.{dex}")
    if not dexes:
        raise CostError(f"{origin}.dexes must not be empty")
    return Hip3Fees(
        base_taker_bps=_number(table, "base_taker_bps", origin, min_value=0.0),
        base_maker_bps=_number(table, "base_maker_bps", origin, min_value=0.0),
        growth_mode_scale=_number(table, "growth_mode_scale", origin, min_value=0.0),
        growth_mode_share=_number(table, "growth_mode_share", origin, min_value=0.0),
        dexes=dexes,
        source=_string(table, "source", origin),
        verified=_string(table, "verified", origin),
    )


def _table(raw: dict[str, Any], key: str, where: str) -> dict[str, Any]:
    """Return ``raw[key]`` as a TOML table or raise :class:`CostError`."""
    value = raw.get(key)
    if value is None:
        raise CostError(f"{where}: missing table `{key}`")
    if not isinstance(value, dict):
        raise CostError(f"{where}: `{key}` must be a table")
    return value


def _number(
    table: dict[str, Any], key: str, where: str, *, min_value: float
) -> float:
    """Return ``table[key]`` as a float no smaller than ``min_value``."""
    if key not in table:
        raise CostError(f"{where}: missing key `{key}`")
    return _float_value(table[key], f"{where}:{key}", min_value=min_value)


def _float_value(value: Any, where: str, *, min_value: float | None = None) -> float:
    """Coerce a TOML number to float, rejecting bools and out-of-range values."""
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise CostError(f"{where} must be a number, got {type(value).__name__}")
    result = float(value)
    if min_value is not None and result < min_value:
        raise CostError(f"{where} must be >= {min_value}, got {result}")
    return result


def _string(table: dict[str, Any], key: str, where: str) -> str:
    """Return ``table[key]`` as a non-empty string or raise :class:`CostError`."""
    if key not in table:
        raise CostError(f"{where}: missing key `{key}`")
    value = table[key]
    if not isinstance(value, str) or not value:
        raise CostError(f"{where}: `{key}` must be a non-empty string")
    return value


def _string_list(table: dict[str, Any], key: str, where: str) -> tuple[str, ...]:
    """Return ``table[key]`` as a non-empty tuple of non-empty strings."""
    if key not in table:
        raise CostError(f"{where}: missing key `{key}`")
    value = table[key]
    if (
        not isinstance(value, list)
        or not value
        or not all(isinstance(item, str) and item for item in value)
    ):
        raise CostError(f"{where}: `{key}` must be a non-empty list of strings")
    return tuple(value)
