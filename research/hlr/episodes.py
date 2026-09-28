"""Episode detector and latency capture (task P-4, SPEC-0008 §13.3–§13.5).

A *study* maps its markets into one aligned, as-of joined polars frame on ``t_ns``
(``join_asof`` backward, so a row only ever holds data that was already
available) and supplies three things:

* a ``net_bps`` column/expression — the §13.3 edge in basis points, already net
  of fees, the safety buffer and slippage at that row's size;
* a ``size_usd`` column/expression — the §13.3 capturable notional, already
  capped at the study's ``max_notional`` (or pass ``max_notional`` to
  :func:`detect_episodes` to cap it here);
* a ``valid`` mask — whether every input feed is fresh and gap-free (§13.3),
  built with :func:`feed_validity` from a ``stale_ms`` and the ``gaps`` table.

:func:`detect_episodes` then finds the maximal ``[t_start, t_end)`` intervals
where ``net_bps > 0`` and the feeds are valid, merges episodes separated by less
than ``merge_ms``, and captures the edge at each latency ``L`` in the §13.4 grid
by re-reading the as-of state at ``t_start + L`` (never a later row, so there is
no lookahead). :func:`episode_metrics` rolls the episodes up into the per-latency
part of the §13.5 table.

Conservative readings (the spec is silent or terse in these places; see the P-4
report):

* **Capture is USD.** §13.3 writes ``captured_L = net_bps(...) × size_usd(...)``,
  but §13.5 makes ``Σ captured_L / days`` a USD/day figure, so the bps value is
  applied as a fraction: ``captured_L = net_bps / 1e4 × size_usd``.
* **``t_end`` is the first event time at which the condition no longer holds**
  (the interval is half open, so an episode is *not* open at exactly ``t_end``).
  At the end of the observed data ``t_end`` is the last event time.
* **Merging never crosses invalid data.** A short ``<= merge_ms`` separation is
  merged only when the separating rows are valid; a gap or a stale feed is not
  papered over.
* **Gaps are half open** ``[start_ns, end_ns)``: a feed is valid again at
  ``end_ns``.
* **The probe edge is clamped at >= 0.** A merged episode can contain a sub-
  ``merge_ms`` stretch where the edge briefly vanished; that stretch never counts
  as a negative capture.
* **A capture needs a fresh probe.** Besides the episode still being open, the
  last state observed at or before ``t_start + L`` must be no older than
  ``stale_ms``; a probe onto a held-but-stale quote is counted as missed rather
  than assumed still actionable.
* **``size_usd`` is capped at ``max_notional``** when that is set, before any
  capture arithmetic.
* **``date`` is the UTC day of ``t_start``**, so an episode that crosses midnight
  stays one episode attributed to the day it began (the §13.5 per-day counts).
* **A null ``net_bps`` or ``size_usd`` is not an episode.**

Rigor hooks for §13.10 live here so P-5/studies do not re-implement them:
:func:`day_block_bootstrap_ci` (uncertainty), :func:`jittered_latency_ms` (latency
jitter), the ``compete_usd`` argument of :func:`detect_episodes` (fill
competition), the optional ``mid``/``direction`` markout columns (adverse
selection), and the ``date`` column plus :func:`daily_capture` (OOS split and
per-day aggregation). The verdict itself (APR, CI gating, capital grid) is P-5.

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
    "DEFAULT_HEADLINE_LATENCY_MS",
    "DEFAULT_LATENCIES_MS",
    "DEFAULT_MERGE_MS",
    "DEFAULT_STALE_MS",
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
]

#: Nanoseconds per millisecond.
MS_NS = 1_000_000
#: Nanoseconds per UTC day.
DAY_NS = 86_400 * MS_NS
#: bps -> fraction.
BPS_SCALE = 1e-4

#: The §13.4 latency grid, in milliseconds.
DEFAULT_LATENCIES_MS: tuple[int, ...] = (10, 50, 100, 250, 500, 1000)
#: The headline latency until V-4/H-7 replace it (§13.4).
DEFAULT_HEADLINE_LATENCY_MS = 250
#: The §13.3 feed-staleness default.
DEFAULT_STALE_MS = 2000
#: The §13.3 episode-merge default.
DEFAULT_MERGE_MS = 50

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
    markout_latency_ms: int = DEFAULT_HEADLINE_LATENCY_MS
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
        if self.markout_latency_ms < 0:
            raise EpisodeError("markout_latency_ms must be >= 0")


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
    feed's ``gaps`` (matching ``src`` and, when given, ``conn``). The returned
    frame carries ``{feed.name}_valid`` columns plus a combined ``valid`` column;
    the original columns and row order are preserved.
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
    ``valid`` defaults to "all rows valid". ``compete_usd`` (fill competition,
    §13.10) is an optional per-row notional that someone else traded at the
    episode price; ``mid``/``direction`` (a signed +1 buy / -1 sell expression)
    add per-episode markout columns at ``config.markout_latency_ms``.

    Returns one row per episode with the §13.3 fields (``t_start``, ``t_end``,
    ``duration_ms``, ``peak_net_bps``, ``start_net_bps``, ``size_usd_at_start``),
    a UTC ``date``, and ``captured_{L}``/``open_{L}`` per grid latency (plus
    ``competed_{L}``/``captured_adj_{L}``, ``*_jitter_{L}`` and
    ``markout_{h}s`` when the matching hook is enabled). Sorted by ``t_start``.
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
            _as_expr(size_usd).cast(pl.Float64, strict=False).alias("size_usd"),
        ]
    )
    if config.max_notional is not None:
        work = work.with_columns(
            pl.min_horizontal(pl.col("size_usd"), pl.lit(float(config.max_notional))).alias(
                "size_usd"
            )
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

    episodes = _merge_runs(final, config.merge_ms, time_col)
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
    latencies_ms: Sequence[int] | None = None,
    all_dates: Sequence[_dt.date] | None = None,
    capital_usd: float | None = None,
    bootstrap_draws: int = 2000,
    bootstrap_seed: int = 0,
) -> pl.DataFrame:
    """Roll an episode table up into the per-latency §13.5 metrics (§13.4 grid).

    One row per latency ``L``: episode counts, duration/peak percentiles,
    ``capture_rate_L``, naive and (when present) competition-adjusted capture and
    ``usd_per_day_L``, the day-block-bootstrap 90% CI, markout medians and the
    ``competition_hint``. ``days`` is the number of valid days used (§13.5) and
    should match ``len(all_dates)`` when that is given, so the bootstrap's
    per-day mean equals ``usd_per_day_L``; ``all_dates`` (optional) adds
    zero-episode days to the per-day stats. ``capital_usd`` (optional) adds
    ``apr_L`` and its 90% CI.
    """
    if days <= 0:
        raise EpisodeError(f"days must be > 0, got {days}")
    latencies = tuple(latencies_ms) if latencies_ms is not None else DEFAULT_LATENCIES_MS
    if episodes.height == 0:
        return pl.DataFrame()

    date_values = _date_values(episodes, all_dates)
    duration = episodes["duration_ms"]
    peak = episodes["peak_net_bps"]
    per_day_counts = _per_day_counts(episodes, date_values)
    rows: list[dict[str, object]] = []
    for lat in latencies:
        captured_col = f"captured_{lat}"
        open_col = f"open_{lat}"
        if captured_col not in episodes.columns or open_col not in episodes.columns:
            raise EpisodeError(f"episodes has no `{captured_col}`/`{open_col}` column")
        captured = episodes[captured_col].fill_null(0.0)
        n = episodes.height
        n_open = int(episodes[open_col].fill_null(False).sum())
        daily = _daily_totals(episodes, captured, date_values)
        lo, hi = day_block_bootstrap_ci(
            daily, draws=bootstrap_draws, level=0.90, seed=bootstrap_seed
        )
        row: dict[str, object] = {
            "latency_ms": lat,
            "days": days,
            "episodes": n,
            "episodes_per_day": n / days,
            "episodes_per_day_p50": _quantile(sorted(per_day_counts), 0.5),
            "episodes_per_day_p90": _quantile(sorted(per_day_counts), 0.9),
            "duration_ms_p50": _quantile(sorted(duration.to_list()), 0.5),
            "duration_ms_p90": _quantile(sorted(duration.to_list()), 0.9),
            "peak_net_bps_p50": _quantile(sorted(peak.to_list()), 0.5),
            "peak_net_bps_p90": _quantile(sorted(peak.to_list()), 0.9),
            "capture_rate": (n_open / n) if n else 0.0,
            "capture_naive_usd": float(captured.sum()),
            "usd_per_day": float(captured.sum()) / days,
            "usd_per_day_ci90_lo": lo,
            "usd_per_day_ci90_hi": hi,
            "competition_hint": _competition_hint(_quantile(sorted(duration.to_list()), 0.5)),
        }
        adj_col = f"captured_adj_{lat}"
        if adj_col in episodes.columns:
            adj = episodes[adj_col].fill_null(0.0)
            row["capture_adj_usd"] = float(adj.sum())
            row["usd_per_day_adj"] = float(adj.sum()) / days
        row["markout_1s_median"] = _median_over(captured > 0, episodes, "markout_1s")
        row["markout_10s_median"] = _median_over(captured > 0, episodes, "markout_10s")
        if capital_usd is not None:
            scale = 365.0 / capital_usd
            row["apr"] = row["usd_per_day"] * scale
            row["apr_ci90_lo"] = lo * scale
            row["apr_ci90_hi"] = hi * scale
        rows.append(row)
    return pl.DataFrame(rows)


def daily_capture(episodes: pl.DataFrame, latency_ms: int) -> pl.DataFrame:
    """Per-UTC-day episode count and captured USD at one latency (§13.5).

    An episode is attributed to the day of its ``t_start``. Days with no episode
    are absent; add them with ``all_dates`` before a bootstrap if the study needs
    them counted.
    """
    captured_col = f"captured_{latency_ms}"
    if "date" not in episodes.columns or captured_col not in episodes.columns:
        raise EpisodeError(f"episodes needs `date` and `{captured_col}` for daily_capture")
    return (
        episodes.group_by("date")
        .agg(
            episodes=pl.len(),
            captured_usd=pl.col(captured_col).fill_null(0.0).sum(),
        )
        .sort("date")
    )


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


def _merge_runs(frame: pl.DataFrame, merge_ms: int, time_col: str) -> pl.DataFrame:
    """Turn the per-row ``core`` flag into merged episode rows."""
    runs = (
        frame.with_columns(pl.col("core").rle_id().alias("_rid"))
        .group_by("_rid")
        .agg(
            core=pl.col("core").first(),
            valid_all=pl.col("valid").all(),
            run_start=pl.col(time_col).first(),
            row_end=pl.col("_row_end").max(),
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
            t_end=pl.col("_row_end").max(),
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
    """Add ``captured_{L}``/``open_{L}`` (and hooks) for every grid latency."""
    episodes = episodes.with_columns(
        ((pl.col("t_end") - pl.col("t_start")) // MS_NS).cast(pl.Int64).alias("duration_ms"),
        pl.col("t_start").cast(pl.Datetime("ns")).dt.date().alias("date"),
    )
    look_cols = ["net_bps", "size_usd"]
    stale_ns = config.stale_ms * MS_NS
    if compete:
        states = states.with_columns(
            [
                pl.col("compete_usd").cum_sum().alias("_compete_le"),
            ]
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
            )
    if markout:
        episodes = _add_markouts(episodes, states, config, time_col=time_col)
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
) -> pl.DataFrame:
    """Add the open/captured columns for one latency ``lat``.

    The probe at ``t_start + L`` is open only while the episode has not ended
    *and* the last state observed at or before the probe is no older than
    ``stale_ms`` — a conservative guard against capturing a held-but-stale
    quote between two sparse events (§13.3 validity, read at the probe).
    """
    suffix = "_jitter" if jitter else ""
    probe_key = pl.col("t_start") + offset
    lookup = _asof_lookup(episodes, states, probe_key, look_cols, time_col=time_col)
    episodes = episodes.with_columns(
        probe_key.alias("_probe_ns"),
        lookup["_state_ns"].alias("_state_ns"),
        lookup["net_bps"].fill_null(0.0).clip(lower_bound=0.0).alias("_net"),
        lookup["size_usd"].fill_null(0.0).clip(lower_bound=0.0).alias("_size"),
    )
    temp = ["_probe_ns", "_state_ns", "_net", "_size"]
    open_ = (pl.col("_probe_ns") < pl.col("t_end")) & (
        (pl.col("_probe_ns") - pl.col("_state_ns")) <= stale_ns
    )
    new_cols = [
        open_.alias(f"open{suffix}_{lat}"),
        pl.when(open_).then(pl.col("_net") * pl.col("_size") * BPS_SCALE).otherwise(0.0).alias(
            f"captured{suffix}_{lat}"
        ),
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
        adjusted = (pl.col("_size") - pl.col("_competed")).clip(lower_bound=0.0)
        new_cols.extend(
            [
                pl.col("_competed").alias(f"competed{suffix}_{lat}"),
                pl.when(open_)
                .then(pl.col("_net") * adjusted * BPS_SCALE)
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
) -> pl.DataFrame:
    """Add signed mid-move (bps) markout columns after the hypothetical fill."""
    if not config.markout_horizons_s:
        return episodes
    offset = pl.lit(config.markout_latency_ms * MS_NS, dtype=pl.Int64)
    probe_key = pl.col("t_start") + offset
    mid_fill = _asof_lookup(episodes, states, probe_key, ["mid"], time_col=time_col)["mid"]
    episodes = episodes.with_columns(mid_fill.alias("_mid_fill"))
    for horizon in config.markout_horizons_s:
        after_key = probe_key + pl.lit(horizon * 1_000 * MS_NS, dtype=pl.Int64)
        mid_after = _asof_lookup(episodes, states, after_key, ["mid"], time_col=time_col)["mid"]
        episodes = episodes.with_columns(mid_after.alias("_mid_after"))
        episodes = episodes.with_columns(
            pl.when((pl.col("_mid_fill") > 0) & pl.col("_mid_after").is_not_null())
            .then(
                pl.col("direction")
                * (pl.col("_mid_after") - pl.col("_mid_fill"))
                / pl.col("_mid_fill")
                / BPS_SCALE
            )
            .otherwise(None)
            .alias(f"markout_{horizon}s")
        ).drop("_mid_after")
    return episodes.drop("_mid_fill")


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
        for horizon in config.markout_horizons_s:
            schema[f"markout_{horizon}s"] = pl.Float64
    return pl.DataFrame(schema=schema)


def _gap_mask(
    frame: pl.DataFrame,
    feed: Feed,
    gaps: pl.DataFrame | None,
    time_col: str,
) -> pl.Series | None:
    """Boolean series: is ``t`` inside one of the feed's gaps?"""
    if gaps is None or (feed.src is None and feed.conn is None):
        return None
    _require_columns(gaps, ["start_ns", "end_ns"])
    subset = gaps
    if feed.src is not None:
        subset = subset.filter(pl.col("src") == feed.src)
    if feed.conn is not None:
        subset = subset.filter(pl.col("conn") == feed.conn)
    subset = subset.select("start_ns", "end_ns").sort("start_ns")
    if subset.height == 0:
        return pl.Series([False] * frame.height)
    probes = frame.select(time_col).join_asof(
        subset, left_on=time_col, right_on="start_ns", strategy="backward"
    )
    return probes["start_ns"].is_not_null() & (probes[time_col] < probes["end_ns"])


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


def _date_values(episodes: pl.DataFrame, all_dates: Sequence[_dt.date] | None) -> list[_dt.date]:
    if all_dates is not None:
        dates = list(all_dates)
    else:
        dates = sorted(set(episodes["date"].to_list()))
    if not dates:
        raise EpisodeError("episodes has no dates to score")
    return dates


def _per_day_counts(episodes: pl.DataFrame, dates: Sequence[_dt.date]) -> list[int]:
    counts = episodes.group_by("date").agg(pl.len().alias("n"))
    present = {row["date"]: row["n"] for row in counts.iter_rows(named=True)}
    return [present.get(day, 0) for day in dates]


def _daily_totals(
    episodes: pl.DataFrame,
    captured: pl.Series,
    dates: Sequence[_dt.date],
) -> list[float]:
    frame = episodes.select("date").with_columns(captured.alias("_captured"))
    totals = frame.group_by("date").agg(pl.col("_captured").sum())
    present = {row["date"]: row["_captured"] for row in totals.iter_rows(named=True)}
    return [float(present.get(day, 0.0)) for day in dates]


def _median_over(mask: pl.Series, episodes: pl.DataFrame, column: str) -> float | None:
    if column not in episodes.columns:
        return None
    values = episodes.filter(mask)[column].drop_nulls()
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
