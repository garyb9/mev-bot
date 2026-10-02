# mev-core

**Common layer.** Shared primitives; depends on no other internal crate.

| Module | Purpose |
|---|---|
| `config` | layered config (defaults, TOML, `HL_*` env, CLI) and redaction |
| `clock` | `Clock` / `SystemClock` abstraction for deterministic tests and replay |
| `error` | shared error types |
| `db`, `db/writer` | SQLite store and the single-writer task |
| `watchlist` | persisted coin watchlist load/save |

Specs: SPEC-0000, SPEC-0004. Used by every other crate.
