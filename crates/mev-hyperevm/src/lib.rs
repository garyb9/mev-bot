//! HyperEVM (chain 999) support: DEX pool sources, revm simulation, and the
//! on-chain executor bindings.
//!
//! Deferred until after the HyperCore path ships; see SPEC-0005.

/// HyperEVM chain IDs.
pub mod chain {
    /// HyperEVM mainnet chain id.
    pub const MAINNET: u64 = 999;
    /// HyperEVM testnet chain id.
    pub const TESTNET: u64 = 998;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chain_ids() {
        assert_eq!(chain::MAINNET, 999);
        assert_eq!(chain::TESTNET, 998);
    }
}
