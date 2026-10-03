# References

> External links used by this project, grouped. This file is a link index, not
> a spec: the owning spec is the source of truth. Every URL here is already
> cited somewhere in the repo (specs, `research/`, or the review notes), or is
> the canonical home of a dependency in `Cargo.toml` / `research/pyproject.toml`.

## Hyperliquid official documentation

| Link | Why it matters | Used in |
|---|---|---|
| [API — info endpoint](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/info-endpoint) | Catalog and field shapes for every `/info` request (`metaAndAssetCtxs`, `clearinghouseState`, `openOrders`, `userFills`, `orderStatus`, `userRateLimit`, `fundingHistory`, `candleSnapshot`, `vaultDetails`). | SPEC-0001 §4/§6, SPEC-0002 §9/§11, SPEC-0008 §8/§15 |
| [API — exchange endpoint](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/exchange-endpoint) | Signed action envelope, action catalog, `scheduleCancel` (dead-man) and `expiresAfter` semantics. | SPEC-0002 §4/§7/§12 |
| [API — nonces and API wallets](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/nonces-and-api-wallets) | The nonce window (100 highest per signer) and the agent/API-wallet model. | SPEC-0002 §5 |
| [API — WebSocket subscriptions](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/websocket/subscriptions) | WS channel list (`bbo`, `l2Book`, `trades`, `activeAssetCtx`, `allMids`, `orderUpdates`, `userFills`, `userEvents`); newer `fastAssetCtxs`/`twapStates` channels. | SPEC-0001 §7, SPEC-0002 §11 (H-3), SPEC-0008 §7.2/§15 |
| [API — HIP-3 deployer actions](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/hip-3-deployer-actions) | HIP-3 dex config, deployer fee scale, per-dex backstop liquidator. | SPEC-0008 §13.2/§15, `research/mappings/addresses.toml` |
| [API — perpetuals info](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/info-endpoint/perpetuals) | Perp metadata and `perpDexs` funding fields (`assetToFundingMultiplier`, OI caps). | SPEC-0008 §15, O18 |
| [API — optimizing latency](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/optimizing-latency) | Venue-documented co-located end-to-end latency. | SPEC-0008 V-4, SPEC-0009 |
| [API — priority fees](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/priority-fees) | Write and gossip priority-fee mechanics (fees are burned). | SPEC-0008 V-5 |
| [HyperCore overview](https://hyperliquid.gitbook.io/hyperliquid-docs/hypercore/overview) | What HyperCore is and its documented latency profile. | GOAL §2, SPEC-0008 V-4 |
| [Trading — fees](https://hyperliquid.gitbook.io/hyperliquid-docs/trading/fees) | Fee schedule (perp/spot/HIP-3, aligned-quote multipliers) that the research cost model mirrors. | SPEC-0003 §5, SPEC-0008 §13.2/§15 |
| [Trading — funding](https://hyperliquid.gitbook.io/hyperliquid-docs/trading/funding) | Hourly funding paid at 1/8 of the computed 8 h rate; oracle-notional payment. | SPEC-0008 §13.2/§15, O7, O14 |
| [Trading — portfolio margin](https://hyperliquid.gitbook.io/hyperliquid-docs/trading/portfolio-margin) | Portfolio-margin / cross-margin liquidation behavior. | SPEC-0004, `research/mappings/addresses.toml`, O20 |
| [HyperCore — aligned quote assets](https://hyperliquid.gitbook.io/hyperliquid-docs/hypercore/aligned-quote-assets) | Aligned quote assets carry lower taker fees / better maker rebates — changes O2's triangle threshold. | SPEC-0008 §15, O2 |
| [HyperCore — permissionless spot quote assets](https://hyperliquid.gitbook.io/hyperliquid-docs/hypercore/permissionless-spot-quote-assets) | Quote-token set is no longer just USDC/USDT0. | SPEC-0008 §15, O2 |
| [Historical data](https://hyperliquid.gitbook.io/hyperliquid-docs/historical-data) | The official requester-pays S3 archives (`hyperliquid-archive`, `hl-mainnet-node-data`). | SPEC-0008 §13.11, V-8 |
| [HyperEVM — dual block architecture](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/hyperevm/dual-block-architecture) | Fast (~1 s) and slow (~1 min) EVM blocks; gas limits. | SPEC-0008 §10, V-5 |
| [HyperEVM — interaction timings](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/hyperevm/interaction-timings) | Core↔EVM transfer timing asymmetry. | SPEC-0008 §10, V-5 |
| [HyperEVM — interacting with HyperCore](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/hyperevm/interacting-with-hypercore) | `CoreWriter` (delayed actions) and HyperCore read precompiles. | SPEC-0005, `research/mappings/addresses.toml`, O22 |
| [HyperEVM — HyperCore ⇄ HyperEVM transfers](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/hyperevm/hypercore-less-than-greater-than-hyperevm-transfers) | System-address conventions for moving tokens/HYPE between Core and EVM. | `research/mappings/addresses.toml` |
| [HyperEVM — raw block data](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/hyperevm/raw-hyperevm-block-data) | EVM block data availability for backfill. | SPEC-0008 V-8 |
| [HIP-4 outcome markets](https://hyperliquid.gitbook.io/hyperliquid-docs/hyperliquid-improvement-proposals-hips/hip-4-outcome-markets) | Fully-collateralized binaries settling to the HyperCore mark. | SPEC-0008 §15, O17 |

## Data sources

| Link | Why it matters | Used in |
|---|---|---|
| [Tardis free datasets](https://datasets.tardis.dev/v1) · [tardis.dev](https://tardis.dev/) · [HL data types](https://docs.tardis.dev/historical-data-details/hyperliquid) | First-of-month free day per exchange/type; the paid plan is the main historical `bbo`/`trades`/`book` source. Downloaded by `hlr/backfill/tardis.py` (task B-1). | SPEC-0008 §13.11, B-1 |
| [Hyperliquid `/info`](https://api.hyperliquid.xyz/info) | Public REST for `fundingHistory`, `candleSnapshot` bars, and market metadata backfill. | SPEC-0008 §13.11 |
| [Binance public data](https://github.com/binance/binance-public-data) · [data.binance.vision](https://data.binance.vision/) | Free CEX trades/bars/funding for the cross-venue and lead-lag studies. | SPEC-0008 §13.11, O5, R-8 |
| [Bybit public trades](https://public.bybit.com/) · [Bybit history data](https://www.bybit.com/derivatives/en/history-data) | Second CEX reference feed. | SPEC-0008 §13.11, O5, R-8 |
| [Deribit historical trades API](https://history.deribit.com/api/v2/public/get_last_trades_by_currency_and_time) · [Deribit DVOL](https://www.deribit.com/api/v2/public/get_volatility_index_data) | Crypto option IV/OI history for the options-informed study. | SPEC-0008 §13.11, O9, R-12 |
| [Hydromancer Reservoir](https://hydromancer.xyz/hyperliquid-historical-data) · [Reservoir docs](https://docs.hydromancer.xyz/reservoir) | Requester-pays S3 with 1 s bars, fills (liquidation/ADL flags), and 1-min L2. | SPEC-0008 §13.11 |
| [Alpaca market data](https://docs.alpaca.markets/us/docs/market-data-faq) · [Massive](https://massive.com/stocks) | US equity quotes/bars for the HIP-3 stock-perp studies. | SPEC-0008 §13.11, V-11, O10 |

## Vendor pricing and terms

| Link | Why it matters | Used in |
|---|---|---|
| [EODHD pricing](https://eodhd.com/pricing) · [EODHD US Options API](https://eodhd.com/lp/us-stock-options-api) | Cheapest licensed route to stock bars and ≥ 2-year daily US options positioning history; personal-use licence, storage/automation clause unread. | SPEC-0008 §9.1, V-11/V-13, §17 #6/#31 |
| [Massive pricing](https://massive.com/pricing) · [Massive Individuals ToS](https://massive.com/legal/individuals-terms-of-service) | Paid real-time equity candidate (V-11) and the personal/non-business licence to read before relying on it. | SPEC-0008 V-11, §17 #27 |
| [Alpaca market data](https://alpaca.markets/data) | Free IEX-only real-time WS (30 symbols) for the O10 Part A pre-check; paid tier 99 USD/mo. | SPEC-0008 V-11, O10 Part A |
| [Tardis pricing](https://tardis.dev/#pricing) · [Tardis ToS](https://docs.tardis.dev/legal/terms-of-service) | 350–3,000 USD/mo and the ML-training clause relevant to the classifier studies and data licences. | SPEC-0008 §17 #27, M-1…M-5 |
| [HistoricalData.net options](https://historicaldata.net/options.html) · [ThetaData pricing](https://www.thetadata.net/pricing) | Longer options history (199–590 USD one-off; 40 USD/mo for 6 years). | SPEC-0008 V-13, §17 #6 |
| [Cboe delayed quotes](https://www.cboe.com/delayed_quotes/) | Stated prohibition on automated download (secondary) — why we don't build on it. | SPEC-0008 §17 #31 |

## News and alternative data

| Link | Why it matters | Used in |
|---|---|---|
| [SEC EDGAR APIs](https://www.sec.gov/search-filings/edgar-application-programming-interfaces) | Free, official filings with second-resolution acceptance timestamps; the cleanest sub-minute source. | SPEC-0008 §9.2, O10 B/E, O6 |
| [GDELT data](https://www.gdeltproject.org/data.html) | Free 15-minute macro/attention series; ingest-side timestamps (`DATEADDED`), so hours-scale only. | SPEC-0008 §9.2, O10 B/E |
| [FinBERT (Hugging Face)](https://huggingface.co/ProsusAI/finbert) | Offline headline-sentiment scoring under our own control. | SPEC-0008 §9.2, M-4 |
| [NewsAPI pricing](https://newsapi.org/pricing) · [CryptoPanic plans](https://cryptopanic.com/developers/api/plans) | Cost/ToS reasons they are skipped in the minimal stack. | SPEC-0008 §9.2 |

## Libraries — Rust workspace (`Cargo.toml`)

| Link | Why it matters | Used in |
|---|---|---|
| [Alloy](https://github.com/alloy-rs/alloy) | EIP-712 signing, keccak, address types; the no-`ethers` choice. | SPEC-0000 §4, SPEC-0002 §4, ADR-0001 |
| [rust_decimal](https://github.com/paupino/rust-decimal) | Exact decimal money math on every production path. | GOAL §4.5, SPEC-0000 §4 |
| [criterion.rs](https://github.com/bheisler/criterion.rs) | The decode/sign/engine benchmarks and CI quick mode. | SPEC-0010 §17, SPEC-0002 H-7, ADR-0001 |
| [zstd-rs](https://github.com/gyscos/zstd-rs) | Streaming zstd for recorder segments. | SPEC-0008 §6 |
| [rusqlite](https://github.com/rusqlite/rusqlite) | Bundled SQLite for the bot's event store. | SPEC-0004 §9 |
| [tokio](https://tokio.rs) | Async runtime for all I/O tasks. | SPEC-0000 §4 |
| [tokio-tungstenite](https://github.com/snapview/tokio-tungstenite) | WebSocket transport (market stream + WS `post`). | ADR-0001, SPEC-0002 §8 |
| [reqwest](https://github.com/seanmonstar/reqwest) | REST `/info` / `/exchange` client. | SPEC-0001 §4, SPEC-0002 §8 |
| [rustls](https://github.com/rustls/rustls) | TLS backend. | SPEC-0000 §4 |
| [crossbeam](https://github.com/crossbeam-rs/crossbeam) | Bounded channels into and out of the engine. | SPEC-0010 §5 |
| [smallvec](https://github.com/servo/rust-smallvec) | Reused action/trade buffers, no per-event allocation. | SPEC-0010 §6 |
| [serde](https://serde.rs) | JSON/msgpack derive for wire types and config. | SPEC-0000 §4 |
| [metrics.rs](https://github.com/metrics-rs/metrics) | Prometheus metric facade and exporter. | SPEC-0000 §9, SPEC-0006 §6 |
| [tracing](https://github.com/tokio-rs/tracing) | Structured JSON logging in production. | SPEC-0000 §4 |
| [clap](https://github.com/clap-rs/clap) · [figment](https://github.com/SergioBenitez/figment) | CLI and layered TOML/env config. | SPEC-0000 §7 |
| [k256](https://github.com/RustCrypto/elliptic-curves) · [rmp-serde](https://github.com/3Hren/msgpack-rust) | secp256k1 signing and msgpack action hashing. | SPEC-0002 §4 |
| [wiremock](https://github.com/LukeMathWalker/wiremock-rs) | Mock `/exchange` / `/info` tests. | SPEC-0002 §14 |
| [axum](https://github.com/tokio-rs/axum) | `/healthz`, `/readyz`, `/metrics` endpoints. | SPEC-0000 §9 |

## Libraries — Python research (`research/pyproject.toml`)

| Link | Why it matters | Used in |
|---|---|---|
| [uv](https://docs.astral.sh/uv/) | Reproducible research environment (`uv.lock`). | `research/README.md` |
| [polars](https://www.pola.rs) | Normalized Parquet tables and study transforms. | SPEC-0008 §4/§13.1 |
| [DuckDB](https://duckdb.org) | SQL over Parquet where it is easier. | SPEC-0008 §4 |
| [orjson](https://github.com/ijl/orjson) | Fast envelope line parsing. | `research/hlr/io.py` |
| [python-zstandard](https://github.com/indygreg/python-zstandard) | Streaming zstd reader for segments (incl. crashed tails). | `research/hlr/io.py` |
| [Matplotlib](https://matplotlib.org) | Report charts (per-day bars, capacity curves). | SPEC-0008 §13.7 |
| [pytest](https://github.com/pytest-dev/pytest) | Research test suite. | `research/tests/` |

## Papers and articles

Research reports must not invent external citations; when a study relies on a
paper, add it here with its source and the study that uses it (SPEC-0008 §13.7
requires the citation in the report too).

| Link | Why it matters | Used in |
|---|---|---|
| [DeepLOB (Zhang, Zohren, Roberts 2019)](https://arxiv.org/abs/1808.03668) | Deep LOB model on LSE data; a baseline for the deep-LOB family. | SPEC-0008 M-1…M-5 (literature) |
| [LOBCAST](https://arxiv.org/html/2308.01915) | Benchmark showing deep LOB models drop on unseen LOBSTER data; FI-2010 is too easy. | SPEC-0008 M-1…M-5 |
| [Cont, Kukanov, Stoikov (2014)](https://arxiv.org/abs/1011.6402) | Linear relation between best-level OFI and price change; the baseline every ML model must beat. | SPEC-0008 M-2 |
| [FinBERT (Araci 2019)](https://arxiv.org/abs/1908.10063) | Offline news/headline sentiment classifier used as a feature, not an edge. | SPEC-0008 §9.2, M-4 |
| [Lopez-Lira & Tang](https://arxiv.org/abs/2304.07619) | LLM news scores predict next-day returns, but the edge shrinks with costs and adoption. | SPEC-0008 M-4 |
| [Deflated Sharpe Ratio (Bailey & Lopez de Prado)](https://papers.ssrn.com/sol3/papers.cfm?abstract_id=2460551) | Pass/fail input that counts the true trial count. | SPEC-0008 §13 M-1…M-5 |
| [Probability of Backtest Overfitting (Bailey & Lopez de Prado)](https://papers.ssrn.com/sol3/papers.cfm?abstract_id=2326253) | Overfitting control for the multiple-testing haircut. | SPEC-0008 §13 M-1…M-5 |
| [Does Meta-Labeling Add to Signal Efficacy? (Hudson & Thames)](https://hudsonthames.org/does-meta-labeling-add-to-signal-efficacy-triple-barrier-method/) | Meta-labelling improves an existing rule but is a filter, not an edge creator. | SPEC-0008 M-1 |
