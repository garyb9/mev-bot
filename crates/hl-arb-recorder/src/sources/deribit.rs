//! Deribit public options source (SPEC-0008 §9.1, task R-12).
//!
//! Polls the public, unauthenticated Deribit JSON-RPC endpoints every
//! `poll_interval` for each configured currency and records the exact response
//! body as a `kind:"rest"` envelope under `src:"deribit"`, mirroring
//! [`crate::sources::hl_rest`]. Only public endpoints are used: no keys, no
//! auth, no env secrets.
//!
//! Per V-10 (2026-09-28) the polled endpoints are the per-currency options
//! summary (`public/get_book_summary_by_currency`) and the index price
//! (`public/get_index_price`). The per-instrument `public/ticker` fan-out
//! (which alone carries `bid_iv`/`ask_iv`/greeks) and `public/get_instruments`
//! are not polled here; see SPEC-0008 §17 #33.
//!
//! # Rate limits (V-10)
//!
//! Deribit's public calls are per-IP: 20 req/s sustained / 100 burst by
//! default, and `public/get_instruments` is capped at 1 req/s sustained /
//! 50 burst. The default cadence is two requests per currency per 60 s, far
//! below both, so the interval itself is the rate limit and no token bucket is
//! needed.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use thiserror::Error;
use tokio::sync::Notify;
use tokio::time::MissedTickBehavior;
use tracing::{debug, warn};

use crate::envelope::{Envelope, EnvelopeClock};
use crate::sources::hl_rest::{EnvelopeSink, RawResponse};

/// Default Deribit JSON-RPC base URL (SPEC-0008 §9.1, V-10).
pub const DEFAULT_BASE_URL: &str = "https://www.deribit.com/api/v2";

/// Default poll cadence per currency (SPEC-0008 §9.1).
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(60);

/// Errors raised by the Deribit source.
#[derive(Debug, Error)]
pub enum DeribitError {
    /// The HTTP request failed before a response was received.
    #[error("deribit transport error: {0}")]
    Transport(String),
}

/// A raw Deribit JSON-RPC-over-GET client.
#[derive(Debug, Clone)]
pub struct DeribitClient {
    client: reqwest::Client,
    base_url: String,
}

impl DeribitClient {
    /// Build a client against a JSON-RPC base URL.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into(),
        }
    }

    /// GET `{base}/{method}?{params}`, capturing the raw response text and
    /// latency. A non-2xx status is returned as a successful [`RawResponse`]
    /// (the body is still recorded); only transport failures yield an error.
    pub async fn get(
        &self,
        method: &str,
        params: &[(String, String)],
    ) -> Result<RawResponse, DeribitError> {
        let url = format!("{}/{}", self.base_url.trim_end_matches('/'), method);
        let started = Instant::now();
        let response = self
            .client
            .get(&url)
            .query(params)
            .send()
            .await
            .map_err(|err| DeribitError::Transport(err.to_string()))?;
        let status = response.status().as_u16();
        let body = response
            .text()
            .await
            .map_err(|err| DeribitError::Transport(err.to_string()))?;
        Ok(RawResponse {
            status,
            body,
            latency_us: started.elapsed().as_micros() as u64,
        })
    }
}

/// Deribit source configuration (SPEC-0008 §9.1).
#[derive(Debug, Clone)]
pub struct DeribitConfig {
    /// JSON-RPC base URL.
    pub base_url: String,
    /// Source id written to envelopes (default `deribit`).
    pub src: String,
    /// Connection id written to envelopes (default `deribit`).
    pub conn: String,
    /// Currencies to poll (`BTC`, `ETH`, …).
    pub currencies: Vec<String>,
    /// Option kind passed to the summary call (default `option`).
    pub kind: String,
    /// Poll cadence per currency.
    pub poll_interval: Duration,
}

impl Default for DeribitConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            src: "deribit".to_string(),
            conn: "deribit".to_string(),
            currencies: vec!["BTC".to_string(), "ETH".to_string()],
            kind: "option".to_string(),
            poll_interval: DEFAULT_POLL_INTERVAL,
        }
    }
}

/// One Deribit JSON-RPC request to issue in a poll round.
struct DeribitRequest {
    method: &'static str,
    params: Vec<(String, String)>,
}

impl DeribitRequest {
    fn new(method: &'static str, params: Vec<(String, String)>) -> Self {
        Self { method, params }
    }
}

/// The Deribit public-options poller (SPEC-0008 §9.1, task R-12).
pub struct DeribitSource {
    client: DeribitClient,
    sink: Arc<dyn EnvelopeSink>,
    clock: Arc<dyn EnvelopeClock>,
    config: DeribitConfig,
    seq: u64,
}

impl DeribitSource {
    /// Build a source that writes envelopes to `sink` and stamps them with
    /// `clock`.
    pub fn new(
        config: DeribitConfig,
        sink: Arc<dyn EnvelopeSink>,
        clock: Arc<dyn EnvelopeClock>,
    ) -> Self {
        Self {
            client: DeribitClient::new(config.base_url.clone()),
            sink,
            clock,
            config,
            seq: 0,
        }
    }

    /// Run until `shutdown` is notified.
    ///
    /// A failed poll is logged and never stops the loop.
    pub async fn run(mut self, shutdown: Arc<Notify>) {
        let mut interval = tokio::time::interval(self.config.poll_interval);
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = interval.tick() => {}
                _ = shutdown.notified() => break,
            }
            self.poll_round().await;
        }
        debug!("deribit source stopped");
    }

    /// Issue every configured request for one poll round.
    async fn poll_round(&mut self) {
        for request in self.requests() {
            self.fetch(&request).await;
        }
    }

    /// The requests for one round: for each currency, one options summary and
    /// one index price (V-10).
    fn requests(&self) -> Vec<DeribitRequest> {
        let mut requests = Vec::with_capacity(self.config.currencies.len() * 2);
        for currency in &self.config.currencies {
            requests.push(DeribitRequest::new(
                "public/get_book_summary_by_currency",
                vec![
                    ("currency".to_string(), currency.clone()),
                    ("kind".to_string(), self.config.kind.clone()),
                ],
            ));
            requests.push(DeribitRequest::new(
                "public/get_index_price",
                vec![(
                    "index_name".to_string(),
                    format!("{}_usd", currency.to_lowercase()),
                )],
            ));
        }
        requests
    }

    /// Fetch one request, recording its response or logging a transport error.
    async fn fetch(&mut self, request: &DeribitRequest) {
        match self.client.get(request.method, &request.params).await {
            Ok(response) => self.record_rest(request, &response),
            Err(err) => {
                warn!(
                    src = %self.config.src,
                    method = request.method,
                    error = %err,
                    "deribit request failed"
                );
            }
        }
    }

    /// Record a REST envelope with the raw response body preserved verbatim.
    fn record_rest(&mut self, request: &DeribitRequest, response: &RawResponse) {
        let params: serde_json::Map<String, Value> = request
            .params
            .iter()
            .map(|(key, value)| (key.clone(), Value::String(value.clone())))
            .collect();
        let meta = json!({
            "req": { "method": request.method, "params": params },
            "status": response.status,
            "latency_us": response.latency_us,
        });
        let env = Envelope::rest(
            &*self.clock,
            &self.config.src,
            &self.config.conn,
            self.seq,
            response.body.clone(),
            meta,
        );
        self.seq += 1;
        if !self.sink.send(env) {
            warn!(
                src = %self.config.src,
                conn = %self.config.conn,
                "deribit rest envelope dropped"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use tokio::sync::mpsc;
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

    use super::*;
    use crate::envelope::{Kind, SystemEnvelopeClock};

    /// A sink that forwards envelopes to a channel so tests can await them.
    struct ChannelSink {
        tx: Mutex<mpsc::UnboundedSender<Envelope>>,
    }

    impl EnvelopeSink for ChannelSink {
        fn send(&self, env: Envelope) -> bool {
            match self.tx.lock() {
                Ok(tx) => tx.send(env).is_ok(),
                Err(_) => false,
            }
        }
    }

    fn channel_sink() -> (Arc<dyn EnvelopeSink>, mpsc::UnboundedReceiver<Envelope>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Arc::new(ChannelSink { tx: Mutex::new(tx) }), rx)
    }

    fn config_for(uri: String) -> DeribitConfig {
        DeribitConfig {
            base_url: uri,
            currencies: vec!["BTC".into()],
            ..DeribitConfig::default()
        }
    }

    fn envelope_meta(env: &Envelope) -> &Value {
        env.meta.as_ref().expect("rest envelope has meta")
    }

    #[tokio::test]
    async fn records_raw_body_byte_for_byte() {
        let server = MockServer::start().await;
        let body = "{\"result\":{\"mark_iv\":55.5},\"note\":\"quote \\\" newline\\n unicode ✓\"}";
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        let (sink, mut rx) = channel_sink();
        let clock: Arc<dyn EnvelopeClock> = Arc::new(SystemEnvelopeClock::new());
        let source = DeribitSource::new(config_for(server.uri()), sink, clock);
        let shutdown = Arc::new(Notify::new());
        let handle = tokio::spawn(source.run(shutdown.clone()));

        let env = rx.recv().await.unwrap();
        shutdown.notify_one();
        handle.await.unwrap();

        assert_eq!(env.kind, Kind::Rest);
        assert_eq!(env.src, "deribit");
        assert_eq!(env.conn, "deribit");
        assert_eq!(env.raw.as_deref(), Some(body));
        let meta = envelope_meta(&env);
        assert_eq!(meta["status"], 200);
        assert!(meta["latency_us"].is_number());
        assert_eq!(meta["req"]["method"], "public/get_book_summary_by_currency");
        assert_eq!(meta["req"]["params"]["currency"], "BTC");
        assert_eq!(meta["req"]["params"]["kind"], "option");
    }

    #[tokio::test(start_paused = true)]
    async fn polls_every_endpoint_per_currency_each_interval() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(&server)
            .await;

        let (sink, mut rx) = channel_sink();
        let clock: Arc<dyn EnvelopeClock> = Arc::new(SystemEnvelopeClock::new());
        let config = DeribitConfig {
            base_url: server.uri(),
            currencies: vec!["BTC".into(), "ETH".into()],
            ..DeribitConfig::default()
        };
        let source = DeribitSource::new(config, sink, clock);
        let shutdown = Arc::new(Notify::new());
        let handle = tokio::spawn(source.run(shutdown.clone()));

        // One round = 2 currencies * 2 endpoints.
        const PER_ROUND: usize = 4;
        const ROUNDS: usize = 3;

        let mut envelopes = Vec::new();
        for round in 0..ROUNDS {
            for _ in 0..PER_ROUND {
                envelopes.push(rx.recv().await.unwrap());
            }
            if round + 1 < ROUNDS {
                tokio::task::yield_now().await;
                tokio::time::advance(Duration::from_secs(60)).await;
            }
        }
        shutdown.notify_one();
        handle.await.unwrap();

        let count = |method: &str, key: &str, value: &str| {
            envelopes
                .iter()
                .filter(|env| {
                    let req = &envelope_meta(env)["req"];
                    req["method"] == method && req["params"][key] == value
                })
                .count()
        };
        for currency in ["BTC", "ETH"] {
            assert_eq!(
                count("public/get_book_summary_by_currency", "currency", currency),
                ROUNDS,
                "summary {currency}"
            );
        }
        assert_eq!(
            count("public/get_index_price", "index_name", "btc_usd"),
            ROUNDS
        );
        assert_eq!(
            count("public/get_index_price", "index_name", "eth_usd"),
            ROUNDS
        );
    }

    #[tokio::test(start_paused = true)]
    async fn http_error_does_not_stop_polling() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let (sink, mut rx) = channel_sink();
        let clock: Arc<dyn EnvelopeClock> = Arc::new(SystemEnvelopeClock::new());
        let source = DeribitSource::new(config_for(server.uri()), sink, clock);
        let shutdown = Arc::new(Notify::new());
        let handle = tokio::spawn(source.run(shutdown.clone()));

        // Both requests of the first round fail with HTTP 500 and are recorded.
        let first = [rx.recv().await.unwrap(), rx.recv().await.unwrap()];
        for env in &first {
            assert_eq!(envelope_meta(env)["status"], 500);
            assert_eq!(env.raw.as_deref(), Some("boom"));
        }

        // The 500 did not stop the loop: advance one cadence and poll again.
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(60)).await;
        let second = [rx.recv().await.unwrap(), rx.recv().await.unwrap()];
        for env in &second {
            assert_eq!(envelope_meta(env)["status"], 500);
        }

        shutdown.notify_one();
        handle.await.unwrap();
    }
}
