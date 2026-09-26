//! REST `/info` client (SPEC-0001 §4, §9).

use async_trait::async_trait;
use mev_core::{
    config::Network,
    error::{Error, Result},
};
use serde::de::DeserializeOwned;
use serde_json::json;

use crate::types::{
    AllMids, AssetCtx, ClearinghouseState, L2Book, Meta, MetaAndAssetCtxs, OpenOrder,
    OrderStatusResponse, PerpDex, SpotClearinghouseState, SpotMeta, UserFees, UserFill,
    UserFunding, UserRateLimit,
};

/// Read-only HyperCore info API.
#[async_trait]
pub trait InfoApi: Send + Sync {
    /// Perpetuals metadata for the default dex.
    async fn meta(&self) -> Result<Meta>;
    /// Perpetuals metadata for a builder-deployed HIP-3 dex.
    async fn meta_for(&self, dex: &str) -> Result<Meta>;
    /// List builder-deployed HIP-3 perpetual dexes.
    async fn perp_dexs(&self) -> Result<Vec<PerpDex>>;
    /// Spot metadata.
    async fn spot_meta(&self) -> Result<SpotMeta>;
    /// All mid prices for the default dex.
    async fn all_mids(&self) -> Result<AllMids>;
    /// All mid prices for a builder-deployed HIP-3 dex.
    async fn all_mids_for(&self, dex: &str) -> Result<AllMids>;
    /// L2 book snapshot for a coin (dex-qualified for HIP-3, e.g. `xyz:TSLA`).
    async fn l2_book(&self, coin: &str) -> Result<L2Book>;
    /// Perpetuals metadata plus per-asset contexts for the default dex.
    async fn meta_and_asset_ctxs(&self) -> Result<MetaAndAssetCtxs>;
    /// Perp account state: positions, margin, withdrawable.
    async fn clearinghouse_state(&self, user: &str) -> Result<ClearinghouseState>;
    /// Open orders for a user.
    async fn open_orders(&self, user: &str) -> Result<Vec<OpenOrder>>;
    /// Status of a single order by id.
    async fn order_status(&self, user: &str, oid: u64) -> Result<OrderStatusResponse>;
    /// Spot account state: token balances.
    async fn spot_clearinghouse_state(&self, user: &str) -> Result<SpotClearinghouseState>;
    /// The user's funding payment history since `start_ms`.
    async fn user_funding(&self, user: &str, start_ms: u64) -> Result<Vec<UserFunding>>;
    /// The user's fills since `start_ms`.
    async fn user_fills_by_time(&self, user: &str, start_ms: u64) -> Result<Vec<UserFill>>;
    /// The user's effective fee schedule.
    async fn user_fees(&self, user: &str) -> Result<UserFees>;
    /// The user's address-based rate-limit budget.
    async fn user_rate_limit(&self, user: &str) -> Result<UserRateLimit>;
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

    async fn meta_for(&self, dex: &str) -> Result<Meta> {
        self.info(json!({ "type": "meta", "dex": dex })).await
    }

    async fn perp_dexs(&self) -> Result<Vec<PerpDex>> {
        let dexs: Vec<Option<PerpDex>> = self.info(json!({ "type": "perpDexs" })).await?;
        Ok(dexs.into_iter().flatten().collect())
    }

    async fn spot_meta(&self) -> Result<SpotMeta> {
        self.info(json!({ "type": "spotMeta" })).await
    }

    async fn all_mids(&self) -> Result<AllMids> {
        self.info(json!({ "type": "allMids" })).await
    }

    async fn all_mids_for(&self, dex: &str) -> Result<AllMids> {
        self.info(json!({ "type": "allMids", "dex": dex })).await
    }

    async fn l2_book(&self, coin: &str) -> Result<L2Book> {
        self.info(json!({ "type": "l2Book", "coin": coin })).await
    }

    async fn meta_and_asset_ctxs(&self) -> Result<MetaAndAssetCtxs> {
        let (meta, asset_ctxs): (Meta, Vec<AssetCtx>) =
            self.info(json!({ "type": "metaAndAssetCtxs" })).await?;
        Ok(MetaAndAssetCtxs { meta, asset_ctxs })
    }

    async fn clearinghouse_state(&self, user: &str) -> Result<ClearinghouseState> {
        self.info(json!({ "type": "clearinghouseState", "user": user }))
            .await
    }

    async fn open_orders(&self, user: &str) -> Result<Vec<OpenOrder>> {
        self.info(json!({ "type": "openOrders", "user": user }))
            .await
    }

    async fn order_status(&self, user: &str, oid: u64) -> Result<OrderStatusResponse> {
        self.info(json!({ "type": "orderStatus", "user": user, "oid": oid }))
            .await
    }

    async fn spot_clearinghouse_state(&self, user: &str) -> Result<SpotClearinghouseState> {
        self.info(json!({ "type": "spotClearinghouseState", "user": user }))
            .await
    }

    async fn user_funding(&self, user: &str, start_ms: u64) -> Result<Vec<UserFunding>> {
        self.info(json!({ "type": "userFunding", "user": user, "startTime": start_ms }))
            .await
    }

    async fn user_fills_by_time(&self, user: &str, start_ms: u64) -> Result<Vec<UserFill>> {
        self.info(json!({ "type": "userFillsByTime", "user": user, "startTime": start_ms }))
            .await
    }

    async fn user_fees(&self, user: &str) -> Result<UserFees> {
        self.info(json!({ "type": "userFees", "user": user })).await
    }

    async fn user_rate_limit(&self, user: &str) -> Result<UserRateLimit> {
        self.info(json!({ "type": "userRateLimit", "user": user }))
            .await
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
    async fn parses_perp_dexs_skipping_null_default() {
        let body = r#"[null,{"name":"xyz","fullName":"XYZ"},{"name":"cash"}]"#;
        let server = mount(body).await;
        let client = HttpInfo::with_base_url(server.uri());
        let dexs = client.perp_dexs().await.unwrap();
        assert_eq!(dexs.len(), 2);
        assert_eq!(dexs[0].name, "xyz");
        assert_eq!(dexs[1].name, "cash");
    }

    #[tokio::test]
    async fn parses_clearinghouse_state() {
        let body = r#"{
            "marginSummary":{"accountValue":"1234.5","totalNtlPos":"5000","totalRawUsd":"1200","totalMarginUsed":"300"},
            "crossMarginSummary":{"accountValue":"1234.5","totalNtlPos":"5000","totalRawUsd":"1200","totalMarginUsed":"300"},
            "withdrawable":"900.25",
            "assetPositions":[{"position":{"coin":"BTC","szi":"0.5","entryPx":"60000","positionValue":"30500","unrealizedPnl":"500","returnOnEquity":"0.05","marginUsed":"1500","leverage":{"type":"cross","value":20}}}]
        }"#;
        let server = mount(body).await;
        let client = HttpInfo::with_base_url(server.uri());
        let state = client.clearinghouse_state("0xabc").await.unwrap();
        assert_eq!(
            state.margin_summary.account_value,
            Decimal::from_str("1234.5").unwrap()
        );
        assert_eq!(state.withdrawable, Decimal::from_str("900.25").unwrap());
        let position = state.position("BTC").unwrap();
        assert_eq!(position.szi, Decimal::from_str("0.5").unwrap());
        assert_eq!(position.leverage.as_ref().unwrap().value, 20);
        assert!(state.position("ETH").is_none());
    }

    #[tokio::test]
    async fn parses_open_orders_and_status() {
        let orders = r#"[{"coin":"BTC","oid":42,"side":"B","limitPx":"50000","sz":"0.1","origSz":"0.1","timestamp":1700000000000,"reduceOnly":false,"cloid":"0x01"}]"#;
        let server = mount(orders).await;
        let client = HttpInfo::with_base_url(server.uri());
        let open = client.open_orders("0xabc").await.unwrap();
        assert_eq!(open.len(), 1);
        assert!(open[0].is_buy());
        assert_eq!(open[0].oid, 42);
    }

    #[tokio::test]
    async fn parses_user_fees() {
        let body = r#"{
            "feeSchedule":{"add":"0.00015","cross":"0.00045"},
            "dailyUserVlm":[{"date":"2026-09-01","userVlm":"100000"}],
            "userCrossRate":"0.0003",
            "userAddRate":"0.0001"
        }"#;
        let server = mount(body).await;
        let client = HttpInfo::with_base_url(server.uri());
        let fees = client.user_fees("0xabc").await.unwrap();
        assert_eq!(
            fees.fee_schedule.as_ref().unwrap().cross.as_deref(),
            Some("0.00045")
        );
        assert_eq!(
            fees.user_cross_rate,
            Some(Decimal::from_str("0.0003").unwrap())
        );
    }

    #[tokio::test]
    async fn parses_order_status_response() {
        let body = r#"{"status":"order","order":{"coin":"BTC","oid":7,"side":"A","limitPx":"60000","sz":"0.2","origSz":"0.2","timestamp":1,"reduceOnly":true}}"#;
        let server = mount(body).await;
        let client = HttpInfo::with_base_url(server.uri());
        let status = client.order_status("0xabc", 7).await.unwrap();
        assert_eq!(status.status, "order");
        assert!(!status.is_filled());
        assert_eq!(status.order.as_ref().unwrap().oid, 7);
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

    #[tokio::test]
    async fn parses_spot_clearinghouse_state() {
        let body = r#"{"balances":[
            {"coin":"USDC","token":0,"hold":"10","total":"1000.5","entryNtl":"0"},
            {"coin":"UBTC","token":1,"hold":"0","total":"0.25","entryNtl":"15000"}
        ]}"#;
        let server = mount(body).await;
        let client = HttpInfo::with_base_url(server.uri());
        let state = client.spot_clearinghouse_state("0xabc").await.unwrap();
        assert_eq!(state.balances.len(), 2);
        assert_eq!(
            state.balance("USDC").unwrap().total,
            Decimal::from_str("1000.5").unwrap()
        );
        assert_eq!(
            state.balance("UBTC").unwrap().total,
            Decimal::from_str("0.25").unwrap()
        );
        assert!(state.balance("ETH").is_none());
    }

    #[tokio::test]
    async fn parses_user_funding() {
        let body = r#"[{"time":1754450974231,"hash":"0xabc","delta":{"type":"funding","coin":"ETH","usdc":"-0.5123","szi":"2","rate":"0.0000125"}}]"#;
        let server = mount(body).await;
        let client = HttpInfo::with_base_url(server.uri());
        let funding = client.user_funding("0xabc", 1).await.unwrap();
        assert_eq!(funding.len(), 1);
        assert_eq!(funding[0].delta.coin, "ETH");
        assert_eq!(funding[0].delta.usdc, Decimal::from_str("-0.5123").unwrap());
        assert_eq!(
            funding[0].delta.rate,
            Decimal::from_str("0.0000125").unwrap()
        );
    }

    #[tokio::test]
    async fn parses_user_fills() {
        let body = r#"[{
            "coin":"BTC","px":"60000","sz":"0.01","side":"B","time":1754450974231,
            "closedPnl":"0","oid":42,"crossed":true,"fee":"0.27","tid":7,"dir":"Open Long"
        }]"#;
        let server = mount(body).await;
        let client = HttpInfo::with_base_url(server.uri());
        let fills = client.user_fills_by_time("0xabc", 1).await.unwrap();
        assert_eq!(fills.len(), 1);
        assert!(fills[0].is_buy());
        assert!(!fills[0].is_maker());
        assert_eq!(fills[0].oid, Some(42));
        assert_eq!(fills[0].fee, Decimal::from_str("0.27").unwrap());
    }
}
