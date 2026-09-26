# SPEC-0009 — Own Hyperliquid Non-Validator Node

**Status:** Draft (**later**: see §2 for when to start)
**Milestone:** M3.5 (see [`docs/GOAL.md`](../docs/GOAL.md) §7)
**Depends on:** SPEC-0008 recorder in production (R-10). SPEC-0000/0006 for ops conventions.
**Unblocks:** SPEC-0008 R-9, S-4, S-8 (HyperEVM data); a faster and richer data path for the bot (latency-first, GOAL §5); SPEC-0005.

---

## 0. How to use this spec

Same rules as SPEC-0008 §0: read [`docs/GOAL.md`](../docs/GOAL.md) and [`AGENTS.md`](../AGENTS.md), pick a task from §12, respect its dependencies, do exactly the **Do**, satisfy the **Done when**, and tick the status in the same commit. Facts marked **⚠ verify** need a source before you rely on them.

**Host safety:** the node host holds **no keys**. It never runs the trading bot in `live` mode as part of this spec.

## 1. Purpose

Run our own **non-validating** Hyperliquid node (a non-validator is already non-archive by default; there is no lighter mode) in Tokyo, so that we get:

| Benefit | Why it matters | Replaces / improves |
|---|---|---|
| **Lowest-latency HyperCore data**: blocks stream from peers straight into our process | Latency-first (GOAL §5); likely faster than the public WS API, which has to process and fan out the data first. **N-8 measures this.** | Public WS `bbo` / `l2Book` / `trades` |
| **Local EVM JSON-RPC** (`--serve-eth-rpc`) | The public HyperEVM RPC allows 100 req/min, far too few for per-block pool reads. Commercial RPC costs money and adds a hop. | SPEC-0008 R-9's `HL_EVM_WS_URL`; SPEC-0005 |
| **Local `/info` server** (`--serve-info`) | Reads without spending the 1200 weight/min per-IP budget | `HttpInfo` against `api.hyperliquid.xyz` |
| **Richer data**: every fill, every order status, raw book diffs (L4, per-order), HIP-3 oracle updates, misc events | Liquidation/flow studies (O6), queue-position modeling, HIP-3 oracle timing (O1/O10) | Nothing public offers this |

## 2. When to start (triggers)

This is deliberately **not** on the critical path. Start when **any** of these is true:

1. SPEC-0008 studies **S-4 (O4)** or **S-8 (O8)** are blocked on HyperEVM RPC access.
2. A latency-sensitive study (O1, O2, O5, O6, O10) comes out **PASS or MARGINAL**, and its `latency_requirement_ms` is below what the public API path achieves (V-4 + H-7 measurements).
3. A study needs node-only data (e.g. O6 needs to tag liquidations reliably).
4. The owner decides the latency advantage is worth it regardless (it's a GOAL §1 priority).

## 3. Non-goals

- Running a **validator**.
- An **archive** node (full history). We prune.
- The Hyper Foundation's non-validating node program (low-latency peering). Its requirements are staking 10,000 HYPE, maker rebate tier 1+ (> 0.5% of 14-day weighted maker volume), and 98% uptime as a public peer. Revisit if we ever qualify.
- Serving data to third parties.

## 4. Known facts (from the official repo, fetched 2026-09-26)

Source: <https://github.com/hyperliquid-dex/node>, unless noted otherwise.

| Fact | Value |
|---|---|
| Hardware | **16 vCPUs, 128 GB RAM, 500 GB SSD** |
| OS | **Ubuntu 24.04 only** |
| Location | "For lowest latency, run the node in **Tokyo, Japan**." |
| Ports | **4001 and 4002** (gossip) must be open to the public, or peers deprioritize the node |
| Chain config | `echo '{"chain": "Mainnet"}' > ~/visor.json` |
| Binary | `curl https://binaries.hyperliquid.xyz/Mainnet/hl-visor > ~/hl-visor && chmod a+x ~/hl-visor` |
| Run | `~/hl-visor run-non-validator [flags]`. `applied block X` in the logs means it's live. Finding a peer can take a while. |
| Mainnet peers | `~/override_gossip_config.json`: `{"root_node_ips": [{"Ip": "1.2.3.4"}], "chain": "Mainnet"}` (optionally `"try_new_peers": false`) |
| Seed IPs | Believed obtainable from the API with the `gossipRootIps` info request; community tooling filters peers by latency (e.g. < 80 ms) ⚠ verify (N-3) |
| Data dir | `~/hl/data` |
| Disk growth | "around **100 GB of logs per day**" with default settings. Must be archived or deleted. |
| Snapshots | every 10,000 blocks at `~/hl/data/periodic_abci_states/{date}/{height}.rmp` |
| EVM RPC | `--serve-eth-rpc` → `http://localhost:3001/evm` |
| Info server | `--serve-info` → `http://localhost:3001/info` |

Output flags:

| Flag | Output |
|---|---|
| `--write-trades` | `~/hl/data/node_trades/hourly/{date}/{hour}` |
| `--write-fills` | `~/hl/data/node_fills/hourly/{date}/{hour}` (API fills format) |
| `--write-order-statuses` | `~/hl/data/node_order_statuses/hourly/{date}/{hour}`: every L1 order status |
| `--write-raw-book-diffs` | `~/hl/data/node_raw_book_diffs/hourly/{date}/{hour}`: every L1 order diff |
| `--write-hip3-oracle-updates` | every HIP-3 deployer oracle update action |
| `--write-misc-events` | `~/hl/data/misc_events/hourly/{date}/{hour}` |
| `--batch-by-block` | one block per line instead of one event per line |
| `--stream-with-block-info` | events written as processed, with block metadata |
| `--disable-output-file-buffering` | flush every line immediately (**required for low latency tailing**) |
| `--replica-cmds-style` | `actions` \| `actions-and-responses` \| `recent-actions` |

Order book server (optional add-on): <https://github.com/hyperliquid-dex/order_book_server>, in Rust. It serves `l2book` (up to 100 levels), `trades`, and a new **`l4Book`** (full per-order snapshot + per-block diffs) over a local WS. It needs the node running with `--batch-by-block` + fills + order statuses + raw book diffs. Caveats from its README: "not written by the Hyperliquid Labs core team", no spot books, no untriggered trigger orders, and block batching adds "a few milliseconds".

## 5. Cost and overhead (honest)

| Item | Estimate | Status |
|---|---|---|
| Server (16 vCPU / 128 GB / ≥ 1 TB NVMe, Tokyo) | roughly **$300–1,000 / month** (bare metal at the low end, big-cloud VMs at the high end) | ⚠ N-1 gets real quotes |
| Disk churn | ~100 GB/day written ⇒ needs a pruning job; keep ≤ 3 days local unless shipped | known |
| Egress | shipping node data off-host is optional; keep only what research needs | — |
| Ops time | initial setup ~1–2 days; then updates, disk, peer health, and a monthly check | estimate |
| Risk | the node falls behind or stops ⇒ stale data. Mitigation: the bot keeps the public API as a fallback and the risk engine treats stale feeds as not-ready (SPEC-0001 §7) | design |

**Verdict:** moderate overhead, not trivial (128 GB RAM, 100 GB/day). It's worth it once §2 triggers fire, because it improves both **speed** (GOAL §1) and **data richness**.

## 6. Architecture

```
┌──────────────────────── node host (Tokyo, Ubuntu 24.04) ────────────────────────┐
│                                                                                   │
│  hl-visor run-non-validator  --serve-eth-rpc --serve-info                         │
│        --write-trades --write-fills --write-order-statuses --write-raw-book-diffs │
│        --write-hip3-oracle-updates --disable-output-file-buffering                │
│        │                     │                    │                               │
│        │ :3001/evm           │ :3001/info         │ ~/hl/data/node_*/hourly/…     │
│        ▼                     ▼                    ▼                               │
│   (localhost / private net only)          hl node-tap (inotify tail)              │
│                                              │           │                        │
│                                              ▼           ▼                        │
│                                   recorder envelopes   local low-latency feed     │
│                                   src = "hl-node"      (later: for the bot)       │
│  firewall: 4001/4002 public; everything else private                              │
└───────────────────────────────────────────────────────────────────────────────────┘
```

Placement decision for v1: the **recorder** and **node-tap** run on the node host (they read local files). The **bot** stays on its own host in the same region/AZ, and connects over the private network. N-8 measures whether co-locating the bot on the node host is worth it.

## 7. Integration points

| Consumer | Change | Task |
|---|---|---|
| SPEC-0008 R-9 (HyperEVM pool source) | Point it at `http://<node>:3001/evm`. If the node's RPC has no WS subscriptions ⚠ verify, poll `eth_blockNumber` every 50 ms locally and fetch new blocks. | N-7 |
| SPEC-0008 R-5 (REST snapshotter) | Optional: use `HttpInfo::with_base_url("http://<node>:3001")` for reads, keeping the public API as fallback. Confirm which request types the local server supports ⚠ verify. | N-7 |
| SPEC-0008 recorder | New source `hl-node`: node-tap wraps each new line from node output files in an envelope (`kind:"frame"`, `raw` = the line) with `t_ns` taken when inotify reports it. Segments go to `data/rec/mainnet/hl-node/…` as usual. | N-6 |
| SPEC-0008 research | New normalized tables from node data: `node_fills` (with liquidation info if present), `node_order_statuses`, `book_diffs`. Normalizer extension. | N-6 (format notes), research follow-up |
| Bot (M4+) | Later: a `MarketStream` implementation fed by node-tap (lowest latency), with the public WS as fallback. Specified in the engine/strategy spec, not here. | — |

## 8. Latency measurement (the justification)

N-8 runs the public WS recorder and the node-tap side by side on the same host for ≥ 24 h and compares, for identical events:

| Event | Matching key | Metric |
|---|---|---|
| Trades | `tid` (or hash + coin + px + sz) | `t_ns(public) − t_ns(node)` distribution |
| BBO changes | block height/time + coin | same |
| Own-order status (later, testnet) | `cloid` | same |

Report p50/p90/p99 per event type. If the node isn't faster, the node still has value for EVM RPC and data richness, but the bot keeps using the public API. Record the result in §14.

## 9. Operations

| Area | Requirement |
|---|---|
| Service | systemd unit for `hl-visor` (`Restart=always`); a separate unit for node-tap |
| Time | chrony enabled |
| Disk | pruning timer: delete `~/hl/data/node_*` hourly dirs older than `node_retain_hours` (default 48), **after** node-tap has consumed them (node-tap writes a checkpoint file). Alert at 80% disk. |
| Snapshots | keep only the latest few `periodic_abci_states` ⚠ verify which files are safe to delete (N-4) |
| Health metrics | block lag (node latest block time vs wall clock), peers connected, disk free, node-tap lag, RPC/info latency. Exported to Prometheus via a small exporter (N-5). |
| Upgrades | how `hl-visor` handles binary upgrades ⚠ verify (N-3); document the upgrade runbook |
| Runbook | a "Node" section in `RUNBOOK.md`: start/stop, resync, peer issues, disk full, fall back to the public API |

## 10. Security

- Only ports **4001/4002** are public. `3001` (RPC/info), node-tap, and the metrics ports bind to localhost or the private network, enforced by the host firewall (`ufw`/`nftables`).
- No private keys on the node host.
- Run as a non-root user; SSH by key only.

## 11. Acceptance criteria

- [ ] The node stays synced (block lag < 2 s) ≥ 99% of the time over 7 days.
- [ ] Disk usage stays stable under pruning for 7 days, with no manual intervention.
- [ ] SPEC-0008 R-9 records HyperEVM pool state from the local RPC at every block.
- [ ] N-8 latency comparison report exists, with a clear recommendation.
- [ ] The runbook section exists and has been rehearsed once (restart + resync).

## 12. Work breakdown

| ID | Title | Size | Depends on | Status |
|---|---|---|---|---|
| N-1 | Price and choose the host (≥ 2 quotes, Tokyo, spec per §4) | S | — | ☐ |
| N-2 | Provision: Ubuntu 24.04, user, firewall, chrony, disk layout | S | N-1 | ☐ |
| N-3 | Install `hl-visor`, mainnet gossip config (seed IPs), first sync; document upgrades | M | N-2 | ☐ |
| N-4 | Enable output flags + pruning timer; verify which snapshot files are safe to delete | S | N-3 | ☐ |
| N-5 | Health exporter + alerts (block lag, peers, disk, RPC latency) | M | N-3 | ☐ |
| N-6 | `hl node-tap`: inotify tail → recorder envelopes (`src:"hl-node"`), with a checkpoint | M | N-4, SPEC-0008 R-2 | ☐ |
| N-7 | Point SPEC-0008 R-9 (EVM) and optionally R-5 (info) at the node | S | N-3, SPEC-0008 R-9 | ☐ |
| N-8 | Latency comparison: node vs public WS (§8) | M | N-6, SPEC-0008 R-6 | ☐ |
| N-9 | Evaluate `order_book_server` (L4 book, deep L2) | M | N-4 | ☐ |
| N-10 | Runbook section + rehearsal | S | N-5 | ☐ |

Task details:

- **N-1:** Get ≥ 2 quotes (≥ 1 bare metal, ≥ 1 cloud) for Tokyo machines meeting §4 (prefer ≥ 1 TB NVMe for pruning headroom). Include monthly cost, setup fee, bandwidth terms, and measured RTT to `api.hyperliquid.xyz` (`hl probe latency`). *Done when:* §14 has the comparison and a recommendation for the owner to approve.
- **N-2:** Provision the approved host per §9–§10. *Done when:* `ufw status` (or equivalent) shows only 22 (restricted), 4001, and 4002 open publicly; chrony is synced.
- **N-3:** Install per §4, fetch seed IPs (verify `gossipRootIps`), write `override_gossip_config.json`, sync. Find out how visor upgrades work. *Done when:* the logs show `applied block`, and block lag < 2 s for 1 h; §14 records the seed-IP method and upgrade behavior with sources.
- **N-4:** Enable the §6 flag set; implement the pruning timer. *Done when:* 48 h of stable disk usage, with numbers in §14.
- **N-5:** Exporter + Prometheus alerts. *Done when:* metrics are visible, and killing the node triggers the block-lag alert.
- **N-6:** A Rust `node-tap` (in `crates/mev-recorder`, reusing `SegmentWriter`): follow new files and appended lines with inotify, stamp `t_ns`/`mono_ns`, write envelopes, and keep a checkpoint (`file`, `offset`) so restarts don't duplicate or skip data. Document each node file's line format in §14 from samples. *Done when:* tests cover rotation to a new hourly file, restart from checkpoint, and a partially written last line; a 1 h real run matches the node file line counts exactly.
- **N-7:** Config-only change in the recorder profile, plus polling if WS isn't supported. *Done when:* R-9's Done-when holds against the local node.
- **N-8:** Per §8. *Done when:* the report is in `research/reports/N8-node-latency.md` and summarized in §14.
- **N-9:** Build and run `order_book_server` against the node; compare its `l2book` with the public API `l2Book`; measure its latency the same way as N-8. *Done when:* a short report says adopt / don't adopt, with numbers.
- **N-10:** Write it, then rehearse a restart and a resync from snapshot. *Done when:* the rehearsal date is logged in §14.

## 13. Open questions

1. Monthly cost ceiling the owner is comfortable with.
2. Co-locating the bot on the node host (lowest latency, shared failure) vs a separate host (isolation). Decide after N-8.
3. Is a testnet node worth running first as a rehearsal? (Same software, but likely also heavy.)

## 14. Verified facts / results (filled in by N-tasks)

| Item | Value | Source / date | Task |
|---|---|---|---|
| Host quotes + choice | | | N-1 |
| Seed-IP method, upgrade behavior | | | N-3 |
| Disk usage with the flag set | | | N-4 |
| Node output line formats | | | N-6 |
| Node vs public latency | | | N-8 |
| order_book_server verdict | | | N-9 |
| Runbook rehearsal date | | | N-10 |
