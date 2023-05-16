
use ethers::prelude::*;
use mev_bot::{address_book::UniV2RouterCalls, utils::Config};

#[tokio::main]
async fn main() -> eyre::Result<()> {
    dotenv::dotenv().ok();
    let config = Config::new().await;
    println!("[STARTING]");

    let stream = config.wss.subscribe_pending_txs().await?;
    let mut tx_stream = stream.transactions_unordered(usize::MAX);
    while let Some(maybe_tx) = tx_stream.next().await {
        if let Ok(tx) = maybe_tx {
            if let Ok(decoded) = UniV2RouterCalls::decode_hex(&tx.input) {
                println!("[TX] {:?}\n[DECODED]{:?}\n", tx.hash, decoded);
            }
        }
    }
    
    Ok(())
}
  