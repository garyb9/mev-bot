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
    AccountUpdate, AssetCtxLite, BOOK_DEPTH, BookSnapshot, Cloid, CoinId, CoinRegistry, ConnId,
    Level, MarketUpdate, Side, Stamp, Trade, VenueOrderStatus,
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

/// Decode one account-channel text frame into [`AccountUpdate`]s.
///
/// Yields zero or more updates: `orderUpdates` and `userEvents.fills` are
/// batches, while a `userFills` snapshot produces one update per fill. Channels
/// the engine does not model here (non-user cancels, liquidations) are counted
/// but otherwise skipped; E-8 decides how to route them.
pub fn decode_account(text: &str, coins: &IngestCoins, stamp: Stamp) -> Result<Vec<AccountUpdate>> {
    let tag: ChannelTag<'_> =
        serde_json::from_str(text).map_err(|e| Error::Decode(e.to_string()))?;
    match tag.channel {
        "orderUpdates" => {
            let frame: OrderUpdatesFrame<'_> =
                serde_json::from_str(text).map_err(|e| Error::Decode(e.to_string()))?;
            Ok(frame
                .data
                .iter()
                .filter_map(|order| order_update(order, coins, stamp))
                .collect())
        }
        "userFills" => {
            let frame: UserFillsFrame =
                serde_json::from_str(text).map_err(|e| Error::Decode(e.to_string()))?;
            Ok(frame
                .data
                .fills
                .iter()
                .filter_map(|fill| fill_update(fill, coins, stamp))
                .collect())
        }
        "user" => {
            let frame: UserEventFrame =
                serde_json::from_str(text).map_err(|e| Error::Decode(e.to_string()))?;
            let mut out = Vec::new();
            if let Some(fills) = &frame.data.fills {
                out.extend(
                    fills
                        .iter()
                        .filter_map(|fill| fill_update(fill, coins, stamp)),
                );
            }
            if let Some(funding) = &frame.data.funding
                && let Some(coin) = coins.id(&funding.coin)
            {
                out.push(AccountUpdate::Funding {
                    stamp,
                    coin,
                    usdc: funding.usdc,
                });
            }
            Ok(out)
        }
        _ => Ok(Vec::new()),
    }
}

#[derive(serde::Deserialize)]
struct OrderUpdatesFrame<'a> {
    #[serde(borrow)]
    data: Vec<WireWsOrder<'a>>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireWsOrder<'a> {
    #[serde(borrow)]
    order: WireBasicOrder<'a>,
    #[serde(borrow)]
    status: &'a str,
    #[serde(default)]
    status_timestamp: u64,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireBasicOrder<'a> {
    #[serde(borrow)]
    coin: &'a str,
    /// Side (`B`/`A`); parsed by E-5's order manager, ignored here.
    #[serde(borrow)]
    _side: &'a str,
    limit_px: rust_decimal::Decimal,
    sz: rust_decimal::Decimal,
    oid: u64,
    orig_sz: rust_decimal::Decimal,
    #[serde(default)]
    cloid: Option<&'a str>,
}

fn order_update(
    order: &WireWsOrder<'_>,
    coins: &IngestCoins,
    stamp: Stamp,
) -> Option<AccountUpdate> {
    let coin = coins.id(order.order.coin)?;
    let _ = coin; // CoinId is not carried on OrderUpdate; kept for validation.
    let mut stamp = stamp;
    stamp.ts_exch_ms = order.status_timestamp;
    Some(AccountUpdate::OrderUpdate {
        stamp,
        cloid: order
            .order
            .cloid
            .and_then(Cloid::from_hex)
            .unwrap_or(Cloid([0; 16])),
        oid: order.order.oid,
        status: venue_status(order.status),
        filled_sz: order.order.orig_sz - order.order.sz,
        avg_px: order.order.limit_px,
    })
}

#[derive(serde::Deserialize)]
struct UserFillsFrame {
    data: WireUserFills,
}

#[derive(serde::Deserialize)]
struct WireUserFills {
    #[serde(default)]
    fills: Vec<WireFill>,
}

#[derive(serde::Deserialize)]
struct UserEventFrame {
    data: WireUserEvent,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireUserEvent {
    #[serde(default)]
    fills: Option<Vec<WireFill>>,
    #[serde(default)]
    funding: Option<WireFunding>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireFunding {
    coin: String,
    usdc: rust_decimal::Decimal,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireFill {
    coin: String,
    side: String,
    px: rust_decimal::Decimal,
    sz: rust_decimal::Decimal,
    time: u64,
    #[serde(default)]
    oid: u64,
    #[serde(default)]
    fee: rust_decimal::Decimal,
    #[serde(default)]
    liquidation: Option<serde_json::Value>,
}

fn fill_update(fill: &WireFill, coins: &IngestCoins, stamp: Stamp) -> Option<AccountUpdate> {
    let coin = coins.id(&fill.coin)?;
    let mut stamp = stamp;
    stamp.ts_exch_ms = fill.time;
    Some(AccountUpdate::Fill {
        stamp,
        cloid: None,
        oid: fill.oid,
        coin,
        side: parse_side(&fill.side),
        px: fill.px,
        sz: fill.sz,
        fee: fill.fee,
        liquidation: fill.liquidation.is_some(),
    })
}

/// Map a venue order status string to a typed [`VenueOrderStatus`].
pub fn venue_status(status: &str) -> VenueOrderStatus {
    match status {
        "open" | "resting" => VenueOrderStatus::Resting,
        "filled" => VenueOrderStatus::Filled,
        "partiallyFilled" => VenueOrderStatus::PartiallyFilled,
        "canceled" | "scheduledCancel" => VenueOrderStatus::Cancelled,
        "rejected" => VenueOrderStatus::Rejected,
        _ if status.ends_with("Canceled") => VenueOrderStatus::Cancelled,
        _ if status.ends_with("Rejected") => VenueOrderStatus::Rejected,
        _ => VenueOrderStatus::Other,
    }
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

    /// Decode a market text frame at `stamp`.
    pub fn decode(&self, text: &str, stamp: Stamp) -> Result<Option<MarketUpdate>> {
        decode_market(text, &self.coins, self.conn, stamp)
    }

    /// Decode an account text frame into zero or more [`AccountUpdate`]s.
    pub fn decode_account(&self, text: &str, stamp: Stamp) -> Result<Vec<AccountUpdate>> {
        decode_account(text, &self.coins, stamp)
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
    fn decodes_order_updates_into_account_events() {
        let frame = r#"{"channel":"orderUpdates","data":[
            {"order":{"coin":"BTC","side":"B","limitPx":"60000","sz":"0.5","oid":42,
              "timestamp":1,"origSz":"1.0","cloid":"0x00000000deadbeef0000000000000001"},
             "status":"open","statusTimestamp":99}]}"#;
        let updates = ingester().decode_account(frame, Stamp::default()).unwrap();
        assert_eq!(updates.len(), 1);
        match &updates[0] {
            AccountUpdate::OrderUpdate {
                cloid,
                oid,
                status,
                filled_sz,
                ..
            } => {
                assert_eq!(oid, &42);
                assert_eq!(status, &VenueOrderStatus::Resting);
                assert_eq!(filled_sz, &ds("0.5")); // orig 1.0 - remaining 0.5
                assert_eq!(cloid.to_hex(), "0x00000000deadbeef0000000000000001");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn decodes_user_fills_batch() {
        let frame = r#"{"channel":"userFills","data":{"isSnapshot":false,"user":"0xabc","fills":[
            {"coin":"BTC","px":"60000","sz":"0.01","side":"B","time":7,
             "closedPnl":"0","oid":42,"crossed":true,"fee":"0.27","tid":7,"dir":"Open Long"}]}}"#;
        let updates = ingester().decode_account(frame, Stamp::default()).unwrap();
        assert_eq!(updates.len(), 1);
        match &updates[0] {
            AccountUpdate::Fill {
                coin,
                side,
                px,
                sz,
                fee,
                liquidation,
                ..
            } => {
                assert_eq!(coin, &CoinId(0));
                assert_eq!(side, &Side::Buy);
                assert_eq!(px, &ds("60000"));
                assert_eq!(sz, &ds("0.01"));
                assert_eq!(fee, &ds("0.27"));
                assert!(!liquidation);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn decodes_user_events_funding() {
        let frame = r#"{"channel":"user","data":{"funding":{
            "time":5,"coin":"ETH","usdc":"-0.5","szi":"2","fundingRate":"0.00001"}}}"#;
        let updates = ingester().decode_account(frame, Stamp::default()).unwrap();
        assert_eq!(updates.len(), 1);
        match &updates[0] {
            AccountUpdate::Funding { coin, usdc, .. } => {
                assert_eq!(coin, &CoinId(1)); // ETH
                assert_eq!(usdc, &ds("-0.5"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn unknown_coins_in_account_events_are_skipped() {
        let frame = r#"{"channel":"userFills","data":{"fills":[
            {"coin":"DOGE","px":"1","sz":"1","side":"B","time":1,"fee":"0","oid":1}]}}"#;
        assert!(
            ingester()
                .decode_account(frame, Stamp::default())
                .unwrap()
                .is_empty()
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
