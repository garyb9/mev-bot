"""Tardis free-days downloader (SPEC-0008 §13.11, task B-1).

Tardis.dev publishes one free day per month per exchange and data type: the
first of the month, at

    https://datasets.tardis.dev/v1/{exchange}/{data_type}/{YYYY}/{MM}/01/{SYMBOL}.csv.gz

with no API key (``histdata`` review, 2026-09-28). Other days need a paid API
key; this tool refuses them unless the key is supplied through ``--api-key-env``
(and the key is only ever read from that environment variable, never printed).

Research only. It writes under ``research/data/`` (git-ignored) and sends no
credentials for free days. Downloads stream to a ``.part`` file that is renamed
into place on success, so an interrupted run never leaves a half file under the
final name and re-running is cheap.
"""

from __future__ import annotations

import argparse
import datetime as _dt
import os
import sys
import time
import urllib.error
import urllib.request
from collections.abc import Callable, Iterator, Mapping, Sequence
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Protocol

__all__ = [
    "DATASETS_BASE",
    "DEFAULT_MAX_MB",
    "EXCHANGE_DATA_TYPES",
    "KNOWN_SPOT_SYMBOLS",
    "MAX_CONCURRENCY",
    "DownloadError",
    "DownloadJob",
    "DownloadResult",
    "Fetcher",
    "FreeDayError",
    "HttpResponse",
    "SizeCapError",
    "SymbolMappingError",
    "TardisError",
    "TardisHttpError",
    "UrllibFetcher",
    "assert_free_day",
    "dataset_url",
    "download_file",
    "main",
    "parse_boundary",
    "plan_days",
    "run_downloads",
    "tardis_symbol",
]

#: Base URL of the no-key Tardis dataset mirror.
DATASETS_BASE = "https://datasets.tardis.dev/v1"

#: Default per-file size cap. Deribit ``options_chain`` is ~11.7 GB/day and is
#: only fetched when this cap is raised explicitly.
DEFAULT_MAX_MB = 2000

#: Upper bound for ``--concurrency`` (politeness towards the dataset host).
MAX_CONCURRENCY = 4

#: Data types this task knows about, per exchange (histdata review §3a, §9 B-1).
EXCHANGE_DATA_TYPES: dict[str, frozenset[str]] = {
    "hyperliquid": frozenset(
        {
            "book_ticker",
            "quotes",
            "trades",
            "derivative_ticker",
            "book_snapshot_5",
            "book_snapshot_25",
            "incremental_book_L2",
        }
    ),
    "binance-futures": frozenset({"book_ticker", "trades"}),
    "bybit": frozenset({"book_ticker", "trades"}),
    "deribit": frozenset({"options_chain", "trades"}),
}

#: HIP-3 perp dexes seen in Tardis's symbol list (histdata review §3a). ``abcd``
#: is deliberately absent: Tardis does not carry it.
HIP3_DEXES: frozenset[str] = frozenset(
    {"xyz", "flx", "vntl", "hyna", "km", "cash", "para", "mkts", "io"}
)

#: Venue prefixes used by SPEC-0008 §13.1 for CEX markets.
_CEX_PREFIXES: frozenset[str] = frozenset(
    {"binance-usdm", "bybit-linear", "binance", "bybit"}
)

#: Verified spot ``@N`` symbols. Tardis names HL spot by its raw index; the full
#: map is built from ``spotMeta`` (task B-2). ``@107`` = HYPE/USDC on 2026-09-28.
KNOWN_SPOT_SYMBOLS: dict[str, str] = {"HYPE/USDC": "@107"}

#: HTTP statuses worth retrying (transient server-side failures / throttling).
_RETRY_STATUS = frozenset({429, 500, 502, 503, 504})

#: Read size when streaming a download; keeps memory bounded.
_CHUNK = 1 << 20

#: Scheme/suffix of a Tardis CSV dataset file.
_FILE_SUFFIX = ".csv.gz"


class TardisError(Exception):
    """Base class for every error this module raises."""


class FreeDayError(TardisError):
    """A requested day is not the free first-of-month day and has no API key."""


class SymbolMappingError(TardisError):
    """A market name cannot be mapped to a Tardis symbol."""


class SizeCapError(TardisError):
    """A file's size exceeds the configured per-file cap."""


class DownloadError(TardisError):
    """A download failed after exhausting retries."""


class TardisHttpError(DownloadError):
    """An HTTP request returned a non-success status (or failed on the wire)."""

    def __init__(self, url: str, status: int | None) -> None:
        detail = f"HTTP {status}" if status is not None else "connection failed"
        super().__init__(f"{detail} for {url}")
        self.url = url
        self.status = status


class HttpResponse(Protocol):
    """The minimal response surface :func:`download_file` needs from a fetcher."""

    @property
    def content_length(self) -> int | None:
        """The body length in bytes, or ``None`` when the server omits it."""

    def read(self, size: int) -> bytes:
        """Read up to ``size`` bytes of the body; empty bytes means EOF."""

    def close(self) -> None:
        """Release the underlying connection."""


class Fetcher(Protocol):
    """Injected HTTP layer, so tests never touch the network."""

    def head_length(self, url: str) -> int | None:
        """Return ``Content-Length`` for ``url``, or ``None`` if unavailable."""

    def stream(self, url: str, headers: Mapping[str, str]) -> HttpResponse:
        """Open a GET for ``url`` and return a streaming response."""


@dataclass(frozen=True)
class DownloadJob:
    """One symbol on one day, with the header set its request needs."""

    url: str
    dest: Path
    headers: Mapping[str, str]
    symbol: str
    day: _dt.date


@dataclass(frozen=True)
class DownloadResult:
    """Outcome of one job: ``status`` ("downloaded"/"skipped") or an error."""

    job: DownloadJob
    status: str | None
    error: str | None


# --------------------------------------------------------------------------
# URL and date planning
# --------------------------------------------------------------------------


def _require_exchange(exchange: str) -> frozenset[str]:
    """Return the known data types for ``exchange`` or raise a clear error."""
    types = EXCHANGE_DATA_TYPES.get(exchange)
    if types is None:
        known = ", ".join(sorted(EXCHANGE_DATA_TYPES))
        raise TardisError(f"unknown exchange `{exchange}`; known: {known}")
    return types


def _check_exchange_type(exchange: str, data_type: str) -> None:
    """Validate an exchange/data-type pair, raising a clear error if unknown."""
    types = _require_exchange(exchange)
    if data_type not in types:
        known = ", ".join(sorted(types))
        raise TardisError(
            f"unsupported data type `{data_type}` for {exchange}; known: {known}"
        )


def dataset_url(exchange: str, data_type: str, day: _dt.date, symbol: str) -> str:
    """Build the Tardis dataset URL for one symbol on one UTC day.

    The month and day are zero-padded; ``symbol`` must already be a Tardis
    symbol (see :func:`tardis_symbol`).
    """
    _check_exchange_type(exchange, data_type)
    return (
        f"{DATASETS_BASE}/{exchange}/{data_type}/"
        f"{day.year:04d}/{day.month:02d}/{day.day:02d}/{symbol}{_FILE_SUFFIX}"
    )


def parse_boundary(value: str) -> _dt.date:
    """Parse a ``YYYY-MM`` month (day 1) or a literal ``YYYY-MM-DD`` date."""
    text = value.strip()
    if len(text) == 7:
        text += "-01"
    try:
        return _dt.date.fromisoformat(text)
    except ValueError as err:
        raise TardisError(
            f"invalid month/date `{value}` (use YYYY-MM or YYYY-MM-DD)"
        ) from err


def assert_free_day(day: _dt.date, *, has_api_key: bool) -> None:
    """Refuse a non-first-of-month day unless an API key is available."""
    if day.day == 1 or has_api_key:
        return
    raise FreeDayError(
        f"{day.isoformat()} is not a free Tardis day (only the first of each "
        "month is free without a key); pass --api-key-env VAR to allow it"
    )


def _iter_months(first: _dt.date, last: _dt.date) -> Iterator[tuple[int, int]]:
    """Yield ``(year, month)`` from ``first``'s month through ``last``'s month."""
    year, month = first.year, first.month
    while (year, month) <= (last.year, last.month):
        yield year, month
        month += 1
        if month == 13:
            year, month = year + 1, 1


def plan_days(
    date_from: _dt.date, date_to: _dt.date, *, has_api_key: bool
) -> list[_dt.date]:
    """Resolve a ``--from``/``--to`` range to the days to download.

    Month boundaries (day 1) expand to the first of every month in the
    inclusive range, the free Tardis sample. Two identical boundaries carrying
    an explicit non-first day select that single day and are refused without an
    API key. Any other explicit-day range is rejected.
    """
    if date_from > date_to:
        raise TardisError(
            f"--from {date_from.isoformat()} is after --to {date_to.isoformat()}"
        )
    if date_from.day != 1 or date_to.day != 1:
        if date_from != date_to:
            raise TardisError(
                "explicit non-first-of-month ranges must use the same start and end day"
            )
        assert_free_day(date_from, has_api_key=has_api_key)
        return [date_from]

    days = [_dt.date(year, month, 1) for year, month in _iter_months(date_from, date_to)]
    for day in days:
        assert_free_day(day, has_api_key=has_api_key)
    return days


# --------------------------------------------------------------------------
# Symbol mapping
# --------------------------------------------------------------------------


def _normalize_spot_key(market: str) -> str:
    """Normalize ``BASE/QUOTE`` to an upper-case ``BASE/QUOTE`` lookup key."""
    base, sep, quote = market.partition("/")
    if not sep or not base.strip() or not quote.strip() or "/" in quote:
        raise SymbolMappingError(f"`{market}` is not a valid BASE/QUOTE spot market")
    return f"{base.strip().upper()}/{quote.strip().upper()}"


def _cex_symbol(market: str) -> str:
    """Map a CEX market, dropping an optional SPEC-0008 venue prefix."""
    head, sep, tail = market.partition(":")
    if sep and head.strip().lower() in _CEX_PREFIXES:
        market = tail.strip()
    if not market or "/" in market:
        raise SymbolMappingError(f"`{market}` is not a valid CEX Tardis symbol")
    return market


def tardis_symbol(
    market: str,
    exchange: str,
    *,
    spot_symbols: Mapping[str, str] | None = None,
) -> str:
    """Map a SPEC-0008 market name to its Tardis dataset symbol.

    HL perps keep their (case-sensitive) name (``BTC``, ``kPEPE``); HIP-3
    ``dex:COIN`` becomes ``DEX-COIN``; HL spot ``BASE/QUOTE`` is resolved
    through :data:`KNOWN_SPOT_SYMBOLS` plus ``spot_symbols`` (raw ``@N`` passes
    through). CEX names drop an optional ``binance-usdm:``/``bybit-linear:``
    prefix. An unknown exchange, dex, or spot pair raises
    :class:`SymbolMappingError`.
    """
    name = market.strip()
    if not name:
        raise SymbolMappingError("empty market name")
    _require_exchange(exchange)

    if exchange != "hyperliquid":
        return _cex_symbol(name)

    if name.startswith("@"):
        if not name[1:].isdigit():
            raise SymbolMappingError(f"`{name}` is not a valid Tardis @index symbol")
        return name

    if "/" in name:
        key = _normalize_spot_key(name)
        table = dict(KNOWN_SPOT_SYMBOLS)
        if spot_symbols:
            table.update(
                {_normalize_spot_key(k): v for k, v in spot_symbols.items()}
            )
        try:
            return table[key]
        except KeyError as err:
            raise SymbolMappingError(
                f"no Tardis @index known for spot `{name}`; build it from spotMeta (B-2)"
            ) from err

    if ":" in name:
        dex, _, coin = name.partition(":")
        dex_key = dex.strip().lower()
        if dex_key not in HIP3_DEXES:
            known = ", ".join(sorted(HIP3_DEXES))
            raise SymbolMappingError(f"unknown HIP-3 dex `{dex}`; known: {known}")
        coin_name = coin.strip().upper()
        if not coin_name:
            raise SymbolMappingError(f"`{name}` is missing a HIP-3 coin")
        return f"{dex.strip().upper()}-{coin_name}"

    return name


# --------------------------------------------------------------------------
# HTTP layer
# --------------------------------------------------------------------------


class _UrllibResponse:
    """Adapter over an ``http.client.HTTPResponse`` implementing :class:`HttpResponse`."""

    def __init__(self, response: Any) -> None:
        self._response = response

    @property
    def content_length(self) -> int | None:
        headers = getattr(self._response, "headers", None)
        value = headers.get("Content-Length") if headers else None
        if value is None:
            return None
        try:
            length = int(value)
        except (TypeError, ValueError):
            return None
        return length if length >= 0 else None

    def read(self, size: int) -> bytes:
        return self._response.read(size)

    def close(self) -> None:
        self._response.close()


class UrllibFetcher:
    """stdlib-``urllib`` :class:`Fetcher` with retry/backoff on 5xx and 429.

    ``opener``, ``sleep`` and the retry knobs are injectable so tests can drive
    the retry path without touching the network.
    """

    def __init__(
        self,
        *,
        timeout: float = 60.0,
        retries: int = 3,
        backoff_s: float = 1.0,
        sleep: Callable[[float], None] = time.sleep,
        opener: Callable[..., Any] | None = None,
    ) -> None:
        self._timeout = timeout
        self._retries = max(0, retries)
        self._backoff_s = backoff_s
        self._sleep = sleep
        self._opener = opener if opener is not None else urllib.request.urlopen

    def _request(self, url: str, *, method: str, headers: Mapping[str, str]) -> Any:
        """Issue one request, retrying transient failures with backoff."""
        attempt = 0
        while True:
            request = urllib.request.Request(url, method=method, headers=dict(headers))
            try:
                return self._opener(request, timeout=self._timeout)
            except urllib.error.HTTPError as exc:
                if exc.code in _RETRY_STATUS and attempt < self._retries:
                    self._sleep(self._backoff_s * (2**attempt))
                    attempt += 1
                    continue
                raise TardisHttpError(url, exc.code) from exc
            except urllib.error.URLError as exc:
                if attempt < self._retries:
                    self._sleep(self._backoff_s * (2**attempt))
                    attempt += 1
                    continue
                raise TardisHttpError(url, None) from exc

    def head_length(self, url: str) -> int | None:
        """HEAD ``url`` for ``Content-Length``; return ``None`` if unavailable.

        Tardis rejects HEAD on the dataset mirror, so a 4xx here means "unknown
        size", not a hard failure.
        """
        try:
            response = self._request(url, method="HEAD", headers={})
        except TardisHttpError:
            return None
        try:
            return _UrllibResponse(response).content_length
        finally:
            response.close()

    def stream(self, url: str, headers: Mapping[str, str]) -> HttpResponse:
        """GET ``url`` and return a streaming response."""
        response = self._request(url, method="GET", headers=headers)
        return _UrllibResponse(response)


# --------------------------------------------------------------------------
# Downloading
# --------------------------------------------------------------------------


def _existing_size(path: Path) -> int | None:
    """Size of ``path`` if it is a regular file, else ``None``."""
    try:
        return path.stat().st_size if path.is_file() else None
    except OSError:
        return None


def download_file(
    fetcher: Fetcher,
    url: str,
    dest: Path,
    *,
    headers: Mapping[str, str],
    max_bytes: int,
) -> str:
    """Download ``url`` to ``dest`` atomically, returning ``"downloaded"``/``"skipped"``.

    An existing file is skipped when its size matches the server's
    ``Content-Length`` (or, when HEAD is unavailable, when it is non-empty).
    The body streams to ``dest.part`` and is renamed on success; on any failure
    the partial file is removed and no credentials are logged.
    """
    dest = Path(dest)
    dest.parent.mkdir(parents=True, exist_ok=True)

    existing = _existing_size(dest)
    if existing is not None:
        remote = fetcher.head_length(url)
        if remote is None:
            if existing > 0:
                return "skipped"
        elif remote == existing:
            return "skipped"

    part = dest.with_name(dest.name + ".part")
    response = fetcher.stream(url, headers)
    try:
        if response.content_length is not None and response.content_length > max_bytes:
            raise SizeCapError(
                f"{url} is {response.content_length} bytes, over the "
                f"{max_bytes}-byte cap; raise --max-mb to allow it"
            )
        written = 0
        with open(part, "wb") as handle:
            while True:
                chunk = response.read(_CHUNK)
                if not chunk:
                    break
                written += len(chunk)
                if written > max_bytes:
                    raise SizeCapError(
                        f"{url} exceeds the {max_bytes}-byte cap while streaming; "
                        "raise --max-mb to allow it"
                    )
                handle.write(chunk)
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(part, dest)
    except BaseException:
        part.unlink(missing_ok=True)
        raise
    finally:
        response.close()
    return "downloaded"


def run_downloads(
    jobs: Sequence[DownloadJob],
    *,
    fetcher: Fetcher,
    max_bytes: int,
    concurrency: int = 1,
) -> list[DownloadResult]:
    """Run ``jobs`` sequentially or on a bounded thread pool, collecting outcomes.

    A failing job does not stop the others; each result carries either a status
    or the error string. Result order matches ``jobs``.
    """

    def run(job: DownloadJob) -> DownloadResult:
        try:
            status = download_file(
                fetcher, job.url, job.dest, headers=job.headers, max_bytes=max_bytes
            )
        except TardisError as exc:
            return DownloadResult(job, None, str(exc))
        return DownloadResult(job, status, None)

    if concurrency <= 1 or len(jobs) <= 1:
        return [run(job) for job in jobs]
    with ThreadPoolExecutor(max_workers=concurrency) as pool:
        return list(pool.map(run, jobs))


# --------------------------------------------------------------------------
# CLI
# --------------------------------------------------------------------------


def _concurrency(value: str) -> int:
    """argparse type for ``--concurrency`` (1..``MAX_CONCURRENCY``)."""
    try:
        parsed = int(value)
    except ValueError as err:
        raise argparse.ArgumentTypeError("must be an integer") from err
    if not 1 <= parsed <= MAX_CONCURRENCY:
        raise argparse.ArgumentTypeError(f"must be between 1 and {MAX_CONCURRENCY}")
    return parsed


def _positive_int(value: str) -> int:
    """argparse type for a strictly positive integer."""
    try:
        parsed = int(value)
    except ValueError as err:
        raise argparse.ArgumentTypeError("must be an integer") from err
    if parsed <= 0:
        raise argparse.ArgumentTypeError("must be positive")
    return parsed


def _build_parser() -> argparse.ArgumentParser:
    """Build the ``hlr-tardis-fetch`` argument parser."""
    parser = argparse.ArgumentParser(
        prog="hlr-tardis-fetch",
        description=(
            "Download Tardis.dev's free first-of-month CSVs. Without "
            "--api-key-env only the first day of each month is allowed."
        ),
    )
    parser.add_argument("--exchange", required=True, help="Tardis exchange id")
    parser.add_argument("--type", dest="data_type", required=True, help="Tardis data type")
    parser.add_argument("--symbols", required=True, help="comma-separated market names")
    parser.add_argument(
        "--from", dest="date_from", required=True, metavar="YYYY-MM", help="first month"
    )
    parser.add_argument(
        "--to", dest="date_to", required=True, metavar="YYYY-MM", help="last month"
    )
    parser.add_argument(
        "--out",
        default="research/data/raw/tardis",
        help="output root (default: research/data/raw/tardis)",
    )
    parser.add_argument(
        "--concurrency",
        type=_concurrency,
        default=1,
        help=f"parallel downloads, 1..{MAX_CONCURRENCY} (default: 1)",
    )
    parser.add_argument(
        "--max-mb",
        type=_positive_int,
        default=DEFAULT_MAX_MB,
        help=f"per-file size cap in MiB (default: {DEFAULT_MAX_MB})",
    )
    parser.add_argument(
        "--api-key-env",
        default=None,
        metavar="VAR",
        help="environment variable holding a paid Tardis API key; never logged",
    )
    return parser


def _parse_symbols(raw: str) -> list[str]:
    """Split ``--symbols`` into non-empty, de-duplicated, order-preserving names."""
    seen: set[str] = set()
    symbols: list[str] = []
    for part in raw.split(","):
        name = part.strip()
        if name and name not in seen:
            seen.add(name)
            symbols.append(name)
    if not symbols:
        raise TardisError("--symbols is empty")
    return symbols


def _auth_headers(api_key: str | None, day: _dt.date) -> dict[str, str]:
    """Bearer header for a paid day only; free days never carry credentials."""
    if api_key and day.day != 1:
        return {"Authorization": f"Bearer {api_key}"}
    return {}


def _run(args: argparse.Namespace) -> int:
    """Execute one CLI invocation; returns a process exit code."""
    api_key = None
    if args.api_key_env:
        api_key = os.environ.get(args.api_key_env)
        if not api_key or not api_key.strip():
            raise TardisError(f"environment variable `{args.api_key_env}` is empty or unset")

    date_from = parse_boundary(args.date_from)
    date_to = parse_boundary(args.date_to)
    days = plan_days(date_from, date_to, has_api_key=api_key is not None)

    mapped = [
        (name, tardis_symbol(name, args.exchange))
        for name in _parse_symbols(args.symbols)
    ]
    out_root = Path(args.out)
    jobs: list[DownloadJob] = []
    for day in days:
        headers = _auth_headers(api_key, day)
        for _name, symbol in mapped:
            jobs.append(
                DownloadJob(
                    url=dataset_url(args.exchange, args.data_type, day, symbol),
                    dest=out_root
                    / args.exchange
                    / args.data_type
                    / day.isoformat()
                    / f"{symbol}{_FILE_SUFFIX}",
                    headers=headers,
                    symbol=symbol,
                    day=day,
                )
            )

    results = run_downloads(
        jobs,
        fetcher=UrllibFetcher(),
        max_bytes=args.max_mb * 1024 * 1024,
        concurrency=args.concurrency,
    )

    downloaded = skipped = failed = 0
    for result in results:
        if result.error is not None:
            failed += 1
            print(f"error: {result.job.symbol} {result.job.day}: {result.error}", file=sys.stderr)
        elif result.status == "downloaded":
            downloaded += 1
            print(f"downloaded {result.job.dest}")
        else:
            skipped += 1
            print(f"skipped {result.job.dest}")

    print(
        f"{len(results)} file(s): {downloaded} downloaded, {skipped} skipped, {failed} failed"
    )
    return 1 if failed else 0


def main(argv: Sequence[str] | None = None) -> int:
    """CLI entry point for ``hlr-tardis-fetch``."""
    parser = _build_parser()
    args = parser.parse_args(argv)
    try:
        return _run(args)
    except TardisError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
