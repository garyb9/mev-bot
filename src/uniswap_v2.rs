use crate::market;

struct UniswapV2EthPair {
    tokens: Vec<String>,
    market_address: String,
    protocol: String,
}

impl UniswapV2EthPair {
    pub fn new(tokens: Vec<String>, market_address: String, protocol: String) -> Self {
        UniswapV2EthPair {
            tokens,
            market_address,
            protocol,
        }
    }
}

impl market::EthMarket for UniswapV2EthPair {
    fn get_tokens_out<'life0, 'async_trait>(
        &'life0 self,
        token_in: String,
        token_out: String,
        amount_in: ethers::types::U256,
    ) -> core::pin::Pin<
        Box<
            dyn core::future::Future<Output = ethers::types::U256>
                + core::marker::Send
                + 'async_trait,
        >,
    >
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        todo!()
    }

    fn get_tokens_in<'life0, 'async_trait>(
        &'life0 self,
        token_in: String,
        token_out: String,
        amount_out: ethers::types::U256,
    ) -> core::pin::Pin<
        Box<
            dyn core::future::Future<Output = ethers::types::U256>
                + core::marker::Send
                + 'async_trait,
        >,
    >
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        todo!()
    }

    // fn sell_tokens_to_next_market<'life0, 'async_trait>(
    //     &'life0 self,
    //     token_in: String,
    //     amount_in: ethers::types::U256,
    //     eth_market: dyn market::EthMarket,
    // ) -> core::pin::Pin<
    //     Box<
    //         dyn core::future::Future<Output = market::MultipleCallData>
    //             + core::marker::Send
    //             + 'async_trait,
    //     >,
    // >
    // where
    //     'life0: 'async_trait,
    //     Self: 'async_trait,
    // {
    //     todo!()
    // }

    fn sell_tokens<'life0, 'async_trait>(
        &'life0 self,
        token_in: String,
        amount_in: ethers::types::U256,
        recipient: String,
    ) -> core::pin::Pin<
        Box<dyn core::future::Future<Output = String> + core::marker::Send + 'async_trait>,
    >
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        todo!()
    }

    fn receive_directly<'life0, 'async_trait>(
        &'life0 self,
        token_address: String,
    ) -> core::pin::Pin<
        Box<dyn core::future::Future<Output = bool> + core::marker::Send + 'async_trait>,
    >
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        todo!()
    }

    fn prepare_receive<'life0, 'async_trait>(
        &'life0 self,
        token_address: String,
        amount_in: ethers::types::U256,
    ) -> core::pin::Pin<
        Box<
            dyn core::future::Future<Output = Vec<market::CallDetails>>
                + core::marker::Send
                + 'async_trait,
        >,
    >
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        todo!()
    }
}
