//! REST `/info` client (SPEC-0001 §4, §9).

use async_trait::async_trait;
use mev_core::{
    config::Network,
    error::{Error, Result},
};
use serde::de::DeserializeOwned;
use serde_json::json;

use crate::types::{AllMids, AssetCtx, L2Book, Meta, MetaAndAssetCtxs, SpotMeta};

/// Read-only HyperCore info API.
#[async_trait]
pub trait InfoApi: Send + Sync {
    /// Perpetuals metadata.
    async fn meta(&self) -> Result<Meta>;
    /// Spot metadata.
    async fn spot_meta(&self) -> Result<SpotMeta>;
    /// All mid prices.
    async fn all_mids(&self) -> Result<AllMids>;
    /// L2 book snapshot for a coin.
    async fn l2_book(&self, coin: &str) -> Result<L2Book>;
    /// Perpetuals metadata plus per-asset contexts.
    async fn meta_and_asset_ctxs(&self) -> Result<MetaAndAssetCtxs>;
}

/// HTTP implementation of [`InfoApi`].
#[derive(Debug, Clone)]
pub struct HttpInfo {
    client: reqwest::Client,
    base_url: String,
}

impl HttpInfo {
    /// Create a client for the given network.
    pub fn new(network: Network) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: network.rest_url().to_string(),
        }
    }

    /// Create a client against an explicit base URL (used in tests).
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into(),
        }
    }

    /// POST a body to `/info` and decode the response.
    pub async fn info<T: DeserializeOwned>(&self, body: serde_json::Value) -> Result<T> {
        let url = format!("{}/info", self.base_url.trim_end_matches('/'));
        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Http(e.to_string()))?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(Error::Http(format!("POST /info -> {status}: {text}")));
        }

        resp.json::<T>()
            .await
            .map_err(|e| Error::Decode(e.to_string()))
    }
}

#[async_trait]
impl InfoApi for HttpInfo {
    async fn meta(&self) -> Result<Meta> {
        self.info(json!({ "type": "meta" })).await
    }

    async fn spot_meta(&self) -> Result<SpotMeta> {
        self.info(json!({ "type": "spotMeta" })).await
    }

    async fn all_mids(&self) -> Result<AllMids> {
        self.info(json!({ "type": "allMids" })).await
    }

    async fn l2_book(&self, coin: &str) -> Result<L2Book> {
        self.info(json!({ "type": "l2Book", "coin": coin })).await
    }

    async fn meta_and_asset_ctxs(&self) -> Result<MetaAndAssetCtxs> {
        let (meta, asset_ctxs): (Meta, Vec<AssetCtx>) =
            self.info(json!({ "type": "metaAndAssetCtxs" })).await?;
        Ok(MetaAndAssetCtxs { meta, asset_ctxs })
    }
}

#[cfg(test)]
mod tests {
    use rust_decimal::Decimal;
    use std::str::FromStr;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    use super::*;

    async fn mount(body: &str) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/info"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn parses_all_mids() {
        let server = mount(r#"{"BTC":"100.5","ETH":"20"}"#).await;
        let client = HttpInfo::with_base_url(server.uri());
        let mids = client.all_mids().await.unwrap();
        assert_eq!(
            mids.get("BTC").unwrap(),
            &Decimal::from_str("100.5").unwrap()
        );
        assert_eq!(mids.get("ETH").unwrap(), &Decimal::from_str("20").unwrap());
    }

    #[tokio::test]
    async fn parses_l2_book_and_mid() {
        let body = r#"{
            "coin":"BTC","time":1754450974231,
            "levels":[
                [{"px":"100","sz":"1.5","n":3}],
                [{"px":"101","sz":"2","n":4}]
            ]
        }"#;
        let server = mount(body).await;
        let client = HttpInfo::with_base_url(server.uri());
        let book = client.l2_book("BTC").await.unwrap();
        assert_eq!(book.best_bid().unwrap().px, Decimal::from(100));
        assert_eq!(book.best_ask().unwrap().px, Decimal::from(101));
        assert_eq!(book.mid().unwrap(), Decimal::from_str("100.5").unwrap());
    }

    #[tokio::test]
    async fn parses_meta() {
        let body = r#"{"universe":[{"name":"BTC","szDecimals":5,"maxLeverage":40}]}"#;
        let server = mount(body).await;
        let client = HttpInfo::with_base_url(server.uri());
        let meta = client.meta().await.unwrap();
        assert_eq!(meta.universe.len(), 1);
        assert_eq!(meta.universe[0].name, "BTC");
        assert_eq!(meta.universe[0].sz_decimals, 5);
    }

    #[tokio::test]
    async fn surfaces_http_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/info"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        let client = HttpInfo::with_base_url(server.uri());
        assert!(client.all_mids().await.is_err());
    }
}
