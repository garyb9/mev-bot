//! Asset index/metadata registry and market validation (SPEC-0001 §6).
//!
//! `AssetMap` resolves a user-facing coin string to the numeric asset index,
//! size decimals, and leverage limits needed for subscriptions and (later)
//! order placement. It covers the default perp universe, builder-deployed
//! HIP-3 perp dexes (`dex:coin`), and spot pairs (`@index` / `PURR/USDC`).
//!
//! Note: the *asset id* used on the wire for HIP-3 orders is an encoded
//! `100_000 + 10_000 * dex_offset + index`; that encoding is applied by the
//! order builder (SPEC-0002). Here `index` is the within-universe position.

use std::collections::BTreeMap;

use hl_arb_core::error::{Error, Result};
use tracing::warn;

use crate::client::InfoApi;
use crate::types::{Meta, SpotMeta};

/// Whether a market is a perpetual or a spot pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarketKind {
    /// Perpetual futures (default dex or HIP-3).
    Perp,
    /// Spot pair.
    Spot,
}

/// A resolved, tradable market.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Market {
    /// Canonical symbol used for subscriptions and orders.
    pub coin: String,
    /// Human-readable name (`BTC`, `UBTC/USDC`).
    pub name: String,
    /// Perp or spot.
    pub kind: MarketKind,
    /// HIP-3 dex name, or `None` for the default perp dex / spot.
    pub dex: Option<String>,
    /// HIP-3 dex offset (1-based) used to encode the wire asset id; `None`
    /// for the default perp dex and spot.
    pub dex_offset: Option<u32>,
    /// Asset index within its universe (perp) or spot pair index.
    pub index: u32,
    /// Size decimals for order sizing.
    pub sz_decimals: u32,
    /// Maximum leverage (perps only).
    pub max_leverage: Option<u32>,
}

impl Market {
    /// The numeric asset id used on the wire for orders.
    ///
    /// Default perps use the universe index; spot uses `10000 + pair_index`;
    /// HIP-3 perps use `100000 + dex_offset * 10000 + index` (SPEC-0002 §6).
    pub fn asset_id(&self) -> u32 {
        match self.kind {
            MarketKind::Spot => 10_000 + self.index,
            MarketKind::Perp => match self.dex_offset {
                None => self.index,
                Some(offset) => 100_000 + offset * 10_000 + self.index,
            },
        }
    }
}

/// Registry of known markets, keyed by canonical coin.
#[derive(Debug, Clone, Default)]
pub struct AssetMap {
    markets: BTreeMap<String, Market>,
    aliases: BTreeMap<String, String>,
}

impl AssetMap {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Load perps (default dex), spot pairs, and optionally all HIP-3 dexes.
    pub async fn load<I: InfoApi>(info: &I, include_hip3: bool) -> Result<Self> {
        let mut map = AssetMap::new();
        map.insert_perp_dex(None, None, &info.meta().await?);
        map.insert_spot(&info.spot_meta().await?);
        if include_hip3 {
            // `perpDexs[0]` is the default dex (dropped during parsing); builder
            // dexes are 1-based, matching the wire asset-id encoding.
            for (i, dex) in info.perp_dexs().await?.into_iter().enumerate() {
                match info.meta_for(&dex.name).await {
                    Ok(meta) => {
                        map.insert_perp_dex(Some(&dex.name), Some(i as u32 + 1), &meta);
                    }
                    Err(err) => warn!(dex = %dex.name, error = %err, "skipping hip-3 dex metadata"),
                }
            }
        }
        Ok(map)
    }

    /// Insert every non-delisted asset of a perp universe. `dex_offset` is the
    /// 1-based HIP-3 dex offset (`None` for the default dex). Returns the count.
    pub fn insert_perp_dex(
        &mut self,
        dex: Option<&str>,
        dex_offset: Option<u32>,
        meta: &Meta,
    ) -> usize {
        let mut inserted = 0;
        for (index, asset) in meta.universe.iter().enumerate() {
            if asset.is_delisted {
                continue;
            }
            // HIP-3 `meta` responses already carry dex-prefixed names
            // (e.g. `xyz:TSLA`); only prefix bare names.
            let coin = if asset.name.contains(':') {
                asset.name.clone()
            } else {
                match dex {
                    Some(dex) => format!("{dex}:{}", asset.name),
                    None => asset.name.clone(),
                }
            };
            self.insert(Market {
                coin,
                name: asset.name.clone(),
                kind: MarketKind::Perp,
                dex: dex.map(str::to_owned),
                dex_offset,
                index: index as u32,
                sz_decimals: asset.sz_decimals,
                max_leverage: Some(asset.max_leverage),
            });
            inserted += 1;
        }
        inserted
    }

    /// Insert every spot pair, aliasing both `@index` and the pair name.
    pub fn insert_spot(&mut self, spot: &SpotMeta) -> usize {
        let mut inserted = 0;
        for pair in &spot.universe {
            let base_token = spot
                .tokens
                .iter()
                .find(|token| token.index == pair.tokens[0]);
            let quote_token = spot
                .tokens
                .iter()
                .find(|token| token.index == pair.tokens[1]);
            let sz_decimals = base_token.map(|token| token.sz_decimals).unwrap_or(0);
            let canonical = spot_canonical(pair.index, &pair.name);
            self.insert(Market {
                coin: canonical.clone(),
                name: pair.name.clone(),
                kind: MarketKind::Spot,
                dex: None,
                dex_offset: None,
                index: pair.index,
                sz_decimals,
                max_leverage: None,
            });
            self.aliases.insert(pair.name.clone(), canonical.clone());
            self.aliases
                .insert(format!("@{}", pair.index), canonical.clone());
            // Human pair name built from token symbols, e.g. `UBTC/USDC`.
            if let (Some(base), Some(quote)) = (base_token, quote_token) {
                self.aliases
                    .insert(format!("{}/{}", base.name, quote.name), canonical);
            }
            inserted += 1;
        }
        inserted
    }

    /// Number of markets (aliases excluded).
    pub fn len(&self) -> usize {
        self.markets.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.markets.is_empty()
    }

    /// Look up a market by canonical coin or alias.
    pub fn get(&self, coin: &str) -> Option<&Market> {
        if let Some(market) = self.markets.get(coin) {
            return Some(market);
        }
        self.aliases
            .get(coin)
            .and_then(|canonical| self.markets.get(canonical))
    }

    /// Iterate markets in canonical order.
    pub fn iter(&self) -> impl Iterator<Item = &Market> {
        self.markets.values()
    }

    fn insert(&mut self, market: Market) {
        self.aliases
            .insert(market.coin.clone(), market.coin.clone());
        self.markets.insert(market.coin.clone(), market);
    }
}

/// Canonical symbol for a spot pair: `PURR/USDC` by name, others `@index`.
fn spot_canonical(index: u32, name: &str) -> String {
    if name == "PURR/USDC" {
        name.to_string()
    } else {
        format!("@{index}")
    }
}

/// Validates coin strings against an [`AssetMap`] before subscription.
#[derive(Debug, Clone)]
pub struct MarketSelector {
    map: AssetMap,
}

impl MarketSelector {
    /// Wrap an asset map.
    pub fn new(map: AssetMap) -> Self {
        Self { map }
    }

    /// The underlying registry.
    pub fn asset_map(&self) -> &AssetMap {
        &self.map
    }

    /// Resolve a single coin, trying case variants (`btc` → `BTC`,
    /// `xyz:tsla` → `xyz:TSLA`).
    pub fn resolve(&self, coin: &str) -> Result<Market> {
        for candidate in case_variants(coin) {
            if let Some(market) = self.map.get(&candidate) {
                return Ok(market.clone());
            }
        }
        Err(Error::Config(format!("unknown market `{coin}`")))
    }

    /// Resolve a list of coins, failing fast on the first unknown entry and
    /// de-duplicating by canonical symbol while preserving order.
    pub fn resolve_all(&self, coins: &[String]) -> Result<Vec<Market>> {
        let mut resolved: Vec<Market> = Vec::with_capacity(coins.len());
        for coin in coins {
            let market = self.resolve(coin)?;
            if !resolved.iter().any(|m| m.coin == market.coin) {
                resolved.push(market);
            }
        }
        Ok(resolved)
    }
}

/// Case variants to try for a user-entered coin string.
fn case_variants(coin: &str) -> Vec<String> {
    let mut variants = vec![coin.to_string()];
    match coin.split_once(':') {
        Some((dex, name)) => variants.push(format!("{dex}:{}", name.to_uppercase())),
        None => variants.push(coin.to_uppercase()),
    }
    variants
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AssetMeta, SpotPair, SpotToken};

    fn perp_meta() -> Meta {
        Meta {
            universe: vec![
                AssetMeta {
                    name: "BTC".into(),
                    sz_decimals: 5,
                    max_leverage: 40,
                    is_delisted: false,
                    only_isolated: false,
                },
                AssetMeta {
                    name: "OLD".into(),
                    sz_decimals: 2,
                    max_leverage: 10,
                    is_delisted: true,
                    only_isolated: false,
                },
            ],
        }
    }

    fn spot_meta() -> SpotMeta {
        SpotMeta {
            universe: vec![
                SpotPair {
                    name: "PURR/USDC".into(),
                    index: 0,
                    tokens: [0, 1],
                },
                // The live API reports the pair `name` as `@index`, not a
                // human name; the human alias comes from the token symbols.
                SpotPair {
                    name: "@1".into(),
                    index: 1,
                    tokens: [2, 1],
                },
            ],
            tokens: vec![
                SpotToken {
                    name: "PURR".into(),
                    index: 0,
                    sz_decimals: 1,
                },
                SpotToken {
                    name: "USDC".into(),
                    index: 1,
                    sz_decimals: 8,
                },
                SpotToken {
                    name: "UBTC".into(),
                    index: 2,
                    sz_decimals: 6,
                },
            ],
        }
    }

    fn map() -> AssetMap {
        let mut map = AssetMap::new();
        map.insert_perp_dex(None, None, &perp_meta());
        map.insert_perp_dex(Some("xyz"), Some(1), &perp_meta());
        map.insert_spot(&spot_meta());
        map
    }

    #[test]
    fn skips_delisted_and_indexes_perps() {
        let map = map();
        assert_eq!(map.get("BTC").unwrap().index, 0);
        assert_eq!(map.get("BTC").unwrap().sz_decimals, 5);
        assert!(map.get("OLD").is_none());
    }

    #[test]
    fn wire_asset_ids_follow_conventions() {
        let map = map();
        assert_eq!(map.get("BTC").unwrap().asset_id(), 0);
        assert_eq!(map.get("xyz:BTC").unwrap().asset_id(), 100_000 + 10_000);
        assert_eq!(map.get("@1").unwrap().asset_id(), 10_001);
    }

    #[test]
    fn hip3_coins_are_dex_qualified() {
        let map = map();
        let btc = map.get("xyz:BTC").unwrap();
        assert_eq!(btc.dex.as_deref(), Some("xyz"));
        assert_eq!(btc.coin, "xyz:BTC");
        assert!(map.get("BTC").unwrap().dex.is_none());
    }

    #[test]
    fn hip3_already_prefixed_names_are_not_doubled() {
        let meta = Meta {
            universe: vec![AssetMeta {
                name: "xyz:TSLA".into(),
                sz_decimals: 3,
                max_leverage: 20,
                is_delisted: false,
                only_isolated: false,
            }],
        };
        let mut map = AssetMap::new();
        map.insert_perp_dex(Some("xyz"), Some(1), &meta);
        let tsla = map.get("xyz:TSLA").expect("resolves once");
        assert_eq!(tsla.coin, "xyz:TSLA");
        assert_eq!(tsla.dex.as_deref(), Some("xyz"));
        assert!(map.get("xyz:xyz:TSLA").is_none());
    }

    #[test]
    fn spot_pairs_resolve_by_human_name_and_index() {
        let map = map();
        assert_eq!(map.get("PURR/USDC").unwrap().coin, "PURR/USDC");
        // `UBTC/USDC` is an alias built from the base/quote token symbols.
        assert_eq!(map.get("UBTC/USDC").unwrap().coin, "@1");
        assert_eq!(map.get("@1").unwrap().coin, "@1");
        assert_eq!(map.get("@1").unwrap().sz_decimals, 6);
        assert_eq!(map.get("@1").unwrap().kind, MarketKind::Spot);
    }

    #[test]
    fn selector_resolves_human_spot_pair() {
        let selector = MarketSelector::new(map());
        let spot = selector.resolve("ubtc/usdc").unwrap();
        assert_eq!(spot.coin, "@1");
    }

    #[test]
    fn selector_resolves_case_and_dedups() {
        let selector = MarketSelector::new(map());
        let coins = vec!["btc".to_string(), "BTC".to_string(), "xyz:btc".to_string()];
        let resolved = selector.resolve_all(&coins).unwrap();
        assert_eq!(
            resolved.iter().map(|m| m.coin.as_str()).collect::<Vec<_>>(),
            vec!["BTC", "xyz:BTC"]
        );
    }

    #[test]
    fn selector_rejects_unknown_market() {
        let selector = MarketSelector::new(map());
        let err = selector.resolve("NOPE").unwrap_err();
        assert!(err.to_string().contains("unknown market `NOPE`"));
    }
}
