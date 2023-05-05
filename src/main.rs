#![allow(dead_code)]
#![allow(unused_variables)]
use ethers::prelude::*;
use ethers_flashbots::*;
use reqwest::Url;
use std::{env, str::FromStr, sync::Arc};

mod arbitrage;
mod listener;
mod market;

use crate::arbitrage::Arbitrage;
use crate::listener::BlockListener;

const FLASHBOTS_RELAY_URL_GOERLI: &str = "https://relay-goerli.flashbots.net";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenv::dotenv().ok();

    let goerli_rpc: String = env::var("GOERLI_RPC_URL").expect("GOERLI_RPC_URL not set");

    let provider: Provider<Http> =
        Provider::<Http>::try_from(goerli_rpc).expect("could not instantiate HTTP Provider");
    let provider_arc: Arc<Provider<Http>> = Arc::new(provider); // using Arc since provider will be shared across multiple classes

    let private_key: String = env::var("PRIVATE_KEY").expect("PRIVATE_KEY not set");
    let flashbots_key: String = env::var("FLASHBOTS_KEY").expect("FLASHBOTS_KEY not set");

    let wallet = LocalWallet::from_str(&private_key)?;
    let flashbots_wallet = LocalWallet::from_str(&flashbots_key)?;

    // Print the wallet address to the console
    println!("Wallet address: {:?}", wallet.address());
    println!("Flashbots signer address: {:?}", flashbots_wallet.address());

    let flashbots_middleware = FlashbotsMiddleware::new(
        provider_arc.clone(),
        Url::parse(FLASHBOTS_RELAY_URL_GOERLI)?,
        flashbots_wallet,
    );

    // Add signer and Flashbots middleware
    let client = SignerMiddleware::new(flashbots_middleware, wallet);

    let arb = Arbitrage::new(client);

    let listener: BlockListener = BlockListener::new(provider_arc);
    listener.listen().await?;

    Ok(())
}
