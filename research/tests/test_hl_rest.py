"""Tests for :mod:`hlr.backfill.hl_rest` (SPEC-0008 B-3).

No test touches the network: the HTTP layer is a fake :class:`Transport`, the
token-bucket clock/sleep are fakes, and everything is written under ``tmp_path``.
No keys are used anywhere.
"""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from pathlib import Path
from typing import Any

import orjson
import polars as pl
import pytest

from hlr import tables
from hlr.backfill.hl_rest import (
    BARS_SOURCE,
    CANDLE_ITEMS_PER_WEIGHT,
    FUNDING_ITEMS_PER_WEIGHT,
    INTERVAL_MS,
    SOURCE_REST,
    InfoClient,
    InfoHttpError,
    TransportError,
    UrllibTransport,
    WeightBudget,
    _fetch_candles,
    _fetch_funding,
    backfill,
    build_markets,
    candle_rows,
    funding_rows,
    load_state,
    main,
    market_rows,
    poll,
    save_state,
    spot_symbol_map,
)

DAY = 86_400_000

# --------------------------------------------------------------------------
# Fake universe (the live shapes observed 2026-09-28)
# --------------------------------------------------------------------------

META: dict[str, Any] = {
    "universe": [
        {"name": "BTC", "szDecimals": 5, "maxLeverage": 40},
        {"name": "ETH", "szDecimals": 4, "maxLeverage": 25},
    ]
}

SPOT_META: dict[str, Any] = {
    "tokens": [
        {"index": 0, "name": "USDC", "szDecimals": 8},
        {"index": 1, "name": "HYPE", "szDecimals": 2},
    ],
    "universe": [
        {"tokens": [1, 0], "name": "@107", "index": 107},
    ],
}

PERP_DEXS: list[Any] = [None, {"name": "xyz", "fullName": "XYZ"}]

HIP3_META: dict[str, Any] = {
    "universe": [
        {"name": "xyz:TSLA", "szDecimals": 3, "maxLeverage": 20},
    ]
}


def _json(value: Any) -> bytes:
    return orjson.dumps(value)


class FakeTransport:
    """A scriptable :class:`Transport`; unmatched requests get ``[]``.

    A handler is either a list (popped, so paging can be scripted) or a callable
    ``(body) -> (status, bytes)``.
    """

    def __init__(self, handlers: Mapping[str, Any] | None = None) -> None:
        self.handlers: dict[str, Any] = dict(handlers or {})
        self.calls: list[dict[str, Any]] = []

    def post(self, url: str, data: bytes) -> tuple[int, bytes]:
        body = orjson.loads(data)
        self.calls.append(body)
        handler = self.handlers.get(body.get("type"))
        if handler is None:
            return 200, _json([])
        if callable(handler):
            return handler(body)
        if isinstance(handler, list):
            if not handler:
                return 200, _json([])
            item = handler[0] if len(handler) == 1 else handler.pop(0)
            if isinstance(item, BaseException):
                raise item
            return item
        return 200, _json(handler)


def _universe_handlers() -> dict[str, Any]:
    """Handlers for meta/perpDexs/spotMeta plus empty funding/candles."""

    def meta_handler(body: Mapping[str, Any]) -> tuple[int, bytes]:
        if body.get("dex"):
            return 200, _json(HIP3_META)
        return 200, _json(META)

    return {
        "meta": meta_handler,
        "perpDexs": lambda _body: (200, _json(PERP_DEXS)),
        "spotMeta": lambda _body: (200, _json(SPOT_META)),
        "fundingHistory": lambda _body: (200, _json([])),
        "candleSnapshot": lambda _body: (200, _json([])),
    }


class SpyBudget:
    """A :class:`WeightBudget` stand-in that records every charge."""

    def __init__(self) -> None:
        self.calls: list[int] = []
        self.spent = 0

    def spend(self, weight: int) -> None:
        self.calls.append(weight)
        self.spent += weight


def _client(handlers: Mapping[str, Any] | None = None) -> InfoClient:
    return InfoClient(
        FakeTransport(handlers if handlers is not None else _universe_handlers()),
        budget=SpyBudget(),
        retries=2,
        sleep=lambda _s: None,
        rng=lambda: 1.0,
    )


def _funding_items(
    count: int, *, first_ms: int = 1_683_849_600_048, step: int = 3_600_000
) -> list[dict[str, Any]]:
    return [
        {
            "coin": "BTC",
            "fundingRate": "0.0001",
            "premium": "0.0002",
            "time": first_ms + i * step,
        }
        for i in range(count)
    ]


def _candle_items(
    opens: Sequence[int], *, n: int = 3, coin: str = "BTC", interval: str = "1h"
) -> list[dict[str, Any]]:
    return [
        {
            "t": open_ms,
            "T": open_ms + INTERVAL_MS[interval] - 1,
            "s": coin,
            "i": interval,
            "o": "100.0",
            "c": "101.0",
            "h": "102.0",
            "l": "99.0",
            "v": "10.0",
            "n": n,
        }
        for open_ms in opens
    ]


def _market(name: str) -> Any:
    from hlr.backfill.hl_rest import Market

    return Market(
        market=name,
        kind="perp",
        dex="",
        base=None,
        quote=None,
        asset_id=0,
        sz_decimals=5,
        max_leverage=40,
        request_coin=name,
    )


# --------------------------------------------------------------------------
# Schemas
# --------------------------------------------------------------------------


def test_bars_schema_does_not_duplicate_source() -> None:
    assert tables.columns("bars").count("source") == 1
    assert tables.columns("bars")[-1] == "fidelity"
    assert tables.schema("bars")["source"] == pl.String
    # funding_hist and markets get the two provenance columns appended.
    assert tables.columns("funding_hist")[-2:] == ["source", "fidelity"]
    assert tables.columns("markets")[-2:] == ["source", "fidelity"]


# --------------------------------------------------------------------------
# Weight accounting (V-1)
# --------------------------------------------------------------------------


def test_funding_weight_is_base_plus_per_20_items() -> None:
    budget = SpyBudget()
    transport = FakeTransport({"fundingHistory": [(200, _json(_funding_items(500)))]})
    client = InfoClient(transport, budget=budget)
    client.call(
        {"type": "fundingHistory", "coin": "BTC", "startTime": 0},
        item_divisor=FUNDING_ITEMS_PER_WEIGHT,
    )
    assert budget.calls == [20, 25]


def test_candle_weight_is_base_plus_per_60_items() -> None:
    budget = SpyBudget()
    transport = FakeTransport(
        {"candleSnapshot": [(200, _json(_candle_items(range(5000))))]}
    )
    client = InfoClient(transport, budget=budget)
    client.call(
        {"type": "candleSnapshot", "req": {}},
        item_divisor=CANDLE_ITEMS_PER_WEIGHT,
    )
    assert budget.calls == [20, 84]


class _FakeClock:
    """A monotonic clock whose ``sleep`` advances it, so tests never wait."""

    def __init__(self) -> None:
        self.now = 0.0

    def __call__(self) -> float:
        return self.now

    def sleep(self, seconds: float) -> None:
        self.now += seconds


def test_weight_budget_waits_when_empty() -> None:
    clock = _FakeClock()
    budget = WeightBudget(60, clock=clock, sleep=clock.sleep)
    budget.spend(60)
    assert budget.spent == 60
    # The bucket is empty; the next spend must wait ~1 s for 1 token at 1/s.
    budget.spend(1)
    assert clock.now == pytest.approx(1.0, abs=0.01)
    assert budget.spent == 61


# --------------------------------------------------------------------------
# Retry / backoff
# --------------------------------------------------------------------------


def test_transient_status_retries_then_succeeds() -> None:
    sleeps: list[float] = []
    transport = FakeTransport({"meta": [(429, b""), (200, _json(META))]})
    client = InfoClient(
        transport,
        budget=SpyBudget(),
        retries=2,
        backoff_s=2.0,
        sleep=sleeps.append,
        rng=lambda: 1.0,
    )
    assert client.call({"type": "meta"}) == META
    assert sleeps == [2.0]


def test_non_retryable_status_raises() -> None:
    transport = FakeTransport({"meta": [(422, b"nope")]})
    client = InfoClient(transport, budget=SpyBudget(), retries=3, sleep=lambda _s: None)
    with pytest.raises(InfoHttpError):
        client.call({"type": "meta"})


def test_wire_error_is_retried() -> None:
    calls = {"n": 0}

    def handler(_body: Mapping[str, Any]) -> tuple[int, bytes]:
        calls["n"] += 1
        if calls["n"] == 1:
            raise TransportError("boom")
        return 200, _json(META)

    transport = FakeTransport({"meta": handler})
    client = InfoClient(transport, budget=SpyBudget(), retries=2, sleep=lambda _s: None)
    assert client.call({"type": "meta"}) == META


# --------------------------------------------------------------------------
# Markets / symbol naming
# --------------------------------------------------------------------------


def test_spot_symbol_map_resolves_index() -> None:
    assert spot_symbol_map(SPOT_META) == {"@107": "HYPE/USDC"}


def test_build_markets_names_hip3_and_spot() -> None:
    markets = build_markets(META, PERP_DEXS, {"xyz": HIP3_META}, SPOT_META)
    by_name = {m.market: m for m in markets}
    assert by_name["BTC"].kind == "perp"
    assert by_name["BTC"].asset_id == 0
    assert by_name["BTC"].request_coin == "BTC"
    assert by_name["xyz:TSLA"].kind == "hip3"
    assert by_name["xyz:TSLA"].dex == "xyz"
    assert by_name["HYPE/USDC"].kind == "spot"
    assert by_name["HYPE/USDC"].base == "HYPE"
    assert by_name["HYPE/USDC"].quote == "USDC"
    assert by_name["HYPE/USDC"].request_coin == "@107"
    # No @N or raw dex syntax leaks into the market names (§13.1).
    assert all(not m.market.startswith("@") for m in markets)


def test_market_rows_have_provenance() -> None:
    markets = build_markets(META, PERP_DEXS, {"xyz": HIP3_META}, SPOT_META)
    rows = market_rows(markets, 123)
    assert all(row[0] == 123 for row in rows)
    assert all(row[-2] == SOURCE_REST and row[-1] == tables.FIDELITY_H2 for row in rows)
    assert len(rows) == 4  # BTC, ETH, xyz:TSLA, HYPE/USDC


# --------------------------------------------------------------------------
# Normalizers / paging
# --------------------------------------------------------------------------


def test_funding_rows_parse_strings() -> None:
    rows = funding_rows(_funding_items(1), "BTC")
    assert rows == [
        (1_683_849_600_048, "BTC", 0.0001, 0.0002, SOURCE_REST, tables.FIDELITY_H3)
    ]


def test_candle_rows_drop_zero_n() -> None:
    items = _candle_items([1000], n=0) + _candle_items([2000], n=5)
    rows = candle_rows(items, "BTC", "1h")
    assert [row[0] for row in rows] == [2000]
    assert rows[0][-2] == BARS_SOURCE


def test_funding_paging_stops_on_empty_page() -> None:
    pages = [
        (200, _json(_funding_items(500, first_ms=0))),
        (200, _json([])),
    ]
    transport = FakeTransport({"fundingHistory": pages})
    client = InfoClient(transport, budget=SpyBudget())
    rows, last = _fetch_funding(client, _market("BTC"), start_ms=0)
    assert len(rows) == 500
    assert last == 499 * 3_600_000
    assert len(transport.calls) == 2
    assert transport.calls[1]["startTime"] == last + 1


def test_candle_paging_stops_on_short_page() -> None:
    opens = [i * INTERVAL_MS["1h"] for i in range(10)]
    transport = FakeTransport(
        {"candleSnapshot": [(200, _json(_candle_items(opens, interval="1h")))]}
    )
    client = InfoClient(transport, budget=SpyBudget())
    rows, last = _fetch_candles(
        client, _market("BTC"), "1h", start_ms=0, end_ms=10**15
    )
    assert len(rows) == 10
    assert last == opens[-1]
    assert len(transport.calls) == 1


# --------------------------------------------------------------------------
# Parquet writers, state, idempotency
# --------------------------------------------------------------------------


def test_state_roundtrip(tmp_path: Path) -> None:
    state = {"funding": {"BTC": 5}, "candles": {"BTC|1h": 42}}
    save_state(tmp_path, state)
    assert load_state(tmp_path) == state
    # A missing file yields empty sections, not an error.
    assert load_state(tmp_path / "nope") == {"funding": {}, "candles": {}}


def test_backfill_writes_tables_and_is_idempotent(tmp_path: Path) -> None:
    fund = _funding_items(2, first_ms=DAY)
    candles = _candle_items([DAY, DAY + INTERVAL_MS["1h"]])
    handlers = _universe_handlers()

    def funding_handler(body: Mapping[str, Any]) -> tuple[int, bytes]:
        start = body.get("startTime", 0)
        return 200, _json([item for item in fund if item["time"] >= start])

    def candle_handler(body: Mapping[str, Any]) -> tuple[int, bytes]:
        start = body["req"]["startTime"]
        return 200, _json([item for item in candles if item["t"] >= start])

    handlers["fundingHistory"] = funding_handler
    handlers["candleSnapshot"] = candle_handler

    client = _client(handlers)
    report = backfill(client, tmp_path, now_ns=2 * DAY * 1_000_000, intervals=("1h",))
    assert report.markets == 4
    assert report.funding_rows == 2 * 3  # BTC + ETH + xyz:TSLA, two rows each
    assert report.candle_rows == 2 * 4  # four markets, two candles each
    assert report.markets_rows == 4
    assert not report.errors

    funding_files = sorted((tmp_path / "funding_hist").glob("date=*/hl-rest.*.parquet"))
    bars_files = sorted((tmp_path / "bars").glob("date=*/candle.1h.*.parquet"))
    markets_file = tmp_path / "markets" / "date=1970-01-03" / "hl-rest.parquet"
    assert markets_file.is_file()
    assert len(funding_files) == 3 and len(bars_files) == 4
    assert sum(pl.read_parquet(f).height for f in funding_files) == 6
    assert sum(pl.read_parquet(f).height for f in bars_files) == 8
    assert pl.read_parquet(markets_file).height == 4

    # Second run: requests resume past the stored points and writes dedupe.
    before = sum(pl.read_parquet(f).height for f in bars_files)
    report2 = backfill(client, tmp_path, now_ns=3 * DAY * 1_000_000, intervals=("1h",))
    assert report2.candle_rows == 0
    assert sum(pl.read_parquet(f).height for f in bars_files) == before
    assert report2.funding_rows == 0

    funding_calls = [c for c in client._transport.calls if c["type"] == "fundingHistory"]
    candle_calls = [c for c in client._transport.calls if c["type"] == "candleSnapshot"]
    assert funding_calls[-1]["startTime"] == DAY + 3_600_000 + 1
    assert candle_calls[-1]["req"]["startTime"] == DAY + 2 * INTERVAL_MS["1h"]


def test_backfill_drops_n_zero_candles(tmp_path: Path) -> None:
    handlers = _universe_handlers()
    handlers["candleSnapshot"] = lambda _body: (
        200,
        _json(_candle_items([DAY], n=0) + _candle_items([DAY + 3_600_000], n=4)),
    )
    client = _client(handlers)
    report = backfill(
        client,
        tmp_path,
        now_ns=2 * DAY * 1_000_000,
        intervals=("1h",),
        do_funding=False,
        do_markets=False,
    )
    bars_files = sorted((tmp_path / "bars").glob("date=*/candle.1h.*.parquet"))
    frames = [pl.read_parquet(f) for f in bars_files]
    assert len(frames) == 4
    assert sum(frame.height for frame in frames) == 4  # one surviving candle per market
    assert report.candle_rows == 4
    assert all(frame["t_open_ms"].min() == DAY + 3_600_000 for frame in frames)


def test_poll_appends_rolling_intervals_and_records_day(tmp_path: Path) -> None:
    handlers = _universe_handlers()
    requested: list[str] = []

    def candle_handler(body: Mapping[str, Any]) -> tuple[int, bytes]:
        interval = body["req"]["interval"]
        requested.append(interval)
        return 200, _json(_candle_items([DAY], interval=interval))

    handlers["candleSnapshot"] = candle_handler
    client = _client(handlers)
    report = poll(client, tmp_path, now_ns=2 * DAY * 1_000_000)
    assert set(requested) == {"1m", "5m"}
    assert report.appended_days
    assert "1970-01-02" in report.appended_days


def test_cli_help_exits_zero() -> None:
    with pytest.raises(SystemExit) as excinfo:
        main(["backfill", "--help"])
    assert excinfo.value.code == 0
    with pytest.raises(SystemExit) as excinfo:
        main(["poll", "--help"])
    assert excinfo.value.code == 0


def test_urllib_transport_contract() -> None:
    assert hasattr(UrllibTransport(), "post")
