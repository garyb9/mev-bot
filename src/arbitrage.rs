use ethers::prelude::*;

pub struct Arbitrage {
    provider: Provider<Http>,
    wallet: LocalWallet,
    flashbots_wallet: LocalWallet,
}

impl Arbitrage {
    pub fn new(
        provider: Provider<Http>,
        wallet: LocalWallet,
        flashbots_wallet: LocalWallet,
    ) -> Self {
        Self {
            provider,
            wallet,
            flashbots_wallet,
        }
    }
}
