use async_trait::async_trait;
use ethers::types::U256;
use std::collections::HashMap;
use std::future::Future;

pub struct TokenBalances(HashMap<String, U256>);

pub struct MultipleCallData {
    targets: Vec<String>,
    data: Vec<String>,
}

pub struct CallDetails<'a> {
    target: String,
    data: String,
    value: Option<&'a U256>,
}

#[async_trait]
pub trait EthMarket {
    async fn get_tokens_out(
        &self,
        token_in: String,
        token_out: String,
        amount_in: U256,
    ) -> dyn Future<Output = U256>;

    async fn get_tokens_in(
        &self,
        token_in: String,
        token_out: String,
        amount_out: U256,
    ) -> dyn Future<Output = U256>;

    // async fn sell_tokens_to_next_market(
    //     &self,
    //     token_in: String,
    //     amount_in: U256,
    //     eth_market: dyn EthMarket,
    // ) -> dyn Future<Output = MultipleCallData>; // Future

    async fn sell_tokens(
        &self,
        token_in: String,
        amount_in: U256,
        recipient: String,
    ) -> dyn Future<Output = String>; // Future

    async fn receive_directly(&self, token_address: String) -> dyn Future<Output = bool>;

    async fn prepare_receive(
        &self,
        token_address: String,
        amount_in: U256,
    ) -> dyn Future<Output = Vec<CallDetails>>; // Future
}
