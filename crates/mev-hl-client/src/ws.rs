//! WebSocket market-data stream (SPEC-0001 §4, §7–§8).
//!
//! This is the first stream backend. The [`MarketStream`] trait is the seam
//! that the M1.4 benchmark uses to swap in the official/community SDKs.

use std::time::Duration;

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use mev_core::{
    config::Network,
    error::{Error, Result},
};
use mev_metrics::names;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::json;
use tokio::net::TcpStream;
use tokio::time::Instant;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

use crate::types::{AllMids, AssetCtxUpdate, Bbo, L2Book, Trade};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

const PING_INTERVAL: Duration = Duration::from_secs(30);
const BACKOFF_BASE: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
const BACKOFF_MAX_SHIFT: u32 = 6;

/// A Hyperliquid WS subscription request.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Subscription {
    /// All mid prices.
    AllMids,
    /// L2 book snapshots for a coin.
    L2Book {
        /// Coin (dex-qualified for HIP-3, e.g. `xyz:TSLA`).
        coin: String,
    },
    /// Best bid/offer updates for a coin.
    Bbo {
        /// Coin.
        coin: String,
    },
    /// Public trades for a coin.
    Trades {
        /// Coin.
        coin: String,
    },
    /// Mark/mid/funding/OI updates for a coin.
    ActiveAssetCtx {
        /// Coin.
        coin: String,
    },
    /// Candles for a coin.
    Candle {
        /// Coin.
        coin: String,
        /// Candle interval, e.g. `1m`.
        interval: String,
    },
}

/// A decoded market-data event.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// Mid prices for all coins.
    Mids(AllMids),
    /// L2 book snapshot.
    Book(L2Book),
    /// Best bid/offer.
    Bbo(Bbo),
    /// Batch of trades.
    Trades(Vec<Trade>),
    /// Asset context update.
    AssetCtx(AssetCtxUpdate),
}

/// A reconnect-capable market-data stream.
#[async_trait]
pub trait MarketStream: Send {
    /// Subscribe to additional channels.
    async fn subscribe(&mut self, subs: &[Subscription]) -> Result<()>;
    /// Await the next decoded event.
    async fn next(&mut self) -> Result<StreamEvent>;
}

/// `tokio-tungstenite`-backed market stream with reconnect and heartbeat.
pub struct WsMarketStream {
    network: Network,
    subs: Vec<Subscription>,
    socket: Socket,
    ping: tokio::time::Interval,
    last_data: Instant,
}

impl WsMarketStream {
    /// Dial the network and subscribe to `subs`.
    pub async fn connect(network: Network, subs: &[Subscription]) -> Result<Self> {
        let socket = Self::dial(network).await?;
        let mut stream = Self {
            network,
            subs: Vec::new(),
            socket,
            ping: tokio::time::interval_at(Instant::now() + PING_INTERVAL, PING_INTERVAL),
            last_data: Instant::now(),
        };
        stream.subscribe(subs).await?;
        Ok(stream)
    }

    /// Milliseconds since the last decoded inbound frame.
    pub fn idle_ms(&self) -> u128 {
        self.last_data.elapsed().as_millis()
    }

    async fn dial(network: Network) -> Result<Socket> {
        ensure_crypto_provider();
        let (socket, _resp) = connect_async(network.ws_url())
            .await
            .map_err(|e| Error::Http(e.to_string()))?;
        metrics::gauge!(names::WS_CONNECTED).set(1.0);
        Ok(socket)
    }

    async fn send_sub(&mut self, sub: &Subscription) -> Result<()> {
        let msg = json!({ "method": "subscribe", "subscription": sub }).to_string();
        self.socket
            .send(Message::Text(msg.into()))
            .await
            .map_err(|e| Error::Http(e.to_string()))
    }

    async fn reconnect(&mut self) -> Result<()> {
        metrics::gauge!(names::WS_CONNECTED).set(0.0);
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            let shift = attempt.min(BACKOFF_MAX_SHIFT) - 1;
            let backoff = BACKOFF_BASE.saturating_mul(1 << shift).min(BACKOFF_MAX);
            tokio::time::sleep(backoff).await;

            match Self::dial(self.network).await {
                Ok(socket) => {
                    self.socket = socket;
                    metrics::counter!(names::WS_RECONNECTS, "reason" => "reconnect").increment(1);
                    tracing::info!(attempt, "websocket reconnected");
                    let subs = self.subs.clone();
                    for sub in &subs {
                        self.send_sub(sub).await?;
                    }
                    self.last_data = Instant::now();
                    return Ok(());
                }
                Err(err) => {
                    tracing::warn!(attempt, error = %err, "websocket reconnect failed");
                }
            }
        }
    }
}

#[async_trait]
impl MarketStream for WsMarketStream {
    async fn subscribe(&mut self, subs: &[Subscription]) -> Result<()> {
        for sub in subs {
            if self.subs.iter().any(|existing| same_sub(existing, sub)) {
                continue;
            }
            self.send_sub(sub).await?;
            self.subs.push(sub.clone());
        }
        Ok(())
    }

    async fn next(&mut self) -> Result<StreamEvent> {
        loop {
            tokio::select! {
                msg = self.socket.next() => match msg {
                    Some(Ok(Message::Text(text))) => {
                        self.last_data = Instant::now();
                        match decode(&text) {
                            Ok(Some(event)) => return Ok(event),
                            Ok(None) => {}
                            Err(err) => {
                                metrics::counter!(names::WS_PARSE_ERRORS).increment(1);
                                tracing::debug!(error = %err, "skipping undecodable frame");
                            }
                        }
                    }
                    Some(Ok(Message::Binary(_))) | Some(Ok(Message::Frame(_))) => {}
                    Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(Message::Close(_))) | None => {
                        tracing::warn!("websocket closed; reconnecting");
                        self.reconnect().await?;
                    }
                    Some(Err(err)) => {
                        tracing::warn!(error = %err, "websocket error; reconnecting");
                        self.reconnect().await?;
                    }
                },
                _ = self.ping.tick() => {
                    let ping = json!({ "method": "ping" }).to_string();
                    if let Err(err) = self.socket.send(Message::Text(ping.into())).await {
                        tracing::warn!(error = %err, "ping failed; reconnecting");
                        self.reconnect().await?;
                    }
                }
            }
        }
    }
}

/// Install the process-wide rustls crypto provider once (idempotent).
pub(crate) fn ensure_crypto_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

fn same_sub(a: &Subscription, b: &Subscription) -> bool {
    serde_json::to_value(a).ok() == serde_json::to_value(b).ok()
}

#[derive(serde::Deserialize)]
struct Envelope {
    channel: String,
    data: serde_json::Value,
}

/// Decode one WS text frame into an event, `Ok(None)` for acks/ignored
/// channels, or `Err` for malformed frames and server errors.
pub fn decode(text: &str) -> Result<Option<StreamEvent>> {
    let envelope: Envelope =
        serde_json::from_str(text).map_err(|e| Error::Decode(e.to_string()))?;

    let event = match envelope.channel.as_str() {
        "l2Book" => StreamEvent::Book(from_value(envelope.data)?),
        "bbo" => StreamEvent::Bbo(from_value(envelope.data)?),
        "trades" => StreamEvent::Trades(from_value(envelope.data)?),
        "activeAssetCtx" => StreamEvent::AssetCtx(from_value(envelope.data)?),
        "allMids" => StreamEvent::Mids(parse_mids(envelope.data)?),
        "subscription" | "pong" | "pongEvent" => return Ok(None),
        "error" => {
            return Err(Error::Http(format!("websocket error: {}", envelope.data)));
        }
        other => {
            tracing::debug!(channel = other, "ignoring unknown channel");
            return Ok(None);
        }
    };

    metrics::counter!(names::WS_MSGS, "channel" => envelope.channel).increment(1);
    Ok(Some(event))
}

fn from_value<T: DeserializeOwned>(value: serde_json::Value) -> Result<T> {
    serde_json::from_value(value).map_err(|e| Error::Decode(e.to_string()))
}

fn parse_mids(value: serde_json::Value) -> Result<AllMids> {
    let inner = value.get("mids").cloned().unwrap_or(value);
    from_value(inner)
}

#[cfg(test)]
mod tests {
    use rust_decimal::Decimal;

    use super::*;

    #[test]
    fn subscription_serializes_with_type_tag() {
        let sub = Subscription::L2Book {
            coin: "xyz:TSLA".into(),
        };
        assert_eq!(
            serde_json::to_value(&sub).unwrap(),
            json!({ "type": "l2Book", "coin": "xyz:TSLA" })
        );
        assert_eq!(
            serde_json::to_value(Subscription::AllMids).unwrap(),
            json!({ "type": "allMids" })
        );
    }

    #[test]
    fn decodes_l2_book() {
        let frame = r#"{"channel":"l2Book","data":{"coin":"BTC","time":1,
            "levels":[[{"px":"100","sz":"1","n":1}],[{"px":"101","sz":"2","n":1}]]}}"#;
        match decode(frame).unwrap() {
            Some(StreamEvent::Book(book)) => {
                assert_eq!(book.coin, "BTC");
                assert_eq!(book.mid().unwrap(), Decimal::new(1005, 1));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn decodes_trades_batch() {
        let frame = r#"{"channel":"trades","data":[
            {"coin":"BTC","side":"B","px":"100","sz":"0.5","time":1,"tid":7}]}"#;
        match decode(frame).unwrap() {
            Some(StreamEvent::Trades(trades)) => {
                assert_eq!(trades.len(), 1);
                assert_eq!(trades[0].tid, Some(7));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn decodes_active_asset_ctx() {
        let frame = r#"{"channel":"activeAssetCtx","data":{"coin":"BTC","ctx":{
            "funding":"0.0000125","openInterest":"123.4","prevDayPx":"99",
            "dayNtlVlm":"1000000","premium":"0.0001","oraclePx":"100",
            "markPx":"100.1","midPx":"100.05"}}}"#;
        match decode(frame).unwrap() {
            Some(StreamEvent::AssetCtx(update)) => {
                assert_eq!(update.coin, "BTC");
                assert_eq!(update.ctx.funding, Decimal::new(125, 7));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn acks_and_unknown_channels_are_ignored() {
        assert!(
            decode(r#"{"channel":"subscription","data":{"type":"l2Book"}}"#)
                .unwrap()
                .is_none()
        );
        assert!(decode(r#"{"channel":"pong","data":{}}"#).unwrap().is_none());
    }

    #[test]
    fn server_errors_surface() {
        let frame = r#"{"channel":"error","data":"invalid subscription"}"#;
        assert!(decode(frame).is_err());
    }

    #[test]
    fn malformed_frames_error_without_panic() {
        assert!(decode("not json").is_err());
        assert!(decode(r#"{"channel":"l2Book","data":{"coin":"BTC"}}"#).is_err());
    }
}
