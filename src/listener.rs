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
        let stream: FilterWatcher<Http, TxHash> = self.provider.watch_blocks().await?;
        tokio::pin!(stream);
        stream
            .for_each(|block_hash| async move {
                let block = self.provider.get_block(block_hash).await.unwrap().unwrap();
                println!(
                    "Transactions in Block {}: {}",
                    block.number.unwrap(),
                    serde_json::to_string_pretty(&(block.transactions)).unwrap()
                );
            })
            .await;
        // Set up the block listener
        Ok(())
    }
}
