# mev-strategy

**Domain layer.** Venue-agnostic strategy building blocks.

| Module | Purpose |
|---|---|
| `cost` | `CostModel`, `FeeRates`: net-edge math |
| `view` | market, book, and account views handed to strategies |
| `intent` | exchange-agnostic `OrderIntent` |
| `size` | sizing |
| `paper` | paper executor |
| `event`, `id` | events, ids, deterministic randomness |

The v2 `Strategy` trait and its implementations live in `mev-engine`.
Specs: SPEC-0003, SPEC-0010, SPEC-0011.
