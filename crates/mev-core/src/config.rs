//! Layered configuration, execution modes, and secrets (SPEC-0000 §6–§7).
//!
//! Precedence: built-in defaults → `config/default.toml` → `config/{HL_ENV}.toml`
//! → `HL_*` environment variables → CLI overrides.

use std::path::PathBuf;

use figment::{
    Figment,
    providers::{Env, Format, Serialized, Toml},
};
use rust_decimal::Decimal;
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

/// Strategy engine configuration (SPEC-0003).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StrategyConfig {
    /// Enabled strategy ids (see [`crate::config::StrategyConfig::default`]).
    pub enabled: Vec<String>,
    /// Edge buffer in bps subtracted from every trade's gross edge.
    pub min_edge_bps: u32,
    /// Max slippage (bps) used to price aggressive (`limit_px: None`) orders
    /// relative to the touch (SPEC-0010 §12).
    pub max_slippage_bps: u32,
    /// Delta-neutral funding/basis settings.
    pub funding: FundingSettings,
    /// Market-making settings.
    pub market_making: MmSettings,
}

/// Market-making strategy settings (SPEC-0003 §7).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MmSettings {
    /// Coins to quote.
    pub coins: Vec<String>,
    /// Levels per side.
    pub levels: u32,
    /// Half-spread to the first level, in bps.
    pub half_spread_bps: u32,
    /// Spacing between levels, in bps.
    pub level_step_bps: u32,
    /// Size per level, in base units.
    pub size_per_level: Decimal,
    /// Absolute inventory cap, in base units.
    pub max_inventory: Decimal,
    /// Maximum reservation skew at full inventory, in bps.
    pub max_skew_bps: u32,
    /// Pull quotes when the spread exceeds this, in bps.
    pub vol_pull_bps: u32,
    /// Replace quotes when the mid moves by at least this, in bps.
    pub refresh_bps: u32,
}

impl Default for MmSettings {
    fn default() -> Self {
        Self {
            coins: vec!["ETH".to_string()],
            levels: 3,
            half_spread_bps: 4,
            level_step_bps: 4,
            size_per_level: Decimal::new(5, 2),
            max_inventory: Decimal::ONE,
            max_skew_bps: 4,
            vol_pull_bps: 30,
            refresh_bps: 2,
        }
    }
}

impl Default for StrategyConfig {
    fn default() -> Self {
        Self {
            enabled: vec!["funding_basis".to_string()],
            min_edge_bps: 5,
            max_slippage_bps: 10,
            funding: FundingSettings::default(),
            market_making: MmSettings::default(),
        }
    }
}

/// Funding/basis strategy settings (SPEC-0003 §6).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FundingSettings {
    /// Perp coin to short.
    pub perp_coin: String,
    /// Spot pair name, resolved to a canonical coin at runtime.
    pub spot_pair: String,
    /// Spot token symbol used for balances and the paper account.
    pub spot_token: String,
    /// Target notional per leg, in USD.
    pub target_notional_usd: Decimal,
    /// Holding horizon used to project funding, in hours.
    pub horizon_hours: u32,
    /// Funding at or below (bps/hour) that counts toward the exit streak.
    pub exit_threshold_bps: u32,
    /// Consecutive hourly settlements below threshold before exiting.
    pub exit_after_hours: u32,
    /// Delta drift (bps of notional) that triggers a rebalance.
    pub rebalance_drift_bps: u32,
    /// Quote as maker (post-only) instead of taking.
    pub maker: bool,
}

impl Default for FundingSettings {
    fn default() -> Self {
        Self {
            perp_coin: "BTC".to_string(),
            spot_pair: "UBTC/USDC".to_string(),
            spot_token: "UBTC".to_string(),
            target_notional_usd: Decimal::from(1_000),
            horizon_hours: 24,
            exit_threshold_bps: 0,
            exit_after_hours: 3,
            rebalance_drift_bps: 100,
            maker: false,
        }
    }
}

/// Engine-side rate-budget settings (SPEC-0010 §12, SPEC-0004 K-5).
///
/// Hyperliquid's IP weight is shared across every client on the host, so the
/// engine uses only its configured `engine_ip_share`. Values are finite by
/// default; they are safety bounds rather than money limits, so `live` does not
/// require them to be set explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RateBudgetSettings {
    /// Total IP weight per minute the account may spend (venue limit).
    pub ip_weight_per_min: u32,
    /// The engine's share of that weight (its token-bucket capacity).
    pub engine_ip_share: u32,
    /// Address order capacity/refill per minute (from `userRateLimit`).
    pub address_per_min: u32,
    /// Minimum IP weight below which new places are rejected.
    pub ip_weight_min: u32,
    /// Minimum address budget below which new places are rejected.
    pub address_min: u32,
    /// Below this, even cancels are rejected (the hard floor).
    pub hard_floor: u32,
}

impl Default for RateBudgetSettings {
    fn default() -> Self {
        Self {
            ip_weight_per_min: 1_200,
            engine_ip_share: 1_200,
            address_per_min: 1_200,
            ip_weight_min: 100,
            address_min: 500,
            hard_floor: 1,
        }
    }
}

/// Pre-trade risk limits (SPEC-0004 §5). `None` disables a limit; `live` mode
/// refuses to start unless every limit is explicitly finite (SPEC-0004 K-1).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RiskSettings {
    /// Maximum notional for one order.
    pub max_order_notional_usd: Option<Decimal>,
    /// Maximum absolute per-coin position notional.
    pub max_position_notional_usd: Option<Decimal>,
    /// Maximum resting orders per coin.
    pub max_open_orders: Option<usize>,
    /// Maximum margin utilization (bps) before new risk is refused.
    pub max_margin_utilization_bps: Option<Decimal>,
    /// Maximum realized+unrealized daily loss in USD before trading halts.
    pub max_daily_loss_usd: Option<Decimal>,
    /// Maximum unhedged (single-leg) notional in USD (SPEC-0011 §9).
    pub max_unhedged_usd: Option<Decimal>,
    /// Engine-side rate-budget settings (SPEC-0010 §12 / SPEC-0004 K-5).
    pub rate_budget: RateBudgetSettings,
}

impl RiskSettings {
    /// Conservative finite limits applied to non-live modes, so `simulate` is
    /// bounded by default while `live` still requires explicit values
    /// (SPEC-0004 K-1). `None` fields are left untouched.
    pub fn conservative_defaults() -> Self {
        Self {
            max_order_notional_usd: Some(Decimal::from(2_500)),
            max_position_notional_usd: Some(Decimal::from(25_000)),
            max_open_orders: Some(50),
            max_margin_utilization_bps: Some(Decimal::from(5_000)),
            max_daily_loss_usd: Some(Decimal::from(500)),
            max_unhedged_usd: Some(Decimal::from(5_000)),
            rate_budget: RateBudgetSettings::default(),
        }
    }

    /// Fill only the unset fields from [`Self::conservative_defaults`].
    fn fill_defaults(&mut self) {
        let defaults = Self::conservative_defaults();
        self.max_order_notional_usd = self
            .max_order_notional_usd
            .or(defaults.max_order_notional_usd);
        self.max_position_notional_usd = self
            .max_position_notional_usd
            .or(defaults.max_position_notional_usd);
        self.max_open_orders = self.max_open_orders.or(defaults.max_open_orders);
        self.max_margin_utilization_bps = self
            .max_margin_utilization_bps
            .or(defaults.max_margin_utilization_bps);
        self.max_daily_loss_usd = self.max_daily_loss_usd.or(defaults.max_daily_loss_usd);
        self.max_unhedged_usd = self.max_unhedged_usd.or(defaults.max_unhedged_usd);
    }
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
    /// Path to the persisted, CLI-editable watchlist file.
    pub watchlist_path: PathBuf,
    /// SQLite database path.
    pub db_path: PathBuf,
    /// Dead-man's switch TTL in milliseconds.
    pub schedule_cancel_ttl_ms: u64,
    /// HTTP port for health/metrics.
    pub http_port: u16,
    /// Strategy engine settings.
    #[serde(default)]
    pub strategy: StrategyConfig,
    /// Pre-trade risk limits.
    #[serde(default)]
    pub risk: RiskSettings,
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
            watchlist_path: PathBuf::from(crate::watchlist::DEFAULT_PATH),
            db_path: PathBuf::from("data/hlbot.db"),
            schedule_cancel_ttl_ms: 120_000,
            http_port: 9090,
            strategy: StrategyConfig::default(),
            risk: RiskSettings::default(),
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
        } else {
            let persisted = crate::watchlist::load(&config.watchlist_path)?;
            if !persisted.is_empty() {
                config.watchlist = persisted;
            }
        }
        if let Some(db_path) = overrides.db_path {
            config.db_path = db_path;
        }

        // `simulate`/`observe` get conservative finite limits; `live` must set
        // every cap explicitly or `validate` refuses to start (K-1).
        if config.mode != Mode::Live {
            config.risk.fill_defaults();
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
            self.validate_live_limits()?;
        }
        Ok(())
    }

    /// Fail closed: every risk cap must be explicitly finite so no limit
    /// silently means "unlimited" on the real-money path (SPEC-0004 K-1).
    fn validate_live_limits(&self) -> Result<()> {
        for (name, value) in [
            ("max_order_notional_usd", self.risk.max_order_notional_usd),
            (
                "max_position_notional_usd",
                self.risk.max_position_notional_usd,
            ),
            (
                "max_margin_utilization_bps",
                self.risk.max_margin_utilization_bps,
            ),
            ("max_daily_loss_usd", self.risk.max_daily_loss_usd),
            ("max_unhedged_usd", self.risk.max_unhedged_usd),
        ] {
            match value {
                Some(v) if v > Decimal::ZERO => {}
                _ => {
                    return Err(Error::Config(format!(
                        "live mode requires a finite risk {name}"
                    )));
                }
            }
        }
        if self.risk.max_open_orders.filter(|cap| *cap > 0).is_none() {
            return Err(Error::Config(
                "live mode requires a finite risk max_open_orders".into(),
            ));
        }
        Ok(())
    }

    /// The configured agent private key, if any. Handle with care: never log
    /// the returned value.
    pub fn agent_key(&self) -> Option<&str> {
        self.agent_private_key
            .as_ref()
            .map(|secret| secret.expose_secret())
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
             watchlist_path: {watchlist_path}\n\
             db_path: {db_path}\n\
             schedule_cancel_ttl_ms: {ttl}\n\
             http_port: {port}\n\
             account_address: {account}\n\
             agent_private_key: {key_state}",
            network = self.network,
            mode = self.mode,
            autonomy = self.autonomy,
            watchlist = self.watchlist,
            watchlist_path = self.watchlist_path.display(),
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

    /// A `live`-shaped config that passes every gate *except* risk limits.
    fn live_without_risk_limits() -> Config {
        Config {
            mode: Mode::Live,
            account_address: Some("0x0000000000000000000000000000000000000000".into()),
            agent_private_key: Some(SecretString::from("0x".to_string() + &"1".repeat(64))),
            ..Config::default()
        }
    }

    #[test]
    fn live_refuses_unset_risk_limits() {
        // HL_LIVE_CONFIRM is required first; set it only for this test process.
        // SAFETY (2024 edition `unsafe` env): tests in this binary run in
        // parallel but none other sets this variable.
        unsafe { std::env::set_var("HL_LIVE_CONFIRM", "YES") };
        let config = live_without_risk_limits();
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("max_order_notional_usd"), "{err}");

        let mut with_limits = config.clone();
        with_limits.risk.max_order_notional_usd = Some(Decimal::from(1_000));
        with_limits.risk.max_position_notional_usd = Some(Decimal::from(10_000));
        with_limits.risk.max_margin_utilization_bps = Some(Decimal::from(5_000));
        with_limits.risk.max_open_orders = Some(10);
        with_limits.risk.max_daily_loss_usd = Some(Decimal::from(500));
        with_limits.risk.max_unhedged_usd = Some(Decimal::from(5_000));
        assert!(with_limits.validate().is_ok());
        unsafe { std::env::remove_var("HL_LIVE_CONFIRM") };
    }

    #[test]
    fn simulate_fills_conservative_finite_defaults() {
        let mut config = Config {
            mode: Mode::Simulate,
            ..Config::default()
        };
        config.risk.fill_defaults();
        for (name, set) in [
            (
                "max_order_notional_usd",
                config.risk.max_order_notional_usd.is_some(),
            ),
            (
                "max_position_notional_usd",
                config.risk.max_position_notional_usd.is_some(),
            ),
            ("max_open_orders", config.risk.max_open_orders.is_some()),
            (
                "max_margin_utilization_bps",
                config.risk.max_margin_utilization_bps.is_some(),
            ),
            (
                "max_daily_loss_usd",
                config.risk.max_daily_loss_usd.is_some(),
            ),
            ("max_unhedged_usd", config.risk.max_unhedged_usd.is_some()),
        ] {
            assert!(set, "{name} must have a conservative default");
        }
        // Explicit values are preserved.
        config.risk.max_order_notional_usd = Some(Decimal::from(42));
        config.risk.fill_defaults();
        assert_eq!(config.risk.max_order_notional_usd, Some(Decimal::from(42)));
    }

    #[test]
    fn default_risk_limits_are_unset_and_safe() {
        let config = Config::default();
        assert!(config.risk.max_order_notional_usd.is_none());
        assert!(config.risk.max_open_orders.is_none());
        assert_eq!(config.strategy.max_slippage_bps, 10);
    }

    #[test]
    fn rate_budget_defaults_are_finite_and_ordered() {
        let budget = RateBudgetSettings::default();
        assert!(budget.engine_ip_share > 0);
        assert!(budget.engine_ip_share <= budget.ip_weight_per_min);
        assert!(budget.ip_weight_min < budget.engine_ip_share);
        assert!(budget.address_min < budget.address_per_min);
        assert!(budget.hard_floor <= budget.ip_weight_min);
        // The engine share defaults to the full account weight and is settable.
        assert_eq!(Config::default().risk.rate_budget, budget);
    }
}
