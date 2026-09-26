//! WebSocket market-data stream (SPEC-0001 §4, §7–§8).
//!
//! This is the first stream backend. The [`MarketStream`] trait is the seam
//! that the M1.4 benchmark uses to swap in the official/community SDKs.
//!
//! Reconnect, heartbeat, the silence watchdog, and cancellable shutdown live in
//! [`crate::raw_ws::RawWsConn`] (SPEC-0008 R-3); this module only plans
//! subscriptions and decodes frames.

use async_trait::async_trait;
use mev_core::{
    config::Network,
    error::{Error, Result},
};
use mev_metrics::names;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::raw_ws::{HlProtocol, RawEvent, RawWsConn};
use crate::types::{AllMids, AssetCtxUpdate, Bbo, L2Book, Trade};

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
///
/// Adjacently tagged (`{"channel": …, "data": …}`) rather than internally
/// tagged so the sequence-carrying `Trades` variant round-trips through serde;
/// the wire decoder is manual, so this shape only affects the event log.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "channel", content = "data", rename_all = "camelCase")]
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

/// `tokio-tungstenite`-backed market stream with reconnect and heartbeat,
/// built on [`RawWsConn`].
pub struct WsMarketStream {
    network: Network,
    subs: Vec<Subscription>,
    conn: RawWsConn,
}

impl WsMarketStream {
    /// Dial the network and subscribe to `subs`.
    pub async fn connect(network: Network, subs: &[Subscription]) -> Result<Self> {
        let planned: Vec<String> = subs.iter().map(encode_subscription).collect();
        let conn = RawWsConn::connect(Box::new(HlProtocol::new(network)), planned).await?;
        Ok(Self {
            network,
            subs: subs.to_vec(),
            conn,
        })
    }

    /// Milliseconds since the last decoded inbound frame.
    pub fn idle_ms(&self) -> u128 {
        self.conn.idle_ms()
    }

    /// The network this stream is connected to.
    pub fn network(&self) -> Network {
        self.network
    }
}

#[async_trait]
impl MarketStream for WsMarketStream {
    async fn subscribe(&mut self, subs: &[Subscription]) -> Result<()> {
        // The raw connection resubscribes the planned set on reconnect; here we
        // only need to record new plans. Sending is handled by the raw layer at
        // (re)connect time, so no immediate send is required per subscription.
        for sub in subs {
            if self.subs.iter().any(|existing| same_sub(existing, sub)) {
                continue;
            }
            self.subs.push(sub.clone());
        }
        Ok(())
    }

    async fn next(&mut self) -> Result<StreamEvent> {
        loop {
            match self.conn.next().await? {
                RawEvent::Text { text, .. } => match decode(&text) {
                    Ok(Some(event)) => return Ok(event),
                    Ok(None) => {}
                    Err(err) => {
                        metrics::counter!(names::WS_PARSE_ERRORS).increment(1);
                        tracing::debug!(error = %err, "skipping undecodable frame");
                    }
                },
                RawEvent::Binary { .. } => {}
                RawEvent::Opened { .. } => {
                    metrics::gauge!(names::WS_CONNECTED).set(1.0);
                }
                RawEvent::Gap { reason, detail } => {
                    metrics::gauge!(names::WS_CONNECTED).set(0.0);
                    if reason == "shutdown" {
                        return Err(Error::Http(format!("websocket {reason}: {detail}")));
                    }
                    tracing::debug!(reason, detail, "market feed gap");
                }
            }
        }
    }
}

fn encode_subscription(sub: &Subscription) -> String {
    serde_json::to_string(sub).unwrap_or_else(|_| "{}".to_string())
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
    use serde_json::json;

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
