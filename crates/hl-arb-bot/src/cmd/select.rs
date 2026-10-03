//! `hl select`: edit the persisted watchlist (SPEC-0001).

use crate::*;

/// Resolve and validate the persisted watchlist against live metadata.
pub(crate) async fn select(
    network: Option<NetworkArg>,
    coins: Vec<String>,
    add: Vec<String>,
    remove: Vec<String>,
) -> Result<()> {
    let config = Config::load(ConfigOverrides {
        network: network.map(Into::into),
        ..Default::default()
    })?;

    let mut watchlist = hl_arb_core::watchlist::load(&config.watchlist_path)?;
    if watchlist.is_empty() {
        watchlist = config.watchlist.clone();
    }

    if !coins.is_empty() {
        watchlist = coins;
    }
    for coin in add {
        if !watchlist.iter().any(|c| c.eq_ignore_ascii_case(&coin)) {
            watchlist.push(coin);
        }
    }
    if !remove.is_empty() {
        watchlist.retain(|c| !remove.iter().any(|r| r.eq_ignore_ascii_case(c)));
    }
    if watchlist.is_empty() {
        anyhow::bail!("refusing to persist an empty watchlist");
    }

    let selector = selector_for(config.network, &watchlist).await?;
    let resolved = selector.resolve_all(&watchlist)?;
    let canonical: Vec<String> = resolved.iter().map(|m| m.coin.clone()).collect();

    hl_arb_core::watchlist::save(&config.watchlist_path, &canonical)?;
    println!(
        "watchlist ({}): {}",
        canonical.len(),
        config.watchlist_path.display()
    );
    for market in &resolved {
        println!("  {}", format_market(market));
    }
    Ok(())
}
pub(crate) fn format_market(market: &Market) -> String {
    let kind = match (market.kind, market.dex.as_deref()) {
        (MarketKind::Perp, Some(dex)) => format!("perp {dex}"),
        (MarketKind::Perp, None) => "perp".to_string(),
        (MarketKind::Spot, _) => "spot".to_string(),
    };
    format!(
        "{:<16} {:<10} index={:<4} szDecimals={}",
        market.coin, kind, market.index, market.sz_decimals
    )
}
