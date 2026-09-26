//! Typed ingest: decode WS frames straight into engine events (SPEC-0010 §6,
//! task E-2).
//!
//! The decoders here parse a text frame into a [`MarketUpdate`] in one pass,
//! without an intermediate `serde_json::Value` and without allocating coin
//! strings into the event: the frame's coin name is resolved to a [`CoinId`]
//! and only the numeric fields are kept. Ingest tasks render these as a
//! bounded channel; the engine drains it in its loop (E-3).

use std::collections::BTreeMap;

use mev_core::error::{Error, Result};
use smallvec::SmallVec;

use crate::types::{
    AssetCtxLite, BOOK_DEPTH, BookSnapshot, CoinId, CoinRegistry, ConnId, Level, MarketUpdate,
    Side, Stamp, Trade,
};

/// The set of coins an ingest connection is responsible for.
#[derive(Debug, Clone)]
pub struct IngestCoins {
    registry: CoinRegistry,
}

impl IngestCoins {
    /// Wrap a coin registry.
    pub fn new(registry: CoinRegistry) -> Self {
        Self { registry }
    }

    /// Resolve a coin name to its id, or `None` if it is not in the universe.
    pub fn id(&self, coin: &str) -> Option<CoinId> {
        self.registry.id(coin)
    }

    /// The registry itself.
    pub fn registry(&self) -> &CoinRegistry {
        &self.registry
    }
}

/// The channel tag of a frame, peeked without decoding the payload.
#[derive(serde::Deserialize)]
struct ChannelTag<'a> {
    #[serde(borrow)]
    channel: &'a str,
}

/// Decode one text frame into a [`MarketUpdate`], `Ok(None)` for channels the
/// engine does not consume (acks, pongs, allMids), or `Err` on malformed data.
///
/// `stamp` is attached by the caller immediately after the socket read.
///
/// The frame is parsed once into the channel-specific typed shape (borrowed
/// strings, no intermediate `serde_json::Value` of the payload); the envelope
/// tag is peeked first so the right shape is chosen.
pub fn decode_market(
    text: &str,
    coins: &IngestCoins,
    conn: ConnId,
    stamp: Stamp,
) -> Result<Option<MarketUpdate>> {
    let tag: ChannelTag<'_> =
        serde_json::from_str(text).map_err(|e| Error::Decode(e.to_string()))?;
    let update = match tag.channel {
        "l2Book" => decode_book(text, coins, stamp)?,
        "bbo" => decode_bbo(text, coins, stamp)?,
        "trades" => decode_trades(text, coins, stamp)?,
        "activeAssetCtx" => decode_ctx(text, coins, stamp)?,
        "subscription" | "pong" | "pongEvent" | "allMids" => return Ok(None),
        "error" => return Err(Error::Http("websocket error frame".into())),
        _ => return Ok(None),
    };
    let _ = conn;
    Ok(update)
}

/// The raw wire book, decoded directly into fixed arrays.
#[derive(serde::Deserialize)]
struct BookFrame<'a> {
    #[serde(borrow)]
    data: WireBook<'a>,
}

#[derive(serde::Deserialize)]
struct WireBook<'a> {
    #[serde(borrow)]
    coin: &'a str,
    time: u64,
    levels: [Vec<WireLevel>; 2],
}

#[derive(serde::Deserialize)]
struct WireLevel {
    px: rust_decimal::Decimal,
    sz: rust_decimal::Decimal,
    #[serde(default)]
    n: u32,
}

fn decode_book(text: &str, coins: &IngestCoins, stamp: Stamp) -> Result<Option<MarketUpdate>> {
    let frame: BookFrame<'_> =
        serde_json::from_str(text).map_err(|e| Error::Decode(e.to_string()))?;
    let book = frame.data;
    let Some(coin) = coins.id(book.coin) else {
        return Ok(None);
    };
    let mut snapshot = BookSnapshot {
        time_ms: book.time,
        ..Default::default()
    };
    snapshot.n_bids = fill_levels(&book.levels[0], &mut snapshot.bids);
    snapshot.n_asks = fill_levels(&book.levels[1], &mut snapshot.asks);
    Ok(Some(MarketUpdate::Book {
        coin,
        stamp,
        book: snapshot,
    }))
}

fn fill_levels(levels: &[WireLevel], out: &mut [Level; BOOK_DEPTH]) -> u8 {
    let mut n = 0u8;
    for level in levels.iter().take(BOOK_DEPTH) {
        out[n as usize] = Level {
            px: level.px,
            sz: level.sz,
            n: level.n,
        };
        n += 1;
    }
    n
}

#[derive(serde::Deserialize)]
struct BboFrame<'a> {
    #[serde(borrow)]
    data: WireBbo<'a>,
}

#[derive(serde::Deserialize)]
struct WireBbo<'a> {
    #[serde(borrow)]
    coin: &'a str,
    time: u64,
    bbo: [Option<WireLevel>; 2],
}

fn decode_bbo(text: &str, coins: &IngestCoins, stamp: Stamp) -> Result<Option<MarketUpdate>> {
    let frame: BboFrame<'_> =
        serde_json::from_str(text).map_err(|e| Error::Decode(e.to_string()))?;
    let bbo = frame.data;
    let Some(coin) = coins.id(bbo.coin) else {
        return Ok(None);
    };
    let to_level = |level: &Option<WireLevel>| match level {
        Some(level) => Level {
            px: level.px,
            sz: level.sz,
            n: level.n,
        },
        None => Level::default(),
    };
    let mut stamp = stamp;
    stamp.ts_exch_ms = bbo.time;
    Ok(Some(MarketUpdate::Bbo {
        coin,
        stamp,
        bid: to_level(&bbo.bbo[0]),
        ask: to_level(&bbo.bbo[1]),
    }))
}

#[derive(serde::Deserialize)]
struct TradesFrame<'a> {
    #[serde(borrow)]
    data: Vec<WireTrade<'a>>,
}

#[derive(serde::Deserialize)]
struct WireTrade<'a> {
    #[serde(borrow)]
    coin: &'a str,
    #[serde(borrow)]
    side: &'a str,
    px: rust_decimal::Decimal,
    sz: rust_decimal::Decimal,
    time: u64,
    #[serde(default)]
    tid: Option<u64>,
}

fn decode_trades(text: &str, coins: &IngestCoins, stamp: Stamp) -> Result<Option<MarketUpdate>> {
    let frame: TradesFrame<'_> =
        serde_json::from_str(text).map_err(|e| Error::Decode(e.to_string()))?;
    let trades = frame.data;
    let mut out: SmallVec<[Trade; 8]> = SmallVec::with_capacity(trades.len());
    let mut coin = None;
    for trade in &trades {
        let Some(id) = coins.id(trade.coin) else {
            continue;
        };
        coin.get_or_insert(id);
        out.push(Trade {
            side: parse_side(trade.side),
            px: trade.px,
            sz: trade.sz,
            time_ms: trade.time,
            tid: trade.tid,
        });
    }
    match coin {
        Some(coin) if !out.is_empty() => Ok(Some(MarketUpdate::Trades {
            coin,
            stamp,
            trades: out,
        })),
        _ => Ok(None),
    }
}

#[derive(serde::Deserialize)]
struct CtxFrame<'a> {
    #[serde(borrow)]
    data: WireCtx<'a>,
}

#[derive(serde::Deserialize)]
struct WireCtx<'a> {
    #[serde(borrow)]
    coin: &'a str,
    ctx: WireCtxInner,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireCtxInner {
    funding: rust_decimal::Decimal,
    open_interest: rust_decimal::Decimal,
    oracle_px: rust_decimal::Decimal,
    mark_px: rust_decimal::Decimal,
}

fn decode_ctx(text: &str, coins: &IngestCoins, stamp: Stamp) -> Result<Option<MarketUpdate>> {
    let frame: CtxFrame<'_> =
        serde_json::from_str(text).map_err(|e| Error::Decode(e.to_string()))?;
    let ctx = frame.data;
    let Some(coin) = coins.id(ctx.coin) else {
        return Ok(None);
    };
    Ok(Some(MarketUpdate::Ctx {
        coin,
        stamp,
        ctx: AssetCtxLite {
            funding: ctx.ctx.funding,
            mark_px: ctx.ctx.mark_px,
            oracle_px: ctx.ctx.oracle_px,
            open_interest: ctx.ctx.open_interest,
        },
    }))
}

/// Parse the venue side (`B`/`A`) into a [`Side`].
pub fn parse_side(side: &str) -> Side {
    if side.eq_ignore_ascii_case("B") {
        Side::Buy
    } else {
        Side::Sell
    }
}

/// A per-connection ingest coordinator: resolves coins and passes frames on.
///
/// Kept minimal in E-2; E-3 owns the engine thread that drains the channel.
#[derive(Debug, Clone)]
pub struct Ingest {
    coins: IngestCoins,
    conn: ConnId,
}

impl Ingest {
    /// Build an ingester for a connection over a coin universe.
    pub fn new(conn: ConnId, registry: CoinRegistry) -> Self {
        Self {
            coins: IngestCoins::new(registry),
            conn,
        }
    }

    /// Decode a text frame at `stamp`.
    pub fn decode(&self, text: &str, stamp: Stamp) -> Result<Option<MarketUpdate>> {
        decode_market(text, &self.coins, self.conn, stamp)
    }

    /// The coins this ingester resolves.
    pub fn coins(&self) -> &IngestCoins {
        &self.coins
    }
}

/// A market event paired with the coin it concerns, for routing/demuxing.
#[derive(Debug, Clone)]
pub struct RoutedMarket {
    /// The originating connection.
    pub conn: ConnId,
    /// The event.
    pub update: MarketUpdate,
}

/// A convenience map of coin → id for callers building an [`IngestCoins`].
#[allow(dead_code)]
pub fn coin_ids(coins: &[String]) -> BTreeMap<String, CoinId> {
    CoinRegistry::from_coins(coins)
        .iter()
        .map(|(id, coin)| (coin.to_string(), id))
        .collect()
}

#[cfg(test)]
mod tests {
    use rust_decimal::Decimal;
    use std::str::FromStr;

    use super::*;

    const L2BOOK: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../mev-hl-client/benches/fixtures/l2Book.jsonl"
    ));
    const TRADES: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../mev-hl-client/benches/fixtures/trades.jsonl"
    ));
    const CTX: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../mev-hl-client/benches/fixtures/activeAssetCtx.jsonl"
    ));

    fn ds(value: &str) -> Decimal {
        Decimal::from_str(value).unwrap()
    }

    fn ingester() -> Ingest {
        Ingest::new(
            ConnId(0),
            CoinRegistry::from_coins(&[
                "BTC".into(),
                "ETH".into(),
                "SOL".into(),
                "xyz:TSLA".into(),
            ]),
        )
    }

    fn first_frame(raw: &str) -> &str {
        raw.lines().next().unwrap()
    }

    #[test]
    fn decodes_book_into_fixed_snapshot() {
        let update = ingester()
            .decode(first_frame(L2BOOK), Stamp::default())
            .unwrap()
            .unwrap();
        match update {
            MarketUpdate::Book { coin, book, .. } => {
                assert_eq!(coin, CoinId(0));
                assert!(book.n_bids > 0 && book.n_asks > 0);
                let bid = book.best_bid().unwrap();
                let ask = book.best_ask().unwrap();
                assert!(bid.px > Decimal::ZERO);
                assert!(ask.px > bid.px, "ask {} <= bid {}", ask.px, bid.px);
                assert!(book.time_ms > 0);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn decodes_trades_batch_with_ids() {
        let update = ingester()
            .decode(first_frame(TRADES), Stamp::default())
            .unwrap()
            .unwrap();
        match update {
            MarketUpdate::Trades { trades, .. } => {
                assert!(!trades.is_empty());
                assert!(trades[0].tid.is_some());
                assert!(trades[0].px > Decimal::ZERO);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn decodes_context_fields() {
        let update = ingester()
            .decode(first_frame(CTX), Stamp::default())
            .unwrap()
            .unwrap();
        match update {
            MarketUpdate::Ctx { ctx, .. } => {
                assert_eq!(ctx.funding, ds("0.0000125"));
                assert!(ctx.mark_px > Decimal::ZERO);
                assert!(ctx.oracle_px > Decimal::ZERO);
                assert!(ctx.open_interest > Decimal::ZERO);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn unknown_coins_are_skipped_not_errors() {
        let update = ingester()
            .decode(
                r#"{"channel":"l2Book","data":{"coin":"DOGE","time":1,"levels":[[],[]]}}"#,
                Stamp::default(),
            )
            .unwrap();
        assert!(update.is_none());
    }

    #[test]
    fn ack_channels_and_errors_are_handled() {
        assert!(
            ingester()
                .decode(r#"{"channel":"subscription","data":{}}"#, Stamp::default())
                .unwrap()
                .is_none()
        );
        assert!(
            ingester()
                .decode(
                    r#"{"channel":"allMids","data":{"mids":{}}}"#,
                    Stamp::default()
                )
                .unwrap()
                .is_none()
        );
        assert!(
            ingester()
                .decode(r#"{"channel":"error","data":"bad"}"#, Stamp::default())
                .is_err()
        );
    }

    #[test]
    fn stamp_is_carried_through() {
        let stamp = Stamp {
            t_recv_ns: 11,
            mono_ns: 22,
            ts_exch_ms: 33,
        };
        match ingester().decode(first_frame(TRADES), stamp).unwrap() {
            Some(MarketUpdate::Trades { stamp: got, .. }) => assert_eq!(got, stamp),
            other => panic!("unexpected {other:?}"),
        }
    }
}
