//! Core engine types (SPEC-0010 §6–§7, task E-1).
//!
//! These are the interning and event types the event-driven engine is built
//! on: coins are [`CoinId`]s resolved at startup, events carry a [`Stamp`], and
//! order ids are fixed-size [`Cloid`]s. The module deliberately lives inside
//! `mev-bot`'s engine module rather than a separate crate: only the engine
//! consumes these types today, and splitting a crate would force
//! `mev-strategy`/`mev-risk` to re-depend on it for no benefit (SPEC-0010 §23
//! Q1; revisit in E-7 if replay benches need the types without the binary).
//!
//! E-1 lands the types before their consumers (E-2/E-3/E-5…), so unused items
//! are expected until those tasks wire them in.
#![allow(dead_code)]

use std::collections::BTreeMap;

use mev_hl_client::{AssetMap, MarketKind, OrderStatusResponse};
use rust_decimal::Decimal;
use smallvec::SmallVec;

/// A price. `Decimal` in v1; E-11 may switch to fixed-point `i64` (SPEC-0010 §6).
pub type Px = Decimal;
/// A size. `Decimal` in v1; E-11 may switch to fixed-point `i64` (SPEC-0010 §6).
pub type Sz = Decimal;

/// A connection id, used to attribute market-feed gaps (SPEC-0010 §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConnId(pub u16);

/// An interned coin index. Indexes `Vec`s of per-coin state on the hot path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CoinId(pub u16);

impl CoinId {
    /// The index as a `usize`, for `Vec` indexing.
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

/// A bidirectional `CoinId` ↔ canonical coin map, built once at startup.
#[derive(Debug, Clone, Default)]
pub struct CoinRegistry {
    coins: Vec<String>,
    ids: BTreeMap<String, CoinId>,
}

impl CoinRegistry {
    /// Build a registry from the engine's configured universe.
    ///
    /// `coins` are canonical market symbols; entries are de-duplicated and
    /// assigned dense `CoinId`s in the given order (so ids are stable for a
    /// stable input).
    pub fn from_coins(coins: &[String]) -> Self {
        let mut registry = Self::default();
        for coin in coins {
            registry.insert(coin.clone());
        }
        registry
    }

    /// Build a registry from every market in an [`AssetMap`], in canonical
    /// order. Prefer [`Self::from_coins`] for the engine's actual universe.
    pub fn from_asset_map(map: &AssetMap) -> Self {
        let coins: Vec<String> = map.iter().map(|market| market.coin.clone()).collect();
        Self::from_coins(&coins)
    }

    /// The id for a canonical coin, if it is in the universe.
    pub fn id(&self, coin: &str) -> Option<CoinId> {
        self.ids.get(coin).copied()
    }

    /// The canonical coin for an id.
    pub fn coin(&self, id: CoinId) -> Option<&str> {
        self.coins.get(id.index()).map(String::as_str)
    }

    /// Number of interned coins.
    pub fn len(&self) -> usize {
        self.coins.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.coins.is_empty()
    }

    /// Iterate `(CoinId, coin)` pairs in id order.
    pub fn iter(&self) -> impl Iterator<Item = (CoinId, &str)> {
        self.coins
            .iter()
            .enumerate()
            .map(|(index, coin)| (CoinId(index as u16), coin.as_str()))
    }

    fn insert(&mut self, coin: String) {
        if self.ids.contains_key(&coin) {
            return;
        }
        let id = CoinId(self.coins.len() as u16);
        self.ids.insert(coin.clone(), id);
        self.coins.push(coin);
    }
}

/// A 16-byte client order id, as used on the wire (`0x` + 32 hex chars).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Cloid(pub [u8; 16]);

impl Cloid {
    /// Parse a `0x`-prefixed 32-hex-char string.
    pub fn from_hex(value: &str) -> Option<Self> {
        let hex = value.strip_prefix("0x").unwrap_or(value);
        if hex.len() != 32 {
            return None;
        }
        let mut bytes = [0u8; 16];
        for (i, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
        }
        Some(Cloid(bytes))
    }

    /// The `0x`-prefixed 32-hex-char string.
    pub fn to_hex(self) -> String {
        let mut out = String::with_capacity(34);
        out.push_str("0x");
        for byte in self.0 {
            out.push_str(&format!("{byte:02x}"));
        }
        out
    }
}

impl std::fmt::Display for Cloid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// A price/size level in a book snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Level {
    /// Price.
    pub px: Px,
    /// Total size.
    pub sz: Sz,
    /// Number of orders at this level.
    pub n: u32,
}

/// Depth kept per side in a fixed-size book (SPEC-0008 V-1).
pub const BOOK_DEPTH: usize = 20;

/// A fixed-size book snapshot (no allocation; SPEC-0010 §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BookSnapshot {
    /// Bid levels, best first.
    pub bids: [Level; BOOK_DEPTH],
    /// Ask levels, best first.
    pub asks: [Level; BOOK_DEPTH],
    /// Number of populated bid levels.
    pub n_bids: u8,
    /// Number of populated ask levels.
    pub n_asks: u8,
    /// Venue timestamp of the snapshot in milliseconds.
    pub time_ms: u64,
}

impl Default for BookSnapshot {
    fn default() -> Self {
        Self {
            bids: [Level::default(); BOOK_DEPTH],
            asks: [Level::default(); BOOK_DEPTH],
            n_bids: 0,
            n_asks: 0,
            time_ms: 0,
        }
    }
}

impl BookSnapshot {
    /// Best bid, if any.
    pub fn best_bid(&self) -> Option<Level> {
        (self.n_bids > 0).then(|| self.bids[0])
    }

    /// Best ask, if any.
    pub fn best_ask(&self) -> Option<Level> {
        (self.n_asks > 0).then(|| self.asks[0])
    }

    /// Whether the snapshot has no levels on either side.
    pub fn is_empty(&self) -> bool {
        self.n_bids == 0 && self.n_asks == 0
    }
}

/// Receive-time stamps carried with every event (SPEC-0010 §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stamp {
    /// Wall clock at the socket read, or the recorded `t_ns` in replay.
    pub t_recv_ns: i64,
    /// Monotonic clock at the socket read.
    pub mono_ns: u64,
    /// Venue timestamp if the payload carried one, else 0.
    pub ts_exch_ms: u64,
}

/// A lightweight per-coin context for the hot path (funding, mark, oracle, OI).
///
/// Distinct from the wire [`mev_hl_client::AssetCtx`], which carries extra
/// serde fields the engine never needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AssetCtxLite {
    /// Current funding rate.
    pub funding: Decimal,
    /// Mark price.
    pub mark_px: Decimal,
    /// Oracle price.
    pub oracle_px: Decimal,
    /// Open interest.
    pub open_interest: Decimal,
}

/// Per-asset metadata precomputed for the order builder (SPEC-0010 §7).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AssetMetaLite {
    /// Wire asset id for orders.
    pub asset_id: u32,
    /// Size decimals for rounding.
    pub sz_decimals: u32,
    /// Whether the market is spot.
    pub is_spot: bool,
}

impl AssetMetaLite {
    /// Precompute from a resolved market.
    pub fn from_market(market: &mev_hl_client::Market) -> Self {
        Self {
            asset_id: market.asset_id(),
            sz_decimals: market.sz_decimals,
            is_spot: market.kind == MarketKind::Spot,
        }
    }
}

/// Order side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Side {
    /// Buy / bid.
    #[default]
    Buy,
    /// Sell / ask.
    Sell,
}

/// A venue order status string, kept typed but lossless (SPEC-0010 §10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VenueOrderStatus {
    /// Working on the book.
    Resting,
    /// Fully filled.
    Filled,
    /// Partially filled.
    PartiallyFilled,
    /// Cancelled.
    Cancelled,
    /// Rejected.
    Rejected,
    /// Any other venue status string.
    Other,
}

/// One per-order outcome in a post ack, with the venue oid when the reply
/// carried one (`resting`/`filled`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrderAck {
    /// The venue status.
    pub status: VenueOrderStatus,
    /// The venue order id, if the reply carried one.
    pub oid: Option<u64>,
}

/// Outcome of a posted action, as routed back from the exec backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostResult {
    /// Per-order statuses, in request order.
    Statuses(SmallVec<[OrderAck; 8]>),
    /// The venue definitively refused the post, or it was never sent. The
    /// orders are terminal `Rejected` (SPEC-0002 H-2). The text is the reason.
    Rejected(String),
    /// The post was sent but no definitive reply arrived; the orders are
    /// `Unknown` and are reconciled by `cloid` (SPEC-0002 H-1/H-2).
    Error(String),
}

/// A control command from the kill switch, CLI, or a strategy pause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Control {
    /// Stop all new risk; cancel working orders.
    KillSwitch,
    /// Clear the halt.
    Resume,
    /// Stop dispatching to a strategy (by its stable id string).
    Pause {
        /// The strategy id.
        strategy: String,
    },
    /// Reload risk limits.
    ReloadLimits,
}

/// A reconciliation snapshot from the REST reconciler (SPEC-0010 §15).
///
/// Minimal placeholder for E-1; E-8 fills in positions, margin, and open
/// orders once the account stream lands.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AccountSnapshot {
    /// Account value.
    pub account_value: Decimal,
    /// Total margin used.
    pub margin_used: Decimal,
}

/// A market-data update produced by the ingest tasks (SPEC-0010 §6).
///
/// `Book` carries a fixed-size snapshot inline rather than a `Box`: the spec
/// forbids per-event allocation on the hot path (SPEC-0010 §4), and the enum is
/// moved, not stack-copied, after construction.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum MarketUpdate {
    /// Best bid/offer for a coin.
    Bbo {
        /// Coin.
        coin: CoinId,
        /// Receive stamp.
        stamp: Stamp,
        /// Best bid.
        bid: Level,
        /// Best ask.
        ask: Level,
    },
    /// Fixed-size book snapshot.
    Book {
        /// Coin.
        coin: CoinId,
        /// Receive stamp.
        stamp: Stamp,
        /// The snapshot.
        book: BookSnapshot,
    },
    /// A batch of trades.
    Trades {
        /// Coin.
        coin: CoinId,
        /// Receive stamp.
        stamp: Stamp,
        /// The trades.
        trades: SmallVec<[Trade; 8]>,
    },
    /// Asset context (funding, mark, oracle, OI).
    Ctx {
        /// Coin.
        coin: CoinId,
        /// Receive stamp.
        stamp: Stamp,
        /// The context.
        ctx: AssetCtxLite,
    },
    /// A feed gap opened or closed on a connection.
    Gap {
        /// Connection.
        conn: ConnId,
        /// Receive stamp.
        stamp: Stamp,
        /// Whether the gap is opening (`true`) or closing (`false`).
        open: bool,
    },
}

/// A lossless account update (SPEC-0010 §6).
#[derive(Debug, Clone)]
pub enum AccountUpdate {
    /// An order state change.
    OrderUpdate {
        /// Receive stamp.
        stamp: Stamp,
        /// Client order id.
        cloid: Cloid,
        /// Venue order id.
        oid: u64,
        /// New status.
        status: VenueOrderStatus,
        /// Filled size so far.
        filled_sz: Sz,
        /// Average fill price.
        avg_px: Px,
    },
    /// A live (non-snapshot) fill from the `userFills` channel.
    Fill {
        /// Receive stamp.
        stamp: Stamp,
        /// Client order id, if the wire carried one.
        cloid: Option<Cloid>,
        /// Venue order id.
        oid: u64,
        /// Venue trade id, used to de-duplicate fill delivery.
        tid: u64,
        /// Coin.
        coin: CoinId,
        /// Side.
        side: Side,
        /// Fill price.
        px: Px,
        /// Fill size.
        sz: Sz,
        /// Fee paid.
        fee: Px,
        /// Whether this was a liquidation.
        liquidation: bool,
    },
    /// A `userFills` snapshot, handled as one unit so the first-connect
    /// snapshot can be recorded without being applied (SPEC-0002 H-3,
    /// SPEC-0010 E-8).
    ///
    /// The fills are a `Vec` (one allocation per snapshot): a snapshot may hold
    /// up to 2000 fills and is rare, while `Fill` stays small on the account
    /// channel.
    Fills {
        /// Receive stamp.
        stamp: Stamp,
        /// The snapshot's fills.
        fills: Vec<FillData>,
    },
    /// An `Unknown` order resolved by an `orderStatus` query (SPEC-0002 H-2).
    ///
    /// Applied only while the order is still `Unknown`, so a stale answer that
    /// arrives after a newer stream update cannot move the order backwards.
    ResolveUnknown {
        /// Receive stamp.
        stamp: Stamp,
        /// Client order id.
        cloid: Cloid,
        /// The venue's `orderStatus` answer.
        status: OrderStatusResponse,
    },
    /// An `orderStatus` retry bound expired with the order still `Unknown`:
    /// resolve it as not placed (`Rejected`) and let the breaker clear.
    UnknownExpired {
        /// Receive stamp.
        stamp: Stamp,
        /// Client order id.
        cloid: Cloid,
    },
    /// A posted action's result.
    PostAck {
        /// Receive stamp.
        stamp: Stamp,
        /// Request id.
        req_id: u64,
        /// Result.
        result: PostResult,
    },
    /// A reconciliation snapshot.
    Reconcile {
        /// Receive stamp.
        stamp: Stamp,
        /// The snapshot.
        snapshot: AccountSnapshot,
    },
    /// A funding payment.
    Funding {
        /// Receive stamp.
        stamp: Stamp,
        /// Coin.
        coin: CoinId,
        /// Signed USDC amount.
        usdc: Px,
    },
    /// A control command.
    Control(Control),
}

/// One fill inside a `userFills` snapshot (`AccountUpdate::Fills`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FillData {
    /// Client order id, if the wire carried one.
    pub cloid: Option<Cloid>,
    /// Venue order id.
    pub oid: u64,
    /// Venue trade id.
    pub tid: u64,
    /// Coin.
    pub coin: CoinId,
    /// Side.
    pub side: Side,
    /// Fill price.
    pub px: Px,
    /// Fill size.
    pub sz: Sz,
    /// Fee paid.
    pub fee: Px,
    /// Whether this was a liquidation.
    pub liquidation: bool,
}

/// A compact trade, borrowing the wire shape but with fixed size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Trade {
    /// Aggressor side.
    pub side: Side,
    /// Price.
    pub px: Px,
    /// Size.
    pub sz: Sz,
    /// Venue timestamp in milliseconds.
    pub time_ms: u64,
    /// Trade id, if present.
    pub tid: Option<u64>,
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use mev_hl_client::types::{AssetMeta, Meta, SpotMeta, SpotPair, SpotToken};

    use super::*;

    fn ds(value: &str) -> Decimal {
        Decimal::from_str(value).unwrap()
    }

    fn asset_map() -> AssetMap {
        let mut map = AssetMap::new();
        map.insert_perp_dex(
            None,
            None,
            &Meta {
                universe: vec![AssetMeta {
                    name: "BTC".into(),
                    sz_decimals: 5,
                    max_leverage: 40,
                    is_delisted: false,
                    only_isolated: false,
                }],
            },
        );
        map.insert_perp_dex(
            Some("xyz"),
            Some(1),
            &Meta {
                universe: vec![AssetMeta {
                    name: "xyz:TSLA".into(),
                    sz_decimals: 3,
                    max_leverage: 20,
                    is_delisted: false,
                    only_isolated: false,
                }],
            },
        );
        map.insert_spot(&SpotMeta {
            universe: vec![SpotPair {
                name: "@101".into(),
                index: 101,
                tokens: [2, 0],
            }],
            tokens: vec![
                SpotToken {
                    name: "USDC".into(),
                    index: 0,
                    sz_decimals: 8,
                },
                SpotToken {
                    name: "HYPE".into(),
                    index: 1,
                    sz_decimals: 2,
                },
                SpotToken {
                    name: "UBTC".into(),
                    index: 2,
                    sz_decimals: 6,
                },
            ],
        });
        map
    }

    #[test]
    fn coin_id_roundtrips_perp_spot_hip3() {
        let map = asset_map();
        let registry = CoinRegistry::from_asset_map(&map);
        for coin in ["BTC", "xyz:TSLA", "@101"] {
            let id = registry
                .id(coin)
                .unwrap_or_else(|| panic!("missing {coin}"));
            assert_eq!(registry.coin(id), Some(coin));
        }
        assert!(registry.id("NOPE").is_none());
        assert!(registry.coin(CoinId(999)).is_none());
    }

    #[test]
    fn coin_ids_are_stable_and_dense() {
        let registry = CoinRegistry::from_coins(&[
            "BTC".into(),
            "ETH".into(),
            "BTC".into(), // duplicate ignored
        ]);
        assert_eq!(registry.len(), 2);
        assert_eq!(registry.id("BTC"), Some(CoinId(0)));
        assert_eq!(registry.id("ETH"), Some(CoinId(1)));
        let ids: Vec<u16> = registry.iter().map(|(id, _)| id.0).collect();
        assert_eq!(ids, vec![0, 1]);
    }

    #[test]
    fn empty_book_snapshot_has_no_levels() {
        let book = BookSnapshot::default();
        assert!(book.is_empty());
        assert_eq!(book.best_bid(), None);
        assert_eq!(book.best_ask(), None);
        assert_eq!(book.n_bids, 0);
        assert_eq!(book.n_asks, 0);
    }

    #[test]
    fn book_snapshot_reports_best_levels() {
        let mut book = BookSnapshot {
            n_bids: 1,
            n_asks: 1,
            time_ms: 42,
            ..Default::default()
        };
        book.bids[0] = Level {
            px: ds("100"),
            sz: ds("2"),
            n: 3,
        };
        book.asks[0] = Level {
            px: ds("101"),
            sz: ds("1"),
            n: 1,
        };
        assert_eq!(book.best_bid().unwrap().px, ds("100"));
        assert_eq!(book.best_ask().unwrap().px, ds("101"));
        assert_eq!(book.time_ms, 42);
    }

    #[test]
    fn cloid_hex_roundtrips() {
        let cloid = Cloid::from_hex("0x00000000deadbeef0000000000000001").unwrap();
        assert_eq!(cloid.to_hex(), "0x00000000deadbeef0000000000000001");
        assert_eq!(cloid.to_string(), cloid.to_hex());
        // Fixed-size access for the wire.
        assert_eq!(cloid.0.len(), 16);
        assert!(Cloid::from_hex("0x1234").is_none());
        assert!(Cloid::from_hex("0xzz000000000000000000000000000000").is_none());
    }

    #[test]
    fn asset_meta_precomputes_wire_ids() {
        let map = asset_map();
        let btc = AssetMetaLite::from_market(map.get("BTC").unwrap());
        assert_eq!(btc.asset_id, 0);
        assert!(!btc.is_spot);
        let tsla = AssetMetaLite::from_market(map.get("xyz:TSLA").unwrap());
        assert_eq!(tsla.asset_id, 110_000);
        let spot = AssetMetaLite::from_market(map.get("@101").unwrap());
        assert_eq!(spot.asset_id, 10_101);
        assert!(spot.is_spot);
    }

    #[test]
    fn updates_construct_with_expected_shapes() {
        let stamp = Stamp {
            t_recv_ns: 1,
            mono_ns: 2,
            ts_exch_ms: 3,
        };
        let mut trades: SmallVec<[Trade; 8]> = SmallVec::new();
        trades.push(Trade {
            side: Side::Buy,
            px: ds("100"),
            sz: ds("1"),
            time_ms: 3,
            tid: Some(7),
        });
        let market = [
            MarketUpdate::Bbo {
                coin: CoinId(0),
                stamp,
                bid: Level::default(),
                ask: Level::default(),
            },
            MarketUpdate::Book {
                coin: CoinId(0),
                stamp,
                book: BookSnapshot::default(),
            },
            MarketUpdate::Trades {
                coin: CoinId(0),
                stamp,
                trades,
            },
            MarketUpdate::Ctx {
                coin: CoinId(0),
                stamp,
                ctx: AssetCtxLite::default(),
            },
            MarketUpdate::Gap {
                conn: ConnId(1),
                stamp,
                open: true,
            },
        ];
        assert_eq!(market.len(), 5);

        let account = [
            AccountUpdate::OrderUpdate {
                stamp,
                cloid: Cloid([0; 16]),
                oid: 1,
                status: VenueOrderStatus::Resting,
                filled_sz: ds("0"),
                avg_px: ds("0"),
            },
            AccountUpdate::Fill {
                stamp,
                cloid: None,
                oid: 1,
                tid: 42,
                coin: CoinId(0),
                side: Side::Sell,
                px: ds("100"),
                sz: ds("1"),
                fee: ds("0.05"),
                liquidation: false,
            },
            AccountUpdate::Fills {
                stamp,
                fills: Vec::new(),
            },
            AccountUpdate::PostAck {
                stamp,
                req_id: 9,
                result: PostResult::Statuses(SmallVec::new()),
            },
            AccountUpdate::Reconcile {
                stamp,
                snapshot: AccountSnapshot {
                    account_value: ds("1000"),
                    margin_used: ds("100"),
                },
            },
            AccountUpdate::Funding {
                stamp,
                coin: CoinId(0),
                usdc: ds("-0.5"),
            },
            AccountUpdate::Control(Control::KillSwitch),
        ];
        assert_eq!(account.len(), 7);
    }
}
