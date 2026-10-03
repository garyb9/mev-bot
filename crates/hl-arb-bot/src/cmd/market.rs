//! Market-data commands: `markets`, `dexs`, `book`, `watch` (SPEC-0001).

use crate::*;

pub(crate) async fn markets(
    network: Option<NetworkArg>,
    query: Option<String>,
    perp: bool,
    spot: bool,
    dex: Option<String>,
) -> Result<()> {
    let network = resolve_network(network)?;
    let info = HttpInfo::new(network);

    let filter = query.unwrap_or_default().to_lowercase();
    let matches = |name: &str| filter.is_empty() || name.to_lowercase().contains(&filter);
    let show_perp = perp || !spot;
    let show_spot = spot || !perp;

    match dex {
        Some(dex) => {
            let meta = info.meta_for(&dex).await?;
            println!("{} perps ({}):", dex, meta.universe.len());
            for asset in &meta.universe {
                if matches(&asset.name) {
                    println!(
                        "  {:<20} szDecimals={} maxLeverage={}",
                        asset.name, asset.sz_decimals, asset.max_leverage
                    );
                }
            }
        }
        None => {
            if show_perp {
                let meta = info.meta().await?;
                println!("perps ({}):", meta.universe.len());
                for asset in &meta.universe {
                    if matches(&asset.name) {
                        println!(
                            "  {:<12} szDecimals={} maxLeverage={}",
                            asset.name, asset.sz_decimals, asset.max_leverage
                        );
                    }
                }

                let dexs = info.perp_dexs().await.unwrap_or_default();
                let names: Vec<&str> = dexs.iter().map(|d| d.name.as_str()).collect();
                if !names.is_empty() {
                    println!("hip-3 dexes: {} (use --dex <name>)", names.join(", "));
                }
            }

            if show_spot {
                let spot = info.spot_meta().await?;
                println!("spot ({}):", spot.universe.len());
                for pair in &spot.universe {
                    if matches(&pair.name) {
                        println!("  {:<12} @{}", pair.name, pair.index);
                    }
                }
            }
        }
    }
    Ok(())
}
pub(crate) async fn dexs(network: Option<NetworkArg>) -> Result<()> {
    let network = resolve_network(network)?;
    let info = HttpInfo::new(network);
    let dexs = info.perp_dexs().await?;
    for dex in &dexs {
        match &dex.full_name {
            Some(full) => println!("{:<8} {}", dex.name, full),
            None => println!("{}", dex.name),
        }
    }
    Ok(())
}
pub(crate) async fn book(network: Option<NetworkArg>, coin: String, levels: usize) -> Result<()> {
    let network = resolve_network(network)?;
    let info = HttpInfo::new(network);
    let book = info.l2_book(&coin).await?;

    println!("{}  time={}  mid={:?}", book.coin, book.time, book.mid());
    println!("  bids:");
    for level in book.levels[0].iter().take(levels) {
        println!("    {:<16} {}", level.px, level.sz);
    }
    println!("  asks:");
    for level in book.levels[1].iter().take(levels) {
        println!("    {:<16} {}", level.px, level.sz);
    }
    Ok(())
}
pub(crate) async fn watch(network: Option<NetworkArg>, coins: Vec<String>) -> Result<()> {
    let network = resolve_network(network)?;
    let coins = if coins.is_empty() {
        Config::load(ConfigOverrides::default())?.watchlist
    } else {
        coins
    };
    if coins.is_empty() {
        anyhow::bail!("no coins to watch; pass coin names or configure a watchlist");
    }
    let coins: Vec<String> = selector_for(network, &coins)
        .await?
        .resolve_all(&coins)?
        .iter()
        .map(|market| market.coin.clone())
        .collect();

    let mut subs = vec![Subscription::AllMids];
    for coin in &coins {
        subs.push(Subscription::L2Book { coin: coin.clone() });
        subs.push(Subscription::Trades { coin: coin.clone() });
        subs.push(Subscription::ActiveAssetCtx { coin: coin.clone() });
    }

    let mut stream = WsMarketStream::connect(network, &subs).await?;
    println!(
        "watching {} on {:?} (ctrl-c to stop)",
        coins.join(", "),
        network
    );

    loop {
        tokio::select! {
            _ = shutdown_signal() => break,
            event = stream.next() => match event {
                Ok(event) => print_event(event),
                Err(err) => {
                    error!(error = %err, "stream error");
                    break;
                }
            },
        }
    }
    Ok(())
}
pub(crate) fn print_event(event: StreamEvent) {
    match event {
        StreamEvent::Mids(mids) => {
            let btc = mids.get("BTC").copied().unwrap_or_default();
            println!("mids      {} coins (BTC={})", mids.len(), btc);
        }
        StreamEvent::Book(book) => println!(
            "book      {:<12} bid={:?} ask={:?} mid={:?}",
            book.coin,
            book.best_bid().map(|l| l.px),
            book.best_ask().map(|l| l.px),
            book.mid(),
        ),
        StreamEvent::Bbo(bbo) => println!(
            "bbo       {:<12} bid={:?} ask={:?}",
            bbo.coin,
            bbo.bid().map(|l| l.px),
            bbo.ask().map(|l| l.px),
        ),
        StreamEvent::Trades(trades) => {
            if let Some(trade) = trades.last() {
                println!(
                    "trade     {:<12} {} {} @ {}",
                    trade.coin, trade.side, trade.sz, trade.px
                );
            }
        }
        StreamEvent::AssetCtx(update) => println!(
            "ctx       {:<12} mark={} oracle={} funding={}",
            update.coin, update.ctx.mark_px, update.ctx.oracle_px, update.ctx.funding,
        ),
        StreamEvent::OrderUpdates(orders) => {
            if let Some(order) = orders.first() {
                println!(
                    "order     {:<12} oid={} status={}",
                    order.order.coin, order.order.oid, order.status
                );
            }
        }
        StreamEvent::UserFills(fills) => {
            println!(
                "fills     {} (snapshot={})",
                fills.fills.len(),
                fills.is_snapshot
            );
        }
        StreamEvent::UserEvent(event) => {
            if let Some(fills) = &event.fills {
                println!("userEvent fills={}", fills.len());
            } else if event.funding.is_some() {
                println!("userEvent funding");
            } else if event.liquidation.is_some() {
                println!("userEvent liquidation");
            } else if event.non_user_cancel.is_some() {
                println!("userEvent nonUserCancel");
            }
        }
    }
}
