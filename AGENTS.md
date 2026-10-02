# AGENTS.md

Instructions for every AI agent (and human) working in this repository.

## 1. Read first, in this order

1. [`docs/GOAL.md`](docs/GOAL.md): what we are building, why, how success is
   measured, the **latency-first** rules, and the roadmap. **Mandatory.**
2. [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md): how the processes, crates,
   hot path, and modes fit together. External links live in
   [`docs/REFERENCES.md`](docs/REFERENCES.md).
3. The spec that owns your task (see §3). Specs live in [`specs/`](specs/).
4. The code you are about to touch, and its tests.

If your task conflicts with `docs/GOAL.md` or with its spec, **stop and say so**.
Don't pick one silently.

## 2. The project in 30 seconds

- A Rust workspace for a **low-latency arbitrage / MEV-style trading bot on
  Hyperliquid** (HyperCore first, HyperEVM later).
- **Evidence before strategy:** the recorder + research (SPEC-0008) decides
  which arb we build. Don't build a strategy without a passing study.
- **Speed matters:** on the hot path, choose the faster of two equally correct
  and safe designs, and measure it (`docs/GOAL.md` §5).
- **Safety first:** default mode is `observe`; `live` is gated; only an agent
  wallet (which cannot withdraw) is ever on the host.

## 3. Where things are

| Path | What | Spec |
|---|---|---|
| `docs/GOAL.md` | Goal, principles, latency budget, roadmap (index of all docs: `docs/README.md`) | — |
| `specs/SPEC-0000…` | Platform: config, modes, observability, CI | SPEC-0000 |
| `crates/mev-core` | Config, clock, errors, SQLite (`db.rs`, `db/writer.rs`), watchlist | 0000, 0004 |
| `crates/mev-hl-client` | Hyperliquid REST/WS client, market state, signing, nonce, orders, transports | 0001, 0002 |
| `crates/mev-engine` | Event-driven engine core: interned types, typed ingest, v2 sync `Strategy` trait + `FundingBasis`/`MarketMaker` (the `hl` run path) | 0010 |
| `crates/mev-recorder` | Market-data recorder (**new, M3**) | 0008 |
| `crates/mev-strategy` | Strategy building blocks: cost model, views, intents, sizing, paper executor; the v2 `Strategy` trait + implementations live in `mev-engine` | 0003, 0010, 0011 |
| `crates/mev-risk` | Risk limit gate (kill switch and breakers pending: SPEC-0004 §16) | 0004 |
| `crates/mev-hyperevm` | HyperEVM sources/executor (deferred) | 0005 |
| `crates/mev-metrics` | Tracing, Prometheus metric names, health | 0000, 0006 |
| `crates/mev-bot` | The `hl` binary (CLI + orchestration); `src/engine.rs` holds config/strategy building, the SQLite `Recorder`, and replay helpers; the run loop is `mev-engine`'s `EngineLoop<StrategyDispatcher>` | all, 0010 |
| `research/` | Python research toolkit + studies (**new, M3**) | 0008 |
| `specs/decisions/` | ADRs (architecture/strategy decisions) | — |
| `legacy/` | Retired Ethereum bot. Reference only; never build or import it. | — |
| `RUNBOOK.md` | Operations | 0006 |

## 4. How to pick up and finish a task

0. **Check the T0 fix-first list** in [`docs/GOAL.md`](docs/GOAL.md) §2.2.
   If any T0 item is open and its dependencies are met, do it first, before
   any other task, unless you were explicitly assigned something else.
1. Find the task ID in its spec's work-breakdown table (e.g. SPEC-0008 §14,
   SPEC-0002 §17).
2. Check that every dependency is ✅. If not, pick another task or report
   the blocker.
3. Read the task's **Do** and **Done when** lines. Implement exactly that.
   No extra features, no drive-by refactors.
4. Write tests first where practical. Every behavior in **Done when** needs a
   test.
5. Run the checks (§6). All must pass.
6. Tick the task's status (☐ → ✅) in the spec table **in the same commit** as
   the code.
7. Commit (§7).

If the spec is wrong, unclear, or reality differs (an API field, a limit, a
fee), **don't guess**. Write it under the spec's "Open questions" (or fix a
verified fact, with a source link and date) and flag it in your final message.

## 5. Hard rules

### Safety (never break these)

- **Never** run `hl run --mode live`, and never set `HL_LIVE_CONFIRM`.
- No testnet order round-trip until every T0 fix (`docs/GOAL.md` §2.2) is ✅.
- **Never** read, create, print, or commit private keys, `.env` files, or
  secrets. Testnet keys only when the owner explicitly provides one for a
  specific task.
- **Never** commit anything under `data/` (recordings, SQLite DBs) or
  `research/data/`.
- Every order path goes through the risk engine; no bypasses, even "for testing".
- Recorder and research code never load keys.

### Code

- **Money math uses `rust_decimal`**, never `f64` (research Python is exempt).
- **No `ethers`** anywhere. Use Alloy for EVM work.
- No `unwrap`/`expect` on network, parse, or I/O paths; return typed errors.
- Follow the **hot-path rules** in `docs/GOAL.md` §5.1: no blocking I/O, no
  per-event INFO logs, no locks held across `.await`, avoid per-event
  allocation.
- Every hot-path change includes a benchmark or latency metric; a latency
  regression is a bug.
- Metric names go in `crates/mev-metrics/src/lib.rs` (`names` module).
- Match the style of the surrounding code: doc comments on public items,
  `thiserror` in libraries, `anyhow` only at binary edges.
- Add dependencies at the workspace level (`[workspace.dependencies]`) and
  justify each new one in the commit message.

### Specs

- Specs are the source of truth. Code that changes specified behavior must
  update the spec, in a **separate** commit (the only exception is ticking a
  task's status).
- Spec numbers are identifiers, not an order. The order lives in
  `docs/GOAL.md` §7.
- Facts marked **⚠ verify** can't be relied on until their verification task
  records a source.

## 6. Checks (must pass before every commit)

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

For `research/` changes:

```sh
cd research && uv run ruff check && uv run pytest
```

Report failures honestly, with output. Never skip, disable, or `#[ignore]` a
test to make a check pass.

## 7. Git

- Conventional commits: `feat(scope):`, `fix(scope):`, `refactor:`, `docs:`,
  `test:`, `chore:`, `ci:`, `bench:`. Scopes in use: `hl`, `core`,
  `recorder`, `research`, `strategy`, `risk`, `spec-XXXX`.
- One logical change per commit. Don't mix spec edits with code (except status
  ticks).
- **Other agents may be working at the same time.** Keep commits small, don't
  reformat or rewrite files outside your task, and re-read a file before
  editing if it may have changed.
- No force-push, no `--no-verify`, no history rewrites.

## 8. Definition of done

- The task's **Done when** items are all true, with tests.
- The §6 checks pass.
- The spec status is ticked; any new facts or open questions are recorded.
- The final message says what changed, what was verified (and how), and
  anything left open.
