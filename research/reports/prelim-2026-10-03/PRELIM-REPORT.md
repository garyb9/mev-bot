> Written by the orchestrator from subagent hand-backs; UNVERIFIED items are marked.

# PRELIMINARY (day-3) studies, recorder forward data (SPEC-0008 §13)

Written by the orchestrator from the subagent's hand-back (the subagent could not write files). Scripts, PREREG.md (with amendment A1), raw/ parquet and result CSVs are in this directory. Repo untouched, nothing committed, nothing written under /mnt/e/mev-rec.

Data: 4 UTC days, ~77 h of hl-ws, finalized .zst only. In-sample (IS) 09-29 + 09-30; out-of-sample (OOS) 10-01 + 10-02 (only 2 OOS days). Hours/day: 15, 24, 24, 14. H1 update-level bbo. Contrary to the earlier assumption, HL `bbo` for HIP-3 dexes (xyz/io/para/mkts) and ~300 spot `@N` pairs IS recorded, so S-1/2/3/19 were feasible.

## Verdicts: nothing PASSes

| Study | Verdict | One line |
|---|---|---|
| S-5 Part 1 lead-lag | Hypothesis supported | HL perp lags Binance/Bybit ~0.5 s (BTC/ETH/SOL), 0.1-0.25 s (HYPE) |
| S-5 Part 2 stale quotes | FAIL (32/32 cells) | gross lag edge ~+4 bps over 5 s; HL taker round trip 9 bps; hedged cells <10 episodes/day |
| S-3 spot vs perp | INCONCLUSIVE, leans FAIL | majors ~0 episodes; thin alts real but dust (~$112/day total capacity) |
| S-1 HIP-3 cross-dex | INCONCLUSIVE, leans MARGINAL | IS-chosen cell OOS APR 14.5% [13.3, 15.8] but ~$10/day; ~$89/day across pairs |
| S-2 stable triangles | FAIL | 2 of 6 triangles have data; best OOS APR 8.6%, CI-lo 0.4% |
| S-19 peg | FAIL / nothing to trade | USDT0 0.5-7 bps under par, USDE 3-4 bps; never beyond the 3.4 bps cost |
| S-11a Bollinger on spreads | FAIL | 0 of 792 cells positive mean net PnL, IS or OOS (OOS mean -34 bps/trade) |
| S-12 oracle lag | INCONCLUSIVE (not testable) | ctx is 1 Hz with no venue time; ~3.06 s oracle tick confirmed |
| Others (S-4, S-6..S-10, S-11b, S-13..S-18, S-20..S-23) | not run | need HyperEVM, equities, options or node data; S-6, S-7 feasible later |

"FAIL on concentration" is mechanical: the toolkit gates best-single-day PnL share at 0.40, and with 2 OOS days the minimum possible is 0.50. Those cells are reported INCONCLUSIVE where nothing else failed.

## Data quality
- Recorder coverage 0.98-0.99 for HL-only studies, 0.76 for Binance-usdm cells. 09-29 down ~13:27-16:45Z. 10-02 hours 00-13 only (this run).
- Binance usdm is the weak feed: 1,673 reconnect gaps (20-30 s each, old policy), 09-30 lost 9.8% of the day; 19.9% of records arrive >1 s after event time (p99 14.5 s). Bybit clean, HL nearly so. Binance spot bookTicker has no timestamp.
- Local clock drifts (t_ns minus venue time: +172 ms on 09-29 to -417 ms on 10-02 for HL). All timelines use venue event time (HL data.time, Binance E, Bybit cts). Add HL's real reveal lag (~230 ms per Tardis) on top.
- No competition model (`compete_usd` = 0, HL `trades` recorded but not extracted) and funding not charged: every number is a generous upper bound. A FAIL is robust; a pass would not be.

## Key findings
- S-5: lead-lag is real (peak lags 480-540 ms BTC/ETH/SOL vs both CEXes, stable across days) but gross convergence ~3.6-5 bps vs a 9 bps taker round trip. It breaks even only at an HL round-trip fee of ~4 bps or less (maker/rebate tiers). Unhedged PnL (enter HL ask +250 ms, exit HL bid +5.25 s) is negative in all 16 groups (OOS -0.1 to -7.1 bps). The toolkit's unhedged net_bps books exit at fair (absurd APRs); not realizable.
- S-1: positive in both halves, but real flicker in thin io: books, sizes $31-$500; dust. Fits only as a low-capital add-on. Not charged: 1 h funding differential, competition.
- S-3: best OOS cells UMON/PURR/UZEC (APR 36-52%) pass all gates except concentration; PURR is the HIP-2 book, likely contested; sell-spot needs inventory.
- S-12: oracle changes in 21-25% of frames, inter-change p50 3.06 s; fair is Binance-only (no OKX weights). Needs sub-second ctx.

## Would a paid purchase (Massive $29) change a verdict? No
Massive is equity data; none of S-1/2/3/5/11a/12/19 use equities. Cheap things that WOULD move verdicts: more recorder days (>=14 final, >=5 OOS days so the concentration gate means something), extract recorded HL `trades` to build compete_usd, know the O5 fee tier, add l2Book depth for spot/HIP-3 pairs + funding. Tardis (~$350/mo) would not change today's verdicts.

## Toolkit fixes needed (none applied)
1. hlr.normalize handles only HL sources; no CEX normalizer (R-8), no all-coin bbo / spot @N mapping (extract.py written instead).
2. 2 s staleness rule in detect_episodes mislabels quiet HL bbo; needs per-connection heartbeat feeds.
3. Unhedged capture books exit at fair with no exit-leg cost; the spec's +5 s HL exit PnL should be first-class.
4. grade_study has no adj_jitter variant without compete_usd; a trades-based competition helper is missing.
5. Concentration gate unevaluable with <5 OOS days; should be INCONCLUSIVE, not FAIL.
6. gap_start{shutdown} should be terminated by the next conn_open/segment_open, not a later gap_end (pairs up to 33,000 s apart).
7. costs.toml para fee scale (0.5) disagrees with recorded per-asset deployerFeeScale (1.0).

## Incident
An early parallel S-5 run (5 workers, 6-14 GB RSS each) exhausted the 24 GB WSL, OOM-killed workers and stopped the live recorder (~21:36 to 22:10+03). Final runs: single process, ulimit -v 8000000, chunked, peak RSS ~1.5 GB.

## Reproduce
`cd research && PYTHONPATH= uv run python <script>` from this directory's scripts: extract.py, dq*.py, s5_p1.py, s5_p1_an.py, s5_run2.py, an_s5.py, pairs.py {O3,O1}, an_pairs.py, grade_cells.py, s2_s19.py, an_generic.py, s11a.py, an_s11a.py, s12.py, s12b.py, headline.py, agg.py, sanity.py.
