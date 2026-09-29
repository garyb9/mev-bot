# Data sources, news/alt-data and classification-model trading: research for SPEC-0008

Date: 2026-09-29. All "seen" dates are 2026-09-29 unless stated. Method: public pages only (pricing, docs, ToS, papers), no signups, no keys. Where a WebFetch summary or a third-party page was the only source, the claim is tagged **(secondary)**; anything not confirmed is **UNVERIFIED**. Vendor prices change; re-check before buying.

## Executive summary

1. The "very many tickers for ~20 USD/month" service is most likely **EODHD "EOD Historical Data" at 19.99 USD/mo** (global EOD, 100k calls/day; https://eodhd.com/pricing). Not confirmed by the owner (UNVERIFIED). Runners-up: Tiingo Power 30 USD (110k symbols), Massive (ex-Polygon) Starter 29 USD (15-min delayed, all US tickers), Twelve Data Grow 29 USD, FMP Starter ~29 USD (secondary).
2. None of the ~20 USD tiers gives real-time consolidated US equities, options history, or crypto tick data. They are EOD/delayed, personal-use, and mostly single-user licences.
3. **Cheapest setup that meets ">= 2 years daily US options positioning history"**: EODHD US Options add-on at **29.99 USD/mo** (6,600+ US underlyings, EOD since Q4 2023, so ~2.9 years today, with OI, volume, bid/ask, IV, five greeks; https://eodhd.com/lp/us-stock-options-api). Buy one month, bulk-pull only the O9 underlyings, and stop. Personal-use licence; the storage/automation clause was not read (UNVERIFIED).
4. Real-time US equity quotes for O10 Part A: nothing at ~20 USD. Cheapest real-time is Massive Stocks Advanced 199 USD/mo, or Alpaca free (IEX-only, 30-symbol WS) as a partial proxy. Recommendation: use Alpaca free or delayed Massive Starter for the pre-check, and buy nothing real-time until an O10 A/D pre-check justifies it.
5. Crypto: we already record HL/Binance/Bybit/Deribit ourselves. Tardis costs 350 to 3,000 USD/mo and its ToS bans ML-training use without a separate agreement. Skip it. Deribit's free public history API (all option trades) plus Binance Vision (free) cover backfill.
6. Do not build on Yahoo/CBOE web endpoints beyond the existing R-11 decision: CBOE's delayed-quotes page states automated download is prohibited and IPs are blocked; Yahoo's ToS forbids automated access (secondary).
7. News, minimal stack: **SEC EDGAR (free, official, second-resolution acceptance times) + GDELT (free, 15-min) + our own forward-recorded RSS/exchange announcement timestamps**. Add Marketaux free only as a sentiment cross-check. Skip NewsAPI (449 USD/mo for production), CryptoPanic (paid only, 199 USD/mo), X (pay-per-use), Reddit (approval queue, no ML training).
8. **"jev/laya" is identified**: TypeSafe AI's closed "Jev" and the open (Apache-2.0) "Laya" are *typed-decision text classifiers* (encoder models answering choice/score questions with probabilities), released Sept 2026. Community `jev-trade` repos run Jev on Hyperliquid. No out-of-sample after-cost evidence exists. Latency (7 to 40 ms Laya, 70 to 500 ms Jev) is 100x to 1000x above our hot-path budget, so they are at most an off-hot-path regime/news tagger.
9. Literature: ML classifiers rarely survive out-of-sample after costs in liquid markets; deep LOB models fail to generalise across datasets and small-tick assets; LLM-news alpha decays and shrinks with cost assumptions. What does fit us: **meta-labelling on episodes already found by arb studies** and **GBT on OFI/microstructure features vs a linear baseline**, both tested with purged walk-forward and CIs.
10. Five study proposals at the end (M-1..M-5), ranked; M-1/M-2 are T1-adjacent and cost nothing in data; the options/news ones are T3 and gated as in GOAL.md section 2.1.

## Fit to GOAL.md (ranked by how directly each recommendation moves the goal)

Goal (docs/GOAL.md s1): fast Hyperliquid arb/MEV first; options-informed directional second; every live strategy needs a net-positive study.

| Rank | Recommendation | GOAL.md / SPEC-0008 link | Verdict |
|---|---|---|---|
| 1 | Meta-labelling filter on arb episodes (M-1) | s2 item 1/3 (arb, CEX-lead), s3 "Evidence", O5/O1/P-4 episodes, s13.4 latency grid | Directly on-goal; data already recorded; runs off hot path (model exported to a threshold/table, not inference in the loop) |
| 2 | OFI/microstructure GBT vs linear baseline for CEX-to-HL lead (M-2) | s2 item 3, O5, O12 | On-goal; only worth building if it beats the linear OFI baseline net of costs |
| 3 | EODHD options month (29.99 USD) + Deribit history | s2 item 8, O9a, V-13, T3-data ("start the clock") | On-goal for the secondary family; the only cheap route to the 2-year target |
| 4 | SEC EDGAR + GDELT forward collection; news event study (M-4) | s2 item 6 (weekend/overnight info on HIP-3 stock perps), O10 B/E | Plausibly on-goal; T3, so wait for gate (R-10 done, >= 3 T1 studies) |
| 5 | Options-feature classifier (M-3) | O9a/O9b | On-goal but statistically thin (<= ~700 days x few tickers); T3 |
| 6 | Massive/Alpaca for equities quotes (V-11) | O10 Part A (R-13 needs V-11) | On-goal; do the pre-check with free feeds first |
| Off-goal / flag | Jev/Laya as a trading model; DeepLOB-style deep nets; X/Reddit/Telegram sentiment; CoinGecko/CoinAPI/Kaiko; Tardis; Databento Standard (199 USD); HYPE options (Derive RFQ, Rysk) | GOAL s6 non-goals: "building a strategy because it is interesting"; s5 latency | Interesting, not goal-serving now. HYPE options only matter for O23 (T2/T3) and have no public liquid chain that I could verify |

---

# PART 1 - Market data sources (stocks, options, crypto)

## 1.1 What the spec already establishes (not repeated)
SPEC-0008 s9.1/s15/s17: Yahoo options endpoint verified (V-9), 15-min delayed, ToS risk; finsnap keeps only 30 days of volume/OI (no IV/bid/ask); Deribit public API verified (V-10); `equities` real-time provider is open (V-11); options history purchase is open (V-13, owner decision, question #6); data licences for Tardis/Hydromancer/HL S3 unstated (#27). This part extends V-11 and V-13.

## 1.2 Comparison table: equities and options vendors

Prices are monthly list prices seen 2026-09-29. "Personal" = individual/non-professional licence.

| Vendor / tier | Price | Coverage and delay | History | Options | Rate limit / WS | Crypto | Licence (as read) | Source |
|---|---|---|---|---|---|---|---|---|
| **EODHD EOD Historical Data** | 19.99 | Global EOD, all tickers | 30+ yr US (some to 1972) | No | 100k calls/day, 1,000/min | No | "Personal use"; commercial by separate arrangement | https://eodhd.com/pricing |
| EODHD EOD+Intraday Extended | 29.99 | Adds 1m/5m/1h intraday, delayed live | intraday 2004-2020+ depending on market (vague) | No | same | Yes (forex+crypto) | Personal | same |
| **EODHD US Options add-on** | 29.99 (39.99 first 3 mo) | 6,600+ US stocks, EOD | 2.5+ yr, since Q4 2023 | **Yes: OI, vol, bid/ask, IV, 5 greeks, 42+ fields** | "generous daily limits" | n/a | Data-provider disclaimer only; terms not read (UNVERIFIED) | https://eodhd.com/lp/us-stock-options-api |
| EODHD ALL-IN-ONE | 99.99 | All feeds, real-time WS stocks/forex/crypto, options, news | as above | Yes | 100k/day | Yes | Personal | https://eodhd.com/pricing |
| **Massive (ex-Polygon) Stocks Starter** | 29 | All US tickers, **15-min delayed**, WS | 5 yr | n/a | unlimited calls | via separate Currencies plan (UNVERIFIED price) | "Personal, non-business use"; redistribution needs Business | https://massive.com/pricing |
| Massive Stocks Developer / Advanced | 79 / 199 | 15-min / **real-time** | 10 / 20+ yr | n/a | unlimited, WS | - | Personal (non-pro) | same |
| Massive Options Basic / Starter | 0 / 29 | EOD (5 calls/min) / 15-min delayed, live Greeks+IV snapshots, WS | 2 yr | Yes, all US options tickers; **history is contract-level, historical OI/IV snapshots not confirmed (UNVERIFIED)** | unlimited paid | - | "Individual use only" | https://massive.com/pricing?product=options |
| Massive Options Developer / Advanced | 79 / 199 | +trades / real-time + quotes | 4 / 5+ yr | Yes | - | - | Individual only | same |
| Twelve Data Basic / Grow / Pro | 0 / 29 / 99 | US (Basic); 20+ markets, US real-time (Grow); options only from Pro | not read | Pro only | 8 / 55 / 610 credits/min; WS trial on Grow | Yes | Basic: internal non-display, no commercial; Grow: internal display | https://twelvedata.com/pricing |
| Tiingo Starter / Power | 0 / 30 (50 commercial) | 110,125 securities; IEX real-time not confirmed | 30+ yr prices | No | 50/h vs 10k/h; 40 GB/mo | Not confirmed | "Internal use = own personal use, may not display or share"; redistribution needs sales | https://www.tiingo.com/about/pricing |
| FMP Starter | ~29 (secondary; page 403) | US, 300 calls/min | 5 yr | UNVERIFIED | 20 GB bandwidth | UNVERIFIED | UNVERIFIED | https://site.financialmodelingprep.com/pricing-plans (blocked) and secondary search results |
| Alpha Vantage Premium | 49.99 to 249.99 | Realtime/15-min US, "options via Alpha X Terminal" | not read | Terminal only | 75 to 1,200 req/min | Yes | not read | https://www.alphavantage.co/premium/ |
| Finnhub | free ~60/min; paid from 49.99 (secondary) | Real-time US quotes on free (secondary) | free candles 1 yr (secondary) | paid (UNVERIFIED) | WS 50 symbols free (secondary, conflicting) | Yes | free = non-commercial | https://finnhub.io/pricing (page gave no numbers), secondary |
| Alpaca Basic / Algo Trader Plus | 0 / 99 | Free: IEX-only, 15-min delayed via API, real-time WS 30 symbols; Plus: all exchanges | 7+ yr | Free: **indicative**; Plus: real-time OPRA | 200/min free | Yes | not read | https://alpaca.markets/data |
| Tradier | free with brokerage (UNVERIFIED) | - | - | chains+greeks (UNVERIFIED) | - | - | pages 404'd | not verified |
| IEX Cloud | shut down 2024-08-31 | - | - | - | - | - | - | https://www.alphavantage.co/iexcloud_shutdown_analysis_and_migration/ |
| Databento Standard | 199 (usage-based also) | US equities + OPRA, live+hist | L1 12 mo (Std), 16+ yr on Plus (1,750/mo); OPRA from 2013 | Yes (CBBO-1m, OHLCV, statistics/OI, definitions) | - | not listed | Personal instant approval; external distribution on Plus | https://databento.com/pricing ; https://databento.com/blog/opra-data |
| ThetaData Options Value / Standard / Pro | 40 / 80 / 160 | 100% US options+stocks, real-time | 6 / 10 / 14 yr | Yes; EOD greeks/IV endpoints exist | 2/4/8 concurrent | - | "Individual use; commercial = business tier" | https://www.thetadata.net/pricing ; docs.thetadata.us |
| ORATS Delayed API | 199 | 15-min delayed | EOD to 2007, 5,000+ symbols | Yes, full | 20k req | - | data agreements | https://orats.com/data-api |
| HistoricalData.net options | 199 (1 yr) / 590 (2002-now full, one-time) / 79 per mo daily | EOD chains, US stocks/ETFs/indices | Feb 2002+ | **34 columns: bid/ask, vol, greeks, IV** | files | - | not stated on page (UNVERIFIED) | https://historicaldata.net/options.html |
| Cboe DataShop Option EOD Summary | quote only | OPRA EOD + 15:45 snapshot | Jan 2012+ | OHLC, vol, OI; IV/greeks paid extra | files | - | index data needs CGI licence (from 1k/mo) | https://datashop.cboe.com/option-eod-summary |
| Nasdaq Data Link | free tiers + premium | varies | varies | Greeks/IV "Powered by Nasdaq Basic" dataset | 50k calls/day free | - | per-dataset | https://help.data.nasdaq.com/article/540-what-is-premium-data |

Not verified at all (pages blocked or no primary source): FMP exact tiers and licence, Finnhub tier numbers, Tradier, Massive's separate "Market Data Terms of Service", Twelve Data history depth, Massive crypto plan.

## 1.3 Free/official sources
| Source | What | Limit / licence | Verdict |
|---|---|---|---|
| Cboe delayed quotes (https://www.cboe.com/delayed_quotes/) | Free 15-min delayed option chains | Page text: automatic extraction "strictly prohibited", IPs blocked (via search excerpt, secondary) | Not for automation; ok to eyeball |
| Yahoo Finance endpoints | Chains, quotes | ToS forbids automated access (secondary); already flagged in SPEC s15/s17 | Keep R-11 as owner-accepted risk; treat as best-effort |
| Nasdaq.com option chain page | Delayed chain | ToS not read (UNVERIFIED) | Skip |
| SEC EDGAR APIs | Filings, XBRL | Free, no auth, sub-second update; fair-access rules (User-Agent) https://www.sec.gov/search-filings/edgar-application-programming-interfaces | Use |
| Binance Vision (data.binance.vision) | Free klines/trades/funding archive | "Binance Vision Dataset Terms v1.0" dated 26 Aug 2026; text not retrieved; a public GitHub issue asks whether personal backtesting is allowed (https://github.com/binance/binance-public-data/issues/502) | Use for personal research only; terms text UNVERIFIED |
| Deribit public history API | All historical option/future trades, free, keyless (community scrapers: BTC options ~10 GB, 1-2 h) | Deribit ToS not read | Best free crypto-options backfill (trades, not chain snapshots) |

## 1.4 Crypto sources
| Vendor | Price | Notes | Source |
|---|---|---|---|
| CoinGecko API | Demo free (10k credits, 100/min); Basic 35 (29 annual), 300/min, 2 yr history; Analyst 129, 10 yr; Pro 999 | OHLC/market data, not tick/LOB; commercial licence tied to paid | https://www.coingecko.com/en/api/pricing |
| CoinAPI | Startup 79/mo, 1,000 REST credits/day, history 16 yr, WS trades/OHLCV (secondary) | credit per 100 datapoints; flat files 250 per 1,000 requests | https://www.coinapi.io/products/market-data-api/pricing (search excerpt) |
| Tardis.dev | Options plan Academic 350, Solo 700, Pro 1,000, Business 3,000; Perps Solo 700; "4 years with yearly billing" | ToS: internal/research/personal use OK; may develop "task-specific quantitative or statistical models"; **using data to train/validate ML models prohibited without separate agreement**; free CSVs for first day of each month | https://tardis.dev/#pricing ; https://docs.tardis.dev/legal/terms-of-service |
| Kaiko | enterprise, ~12k to 55k USD/yr (secondary) | no free tier | https://www.kaiko.com/about-kaiko/pricing-and-contracts |
| CoinDesk Data (ex-CryptoCompare) | commercial from ~80/mo (secondary); free tier retired May 2026 (secondary) | | https://developers.cryptocompare.com/pricing |
| Deribit / Binance / Bybit / HL | own recorder; free | already in SPEC | - |

Crypto options on HYPE / HyperEVM: Derive lists RFQ-based HYPE options with HYPE collateral bridged from HyperEVM (https://insights.derive.xyz/hype-on-derive/); Rysk runs covered-call vaults on HyperEVM hedged on HyperCore (https://x.com/ryskfinance/status/1928442017471701265). I found no public, liquid, keyless chain feed for HYPE options; whether Deribit lists HYPE options is UNVERIFIED. Relevance: only O23 (HYPE realized-vs-implied). Off-goal until then.

## 1.5 Recommendation (solo owner, research + bot)

Principle: the recorder is the primary data source; buy only what cannot be recorded forward (history) or is licence-restricted.

| Need | Buy? | What | Cost |
|---|---|---|---|
| US equities+ETF options positioning, >= 2 yr daily (O9a, V-13) | **Yes, one month, bulk pull** | EODHD Options add-on (2.9 yr, OI+IV+greeks+bid/ask, 6,600 underlyings) | 29.99 (39.99 first 3 mo). Verify the ToS on storage before pulling |
| Longer options history, if O9a passes and needs 5-10 yr | Later, one-off | HistoricalData.net 1-yr 199 or full 590 one-time; or ThetaData Value 40/mo (6 yr) | 199 to 590 |
| Stock daily/1m bars for O10 pre-check, O11 proxy | Optional | EODHD EOD 19.99 (daily, 30+ yr) or Massive Stocks Starter 29 (5 yr minute bars, 15-min delayed) | 20 to 29/mo, cancel after pull |
| Real-time equity quotes (V-11, R-13, O10 Part A) | **Not yet** | Start with Alpaca free IEX WS (30 symbols) for the pre-check; if O10 A/D shows edge, Massive Advanced 199/mo or Alpaca Plus 99 | 0 now |
| Crypto | **No** | Recorder + Binance Vision + Deribit history API | 0 |
| Crypto news/sentiment | No | see Part 2 | 0 |

**Cheapest full setup:** EODHD Options 29.99 (single month) + EODHD EOD 19.99 (single month) + free Alpaca + free crypto = **~50 USD one-off**, then 0. Re-subscribe only for forward equity/options collection if R-11 (Yahoo) proves unreliable, at which point ThetaData Value (40/mo, real-time snapshots) is the cheapest legitimate live-chain source I found. An all-in-one ~20 USD service covering everything does not exist.

**Not available at the ~20 USD price:** real-time consolidated quotes; any options history (EODHD's option history needs the separate 29.99 add-on); crypto tick/LOB data; intraday options snapshots (only EOD, or 15-min delayed live snapshots on Massive Options Starter); commercial/redistribution rights; a documented right to store data indefinitely for use in an automated trading system (none of the personal tiers I read grant this explicitly).

**Licence reading, blunt:** every retail tier is "personal / internal use, no redistribution". Massive's Individuals ToS limits use to "personal, non-commercial, and non-business purposes", bans sharing API keys, and its classification test is use-based (having an LLC does not itself make you professional; unclear for a bot trading own capital; Massive says to contact them) (https://massive.com/knowledge-base/article/what-are-pro-and-non-pro-classifications-for-massives-stock-data ; https://massive.com/legal/individuals-terms-of-service). None of the ToS I read addresses automated trading explicitly; storage is unaddressed in Massive's Individuals ToS but incorporates a separate Market Data ToS (not read: UNVERIFIED). Our use (private research + own-account bot, no redistribution) is the intended personal use, but resolve any ambiguity by email to the vendor before relying on it for the live bot. Keep bought data under `research/data/` (SPEC #27).

---

# PART 2 - News and alternative data

| Source | Cost | Timestamps | History | Sentiment scores | Licence / ToS | Verdict |
|---|---|---|---|---|---|---|
| **SEC EDGAR** (8-K, Form 4, 13F, RSS/JSON) | free | SEC acceptance time (seconds); submissions API updates in <1 s | full | none (do our own) | free, fair-access (User-Agent, rate limits) https://www.sec.gov/search-filings/edgar-application-programming-interfaces | **Use**; cleanest timestamps |
| **GDELT** (Events/GKG/DOC) | free, "100% free and open" https://www.gdeltproject.org/data.html | 15-min files; post-2013 keyed by **DATEADDED** (when reported), i.e. ingest-ish, not publish time | Events 1979+, GKG 2013+ | tone/emotion fields in GKG | open | **Use** for macro/crypto attention series; 15-min resolution is too coarse for HL lead-lag but fine for hours-scale (O10 B/E) |
| Exchange RSS/announcement pages (Binance, Coinbase, Hyperliquid X/Discord, SEC press) | free | publish time in feed; we timestamp on receipt (recv_ts) | forward only | none | per-site ToS (UNVERIFIED) | Forward-record; store both `published` and `recv_ts` |
| Marketaux | free 100 req/day, 3 articles/request; paid higher (price UNVERIFIED) | article time + entity sentiment | free plan limited (UNVERIFIED) | **yes**, entity-level | free tier terms UNVERIFIED (secondary: https://www.marketaux.com/documentation) | Optional cross-check only |
| Alpha Vantage NEWS_SENTIMENT | free 25 req/day; premium from 49.99 | `time_published`; historical depth UNVERIFIED | UNVERIFIED | yes (ticker + topic) | vendor licence not read | Skip unless already on Premium |
| Finnhub company news | free tier includes company news (secondary) | timestamp UNVERIFIED | ~1 yr free (secondary) | premium | free = non-commercial | Possible free complement |
| NewsAPI.org | dev free (100/day, 24 h delay, ~1 month, dev/test only); production 449/mo | publish time; delayed | 1 month | none | dev plan forbids staging/production incl. internal | **Skip** (secondary: https://newsapi.org/pricing) |
| CryptoPanic | free Developer plan discontinued early 2026 (secondary); Growth 199/mo, weekly 50 | posts with votes | limited | community votes | check live page | **Skip** (secondary; primary page gave no content) |
| Benzinga / Massive news | Massive news is bundled with Stocks tiers (UNVERIFIED); Benzinga direct is institutional | - | - | yes (Benzinga) | - | Not verified; skip |
| X (Twitter) API | pay-per-use: ~0.005 per post read, 0.015 per post created; Free/Basic/Pro retired (secondary) https://docs.x.com/x-api/getting-started/pricing | exact | search recent only | none | dev ToS; read-cost math is prohibitive for streams | Skip |
| Reddit Data API | free non-commercial <= 100 QPM per OAuth client; every new app now needs manual approval; no ML training on content without separate licence; commercial ~12k/mo (all secondary, e.g. https://prowlo.com/blog/reddit-data-api) | exact | limited by API | none | as stated | Skip (ML-training clause conflicts with classifier studies) |
| Telegram public channels | free via API (MTProto) | message time exact | full for public channels | none | Telegram API ToS not read (UNVERIFIED); needs a personal account/keys | Feasible technically; not verified legally; skip for now |

Timestamp discipline for event studies: always store `published_at` (source), `first_seen_at` (our clock), and never join on ingest time for a lead/lag claim; only sources with `published_at` set by the originator (EDGAR acceptance datetime, exchange feeds) can support sub-minute claims. Aggregator/GDELT timestamps are ingest-side and bias toward false lead of price over news.

**Minimal stack (all free):** (1) EDGAR 8-K/Form 4/Form 8-K item feed for the O9/O10 underlyings; (2) GDELT GKG filtered by ticker/crypto keywords for the hours-scale attention series; (3) forward RSS/announcement scraper with dual timestamps for crypto exchange listings/delistings and HIP-3 deployer announcements; (4) FinBERT (ProsusAI/finbert, Apache-2.0, https://huggingface.co/ProsusAI/finbert ; https://arxiv.org/abs/1908.10063) run offline to score headline text ourselves, so sentiment provenance is under our control. Fit: serves O10 B/E (weekend/overnight information) and O6; T3, not before the gate.

---

# PART 3 - Trading with classification models

## 3.1 "jev/laya"
Identified with sources (all Sept 2026, so these pages are days to weeks old; treat as a young, hype-adjacent ecosystem):
- **Jev** (TypeSafe AI, San Francisco): a closed, hosted "System One" typed-decision model. Encoder-only with decision heads; the caller sends text plus typed questions (choice/score/"noul") and gets probabilities in one pass. Priced 0.042 USD per million input tokens; latency 70 to 500 ms (independently measured 264 to 276 ms). No paper, weights, or technical report. https://akmaier.substack.com/p/laya-jev-and-the-return-of-the-discriminative (2026-09-28) ; https://dev.to/jamilxt/jev-vs-laya-the-same-ai-idea-one-closed-and-one-open-3c6e (2026-09-22).
- **Laya** (single developer / Convai Innovations, per differing sources): the open (Apache-2.0) equivalent on ModernBERT-large (421M) and mmBERT-base (322M); 33 to 40 ms single query, ~7 ms/question in batches on a Tesla T4; AG News accuracy 0.950, post-hoc-calibrated ECE 0.081; weak on wide label sets (Banking77 0.425 vs Jev 0.870); overconfident on unfamiliar scripts. Same sources.
- **Trading uses**: `jev-trade` / `jev-hyperliquid` clones run Jev on Hyperliquid perps (BUY/SELL/HOLD from ~6 typed questions per market; "Jev judges, code executes") in dry-run/live modes (https://github.com/devsoniclk/jev-hyperliquid ; https://github.com/pozivo/jev-trade), based on Jarrod Watts' jev-trader (MIT); `laya-trader` paper-trades Laya MLX on Binance crypto + S&P 500 with walk-forward tests (https://github.com/antonellof/laya-trader). A survey of Jev finance projects found **no live PnL, drawdowns, Sharpe or after-cost results, only dry-run/paper** (https://gist.github.com/drillan/6916b16e8ea31a8ec36c8f59d6483150, surveyed 2026-09-20).
- Assessment for this project: it is a fast-ish *text classifier* prompted with a text description of market state. Nothing published shows edge. Latency (>= 7 ms batched, 30 to 500 ms per call) is 70x to 5,000x our 100 to 250 us tick-to-order target, so it can never be on the hot path (GOAL s5). It is worth at most a benchmark arm in an off-path study (M-5). I did not find any primary evidence beyond these sources; whether "jev/laya" is what the owner meant is unconfirmed but the spelling and trading context match.

## 3.2 Literature and practice: what works and what does not

| Approach | Key sources | Evidence out-of-sample after costs | Fit here |
|---|---|---|---|
| Triple-barrier labels + meta-labelling (Lopez de Prado, AFML 2018) | Hudson & Thames, "Does Meta-Labeling Add to Signal Efficacy?" (E-mini futures) https://hudsonthames.org/does-meta-labeling-add-to-signal-efficacy-triple-barrier-method/ | Improves precision/Sharpe of an existing rule in their study (vendor-affiliated, single dataset); meta-labelling is a *filter*, it cannot create edge if the primary signal has none | **Best fit**: apply to episodes from O5/O1/O12 that already exist; secondary model only decides take/skip |
| GBT / DNN / RF on returns (daily stat-arb) | Krauss, Do, Huck 2017 (S&P 500, 1992-2015): ~0.45%/day before costs, ensembles; authors note profits declining in recent years https://www.sciencedirect.com/science/article/abs/pii/S0377221716308657 | Edge shrank/vanished over time and with costs; daily, not our timescale | Warning on non-stationarity |
| Order flow imbalance (linear) | Cont, Kukanov, Stoikov 2014: linear relation between best-level OFI and price change, slope ~1/depth, stable across 50 stocks https://arxiv.org/abs/1011.6402 | Contemporaneous impact, not a tradeable forecast by itself, but the right *baseline feature* | **Fit**: baseline every ML model must beat |
| GBT on microstructure features | LOB comparisons on Bybit BTC/USDT: "Better Inputs Matter More Than Stacking Another Hidden Layer" benchmarks logistic regression, XGBoost, DeepLOB, Conv1D+LSTM https://arxiv.org/abs/2506.05764 | Reports that better inputs matter more than depth; general finding in this literature is that statistically significant edges often vanish after costs | Fit: cheap, CPU, inspectable |
| Deep LOB (DeepLOB) | Zhang, Zohren, Roberts 2019, LSE data, stable out-of-sample accuracy https://arxiv.org/abs/1808.03668 ; Benchmark: LOBCAST, all models show significant performance drops on unseen LOBSTER data; FI-2010 too easy https://arxiv.org/html/2308.01915 ; "Deep LOB forecasting: a microstructural guide": MCC 0.29 for large-tick vs 0.11 small-tick stocks at 10 updates, near-random at 100-update horizon for small-tick, and high forecast metrics do not imply tradable signals (probability of executing correct transactions 0.03 to 0.06) https://arxiv.org/html/2403.09267v1 | Weak; generalisation and transaction-level profitability are the failure points | Not now: needs L2 history we only have forward, GPU-heavy, latency-unfriendly; HL perps are small-tick |
| FinBERT / LLM news classifiers | Araci 2019 FinBERT (Apache-2.0) https://arxiv.org/abs/1908.10063 ; Lopez-Lira & Tang, GPT scores predict next-day returns, returns decline as LLM adoption rises, cumulative return falls from 350% at 10 bp to 50% at 25 bp per trade https://arxiv.org/abs/2304.07619 | Real but small, concentrated in small caps and negative news, decays; very sensitive to costs | Plausible for hours-scale studies (M-4), not for the hot path |
| Backtest-overfitting controls | Bailey & Lopez de Prado, Deflated Sharpe Ratio https://papers.ssrn.com/sol3/papers.cfm?abstract_id=2460551 ; Probability of Backtest Overfitting https://papers.ssrn.com/sol3/papers.cfm?abstract_id=2326253 | - | Adopt as pass/fail inputs (count trials) |
| Typed-decision models (Jev/Laya) | see 3.1 | none | Off-path benchmark arm only |

## 3.3 Typical failure modes (and the guard this repo already has or needs)

| Failure | Mechanism | Guard |
|---|---|---|
| Label leakage | Overlapping triple-barrier windows or features computed with future bars; reveal-lag ignored | Purged/embargoed walk-forward (embargo >= label horizon); apply reveal lag at feature time (as B-9 already does for episodes) |
| Look-ahead in options/news | Options OI is published next morning; news joined on ingest time | Use as-of timestamps: OI known from next session open, `published_at` vs `first_seen_at` |
| Non-stationarity | Regime/tick-size/fee changes (HIP-3 fee scales, funding regime); LOBCAST-type collapse on new data | Rolling refit + frozen-model forward test; report per-month stability |
| Overfitting / multiple testing | Many features x horizons x thresholds | Pre-register (SPEC s13.13 registry), count trials, Deflated Sharpe, hold-out never touched until the final run |
| Costs and latency | Signals live inside spread+fee; fills at latency L not at the signal price | Use SPEC s13.2 cost model and s13.4 latency grid; label = net PnL at latency L, not price direction |
| Class imbalance / accuracy illusions | Accuracy 55% with no economic value | Score on net PnL/episode and pT-style "probability of executing correct trades", not F1 |
| Calibration | Overconfident probabilities (Laya's ECE, other LLM classifiers) | Reliability diagram; thresholds set on train folds only; report Brier |
| Data licence | Vendor bans ML training (Tardis, Reddit) | Use own recorded data; check the clause first |

## 3.4 Project-fit filter
Constraints: latency-first (model must compile to a threshold or lookup consumed on the hot path, inference off-path); evidence-before-strategy (any classifier is a *study*, never strategy code first); pre-registered; out-of-sample with confidence intervals; no strategy without a passing study. Approaches that fit: meta-labelling, GBT on OFI features, options/news features at hours-scale. Approaches that do not: deep LOB nets, LLM-in-the-loop decisions (Jev/Laya) on the tick path.

## 3.5 Study proposals (add to SPEC-0008 as new studies; ranked by expected value / cost)

Common protocol for all: pre-registered feature list and hyperparameter grid in the hypothesis registry (s13.13); baseline model always included (linear/logistic or the base rule with no filter); purged walk-forward with embargo; block-bootstrap CIs on net PnL/episode and APR at the headline capital ($25k) and the s13.4 latency grid; PASS = 95% CI lower bound > 0 net of costs AND uplift vs baseline CI > 0 AND >= 30 out-of-sample signals per variant; MARGINAL/FAIL per the s13.6 APR floor/target thresholds; Deflated Sharpe reported with the true trial count.

**M-1 - Meta-labelled take/skip filter on arb episodes (rank 1; serves GOAL s2 items 1/3, O5/O1/O12 after P-4).**
- Hypothesis: among episodes that pass the base rule at latency L, a secondary classifier predicts which are net-positive, raising net PnL/episode and cutting the loss tail versus taking all.
- Features (known at detection): net edge bps, spread and depth at both venues, time since last HL oracle/mark update, Binance-lead move size, recent volatility, hour, funding-settlement proximity.
- Label: 1 if episode net PnL after fees/slippage at latency L is > 0 (fixed horizon = episode life, triple-barrier style: profit target, stop, time).
- Split: chronological purged walk-forward, >= 14 days recorded data (gate G1), last 30% untouched hold-out. Models: logistic (baseline), GBT.
- Cost: reuse P-3 model. Data cost 0; effort S/M.
- Pass: hold-out net PnL/episode of filtered set exceeds unfiltered by CI lower bound > 0 with >= 30 signals; filter must compile to a <= 20-parameter rule (hot-path-safe).

**M-2 - Microstructure GBT vs linear OFI for CEX-to-HL short-horizon lead (rank 2; O5/O12, T1).**
- Hypothesis: a GBT on multi-level OFI, trade imbalance, Binance-minus-HL mid basis, and depth ratios predicts the HL mid move over 100 ms to 5 s beyond the linear OFI baseline (Cont et al.).
- Label: ternary sign of HL mid change beyond (half-spread + fee) at horizon h in {0.25, 1, 5} s; trade only when predicted class prob > threshold.
- Split: walk-forward by day, embargo = h. Cost: taker fee tier + spread + latency grid.
- Pass: net APR CI lower bound > floor at latency >= measured p50 tick-to-order + RTT, uplift over linear OFI > 0; otherwise record FAIL (a useful negative result). Effort M; no data cost.

**M-3 - Options-positioning features -> daily direction of HIP-3 stock perps / bluechips (rank 3; O9a then O9b, T3).**
- Hypothesis: skew, put/call volume+OI ratio, OI-wall distance, and IV minus realized vol (IV from EODHD; Deribit `mark_iv` for BTC/ETH) predict next-1d/3d direction beyond a trailing-return baseline.
- Label: triple barrier on daily close of the real stock (proxy) with vol-scaled barriers; then O9b forward on HL perps.
- Split: purged walk-forward on the EODHD window (~2.9 yr; ~700 days) with the last 6 months as hold-out; underlying-clustered bootstrap.
- Cost: HL stock-perp fees + funding + open-to-open gap in off-hours. Data cost 29.99 once. Power warning: few days x tickers, so pre-register <= 6 features and 1 label; expect MARGINAL/inconclusive unless effects are large.
- Pass: hold-out net APR CI lower bound > floor, and IC CI > 0 across >= 60% of tickers.

**M-4 - News/filing event study with classifier tagging (rank 4; O10 B/E, O6; T3).**
- Hypothesis: for HIP-3 stock perps, 8-K/news events arriving while the stock is closed are priced by the perp within X minutes, and the sign from FinBERT (or a fine-tuned classifier) predicts the perp's 5 to 60 min drift beyond the initial jump (i.e., lead/lag exploitable).
- Data: EDGAR acceptance times (free), forward RSS with dual timestamps; HL perp prices recorded. Label: sign/size of perp return over [t+1 min, t+60 min] net of costs.
- Split: forward-only, chronological; need >= 60 trading days and >= 30 events/variant. Cost: HL fees+spread.
- Pass: event-study CAR CI excludes 0 net of costs at latency 1 s and 30 s. Baseline: keyword/8-K-item-type rules without ML.

**M-5 - Typed-classifier regime gate benchmark (Jev/Laya) as an off-path arm (rank 5; low EV, cheap; optional, T3).**
- Hypothesis: a Laya (open, local) regime tag (trend/chop/high-vol) from a compact text description of state adds value over a plain vol/volume rule as a gate on M-1/M-2 or on the O11 Bollinger reversion.
- Label: regime-conditional net PnL of the base strategy; compare with GBT/logistic gate and no gate. Run on recorded data offline only; cost local GPU/CPU; Jev excluded (closed, per-call cost, no reproducibility).
- Pass: uplift CI > 0 over the simplest non-ML gate. If it fails (the likely outcome), close it and delete the idea; expected to fail per the literature above.

Ranking rationale: M-1 and M-2 reuse data we already record, target the primary (arb) family and the T1 tier, and their negative results are cheap and informative. M-3 costs 30 USD and is statistically thin. M-4 needs forward calendar time. M-5 has no evidence in its favour.

---

# Sources (all seen 2026-09-29)

Vendors and pricing
- https://massive.com/pricing ; https://massive.com/pricing?product=options ; https://massive.com/blog/polygon-is-now-massive (rebrand effective 2025-10-30, secondary via search) ; https://massive.com/legal/individuals-terms-of-service ; https://massive.com/knowledge-base/article/what-are-pro-and-non-pro-classifications-for-massives-stock-data ; https://massive.com/knowledge-base/article/what-are-pro-and-non-pro-classifications-for-massives-options-date
- https://eodhd.com/pricing ; https://eodhd.com/lp/us-stock-options-api
- https://twelvedata.com/pricing ; https://www.tiingo.com/about/pricing ; https://www.alphavantage.co/premium/ ; https://finnhub.io/pricing (no numbers returned) ; https://site.financialmodelingprep.com/pricing-plans (403)
- https://alpaca.markets/data ; https://databento.com/pricing ; https://databento.com/blog/opra-data ; https://www.thetadata.net/pricing ; https://orats.com/data-api ; https://historicaldata.net/options.html ; https://datashop.cboe.com/option-eod-summary ; https://www.cboe.com/delayed_quotes/ ; https://help.data.nasdaq.com/article/540-what-is-premium-data
- https://www.alphavantage.co/iexcloud_shutdown_analysis_and_migration/
- Crypto: https://www.coingecko.com/en/api/pricing ; https://www.coinapi.io/products/market-data-api/pricing ; https://tardis.dev/#pricing ; https://docs.tardis.dev/legal/terms-of-service ; https://www.kaiko.com/about-kaiko/pricing-and-contracts ; https://developers.cryptocompare.com/pricing ; https://github.com/binance/binance-public-data/issues/502 ; https://insights.derive.xyz/hype-on-derive/ ; https://x.com/ryskfinance/status/1928442017471701265 ; https://github.com/RiveChen/deribit-historical-data ; https://insights.deribit.com/exchange-updates/celebrating-our-tardis-dev-partnership-get-free-historical-data/

News / alt data
- https://www.sec.gov/search-filings/edgar-application-programming-interfaces ; https://www.gdeltproject.org/data.html ; https://www.marketaux.com/documentation ; https://newsapi.org/pricing ; https://cryptopanic.com/developers/api/plans ; https://docs.x.com/x-api/getting-started/pricing ; https://prowlo.com/blog/reddit-data-api

Jev/Laya
- https://akmaier.substack.com/p/laya-jev-and-the-return-of-the-discriminative ; https://dev.to/jamilxt/jev-vs-laya-the-same-ai-idea-one-closed-and-one-open-3c6e ; https://gist.github.com/drillan/6916b16e8ea31a8ec36c8f59d6483150 ; https://systemonemodels.org/examples/projects/laya-jev-lab/ ; https://github.com/antonellof/laya-trader ; https://github.com/devsoniclk/jev-hyperliquid ; https://github.com/pozivo/jev-trade

Literature
- https://arxiv.org/abs/1808.03668 (DeepLOB) ; https://arxiv.org/html/2308.01915 (LOBCAST) ; https://arxiv.org/html/2403.09267v1 (LOB microstructural guide) ; https://arxiv.org/abs/2506.05764 (crypto LOB inputs vs depth) ; https://arxiv.org/abs/1011.6402 (Cont et al. OFI) ; https://arxiv.org/abs/1908.10063 (FinBERT) ; https://huggingface.co/ProsusAI/finbert ; https://arxiv.org/abs/2304.07619 (Lopez-Lira & Tang) ; https://www.sciencedirect.com/science/article/abs/pii/S0377221716308657 (Krauss et al.) ; https://hudsonthames.org/does-meta-labeling-add-to-signal-efficacy-triple-barrier-method/ ; https://papers.ssrn.com/sol3/papers.cfm?abstract_id=2460551 (Deflated Sharpe) ; https://papers.ssrn.com/sol3/papers.cfm?abstract_id=2326253 (Probability of Backtest Overfitting)

Suggested spec updates (for the owner/other agents, not done here): fill V-11 with "Alpaca free pre-check, Massive Advanced 199 or Alpaca Plus 99 only if O10 A/D passes"; fill V-13 with EODHD Options 29.99 (2.9 yr, OI+IV+greeks) and HistoricalData.net 199/590 for longer history; answer open question #6 and #31 accordingly; record that Tardis ToS prohibits ML-training use (relevant to #27).
