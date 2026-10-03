# ADR-0001 — Hyperliquid client backend selection

**Status:** Accepted (provisional) · **Date:** 2026-09-26 · **Spec:** SPEC-0001 §9–§10

## Context

SPEC-0001 requires a backend-swappable HyperCore client. Three candidates were
mandated for evaluation:

| ID | Backend |
|---|---|
| A | `hyperliquid_rust_sdk` 0.6 (official) |
| B | custom `hl-arb-client` (Alloy + a WebSocket transport) |
| C | Alloy-native community SDK — `hypersdk` 0.2.16 |

The trait boundary (`InfoApi`, `MarketStream`) keeps the choice reversible.

## Measurements

### Dependency hygiene & clean-build footprint

Measured on rustc/cargo 1.98.1, Linux x86_64, 28 vCPU, isolated crates with a
stub `main`. `dep count` = `cargo tree --edges normal | wc -l`. Build time is a
clean `cargo build --release`.

| ID | Resolves | Build (s) | dep count | `target/` | `ethers`? | Key versions |
|---|---|---|---|---|---|---|
| A official | yes | 37.7 | 807 | 925 MB | **yes, 2.0.14 (archived)** | `tokio-tungstenite` 0.20.1, `reqwest` 0.11.27 |
| B custom (tokio-tungstenite 0.30) | yes | 10.3 | 258 | 234 MB | no | `rustls` 0.23.45, `reqwest` 0.12.28 |
| B′ fastwebsockets 0.10 (transport only) | yes | 17.5 | 117 | 225 MB | no | `rustls` 0.23.45, no TLS bundled |
| C hypersdk 0.2.16 | yes | 48.6 | 854 | 769 MB | no | `alloy` 2.5.0, edition 2024, MSRV 1.94.1 |

Release-binary sizes were uninformative (~450.7 KB for all; the stub `main`
lets LTO dead-code-eliminate the graph), so they are excluded.

### WS decode throughput (candidate B, typed decode)

`cargo bench -p hl-arb-client --bench decode -- --quick`, recorded live frames
(`crates/hl-arb-client/benches/fixtures`), compared against an untyped
`serde_json::Value` parse of the same frames:

| Channel | Typed decode | per frame | Throughput | `Value` parse | ratio |
|---|---|---|---|---|---|
| `l2Book` (28) | 258.9 µs | 9.25 µs | ~108k/s | 258.5 µs | 1.0× |
| `trades` (55) | 186.1 µs | 3.38 µs | ~296k/s | 176.9 µs | 1.05× |
| `activeAssetCtx` (120) | 130.7 µs | 1.09 µs | ~917k/s | 109.7 µs | 1.19× |
| `allMids` (6) | 2.093 ms | 349 µs | ~2.9k/s | 964 µs | **2.17×** |

Takeaways:

- Decode cost is dominated by serde/`Decimal`, **not** by WebSocket framing
  (frame counts are low; the venue pushes ~1 book update per few seconds per
  coin). Swapping the transport to `fastwebsockets` would not move these
  numbers.
- `allMids` is the outlier: parsing ~1.1k mid prices into `Decimal` is 2.2× the
  untyped parse. If it ever matters, keep mids as raw strings/lazily parse, or
  use `simd-json`. Not a blocker at current cadence.

### Data completeness & correctness (disqualifiers)

- **A (official) cannot express required data.** Source inspection: no HIP-3
  `dex` parameter anywhere, no `bbo` subscription, no perp `metaAndAssetCtxs`.
  Our watchlist and funding strategy need HIP-3 dex-qualified markets; the
  official SDK cannot subscribe to `xyz:TSLA`. It also depends on
  `ethers` 2.x, which upstream **archived** (last push 2024-09-23), violating
  the "no deprecated `ethers`" goal. Last release 2025-05-16; feature-frozen.
- **C (hypersdk) is correct and complete** — full WS set incl. `bbo`,
  perp `metaAndAssetCtxs`, and first-class HIP-3 (`perp_dexs`, `dex` params).
  Costs: MPL-2.0 (file-level copyleft), alloy `^2` → effective MSRV 1.94.1
  (vs our declared 1.90), and a large graph (854).
- **B (custom)** already implements REST `/info` (+ HIP-3 dex params), the WS
  stream with reconnect/heartbeat, and the local state/staleness model. No
  deprecated deps; smallest graph after fastwebsockets.

## Measured (SPEC-0002 H-7, 2026-09-28)

Sign and local submit latency, measured with the new
`crates/hl-arb-client/benches/sign.rs` criterion bench. It exercises the same
path `WriteCore::prepare` runs — the `hl_sign_seconds` stage: order build +
msgpack + EIP-712 sign — plus a localhost mock-WS `post` round trip.

- **Machine:** WSL2 (kernel `6.18.33.2-microsoft-standard-WSL2`), Intel(R)
  Core(TM) i7-14700K, 28 vCPU.
- **Command:** `cargo bench -p hl-arb-client --bench sign -- --warm-up-time 1
  --measurement-time 3 --sample-size 100`. CI quick mode:
  `cargo bench -p hl-arb-client --bench sign -- --quick`.
- Release profile (`lto = "fat"`), 100 samples. The mean equals criterion's own
  point estimate. The p50/p99 columns are the p50/p99 of criterion's
  **per-sample means**, not a strict per-iteration p99 (criterion does not
  expose one).

| Bench | Mean | p50 | p99 (per-sample means) | Budget ([GOAL §5.2](../../docs/GOAL.md)) | Verdict |
|---|---|---|---|---|---|
| `sign/one_order` | 26.8 µs | 26.6 µs | 30.4 µs | p50 ≤ 150 µs, p99 ≤ 500 µs | **PASS**, ~6×/16× margin |
| `sign/batch_10` | 28.4 µs | 28.3 µs | 29.7 µs | (same, per action) | PASS |
| `ws_post/round_trip` | 44.3 µs | 43.5 µs | 72.1 µs | — (localhost mock) | n/a |

`ws_post/round_trip` is an in-process mock venue on `127.0.0.1` (no network),
so it prices sign + JSON + WS framing + loopback, not the real venue RTT. This
is the WSL2 dev host, not the (still pending) reference host
(SPEC-0010 §23 Q-E12-Reference-Host); the numbers should be re-run there.

## Not yet measured (pending, explicitly out of scope today)

- **End-to-end submit** wall time to the real venue (testnet): submit→ack needs
  a funded, agent-approved testnet account (SPEC-0002 H-10). Runtime
  `hl_submit_ack_seconds{transport}` is now recorded and will fill from the
  testnet round-trip.
- **Reconnect** time-from-drop under a forced disconnect (needs a local mock
  WS server; our client already reconnects with capped backoff).

The §15 sign-budget acceptance item is met on the dev host (WSL2, i7-14700K);
it is pending the reference host. The decision rule (p99 on decode/sign/submit)
is now satisfied on decode and sign.

## Decision

1. **Keep the custom `hl-arb-client` as the default backend (candidate B).**
   Rationale: it is the only backend that is simultaneously dependency-clean
   (no `ethers`), already correct for our data needs (HIP-3, BBO, perp ctx,
   staleness), permissively licensed (MIT), and lowest-footprint. Decode
   profiling shows framing is not the bottleneck, so nothing is gained by
   swapping to `fastwebsockets` now.
2. **Disqualify A (official).** Archived `ethers` 2.x + missing HIP-3/BBO/perp
   context make it unable to meet SPEC-0001/0003 requirements today.
3. **Retain C (`hypersdk`) as the reference second implementation** behind the
   trait boundary. Revisit if/when we want a batteries-included execution path
   and can accept MPL-2.0 + MSRV 1.94.1; it is the strongest SDK fallback.
4. **Do not adopt `fastwebsockets` yet.** Offer it as an optional transport
   only if a future profile shows framing (not JSON) is hot; note it does not
   bundle TLS (we would supply `tokio-rustls` + rustls provider).
5. If MSRV matters, the workspace stays at 1.90 because the chosen path adds no
   dep above it. (fastwebsockets/no; hypersdk would force 1.94.1.)

## Consequences

- Default remains `tokio-tungstenite` 0.30; trait boundary unchanged.
- Follow-up landed: the sign/submit benchmarks (SPEC-0002 H-7) are in
  `crates/hl-arb-client/benches/sign.rs` and the numbers are above.
- Remaining optional follow-ups: `allMids` lazy/`simd-json` decoding; a mock-WS
  reconnect harness; and a real testnet submit round-trip (SPEC-0002 H-10) once
  a funded agent wallet exists.
