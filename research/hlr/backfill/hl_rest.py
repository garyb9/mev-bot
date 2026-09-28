"""Hyperliquid REST funding + candle backfill and daily 1m poller (SPEC-0008 B-3).

Backfills three §13.1 tables straight from Hyperliquid's **public** ``POST
/info`` endpoint (no keys, ever):

* ``fundingHistory`` (paged by 500) → ``funding_hist`` for every main-dex perp
  and every HIP-3 dex market;
* ``candleSnapshot`` for 1d/4h/1h (max depth) plus 15m/5m/1m (the venue keeps
  only the most recent ~5000 candles per interval, so 1m is a rolling window) →
  ``bars(source="candle")``;
* a ``meta``/``spotMeta``/``perpDexs`` snapshot → ``markets``.

Pre-launch candles (``n == 0``) are dropped. Every write is a merge-dedupe into
the §13.1 ``{table}/date=YYYY-MM-DD/`` partition, so re-running appends only new
rows and never duplicates. A small JSON state file
(``{out}/hl_rest_state.json``) stores the last funding time and candle open time
per stream, so an interrupted run resumes where it stopped.

Rate limits (SPEC-0008 §8, V-1): the client owns a token bucket of
``--weight-per-min`` weight (default 300, a quarter of the 1200/IP budget) and
charges the documented weight: ``20`` base per request plus ``1 per 20``
returned ``fundingHistory`` items or ``1 per 60`` returned ``candleSnapshot``
items. A request that does not fit waits; it is never dropped. Transient
failures (HTTP 429/5xx or a wire error) retry with exponential backoff + jitter.
Metric parity with R-5: the weight spent is reported by the CLI.

The ``poll`` subcommand is the scheduled daily job: it refreshes the market
snapshot, catches funding up, and appends the rolling 1m/5m candles so that
window stops expiring. See ``deploy/research/hl-rest-poller.md`` for the cron
entry. Installing the job is out of scope for this task.

Research only: never imported by, or deployed with, the trading bot. It reads no
keys and every network response is fakeable, so the tests never touch the
network.
"""

from __future__ import annotations

import argparse
import datetime as _dt
import os
import random
import sys
import time
import urllib.error
import urllib.request
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Protocol

import orjson
import polars as pl

from hlr import tables

__all__ = [
    "BARS_SOURCE",
    "BASE_WEIGHT",
    "CANDLE_CAP",
    "CANDLE_ITEMS_PER_WEIGHT",
    "DEFAULT_WEIGHT_PER_MIN",
    "FIDELITY_BARS",
    "FIDELITY_FUNDING",
    "FIDELITY_MARKETS",
    "FUNDING_ITEMS_PER_WEIGHT",
    "FUNDING_PAGE",
    "INFO_URL",
    "INTERVALS",
    "POLL_INTERVALS",
    "SOURCE_REST",
    "BackfillReport",
    "InfoClient",
    "InfoError",
    "InfoHttpError",
    "Market",
    "TransportError",
    "UrllibTransport",
    "WeightBudget",
    "backfill",
    "build_markets",
    "candle_rows",
    "funding_rows",
    "load_state",
    "main",
    "market_rows",
    "poll",
    "save_state",
    "spot_symbol_map",
]

#: Hyperliquid public ``/info`` endpoint. Research uses public requests only.
INFO_URL = "https://api.hyperliquid.xyz/info"

#: §13.11 source tag for the REST backfill's ``funding_hist``/``markets`` rows.
SOURCE_REST = "hl-rest"

#: §13.1 ``bars.source`` value for ``candleSnapshot`` rows (doubles as provenance).
BARS_SOURCE = "candle"

#: §13.11 fidelity classes: funding and candles are bars/funding (H3); the
#: market snapshot is periodic (H2).
FIDELITY_FUNDING = tables.FIDELITY_H3
FIDELITY_BARS = tables.FIDELITY_H3
FIDELITY_MARKETS = tables.FIDELITY_H2

#: Documented base weight of every ``/info`` request (V-1).
BASE_WEIGHT = 20

#: V-1 weight surcharge: 1 per 20 ``fundingHistory`` items, 1 per 60 candle items.
FUNDING_ITEMS_PER_WEIGHT = 20
CANDLE_ITEMS_PER_WEIGHT = 60

#: Default token-bucket budget, matching the R-5 snapshotter (§8).
DEFAULT_WEIGHT_PER_MIN = 300

#: ``fundingHistory`` returns at most 500 items per call (V-1).
FUNDING_PAGE = 500

#: The venue retains ~5000 candles per interval (V-1); a page is capped there.
CANDLE_CAP = 5000

#: Intervals backfilled by the one-shot run, deepest history first.
INTERVALS: tuple[str, ...] = ("1d", "4h", "1h", "15m", "5m", "1m")

#: Intervals the daily poller appends (the ones whose window expires fastest).
POLL_INTERVALS: tuple[str, ...] = ("1m", "5m")

#: Interval lengths in milliseconds.
INTERVAL_MS: dict[str, int] = {
    "1m": 60_000,
    "5m": 300_000,
    "15m": 900_000,
    "1h": 3_600_000,
    "4h": 14_400_000,
    "1d": 86_400_000,
}

#: HTTP statuses worth retrying (transient server-side / throttling).
_RETRY_STATUS = frozenset({429, 500, 502, 503, 504})

#: One UTC day in milliseconds and in nanoseconds.
_DAY_MS = 86_400_000
_DAY_NS = _DAY_MS * 1_000_000

#: Lookback (in candles) used by ``poll`` when no state exists for a stream.
_POLL_LOOKBACK_CANDLES = CANDLE_CAP

#: Funding lookback (in days) used by ``poll`` when no state exists.
_POLL_FUNDING_LOOKBACK_DAYS = 30


class InfoError(Exception):
    """Base class for every error this module raises."""


class TransportError(InfoError):
    """The HTTP request failed on the wire (no status was received)."""


class InfoHttpError(InfoError):
    """``/info`` returned a non-retryable status."""

    def __init__(self, url: str, status: int | None) -> None:
        detail = f"HTTP {status}" if status is not None else "wire failure"
        super().__init__(f"{detail} for {url}")
        self.url = url
        self.status = status


# --------------------------------------------------------------------------
# HTTP transport
# --------------------------------------------------------------------------


class Transport(Protocol):
    """Injected HTTP layer, so tests never touch the network."""

    def post(self, url: str, data: bytes) -> tuple[int, bytes]:
        """POST ``data`` to ``url``; return ``(status, body_bytes)``."""


class UrllibTransport:
    """stdlib-``urllib`` :class:`Transport` (no credentials are ever sent)."""

    def __init__(self, *, timeout: float = 30.0, opener: Callable[..., Any] | None = None) -> None:
        self._timeout = timeout
        self._opener = opener if opener is not None else urllib.request.urlopen

    def post(self, url: str, data: bytes) -> tuple[int, bytes]:
        """POST a JSON body; an HTTP error status is returned, not raised."""
        request = urllib.request.Request(
            url,
            data=data,
            method="POST",
            headers={"Content-Type": "application/json"},
        )
        try:
            response = self._opener(request, timeout=self._timeout)
        except urllib.error.HTTPError as exc:
            return exc.code, exc.read()
        except urllib.error.URLError as exc:
            raise TransportError(f"cannot reach {url}: {exc}") from exc
        try:
            return getattr(response, "status", 200), response.read()
        finally:
            response.close()


# --------------------------------------------------------------------------
# Weight budget
# --------------------------------------------------------------------------


class WeightBudget:
    """A token bucket of ``weight_per_min`` weight, refilled continuously.

    :meth:`spend` blocks (via the injected ``sleep``) until enough weight is
    available, so a request is delayed, never dropped. ``clock``/``sleep`` are
    injectable so tests advance time without waiting.
    """

    def __init__(
        self,
        weight_per_min: int = DEFAULT_WEIGHT_PER_MIN,
        *,
        clock: Callable[[], float] = time.monotonic,
        sleep: Callable[[float], None] = time.sleep,
    ) -> None:
        if weight_per_min <= 0:
            raise InfoError("weight_per_min must be positive")
        self.capacity = float(weight_per_min)
        self._rate = weight_per_min / 60.0
        self._tokens = float(weight_per_min)
        self._clock = clock
        self._sleep = sleep
        self._last = clock()
        self.spent = 0.0

    def _refill(self) -> None:
        now = self._clock()
        elapsed = now - self._last
        if elapsed > 0:
            self._tokens = min(self.capacity, self._tokens + elapsed * self._rate)
            self._last = now

    def spend(self, weight: int) -> None:
        """Wait until ``weight`` is available, then consume it."""
        if weight <= 0:
            return
        if weight > self.capacity:
            raise InfoError(f"a single request needs {weight} weight, over the budget")
        while True:
            self._refill()
            if self._tokens >= weight:
                self._tokens -= weight
                self.spent += weight
                return
            deficit = weight - self._tokens
            self._sleep(deficit / self._rate)


# --------------------------------------------------------------------------
# Info client
# --------------------------------------------------------------------------


class InfoClient:
    """Weight-metered, retrying ``POST /info`` client over an injected transport."""

    def __init__(
        self,
        transport: Transport,
        *,
        budget: WeightBudget | None = None,
        url: str = INFO_URL,
        retries: int = 5,
        backoff_s: float = 1.0,
        max_backoff_s: float = 30.0,
        sleep: Callable[[float], None] = time.sleep,
        rng: Callable[[], float] | None = None,
    ) -> None:
        self._transport = transport
        self._budget = budget if budget is not None else WeightBudget()
        self._url = url
        self._retries = max(0, retries)
        self._backoff_s = backoff_s
        self._max_backoff_s = max_backoff_s
        self._sleep = sleep
        self._rng = rng if rng is not None else random.random

    @property
    def budget(self) -> WeightBudget:
        """The token bucket this client charges."""
        return self._budget

    def call(self, body: Mapping[str, Any], *, item_divisor: int | None = None) -> Any:
        """POST ``body``, charging the base weight plus an item surcharge.

        ``item_divisor`` (20 for ``fundingHistory``, 60 for candles) adds
        ``ceil(len(response) / item_divisor)`` weight **after** the response, so
        the surcharge matches the documented per-item accounting (V-1).
        """
        self._budget.spend(BASE_WEIGHT)
        value = self._request(body)
        if item_divisor is not None and isinstance(value, list) and value:
            surcharge = (len(value) + item_divisor - 1) // item_divisor
            self._budget.spend(surcharge)
        return value

    def _request(self, body: Mapping[str, Any]) -> Any:
        """Issue one request, retrying transient failures with backoff."""
        payload = orjson.dumps(body)
        for attempt in range(self._retries + 1):
            try:
                status, raw = self._transport.post(self._url, payload)
            except TransportError:
                status, raw = None, b""
            if status == 200:
                try:
                    return orjson.loads(raw)
                except orjson.JSONDecodeError as exc:
                    raise InfoError(f"invalid JSON from {self._url}: {exc}") from exc
            transient = status is None or status in _RETRY_STATUS
            if transient and attempt < self._retries:
                self._sleep(self._backoff(attempt))
                continue
            raise InfoHttpError(self._url, status)

    def _backoff(self, attempt: int) -> float:
        """Exponential backoff with jitter, capped at ``max_backoff_s``."""
        delay = min(self._max_backoff_s, self._backoff_s * (2**attempt))
        return delay * (0.5 + 0.5 * self._rng())


# --------------------------------------------------------------------------
# Markets
# --------------------------------------------------------------------------


@dataclass(frozen=True)
class Market:
    """One tradable market and the coin string its REST requests need."""

    market: str
    kind: str
    dex: str
    base: str | None
    quote: str | None
    asset_id: int
    sz_decimals: int | None
    max_leverage: int | None
    request_coin: str


def spot_symbol_map(spot_meta: Mapping[str, Any] | None) -> dict[str, str]:
    """Build an ``@N → BASE/QUOTE`` map from a ``spotMeta`` response.

    Mirrors :func:`hlr.normalize.spot_index_map`; kept local so this module has
    no import cycle through the recorder normalizer.
    """
    mapping: dict[str, str] = {}
    if not spot_meta:
        return mapping
    tokens = {
        token["index"]: token["name"]
        for token in spot_meta.get("tokens", [])
        if isinstance(token, Mapping) and "index" in token and "name" in token
    }
    for entry in spot_meta.get("universe", []):
        if not isinstance(entry, Mapping):
            continue
        index = entry.get("index")
        pair = entry.get("tokens")
        name = entry.get("name")
        mapped: str | None = None
        if isinstance(pair, (list, tuple)) and len(pair) >= 2:
            base = tokens.get(pair[0])
            quote = tokens.get(pair[1])
            if base is not None and quote is not None:
                mapped = f"{base}/{quote}"
        if isinstance(name, str) and name and not name.startswith("@"):
            mapped = name
        if index is not None and mapped is not None:
            mapping[f"@{index}"] = mapped
    return mapping


def _spot_markets(spot_meta: Mapping[str, Any]) -> list[Market]:
    """Build the ``kind="spot"`` markets from a ``spotMeta`` response."""
    tokens = {
        token["index"]: token
        for token in spot_meta.get("tokens", [])
        if isinstance(token, Mapping) and "index" in token and "name" in token
    }
    markets: list[Market] = []
    for entry in spot_meta.get("universe", []):
        if not isinstance(entry, Mapping):
            continue
        pair = entry.get("tokens")
        if not isinstance(pair, (list, tuple)) or len(pair) < 2:
            continue
        base_token = tokens.get(pair[0])
        quote_token = tokens.get(pair[1])
        if base_token is None or quote_token is None:
            continue
        base = str(base_token["name"])
        quote = str(quote_token["name"])
        raw_name = str(entry.get("name") or "")
        request_coin = raw_name if raw_name else f"@{entry.get('index')}"
        sz = base_token.get("szDecimals")
        markets.append(
            Market(
                market=f"{base}/{quote}",
                kind="spot",
                dex="",
                base=base,
                quote=quote,
                asset_id=int(entry.get("index", len(markets))),
                sz_decimals=int(sz) if isinstance(sz, (int, float)) else None,
                max_leverage=None,
                request_coin=request_coin,
            )
        )
    return markets


def _universe_markets(
    meta: Mapping[str, Any], *, kind: str, dex: str
) -> list[Market]:
    """Build perp/hip3 markets from a ``meta`` (or ``meta{dex}``) response."""
    markets: list[Market] = []
    for index, entry in enumerate(meta.get("universe", [])):
        if not isinstance(entry, Mapping):
            continue
        name = entry.get("name")
        if not isinstance(name, str) or not name:
            continue
        sz = entry.get("szDecimals")
        lev = entry.get("maxLeverage")
        markets.append(
            Market(
                market=name,
                kind=kind,
                dex=dex,
                base=None,
                quote=None,
                asset_id=index,
                sz_decimals=int(sz) if isinstance(sz, (int, float)) else None,
                max_leverage=int(lev) if isinstance(lev, (int, float)) else None,
                request_coin=name,
            )
        )
    return markets


def build_markets(
    meta: Mapping[str, Any],
    perp_dexs: Sequence[Any],
    hip3_metas: Mapping[str, Mapping[str, Any]],
    spot_meta: Mapping[str, Any],
) -> list[Market]:
    """Assemble every market from the REST metadata snapshots.

    ``asset_id`` is the asset's index within the universe it was read from
    (main perps, each HIP-3 dex, or the spot universe); the spec does not define
    an offset convention and this module does not invent one.
    """
    markets = _universe_markets(meta, kind="perp", dex="")
    for entry in perp_dexs:
        if not isinstance(entry, Mapping):
            continue
        dex = entry.get("name")
        if not isinstance(dex, str) or not dex:
            continue
        dex_meta = hip3_metas.get(dex)
        if dex_meta is not None:
            markets.extend(_universe_markets(dex_meta, kind="hip3", dex=dex))
    markets.extend(_spot_markets(spot_meta))
    return markets


def fetch_markets(client: InfoClient) -> list[Market]:
    """Fetch ``meta``/``perpDexs``/``spotMeta`` plus each HIP-3 ``meta``.

    One ``meta(dex)`` request per HIP-3 dex; the resulting markets carry their
    §13.1 names and the coin string each REST request needs.
    """
    meta = client.call({"type": "meta"})
    perp_dexs = client.call({"type": "perpDexs"})
    spot_meta = client.call({"type": "spotMeta"})
    if not isinstance(meta, Mapping) or not isinstance(spot_meta, Mapping):
        raise InfoError("meta/spotMeta response is not an object")
    dex_list = perp_dexs if isinstance(perp_dexs, list) else []
    hip3_metas: dict[str, Mapping[str, Any]] = {}
    for entry in dex_list:
        if not isinstance(entry, Mapping):
            continue
        dex = entry.get("name")
        if not isinstance(dex, str) or not dex:
            continue
        dex_meta = client.call({"type": "meta", "dex": dex})
        if isinstance(dex_meta, Mapping):
            hip3_metas[dex] = dex_meta
    return build_markets(meta, dex_list, hip3_metas, spot_meta)


def market_rows(markets: Sequence[Market], snapshot_t_ns: int) -> list[tuple[Any, ...]]:
    """One ``markets`` row per market at ``snapshot_t_ns`` (schema order)."""
    return [
        (
            snapshot_t_ns,
            m.market,
            m.kind,
            m.dex,
            m.base,
            m.quote,
            m.asset_id,
            m.sz_decimals,
            m.max_leverage,
            SOURCE_REST,
            FIDELITY_MARKETS,
        )
        for m in markets
    ]


# --------------------------------------------------------------------------
# Paging
# --------------------------------------------------------------------------


def funding_rows(items: Sequence[Any], market: str) -> list[tuple[Any, ...]]:
    """Normalize ``fundingHistory`` items into ``funding_hist`` rows."""
    rows: list[tuple[Any, ...]] = []
    for item in items:
        if not isinstance(item, Mapping):
            continue
        time_ms = item.get("time")
        rate = _to_float(item.get("fundingRate"))
        premium = _to_float(item.get("premium"))
        if not isinstance(time_ms, (int, float)):
            continue
        rows.append(
            (int(time_ms), market, rate, premium, SOURCE_REST, FIDELITY_FUNDING)
        )
    return rows


def candle_rows(
    items: Sequence[Any], market: str, interval: str
) -> list[tuple[Any, ...]]:
    """Normalize ``candleSnapshot`` items into ``bars(source="candle")`` rows.

    Pre-launch candles (``n == 0``) are dropped, per the B-3 specification.
    """
    rows: list[tuple[Any, ...]] = []
    for item in items:
        if not isinstance(item, Mapping):
            continue
        open_ms = item.get("t")
        n_trades = item.get("n")
        if not isinstance(open_ms, (int, float)):
            continue
        if isinstance(n_trades, (int, float)) and int(n_trades) == 0:
            continue
        rows.append(
            (
                int(open_ms),
                interval,
                "hl",
                market,
                _to_float(item.get("o")),
                _to_float(item.get("h")),
                _to_float(item.get("l")),
                _to_float(item.get("c")),
                _to_float(item.get("v")),
                int(n_trades) if isinstance(n_trades, (int, float)) else None,
                BARS_SOURCE,
                FIDELITY_BARS,
            )
        )
    return rows


def _fetch_funding(
    client: InfoClient, market: Market, *, start_ms: int
) -> tuple[list[tuple[Any, ...]], int]:
    """Page ``fundingHistory`` forward from ``start_ms`` until caught up.

    Stops on an empty page or a page shorter than the 500-item cap. Returns the
    rows and the last item time (for the resume state).
    """
    rows: list[tuple[Any, ...]] = []
    last_ms = start_ms
    cursor = start_ms
    while True:
        items = client.call(
            {"type": "fundingHistory", "coin": market.request_coin, "startTime": cursor},
            item_divisor=FUNDING_ITEMS_PER_WEIGHT,
        )
        if not items:
            break
        rows.extend(funding_rows(items, market.market))
        times = [
            int(item["time"])
            for item in items
            if isinstance(item, Mapping) and isinstance(item.get("time"), (int, float))
        ]
        if not times:
            break
        page_last = max(times)
        if page_last <= last_ms:
            break
        last_ms = page_last
        if len(items) < FUNDING_PAGE:
            break
        cursor = page_last + 1
    return rows, last_ms


def _fetch_candles(
    client: InfoClient,
    market: Market,
    interval: str,
    *,
    start_ms: int,
    end_ms: int,
) -> tuple[list[tuple[Any, ...]], int]:
    """Page ``candleSnapshot`` forward from ``start_ms`` up to ``end_ms``.

    Stops on an empty page or a page shorter than the ~5000-candle retention
    cap. Returns the rows and the last open time (for the resume state).
    """
    step = INTERVAL_MS[interval]
    rows: list[tuple[Any, ...]] = []
    last_ms = start_ms
    cursor = start_ms
    while True:
        req = {
            "coin": market.request_coin,
            "interval": interval,
            "startTime": cursor,
            "endTime": end_ms,
        }
        items = client.call(
            {"type": "candleSnapshot", "req": req},
            item_divisor=CANDLE_ITEMS_PER_WEIGHT,
        )
        if not items:
            break
        rows.extend(candle_rows(items, market.market, interval))
        opens = [
            int(item["t"])
            for item in items
            if isinstance(item, Mapping) and isinstance(item.get("t"), (int, float))
        ]
        if not opens:
            break
        page_last = max(opens)
        if page_last <= last_ms:
            break
        last_ms = page_last
        if len(items) < CANDLE_CAP:
            break
        cursor = page_last + step
        if cursor >= end_ms:
            break
    return rows, last_ms


# --------------------------------------------------------------------------
# Parquet writing (merge-dedupe, atomic)
# --------------------------------------------------------------------------


def _to_float(value: Any) -> float | None:
    """Parse a string/number to float64, or ``None``."""
    if value is None:
        return None
    try:
        return float(value)
    except (TypeError, ValueError):
        return None


def _sanitize(text: str) -> str:
    """Make ``text`` safe for a filename component (markets contain ``:``/``/``)."""
    return "".join(ch if ch.isalnum() or ch in "-_." else "_" for ch in text)


def _funding_part(market: str) -> str:
    """Per-stream part name for ``funding_hist`` (one market, one day)."""
    return f"{SOURCE_REST}.{_sanitize(market)}.parquet"


def _bars_part(interval: str, market: str) -> str:
    """Per-stream part name for ``bars`` (one market, one interval, one day)."""
    return f"{BARS_SOURCE}.{interval}.{_sanitize(market)}.parquet"


def _day_index_expr(table: str) -> pl.Expr:
    """The table's UTC day index (days since epoch) for date partitioning.

    ``funding_hist.time_ms`` and ``bars.t_open_ms`` are milliseconds;
    ``markets.snapshot_t_ns`` is nanoseconds (§13.1).
    """
    if table == "markets":
        return pl.col("snapshot_t_ns") // _DAY_NS
    column = {"funding_hist": "time_ms", "bars": "t_open_ms"}[table]
    return pl.col(column) // _DAY_MS


def _date_of_days(days: int) -> _dt.date:
    """The UTC date ``days`` after 1970-01-01."""
    return _dt.date(1970, 1, 1) + _dt.timedelta(days=days)


def _date_of_ms(ms: int) -> _dt.date:
    """The UTC date of a millisecond Unix timestamp."""
    return _date_of_days(ms // _DAY_MS)


def _write_atomic(frame: pl.DataFrame, final: Path) -> None:
    """Write ``frame`` to ``final`` via a ``.tmp`` file and an atomic rename."""
    final.parent.mkdir(parents=True, exist_ok=True)
    tmp = final.with_name(final.name + ".tmp")
    try:
        frame.write_parquet(tmp, compression="zstd", row_group_size=250_000)
        os.replace(tmp, final)
    except BaseException:
        tmp.unlink(missing_ok=True)
        raise


def _upsert(
    out_root: Path,
    table: str,
    day_iso: str,
    part: str,
    new: pl.DataFrame,
    unique_keys: Sequence[str],
) -> int:
    """Merge ``new`` into one partition part, dedupe on ``unique_keys``, atomically."""
    final = out_root / table / f"date={day_iso}" / part
    if final.exists():
        old = pl.read_parquet(final)
        combined = pl.concat([old, new.select(old.columns)], how="vertical", rechunk=False)
        combined = combined.unique(subset=list(unique_keys), keep="last")
    else:
        combined = new
    combined = combined.sort(list(unique_keys))
    _write_atomic(combined, final)
    return combined.height


def _write_rows(
    out_root: Path,
    table: str,
    rows: Sequence[tuple[Any, ...]],
    *,
    part: str,
    unique_keys: Sequence[str],
) -> int:
    """Write rows into their day partitions, merging/deduping each part.

    Returns the number of new rows offered (not the merged file size). Rows are
    grouped by the table's timestamp (milliseconds, or nanoseconds for
    ``markets``) so a stream that spans many days (full funding history) lands in
    the correct ``date=`` directories.
    """
    if not rows:
        return 0
    frame = pl.DataFrame(
        rows, schema=list(tables.schema(table).items()), orient="row"
    )
    frame = frame.with_columns(_day_index_expr(table).alias("_day"))
    for (day,), group in frame.sort("_day").group_by("_day", maintain_order=True):
        sub = group.drop("_day")
        _upsert(out_root, table, _date_of_days(int(day)).isoformat(), part, sub, unique_keys)
    return frame.height


# --------------------------------------------------------------------------
# Resume state
# --------------------------------------------------------------------------


def _state_path(out_root: Path) -> Path:
    """Path of the JSON resume-state file."""
    return out_root / "hl_rest_state.json"


def load_state(out_root: str | os.PathLike[str]) -> dict[str, dict[str, int]]:
    """Load the resume state; return an empty structure when absent/corrupt."""
    path = _state_path(Path(out_root))
    empty: dict[str, dict[str, int]] = {"funding": {}, "candles": {}}
    if not path.is_file():
        return empty
    try:
        data = orjson.loads(path.read_bytes())
    except (OSError, orjson.JSONDecodeError):
        return empty
    if not isinstance(data, Mapping):
        return empty
    result: dict[str, dict[str, int]] = {}
    for key in ("funding", "candles"):
        section = data.get(key)
        cleaned: dict[str, int] = {}
        if isinstance(section, Mapping):
            for name, value in section.items():
                if isinstance(value, int):
                    cleaned[str(name)] = value
        result[key] = cleaned
    return result


def save_state(out_root: str | os.PathLike[str], state: Mapping[str, Any]) -> None:
    """Atomically persist the resume state JSON."""
    path = _state_path(Path(out_root))
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(path.name + ".tmp")
    try:
        tmp.write_bytes(orjson.dumps(state, option=orjson.OPT_SORT_KEYS))
        os.replace(tmp, path)
    except BaseException:
        tmp.unlink(missing_ok=True)
        raise


# --------------------------------------------------------------------------
# Drivers
# --------------------------------------------------------------------------


@dataclass
class BackfillReport:
    """Outcome of one :func:`backfill`/:func:`poll` run."""

    markets: int = 0
    funding_markets: int = 0
    funding_rows: int = 0
    candle_streams: int = 0
    candle_rows: int = 0
    markets_rows: int = 0
    appended_days: list[str] = field(default_factory=list)
    errors: list[str] = field(default_factory=list)


def _candle_start(
    state: Mapping[str, dict[str, int]],
    market: str,
    interval: str,
    *,
    now_ms: int,
    poll: bool,
) -> int:
    """Resume point for one candle stream.

    A full backfill starts at 0 (the venue clamps to its retention window); a
    poll resumes from the stored open and, with no state, starts at the retained
    window rather than re-fetching all history.
    """
    key = f"{market}|{interval}"
    stored = state["candles"].get(key)
    if stored is not None:
        return stored + INTERVAL_MS[interval]
    if poll:
        return max(0, now_ms - _POLL_LOOKBACK_CANDLES * INTERVAL_MS[interval])
    return 0


def _run(
    client: InfoClient,
    out_root: Path,
    *,
    now_ns: int,
    intervals: Sequence[str],
    do_funding: bool,
    do_candles: bool,
    do_markets: bool,
    full: bool,
    poll: bool,
) -> BackfillReport:
    """Shared one-shot/poll driver: fetch, write, and advance the resume state."""
    report = BackfillReport()
    now_ms = now_ns // 1_000_000
    # A ``full`` run still merges (never duplicates); it just ignores the resume
    # points and re-fetches from the start.
    state = {"funding": {}, "candles": {}} if full else load_state(out_root)

    try:
        markets = fetch_markets(client)
    except InfoError as exc:
        report.errors.append(str(exc))
        raise
    report.markets = len(markets)

    if do_markets:
        rows = market_rows(markets, now_ns)
        report.markets_rows = _write_rows(
            out_root,
            "markets",
            rows,
            part=f"{SOURCE_REST}.parquet",
            unique_keys=("snapshot_t_ns", "market"),
        )

    if do_funding:
        for market in markets:
            if market.kind == "spot":
                continue
            try:
                stored = state["funding"].get(market.market)
                if full or stored is None:
                    start = (
                        max(0, now_ms - _POLL_FUNDING_LOOKBACK_DAYS * _DAY_MS) if poll else 0
                    )
                else:
                    # ``startTime`` is inclusive; resume one ms past the stored time.
                    start = stored + 1
                rows, last_ms = _fetch_funding(client, market, start_ms=start)
            except InfoError as exc:
                report.errors.append(f"funding {market.market}: {exc}")
                continue
            report.funding_rows += _write_rows(
                out_root,
                "funding_hist",
                rows,
                part=_funding_part(market.market),
                unique_keys=("market", "time_ms"),
            )
            report.funding_markets += 1
            if last_ms > state["funding"].get(market.market, -1):
                state["funding"][market.market] = last_ms
                save_state(out_root, state)

    if do_candles:
        for market in markets:
            advanced = False
            for interval in intervals:
                if interval not in INTERVAL_MS:
                    report.errors.append(f"unknown interval `{interval}`")
                    continue
                try:
                    start = _candle_start(
                        state, market.market, interval, now_ms=now_ms, poll=poll
                    )
                    rows, last_ms = _fetch_candles(
                        client, market, interval, start_ms=start, end_ms=now_ms
                    )
                except InfoError as exc:
                    report.errors.append(f"candles {market.market} {interval}: {exc}")
                    continue
                report.candle_rows += _write_rows(
                    out_root,
                    "bars",
                    rows,
                    part=_bars_part(interval, market.market),
                    unique_keys=("market", "interval", "t_open_ms"),
                )
                report.candle_streams += 1
                key = f"{market.market}|{interval}"
                if last_ms > state["candles"].get(key, -1):
                    state["candles"][key] = last_ms
                    advanced = True
                    report.appended_days.append(_date_of_ms(last_ms).isoformat())
            if advanced:
                save_state(out_root, state)

    save_state(out_root, state)
    return report


def backfill(
    client: InfoClient,
    out_root: str | os.PathLike[str],
    *,
    now_ns: int | None = None,
    intervals: Sequence[str] = INTERVALS,
    do_funding: bool = True,
    do_candles: bool = True,
    do_markets: bool = True,
    full: bool = False,
) -> BackfillReport:
    """One-shot backfill of funding, candles, and the market snapshot."""
    return _run(
        client,
        Path(out_root),
        now_ns=now_ns if now_ns is not None else time.time_ns(),
        intervals=tuple(intervals),
        do_funding=do_funding,
        do_candles=do_candles,
        do_markets=do_markets,
        full=full,
        poll=False,
    )


def poll(
    client: InfoClient,
    out_root: str | os.PathLike[str],
    *,
    now_ns: int | None = None,
    intervals: Sequence[str] = POLL_INTERVALS,
    do_funding: bool = True,
    do_markets: bool = True,
) -> BackfillReport:
    """Daily poller: refresh markets, catch funding up, append rolling candles."""
    return _run(
        client,
        Path(out_root),
        now_ns=now_ns if now_ns is not None else time.time_ns(),
        intervals=tuple(intervals),
        do_funding=do_funding,
        do_candles=True,
        do_markets=do_markets,
        full=False,
        poll=True,
    )


# --------------------------------------------------------------------------
# CLI
# --------------------------------------------------------------------------


def _positive_int(value: str) -> int:
    """argparse type for a strictly positive integer."""
    try:
        parsed = int(value)
    except ValueError as err:
        raise argparse.ArgumentTypeError("must be an integer") from err
    if parsed <= 0:
        raise argparse.ArgumentTypeError("must be positive")
    return parsed


def _intervals(value: str) -> tuple[str, ...]:
    """argparse type: comma-separated intervals, validated against the known set."""
    names = tuple(part.strip() for part in value.split(",") if part.strip())
    if not names:
        raise argparse.ArgumentTypeError("must name at least one interval")
    unknown = [name for name in names if name not in INTERVAL_MS]
    if unknown:
        raise argparse.ArgumentTypeError(f"unknown interval(s): {', '.join(unknown)}")
    return names


def _build_parser() -> argparse.ArgumentParser:
    """Build the ``hlr-hl-rest`` argument parser."""
    parser = argparse.ArgumentParser(
        prog="hlr-hl-rest",
        description=(
            "Backfill Hyperliquid funding and candles from public POST /info "
            "(SPEC-0008 B-3, HIST-PRELIM). No keys are ever used."
        ),
    )
    sub = parser.add_subparsers(dest="command", required=True)
    for name, help_text in (
        ("backfill", "one-shot funding + candle + market backfill"),
        ("poll", "daily job: append rolling 1m/5m candles and catch funding up"),
    ):
        cmd = sub.add_parser(name, help=help_text)
        cmd.add_argument(
            "--out",
            default="research/data/parquet",
            help="parquet output root (default: research/data/parquet)",
        )
        cmd.add_argument(
            "--weight-per-min",
            type=_positive_int,
            default=DEFAULT_WEIGHT_PER_MIN,
            help=f"/info weight budget per minute (default: {DEFAULT_WEIGHT_PER_MIN})",
        )
    backfill_cmd = sub.choices["backfill"]
    backfill_cmd.add_argument(
        "--full",
        action="store_true",
        help="ignore resume state and re-fetch from the start (merge still dedupes)",
    )
    backfill_cmd.add_argument(
        "--intervals",
        type=_intervals,
        default=INTERVALS,
        help=f"comma-separated candle intervals (default: {','.join(INTERVALS)})",
    )
    backfill_cmd.add_argument("--no-funding", action="store_true", help="skip funding_hist")
    backfill_cmd.add_argument("--no-candles", action="store_true", help="skip bars")
    backfill_cmd.add_argument("--no-markets", action="store_true", help="skip markets")
    poll_cmd = sub.choices["poll"]
    poll_cmd.add_argument(
        "--intervals",
        type=_intervals,
        default=POLL_INTERVALS,
        help=f"comma-separated candle intervals (default: {','.join(POLL_INTERVALS)})",
    )
    poll_cmd.add_argument("--no-funding", action="store_true", help="skip funding_hist")
    poll_cmd.add_argument("--no-markets", action="store_true", help="skip markets")
    return parser


def _print_report(command: str, report: BackfillReport, client: InfoClient) -> None:
    """Print the run summary to stdout."""
    print(
        f"{command}: {report.markets} markets; "
        f"funding {report.funding_rows} new rows across {report.funding_markets} streams; "
        f"bars {report.candle_rows} new rows across {report.candle_streams} streams; "
        f"markets {report.markets_rows} rows"
    )
    if report.appended_days:
        first, last = min(report.appended_days), max(report.appended_days)
        print(f"candle days touched: {first} .. {last}")
    print(f"weight spent: {client.budget.spent:.0f}")
    for error in report.errors:
        print(f"error: {error}", file=sys.stderr)


def _run_cli(args: argparse.Namespace) -> int:
    """Execute one CLI invocation; returns a process exit code."""
    client = InfoClient(
        UrllibTransport(),
        budget=WeightBudget(args.weight_per_min),
    )
    if args.command == "backfill":
        report = backfill(
            client,
            args.out,
            intervals=args.intervals,
            do_funding=not args.no_funding,
            do_candles=not args.no_candles,
            do_markets=not args.no_markets,
            full=args.full,
        )
    else:
        report = poll(
            client,
            args.out,
            intervals=args.intervals,
            do_funding=not args.no_funding,
            do_markets=not args.no_markets,
        )
    _print_report(args.command, report, client)
    return 1 if report.errors else 0


def main(argv: Sequence[str] | None = None) -> int:
    """CLI entry point for ``hlr-hl-rest``."""
    parser = _build_parser()
    args = parser.parse_args(argv)
    try:
        return _run_cli(args)
    except InfoError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
