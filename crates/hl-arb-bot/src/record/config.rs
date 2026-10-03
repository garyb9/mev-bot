//! Recorder configuration, profile resolution, and subscription planning
//! (SPEC-0008 §7).

use super::*;

// ---------------------------------------------------------------------------
// Configuration (config/record.toml)
// ---------------------------------------------------------------------------

/// Top-level `config/record.toml`.
#[derive(Debug, Clone, Deserialize, Default)]
pub(super) struct RecordConfig {
    #[serde(default)]
    pub(super) profile: BTreeMap<String, Profile>,
}

/// One `[profile.<name>]` block (SPEC-0008 §7.3).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub(super) struct Profile {
    /// `mainnet` or `testnet`.
    pub(super) network: String,
    /// Root of the recording tree.
    pub(super) out_dir: PathBuf,
    /// zstd compression level.
    pub(super) zstd_level: i32,
    /// Stop recording when free disk drops below this many GiB.
    pub(super) min_free_gb: u64,
    /// Absolute mount path the recorder must write under, if any (R-14). When
    /// set, startup fails unless it is a real mount and `out_dir` is inside it,
    /// and the stream stops if the mount disappears. Unset = current behaviour.
    pub(super) require_mount: Option<PathBuf>,
    /// Optional mount source (e.g. `E:\`) that `require_mount` must also match,
    /// case-insensitively (R-14 fix1 §7). Ignored unless `require_mount` is set.
    pub(super) require_mount_source: Option<String>,
    /// Local retention in days (applied by R-10 shipping, not yet here).
    #[allow(dead_code)]
    pub(super) retain_days: u64,
    /// Metadata refresh cadence in seconds.
    pub(super) meta_refresh_secs: u64,
    /// Port for `/healthz` `/readyz` `/metrics`.
    pub(super) http_port: u16,
    /// Hyperliquid WS subscriptions.
    pub(super) hl: HlSection,
    /// Hyperliquid REST snapshotter.
    pub(super) rest: RestSection,
    /// Deribit public options summaries (R-12).
    pub(super) deribit: DeribitSection,
    /// Reference CEX symbols (R-8).
    pub(super) cex: CexSection,
    /// HyperEVM pool source (R-9, not started yet).
    #[allow(dead_code)]
    pub(super) hyperevm: HyperevmSection,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            network: "mainnet".to_string(),
            out_dir: PathBuf::from("data/rec"),
            zstd_level: 3,
            min_free_gb: 20,
            require_mount: None,
            require_mount_source: None,
            retain_days: 30,
            meta_refresh_secs: 300,
            http_port: 9091,
            hl: HlSection::default(),
            rest: RestSection::default(),
            deribit: DeribitSection::default(),
            cex: CexSection::default(),
            hyperevm: HyperevmSection::default(),
        }
    }
}

/// The `[profile.<name>.hl]` block.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub(super) struct HlSection {
    bbo: Vec<String>,
    trades: Vec<String>,
    active_asset_ctx: Vec<String>,
    all_mids: bool,
    l2book: Vec<String>,
    max_subs: usize,
    subs_per_conn: usize,
    connections: usize,
    allow_truncate: bool,
}

impl Default for HlSection {
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

impl HlSection {
    /// The planner profile, with the CLI `--allow-truncate` flag ORed in.
    pub(super) fn to_planner(&self, allow_truncate: bool) -> HlProfile {
        HlProfile {
            bbo: self.bbo.clone(),
            trades: self.trades.clone(),
            active_asset_ctx: self.active_asset_ctx.clone(),
            all_mids: self.all_mids,
            l2book: self.l2book.clone(),
            max_subs: self.max_subs,
            subs_per_conn: self.subs_per_conn,
            connections: self.connections,
            allow_truncate: allow_truncate || self.allow_truncate,
        }
    }

    /// Whether resolving this profile needs HIP-3 dex metadata.
    pub(super) fn wants_hip3(&self) -> bool {
        self.all_mids
            || self
                .bbo
                .iter()
                .chain(&self.trades)
                .chain(&self.active_asset_ctx)
                .chain(&self.l2book)
                .any(|selector| selector.starts_with("hip3:"))
    }
}

/// The `[profile.<name>.rest]` block.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub(super) struct RestSection {
    pub(super) enabled: bool,
    pub(super) weight_per_min: u32,
}

impl Default for RestSection {
    fn default() -> Self {
        Self {
            enabled: true,
            weight_per_min: 300,
        }
    }
}

/// The `[profile.<name>.deribit]` block (R-12, SPEC-0008 §9.1).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub(super) struct DeribitSection {
    /// Whether to poll Deribit's public endpoints. Off by default so existing
    /// profiles and recordings are unchanged.
    pub(super) enabled: bool,
    /// Currencies to poll (`BTC`, `ETH`, …).
    pub(super) currencies: Vec<String>,
    /// JSON-RPC base URL (public, unauthenticated endpoint).
    pub(super) base_url: String,
}

impl Default for DeribitSection {
    fn default() -> Self {
        Self {
            enabled: false,
            currencies: vec!["BTC".to_string(), "ETH".to_string()],
            base_url: DEFAULT_BASE_URL.to_string(),
        }
    }
}

/// Validate every profile after parsing (config errors are fatal at startup).
pub(super) fn validate_config(config: &RecordConfig) -> Result<()> {
    for (name, profile) in &config.profile {
        profile
            .validate()
            .with_context(|| format!("profile `{name}`"))?;
    }
    Ok(())
}

impl Profile {
    /// Reject profiles that would start a source with no work to do.
    pub(super) fn validate(&self) -> Result<()> {
        if self.deribit.enabled && self.deribit.currencies.is_empty() {
            bail!("deribit.enabled = true but deribit.currencies is empty");
        }
        Ok(())
    }
}

/// Parse a recorder config from figment providers and validate every profile.
pub(super) fn parse_config(figment: Figment) -> Result<RecordConfig> {
    let config: RecordConfig = figment.extract().context("loading config/record.toml")?;
    validate_config(&config)?;
    Ok(config)
}

/// The `[profile.<name>.cex]` block (R-8).
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub(super) struct CexSection {
    pub(super) binance_usdm: Vec<String>,
    pub(super) binance_spot: Vec<String>,
    pub(super) bybit_linear: Vec<String>,
}

/// The `[profile.<name>.hyperevm]` block (R-9).
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub(super) struct HyperevmSection {
    pub(super) enabled: bool,
    pub(super) rpc_ws: Option<String>,
    pub(super) pools: Option<String>,
}

/// Resolve a profile name and an optional `--network` override.
pub(super) fn load_profile(
    name: Option<&str>,
    override_network: Option<Network>,
) -> Result<(String, Profile)> {
    let figment = Figment::new()
        .merge(Toml::file("config/record.toml"))
        .merge(Env::prefixed("HL_RECORD_").split("__"));
    let config = parse_config(figment)?;
    let name = name.unwrap_or("default").to_string();
    let mut profile = config
        .profile
        .get(&name)
        .cloned()
        .with_context(|| format!("profile `{name}` not found in config/record.toml"))?;
    if let Some(network) = override_network {
        profile.network = network_dir(network).to_string();
    }
    Ok((name, profile))
}

/// The directory name for a network (SPEC-0008 §6).
pub(crate) fn network_dir(network: Network) -> &'static str {
    match network {
        Network::Mainnet => "mainnet",
        Network::Testnet => "testnet",
    }
}

/// Parse the `network` string from a profile.
pub(super) fn parse_network(name: &str) -> Result<Network> {
    match name.to_ascii_lowercase().as_str() {
        "mainnet" => Ok(Network::Mainnet),
        "testnet" => Ok(Network::Testnet),
        other => bail!("unknown network `{other}` (expected mainnet or testnet)"),
    }
}

/// Resolve the profile network string to the enum.
pub(super) fn profile_network(profile: &Profile) -> Result<Network> {
    parse_network(&profile.network)
}

// ---------------------------------------------------------------------------
// Universe resolution
// ---------------------------------------------------------------------------

/// The metadata the planner needs: the asset map, volume index, and spot meta.
pub(super) struct Universe {
    pub(super) map: AssetMap,
    pub(super) volumes: VolumeIndex,
    pub(super) spot_meta: hl_arb_client::types::SpotMeta,
}

impl Universe {
    /// Fetch universe metadata over REST (`/info`). The planner itself is pure.
    pub(super) async fn load(network: Network, include_hip3: bool) -> Result<Self> {
        let info = HttpInfo::new(network);
        let map = AssetMap::load(&info, include_hip3).await?;
        let spot_meta = info.spot_meta().await?;
        let mut volumes = VolumeIndex::from_perp_ctxs(&info.meta_and_asset_ctxs().await?);
        // Spot volumes: `spotMetaAndAssetCtxs.ctxs` is indexed by the spot pair
        // `index`, not by position in `spotMeta.universe` (V-1, 2026-09-28), so
        // matching `market.index == ctxs position` joins on that index. Missing
        // volumes rank last.
        if let Ok(value) = info
            .info::<Value>(json!({ "type": "spotMetaAndAssetCtxs" }))
            .await
            && let Some(ctxs) = value.get(1).and_then(Value::as_array)
        {
            for (index, ctx) in ctxs.iter().enumerate() {
                let Some(volume) = ctx
                    .get("dayNtlVlm")
                    .and_then(|value| match value {
                        Value::String(text) => Some(text.clone()),
                        Value::Number(number) => Some(number.to_string()),
                        _ => None,
                    })
                    .and_then(|text| Decimal::from_str(&text).ok())
                else {
                    continue;
                };
                if let Some(market) = map
                    .iter()
                    .find(|m| m.kind == MarketKind::Spot && m.index == index as u32)
                {
                    volumes.insert(market.coin.clone(), volume);
                }
            }
        }
        Ok(Self {
            map,
            volumes,
            spot_meta,
        })
    }
}

// ---------------------------------------------------------------------------
// `hl record plan`
// ---------------------------------------------------------------------------

/// Resolve the plan and print it (SPEC-0008 §7.4, §12.1). No recording sockets.
pub async fn plan(
    profile: Option<String>,
    network: Option<Network>,
    allow_truncate: bool,
) -> Result<()> {
    let (name, profile) = load_profile(profile.as_deref(), network)?;
    let network = profile_network(&profile)?;
    let universe = Universe::load(network, profile.hl.wants_hip3()).await?;
    let plan = build_plan(
        &profile.hl.to_planner(allow_truncate),
        &universe.map,
        &universe.volumes,
        Some(&universe.spot_meta),
    )?;
    println!("profile: {name}  network: {}", network_dir(network));
    println!("{plan}");
    println!(
        "limits: connections {}/10, subscriptions {}/1000",
        plan.totals.connections, plan.totals.subscriptions
    );
    for dropped in &plan.dropped {
        println!("dropped: {dropped}");
    }
    Ok(())
}
