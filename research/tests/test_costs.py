"""Tests for :mod:`hlr.costs` (SPEC-0008 P-3).

The real ``research/costs.toml`` is loaded for the value checks, so a bad edit to
the committed file fails here. Invalid-file cases write a small file under
``tmp_path``; nothing touches the network.
"""

from __future__ import annotations

import math
from pathlib import Path

import pytest

from hlr.costs import (
    BINANCE_USDM,
    BYBIT_LINEAR,
    HL_HIP3,
    HL_PERP,
    HL_SPOT,
    CostError,
    default_costs_path,
    fee_bps,
    gas_cost_usd,
    hip3_fee_bps,
    load_costs,
    maker_bps,
    round_trip_bps,
    slippage_bps,
    taker_bps,
)

MC = pytest.approx


# --------------------------------------------------------------------------
# The committed costs.toml is valid and carries the §13.2 values
# --------------------------------------------------------------------------


def test_default_file_loads_and_is_within_repo() -> None:
    costs = load_costs()
    assert default_costs_path().is_file()
    assert costs.buffer_bps == MC(2.0)
    assert costs.hl.verified == "2026-09-28"


def test_hl_perp_base_tier() -> None:
    assert taker_bps(HL_PERP, "BTC") == MC(4.5)
    assert maker_bps(HL_PERP, "BTC") == MC(1.5)


def test_hl_spot_base_tier() -> None:
    assert taker_bps(HL_SPOT, "PURR/USDC") == MC(7.0)
    assert maker_bps(HL_SPOT, "PURR/USDC") == MC(4.0)


def test_stable_pair_spot_fee_is_scaled_by_0_2() -> None:
    # USDT0/USDC: both base and quote are in the V-3 quote set (SPEC-0008 §13.2).
    assert taker_bps(HL_SPOT, "USDT0/USDC") == MC(1.4)
    assert maker_bps(HL_SPOT, "USDT0/USDC") == MC(0.8)
    assert taker_bps(HL_SPOT, "USDH/USDE") == MC(1.4)


def test_spot_with_one_quote_asset_is_not_a_stable_pair() -> None:
    assert taker_bps(HL_SPOT, "HYPE/USDT0") == MC(7.0)
    assert taker_bps(HL_SPOT, "UBTC/USDH") == MC(7.0)


def test_spot_market_must_be_base_quote() -> None:
    with pytest.raises(CostError, match="BASE/QUOTE"):
        taker_bps(HL_SPOT, "PURRUSDC")


def test_binance_usdm_vip0() -> None:
    assert taker_bps(BINANCE_USDM, "BTCUSDT") == MC(5.0)
    assert maker_bps(BINANCE_USDM, "BTCUSDT") == MC(2.0)


def test_binance_bnb_multiplier() -> None:
    assert taker_bps(BINANCE_USDM, "BTCUSDT", bnb_discount=True) == MC(4.5)
    assert maker_bps(BINANCE_USDM, "BTCUSDT", bnb_discount=True) == MC(1.8)


def test_bnb_discount_rejected_for_other_venues() -> None:
    with pytest.raises(CostError, match="bnb_discount"):
        taker_bps(BYBIT_LINEAR, "BTCUSDT", bnb_discount=True)


def test_bybit_linear_vip0() -> None:
    assert taker_bps(BYBIT_LINEAR, "BTCUSDT") == MC(5.5)
    assert maker_bps(BYBIT_LINEAR, "BTCUSDT") == MC(2.0)


def test_unknown_venue_raises() -> None:
    with pytest.raises(CostError, match="unknown venue"):
        taker_bps("nasdaq", "AAPL")


# --------------------------------------------------------------------------
# HIP-3 formula (SPEC-0008 §13.2/§15 V-3)
# --------------------------------------------------------------------------


def test_hip3_scale_one_no_growth() -> None:
    # scaleIfHip3 = 2 × 1.0 = 2.0 ⇒ 9.0 / 3.0 bps.
    assert hip3_fee_bps("taker", 1.0, False) == MC(9.0)
    assert hip3_fee_bps("maker", 1.0, False) == MC(3.0)


def test_hip3_scale_one_growth() -> None:
    # growthModeScale = 0.1 ⇒ 0.9 / 0.3 bps.
    assert hip3_fee_bps("taker", 1.0, True) == MC(0.9)
    assert hip3_fee_bps("maker", 1.0, True) == MC(0.3)


def test_hip3_scale_below_one_adds_one() -> None:
    # scale 0.5 ⇒ scaleIfHip3 = 1.5 ⇒ 6.75 / 2.25 bps.
    assert hip3_fee_bps("taker", 0.5, False) == MC(6.75)
    assert hip3_fee_bps("maker", 0.5, False) == MC(2.25)


def test_hip3_hyna_observed_scale() -> None:
    # V-3 observed hyna scale 0.1111 ⇒ 5.0 / 1.67 bps (rounded).
    assert hip3_fee_bps("taker", 0.1111, False) == MC(5.0, abs=0.01)
    assert hip3_fee_bps("maker", 0.1111, False) == MC(1.67, abs=0.01)


def test_hip3_scale_at_or_above_one_doubles() -> None:
    # scale 3.0 is not below 1 ⇒ scaleIfHip3 = 6.0 ⇒ 27.0 / 9.0 bps.
    assert hip3_fee_bps("taker", 3.0, False) == MC(27.0)
    assert hip3_fee_bps("maker", 3.0, False) == MC(9.0)


def test_hip3_via_venue_looks_up_dex_and_is_conservative() -> None:
    # No growth mode passed, so the conservative (higher) fee is used.
    assert taker_bps(HL_HIP3, "xyz:TSLA") == MC(9.0)
    assert maker_bps(HL_HIP3, "hyna:BTC") == MC(1.67, abs=0.01)
    assert taker_bps(HL_HIP3, "para:XYZ100") == MC(6.75)


def test_hip3_unknown_dex_raises() -> None:
    with pytest.raises(CostError, match="unknown HIP-3 dex"):
        taker_bps(HL_HIP3, "nope:TSLA")


def test_hip3_market_needs_dex_prefix() -> None:
    with pytest.raises(CostError, match="dex:COIN"):
        taker_bps(HL_HIP3, "TSLA")


def test_hip3_invalid_side_raises() -> None:
    with pytest.raises(CostError, match="side"):
        hip3_fee_bps("both", 1.0, False)


# --------------------------------------------------------------------------
# Round trip: two taker legs + buffer (SPEC-0008 §13.3)
# --------------------------------------------------------------------------


def test_round_trip_uses_default_buffer() -> None:
    fee = round_trip_bps((HL_PERP, "BTC"), (HL_SPOT, "PURR/USDC"))
    assert fee == MC(4.5 + 7.0 + 2.0)


def test_round_trip_buffer_override() -> None:
    fee = round_trip_bps((HL_PERP, "BTC"), (HL_PERP, "ETH"), buffer_bps=0.0)
    assert fee == MC(9.0)


def test_round_trip_hip3_leg() -> None:
    fee = round_trip_bps((HL_PERP, "BTC"), (HL_HIP3, "xyz:TSLA"))
    assert fee == MC(4.5 + 9.0 + 2.0)


# --------------------------------------------------------------------------
# Invalid files fail loudly
# --------------------------------------------------------------------------


def test_missing_file_raises(tmp_path: Path) -> None:
    with pytest.raises(CostError, match="not found"):
        load_costs(tmp_path / "nope.toml")


def test_missing_key_raises(tmp_path: Path) -> None:
    path = tmp_path / "costs.toml"
    path.write_text("buffer_bps = 2.0\n[hl]\ntaker_bps = 4.5\n")
    with pytest.raises(CostError, match="maker_bps"):
        load_costs(path)


def test_missing_table_raises(tmp_path: Path) -> None:
    path = tmp_path / "costs.toml"
    path.write_text("buffer_bps = 2.0\n")
    with pytest.raises(CostError, match="missing table `hl`"):
        load_costs(path)


def test_ill_typed_key_raises(tmp_path: Path) -> None:
    path = tmp_path / "costs.toml"
    path.write_text('buffer_bps = "two"\n')
    with pytest.raises(CostError, match="must be a number"):
        load_costs(path)


def test_invalid_toml_raises(tmp_path: Path) -> None:
    path = tmp_path / "costs.toml"
    path.write_text("buffer_bps = [\n")
    with pytest.raises(CostError, match="invalid TOML"):
        load_costs(path)


# --------------------------------------------------------------------------
# fee_bps: the spec-named entry point delegates to taker_bps/maker_bps
# --------------------------------------------------------------------------


@pytest.mark.parametrize(
    ("venue", "market"),
    [
        (HL_PERP, "BTC"),
        (HL_SPOT, "PURR/USDC"),
        (HL_SPOT, "USDT0/USDC"),
        (HL_HIP3, "xyz:TSLA"),
        (HL_HIP3, "hyna:BTC"),
        (BINANCE_USDM, "BTCUSDT"),
        (BYBIT_LINEAR, "BTCUSDT"),
    ],
)
def test_fee_bps_matches_taker_and_maker(venue: str, market: str) -> None:
    assert fee_bps(venue, market) == MC(taker_bps(venue, market))
    assert fee_bps(venue, market, "taker") == MC(taker_bps(venue, market))
    assert fee_bps(venue, market, "maker") == MC(maker_bps(venue, market))


def test_fee_bps_rejects_unknown_liquidity() -> None:
    with pytest.raises(CostError, match="liquidity"):
        fee_bps(HL_PERP, "BTC", "both")


# --------------------------------------------------------------------------
# slippage_bps: book walk against the touch (SPEC-0008 §13.2)
# --------------------------------------------------------------------------


def test_slippage_inside_top_level_is_zero() -> None:
    # Raw l2Book shape: (bids, asks), each a list of (px, sz) levels.
    book = ([[100.0, 10.0]], [[101.0, 10.0]])
    assert slippage_bps(book, "buy", 500.0) == MC(0.0)
    assert slippage_bps(book, "sell", 500.0) == MC(0.0)


def test_slippage_multi_level_buy() -> None:
    # asks [100×1, 200×1]: 300 USD fills 1 @100 + 1 @200 ⇒ vwap 150,
    # touch 100 ⇒ (150 − 100) / 100 × 1e4 = 5000 bps.
    book = {"bids": [[99.0, 1.0]], "asks": [[100.0, 1.0], [200.0, 1.0]]}
    assert slippage_bps(book, "buy", 300.0) == MC(5000.0)


def test_slippage_partial_last_level_buy() -> None:
    # asks [100×2, 200×2]: 300 USD fills 2 @100 + 0.5 @200 ⇒ vwap 120,
    # touch 100 ⇒ (120 − 100) / 100 × 1e4 = 2000 bps.
    book = {"bids": [[99.0, 1.0]], "asks": [[100.0, 2.0], [200.0, 2.0]]}
    assert slippage_bps(book, "buy", 300.0) == MC(2000.0)


def test_slippage_sell_side_walks_bids() -> None:
    # bids [100×1, 90×1]: 190 USD fills 1 @100 + 1 @90 ⇒ vwap 95,
    # touch 100 ⇒ (100 − 95) / 100 × 1e4 = 500 bps.
    book = {"bids": [[100.0, 1.0], [90.0, 1.0]], "asks": [[101.0, 1.0]]}
    assert slippage_bps(book, "sell", 190.0) == MC(500.0)


def test_slippage_insufficient_depth_is_infinite() -> None:
    book = {"bids": [[99.0, 1.0]], "asks": [[100.0, 1.0]]}
    assert math.isinf(slippage_bps(book, "buy", 200.0))
    assert math.isinf(slippage_bps(book, "sell", 200.0))


def test_slippage_empty_book_is_infinite() -> None:
    assert math.isinf(slippage_bps({"bids": [], "asks": []}, "buy", 1.0))


def test_slippage_zero_notional_is_zero() -> None:
    assert slippage_bps({"bids": [[99.0, 1.0]], "asks": [[100.0, 1.0]]}, "buy", 0.0) == MC(0.0)


def test_slippage_rejects_bad_inputs() -> None:
    book = {"bids": [[99.0, 1.0]], "asks": [[100.0, 1.0]]}
    with pytest.raises(CostError, match="side"):
        slippage_bps(book, "long", 1.0)
    with pytest.raises(CostError, match="usd"):
        slippage_bps(book, "buy", -1.0)
    with pytest.raises(CostError, match="bids"):
        slippage_bps({"asks": [[100.0, 1.0]]}, "buy", 1.0)
    with pytest.raises(CostError, match="px"):
        slippage_bps({"bids": [], "asks": [[0.0, 1.0]]}, "buy", 1.0)


# --------------------------------------------------------------------------
# gas_cost_usd: the §13.2 formula, no invented numbers
# --------------------------------------------------------------------------


def test_gas_cost_formula_with_explicit_numbers() -> None:
    # 21000 gas × 1e-8 HYPE/gas × 40 USDC/HYPE = 0.0084 USDC.
    assert gas_cost_usd(40.0, gas_used=21_000.0, gas_price_hype=1e-8) == MC(0.0084)


def test_gas_cost_without_numbers_is_v6_pending() -> None:
    # The committed costs.toml has no [hyperevm] table yet.
    with pytest.raises(CostError, match="V-6 pending"):
        gas_cost_usd(40.0)


def test_gas_cost_reads_hyperevm_table_when_present(tmp_path: Path) -> None:
    path = tmp_path / "costs.toml"
    path.write_text(
        default_costs_path().read_text()
        + "\n[hyperevm]\ngas_used = 21000\ngas_price_hype = 0.00000001\n"
    )
    costs = load_costs(path)
    assert costs.hyperevm is not None
    assert costs.hyperevm.gas_used == MC(21_000.0)
    assert gas_cost_usd(40.0, costs=costs) == MC(0.0084)


def test_partial_hyperevm_table_raises(tmp_path: Path) -> None:
    path = tmp_path / "costs.toml"
    path.write_text(default_costs_path().read_text() + "\n[hyperevm]\ngas_used = 21000\n")
    with pytest.raises(CostError, match="gas_price_hype"):
        load_costs(path)
