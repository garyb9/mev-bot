# mev-recorder

**Client/tooling layer.** Raw market-data recorder (never loads keys, never
places orders). Driven by `hl record` in `mev-bot`.

| Module | Purpose |
|---|---|
| `envelope` | the recorded line format (`Envelope`, `Kind`) |
| `segment` | per-connection zstd segment writer, rotation, manifest |
| `reader` | segment reader used by `inspect`, `verify`, and replay |
| `planner` | subscription plan and budget for Hyperliquid |
| `sources/` | reference sources: `cex` (Binance/Bybit), `deribit`, `hl_rest` snapshotter |
| `mount_guard` | refuses to record unless the required mount is real |

The on-disk format is frozen; see SPEC-0008 §5/§6. Bench: `benches/segment.rs`.
