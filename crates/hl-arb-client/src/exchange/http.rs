//! REST `POST /exchange` transport and the signed request envelope (SPEC-0002 §9).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Serialize;
use serde_json::{Value, json};

use hl_arb_core::config::{Mode, Network};
use hl_arb_core::db::Db;
use hl_arb_core::error::{Error, Result};

use crate::order::Action;
use crate::signing::{AgentSigner, Signature};

use super::{
    ActionResponse, ExchangeApi, ExchangeResponse, HTTP_CONNECT_TIMEOUT, HTTP_REQUEST_TIMEOUT,
    Prepared, WriteCore,
};

/// A signed `/exchange` request payload.
///
/// The action is stored (not converted to a `Value`) so that its JSON field
/// order matches the msgpack order used for the hash: the venue re-encodes the
/// action it receives to verify the signature, and that encoding is
/// order-sensitive.
#[derive(Debug, Clone, Serialize)]
pub struct ExchangeRequest {
    /// The action (serialized inline).
    pub action: Action,
    /// Nonce (ms timestamp).
    pub nonce: u64,
    /// Signature over the action.
    pub signature: Signature,
    /// Optional vault address.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vault_address: Option<String>,
    /// Optional expiry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_after: Option<u64>,
}

/// Build the signed request envelope for an L1 action.
pub fn build_request(
    action: &Action,
    signer: &AgentSigner,
    nonce: u64,
    vault_address: Option<String>,
    expires_after: Option<u64>,
) -> Result<ExchangeRequest> {
    let vault = match &vault_address {
        Some(addr) => Some(
            addr.parse()
                .map_err(|e| Error::Config(format!("invalid vault address `{addr}`: {e}")))?,
        ),
        None => None,
    };
    let signature = signer.sign_l1(action, nonce, vault, expires_after)?;
    Ok(ExchangeRequest {
        action: action.clone(),
        nonce,
        signature,
        vault_address,
        expires_after,
    })
}

/// Execution mode gate: `observe` must never write to `/exchange`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteGate {
    /// Writes are disabled entirely (observe).
    Blocked,
    /// Writes are built and signed but not sent (simulate).
    DryRun,
    /// Writes are sent (live).
    Open,
}

impl From<Mode> for WriteGate {
    fn from(mode: Mode) -> Self {
        match mode {
            Mode::Observe => WriteGate::Blocked,
            Mode::Simulate => WriteGate::DryRun,
            Mode::Live => WriteGate::Open,
        }
    }
}

/// REST `POST /exchange` transport implementing [`ExchangeApi`].
pub struct HttpExchange {
    client: reqwest::Client,
    base_url: String,
    core: WriteCore,
    /// Cached `hl_submit_ack_seconds{transport="rest"}` handle (SPEC-0002 H-7).
    submit_histogram: metrics::Histogram,
}

impl HttpExchange {
    /// Create a client for the given network and mode.
    ///
    /// `signer` is required for `simulate`/`live`; it may be `None` in
    /// `observe`, where no writes are possible.
    pub fn new(network: Network, mode: Mode, signer: Option<AgentSigner>) -> Result<Self> {
        Self::with_base_url(network.rest_url(), mode, signer)
    }

    /// Create a client against an explicit base URL (tests).
    pub fn with_base_url(
        base_url: impl Into<String>,
        mode: Mode,
        signer: Option<AgentSigner>,
    ) -> Result<Self> {
        Self::with_base_url_and_timeout(
            base_url,
            mode,
            signer,
            HTTP_CONNECT_TIMEOUT,
            HTTP_REQUEST_TIMEOUT,
        )
    }

    /// Create a client against an explicit base URL with explicit connect and
    /// total request timeouts (tests that prove a black-holed server cannot
    /// hang a write; SEC-001).
    pub fn with_base_url_and_timeout(
        base_url: impl Into<String>,
        mode: Mode,
        signer: Option<AgentSigner>,
        connect_timeout: Duration,
        request_timeout: Duration,
    ) -> Result<Self> {
        Ok(Self {
            client: super::http_client_with(connect_timeout, request_timeout),
            base_url: base_url.into(),
            core: WriteCore::new(mode, signer)?,
            submit_histogram: metrics::histogram!(
                hl_arb_metrics::names::SUBMIT_ACK_SECONDS,
                "transport" => "rest"
            ),
        })
    }

    /// Attach a durable nonce store and restore the persisted high-water mark.
    pub fn with_nonce_db(mut self, db: Arc<Mutex<Db>>) -> Result<Self> {
        self.core = self.core.with_nonce_db(db)?;
        Ok(self)
    }

    /// The effective write gate.
    pub fn gate(&self) -> WriteGate {
        self.core.gate()
    }

    /// Set the optional `expiresAfter` field applied to every action.
    pub fn with_expires_after(mut self, expires_after: Option<u64>) -> Self {
        self.core = self.core.with_expires_after(expires_after);
        self
    }

    /// Set the optional vault address applied to every action.
    pub fn with_vault_address(mut self, vault_address: Option<String>) -> Self {
        self.core = self.core.with_vault_address(vault_address);
        self
    }

    /// Restore the persisted nonce high-water mark (operator path).
    pub async fn restore_nonce(&self, last: u64) -> Result<()> {
        self.core.restore_nonce(last).await
    }

    /// Reset a corrupt persisted nonce (operator path).
    pub async fn reset_nonce(&self) -> Result<()> {
        self.core.reset_nonce().await
    }

    /// The current nonce high-water mark (for persistence).
    pub async fn last_nonce(&self) -> u64 {
        self.core.last_nonce().await
    }

    /// Resync the nonce after a stale/duplicate/recent-window rejection.
    pub async fn heal_nonce(&self) -> Result<u64> {
        self.core.heal_nonce().await
    }

    /// Build, sign, and (if allowed) POST an action.
    async fn send(&self, action: &Action) -> Result<ActionResponse> {
        let request = match self.core.prepare(action).await? {
            Prepared::DryRun(request) => {
                return Ok(ActionResponse {
                    value: json!({ "status": "simulated", "request": request }),
                });
            }
            Prepared::Send(request) => request,
        };

        let url = format!("{}/exchange", self.base_url.trim_end_matches('/'));
        let started = Instant::now();
        let resp = self
            .client
            .post(&url)
            .json(&request)
            .send()
            .await
            .map_err(|e| {
                // A connect failure (including a connect timeout) means the
                // action never left the host (`NotSent`); a timeout after the
                // connection was up leaves the outcome unknown and must be
                // reconciled, never resent (SPEC-0002 H-1/H-2, SEC-001).
                if e.is_connect() {
                    Error::NotSent(format!("exchange request failed to connect: {e}"))
                } else if e.is_timeout() {
                    Error::UnknownOutcome(format!("exchange request timed out: {e}"))
                } else {
                    Error::Http(e.to_string())
                }
            })?;
        // Submit-to-ack: request written through the venue's response
        // (SPEC-0002 H-7, `transport="rest"`).
        self.submit_histogram
            .record(started.elapsed().as_secs_f64());

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(Error::Http(format!("POST /exchange -> {status}: {text}")));
        }

        // The response arrived but could not be parsed: reconcile, don't reject.
        let response: ExchangeResponse = resp
            .json()
            .await
            .map_err(|e| Error::UnknownOutcome(format!("undecodable post reply: {e}")))?;

        if !response.is_ok() {
            let message = response
                .error_message()
                .unwrap_or_else(|| "unknown".to_string());
            return Err(Error::Exchange(message));
        }

        Ok(ActionResponse {
            value: response.response.unwrap_or(Value::Null),
        })
    }
}

#[async_trait]
impl ExchangeApi for HttpExchange {
    async fn submit(&self, action: &Action) -> Result<ActionResponse> {
        self.send(action).await
    }
}
