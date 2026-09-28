# SPEC-0000 — Platform & Architecture

**Status:** Draft
**Depends on:** none
**Blocks:** SPEC-0001 … SPEC-0007

## 1. Purpose

Establish the foundation for a Hyperliquid-first, always-on trading system: workspace layout, technology baseline, execution modes, configuration/secrets, observability, process resilience, and safety posture. No trading logic lives here; this spec defines the skeleton every later spec plugs into.

## 2. Goals

- Modern, reproducible Rust codebase with no deprecated dependencies.
- Long-running daemon: resilient to WS/RPC drops, reconnects cleanly, shuts down gracefully.
- Three execution modes with a **safe default** (`observe`), switchable without code changes.
- One consistent way to configure, log, measure, and test everything.
- A clear path to add HyperCore (now) and HyperEVM (later) without restructuring.

## 3. Non-goals

- Strategy logic, order building/signing, or market data handling (SPEC-0001/0002/0003).
- Risk limits and PnL accounting (SPEC-0004).
- HyperEVM DEX arb (SPEC-0005) and Polymarket (SPEC-0007).

## 4. Technology baseline

| Concern | Choice | Rationale |
|---|---|---|
| EVM/EIP-712 | **Alloy 2.x** | Successor to deprecated ethers-rs; reused for HyperEVM later |
| Async runtime | tokio (full) | Standard; matches all deps |
| WebSocket | Backend-swappable: `fastwebsockets` default, `tokio-tungstenite` fallback | Decided by SPEC-0001 benchmark |
| REST | `reqwest` (rustls) | HTTP/2, connection pooling |
| Serialization | `serde` / `serde_json`; `rmp-serde` for HL action hashing | Hyperliquid signs msgpack-encoded actions |
| Decimal math | `rust_decimal` for prices/sizes/PnL; no `f64` in money paths | Exact tick/lot rounding, deterministic accounting |
| Config | `clap` (CLI) + `figment` (TOML + env layering) + `secrecy` | Layered, testable, secret-aware |
| Errors | `thiserror` (libs) + `anyhow` (binary edges) | Typed where it matters |
| Logging | `tracing` + `tracing-subscriber` (JSON in prod, pretty in dev) | Structured, async-aware |
| Metrics | `metrics` facade + Prometheus exporter | Operability |
| Persistence | **SQLite** (`rusqlite`, bundled) with WAL | Durable event store for PnL, attribution, reconciliation, replay |
| Testing | `cargo-nextest`, `wiremock`/`httpmock`, `proptest` | Unit + HTTP mocking + property math |
| Edition/MSRV | edition 2024, MSRV 1.90 | Matches modern dep floor |

**Constraint:** `cargo tree` must contain **no `ethers`**. The official `hyperliquid_rust_sdk` is therefore not a default dependency (it pulls `ethers 2.x`).

## 5. Workspace layout

```
crates/
  mev-core/       types, config, errors, clock, ids, decimal helpers
  mev-hl-client/  HyperCore REST/WS client + signing (backend trait; SPEC-0001/0002)
  mev-hyperevm/   HyperEVM (999) provider, DEX sources, executors      [SPEC-0005]
  mev-strategy/   Strategy trait + implementations                     [SPEC-0003]
  mev-risk/       limits, liquidation guard, portfolio, PnL            [SPEC-0004]
  mev-metrics/    tracing init + metric definitions
  mev-bot/        orchestration binary
specs/            this and sibling specs
contracts/        HyperEVM executors only (later)
```

Rules: `mev-core` depends on nothing internal; strategy/execution depend on core + client; `mev-bot` wires everything. No cycles.

The current `src/` and `contract/Arb.sol` (Ethereum mainnet V2 arb) are **retired**: deleted from the build, ABIs/math retained under `legacy/` as reference for SPEC-0005.

## 6. Execution modes

Selected via `--mode` / `HL_MODE`, default `observe`:

- **observe** — connect `/info` + WS, build local market state, **no orders**. No keys required. Always runnable.
- **simulate** — strategies run and emit intended orders; fills simulated against live book (and revm for EVM legs later). No keys required. Primary development/validation mode.
- **live** — signs and submits real orders via `/exchange`. Requires agent wallet + funded account and an explicit `HL_LIVE_CONFIRM=YES`.

Mode transitions are logged and surfaced as a metric. `live` cannot be entered silently.

## 7. Configuration & secrets

- Layered precedence: defaults → `config/default.toml` → `config/{env}.toml` → `HL_*` env vars → CLI flags.
- Secrets (`HL_AGENT_PRIVATE_KEY`, `HL_ACCOUNT_ADDRESS`, RPC URLs) come from env or a secrets file, wrapped in `secrecy`, never logged; a redaction layer is applied to `tracing`.
- Config is validated at startup with precise errors (missing key in `live`, malformed address, unknown market).
- **One agent wallet per bot instance** (nonce isolation; SPEC-0002).
- `.env.example` documents every variable.

## 8. Clock & time

- A single `Clock` abstraction (system ms) injected everywhere; deterministic in tests.
- NTP/chrony assumed in prod. Nonce generation validates monotonicity (SPEC-0002).
- Timestamps recorded for every state transition for latency analysis.

## 9. Observability

- `tracing` spans per component (ingest, strategy, execution), JSON output in prod.
- Prometheus `/metrics`; `/healthz` (liveness) and `/readyz` (all feeds fresh) HTTP endpoints.
- Core metrics: WS msg rate, decode latency p50/p99, book staleness, reconnect count, order submit latency, order rejects, nonce errors, simulated/real PnL, mode.
- Latency is first-class: any p99 above target raises an alert.

## 10. Process model & resilience

- Supervisor task tree: each source/client runs as a supervised task with exponential-backoff restart and jitter.
- Bounded channels between ingest → strategy → execution; backpressure drops stale data rather than queuing unbounded.
- Graceful shutdown on SIGTERM/SIGINT: cancel token, flush metrics, no in-flight half-orders (execution spec defines drain rules).
- Panic hook logs and triggers shutdown; no silent task death.

## 11. Safety posture

- Agent/API wallet only (cannot withdraw); master key never present on the host.
- `live` gated by mode + explicit confirmation + presence of keys.
- Pre-trade limits (SPEC-0004) enforced before any submit; hard kill switch via signal, file flag, and metric.
- Default posture of the whole system is "never trade unless explicitly told to".

## 12. Testing strategy

- **Unit**: config precedence, decimal/rounding helpers, clock.
- **HTTP/WS**: `wiremock` for `/info` `/exchange`; recorded WS fixtures for replay.
- **Replay**: recorded market sessions drive strategies deterministically (basis for paper trading).
- **Integration**: testnet order round-trip in `simulate`/testnet once keys exist.
- **Benchmarks**: `criterion` harness for the SPEC-0001 client comparison.

## 13. CI/CD

- GitHub Actions: `fmt --check`, `clippy -D warnings`, `nextest`, `cargo deny`, `cargo audit`, coverage.
- `Cargo.lock` committed (binary project).
- Container image: multi-stage → distroless/scratch; non-root; `--mode observe` default.
- Deployment: systemd unit or compose; single instance; documented restart policy. Region/VPS chosen for low RTT to the Hyperliquid API (measured, recorded).

## 14. Version control & milestones

**Policy:** commit at every milestone with a focused, conventional-commit message, and push to `main`.

- Prefixes: `feat:`, `fix:`, `refactor:`, `docs:`, `chore:`, `test:`.
- One logical change per commit; no mixed "spec + code" commits.
- `Cargo.lock` is committed (remove it from `.gitignore`).
- Push to `main` after each green milestone (CI passing). If CI fails, fix before pushing — no force-push, no skipping hooks.
- If `main` is protected and rejects pushes, fall back to a branch + PR and report it rather than forcing.

**Commit checkpoints**

| # | Milestone | Commit |
|---|---|---|
| M0.1 | Specs 0000–0004 drafted | `docs: add platform and hyperliquid specs` |
| M0.2 | Workspace scaffolded, edition/MSRV | `chore: scaffold workspace crates` |
| M0.3 | Old `src/` retired, ethers removed | `refactor: retire ethereum v2 arb, drop ethers` |
| M0.4 | Config + modes + secrets | `feat: layered config and execution modes` |
| M0.5 | Observability + health + shutdown | `feat: tracing, metrics, health endpoints, graceful shutdown` |
| M0.6 | CI + Cargo.lock | `ci: add fmt/clippy/test/deny pipeline` |
| M1.x | Client + market data + benchmark | `feat(hl): client, market data, benchmark` |
| M2.x | Execution/signing/nonce | `feat(hl): order execution, eip-712 signing, nonce` |
| M2.5 | Execution hardening + latency baseline (SPEC-0002 §17) | `feat(hl): …` / `bench(hl): …` per task |
| M3.x | Recorder + opportunity research (SPEC-0008) | `feat(recorder): …` / `feat(research): …` per task |
| M4+ | See the roadmap | — |

Milestones later in the list may split into smaller commits (e.g. client vs benchmark).

> **Roadmap moved (2026-09-26).** The authoritative milestone order from M2.5 onward lives in [`docs/GOAL.md`](../docs/GOAL.md) §7. The recorder and research milestone (SPEC-0008) now comes before the funding pilot; market-making and HyperEVM are conditional on research results.

## 15. Milestones (P0)

1. Scaffold workspace + crates; edition/MSRV set.
2. Remove `src/`, old `Cargo.toml` deps, and all `ethers`; archive ABIs/math to `legacy/`.
3. Config + CLI + secrets + modes.
4. `tracing` + metrics + `/healthz` `/readyz` `/metrics`.
5. Graceful shutdown + panic hook.
6. CI pipelines + `Cargo.lock`.
7. `mev-hl-client` trait stub + no-op observe loop so the daemon idles cleanly.
8. README + runbook.

## 16. Acceptance criteria

- `cargo run -p mev-bot -- --mode observe` starts, loads config, logs, serves health endpoints, and exits cleanly on SIGTERM.
- `cargo tree | grep -c ethers` = 0.
- CI green; `Cargo.lock` committed.
- Running with `--mode live` and no keys fails fast with a clear error.
- No secrets appear in logs at any level.

## 17. Open questions

1. Repo/binary naming — keep `mev-bot`, or rename the binary (e.g. `hl-bot`) while keeping the repo?
2. Delete old code outright, or keep under `legacy/` (ABIs + math + docs only)?
3. Deployment target preference (VPS region, container vs bare metal)?
4. Do you want the SPEC-0001 client benchmark to also include a community Alloy-native SDK, or custom-vs-official only?

## 18. Follow-ups: CI (C-tasks)

§13 requires `cargo deny`, `cargo audit`, and `nextest`; the current `.github/workflows/ci.yml` runs only fmt, clippy, and tests. GOAL §4.7 also makes latency regressions bugs, so benchmarks belong in CI.

| ID | Title | Tier | Size | Status |
|---|---|---|---|---|
| C-1 | `cargo deny` (advisories, licenses, bans incl. `ethers`, duplicate-version warnings) + `deny.toml` | T1 | S | ✅ |
| C-2 | Switch tests to `cargo nextest` | T1 | S | ✅ |
| C-3 | Bench job in quick mode (decode, sign, SPEC-0010 engine benches) comparing against a stored baseline; **warn** on > 15% regression (shared runners are noisy, so it doesn't fail the build) | T1 | M | ✅ |
| C-4 | Research CI: `uv run ruff check` + `uv run pytest` in `research/` when that directory changes | T1 | S | ✅ |

*Done when* (each): the job runs on PRs and on `main`, and is green on the current tree. C-1 must fail the build if `ethers` enters the graph.
