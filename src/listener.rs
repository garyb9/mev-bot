use ethers::prelude::*;
use std::sync::Arc;

pub struct BlockListener {
    provider: Arc<Provider<Http>>,
}

impl BlockListener {
    pub fn new(provider: Arc<Provider<Http>>) -> Self {
        BlockListener { provider }
    }

    pub async fn listen(&self) -> anyhow::Result<()> {
        println!("Listening to incoming blocks");
        let stream: FilterWatcher<Http, TxHash> = self.provider.watch_blocks().await?;
        tokio::pin!(stream);
        stream
            .for_each(|block_hash| async move {
                let block = self.provider.get_block(block_hash).await.unwrap().unwrap();
                println!(
                    "Block {} -> baseFeePerGas: {} Gwei",
                    block.number.unwrap(),
                    block.base_fee_per_gas.unwrap() / 1_000_000_000
                );
            })
            .await;

        Ok(())
    }
}
