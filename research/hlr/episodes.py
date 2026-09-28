"""Episode detector and latency capture (task P-4, SPEC-0008 §13.3–§13.5).

A *study* maps its markets into one aligned, as-of joined polars frame on ``t_ns``
(``join_asof`` backward, so a row only ever holds data that was already
available) and supplies three things:

* a ``net_bps`` column/expression — the §13.3 edge in basis points. **The study
  must apply every cost itself**: both legs' taker fees via
  ``hlr.costs.fee_bps`` (``fee_bps(venue, market_kind)``), the safety buffer and
  ``hlr.costs.slippage_bps`` walked at the capped size. The detector applies no
  fees, funding or slippage;
* a ``size_usd`` column/expression — the §13.3 capturable notional before the
  ``max_notional`` cap (pass ``max_notional`` to :func:`detect_episodes`, or cap
  it in the study and pass ``max_notional=None``);
* a ``valid`` mask — whether every input feed is fresh and gap-free (§13.3),
  built with :func:`feed_validity` from a ``stale_ms`` and the ``gaps`` table.

:func:`detect_episodes` then finds the maximal ``[t_start, t_end)`` intervals
where ``net_bps > 0`` and the feeds are valid, merges episodes separated by less
than ``merge_ms``, and captures the edge at each latency ``L`` in the §13.4 grid
by re-reading the as-of state at ``t_start + L`` (never a later row, so there is
no lookahead). :func:`episode_metrics` rolls the episodes up into the per-latency
part of the §13.5 table, one row per capture variant, so P-5 can grade on the
§13.10 headline (competition-adjusted, jittered).

Conservative readings (the spec is silent or terse in these places; see the P-4
report):

* **Capture size is the order we could have placed at ``t_start``.** The fill is
  the best of the two states' books, so ``size = min(size_usd(t_start),
  size_usd(t_start + L))``; a book that grows *after* the decision does not
  enlarge the order.
* **The cap is applied after competition.** Naive size is ``min(max_notional,
  displayed)``; competition-adjusted size is ``min(max_notional, max(0,
  displayed − competed))``.
* **Capture is USD.** §13.3 writes ``captured_L = net_bps(...) × size_usd(...)``,
  but §13.5 makes ``Σ captured_L / days`` a USD/day figure, so the bps value is
  applied as a fraction: ``captured_L = net_bps / 1e4 × size_usd``.
* **``t_end`` is the first event time at which the condition no longer holds**,
  capped at ``stale_ms`` after the last observed state (so a sparse book cannot
  inflate ``duration_ms``); the interval is half open, so an episode is *not*
  open at exactly ``t_end``.
* **Merging never crosses invalid data.** A short ``<= merge_ms`` separation is
  merged only when the separating rows are valid; a gap or a stale feed is not
  papered over.
* **Gaps are half open** ``[start_ns, end_ns)`` and are unioned before the
  membership test, so nested/overlapping gaps cannot leak a "valid" row.
* **An open probe needs ``net_bps > 0`` as well as a fresh state**, so a merged
  sub-``merge_ms`` dip neither counts as capture nor inflates ``capture_rate``.
* **A capture needs a fresh probe.** Besides the episode still being open, the
  last state observed at or before ``t_start + L`` must be no older than
  ``stale_ms``; a probe onto a held-but-stale quote is counted as missed rather
  than assumed still actionable.
* **``date`` is the UTC day of ``t_start``**, so an episode that crosses midnight
  stays one episode attributed to the day it began (the §13.5 per-day counts).
* **A null ``net_bps`` or ``size_usd`` is not an episode.**

Rigor hooks for §13.10 live here so P-5/studies do not re-implement them:
:func:`day_block_bootstrap_ci` (uncertainty), :func:`jittered_latency_ms` (latency
jitter), the ``compete_usd`` argument of :func:`detect_episodes` (fill
competition), the optional ``mid``/``direction`` markout columns (adverse
selection), :func:`oos_split` (the 60/40 chronological split) and
:func:`daily_capture` (per-day aggregation). The verdict itself (APR, CI gating,
capital grid) is P-5.

This is research code. It is never imported by, or deployed with, the bot; it
reads no keys and no network.
"""

from __future__ import annotations

import datetime as _dt
import math
import random
from collections.abc import Sequence
from dataclasses import dataclass

import polars as pl

__all__ = [
    "BPS_SCALE",
    "DAY_NS",
    "DEFAULT_LATENCIES_MS",
    "DEFAULT_MERGE_MS",
    "DEFAULT_STALE_MS",
    "HEADLINE_VARIANT",
    "MS_NS",
    "EpisodeConfig",
    "EpisodeError",
    "Feed",
    "daily_capture",
    "day_block_bootstrap_ci",
    "detect_episodes",
    "episode_metrics",
    "feed_validity",
    "jittered_latency_ms",
    "oos_split",
]

#: Nanoseconds per millisecond.
MS_NS = 1_000_000
#: Nanoseconds per UTC day.
DAY_NS = 86_400 * 1_000 * MS_NS
#: bps -> fraction.
BPS_SCALE = 1e-4

#: The §13.4 latency grid, in milliseconds.
DEFAULT_LATENCIES_MS: tuple[int, ...] = (10, 50, 100, 250, 500, 1000)
#: The §13.3 feed-staleness default.
DEFAULT_STALE_MS = 2000
#: The §13.3 episode-merge default.
DEFAULT_MERGE_MS = 50

#: The capture variant the §13.10 verdict uses (competition-adjusted, jittered).
HEADLINE_VARIANT = "adj_jitter"

#: ``(name, captured-format, open-format)`` for every capture variant, in report
#: order. The formats take one ``lat`` (the latency in ms) keyword.
_VARIANTS: tuple[tuple[str, str, str], ...] = (
    ("naive", "captured_{lat}", "open_{lat}"),
    ("adj", "captured_adj_{lat}", "open_{lat}"),
    ("jitter", "captured_jitter_{lat}", "open_jitter_{lat}"),
    ("adj_jitter", "captured_adj_jitter_{lat}", "open_jitter_{lat}"),
)

#: z-score of the 99th percentile of a standard normal, for the §13.10 lognormal
#: latency jitter (p99 = 3 × median).
_Z99 = 2.3263478740408408


class EpisodeError(ValueError):
    """The inputs are malformed (missing column, bad parameter, bad dtype)."""


@dataclass(frozen=True)
class Feed:
    """One input feed whose freshness and gaps gate an episode (SPEC-0008 §13.3).

    ``last_ns`` names the column holding the feed's most recent update time as of
    each row (from the study's ``join_asof``). ``src``/``conn`` select the
    ``gaps`` rows that invalidate this feed; both may be ``None`` to ignore gaps.
    """

    name: str
    last_ns: str
    src: str | None = None
    conn: str | None = None


@dataclass(frozen=True)
class EpisodeConfig:
    """Study-independent §13.3/§13.4 parameters."""

    stale_ms: int = DEFAULT_STALE_MS
    merge_ms: int = DEFAULT_MERGE_MS
    max_notional: float | None = None
    latencies_ms: tuple[int, ...] = DEFAULT_LATENCIES_MS
    jitter: bool = False
    jitter_seed: int = 0
    markout_horizons_s: tuple[int, ...] = (1, 10)

    def __post_init__(self) -> None:
        if self.stale_ms <= 0:
            raise EpisodeError(f"stale_ms must be > 0, got {self.stale_ms}")
        if self.merge_ms < 0:
            raise EpisodeError(f"merge_ms must be >= 0, got {self.merge_ms}")
        if self.max_notional is not None and self.max_notional < 0:
            raise EpisodeError(f"max_notional must be >= 0, got {self.max_notional}")
        if not self.latencies_ms or any(lat < 0 for lat in self.latencies_ms):
            raise EpisodeError("latencies_ms must be a non-empty list of >= 0 values")
        if any(horizon <= 0 for horizon in self.markout_horizons_s):
            raise EpisodeError("markout_horizons_s must be positive")


#: Shared immutable default so the signature avoids a call in the default.
_DEFAULT_CONFIG = EpisodeConfig()


def feed_validity(
    frame: pl.DataFrame,
    feeds: Sequence[Feed],
    *,
    stale_ms: int = DEFAULT_STALE_MS,
    gaps: pl.DataFrame | None = None,
    time_col: str = "t_ns",
) -> pl.DataFrame:
    """Add per-feed and combined validity masks to ``frame`` (SPEC-0008 §13.3).

    A feed is valid at ``t`` when its ``last_ns`` column is not null, ``t`` is no
    more than ``stale_ms`` newer than it, and ``t`` is not inside one of the
    feed's ``gaps`` (matching ``src`` and, when given, ``conn``). Overlapping and
    nested gaps are unioned before the test. The returned frame carries
    ``{feed.name}_valid`` columns plus a combined ``valid`` column; the original
    columns and row order are preserved.
    """
    _require_columns(frame, [time_col, *(feed.last_ns for feed in feeds)])
    stale_ns = _positive_ms(stale_ms, "stale_ms")
    out = frame
    for feed in feeds:
        fresh = (
            pl.col(feed.last_ns).is_not_null()
            & (pl.col(time_col) - pl.col(feed.last_ns) <= stale_ns)
        )
        gap = _gap_mask(out, feed, gaps, time_col)
        if gap is not None:
            fresh = fresh & ~gap
        out = out.with_columns(fresh.alias(f"{feed.name}_valid"))
    if feeds:
        combined = pl.all_horizontal(pl.col(f"{feed.name}_valid") for feed in feeds)
    else:
        combined = pl.lit(True)
    return out.with_columns(combined.alias("valid"))


def detect_episodes(
    frame: pl.DataFrame | pl.LazyFrame,
    *,
    net_bps: str | pl.Expr,
    size_usd: str | pl.Expr,
    valid: str | pl.Expr | None = None,
    config: EpisodeConfig = _DEFAULT_CONFIG,
    compete_usd: str | pl.Expr | None = None,
    mid: str | pl.Expr | None = None,
    direction: str | pl.Expr | None = None,
    time_col: str = "t_ns",
) -> pl.DataFrame:
    """Detect §13.3 episodes and capture the edge across the §13.4 grid.

    ``frame`` is the study's aligned, as-of joined frame. ``net_bps`` and
    ``size_usd`` are column names or polars expressions evaluated row-wise;
    ``net_bps`` must already be net of both legs' fees and slippage (see the
    module docstring). ``valid`` defaults to "all rows valid". ``compete_usd``
    (fill competition, §13.10) is an optional per-row notional that someone else
    traded at the episode price; ``mid``/``direction`` (a signed +1 buy / -1 sell
    expression) add per-latency markout columns.

    Returns one row per episode with the §13.3 fields (``t_start``, ``t_end``,
    ``duration_ms``, ``peak_net_bps``, ``start_net_bps``, ``size_usd_at_start``),
    a UTC ``date``, and ``captured*_{L}``/``open*_{L}`` per grid latency (plus
    ``competed*_{L}``, per-latency ``markout_{L}_{h}s`` and the jitter variants).
    Sorted by ``t_start``.
    """
    source = frame.collect() if isinstance(frame, pl.LazyFrame) else frame
    _require_columns(source, [time_col])
    _check_references(
        source,
        net_bps=net_bps,
        size_usd=size_usd,
        valid=valid,
        compete_usd=compete_usd,
        mid=mid,
        direction=direction,
    )

    work = source.with_row_index("_row")
    work = work.sort([time_col, "_row"], maintain_order=True)

    work = work.with_columns(
        [
            _as_expr(net_bps).cast(pl.Float64, strict=False).alias("net_bps"),
            _as_expr(size_usd)
            .cast(pl.Float64, strict=False)
            .clip(lower_bound=0.0)
            .alias("size_usd"),
        ]
    )
    valid_expr = pl.lit(True) if valid is None else _as_expr(valid)
    work = work.with_columns(valid_expr.fill_null(False).cast(pl.Boolean).alias("valid"))
    if compete_usd is not None:
        work = work.with_columns(
            _as_expr(compete_usd)
            .cast(pl.Float64, strict=False)
            .fill_null(0.0)
            .clip(lower_bound=0.0)
            .alias("compete_usd")
        )
    if mid is not None and direction is not None:
        work = work.with_columns(
            [
                _as_expr(mid).cast(pl.Float64, strict=False).alias("mid"),
                _as_expr(direction).cast(pl.Float64, strict=False).alias("direction"),
            ]
        )

    work = work.with_columns(
        (
            (pl.col("net_bps") > 0.0)
            & pl.col("valid")
            & pl.col("net_bps").is_not_null()
            & pl.col("size_usd").is_not_null()
        ).alias("core")
    )
    final = work.with_columns(
        pl.col(time_col).shift(-1).fill_null(pl.col(time_col)).alias("_row_end")
    )

    episodes = _merge_runs(final, config.merge_ms, config.stale_ms * MS_NS, time_col)
    if episodes.height == 0:
        return _empty_episodes(
            config, compete_usd is not None, mid is not None and direction is not None
        )

    episodes = _capture_grid(
        episodes,
        final,
        config,
        compete=compete_usd is not None,
        markout=mid is not None and direction is not None,
        time_col=time_col,
    )
    return episodes.sort("t_start")


def episode_metrics(
    episodes: pl.DataFrame,
    *,
    days: int,
    all_dates: Sequence[_dt.date] | None = None,
    latencies_ms: Sequence[int] | None = None,
    capture_variants: Sequence[str] | None = None,
    capital_usd: float | None = None,
    bootstrap_draws: int = 2000,
    bootstrap_seed: int = 0,
) -> pl.DataFrame:
    """Roll episodes into the per-latency §13.5 metrics (§13.4 grid).

    One row per ``(latency_ms, capture_variant)``. ``all_dates`` is **required**:
    the full list of valid days (length ``days``, no duplicates, every episode's
    ``date`` present) so the day-block bootstrap and ``usd_per_day`` count
    zero-episode days instead of silently dropping them. ``capture_variants``
    selects a subset of ``naive``/``adj``/``jitter``/``adj_jitter`` (default: all
    available); the §13.10 headline is ``adj_jitter`` when both hooks are on.
    ``capital_usd`` adds ``apr`` and its 90% CI. An empty episode table yields
    zero-valued rows rather than vanishing.
    """
    date_values = _validated_days(episodes, days, all_dates)
    latencies = tuple(latencies_ms) if latencies_ms is not None else DEFAULT_LATENCIES_MS
    per_day_counts = _per_day_counts(episodes, date_values)
    duration = episodes["duration_ms"].to_list() if "duration_ms" in episodes.columns else []
    peak = episodes["peak_net_bps"].to_list() if "peak_net_bps" in episodes.columns else []
    rows: list[dict[str, object]] = []
    for lat in latencies:
        variants = _select_variants(episodes, lat, capture_variants)
        for name, captured_col, open_col in variants:
            captured = episodes[captured_col].fill_null(0.0)
            n = episodes.height
            n_open = int(episodes[open_col].fill_null(False).sum())
            daily = _daily_totals(episodes, captured, date_values)
            lo, hi = day_block_bootstrap_ci(
                daily, draws=bootstrap_draws, level=0.90, seed=bootstrap_seed
            )
            row: dict[str, object] = {
                "latency_ms": lat,
                "capture_variant": name,
                "days": days,
                "episodes": n,
                "episodes_per_day": n / days,
                "episodes_per_day_p50": _quantile(sorted(per_day_counts), 0.5),
                "episodes_per_day_p90": _quantile(sorted(per_day_counts), 0.9),
                "duration_ms_p50": _quantile(sorted(duration), 0.5),
                "duration_ms_p90": _quantile(sorted(duration), 0.9),
                "peak_net_bps_p50": _quantile(sorted(peak), 0.5),
                "peak_net_bps_p90": _quantile(sorted(peak), 0.9),
                "capture_rate": (n_open / n) if n else 0.0,
                "capture_usd": float(captured.sum()),
                "usd_per_day": float(captured.sum()) / days,
                "usd_per_day_ci90_lo": lo,
                "usd_per_day_ci90_hi": hi,
                "competition_hint": _competition_hint(_quantile(sorted(duration), 0.5)),
            }
            row["markout_1s_median"] = _median_over_episodes(
                episodes, open_col, f"markout_{lat}_1s"
            )
            row["markout_10s_median"] = _median_over_episodes(
                episodes, open_col, f"markout_{lat}_10s"
            )
            if capital_usd is not None:
                scale = 365.0 / capital_usd
                row["apr"] = row["usd_per_day"] * scale
                row["apr_ci90_lo"] = lo * scale
                row["apr_ci90_hi"] = hi * scale
            rows.append(row)
    return pl.DataFrame(rows)


def daily_capture(
    episodes: pl.DataFrame,
    latency_ms: int,
    *,
    all_dates: Sequence[_dt.date] | None = None,
    variant: str = "naive",
) -> pl.DataFrame:
    """Per-UTC-day episode count and captured USD at one latency (§13.5).

    Every date in ``all_dates`` (required) gets a row, including days with no
    episode (zero count and zero capture). An episode is attributed to the UTC
    day of its ``t_start``. ``variant`` is one of ``naive``/``adj``/``jitter``/
    ``adj_jitter``.
    """
    dates = _required_dates(all_dates)
    captured_col, open_col = _variant_columns(latency_ms, variant)
    for column in (captured_col, open_col, "date"):
        if column not in episodes.columns:
            raise EpisodeError(f"episodes has no `{column}` column for daily_capture")
    counts = _per_day_counts(episodes, dates)
    totals = _daily_totals(episodes, episodes[captured_col].fill_null(0.0), dates)
    return pl.DataFrame(
        {
            "date": dates,
            "episodes": counts,
            "captured_usd": totals,
        }
    )


def oos_split(
    dates: Sequence[_dt.date],
    *,
    frac: float = 0.6,
) -> tuple[list[_dt.date], list[_dt.date]]:
    """Chronological 60/40 day split (SPEC-0008 §13.10).

    Returns ``(in_sample, out_of_sample)``: the sorted unique dates, with the
    first ``frac`` share in-sample. Parameters are chosen on ``in_sample`` and
    the headline is ``out_of_sample``. A single day is in-sample with an empty
    out-of-sample set.
    """
    if not 0.0 < frac < 1.0:
        raise EpisodeError(f"frac must be in (0, 1), got {frac}")
    ordered = sorted(set(dates))
    if not ordered:
        return ([], [])
    if len(ordered) == 1:
        return (ordered, [])
    n_in = max(1, min(int(len(ordered) * frac), len(ordered) - 1))
    return (ordered[:n_in], ordered[n_in:])


def day_block_bootstrap_ci(
    daily_values: Sequence[float],
    *,
    draws: int = 2000,
    level: float = 0.90,
    seed: int = 0,
) -> tuple[float, float]:
    """90% CI of the mean of per-day values, resampling days (§13.10).

    Deterministic for a given ``seed``. An empty input yields ``(nan, nan)``.
    """
    if draws <= 0:
        raise EpisodeError(f"draws must be > 0, got {draws}")
    if not 0.0 < level < 1.0:
        raise EpisodeError(f"level must be in (0, 1), got {level}")
    values = [float(value) for value in daily_values]
    if not values:
        return (math.nan, math.nan)
    n = len(values)
    rng = random.Random(seed)
    means = [sum(values[rng.randrange(n)] for _ in range(n)) / n for _ in range(draws)]
    means.sort()
    tail = (1.0 - level) / 2.0
    return (_quantile(means, tail), _quantile(means, 1.0 - tail))


def jittered_latency_ms(
    median_ms: int,
    n: int,
    *,
    seed: int = 0,
) -> list[float]:
    """``n`` lognormal latency draws with ``median_ms`` as median, p99 = 3× (§13.10).

    Deterministic for a given ``seed``; the draws are positive by construction.
    """
    if median_ms < 0:
        raise EpisodeError(f"median_ms must be >= 0, got {median_ms}")
    if n < 0:
        raise EpisodeError(f"n must be >= 0, got {n}")
    if median_ms == 0:
        return [0.0] * n
    sigma = math.log(3.0) / _Z99
    rng = random.Random(seed)
    mu = math.log(float(median_ms))
    return [rng.lognormvariate(mu, sigma) for _ in range(n)]


# --------------------------------------------------------------------------
# Internals
# --------------------------------------------------------------------------


def _merge_runs(
    frame: pl.DataFrame, merge_ms: int, stale_ns: int, time_col: str
) -> pl.DataFrame:
    """Turn the per-row ``core`` flag into merged episode rows.

    Each row's actionable window ends at ``min(next event, row_t + stale_ns)`` so
    a sparse book cannot stretch ``duration_ms``.
    """
    frame = frame.with_columns(
        pl.min_horizontal(
            pl.col("_row_end"), pl.col(time_col) + stale_ns
        ).alias("_row_valid_end")
    )
    runs = (
        frame.with_columns(pl.col("core").rle_id().alias("_rid"))
        .group_by("_rid")
        .agg(
            core=pl.col("core").first(),
            valid_all=pl.col("valid").all(),
            run_start=pl.col(time_col).first(),
        )
        .sort("_rid")
    )
    merge_ms_ns = merge_ms * MS_NS
    runs = runs.with_columns(
        merge_next=(
            pl.col("core")
            & pl.col("run_start").shift(-2).is_not_null()
            & (pl.col("core").shift(-2).fill_null(False))
            & pl.col("valid_all").shift(-1).fill_null(False)
            & ((pl.col("run_start").shift(-2) - pl.col("run_start").shift(-1)) < merge_ms_ns)
        )
    )
    runs = runs.with_columns(
        _merge_prev=pl.when(pl.col("core"))
        .then(pl.col("merge_next").shift(2))
        .otherwise(True)
        .fill_null(False)
    ).with_columns(_gid=(~pl.col("_merge_prev")).cum_sum())
    tagged = frame.with_columns(pl.col("core").rle_id().alias("_rid")).join(
        runs.select("_rid", "_gid"), on="_rid", how="left"
    )
    return (
        tagged.filter(pl.col("core"))
        .group_by("_gid")
        .agg(
            t_start=pl.col(time_col).first(),
            t_end=pl.col("_row_valid_end").max(),
            peak_net_bps=pl.col("net_bps").max(),
            start_net_bps=pl.col("net_bps").first(),
            size_usd_at_start=pl.col("size_usd").first(),
            direction=(
                pl.col("direction").first()
                if "direction" in frame.columns
                else pl.lit(None)
            ).alias("direction"),
        )
        .drop("_gid")
        .sort("t_start")
    )


def _capture_grid(
    episodes: pl.DataFrame,
    states: pl.DataFrame,
    config: EpisodeConfig,
    *,
    compete: bool,
    markout: bool,
    time_col: str,
) -> pl.DataFrame:
    """Add ``captured*_{L}``/``open*_{L}`` (and hooks) for every grid latency."""
    episodes = episodes.with_columns(
        ((pl.col("t_end") - pl.col("t_start")) // MS_NS).cast(pl.Int64).alias("duration_ms"),
        pl.col("t_start").cast(pl.Datetime("ns")).dt.date().alias("date"),
    )
    look_cols = ["net_bps", "size_usd"]
    stale_ns = config.stale_ms * MS_NS
    cap = config.max_notional
    if compete:
        states = states.with_columns(
            pl.col("compete_usd").cum_sum().alias("_compete_le")
        )
    for lat in config.latencies_ms:
        offset = pl.lit(lat * MS_NS, dtype=pl.Int64)
        episodes = _add_capture(
            episodes,
            states,
            offset,
            lat,
            look_cols,
            compete=compete,
            jitter=False,
            time_col=time_col,
            stale_ns=stale_ns,
            cap=cap,
        )
        if config.jitter:
            draws = jittered_latency_ms(lat, episodes.height, seed=config.jitter_seed + lat)
            jitter_offset = pl.Series("_lat", [round(v * MS_NS) for v in draws])
            episodes = _add_capture(
                episodes,
                states,
                jitter_offset,
                lat,
                look_cols,
                compete=compete,
                jitter=True,
                time_col=time_col,
                stale_ns=stale_ns,
                cap=cap,
            )
    if markout:
        episodes = _add_markouts(
            episodes, states, config, time_col=time_col, stale_ns=stale_ns
        )
    if cap is not None:
        episodes = episodes.with_columns(
            pl.min_horizontal(pl.col("size_usd_at_start"), pl.lit(float(cap))).alias(
                "size_usd_at_start"
            )
        )
    return episodes.drop("direction")


def _add_capture(
    episodes: pl.DataFrame,
    states: pl.DataFrame,
    offset: pl.Expr | pl.Series,
    lat: int,
    look_cols: list[str],
    *,
    compete: bool,
    jitter: bool,
    time_col: str,
    stale_ns: int,
    cap: float | None,
) -> pl.DataFrame:
    """Add the open/captured columns for one latency ``lat``.

    The order is sized at ``t_start``: ``displayed = min(size_usd(t_start),
    size_usd(t_start + L))``. The naive size is ``min(cap, displayed)`` and the
    competition-adjusted size is ``min(cap, max(0, displayed - competed))`` (cap
    after the subtraction). The probe is open only while the episode has not
    ended, the last state is no older than ``stale_ms``, and the edge is still
    positive.
    """
    suffix = "_jitter" if jitter else ""
    probe_key = pl.col("t_start") + offset
    lookup = _asof_lookup(episodes, states, probe_key, look_cols, time_col=time_col)
    episodes = episodes.with_columns(
        probe_key.alias("_probe_ns"),
        lookup["_state_ns"].alias("_state_ns"),
        lookup["net_bps"].fill_null(0.0).alias("_net_raw"),
        lookup["size_usd"].fill_null(0.0).clip(lower_bound=0.0).alias("_size_probe"),
    )
    temp = ["_probe_ns", "_state_ns", "_net_raw", "_size_probe"]
    net = pl.col("_net_raw").clip(lower_bound=0.0)
    displayed = pl.min_horizontal(pl.col("size_usd_at_start"), pl.col("_size_probe"))
    open_ = (
        (pl.col("_probe_ns") < pl.col("t_end"))
        & ((pl.col("_probe_ns") - pl.col("_state_ns")) <= stale_ns)
        & (pl.col("_net_raw") > 0.0)
    )
    naive_size = displayed if cap is None else pl.min_horizontal(displayed, pl.lit(float(cap)))
    new_cols = [
        open_.alias(f"open{suffix}_{lat}"),
        pl.when(open_)
        .then(net * naive_size * BPS_SCALE)
        .otherwise(0.0)
        .alias(f"captured{suffix}_{lat}"),
    ]
    if compete:
        before = _asof_lookup(
            episodes,
            states,
            pl.col("t_start"),
            ["_compete_le"],
            time_col=time_col,
            allow_exact=False,
        )["_compete_le"].fill_null(0.0)
        at_probe = _asof_lookup(
            episodes, states, probe_key, ["_compete_le"], time_col=time_col
        )["_compete_le"].fill_null(0.0)
        episodes = episodes.with_columns(
            (at_probe - before).clip(lower_bound=0.0).alias("_competed")
        )
        temp.append("_competed")
        adjusted = (displayed - pl.col("_competed")).clip(lower_bound=0.0)
        adj_size = adjusted if cap is None else pl.min_horizontal(adjusted, pl.lit(float(cap)))
        new_cols.extend(
            [
                pl.col("_competed").alias(f"competed{suffix}_{lat}"),
                pl.when(open_)
                .then(net * adj_size * BPS_SCALE)
                .otherwise(0.0)
                .alias(f"captured_adj{suffix}_{lat}"),
            ]
        )
    return episodes.with_columns(new_cols).drop(temp)


def _add_markouts(
    episodes: pl.DataFrame,
    states: pl.DataFrame,
    config: EpisodeConfig,
    *,
    time_col: str,
    stale_ns: int,
) -> pl.DataFrame:
    """Add per-latency signed mid-move (bps) markout columns.

    For every grid latency ``L`` and horizon ``h`` the column is
    ``markout_{L}_{h}s``; it is null when either the fill or the horizon state
    is in a gap (as-of ``valid`` false) or older than ``stale_ms``.
    """
    if not config.markout_horizons_s:
        return episodes
    for lat in config.latencies_ms:
        probe_key = pl.col("t_start") + pl.lit(lat * MS_NS, dtype=pl.Int64)
        fill = _asof_lookup(episodes, states, probe_key, ["mid", "valid"], time_col=time_col)
        episodes = episodes.with_columns(
            probe_key.alias("_fill_ns"),
            fill["_state_ns"].alias("_fill_state"),
            fill["mid"].alias("_mid_fill"),
            fill["valid"].fill_null(False).alias("_valid_fill"),
        )
        for horizon in config.markout_horizons_s:
            horizon_ns = pl.lit(horizon * 1_000 * MS_NS, dtype=pl.Int64)
            after_key = probe_key + horizon_ns
            after = _asof_lookup(
                episodes, states, after_key, ["mid", "valid"], time_col=time_col
            )
            episodes = episodes.with_columns(
                after["_state_ns"].alias("_after_state"),
                after["mid"].alias("_mid_after"),
                after["valid"].fill_null(False).alias("_valid_after"),
            )
            fresh = (
                (pl.col("_fill_ns") - pl.col("_fill_state")) <= stale_ns
            ) & ((pl.col("_fill_ns") + horizon_ns - pl.col("_after_state")) <= stale_ns)
            move = (
                pl.when(
                    (pl.col("_mid_fill") > 0)
                    & pl.col("_mid_after").is_not_null()
                    & pl.col("_valid_fill")
                    & pl.col("_valid_after")
                    & fresh
                )
                .then(
                    pl.col("direction")
                    * (pl.col("_mid_after") - pl.col("_mid_fill"))
                    / pl.col("_mid_fill")
                    / BPS_SCALE
                )
                .otherwise(None)
                .alias(f"markout_{lat}_{horizon}s")
            )
            episodes = episodes.with_columns(move).drop(
                "_after_state", "_mid_after", "_valid_after"
            )
        episodes = episodes.drop("_fill_ns", "_fill_state", "_mid_fill", "_valid_fill")
    return episodes


def _asof_lookup(
    episodes: pl.DataFrame,
    states: pl.DataFrame,
    key: pl.Expr,
    value_cols: Sequence[str],
    *,
    time_col: str,
    allow_exact: bool = True,
) -> dict[str, pl.Series]:
    """Backward as-of lookup of ``value_cols`` from ``states`` at ``key``.

    ``states`` must be sorted by ``time_col``. The returned series are aligned to
    ``episodes`` order and include ``_state_ns``, the matched row's timestamp;
    only rows at or before ``key`` are ever read.
    """
    indexed = episodes.with_row_index("_i").select(pl.col("_i"), key.alias("_probe"))
    probe = indexed.sort("_probe")
    lookup = states.select(
        pl.col(time_col).alias("_probe"),
        pl.col(time_col).alias("_state_ns"),
        *value_cols,
    )
    joined = probe.join_asof(
        lookup,
        on="_probe",
        strategy="backward",
        allow_exact_matches=allow_exact,
        check_sortedness=True,
    ).sort("_i")
    return {col: joined[col] for col in (*value_cols, "_state_ns")}


def _empty_episodes(
    config: EpisodeConfig,
    compete: bool,
    markout: bool,
) -> pl.DataFrame:
    """Build a correctly typed zero-row episode frame."""
    schema: dict[str, pl.DataType] = {
        "t_start": pl.Int64,
        "t_end": pl.Int64,
        "duration_ms": pl.Int64,
        "peak_net_bps": pl.Float64,
        "start_net_bps": pl.Float64,
        "size_usd_at_start": pl.Float64,
        "date": pl.Date,
    }
    for lat in config.latencies_ms:
        schema[f"open_{lat}"] = pl.Boolean
        schema[f"captured_{lat}"] = pl.Float64
        if compete:
            schema[f"competed_{lat}"] = pl.Float64
            schema[f"captured_adj_{lat}"] = pl.Float64
        if config.jitter:
            schema[f"open_jitter_{lat}"] = pl.Boolean
            schema[f"captured_jitter_{lat}"] = pl.Float64
            if compete:
                schema[f"competed_jitter_{lat}"] = pl.Float64
                schema[f"captured_adj_jitter_{lat}"] = pl.Float64
    if markout:
        for lat in config.latencies_ms:
            for horizon in config.markout_horizons_s:
                schema[f"markout_{lat}_{horizon}s"] = pl.Float64
    return pl.DataFrame(schema=schema)


def _gap_mask(
    frame: pl.DataFrame,
    feed: Feed,
    gaps: pl.DataFrame | None,
    time_col: str,
) -> pl.Series | None:
    """Boolean series: is ``t`` inside one of the feed's (unioned) gaps?"""
    if gaps is None or (feed.src is None and feed.conn is None):
        return None
    _require_columns(gaps, ["start_ns", "end_ns"])
    subset = gaps
    if feed.src is not None:
        subset = subset.filter(pl.col("src") == feed.src)
    if feed.conn is not None:
        subset = subset.filter(pl.col("conn") == feed.conn)
    if subset.height == 0:
        return pl.Series([False] * frame.height)
    unioned = _union_intervals(subset.select("start_ns", "end_ns"))
    probes = frame.select(time_col).join_asof(
        unioned, left_on=time_col, right_on="start_ns", strategy="backward"
    )
    return probes["start_ns"].is_not_null() & (probes[time_col] < probes["end_ns"])


def _union_intervals(intervals: pl.DataFrame) -> pl.DataFrame:
    """Merge overlapping/nested ``[start_ns, end_ns)`` intervals.

    Sorts by start, then starts a new group whenever a start is past the running
    maximum end; each group is collapsed to ``(min start, max end)``. Rows are
    clipped to a non-negative width because the frames use closed gaps.
    """
    ordered = intervals.sort("start_ns")
    new_group = pl.col("start_ns") > pl.col("end_ns").cum_max().shift(1).fill_null(True)
    return (
        ordered.with_columns(new_group.cast(pl.Int64).cum_sum().alias("_g"))
        .group_by("_g")
        .agg(
            start_ns=pl.col("start_ns").min(),
            end_ns=pl.col("end_ns").max(),
        )
        .sort("start_ns")
    )


def _variant_columns(lat: int, variant: str) -> tuple[str, str]:
    """Return ``(captured, open)`` column names for one capture variant."""
    for name, captured_format, open_format in _VARIANTS:
        if name == variant:
            return captured_format.format(lat=lat), open_format.format(lat=lat)
    known = ", ".join(name for name, _, _ in _VARIANTS)
    raise EpisodeError(f"unknown capture variant `{variant}` (known: {known})")


def _select_variants(
    episodes: pl.DataFrame,
    lat: int,
    requested: Sequence[str] | None,
) -> list[tuple[str, str, str]]:
    """Available ``(name, captured, open)`` variants, filtered by ``requested``."""
    available = [
        (name, *_variant_columns(lat, name))
        for name, _, _ in _VARIANTS
        if _variant_columns(lat, name)[0] in episodes.columns
        and _variant_columns(lat, name)[1] in episodes.columns
    ]
    if requested is None:
        return available
    names = {name for name, _, _ in available}
    for name in requested:
        if name not in names:
            raise EpisodeError(f"capture variant `{name}` is not available at latency {lat}")
    wanted = set(requested)
    return [variant for variant in available if variant[0] in wanted]


def _validated_days(
    episodes: pl.DataFrame,
    days: int,
    all_dates: Sequence[_dt.date] | None,
) -> list[_dt.date]:
    """Validate and return the required, complete list of valid days."""
    dates = _required_dates(all_dates)
    if days <= 0:
        raise EpisodeError(f"days must be > 0, got {days}")
    if len(dates) != days:
        raise EpisodeError(
            f"all_dates has {len(dates)} days but days = {days}; every valid day "
            "must be listed so zero-episode days are counted"
        )
    if "date" in episodes.columns and episodes.height:
        allowed = set(dates)
        unknown = sorted(
            {day for day in episodes["date"].to_list() if day not in allowed}
        )
        if unknown:
            raise EpisodeError(
                f"episode date(s) {unknown} are not in all_dates"
            )
    return dates


def _required_dates(all_dates: Sequence[_dt.date] | None) -> list[_dt.date]:
    """Return ``all_dates`` as a duplicate-free list or raise."""
    if all_dates is None:
        raise EpisodeError(
            "all_dates is required so zero-episode days are counted in "
            "usd_per_day and the bootstrap CI"
        )
    dates = list(all_dates)
    if not dates:
        raise EpisodeError("all_dates must not be empty")
    if len(set(dates)) != len(dates):
        raise EpisodeError("all_dates must not contain duplicates")
    return dates


def _as_expr(value: str | pl.Expr) -> pl.Expr:
    if isinstance(value, str):
        return pl.col(value)
    if isinstance(value, pl.Expr):
        return value
    raise EpisodeError(f"expected a column name or polars expression, got {type(value).__name__}")


def _require_columns(frame: pl.DataFrame, columns: Sequence[str]) -> None:
    missing = [col for col in columns if col not in frame.columns]
    if missing:
        raise EpisodeError(f"frame is missing column(s): {', '.join(missing)}")


def _check_references(frame: pl.DataFrame, **references: object) -> None:
    """Fail loudly when a string reference names a missing column."""
    for name, value in references.items():
        if isinstance(value, str) and value not in frame.columns:
            raise EpisodeError(f"frame is missing column `{value}` for `{name}`")


def _positive_ms(value: int, name: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
        raise EpisodeError(f"{name} must be a positive integer, got {value!r}")
    return value * MS_NS


def _per_day_counts(episodes: pl.DataFrame, dates: Sequence[_dt.date]) -> list[int]:
    if episodes.height == 0 or "date" not in episodes.columns:
        return [0] * len(dates)
    counts = episodes.group_by("date").agg(pl.len().alias("n"))
    present = {row["date"]: row["n"] for row in counts.iter_rows(named=True)}
    return [present.get(day, 0) for day in dates]


def _daily_totals(
    episodes: pl.DataFrame,
    captured: pl.Series,
    dates: Sequence[_dt.date],
) -> list[float]:
    if episodes.height == 0 or "date" not in episodes.columns:
        return [0.0] * len(dates)
    frame = episodes.select("date").with_columns(captured.alias("_captured"))
    totals = frame.group_by("date").agg(pl.col("_captured").sum())
    present = {row["date"]: row["_captured"] for row in totals.iter_rows(named=True)}
    return [float(present.get(day, 0.0)) for day in dates]


def _median_over_episodes(
    episodes: pl.DataFrame, open_col: str, column: str
) -> float | None:
    """Median of ``column`` over episodes open at that latency."""
    if column not in episodes.columns or open_col not in episodes.columns:
        return None
    values = episodes.filter(episodes[open_col].fill_null(False))[column].drop_nulls()
    if values.is_empty():
        return None
    return float(values.median())


def _competition_hint(duration_p50_ms: float) -> str:
    if duration_p50_ms < 100.0:
        return "latency-competitive"
    if duration_p50_ms > 2000.0:
        return "slow / capacity-bound"
    return "mixed"


def _quantile(sorted_values: Sequence[float], q: float) -> float:
    n = len(sorted_values)
    if n == 0:
        return math.nan
    if n == 1:
        return float(sorted_values[0])
    position = q * (n - 1)
    lower = math.floor(position)
    upper = min(lower + 1, n - 1)
    fraction = position - lower
    return float(sorted_values[lower]) * (1.0 - fraction) + float(sorted_values[upper]) * fraction
