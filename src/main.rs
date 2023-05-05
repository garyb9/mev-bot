#![allow(dead_code)]
#![allow(unused_variables)]
use ethers::prelude::*;
use ethers_flashbots::*;
use reqwest::Url;
use std::{env, str::FromStr};

mod arbitrage;
use crate::arbitrage::Arbitrage;

const FLASHBOTS_BUNLDE_RELAY_URL: &str = "https://relay-goerli.flashbots.net";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenv::dotenv().ok();

    let goerli_rpc: String = env::var("GOERLI_RPC_URL").expect("GOERLI_RPC_URL not set");

    let provider: Provider<Http> =
        Provider::<Http>::try_from(goerli_rpc).expect("could not instantiate HTTP Provider");

    let private_key: String = env::var("PRIVATE_KEY").expect("PRIVATE_KEY not set");
    let flashbots_key: String = env::var("FLASHBOTS_KEY").expect("FLASHBOTS_KEY not set");

    let wallet = LocalWallet::from_str(&private_key)?;
    let flashbots_wallet = LocalWallet::from_str(&flashbots_key)?;

    // Print the wallet address to the console
    println!("Wallet address: {:?}", wallet.address());
    println!("Flashbots signer address: {:?}", flashbots_wallet.address());

    let flashbots_middleware = FlashbotsMiddleware::new(
        provider,
        Url::parse(FLASHBOTS_BUNLDE_RELAY_URL)?,
        flashbots_wallet,
    );

    // Add signer and Flashbots middleware
    let client = SignerMiddleware::new(flashbots_middleware, wallet);

    let arb = Arbitrage::new(client);

    Ok(())
}
