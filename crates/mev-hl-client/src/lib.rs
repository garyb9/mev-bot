//! Hyperliquid (HyperCore) client.
//!
//! Owns market data (SPEC-0001) and execution (SPEC-0002) behind
//! backend-swappable traits. Implementations land in later milestones.

/// Network selection for endpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Network {
    /// Mainnet.
    Mainnet,
    /// Testnet.
    Testnet,
}

impl Network {
    /// REST base URL for the network.
    pub const fn rest_url(self) -> &'static str {
        match self {
            Network::Mainnet => "https://api.hyperliquid.xyz",
            Network::Testnet => "https://api.hyperliquid-testnet.xyz",
        }
    }

    /// WebSocket URL for the network.
    pub const fn ws_url(self) -> &'static str {
        match self {
            Network::Mainnet => "wss://api.hyperliquid.xyz/ws",
            Network::Testnet => "wss://api.hyperliquid-testnet.xyz/ws",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_are_https_and_wss() {
        assert!(Network::Mainnet.rest_url().starts_with("https://"));
        assert!(Network::Mainnet.ws_url().starts_with("wss://"));
        assert!(Network::Testnet.rest_url().starts_with("https://"));
        assert!(Network::Testnet.ws_url().starts_with("wss://"));
    }
}
