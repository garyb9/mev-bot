"""Cost model for the SPEC-0008 studies (task P-3, SPEC-0008 §13.2).

Every fee lives in ``research/costs.toml`` as data, so a study never hardcodes
one. This module loads and validates that file (no network, no keys) and exposes
the small API the studies use:

* :func:`taker_bps` / :func:`maker_bps` — the base fee for a ``(venue, market)``
  leg, including the stable-pair scaling for HL spot;
* :func:`fee_bps` — the spec-named entry point (``fee_bps(venue, market_kind,
  liquidity="taker")``) that dispatches to the two above;
* :func:`hip3_fee_bps` — the HIP-3 perp formula, exactly as SPEC-0008 §13.2
  defines it;
* :func:`round_trip_bps` — two taker legs plus the safety buffer;
* :func:`slippage_bps` — walk a best-first book to the target size and return
  the fill VWAP's cost versus the touch, in bps;
* :func:`gas_cost_usd` — the HyperEVM gas term (``gas_used × gas_price ×
  HYPE/USDC mid``); V-5/V-6 have no numbers yet, so it raises until they do.

``market`` is named as SPEC-0008 §13.1: an HL perp is ``BTC``, a HIP-3 perp is
``xyz:TSLA``, an HL spot pair is ``BASE/QUOTE`` (for example ``USDT0/USDC``), and
a CEX market is ``BTCUSDT``. Values are floats: research is exempt from
``rust_decimal`` (SPEC-0008 §13.1).

This is research code. It is never imported by, or deployed with, the bot.
"""

from __future__ import annotations

import math
import tomllib
from collections.abc import Mapping, Sequence
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
    "HyperEvmCosts",
    "default_costs_path",
    "fee_bps",
    "gas_cost_usd",
    "hip3_fee_bps",
    "load_costs",
    "maker_bps",
    "round_trip_bps",
    "slippage_bps",
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

#: One best-first book side: a sequence of ``(px, sz)`` levels.
Levels = Sequence[Sequence[float]]
#: A two-sided book, either ``{"bids": …, "asks": …}`` or ``(bids, asks)``.
BookLevels = Mapping[str, Levels] | Sequence[Levels]


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
class HyperEvmCosts:
    """HyperEVM gas defaults (SPEC-0008 §13.2; owned by V-5/V-6, not done).

    Parsed from the optional ``[hyperevm]`` table in ``costs.toml``. The table
    is absent today, so :func:`gas_cost_usd` raises until V-6 fills it in.
    """

    gas_used: float
    gas_price_hype: float


@dataclass(frozen=True)
class Costs:
    """The validated contents of ``research/costs.toml``."""

    buffer_bps: float
    hl: FeeSchedule
    hl_spot: FeeSchedule
    binance_usdm: FeeSchedule
    bybit_linear: FeeSchedule
    hip3: Hip3Fees
    hyperevm: HyperEvmCosts | None = None


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
    hyperevm = _parse_hyperevm(raw, where)
    return Costs(
        buffer_bps=buffer_bps,
        hl=hl,
        hl_spot=hl_spot,
        binance_usdm=binance_usdm,
        bybit_linear=bybit_linear,
        hip3=hip3,
        hyperevm=hyperevm,
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


def fee_bps(venue: str, market_kind: str, liquidity: str = "taker") -> float:
    """Return the fee in bps for one leg (SPEC-0008 §14.2 P-3).

    This is the spec-named entry point over :func:`taker_bps` and
    :func:`maker_bps`; it adds no logic of its own. ``liquidity`` is ``"taker"``
    or ``"maker"`` and ``market_kind`` is the §13.1 market name (``BTC``,
    ``xyz:TSLA``, ``USDT0/USDC``, ``BTCUSDT``). The Binance BNB multiplier is
    not exposed here; call :func:`taker_bps` / :func:`maker_bps` for that.
    """
    if liquidity == "taker":
        return taker_bps(venue, market_kind)
    if liquidity == "maker":
        return maker_bps(venue, market_kind)
    raise CostError(f"liquidity must be `taker` or `maker`, got `{liquidity}`")


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


def slippage_bps(book_levels: BookLevels, side: str, usd: float) -> float:
    """Return the book-walk slippage in bps for a ``usd`` notional (P-3).

    ``book_levels`` is ``{"bids": levels, "asks": levels}`` or the raw
    ``l2Book`` pair ``(bids, asks)``, each a best-first sequence of ``(px, sz)``
    real numbers (the recorder's ``book`` table). ``side="buy"`` walks the asks,
    ``"sell"`` walks the bids, and the result is the fill VWAP versus the
    **touch** (the best price on that side)::

        buy:  (vwap − best_ask) / best_ask × 1e4
        sell: (best_bid − vwap) / best_bid × 1e4

    SPEC-0008 §13.2 fixes neither the reference (touch vs mid) nor what happens
    when the book is too thin. This uses the touch, because the §13.3
    ``net_bps`` already prices the two legs at the touch; and it returns
    ``inf`` for insufficient depth, so a thin book never looks free.
    """
    if side not in ("buy", "sell"):
        raise CostError(f"side must be `buy` or `sell`, got `{side}`")
    usd = _float_value(usd, "usd", min_value=0.0)
    if usd == 0.0:
        return 0.0
    bids, asks = _book_sides(book_levels)
    levels = asks if side == "buy" else bids
    remaining = usd
    filled_notional = 0.0
    filled_qty = 0.0
    best: float | None = None
    for level in levels:
        px, sz = _level(level, "book")
        if best is None:
            best = px
        take = min(remaining, px * sz)
        if take > 0.0:
            filled_notional += take
            filled_qty += take / px
            remaining -= take
        if remaining <= 0.0:
            break
    if best is None or remaining > 0.0 or filled_qty <= 0.0:
        return math.inf
    vwap = filled_notional / filled_qty
    if side == "buy":
        return (vwap - best) / best * 1e4
    return (best - vwap) / best * 1e4


def gas_cost_usd(
    hype_usdc_mid: float,
    *,
    gas_used: float | None = None,
    gas_price_hype: float | None = None,
    costs: Costs | None = None,
) -> float:
    """Return an EVM gas cost in USD (SPEC-0008 §13.2).

    The spec formula is ``gas_used × gas_price_hype × hype_usdc_mid`` (gas units
    × HYPE per gas × USDC per HYPE). ``hype_usdc_mid`` is the market mid at that
    time. ``gas_used`` and ``gas_price_hype`` default to the ``[hyperevm]``
    table of ``costs.toml``; V-5/V-6 have not filled it in, so with no override
    this raises :class:`CostError` naming V-6 rather than inventing a number.
    """
    costs = costs if costs is not None else _default_costs()
    used = gas_used
    price = gas_price_hype
    if used is None or price is None:
        if costs.hyperevm is None:
            raise CostError(
                "V-6 pending: no [hyperevm] gas numbers in costs.toml; pass "
                "gas_used and gas_price_hype once V-5/V-6 land"
            )
        if used is None:
            used = costs.hyperevm.gas_used
        if price is None:
            price = costs.hyperevm.gas_price_hype
    used = _float_value(used, "gas_used", min_value=0.0)
    price = _float_value(price, "gas_price_hype", min_value=0.0)
    mid = _float_value(hype_usdc_mid, "hype_usdc_mid", min_value=0.0)
    return used * price * mid


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


def _book_sides(book_levels: BookLevels) -> tuple[Levels, Levels]:
    """Return ``(bids, asks)`` from a mapping or a raw ``(bids, asks)`` pair."""
    if isinstance(book_levels, Mapping):
        try:
            return book_levels["bids"], book_levels["asks"]
        except KeyError as err:
            raise CostError("book_levels must have `bids` and `asks`") from err
    if isinstance(book_levels, (list, tuple)) and len(book_levels) == 2:
        return book_levels[0], book_levels[1]
    raise CostError("book_levels must be a `bids`/`asks` mapping or `(bids, asks)`")


def _level(level: Sequence[float], where: str) -> tuple[float, float]:
    """Return one ``(px, sz)`` book level as positive floats."""
    try:
        px, sz = level[0], level[1]
    except (TypeError, IndexError, KeyError) as err:
        raise CostError(f"{where} level must be `(px, sz)`, got {level!r}") from err
    px = _float_value(px, f"{where} px")
    sz = _float_value(sz, f"{where} sz")
    if px <= 0.0:
        raise CostError(f"{where} px must be > 0, got {px}")
    if sz < 0.0:
        raise CostError(f"{where} sz must be >= 0, got {sz}")
    return px, sz


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


def _parse_hyperevm(raw: dict[str, Any], where: str) -> HyperEvmCosts | None:
    """Parse the optional ``[hyperevm]`` gas table; ``None`` when absent.

    V-6 owns the real numbers. A partially filled table is a mistake, so both
    gas keys are required once the table exists.
    """
    if "hyperevm" not in raw:
        return None
    table = _table(raw, "hyperevm", where)
    origin = f"{where}:hyperevm"
    return HyperEvmCosts(
        gas_used=_number(table, "gas_used", origin, min_value=0.0),
        gas_price_hype=_number(table, "gas_price_hype", origin, min_value=0.0),
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
