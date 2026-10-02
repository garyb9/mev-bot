# mev-bot

**App layer.** Package `mev-bot`, binary `hl`: CLI and orchestration only; the
logic lives in the library crates.

| Module | Purpose |
|---|---|
| `main` | clap CLI and the market-data / account subcommands |
| `engine` | config and strategy building, SQLite `Recorder`, replay helpers |
| `live` | live I/O tasks: ingest, exec writer, account stream, reconciler |
| `record` | `hl record` (plan, run, inspect, verify, repair-manifest) |
| `replay` | deterministic replay driver (SQLite session or recorder segments) |
| `db_lock` | single-process lock on the SQLite store |

Try `cargo run -p mev-bot -- --help`. Never run `--mode live` outside the
documented procedure (`RUNBOOK.md`). Bench: `benches/engine.rs`.
