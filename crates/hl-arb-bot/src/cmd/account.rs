//! `hl account`: show account state and open orders.

use crate::*;

/// Print account state and open orders for an address.
pub(crate) async fn account(network: Option<NetworkArg>, address: String) -> Result<()> {
    let network = resolve_network(network)?;
    let info = HttpInfo::new(network);

    let state = info.clearinghouse_state(&address).await?;
    println!(
        "account {address}  value={}  withdrawable={}  marginUsed={}",
        state.margin_summary.account_value,
        state.withdrawable,
        state.margin_summary.total_margin_used
    );
    if state.asset_positions.is_empty() {
        println!("positions: none");
    } else {
        println!("positions:");
        for entry in &state.asset_positions {
            let position = &entry.position;
            println!(
                "  {:<12} szi={:<14} entry={:<12} value={:<14} uPnl={}",
                position.coin,
                position.szi,
                position.entry_px.map(|p| p.to_string()).unwrap_or_default(),
                position.position_value,
                position.unrealized_pnl,
            );
        }
    }

    let orders = info.open_orders(&address).await?;
    println!("open orders: {}", orders.len());
    for order in &orders {
        println!(
            "  {:<12} {} {:<12} @ {:<12} oid={}",
            order.coin,
            if order.is_buy() { "buy " } else { "sell" },
            order.sz,
            order.limit_px,
            order.oid,
        );
    }
    Ok(())
}
