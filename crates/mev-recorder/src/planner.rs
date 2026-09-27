//! Hyperliquid subscription planner (SPEC-0008 §7.3–§7.4).
//!
//! The planner turns a recording profile into a deterministic [`Plan`]: a list
//! of connections, each with an ordered list of subscriptions. It performs no
//! I/O — the caller resolves the universe metadata (an [`AssetMap`],
//! [`SpotMeta`], and the day-notional volumes needed by `*:top:N`) and passes
//! it in.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::time::Duration;

use mev_hl_client::assets::{AssetMap, Market, MarketKind};
use mev_hl_client::types::{MetaAndAssetCtxs, SpotMeta};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use thiserror::Error;

/// Maximum subscribe messages (including pings) per process per second
/// (SPEC-0008 §7.4 step 6).
pub const MAX_SUBSCRIBE_MSGS_PER_SEC: u32 = 20;

/// Minimum spacing between new WS connections (SPEC-0008 §7.4 step 7).
pub const MIN_NEW_CONN_INTERVAL: Duration = Duration::from_secs(3);

/// The streams the HL WS recorder subscribes to (SPEC-0008 §7.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Stream {
    /// Best bid/offer updates.
    Bbo,
    /// Public trades.
    Trades,
    /// Funding/mark/oracle/OI updates.
    ActiveAssetCtx,
    /// All mid prices (main dex and per HIP-3 dex).
    AllMids,
    /// L2 book snapshots.
    L2Book,
}

impl Stream {
    /// The wire channel/type name.
    pub fn as_str(self) -> &'static str {
        match self {
            Stream::Bbo => "bbo",
            Stream::Trades => "trades",
            Stream::ActiveAssetCtx => "activeAssetCtx",
            Stream::AllMids => "allMids",
            Stream::L2Book => "l2Book",
        }
    }

    /// Recording priority (SPEC-0008 §7.2); lower is more important.
    pub fn priority(self) -> u8 {
        match self {
            Stream::Bbo => 1,
            Stream::Trades | Stream::ActiveAssetCtx => 2,
            Stream::AllMids | Stream::L2Book => 3,
        }
    }
}

/// One planned subscription.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Subscription {
    /// The stream to subscribe to.
    pub stream: Stream,
    /// Coin (dex-qualified for HIP-3), or `None` for `allMids`.
    pub coin: Option<String>,
    /// HIP-3 dex for an `allMids` subscription; `None` otherwise.
    pub dex: Option<String>,
}

impl Subscription {
    /// The subscribe message body (`{"type": …, "coin": …}` etc.).
    pub fn to_json(&self) -> Value {
        match self.stream {
            Stream::AllMids => match &self.dex {
                Some(dex) => json!({ "type": "allMids", "dex": dex }),
                None => json!({ "type": "allMids" }),
            },
            stream => {
                let coin = self.coin.as_deref().unwrap_or_default();
                json!({ "type": stream.as_str(), "coin": coin })
            }
        }
    }

    fn sort_key(&self) -> (u8, &'static str, Option<&str>, Option<&str>) {
        (
            self.stream.priority(),
            self.stream.as_str(),
            self.coin.as_deref(),
            self.dex.as_deref(),
        )
    }
}

impl PartialOrd for Subscription {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Subscription {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.sort_key().cmp(&other.sort_key())
    }
}

impl fmt::Display for Subscription {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let market = self.coin.as_deref().or(self.dex.as_deref()).unwrap_or("-");
        write!(
            f,
            "{:<16} {:<20} p{}",
            self.stream.as_str(),
            market,
            self.stream.priority()
        )
    }
}

/// A per-coin day-notional-volume index used to rank `*:top:N` selectors.
///
/// Perp volumes come from `metaAndAssetCtxs`; spot volumes from
/// `spotMetaAndAssetCtxs` (shape ⚠ verify V-1). Coins absent from the index
/// rank last (volume treated as zero).
#[derive(Debug, Clone, Default)]
pub struct VolumeIndex {
    volumes: BTreeMap<String, Decimal>,
}

impl VolumeIndex {
    /// An empty index.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace a coin's 24h notional volume.
    pub fn insert(&mut self, coin: impl Into<String>, day_ntl_vlm: Decimal) {
        self.volumes.insert(coin.into(), day_ntl_vlm);
    }

    /// The volume for a coin, if known.
    pub fn get(&self, coin: &str) -> Option<Decimal> {
        self.volumes.get(coin).copied()
    }

    /// Build the perp index from a `metaAndAssetCtxs` response.
    pub fn from_perp_ctxs(ctxs: &MetaAndAssetCtxs) -> Self {
        let mut index = Self::new();
        for (asset, ctx) in ctxs.meta.universe.iter().zip(ctxs.asset_ctxs.iter()) {
            index.insert(asset.name.clone(), ctx.day_ntl_vlm);
        }
        index
    }
}

/// The `[profile.<name>.hl]` block (SPEC-0008 §7.3).
#[derive(Debug, Clone)]
pub struct HlProfile {
    /// Coins/selectors recorded on `bbo` (priority 1).
    pub bbo: Vec<String>,
    /// Coins/selectors recorded on `trades` (priority 2).
    pub trades: Vec<String>,
    /// Coins/selectors recorded on `activeAssetCtx` (priority 2).
    pub active_asset_ctx: Vec<String>,
    /// Record `allMids` for the main dex and every HIP-3 dex.
    pub all_mids: bool,
    /// Coins/selectors recorded on `l2Book` (priority 3).
    pub l2book: Vec<String>,
    /// Hard cap on subscriptions before priority dropping kicks in.
    pub max_subs: usize,
    /// Maximum subscriptions per WS connection.
    pub subs_per_conn: usize,
    /// Maximum number of WS connections.
    pub connections: usize,
    /// Allow dropping priority-1 subscriptions when over budget (`--allow-truncate`).
    pub allow_truncate: bool,
}

impl Default for HlProfile {
    fn default() -> Self {
        Self {
            bbo: Vec::new(),
            trades: Vec::new(),
            active_asset_ctx: Vec::new(),
            all_mids: false,
            l2book: Vec::new(),
            max_subs: 900,
            subs_per_conn: 150,
            connections: 8,
            allow_truncate: false,
        }
    }
}

/// One planned WS connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connection {
    /// Stable connection id within the source, e.g. `hl-ws-01`.
    pub id: String,
    /// Subscriptions, in send order.
    pub subs: Vec<Subscription>,
}

/// Limits the plan was built against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanLimits {
    /// `max_subs`.
    pub max_subs: usize,
    /// `subs_per_conn`.
    pub subs_per_conn: usize,
    /// `connections`.
    pub connections: usize,
}

/// Aggregate counts for a plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanTotals {
    /// Number of connections used.
    pub connections: usize,
    /// Number of subscriptions kept.
    pub subscriptions: usize,
    /// Number of subscriptions dropped for budget.
    pub dropped: usize,
    /// Subscriptions by stream name.
    pub by_stream: BTreeMap<&'static str, usize>,
    /// Subscriptions by priority.
    pub by_priority: BTreeMap<u8, usize>,
}

/// The resolved subscription plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// Connections in send order.
    pub connections: Vec<Connection>,
    /// Subscriptions dropped to fit `max_subs`, lowest priority first.
    pub dropped: Vec<Subscription>,
    /// The limits the plan was built against.
    pub limits: PlanLimits,
    /// Aggregate counts.
    pub totals: PlanTotals,
}

/// Pacing constants derived from the plan (SPEC-0008 §7.4 steps 6–7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pacer {
    /// Minimum spacing between two subscribe messages.
    pub subscribe_interval: Duration,
    /// Minimum spacing between two new connections.
    pub connect_interval: Duration,
    /// Total subscribe messages to send.
    pub subscribe_messages: usize,
    /// Minimum time to drain all subscribe messages at the paced rate.
    pub drain: Duration,
}

impl Plan {
    /// Pacer schedule for sending this plan's subscriptions.
    pub fn pacer(&self) -> Pacer {
        let interval = Duration::from_millis(1_000 / MAX_SUBSCRIBE_MSGS_PER_SEC as u64);
        let messages = self.totals.subscriptions;
        Pacer {
            subscribe_interval: interval,
            connect_interval: MIN_NEW_CONN_INTERVAL,
            subscribe_messages: messages,
            drain: interval * messages as u32,
        }
    }
}

impl fmt::Display for Plan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "plan: {} connection(s), {} subscription(s), {} dropped",
            self.totals.connections, self.totals.subscriptions, self.totals.dropped
        )?;
        for (stream, count) in &self.totals.by_stream {
            writeln!(f, "  {stream:<16} {count}")?;
        }
        for (priority, count) in &self.totals.by_priority {
            writeln!(f, "  priority {priority:<8} {count}")?;
        }
        writeln!(
            f,
            "  limits: max_subs={} subs_per_conn={} connections={}",
            self.limits.max_subs, self.limits.subs_per_conn, self.limits.connections
        )?;
        for conn in &self.connections {
            writeln!(f, "\n{} ({} subs)", conn.id, conn.subs.len())?;
            for sub in &conn.subs {
                writeln!(f, "  {sub}")?;
            }
        }
        Ok(())
    }
}

/// A planner failure.
#[derive(Debug, Error)]
pub enum PlannerError {
    /// A selector string was malformed or unknown.
    #[error("unknown selector `{0}`")]
    UnknownSelector(String),
    /// An exact coin did not resolve in the asset map.
    #[error("unknown market `{0}`")]
    UnknownMarket(String),
    /// `spot:quotes` needs `spotMeta`, which was not provided.
    #[error("`spot:quotes` requires spot metadata")]
    MissingSpotMeta,
    /// A numeric selector bound was missing or non-numeric.
    #[error("invalid number in selector `{0}`")]
    InvalidNumber(String),
    /// The plan would drop a priority-1 subscription.
    #[error("over budget by {over} subscription(s); dropping priority-1 needs --allow-truncate")]
    PriorityOneTruncation {
        /// How many subscriptions are over `max_subs`.
        over: usize,
    },
    /// `subs_per_conn` or `connections` is zero.
    #[error("invalid plan limits: subs_per_conn={subs_per_conn}, connections={connections}")]
    InvalidLimits {
        /// Configured subscriptions per connection.
        subs_per_conn: usize,
        /// Configured connection count.
        connections: usize,
    },
    /// Different subscription groups need more connections than allowed.
    #[error("plan needs {needed} connections but only {allowed} are allowed")]
    ConnectionLimit {
        /// Connections required.
        needed: usize,
        /// Connections allowed.
        allowed: usize,
    },
    /// The subscriptions do not fit in `subs_per_conn × connections`.
    #[error("plan needs {needed} subscription slots but only {capacity} are available")]
    SubscriptionCapacity {
        /// Subscription slots required.
        needed: usize,
        /// Subscription slots available.
        capacity: usize,
    },
}

/// One parsed universe selector (SPEC-0008 §7.3).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Selector {
    Exact(String),
    PerpsAll,
    PerpsTop(usize),
    Hip3All,
    Hip3Dex(String),
    SpotAll,
    SpotTop(usize),
    SpotQuotes,
}

/// Resolve `profile` against the supplied metadata into a [`Plan`].
pub fn plan(
    profile: &HlProfile,
    map: &AssetMap,
    volumes: &VolumeIndex,
    spot_meta: Option<&SpotMeta>,
) -> Result<Plan, PlannerError> {
    if profile.subs_per_conn == 0 || profile.connections == 0 {
        return Err(PlannerError::InvalidLimits {
            subs_per_conn: profile.subs_per_conn,
            connections: profile.connections,
        });
    }

    let universe = Universe {
        map,
        volumes,
        spot_meta,
    };

    let mut set: BTreeSet<Subscription> = BTreeSet::new();
    for (stream, selectors) in [
        (Stream::Bbo, &profile.bbo),
        (Stream::Trades, &profile.trades),
        (Stream::ActiveAssetCtx, &profile.active_asset_ctx),
        (Stream::L2Book, &profile.l2book),
    ] {
        for coin in universe.expand_coins(selectors)? {
            set.insert(Subscription {
                stream,
                coin: Some(coin),
                dex: None,
            });
        }
    }
    if profile.all_mids {
        set.insert(Subscription {
            stream: Stream::AllMids,
            coin: None,
            dex: None,
        });
        for dex in hip3_dexes(map) {
            set.insert(Subscription {
                stream: Stream::AllMids,
                coin: None,
                dex: Some(dex),
            });
        }
    }

    let mut subs: Vec<Subscription> = set.into_iter().collect();
    let mut dropped: Vec<Subscription> = Vec::new();
    while subs.len() > profile.max_subs {
        let priority = subs
            .iter()
            .map(|sub| sub.stream.priority())
            .max()
            .unwrap_or(1);
        let over = subs.len() - profile.max_subs;
        if priority == 1 && !profile.allow_truncate {
            return Err(PlannerError::PriorityOneTruncation { over });
        }
        let Some(index) = subs
            .iter()
            .rposition(|sub| sub.stream.priority() == priority)
        else {
            break;
        };
        let removed = subs.remove(index);
        tracing::warn!(
            stream = removed.stream.as_str(),
            coin = removed.coin.as_deref().unwrap_or("-"),
            "dropping subscription to fit max_subs"
        );
        dropped.push(removed);
    }

    let (l2, rest): (Vec<_>, Vec<_>) = subs
        .into_iter()
        .partition(|sub| sub.stream == Stream::L2Book);

    let l2_conns = l2.len().div_ceil(profile.subs_per_conn);
    if l2_conns > profile.connections {
        return Err(PlannerError::ConnectionLimit {
            needed: l2_conns,
            allowed: profile.connections,
        });
    }
    let remaining = profile.connections - l2_conns;
    if !rest.is_empty() && remaining == 0 {
        return Err(PlannerError::ConnectionLimit {
            needed: l2_conns + 1,
            allowed: profile.connections,
        });
    }
    if rest.len() > remaining * profile.subs_per_conn {
        return Err(PlannerError::SubscriptionCapacity {
            needed: rest.len(),
            capacity: remaining * profile.subs_per_conn,
        });
    }

    let mut connections = Vec::new();
    for (i, chunk) in l2.chunks(profile.subs_per_conn).enumerate() {
        connections.push(Connection {
            id: conn_id(i + 1),
            subs: chunk.to_vec(),
        });
    }
    if !rest.is_empty() {
        let mut buckets: Vec<Vec<Subscription>> = (0..remaining).map(|_| Vec::new()).collect();
        for (i, sub) in rest.into_iter().enumerate() {
            buckets[i % remaining].push(sub);
        }
        for bucket in buckets {
            connections.push(Connection {
                id: conn_id(connections.len() + 1),
                subs: bucket,
            });
        }
    }

    let totals = PlanTotals {
        connections: connections.len(),
        subscriptions: connections.iter().map(|conn| conn.subs.len()).sum(),
        dropped: dropped.len(),
        by_stream: count_by(&connections, |sub| sub.stream.as_str()),
        by_priority: count_by(&connections, |sub| sub.stream.priority()),
    };

    Ok(Plan {
        connections,
        dropped,
        limits: PlanLimits {
            max_subs: profile.max_subs,
            subs_per_conn: profile.subs_per_conn,
            connections: profile.connections,
        },
        totals,
    })
}

fn conn_id(n: usize) -> String {
    format!("hl-ws-{n:02}")
}

fn count_by<K: Ord>(
    connections: &[Connection],
    key: impl Fn(&Subscription) -> K,
) -> BTreeMap<K, usize> {
    let mut counts = BTreeMap::new();
    for conn in connections {
        for sub in &conn.subs {
            *counts.entry(key(sub)).or_insert(0) += 1;
        }
    }
    counts
}

struct Universe<'a> {
    map: &'a AssetMap,
    volumes: &'a VolumeIndex,
    spot_meta: Option<&'a SpotMeta>,
}

impl Universe<'_> {
    fn expand_coins(&self, selectors: &[String]) -> Result<Vec<String>, PlannerError> {
        let mut coins: BTreeSet<String> = BTreeSet::new();
        for selector in selectors {
            for coin in self.resolve(selector)? {
                coins.insert(coin);
            }
        }
        Ok(coins.into_iter().collect())
    }

    fn resolve(&self, selector: &str) -> Result<Vec<String>, PlannerError> {
        match parse_selector(selector)? {
            Selector::Exact(coin) => {
                let market = resolve_exact(self.map, &coin)
                    .ok_or_else(|| PlannerError::UnknownMarket(coin.clone()))?;
                Ok(vec![market.coin.clone()])
            }
            Selector::PerpsAll => Ok(self
                .map
                .iter()
                .filter(|m| m.kind == MarketKind::Perp && m.dex.is_none())
                .map(|m| m.coin.clone())
                .collect()),
            Selector::PerpsTop(n) => {
                let mut markets: Vec<&Market> = self
                    .map
                    .iter()
                    .filter(|m| m.kind == MarketKind::Perp && m.dex.is_none())
                    .collect();
                sort_by_volume(&mut markets, self.volumes);
                Ok(markets
                    .into_iter()
                    .take(n)
                    .map(|m| m.coin.clone())
                    .collect())
            }
            Selector::Hip3All => Ok(self
                .map
                .iter()
                .filter(|m| m.kind == MarketKind::Perp && m.dex.is_some())
                .map(|m| m.coin.clone())
                .collect()),
            Selector::Hip3Dex(dex) => Ok(self
                .map
                .iter()
                .filter(|m| m.kind == MarketKind::Perp && m.dex.as_deref() == Some(dex.as_str()))
                .map(|m| m.coin.clone())
                .collect()),
            Selector::SpotAll => Ok(self
                .map
                .iter()
                .filter(|m| m.kind == MarketKind::Spot)
                .map(|m| m.coin.clone())
                .collect()),
            Selector::SpotTop(n) => {
                let mut markets: Vec<&Market> = self
                    .map
                    .iter()
                    .filter(|m| m.kind == MarketKind::Spot)
                    .collect();
                sort_by_volume(&mut markets, self.volumes);
                Ok(markets
                    .into_iter()
                    .take(n)
                    .map(|m| m.coin.clone())
                    .collect())
            }
            Selector::SpotQuotes => {
                let spot = self.spot_meta.ok_or(PlannerError::MissingSpotMeta)?;
                Ok(spot_quote_coins(self.map, spot))
            }
        }
    }
}

/// Sort markets by volume descending, then canonical coin, for determinism.
fn sort_by_volume(markets: &mut [&Market], volumes: &VolumeIndex) {
    markets.sort_by(|a, b| {
        let va = volumes.get(&a.coin).unwrap_or(Decimal::ZERO);
        let vb = volumes.get(&b.coin).unwrap_or(Decimal::ZERO);
        vb.cmp(&va).then_with(|| a.coin.cmp(&b.coin))
    });
}

/// Every HIP-3 dex name present in the map, sorted.
fn hip3_dexes(map: &AssetMap) -> Vec<String> {
    map.iter()
        .filter_map(|market| market.dex.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Coins of the `spot:quotes` universe: pairs whose base token trades against
/// more than one quote token (SPEC-0008 §7.3, O2).
fn spot_quote_coins(map: &AssetMap, spot: &SpotMeta) -> Vec<String> {
    let mut base_quotes: BTreeMap<u32, BTreeSet<u32>> = BTreeMap::new();
    for pair in &spot.universe {
        base_quotes
            .entry(pair.tokens[0])
            .or_default()
            .insert(pair.tokens[1]);
    }
    let mut coins = Vec::new();
    for pair in &spot.universe {
        let multi_quote = base_quotes
            .get(&pair.tokens[0])
            .is_some_and(|quotes| quotes.len() > 1);
        if multi_quote
            && let Some(market) = map
                .iter()
                .find(|m| m.kind == MarketKind::Spot && m.index == pair.index)
        {
            coins.push(market.coin.clone());
        }
    }
    coins
}

fn parse_selector(selector: &str) -> Result<Selector, PlannerError> {
    if selector == "perps:all" {
        return Ok(Selector::PerpsAll);
    }
    if let Some(rest) = selector.strip_prefix("perps:top:") {
        return Ok(Selector::PerpsTop(parse_count(selector, rest)?));
    }
    if selector == "hip3:all" {
        return Ok(Selector::Hip3All);
    }
    if let Some(dex) = selector.strip_prefix("hip3:") {
        if !dex.is_empty() {
            return Ok(Selector::Hip3Dex(dex.to_string()));
        }
        return Err(PlannerError::UnknownSelector(selector.to_string()));
    }
    if selector == "spot:all" {
        return Ok(Selector::SpotAll);
    }
    if let Some(rest) = selector.strip_prefix("spot:top:") {
        return Ok(Selector::SpotTop(parse_count(selector, rest)?));
    }
    if selector == "spot:quotes" {
        return Ok(Selector::SpotQuotes);
    }
    if selector.starts_with("perps:")
        || selector.starts_with("hip3:")
        || selector.starts_with("spot:")
    {
        return Err(PlannerError::UnknownSelector(selector.to_string()));
    }
    Ok(Selector::Exact(selector.to_string()))
}

fn parse_count(selector: &str, value: &str) -> Result<usize, PlannerError> {
    value
        .parse::<usize>()
        .map_err(|_| PlannerError::InvalidNumber(selector.to_string()))
}

/// Resolve a market by exact coin, trying the same case variants as
/// [`mev_hl_client::MarketSelector`].
fn resolve_exact<'a>(map: &'a AssetMap, coin: &str) -> Option<&'a Market> {
    if let Some(market) = map.get(coin) {
        return Some(market);
    }
    let upper = coin.to_uppercase();
    if let Some(market) = map.get(&upper) {
        return Some(market);
    }
    if let Some((dex, name)) = coin.split_once(':') {
        return map.get(&format!("{dex}:{}", name.to_uppercase()));
    }
    None
}

#[cfg(test)]
mod tests {
    use mev_hl_client::types::{AssetMeta, Meta, SpotPair, SpotToken};

    use super::*;

    fn perp_meta(names: &[&str]) -> Meta {
        Meta {
            universe: names
                .iter()
                .map(|name| AssetMeta {
                    name: (*name).to_string(),
                    sz_decimals: 5,
                    max_leverage: 20,
                    is_delisted: false,
                    only_isolated: false,
                })
                .collect(),
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
                SpotPair {
                    name: "@1".into(),
                    index: 1,
                    tokens: [2, 1],
                },
                SpotPair {
                    name: "@2".into(),
                    index: 2,
                    tokens: [2, 3],
                },
                SpotPair {
                    name: "@3".into(),
                    index: 3,
                    tokens: [4, 1],
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
                SpotToken {
                    name: "USDT0".into(),
                    index: 3,
                    sz_decimals: 8,
                },
                SpotToken {
                    name: "UETH".into(),
                    index: 4,
                    sz_decimals: 6,
                },
            ],
        }
    }

    fn fixture_map() -> AssetMap {
        let mut map = AssetMap::new();
        map.insert_perp_dex(None, None, &perp_meta(&["BTC", "ETH", "SOL"]));
        map.insert_perp_dex(Some("xyz"), Some(1), &perp_meta(&["TSLA", "NVDA"]));
        map.insert_spot(&spot_meta());
        map
    }

    fn volumes() -> VolumeIndex {
        let mut index = VolumeIndex::new();
        index.insert("BTC", Decimal::from(1000));
        index.insert("ETH", Decimal::from(500));
        index.insert("SOL", Decimal::from(100));
        index.insert("@1", Decimal::from(50));
        index.insert("@2", Decimal::from(10));
        index
    }

    fn sub(stream: Stream, coin: &str) -> Subscription {
        Subscription {
            stream,
            coin: Some(coin.to_string()),
            dex: None,
        }
    }

    fn build(profile: &HlProfile) -> Plan {
        try_build(profile).expect("plan")
    }

    fn try_build(profile: &HlProfile) -> Result<Plan, PlannerError> {
        plan(profile, &fixture_map(), &volumes(), Some(&spot_meta()))
    }

    fn try_build_without_spot(profile: &HlProfile) -> Result<Plan, PlannerError> {
        plan(profile, &fixture_map(), &volumes(), None)
    }

    #[test]
    fn golden_small_plan() {
        let profile = HlProfile {
            bbo: vec!["perps:all".into()],
            trades: vec!["BTC".into()],
            l2book: vec!["BTC".into()],
            max_subs: 100,
            subs_per_conn: 2,
            connections: 4,
            ..HlProfile::default()
        };
        let plan = build(&profile);
        assert_eq!(plan.totals.connections, 4);
        assert_eq!(plan.totals.subscriptions, 5);
        assert_eq!(plan.totals.dropped, 0);
        assert_eq!(
            plan.connections[0],
            Connection {
                id: "hl-ws-01".into(),
                subs: vec![sub(Stream::L2Book, "BTC")],
            }
        );
        assert_eq!(
            plan.connections[1],
            Connection {
                id: "hl-ws-02".into(),
                subs: vec![sub(Stream::Bbo, "BTC"), sub(Stream::Trades, "BTC")],
            }
        );
        assert_eq!(
            plan.connections[2],
            Connection {
                id: "hl-ws-03".into(),
                subs: vec![sub(Stream::Bbo, "ETH")],
            }
        );
        assert_eq!(
            plan.connections[3],
            Connection {
                id: "hl-ws-04".into(),
                subs: vec![sub(Stream::Bbo, "SOL")],
            }
        );
    }

    #[test]
    fn over_budget_drops_lowest_priority_first() {
        let profile = HlProfile {
            bbo: vec!["BTC".into(), "ETH".into()],
            l2book: vec!["BTC".into()],
            max_subs: 2,
            subs_per_conn: 5,
            connections: 2,
            ..HlProfile::default()
        };
        let plan = build(&profile);
        assert_eq!(plan.totals.subscriptions, 2);
        assert_eq!(plan.dropped, vec![sub(Stream::L2Book, "BTC")]);
    }

    #[test]
    fn priority_one_drop_fails_without_truncate() {
        let mut profile = HlProfile {
            bbo: vec!["BTC".into(), "ETH".into(), "SOL".into()],
            max_subs: 2,
            subs_per_conn: 5,
            connections: 2,
            ..HlProfile::default()
        };
        let err = plan(&profile, &fixture_map(), &volumes(), None).unwrap_err();
        assert!(matches!(
            err,
            PlannerError::PriorityOneTruncation { over: 1 }
        ));

        profile.allow_truncate = true;
        let plan = plan(&profile, &fixture_map(), &volumes(), None).unwrap();
        assert_eq!(plan.totals.subscriptions, 2);
        assert_eq!(plan.dropped, vec![sub(Stream::Bbo, "SOL")]);
    }

    #[test]
    fn l2book_is_isolated_on_its_own_connections() {
        let profile = HlProfile {
            bbo: vec!["perps:all".into()],
            l2book: vec!["perps:all".into()],
            max_subs: 100,
            subs_per_conn: 2,
            connections: 5,
            ..HlProfile::default()
        };
        let plan = build(&profile);
        for conn in &plan.connections {
            let has_l2 = conn.subs.iter().any(|s| s.stream == Stream::L2Book);
            let has_other = conn.subs.iter().any(|s| s.stream != Stream::L2Book);
            assert!(
                !(has_l2 && has_other),
                "connection {} mixes l2Book",
                conn.id
            );
        }
        let l2_total: usize = plan
            .connections
            .iter()
            .flat_map(|c| &c.subs)
            .filter(|s| s.stream == Stream::L2Book)
            .count();
        assert_eq!(l2_total, 3);
    }

    #[test]
    fn determinism_is_independent_of_selector_order() {
        let a = HlProfile {
            bbo: vec!["perps:all".into(), "BTC".into()],
            trades: vec!["ETH".into(), "hip3:all".into()],
            active_asset_ctx: vec!["spot:quotes".into()],
            all_mids: true,
            l2book: vec!["BTC".into()],
            max_subs: 100,
            subs_per_conn: 4,
            connections: 6,
            ..HlProfile::default()
        };
        let b = HlProfile {
            bbo: vec!["BTC".into(), "perps:all".into()],
            trades: vec!["hip3:all".into(), "ETH".into()],
            active_asset_ctx: vec!["spot:quotes".into()],
            all_mids: true,
            l2book: vec!["BTC".into()],
            ..a.clone()
        };
        let plan_a = build(&a);
        let plan_b = build(&b);
        assert_eq!(plan_a, plan_b);
        assert!(plan_a.totals.subscriptions > 0);
    }

    #[test]
    fn top_selectors_rank_by_volume() {
        let profile = HlProfile {
            bbo: vec!["perps:top:2".into()],
            trades: vec!["spot:top:1".into()],
            max_subs: 100,
            subs_per_conn: 5,
            connections: 3,
            ..HlProfile::default()
        };
        let plan = build(&profile);
        assert!(
            plan.connections
                .iter()
                .flat_map(|c| &c.subs)
                .any(|s| s == &sub(Stream::Bbo, "BTC"))
        );
        assert!(
            plan.connections
                .iter()
                .flat_map(|c| &c.subs)
                .any(|s| s == &sub(Stream::Bbo, "ETH"))
        );
        assert!(
            !plan
                .connections
                .iter()
                .flat_map(|c| &c.subs)
                .any(|s| s == &sub(Stream::Bbo, "SOL"))
        );
        assert!(
            plan.connections
                .iter()
                .flat_map(|c| &c.subs)
                .any(|s| s == &sub(Stream::Trades, "@1"))
        );
    }

    #[test]
    fn spot_quotes_requires_multi_quote_base() {
        let profile = HlProfile {
            bbo: vec!["spot:quotes".into()],
            max_subs: 100,
            subs_per_conn: 5,
            connections: 2,
            ..HlProfile::default()
        };
        let resolved = build(&profile);
        let coins: Vec<&str> = resolved
            .connections
            .iter()
            .flat_map(|c| &c.subs)
            .filter_map(|s| s.coin.as_deref())
            .collect();
        assert!(coins.contains(&"@1"));
        assert!(coins.contains(&"@2"));
        assert!(!coins.contains(&"@3"));

        let err = try_build_without_spot(&profile).unwrap_err();
        assert!(matches!(err, PlannerError::MissingSpotMeta));
    }

    #[test]
    fn unknown_selectors_and_markets_error() {
        let profile = HlProfile {
            bbo: vec!["spot:nope".into()],
            ..HlProfile::default()
        };
        assert!(matches!(
            try_build(&profile),
            Err(PlannerError::UnknownSelector(_))
        ));

        let profile = HlProfile {
            bbo: vec!["NOPE".into()],
            ..HlProfile::default()
        };
        assert!(matches!(
            try_build(&profile),
            Err(PlannerError::UnknownMarket(_))
        ));

        let profile = HlProfile {
            bbo: vec!["perps:top:x".into()],
            ..HlProfile::default()
        };
        assert!(matches!(
            try_build(&profile),
            Err(PlannerError::InvalidNumber(_))
        ));
    }

    #[test]
    fn pacer_stays_under_twenty_messages_per_second() {
        let profile = HlProfile {
            bbo: vec!["perps:all".into()],
            max_subs: 100,
            subs_per_conn: 5,
            connections: 2,
            ..HlProfile::default()
        };
        let plan = build(&profile);
        let pacer = plan.pacer();
        assert_eq!(
            1_000 / pacer.subscribe_interval.as_millis(),
            MAX_SUBSCRIBE_MSGS_PER_SEC as u128
        );
        assert_eq!(pacer.connect_interval, Duration::from_secs(3));
        assert_eq!(pacer.subscribe_messages, plan.totals.subscriptions);
        assert_eq!(
            pacer.drain,
            Duration::from_millis(50 * plan.totals.subscriptions as u64)
        );
    }

    #[test]
    fn connection_limit_is_enforced() {
        let profile = HlProfile {
            l2book: vec!["perps:all".into()],
            max_subs: 100,
            subs_per_conn: 1,
            connections: 2,
            ..HlProfile::default()
        };
        let err = plan(&profile, &fixture_map(), &volumes(), None).unwrap_err();
        assert!(matches!(
            err,
            PlannerError::ConnectionLimit {
                needed: 3,
                allowed: 2
            }
        ));
    }

    #[test]
    fn all_mids_covers_main_dex_and_each_hip3_dex() {
        let profile = HlProfile {
            all_mids: true,
            max_subs: 10,
            subs_per_conn: 5,
            connections: 2,
            ..HlProfile::default()
        };
        let plan = build(&profile);
        let subs: Vec<&Subscription> = plan.connections.iter().flat_map(|c| &c.subs).collect();
        assert!(subs.iter().any(|s| s.dex.is_none()));
        assert!(subs.iter().any(|s| s.dex.as_deref() == Some("xyz")));
        assert_eq!(
            Subscription::to_json(&subscriptions(Stream::AllMids, None)),
            json!({ "type": "allMids" })
        );
        assert_eq!(
            Subscription::to_json(&Subscription {
                stream: Stream::AllMids,
                coin: None,
                dex: Some("xyz".into()),
            }),
            json!({ "type": "allMids", "dex": "xyz" })
        );
    }

    fn subscriptions(stream: Stream, coin: Option<&str>) -> Subscription {
        Subscription {
            stream,
            coin: coin.map(str::to_string),
            dex: None,
        }
    }
}
