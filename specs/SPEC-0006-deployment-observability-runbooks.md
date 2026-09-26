# SPEC-0006 — Deployment, Observability & Runbooks

**Status:** Draft
**Depends on:** SPEC-0000 … SPEC-0005
**Blocks:** production operation

## 1. Purpose

Operationalize the system: deployment topology, environments, secrets, observability, alerting, backup/recovery, and runbooks. SPEC-0000 sets the principles; this spec makes them concrete enough to run 24/7.

## 2. Goals

- Reliable, always-on operation with fast, rehearsed incident response.
- Reproducible deploys and a clean local → testnet → mainnet path.
- Clear, actionable telemetry and alerting.
- Durable state (SQLite) with tested backup/restore.

## 3. Deployment topology

- **One instance per agent wallet** (nonce isolation, SPEC-0002). No horizontal scaling of the trading process.
- Container image (multi-stage → distroless/scratch, non-root) or systemd unit; explicit restart policy.
- Host located for **low RTT to the Hyperliquid API** (measure and record; revisit colocation/direct peering later).
- NTP/chrony required; the nonce clock must never regress.
- Persistent volume for the SQLite DB (`HL_DB_PATH`); resource limits (CPU/mem) set; OOM/restart surfaced.

## 4. Environments

| Env | Network | Keys | Mode default |
|---|---|---|---|
| local | — | none | observe |
| testnet | Hyperliquid testnet | testnet agent | simulate |
| mainnet | Hyperliquid mainnet | mainnet agent | observe → live (gated) |

Separate config files and **separate agent wallets** per environment.

## 5. Config & secrets

- Layered config (SPEC-0000 §7); secrets from env or a secrets file, wrapped in `secrecy`.
- Redaction enforced at the logging layer; a test asserts no secret pattern appears in logs.
- **Rotation:** agent keys are rotatable without code changes (re-`approveAgent` + restart); documented runbook.
- Master key never on the host.

## 6. Observability

- **Logs:** structured `tracing` JSON; per-component levels; rationale attached to every order/decision; configurable retention.
- **Metrics:** Prometheus catalog aggregated from specs — feed staleness, reconnect count/duration, REST weight/min, decode & submit latency histograms, order rejects by status, nonce errors, exposure & margin utilization, liquidation distance, PnL (realized/unrealized per strategy/coin), breaker trips, dead-man armed, DB write health.
- **Traces:** one span per decision path (ingest → strategy → risk → execution) with timings, so a slow path is attributable.
- **Health:** `/healthz` (liveness + breaker state + dead-man armed), `/readyz` (all subscribed feeds fresh), `/metrics`.

## 7. Alerting

Thresholds (initial): feed stale > tolerance; reconnect storm; submit p99 regression; order-reject spike; any nonce error; margin utilization / liquidation-distance breach; daily-loss or drawdown break; breaker trip; dead-man switch not armed in `live`; DB write failures; reconciliation drift unresolved. Alerts route to the chosen channel and reference a runbook.

## 8. Runbooks

- **Start/stop/upgrade** (graceful shutdown disarms scheduleCancel, flushes DB).
- **Force reconnect** and verify fresh state.
- **Nonce recovery** (stale/duplicate rejection path).
- **Key rotation** (agent re-approval).
- **Kill switch**: manual trip and verification that cancel-all/halt took effect.
- **DB backup/restore** and a periodic restore test.
- **Reconciliation drift**: diagnose, resync, or halt.
- **Failed deploy**: rollback procedure.

## 9. Backup & recovery

- WAL-safe SQLite backup on a cadence to off-host storage; snapshot before upgrades.
- Restore tested on a schedule; a restore must not require re-deriving PnL from scratch.

## 10. Security

- Least privilege; agent-only key; no master key on host.
- Network egress allowlist to Hyperliquid endpoints (+ chosen RPC/relay).
- Audit log of all orders/cancels/config changes; secrets never logged.

## 11. CI/CD & testing gates

- Pipeline: `fmt`, `clippy -D warnings`, `nextest`, `cargo deny`, `cargo audit`, coverage; `Cargo.lock` committed.
- Fork/replay integration tests (deterministic), contract tests (Foundry, SPEC-0005), and a testnet smoke test before mainnet rollout.
- Canary on testnet, then mainnet in `observe`, then `simulate`, then `live` with small size.

## 12. Acceptance criteria

- Deploy to a clean host in one documented step; daemon survives reboot and reconnects.
- Every alert in §7 maps to a runbook, and the kill switch is verified end-to-end.
- Backup/restore drill passes.
- No secret appears in logs; audit log present for all write actions.

## 13. Open questions

1. Hosting target and region (measure RTT to the Hyperliquid API).
2. Alert channel/tooling (e.g. PagerDuty, Slack, Grafana alerts).
3. Off-host backup destination and retention policy.
