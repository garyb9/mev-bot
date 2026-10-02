# Documentation index

Read in this order: GOAL, ARCHITECTURE, then the spec that owns your task.

## Core docs

| Doc | What |
|---|---|
| [GOAL.md](GOAL.md) | Goal, success metrics, latency-first rules, priority tiers, roadmap |
| [ARCHITECTURE.md](ARCHITECTURE.md) | Processes, crates, hot path, modes, order lifecycle, persistence |
| [REFERENCES.md](REFERENCES.md) | External links and sources |
| [../RUNBOOK.md](../RUNBOOK.md) | Operations: start/stop, health, incidents |
| [../AGENTS.md](../AGENTS.md) | Rules for agents and contributors |

## Specs (`../specs/`)

Numbers are identifiers, not an order; the build order is in GOAL.md §7.

| Spec | Topic |
|---|---|
| [SPEC-0000](../specs/SPEC-0000-platform.md) | Platform: config, modes, observability, CI |
| [SPEC-0001](../specs/SPEC-0001-hyperliquid-client.md) | Hyperliquid client and market data |
| [SPEC-0002](../specs/SPEC-0002-execution-signing.md) | Execution, signing, account |
| [SPEC-0003](../specs/SPEC-0003-strategy-engine.md) | Strategy engine |
| [SPEC-0004](../specs/SPEC-0004-risk-portfolio-accounting.md) | Risk, portfolio, accounting |
| [SPEC-0005](../specs/SPEC-0005-hyperevm.md) | HyperEVM sources and executor (deferred) |
| [SPEC-0006](../specs/SPEC-0006-deployment-observability-runbooks.md) | Deployment, observability, runbooks |
| [SPEC-0007](../specs/SPEC-0007-polymarket-parked.md) | Polymarket (parked) |
| [SPEC-0008](../specs/SPEC-0008-recorder-and-opportunity-research.md) | Recorder and opportunity research |
| [SPEC-0009](../specs/SPEC-0009-own-node.md) | Own non-validator node (later) |
| [SPEC-0010](../specs/SPEC-0010-event-driven-engine.md) | Event-driven engine and hot path |
| [SPEC-0011](../specs/SPEC-0011-multi-leg-execution.md) | Multi-leg execution and hedging |

## Decisions (ADRs)

| ADR | Topic |
|---|---|
| [0001](../specs/decisions/0001-hl-client-backend.md) | Hyperliquid client backend |

## Notes and briefs

| Doc | What |
|---|---|
| [research/data-sources-2026-09-29.md](research/data-sources-2026-09-29.md) | Data-source and alt-data survey for SPEC-0008 |
| [briefs/2026-09-27-post-e13-review-fixes.md](briefs/2026-09-27-post-e13-review-fixes.md) | Historical task brief: review fixes after E-13 |

## Elsewhere

- Crate overviews: `crates/*/README.md`.
- Recorder deployment: [../deploy/recorder/README.md](../deploy/recorder/README.md).
- Research toolkit: [../research/README.md](../research/README.md).
