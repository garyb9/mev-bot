// not for production!
#![allow(dead_code)]
#![allow(unused_variables)]
use std::env;
use ethers::prelude::*;

// const FLASHBOTS_BUNLDE_RELAY_URL: &str = "https://relay-goerli.flashbots.net";
#[tokio::main]
async fn main() {
    dotenv::dotenv().ok();

    let goerli_rpc: String = env::var("GOERLI_RPC_URL").expect("GOERLI_RPC_URL not set");

    let provider: Provider<Http> =
        Provider::<Http>::try_from(goerli_rpc).expect("could not instantiate HTTP Provider");

    let block: Block<Transaction> = provider
        .get_block_with_txs(BlockNumber::Latest)
        .await
        .unwrap()
        .unwrap();

    println!("Block {}", serde_json::to_string_pretty(&block).unwrap());

    // let private_key: String = env::var("PRIVATE_KEY").expect("PRIVATE_KEY not set");
    // let flashbots_key: String = env::var("FLASHBOTS_KEY").expect("FLASHBOTS_KEY not set");

    // let wallet = Wallet::new(&mut &private_key);

    // Print the wallet address
    // println!("Wallet address: {}", wallet.address());

    // Ok(())
}
