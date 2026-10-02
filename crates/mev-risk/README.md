# mev-risk

**Domain layer.** The mandatory, fail-closed gate between strategy intents and
execution.

| Module | Purpose |
|---|---|
| `limits` | `LimitRisk` / `Limits`: per-order limit checks |
| `kill` | `KillSwitch` (flag file polled by the bot; see `hl panic` / `hl resume`) |
| `halt` | `TradingHalt` and cancel-all helpers |

Spec: SPEC-0004 (breakers and drawdown guards still pending, §16).
