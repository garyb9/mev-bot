# hl-arb-client

**Client layer.** Hyperliquid HyperCore REST and WebSocket client behind
backend-swappable traits.

| Module | Purpose |
|---|---|
| `client`, `types`, `assets` | `/info` client, wire types, asset metadata |
| `market`, `raw_ws`, `ws` | market selection and state, raw WS connection, market stream |
| `exchange`, `ws_exchange` | `/exchange` HTTP and WS-post order submission |
| `order`, `cloid` | order builder, client order ids |
| `signing`, `nonce` | EIP-712 agent signing, nonce manager |
| `deadman` | dead-man's switch (`scheduleCancel`) |

Specs: SPEC-0001, SPEC-0002, ADR 0001. Benches in `benches/` (decode, sign);
`examples/capture.rs` captures raw frames.
