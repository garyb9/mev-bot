#![allow(dead_code)]
use ethers::prelude::*;

pub struct Arbitrage<M, S> {
    client: SignerMiddleware<M, S>,
}

impl<M, S> Arbitrage<M, S> {
    pub fn new(client: SignerMiddleware<M, S>) -> Self {
        Arbitrage { client }
    }
}
