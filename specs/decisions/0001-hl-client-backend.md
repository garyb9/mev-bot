# ADR-0001 — Hyperliquid client backend selection

**Status:** Accepted (provisional) · **Date:** 2026-09-26 · **Spec:** SPEC-0001 §9–§10

## Context

SPEC-0001 requires a backend-swappable HyperCore client. Three candidates were
mandated for evaluation:

| ID | Backend |
|---|---|
| A | `hyperliquid_rust_sdk` 0.6 (official) |
| B | custom `mev-hl-client` (Alloy + a WebSocket transport) |
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

`cargo bench -p mev-hl-client --bench decode -- --quick`, recorded live frames
(`crates/mev-hl-client/benches/fixtures`), compared against an untyped
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

## Not yet measured (pending, explicitly out of scope today)

These require an agent/API wallet and funded testnet account, which are not
available yet (SPEC-0002 / user-provided keys):

- EIP-712 phantom-agent **sign latency** (build + hash + sign).
- **End-to-end submit** wall time (build → sign → POST `/exchange`, testnet).
- **Reconnect** time-from-drop under a forced disconnect (needs a local mock
  WS server; our client already reconnects with capped backoff).

They will be added to this record when keys/testnet funds exist; the decision
rule (p99 on decode/sign/submit) is currently satisfied only on decode.

## Decision

1. **Keep the custom `mev-hl-client` as the default backend (candidate B).**
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
- Optional follow-ups: `allMids` lazy/`simd-json` decoding; a mock-WS reconnect
  harness; sign/submit benchmarks once keys land.
