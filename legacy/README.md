# Legacy — Ethereum Uniswap V2 MEV bot (retired)

This directory contains the original single-crate Ethereum mainnet MEV bot
(Uniswap V2 / Sushiswap V2 arbitrage). It is **not part of the build** and is
kept for reference only.

Depends on the deprecated `ethers-rs` and is intentionally excluded from the
Cargo workspace. Retained as reference for the HyperEVM work (SPEC-0005):

- `legacy/src/crossed_pair.rs` — constant-product optimal-input math
- `legacy/src/abi/` — Uniswap V2 ABIs
- `legacy/contract/` — original executor and flash-query Solidity contracts
- `legacy/examples/` — original exploratory binaries

See `specs/` for the current design.
