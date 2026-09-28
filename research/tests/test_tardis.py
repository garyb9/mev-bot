"""Tests for :mod:`hlr.backfill.tardis` (SPEC-0008 B-1).

No test touches the network: the HTTP layer is a fake :class:`Fetcher`, and the
retry test injects a fake ``urllib`` opener. Nothing is written outside
``tmp_path`` and no credentials are used.
"""

from __future__ import annotations

import datetime as dt
import urllib.error
from collections.abc import Mapping
from pathlib import Path
from typing import ClassVar

import pytest

from hlr.backfill.tardis import (
    DATASETS_BASE,
    DownloadError,
    FreeDayError,
    SizeCapError,
    SymbolMappingError,
    TardisError,
    UrllibFetcher,
    assert_free_day,
    dataset_url,
    download_file,
    parse_boundary,
    plan_days,
    run_downloads,
    tardis_symbol,
)


class FakeResponse:
    """A scriptable :class:`hlr.backfill.tardis.HttpResponse`."""

    def __init__(
        self,
        chunks: list[bytes],
        *,
        content_length: int | None = None,
        fail_after: int | None = None,
    ) -> None:
        self._chunks = list(chunks)
        self.content_length = content_length
        self.closed = False
        self.reads = 0
        self._fail_after = fail_after

    def read(self, size: int) -> bytes:
        if self._fail_after is not None and self.reads >= self._fail_after:
            raise OSError("connection reset")
        self.reads += 1
        if not self._chunks:
            return b""
        return self._chunks.pop(0)

    def close(self) -> None:
        self.closed = True


class FakeFetcher:
    """A fake :class:`Fetcher` returning pre-scripted responses."""

    def __init__(
        self,
        *,
        head: int | None = None,
        response: FakeResponse | None = None,
        headers_seen: dict[str, object] | None = None,
    ) -> None:
        self._head = head
        self._response = response if response is not None else FakeResponse([b""])
        self.head_calls = 0
        self.stream_calls = 0
        self.last_headers: Mapping[str, str] | None = None
        self._headers_seen = headers_seen

    def head_length(self, url: str) -> int | None:
        self.head_calls += 1
        return self._head

    def stream(self, url: str, headers: Mapping[str, str]) -> FakeResponse:
        self.stream_calls += 1
        self.last_headers = dict(headers)
        if self._headers_seen is not None:
            self._headers_seen.update(headers)
        return self._response


# --------------------------------------------------------------------------
# URL building
# --------------------------------------------------------------------------


@pytest.mark.parametrize(
    ("exchange", "data_type", "symbol"),
    [
        ("hyperliquid", "book_ticker", "BTC"),
        ("hyperliquid", "quotes", "ETH"),
        ("hyperliquid", "trades", "BTC"),
        ("hyperliquid", "derivative_ticker", "BTC"),
        ("hyperliquid", "book_snapshot_5", "BTC"),
        ("hyperliquid", "book_snapshot_25", "BTC"),
        ("hyperliquid", "incremental_book_L2", "BTC"),
        ("binance-futures", "book_ticker", "BTCUSDT"),
        ("binance-futures", "trades", "BTCUSDT"),
        ("bybit", "book_ticker", "BTCUSDT"),
        ("bybit", "trades", "BTCUSDT"),
        ("deribit", "options_chain", "BTC"),
    ],
)
def test_dataset_url(exchange: str, data_type: str, symbol: str) -> None:
    url = dataset_url(exchange, data_type, dt.date(2026, 9, 1), symbol)
    assert url == (
        f"https://datasets.tardis.dev/v1/"
        f"{exchange}/{data_type}/2026/09/01/{symbol}.csv.gz"
    )


def test_dataset_url_zero_pads_month_and_day() -> None:
    assert dataset_url("hyperliquid", "trades", dt.date(2025, 1, 1), "BTC") == (
        f"{DATASETS_BASE}/hyperliquid/trades/2025/01/01/BTC.csv.gz"
    )


def test_dataset_url_rejects_unknown_exchange_and_type() -> None:
    with pytest.raises(TardisError, match="unknown exchange"):
        dataset_url("kraken", "trades", dt.date(2026, 9, 1), "BTC")
    with pytest.raises(TardisError, match="unsupported data type"):
        dataset_url("bybit", "options_chain", dt.date(2026, 9, 1), "BTC")


# --------------------------------------------------------------------------
# Free-day guard and date planning
# --------------------------------------------------------------------------


def test_free_day_allows_first_and_key_bypasses() -> None:
    assert_free_day(dt.date(2026, 9, 1), has_api_key=False)
    assert_free_day(dt.date(2026, 9, 15), has_api_key=True)


def test_free_day_refuses_other_days_without_key() -> None:
    with pytest.raises(FreeDayError, match="not a free Tardis day"):
        assert_free_day(dt.date(2026, 9, 15), has_api_key=False)


def test_parse_boundary_accepts_month_and_date() -> None:
    assert parse_boundary("2025-07") == dt.date(2025, 7, 1)
    assert parse_boundary("2025-07-01") == dt.date(2025, 7, 1)
    assert parse_boundary(" 2025-07-15 ") == dt.date(2025, 7, 15)


def test_parse_boundary_rejects_garbage() -> None:
    with pytest.raises(TardisError, match="invalid month/date"):
        parse_boundary("July 2025")


def test_plan_days_yields_first_of_each_month() -> None:
    days = plan_days(dt.date(2025, 7, 1), dt.date(2025, 9, 1), has_api_key=False)
    assert days == [dt.date(2025, 7, 1), dt.date(2025, 8, 1), dt.date(2025, 9, 1)]


def test_plan_days_crosses_year_boundary() -> None:
    days = plan_days(dt.date(2025, 12, 1), dt.date(2026, 2, 1), has_api_key=False)
    assert days == [dt.date(2025, 12, 1), dt.date(2026, 1, 1), dt.date(2026, 2, 1)]


def test_plan_days_single_day_range_is_one_day() -> None:
    assert plan_days(dt.date(2026, 9, 1), dt.date(2026, 9, 1), has_api_key=False) == [
        dt.date(2026, 9, 1)
    ]


def test_plan_days_reversed_range_raises() -> None:
    with pytest.raises(TardisError, match="is after"):
        plan_days(dt.date(2026, 9, 1), dt.date(2026, 8, 1), has_api_key=False)


def test_plan_days_explicit_non_first_refused_without_key() -> None:
    with pytest.raises(FreeDayError):
        plan_days(dt.date(2026, 9, 15), dt.date(2026, 9, 15), has_api_key=False)


def test_plan_days_explicit_non_first_allowed_with_key() -> None:
    assert plan_days(dt.date(2026, 9, 15), dt.date(2026, 9, 15), has_api_key=True) == [
        dt.date(2026, 9, 15)
    ]


def test_plan_days_explicit_multi_day_range_raises() -> None:
    with pytest.raises(TardisError, match="same start and end day"):
        plan_days(dt.date(2026, 9, 15), dt.date(2026, 9, 16), has_api_key=True)


# --------------------------------------------------------------------------
# Symbol mapping
# --------------------------------------------------------------------------


@pytest.mark.parametrize("market", ["BTC", "ETH", "SOL", "kPEPE"])
def test_symbol_perp_identity_preserves_case(market: str) -> None:
    assert tardis_symbol(market, "hyperliquid") == market


@pytest.mark.parametrize(
    ("market", "expected"),
    [
        ("xyz:TSLA", "XYZ-TSLA"),
        ("xyz:XYZ100", "XYZ-XYZ100"),
        ("flx:AAPL", "FLX-AAPL"),
        ("io:SPX", "IO-SPX"),
    ],
)
def test_symbol_hip3_upper_dash(market: str, expected: str) -> None:
    assert tardis_symbol(market, "hyperliquid") == expected


def test_symbol_hip3_unknown_dex_raises() -> None:
    with pytest.raises(SymbolMappingError, match="unknown HIP-3 dex"):
        tardis_symbol("abcd:TSLA", "hyperliquid")


def test_symbol_hip3_missing_coin_raises() -> None:
    with pytest.raises(SymbolMappingError, match="missing a HIP-3 coin"):
        tardis_symbol("xyz:", "hyperliquid")


def test_symbol_spot_known_table() -> None:
    assert tardis_symbol("HYPE/USDC", "hyperliquid") == "@107"


def test_symbol_spot_override_and_normalization() -> None:
    got = tardis_symbol(
        "purr/usdc", "hyperliquid", spot_symbols={"PURR/USDC": "@4"}
    )
    assert got == "@4"


def test_symbol_spot_unknown_raises() -> None:
    with pytest.raises(SymbolMappingError, match="no Tardis @index"):
        tardis_symbol("PURR/USDC", "hyperliquid")


def test_symbol_bad_spot_shape_raises() -> None:
    with pytest.raises(SymbolMappingError, match="BASE/QUOTE"):
        tardis_symbol("HYPE//USDC", "hyperliquid")


def test_symbol_at_index_passthrough() -> None:
    assert tardis_symbol("@107", "hyperliquid") == "@107"
    with pytest.raises(SymbolMappingError, match="@index"):
        tardis_symbol("@abc", "hyperliquid")


def test_symbol_cex_prefix_stripped() -> None:
    assert tardis_symbol("binance-usdm:BTCUSDT", "binance-futures") == "BTCUSDT"
    assert tardis_symbol("bybit-linear:BTCUSDT", "bybit") == "BTCUSDT"
    assert tardis_symbol("BTCUSDT", "binance-futures") == "BTCUSDT"


def test_symbol_cex_bad_shape_raises() -> None:
    with pytest.raises(SymbolMappingError, match="CEX"):
        tardis_symbol("BTC/USDT", "bybit")


def test_symbol_empty_and_unknown_exchange_raise() -> None:
    with pytest.raises(SymbolMappingError, match="empty market name"):
        tardis_symbol("  ", "hyperliquid")
    with pytest.raises(TardisError, match="unknown exchange"):
        tardis_symbol("BTC", "kraken")


# --------------------------------------------------------------------------
# Downloading: idempotency, atomicity, size cap
# --------------------------------------------------------------------------


def test_download_skips_when_size_matches_head(tmp_path: Path) -> None:
    dest = tmp_path / "BTC.csv.gz"
    dest.write_bytes(b"abc")
    fetcher = FakeFetcher(head=3)

    status = download_file(
        fetcher, "http://x/BTC.csv.gz", dest, headers={}, max_bytes=100
    )

    assert status == "skipped"
    assert fetcher.stream_calls == 0
    assert dest.read_bytes() == b"abc"


def test_download_skips_existing_when_head_unavailable(tmp_path: Path) -> None:
    dest = tmp_path / "BTC.csv.gz"
    dest.write_bytes(b"abc")
    fetcher = FakeFetcher(head=None)

    status = download_file(
        fetcher, "http://x/BTC.csv.gz", dest, headers={}, max_bytes=100
    )

    assert status == "skipped"
    assert fetcher.stream_calls == 0


def test_download_redownloads_on_size_mismatch(tmp_path: Path) -> None:
    dest = tmp_path / "BTC.csv.gz"
    dest.write_bytes(b"stale")
    fetcher = FakeFetcher(head=10, response=FakeResponse([b"newdata123"], content_length=10))

    status = download_file(
        fetcher, "http://x/BTC.csv.gz", dest, headers={}, max_bytes=100
    )

    assert status == "downloaded"
    assert dest.read_bytes() == b"newdata123"


def test_download_redownloads_empty_existing_file(tmp_path: Path) -> None:
    dest = tmp_path / "BTC.csv.gz"
    dest.write_bytes(b"")
    fetcher = FakeFetcher(
        head=None, response=FakeResponse([b"data"], content_length=4)
    )

    status = download_file(
        fetcher, "http://x/BTC.csv.gz", dest, headers={}, max_bytes=100
    )

    assert status == "downloaded"
    assert dest.read_bytes() == b"data"


def test_download_is_atomic_and_leaves_no_part(tmp_path: Path) -> None:
    dest = tmp_path / "nested/BTC.csv.gz"
    fetcher = FakeFetcher(
        head=None,
        response=FakeResponse([b"hello ", b"world"], content_length=11),
    )

    status = download_file(
        fetcher, "http://x/BTC.csv.gz", dest, headers={}, max_bytes=100
    )

    assert status == "downloaded"
    assert dest.read_bytes() == b"hello world"
    assert not dest.with_name(dest.name + ".part").exists()


def test_download_cleans_part_and_closes_on_failure(tmp_path: Path) -> None:
    dest = tmp_path / "BTC.csv.gz"
    response = FakeResponse([b"partial"], content_length=100, fail_after=1)
    fetcher = FakeFetcher(head=None, response=response)

    with pytest.raises(OSError, match="reset"):
        download_file(fetcher, "http://x/BTC.csv.gz", dest, headers={}, max_bytes=1000)

    assert not dest.exists()
    assert not dest.with_name(dest.name + ".part").exists()
    assert response.closed


def test_download_refuses_file_over_cap_by_content_length(tmp_path: Path) -> None:
    dest = tmp_path / "options.csv.gz"
    response = FakeResponse([b"x"], content_length=100)
    fetcher = FakeFetcher(head=None, response=response)

    with pytest.raises(SizeCapError, match="over the"):
        download_file(fetcher, "http://x/options.csv.gz", dest, headers={}, max_bytes=10)

    assert not dest.exists()
    assert not dest.with_name(dest.name + ".part").exists()
    assert response.closed


def test_download_refuses_stream_over_cap_without_content_length(tmp_path: Path) -> None:
    dest = tmp_path / "BTC.csv.gz"
    response = FakeResponse([b"aaaa", b"bbbb"], content_length=None)
    fetcher = FakeFetcher(head=None, response=response)

    with pytest.raises(SizeCapError, match="while streaming"):
        download_file(fetcher, "http://x/BTC.csv.gz", dest, headers={}, max_bytes=5)

    assert not dest.exists()
    assert not dest.with_name(dest.name + ".part").exists()


def test_download_sends_given_headers(tmp_path: Path) -> None:
    dest = tmp_path / "BTC.csv.gz"
    seen: dict[str, object] = {}
    fetcher = FakeFetcher(
        head=None,
        response=FakeResponse([b"x"], content_length=1),
        headers_seen=seen,
    )

    download_file(
        fetcher,
        "http://x/BTC.csv.gz",
        dest,
        headers={"Authorization": "Bearer secret"},
        max_bytes=100,
    )

    assert fetcher.last_headers == {"Authorization": "Bearer secret"}
    assert seen == {"Authorization": "Bearer secret"}


def test_run_downloads_collects_errors_without_stopping(tmp_path: Path) -> None:
    from hlr.backfill.tardis import DownloadJob

    good = DownloadJob(
        url="http://x/good",
        dest=tmp_path / "good.csv.gz",
        headers={},
        symbol="GOOD",
        day=dt.date(2026, 9, 1),
    )
    # A size-capped job fails, the following good job still runs.
    bad = DownloadJob(
        url="http://x/bad",
        dest=tmp_path / "bad.csv.gz",
        headers={},
        symbol="BAD",
        day=dt.date(2026, 9, 1),
    )

    class RoutingFetcher(FakeFetcher):
        def stream(self, url: str, headers: Mapping[str, str]) -> FakeResponse:
            self.stream_calls += 1
            body = b"ok"
            length = 100 if url.endswith("bad") else len(body)
            return FakeResponse([body], content_length=length)

    results = run_downloads(
        [bad, good], fetcher=RoutingFetcher(head=None), max_bytes=10, concurrency=1
    )

    assert results[0].error is not None
    assert results[1].status == "downloaded"
    assert good.dest.read_bytes() == b"ok"


# --------------------------------------------------------------------------
# Retry / backoff
# --------------------------------------------------------------------------


class _RetryResponse:
    headers: ClassVar[dict[str, str]] = {"Content-Length": "0"}

    def read(self, size: int) -> bytes:
        return b""

    def close(self) -> None:
        pass


def test_urllib_fetcher_retries_5xx_with_backoff() -> None:
    calls: list[str] = []
    sleeps: list[float] = []

    def opener(request: object, timeout: float | None = None) -> _RetryResponse:
        calls.append(getattr(request, "full_url", ""))
        if len(calls) < 3:
            raise urllib.error.HTTPError("http://x", 503, "busy", {}, None)
        return _RetryResponse()

    fetcher = UrllibFetcher(
        retries=3, backoff_s=1.0, sleep=sleeps.append, opener=opener
    )
    response = fetcher.stream("http://x/BTC.csv.gz", {})
    try:
        assert response.content_length == 0
    finally:
        response.close()

    assert len(calls) == 3
    assert sleeps == [1.0, 2.0]


def test_urllib_fetcher_retries_429_then_gives_up() -> None:
    calls: list[str] = []

    def opener(request: object, timeout: float | None = None) -> _RetryResponse:
        calls.append(getattr(request, "full_url", ""))
        raise urllib.error.HTTPError("http://x", 429, "slow down", {}, None)

    fetcher = UrllibFetcher(retries=2, backoff_s=0.5, sleep=lambda _s: None, opener=opener)

    with pytest.raises(DownloadError, match="HTTP 429"):
        fetcher.stream("http://x/BTC.csv.gz", {})

    assert len(calls) == 3


def test_urllib_fetcher_does_not_retry_404() -> None:
    calls: list[str] = []

    def opener(request: object, timeout: float | None = None) -> _RetryResponse:
        calls.append(getattr(request, "full_url", ""))
        raise urllib.error.HTTPError("http://x", 404, "nope", {}, None)

    fetcher = UrllibFetcher(retries=3, sleep=lambda _s: None, opener=opener)

    with pytest.raises(DownloadError, match="HTTP 404"):
        fetcher.stream("http://x/BTC.csv.gz", {})

    assert len(calls) == 1


def test_urllib_fetcher_head_returns_none_on_rejected_head() -> None:
    def opener(request: object, timeout: float | None = None) -> _RetryResponse:
        raise urllib.error.HTTPError("http://x", 403, "forbidden", {}, None)

    fetcher = UrllibFetcher(retries=0, opener=opener)

    assert fetcher.head_length("http://x/BTC.csv.gz") is None
