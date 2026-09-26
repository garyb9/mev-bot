# mev-bot

A **low-latency arbitrage / MEV-style trading system for Hyperliquid**, written
in Rust (HyperCore first, HyperEVM where it pays). Which arb we build is
decided by recorded data, not preference. A second, slower strategy family
uses options-market positioning to trade HIP-3 tokenized-stock perps and
crypto bluechips. The original Ethereum Uniswap V2 bot is retired under
[`legacy/`](legacy/).

**Start here:**

- [`docs/GOAL.md`](docs/GOAL.md): the goal, success metrics, latency budget,
  and roadmap.
- [`AGENTS.md`](AGENTS.md): how agents and contributors work in this repo.

> Status: **M0–M2 done** (platform, HyperCore market data, signing and
> execution). **Now:** M2.5 execution hardening (SPEC-0002 §17) and M3
> market-data recorder + opportunity research (SPEC-0008). The bot defaults
> to `observe` and never trades without explicit configuration.

## Design

Everything is specified before it is built. Spec numbers are identifiers; the
build order is the roadmap in [`docs/GOAL.md`](docs/GOAL.md). See [`specs/`](specs/):

| Spec | Topic |
|---|---|
| [SPEC-0000](specs/SPEC-0000-platform.md) | Platform & architecture |
| [SPEC-0001](specs/SPEC-0001-hyperliquid-client.md) | Hyperliquid client & market data |
| [SPEC-0002](specs/SPEC-0002-execution-signing.md) | Execution, signing & account |
| [SPEC-0003](specs/SPEC-0003-strategy-engine.md) | Strategy engine |
| [SPEC-0004](specs/SPEC-0004-risk-portfolio-accounting.md) | Risk, portfolio & accounting |
| [SPEC-0005](specs/SPEC-0005-hyperevm.md) | HyperEVM sources & executor (deferred) |
| [SPEC-0006](specs/SPEC-0006-deployment-observability-runbooks.md) | Deployment, observability & runbooks |
| [SPEC-0007](specs/SPEC-0007-polymarket-parked.md) | Polymarket (parked) |
| [SPEC-0008](specs/SPEC-0008-recorder-and-opportunity-research.md) | Market-data recorder & opportunity research (M3) |
| [SPEC-0009](specs/SPEC-0009-own-node.md) | Own Hyperliquid non-validator node (M3.5, later) |

## Workspace

```
crates/
  mev-core/       shared types, config, errors, clock
  mev-hl-client/  Hyperliquid REST/WS client (market data + execution)
  mev-recorder/   raw market-data recorder (SPEC-0008)          [M3]
  mev-hyperevm/   HyperEVM (chain 999) sources & executor      [later]
  mev-strategy/   pluggable strategies + cost/edge model
  mev-risk/       risk limits, portfolio, accounting, kill switch
  mev-metrics/    tracing, metrics, health
  mev-bot/        the `hl` binary (orchestration)
research/         Python opportunity-research toolkit (SPEC-0008) [M3]
docs/             project goal and roadmap
```

## Quickstart

```sh
# Build and test
cargo build --workspace
cargo test --workspace

# Show resolved config (secrets redacted)
cargo run -p mev-bot -- config show

# Run in observe mode (safe default: connects, builds state, never trades)
cargo run -p mev-bot -- run --mode observe

# Health / readiness / metrics
curl localhost:9090/healthz
curl localhost:9090/readyz
curl localhost:9090/metrics
```

Configuration layers as: built-in defaults → `config/default.toml` →
`config/{HL_ENV}.toml` → `HL_*` env vars → CLI flags. Copy
[`.env.example`](.env.example) to `.env` to get started.

## Execution modes & safety

| Mode | Behavior | Keys |
|---|---|---|
| `observe` (default) | connect & build state; **never** place orders | none |
| `simulate` | run strategies, simulate fills; **never** submit | none |
| `live` | submit orders | agent key + `HL_LIVE_CONFIRM=YES` |

- Only an **agent/API wallet** (which cannot withdraw) lives on the host; the
  master key never does.
- `live` also arms a dead-man's switch (`scheduleCancel`) so a dead bot can't
  leave stale orders.
- `HL_AUTONOMY=auto` (default) lets the engine trade; `confirm` asks first.

See [`RUNBOOK.md`](RUNBOOK.md) for operations.

## License

MIT — see [LICENSE](LICENSE).
