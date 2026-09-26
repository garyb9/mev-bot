//! Layered configuration, execution modes, and secrets (SPEC-0000 §6–§7).
//!
//! Precedence: built-in defaults → `config/default.toml` → `config/{HL_ENV}.toml`
//! → `HL_*` environment variables → CLI overrides.

use std::path::PathBuf;

use figment::{
    Figment,
    providers::{Env, Format, Serialized, Toml},
};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// The venue network to connect to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    /// Hyperliquid mainnet.
    Mainnet,
    /// Hyperliquid testnet.
    Testnet,
}

impl Network {
    /// REST base URL.
    pub const fn rest_url(self) -> &'static str {
        match self {
            Network::Mainnet => "https://api.hyperliquid.xyz",
            Network::Testnet => "https://api.hyperliquid-testnet.xyz",
        }
    }

    /// WebSocket URL.
    pub const fn ws_url(self) -> &'static str {
        match self {
            Network::Mainnet => "wss://api.hyperliquid.xyz/ws",
            Network::Testnet => "wss://api.hyperliquid-testnet.xyz/ws",
        }
    }
}

/// Execution mode (SPEC-0000 §6). Default is the safe `Observe`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Connect and build state; never place orders. No keys required.
    Observe,
    /// Run strategies and simulate; never submit.
    Simulate,
    /// Submit orders; requires keys and explicit confirmation.
    Live,
}

/// Whether the engine trades autonomously or asks for confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Autonomy {
    /// The engine submits on its own once risk checks pass (default).
    Auto,
    /// The engine proposes; a human confirms before submission.
    Confirm,
}

/// Resolved, validated configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Network.
    pub network: Network,
    /// Execution mode.
    pub mode: Mode,
    /// Execution autonomy.
    pub autonomy: Autonomy,
    /// Markets to trade.
    pub watchlist: Vec<String>,
    /// SQLite database path.
    pub db_path: PathBuf,
    /// Dead-man's switch TTL in milliseconds.
    pub schedule_cancel_ttl_ms: u64,
    /// HTTP port for health/metrics.
    pub http_port: u16,
    /// Master account address (required for `live`).
    pub account_address: Option<String>,
    /// Agent wallet private key (required for `live`); never serialized or logged.
    #[serde(default, skip_serializing)]
    pub agent_private_key: Option<SecretString>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            network: Network::Mainnet,
            mode: Mode::Observe,
            autonomy: Autonomy::Auto,
            watchlist: vec!["BTC".to_string(), "ETH".to_string(), "SOL".to_string()],
            db_path: PathBuf::from("data/hlbot.db"),
            schedule_cancel_ttl_ms: 30_000,
            http_port: 9090,
            account_address: None,
            agent_private_key: None,
        }
    }
}

/// CLI-provided overrides applied after file/env layering.
#[derive(Debug, Default, Clone)]
pub struct ConfigOverrides {
    /// Execution mode.
    pub mode: Option<Mode>,
    /// Network.
    pub network: Option<Network>,
    /// Watchlist override.
    pub coins: Option<Vec<String>>,
    /// DB path override.
    pub db_path: Option<PathBuf>,
}

impl Config {
    /// Load, layer, and validate configuration.
    pub fn load(overrides: ConfigOverrides) -> Result<Self> {
        let env_name = std::env::var("HL_ENV").unwrap_or_else(|_| "default".to_string());
        let figment = Figment::new()
            .merge(Serialized::defaults(Config::default()))
            .merge(Toml::file("config/default.toml"))
            .merge(Toml::file(format!("config/{env_name}.toml")))
            .merge(Env::prefixed("HL_").split("__"));

        let mut config: Config = figment
            .extract()
            .map_err(|e| Error::Config(e.to_string()))?;

        if let Some(mode) = overrides.mode {
            config.mode = mode;
        }
        if let Some(network) = overrides.network {
            config.network = network;
        }
        if let Some(coins) = overrides.coins {
            config.watchlist = coins;
        }
        if let Some(db_path) = overrides.db_path {
            config.db_path = db_path;
        }

        config.validate()?;
        Ok(config)
    }

    /// Validate invariants, including live-mode key gating.
    pub fn validate(&self) -> Result<()> {
        if self.watchlist.is_empty() {
            return Err(Error::Config("watchlist is empty".into()));
        }
        for coin in &self.watchlist {
            if coin.trim().is_empty() {
                return Err(Error::Config("watchlist contains an empty coin".into()));
            }
        }
        if let Some(addr) = &self.account_address
            && !is_hex_address(addr)
        {
            return Err(Error::Config(format!("invalid account address: {addr}")));
        }

        if self.mode == Mode::Live {
            let confirmed = std::env::var("HL_LIVE_CONFIRM")
                .map(|v| v == "YES")
                .unwrap_or(false);
            if !confirmed {
                return Err(Error::Config(
                    "live mode requires HL_LIVE_CONFIRM=YES".into(),
                ));
            }
            match &self.agent_private_key {
                Some(key) if !key.expose_secret().is_empty() => {}
                _ => {
                    return Err(Error::Config(
                        "live mode requires HL_AGENT_PRIVATE_KEY".into(),
                    ));
                }
            }
            if self.account_address.is_none() {
                return Err(Error::Config(
                    "live mode requires HL_ACCOUNT_ADDRESS".into(),
                ));
            }
        }
        Ok(())
    }

    /// A human-readable summary with secrets redacted.
    pub fn summary(&self) -> String {
        let key_state = match &self.agent_private_key {
            Some(_) => "set (redacted)",
            None => "unset",
        };
        let account = self.account_address.as_deref().unwrap_or("unset");
        format!(
            "network: {network:?}\n\
             mode: {mode:?}\n\
             autonomy: {autonomy:?}\n\
             watchlist: {watchlist:?}\n\
             db_path: {db_path}\n\
             schedule_cancel_ttl_ms: {ttl}\n\
             http_port: {port}\n\
             account_address: {account}\n\
             agent_private_key: {key_state}",
            network = self.network,
            mode = self.mode,
            autonomy = self.autonomy,
            watchlist = self.watchlist,
            db_path = self.db_path.display(),
            ttl = self.schedule_cancel_ttl_ms,
            port = self.http_port,
        )
    }
}

/// True if `s` looks like a 42-char `0x`-prefixed hex address.
pub fn is_hex_address(s: &str) -> bool {
    let Some(rest) = s.strip_prefix("0x") else {
        return false;
    };
    rest.len() == 40 && rest.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_validation() {
        assert!(is_hex_address("0x0000000000000000000000000000000000000000"));
        assert!(!is_hex_address("0x0"));
        assert!(!is_hex_address("0000000000000000000000000000000000000000"));
        assert!(!is_hex_address(
            "0xZZ00000000000000000000000000000000000000"
        ));
    }

    #[test]
    fn endpoints() {
        assert!(Network::Mainnet.rest_url().starts_with("https://"));
        assert!(Network::Mainnet.ws_url().starts_with("wss://"));
        assert!(Network::Testnet.ws_url().starts_with("wss://"));
    }

    #[test]
    fn default_is_safe() {
        let config = Config::default();
        assert_eq!(config.mode, Mode::Observe);
        assert_eq!(config.autonomy, Autonomy::Auto);
        config.validate().expect("default config is valid");
    }

    #[test]
    fn live_requires_confirmation() {
        let config = Config {
            mode: Mode::Live,
            ..Config::default()
        };
        // No HL_LIVE_CONFIRM, no keys: must fail.
        assert!(config.validate().is_err());
    }
}
