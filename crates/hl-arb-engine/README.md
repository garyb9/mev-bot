# hl-arb-engine

**Domain layer.** The event-driven, single-threaded engine that `hl run` and
`hl replay` drive. Hot-path rules: `docs/GOAL.md` §5.1.

| Module | Purpose |
|---|---|
| `types`, `instrument` | interned ids, events, instrument table |
| `ingest`, `channels` | typed decoders, market/account channels |
| `run`, `dispatch`, `timers`, `routes` | `EngineLoop`, `StrategyDispatcher`, timers, routing |
| `strategy`, `strategies/` | v2 `Strategy` trait, `FundingBasis`, `MarketMaker` |
| `orders`, `risk`, `reconcile` | order manager, risk gate integration, REST reconciliation |
| `exec`, `paper_exec`, `builder` | exec backends and order building |
| `journal`, `state`, `clock` | action journal, state, engine clock |

Spec: SPEC-0010. Benches: `benches/ingest.rs`, `benches/iterate.rs`;
`tests/zero_alloc.rs` guards hot-path allocation.
