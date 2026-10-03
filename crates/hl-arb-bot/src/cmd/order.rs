//! `hl order`: build and sign an order without submitting it (SPEC-0002).

use crate::*;

/// Build and sign a single order, printing the wire form and envelope. Never
/// submits, so it is safe to run in any mode.
pub(crate) async fn order(network: Option<NetworkArg>, args: OrderArgs) -> Result<()> {
    use rust_decimal::Decimal;
    use std::str::FromStr;

    let config = Config::load(ConfigOverrides {
        network: network.map(Into::into),
        ..Default::default()
    })?;
    let key = config
        .agent_key()
        .ok_or_else(|| anyhow::anyhow!("set HL_AGENT_PRIVATE_KEY to sign an order"))?;
    let signer = AgentSigner::from_hex(key, config.network == Network::Mainnet)?;

    let selector = selector_for(config.network, std::slice::from_ref(&args.coin)).await?;
    let market = selector.resolve(&args.coin)?;

    let params = OrderParams {
        is_buy: matches!(args.side, Side::Buy),
        size: Decimal::from_str(&args.sz)
            .map_err(|e| anyhow::anyhow!("invalid --sz `{}`: {e}", args.sz))?,
        limit_px: Decimal::from_str(&args.px)
            .map_err(|e| anyhow::anyhow!("invalid --px `{}`: {e}", args.px))?,
        tif: args.tif.into(),
        reduce_only: args.reduce_only,
        cloid: args.cloid,
    };
    let wire = build_order_wire(&market, &params)?;
    let action = Action::order(vec![wire.clone()]);
    let nonce = now_ms();
    let request = build_request(&action, &signer, nonce, None, None)?;

    println!("mode:      dry-run (nothing submitted)");
    println!("network:   {:?}", config.network);
    println!("agent:     {}", signer.address());
    println!(
        "market:    {} (asset_id={}, szDecimals={})",
        market.coin,
        market.asset_id(),
        market.sz_decimals
    );
    println!("order:     {}", serde_json::to_string(&wire)?);
    println!("nonce:     {nonce}");
    println!(
        "signature: r={} s={} v={}",
        request.signature.r_hex(),
        request.signature.s_hex(),
        request.signature.v
    );
    println!("envelope:  {}", serde_json::to_string(&request)?);
    Ok(())
}
