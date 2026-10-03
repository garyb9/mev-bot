# hl-arb-metrics

**Common layer.** Observability.

| Module | Purpose |
|---|---|
| `logging` | `tracing` subscriber setup |
| `prometheus` | Prometheus recorder install and exporter |
| `health` | `/healthz` and `/readyz` state |
| `lib.rs` (`names`) | the single registry of metric names |

Specs: SPEC-0000, SPEC-0006. New metric names go in `names`.
