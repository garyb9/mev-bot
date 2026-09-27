//! Paper execution backend with a latency model (SPEC-0010 §14, task E-7).
//!
//! `PaperExec` is the `simulate`/`replay` exec backend: it accepts the engine's
//! built orders ([`PaperOrder`]s, produced from an [`UnsignedPost`]), fills them
//! against the engine's [`MarketSlot`] book state after a configurable latency,
//! and emits the same `AccountUpdate` stream a live venue would — fills plus
//! order-status changes.
//!
//! It reads no clock and uses no randomness: every timestamp comes from the
//! caller-supplied `now_ms`, so the same input sequence produces byte-identical
//! output. That is what makes replay determinism (SPEC-0010 G-6) testable here.
//!
//! ## Latency model
//!
//! Every accepted order gets `eligible_at_ms = now_ms + latency_ms`. A
//! *marketable* (taker) order then fills at its deadline against the book that
//! is current at that deadline. A resting `Alo`/`Gtc` order rests once it is
//! eligible and fills later when a subsequent book update trades through its
//! limit, again `latency_ms` after that update is observed. Maker fills are
//! controlled by [`PaperConfig::maker_fills`].
//!
//! ## Why not reuse `mev_strategy::PaperExecutor`
//!
//! [`mev_strategy::PaperExecutor`] matches against a `MarketView` and returns
//! only fills; it cannot express per-order latency or emit `OrderUpdate`s. This
//! module mirrors its account/fee semantics (positions, spot balances,
//! maker/taker fees) but matches directly against the fixed-size book via
//! [`mev_strategy::BookView::walk_bounded`].
//!
//! ## Recorder-segment replay
//!
//! Feeding `PaperExec` from SPEC-0008 recorder segments (R-7) is out of scope
//! for this part; the latency model and event emission here are what replay
//! will drive.

use std::collections::BTreeMap;
use std::str::FromStr;

use mev_hl_client::{Action, OrderType, Tif};
use mev_strategy::{AccountView, BookView, FeeRates, Instrument, PositionView, TimeInForce};
use rust_decimal::Decimal;

use crate::builder::AssetTable;
use crate::exec::UnsignedPost;
use crate::state::MarketSlot;
use crate::types::{
    AccountUpdate, Cloid, CoinId, CoinRegistry, Px, Side, Stamp, Sz, VenueOrderStatus,
};

/// A typed order handed to [`PaperExec`] (SPEC-0010 §14).
///
/// This is the paper backend's input: already-priced, already-cloided, and
/// independent of venue wire encoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaperOrder {
    /// Client order id assigned by the engine.
    pub cloid: Cloid,
    /// Interned coin.
    pub coin: CoinId,
    /// Side.
    pub side: Side,
    /// Limit price. Aggressive orders are priced by the builder before they
    /// reach an exec backend, so this is always concrete.
    pub limit_px: Px,
    /// Order size.
    pub size: Sz,
    /// Time in force.
    pub tif: TimeInForce,
    /// Whether the order may only reduce an existing position.
    pub reduce_only: bool,
}

/// Paper backend configuration (SPEC-0010 §19).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaperConfig {
    /// Simulated one-way latency, in milliseconds.
    pub latency_ms: u64,
    /// Whether resting orders may fill as makers when the book trades through.
    pub maker_fills: bool,
}

impl Default for PaperConfig {
    fn default() -> Self {
        Self {
            latency_ms: 20,
            maker_fills: true,
        }
    }
}

/// A maker fill that has been observed but is deferred until its latency
/// deadline (`limit_px` is the fill price, so only the size is carried).
#[derive(Debug, Clone, Copy)]
struct PendingFill {
    at_ms: u64,
    sz: Sz,
}

/// An order tracked by the paper backend.
#[derive(Debug, Clone)]
struct TrackedOrder {
    oid: u64,
    cloid: Cloid,
    coin: CoinId,
    coin_name: String,
    side: Side,
    limit_px: Px,
    size: Sz,
    filled_sz: Sz,
    filled_notional: Px,
    reduce_only: bool,
    tif: TimeInForce,
    eligible_at_ms: u64,
    pending_fill: Option<PendingFill>,
}

impl TrackedOrder {
    /// Remaining size to fill, floored at zero.
    fn remaining(&self) -> Sz {
        (self.size - self.filled_sz).max(Decimal::ZERO)
    }

    /// Whether this is a buy.
    fn is_buy(&self) -> bool {
        matches!(self.side, Side::Buy)
    }
}

/// The paper exec backend (SPEC-0010 §14).
///
/// Fills are emitted as [`AccountUpdate::Fill`] and state changes as
/// [`AccountUpdate::OrderUpdate`], exactly as the live account stream would
/// report them.
#[derive(Debug)]
pub struct PaperExec {
    config: PaperConfig,
    account: AccountView,
    instruments: BTreeMap<String, Instrument>,
    perp_fees: FeeRates,
    spot_fees: FeeRates,
    /// Accepted but not yet eligible (waiting out `latency_ms`).
    pending: Vec<TrackedOrder>,
    /// Eligible and on the book, waiting for a cross.
    resting: Vec<TrackedOrder>,
    next_oid: u64,
    next_tid: u64,
    fees_paid: Decimal,
}

impl PaperExec {
    /// Build a backend over the given instruments and starting account.
    pub fn new(
        config: PaperConfig,
        account: AccountView,
        instruments: BTreeMap<String, Instrument>,
        perp_fees: FeeRates,
        spot_fees: FeeRates,
    ) -> Self {
        Self {
            config,
            account,
            instruments,
            perp_fees,
            spot_fees,
            pending: Vec::new(),
            resting: Vec::new(),
            next_oid: 1,
            next_tid: 1,
            fees_paid: Decimal::ZERO,
        }
    }

    /// The simulated account snapshot.
    pub fn account(&self) -> &AccountView {
        &self.account
    }

    /// Total fees paid by the simulated account.
    pub fn fees_paid(&self) -> Decimal {
        self.fees_paid
    }

    /// Number of orders accepted but not yet eligible.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Number of eligible orders currently resting on the book.
    pub fn resting_len(&self) -> usize {
        self.resting.len()
    }

    /// The current config.
    pub fn config(&self) -> PaperConfig {
        self.config
    }

    /// Queue orders from one iteration.
    ///
    /// Each order is assigned a venue `oid` and becomes eligible `latency_ms`
    /// after `now_ms`. No account event is emitted yet; the order is "in
    /// flight" until a later [`Self::on_market`] reaches its deadline. `markets`
    /// is accepted for API parity with the live submit path and is not needed to
    /// queue.
    pub fn submit(
        &mut self,
        orders: &[PaperOrder],
        registry: &CoinRegistry,
        _markets: &[MarketSlot],
        now_ms: u64,
    ) -> Vec<AccountUpdate> {
        for order in orders {
            let oid = self.next_oid;
            self.next_oid = self.next_oid.wrapping_add(1);
            self.pending.push(TrackedOrder {
                oid,
                cloid: order.cloid,
                coin: order.coin,
                coin_name: registry.coin(order.coin).unwrap_or("").to_string(),
                side: order.side,
                limit_px: order.limit_px,
                size: order.size,
                filled_sz: Decimal::ZERO,
                filled_notional: Decimal::ZERO,
                reduce_only: order.reduce_only,
                tif: order.tif,
                eligible_at_ms: now_ms.saturating_add(self.config.latency_ms),
                pending_fill: None,
            });
        }
        Vec::new()
    }

    /// Cancel orders by cloid, whether still in flight or already resting.
    ///
    /// Each cancelled order emits a `Cancelled` [`AccountUpdate::OrderUpdate`];
    /// unknown cloids are ignored.
    pub fn cancel(&mut self, cloids: &[Cloid], now_ms: u64) -> Vec<AccountUpdate> {
        let stamp = stamp(now_ms);
        let mut out = Vec::new();
        Self::retain_cancels(&mut self.pending, cloids, stamp, &mut out);
        Self::retain_cancels(&mut self.resting, cloids, stamp, &mut out);
        out
    }

    /// Advance matching against the current book.
    ///
    /// Activates in-flight orders whose deadline has passed, then resolves
    /// deferred maker fills and detects new crosses. Emits fill/status
    /// [`AccountUpdate`]s in a deterministic order.
    pub fn on_market(
        &mut self,
        _registry: &CoinRegistry,
        markets: &[MarketSlot],
        now_ms: u64,
    ) -> Vec<AccountUpdate> {
        let stamp = stamp(now_ms);
        let mut out = Vec::new();

        let mut still_pending = Vec::with_capacity(self.pending.len());
        for mut order in std::mem::take(&mut self.pending) {
            if order.eligible_at_ms > now_ms {
                still_pending.push(order);
                continue;
            }
            if self.activate(&mut order, markets, stamp, &mut out) {
                self.resting.push(order);
            }
        }
        self.pending = still_pending;

        let mut still_resting = Vec::with_capacity(self.resting.len());
        for mut order in std::mem::take(&mut self.resting) {
            if let Some(pending) = order.pending_fill {
                if pending.at_ms > now_ms {
                    still_resting.push(order);
                    continue;
                }
                let limit_px = order.limit_px;
                let updates = self.apply_fill(&mut order, pending.sz, limit_px, true, stamp);
                out.extend(updates);
                order.pending_fill = None;
                if order.remaining() <= Decimal::ZERO {
                    continue;
                }
            }
            if self.config.maker_fills
                && let Some(book) = book_for(order.coin, markets)
                && let Some(sz) = maker_depth(&order, &book)
            {
                order.pending_fill = Some(PendingFill {
                    at_ms: now_ms.saturating_add(self.config.latency_ms),
                    sz,
                });
            }
            still_resting.push(order);
        }
        self.resting = still_resting;

        out
    }

    /// Apply a due order to the book, emitting events; returns whether it
    /// should rest afterwards.
    fn activate(
        &mut self,
        order: &mut TrackedOrder,
        markets: &[MarketSlot],
        stamp: Stamp,
        out: &mut Vec<AccountUpdate>,
    ) -> bool {
        let Some(book) = book_for(order.coin, markets) else {
            out.push(order_update(order, VenueOrderStatus::Cancelled, stamp));
            return false;
        };
        let crosses = if order.is_buy() {
            book.best_ask().is_some_and(|(px, _)| px <= order.limit_px)
        } else {
            book.best_bid().is_some_and(|(px, _)| px >= order.limit_px)
        };

        if crosses {
            if order.tif == TimeInForce::Alo {
                // A post-only order that would cross is rejected by the venue.
                out.push(order_update(order, VenueOrderStatus::Cancelled, stamp));
                return false;
            }
            let walk =
                book.walk_bounded(strategy_side(order.side), order.remaining(), order.limit_px);
            if walk.filled > Decimal::ZERO {
                let updates = self.apply_fill(order, walk.filled, walk.avg_px, false, stamp);
                out.extend(updates);
            }
            if order.remaining() > Decimal::ZERO {
                if order.tif == TimeInForce::Ioc {
                    // The unfilled IOC remainder is cancelled.
                    out.push(order_update(order, VenueOrderStatus::Cancelled, stamp));
                    return false;
                }
                // A Gtc remainder rests (already marked `PartiallyFilled`).
                return true;
            }
            return false;
        }

        if order.tif == TimeInForce::Ioc {
            out.push(order_update(order, VenueOrderStatus::Cancelled, stamp));
            return false;
        }
        out.push(order_update(order, VenueOrderStatus::Resting, stamp));
        true
    }

    /// Fill `fill_sz` at `px`, updating the order and the simulated account.
    ///
    /// Emits one `Fill` and one `OrderUpdate` (`Filled` or `PartiallyFilled`).
    fn apply_fill(
        &mut self,
        order: &mut TrackedOrder,
        fill_sz: Sz,
        px: Px,
        maker: bool,
        stamp: Stamp,
    ) -> Vec<AccountUpdate> {
        let fill_sz = fill_sz.min(order.remaining());
        if fill_sz <= Decimal::ZERO {
            return Vec::new();
        }
        let is_spot = self.instrument(&order.coin_name).is_spot;
        let size = if order.reduce_only && !is_spot {
            let current = self.account.position_szi(&order.coin_name);
            let closing = if order.is_buy() {
                (-current).max(Decimal::ZERO)
            } else {
                current.max(Decimal::ZERO)
            };
            closing.min(fill_sz)
        } else {
            fill_sz
        };
        if size <= Decimal::ZERO {
            return Vec::new();
        }
        let signed = if order.is_buy() { size } else { -size };

        if is_spot {
            let instrument = self.instrument(&order.coin_name);
            let balance = self
                .account
                .spot
                .entry(instrument.token)
                .or_insert(Decimal::ZERO);
            *balance = (*balance + signed).max(Decimal::ZERO);
        } else {
            let position = self
                .account
                .positions
                .entry(order.coin_name.clone())
                .or_insert_with(|| PositionView {
                    coin: order.coin_name.clone(),
                    ..Default::default()
                });
            let was_flat = position.szi.is_zero();
            position.szi += signed;
            position.position_value = position.szi.abs() * px;
            if was_flat || position.entry_px.is_none() {
                position.entry_px = Some(px);
            }
        }

        let rates = if is_spot {
            self.spot_fees
        } else {
            self.perp_fees
        };
        let fee = px * size * rates.rate(maker);
        self.account.account_value -= fee;
        self.fees_paid += fee;

        order.filled_sz += size;
        order.filled_notional += px * size;
        let status = if order.remaining() <= Decimal::ZERO {
            VenueOrderStatus::Filled
        } else {
            VenueOrderStatus::PartiallyFilled
        };

        let tid = self.next_tid;
        self.next_tid = self.next_tid.wrapping_add(1);
        vec![
            AccountUpdate::Fill {
                stamp,
                cloid: Some(order.cloid),
                oid: order.oid,
                tid,
                coin: order.coin,
                side: order.side,
                px,
                sz: size,
                fee,
                liquidation: false,
            },
            order_update(order, status, stamp),
        ]
    }

    /// Remove orders whose cloid is in `cloids`, emitting a `Cancelled` update.
    fn retain_cancels(
        orders: &mut Vec<TrackedOrder>,
        cloids: &[Cloid],
        stamp: Stamp,
        out: &mut Vec<AccountUpdate>,
    ) {
        orders.retain(|order| {
            if cloids.contains(&order.cloid) {
                out.push(order_update(order, VenueOrderStatus::Cancelled, stamp));
                false
            } else {
                true
            }
        });
    }

    /// The instrument for a coin, defaulting to a perp.
    fn instrument(&self, coin: &str) -> Instrument {
        self.instruments
            .get(coin)
            .cloned()
            .unwrap_or_else(Instrument::perp)
    }
}

/// Convert a built `order` post into typed [`PaperOrder`]s.
///
/// Only [`Action::Order`] posts are converted; use [`paper_cancels_from_post`]
/// for cancels. The engine's canonical cloids are taken from `post.cloids`
/// (falling back to the wire `c`), and the coin is resolved from the wire asset
/// id via `registry` + `table` — [`CoinRegistry`] alone cannot reverse an asset
/// id, which is why `table` is passed here (documented deviation from the
/// two-argument sketch; `OrderWire` identifies assets by numeric id).
///
/// Trigger orders and orders whose asset is unknown or whose price/size do not
/// parse are skipped.
pub fn paper_orders_from_post(
    post: &UnsignedPost,
    registry: &CoinRegistry,
    table: &AssetTable,
) -> Vec<PaperOrder> {
    let Action::Order { orders, .. } = &post.action else {
        return Vec::new();
    };
    orders
        .iter()
        .enumerate()
        .filter_map(|(index, wire)| {
            let coin = coin_for_asset(registry, table, wire.a)?;
            let cloid = post
                .cloids
                .get(index)
                .copied()
                .or_else(|| wire.c.as_deref().and_then(Cloid::from_hex))?;
            let limit_px = Decimal::from_str(wire.p.as_str()).ok()?;
            let size = Decimal::from_str(wire.s.as_str()).ok()?;
            let tif = match &wire.t {
                OrderType::Limit(limit) => tif_to_time_in_force(limit.tif),
                OrderType::Trigger(_) => return None,
            };
            Some(PaperOrder {
                cloid,
                coin,
                side: if wire.b { Side::Buy } else { Side::Sell },
                limit_px,
                size,
                tif,
                reduce_only: wire.r,
            })
        })
        .collect()
}

/// The cloids cancelled by a `cancelByCloid` post, in post order.
///
/// The engine's canonical cloids in `post.cloids` are preferred; the wire cloid
/// string is the fallback. Non-cancel posts yield an empty list.
pub fn paper_cancels_from_post(post: &UnsignedPost) -> Vec<Cloid> {
    let Action::CancelByCloid { cancels } = &post.action else {
        return Vec::new();
    };
    cancels
        .iter()
        .enumerate()
        .filter_map(|(index, cancel)| {
            post.cloids
                .get(index)
                .copied()
                .or_else(|| Cloid::from_hex(&cancel.cloid))
        })
        .collect()
}

/// Resolve a wire asset id back to an interned coin.
fn coin_for_asset(registry: &CoinRegistry, table: &AssetTable, asset_id: u32) -> Option<CoinId> {
    registry.iter().find_map(|(coin, _)| {
        table
            .get(coin)
            .filter(|market| market.asset_id() == asset_id)
            .map(|_| coin)
    })
}

/// Map a wire time in force to the strategy-facing one.
fn tif_to_time_in_force(tif: Tif) -> TimeInForce {
    match tif {
        Tif::Alo => TimeInForce::Alo,
        Tif::Ioc => TimeInForce::Ioc,
        Tif::Gtc => TimeInForce::Gtc,
    }
}

/// Map an engine side to the strategy-facing one (for book walks).
fn strategy_side(side: Side) -> mev_strategy::Side {
    match side {
        Side::Buy => mev_strategy::Side::Buy,
        Side::Sell => mev_strategy::Side::Sell,
    }
}

/// An `OrderUpdate` snapshot for an order at its current fill state.
fn order_update(order: &TrackedOrder, status: VenueOrderStatus, stamp: Stamp) -> AccountUpdate {
    let avg_px = if order.filled_sz > Decimal::ZERO {
        order.filled_notional / order.filled_sz
    } else {
        Decimal::ZERO
    };
    AccountUpdate::OrderUpdate {
        stamp,
        cloid: order.cloid,
        oid: order.oid,
        status,
        filled_sz: order.filled_sz,
        avg_px,
    }
}

/// A deterministic event stamp from the caller-supplied milliseconds.
fn stamp(now_ms: u64) -> Stamp {
    Stamp {
        t_recv_ns: i64::try_from(now_ms)
            .unwrap_or(i64::MAX)
            .saturating_mul(1_000_000),
        mono_ns: now_ms.saturating_mul(1_000_000),
        ts_exch_ms: now_ms,
    }
}

/// The strategy-facing book for a coin's slot, preferring the full snapshot and
/// falling back to the bbo. `None` when the slot is absent, stale, or empty.
fn book_for(coin: CoinId, markets: &[MarketSlot]) -> Option<BookView> {
    let slot = markets.get(coin.index())?;
    if slot.stale {
        return None;
    }
    if let Some((book, _)) = &slot.book {
        let view = BookView::from_levels(
            book.bids.iter().map(|level| (level.px, level.sz)),
            book.asks.iter().map(|level| (level.px, level.sz)),
            0,
            book.time_ms,
        );
        if !view.bids.is_empty() || !view.asks.is_empty() {
            return Some(view);
        }
    }
    let (bid, ask, _) = slot.bbo.as_ref()?;
    Some(BookView::from_levels(
        [(bid.px, bid.sz)],
        [(ask.px, ask.sz)],
        0,
        0,
    ))
}

/// The size a resting order could fill as a maker against `book`, if the book
/// trades through its limit.
fn maker_depth(order: &TrackedOrder, book: &BookView) -> Option<Sz> {
    let limit = order.limit_px;
    let available: Decimal = if order.is_buy() {
        if book.best_ask().is_none_or(|(px, _)| px > limit) {
            return None;
        }
        book.asks
            .iter()
            .take_while(|(px, _)| *px <= limit)
            .map(|(_, sz)| *sz)
            .sum()
    } else {
        if book.best_bid().is_none_or(|(px, _)| px < limit) {
            return None;
        }
        book.bids
            .iter()
            .take_while(|(px, _)| *px >= limit)
            .map(|(_, sz)| *sz)
            .sum()
    };
    (available > Decimal::ZERO).then(|| order.remaining().min(available))
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use mev_hl_client::AssetMap;
    use mev_hl_client::types::{AssetMeta as WireAssetMeta, Meta};
    use smallvec::smallvec;

    use super::*;
    use crate::state::AccountState;
    use crate::strategies::mm::{MarketMaker, MmConfig};
    use crate::strategy::{Action, Actions, Ctx, Strategy};
    use crate::types::{BookSnapshot, Level};

    fn ds(value: &str) -> Decimal {
        Decimal::from_str(value).unwrap()
    }

    fn registry() -> CoinRegistry {
        CoinRegistry::from_coins(&["BTC".into()])
    }

    fn asset_map() -> AssetMap {
        let mut map = AssetMap::new();
        map.insert_perp_dex(
            None,
            None,
            &Meta {
                universe: vec![WireAssetMeta {
                    name: "BTC".into(),
                    sz_decimals: 3,
                    max_leverage: 40,
                    is_delisted: false,
                    only_isolated: false,
                }],
            },
        );
        map
    }

    fn instruments() -> BTreeMap<String, Instrument> {
        let mut map = BTreeMap::new();
        map.insert("BTC".into(), Instrument::perp());
        map
    }

    fn paper(config: PaperConfig) -> PaperExec {
        PaperExec::new(
            config,
            AccountView {
                account_value: ds("100000"),
                ..Default::default()
            },
            instruments(),
            FeeRates::PERP,
            FeeRates::SPOT,
        )
    }

    fn slot_with(bid: &str, ask: &str, sz: &str) -> MarketSlot {
        let mut bids = [Level::default(); crate::types::BOOK_DEPTH];
        bids[0] = Level {
            px: ds(bid),
            sz: ds(sz),
            n: 1,
        };
        let mut asks = [Level::default(); crate::types::BOOK_DEPTH];
        asks[0] = Level {
            px: ds(ask),
            sz: ds(sz),
            n: 1,
        };
        MarketSlot {
            book: Some((
                BookSnapshot {
                    bids,
                    asks,
                    n_bids: 1,
                    n_asks: 1,
                    time_ms: 0,
                },
                Stamp::default(),
            )),
            ..Default::default()
        }
    }

    /// A one-element slice borrowing `slot`, avoiding a needless clone.
    fn one(slot: &MarketSlot) -> &[MarketSlot] {
        std::slice::from_ref(slot)
    }

    fn cloid(n: u8) -> Cloid {
        let mut bytes = [0u8; 16];
        bytes[0] = n;
        Cloid(bytes)
    }

    fn is_fill(update: &AccountUpdate) -> bool {
        matches!(update, AccountUpdate::Fill { .. })
    }

    fn order(side: Side, limit_px: &str, size: &str, tif: TimeInForce, id: u8) -> PaperOrder {
        PaperOrder {
            cloid: cloid(id),
            coin: CoinId(0),
            side,
            limit_px: ds(limit_px),
            size: ds(size),
            tif,
            reduce_only: false,
        }
    }

    #[test]
    fn taker_fills_only_at_or_after_its_latency_deadline() {
        let registry = registry();
        let mut paper = paper(PaperConfig {
            latency_ms: 20,
            maker_fills: true,
        });
        let market = slot_with("100", "100", "5");
        let order = order(Side::Buy, "101", "1", TimeInForce::Ioc, 1);

        assert!(
            paper
                .submit(&[order], &registry, one(&market), 0)
                .is_empty()
        );
        assert_eq!(paper.pending_len(), 1);

        let at19 = paper.on_market(&registry, one(&market), 19);
        assert!(
            !at19.iter().any(is_fill),
            "filled before the deadline: {at19:?}"
        );
        assert_eq!(paper.pending_len(), 1);

        let at20 = paper.on_market(&registry, one(&market), 20);
        assert!(
            at20.iter().any(is_fill),
            "did not fill at the deadline: {at20:?}"
        );
        assert!(
            at20.iter()
                .any(|update| matches!(update, AccountUpdate::Fill { px, .. } if *px == ds("100")))
        );
        assert_eq!(paper.account().position_szi("BTC"), ds("1"));
        assert_eq!(paper.pending_len(), 0);
        assert_eq!(paper.resting_len(), 0);
    }

    #[test]
    fn resting_alo_fills_only_after_a_later_cross_and_its_latency() {
        let registry = registry();
        let mut paper = paper(PaperConfig {
            latency_ms: 20,
            maker_fills: true,
        });
        let quiet = slot_with("99.9", "100.1", "5");
        let order = order(Side::Buy, "100", "1", TimeInForce::Alo, 2);
        paper.submit(&[order], &registry, one(&quiet), 0);

        let live = paper.on_market(&registry, one(&quiet), 20);
        assert!(live.iter().any(|update| matches!(
            update,
            AccountUpdate::OrderUpdate {
                status: VenueOrderStatus::Resting,
                ..
            }
        )));
        assert!(!live.iter().any(is_fill));
        assert_eq!(paper.resting_len(), 1);

        let crossed = slot_with("99.5", "99.9", "5");
        let cross_seen = paper.on_market(&registry, one(&crossed), 30);
        assert!(
            !cross_seen.iter().any(is_fill),
            "maker fill ignored latency: {cross_seen:?}"
        );

        let due = paper.on_market(&registry, one(&crossed), 50);
        let fill = due.iter().find(|update| is_fill(update)).expect("fill");
        // A maker fills at its own limit, not at the touched ask.
        assert!(matches!(fill, AccountUpdate::Fill { px, .. } if *px == ds("100")));
        assert_eq!(paper.account().position_szi("BTC"), ds("1"));
        assert_eq!(paper.resting_len(), 0);
    }

    #[test]
    fn post_only_that_would_cross_is_cancelled() {
        let registry = registry();
        let mut paper = paper(PaperConfig::default());
        let market = slot_with("100", "100", "5");
        let order = order(Side::Buy, "101", "1", TimeInForce::Alo, 3);
        paper.submit(&[order], &registry, one(&market), 0);

        let updates = paper.on_market(&registry, &[market], 20);
        assert!(!updates.iter().any(is_fill));
        assert!(updates.iter().any(|update| matches!(
            update,
            AccountUpdate::OrderUpdate {
                status: VenueOrderStatus::Cancelled,
                ..
            }
        )));
        assert_eq!(paper.pending_len(), 0);
        assert_eq!(paper.resting_len(), 0);
    }

    #[test]
    fn maker_fills_can_be_disabled() {
        let registry = registry();
        let mut paper = paper(PaperConfig {
            latency_ms: 0,
            maker_fills: false,
        });
        let quiet = slot_with("99.9", "100.1", "5");
        paper.submit(
            &[order(Side::Buy, "100", "1", TimeInForce::Alo, 4)],
            &registry,
            one(&quiet),
            0,
        );
        let live = paper.on_market(&registry, one(&quiet), 0);
        assert_eq!(paper.resting_len(), 1);
        assert!(!live.iter().any(is_fill));
        let crossed = slot_with("99.5", "99.9", "5");
        let updates = paper.on_market(&registry, &[crossed], 10);
        assert!(!updates.iter().any(is_fill));
        assert_eq!(paper.resting_len(), 1);
    }

    #[test]
    fn cancel_removes_pending_and_resting_orders() {
        let registry = registry();
        let mut paper = paper(PaperConfig {
            latency_ms: 20,
            maker_fills: true,
        });
        let market = slot_with("99.9", "100.1", "5");
        paper.submit(
            &[
                order(Side::Buy, "99.95", "1", TimeInForce::Alo, 5),
                order(Side::Buy, "99.9", "1", TimeInForce::Alo, 6),
            ],
            &registry,
            one(&market),
            0,
        );
        // Cancel one before it is eligible, then activate the other.
        let updates = paper.cancel(&[cloid(5)], 5);
        assert!(updates.iter().any(|update| matches!(
            update,
            AccountUpdate::OrderUpdate {
                status: VenueOrderStatus::Cancelled,
                ..
            }
        )));
        let live = paper.on_market(&registry, &[market], 20);
        assert_eq!(paper.pending_len(), 0);
        assert_eq!(paper.resting_len(), 1);
        assert!(live.iter().any(|update| matches!(
            update,
            AccountUpdate::OrderUpdate {
                status: VenueOrderStatus::Resting,
                ..
            }
        )));
        let cancelled = paper.cancel(&[cloid(6)], 25);
        assert_eq!(cancelled.len(), 1);
        assert_eq!(paper.resting_len(), 0);
        // Cancelling an unknown cloid is a no-op.
        assert!(paper.cancel(&[cloid(99)], 25).is_empty());
    }

    #[test]
    fn converts_built_orders_and_cancels_from_posts() {
        let registry = registry();
        let table = AssetTable::from_markets(&registry, &asset_map());
        let c = cloid(7);
        let wire = mev_hl_client::order::limit_order(
            0,
            true,
            "99.95",
            "1",
            Tif::Alo,
            false,
            Some(&c.to_hex()),
        );
        let post = UnsignedPost {
            req_id: 1,
            action: mev_hl_client::Action::Order {
                orders: vec![wire],
                grouping: mev_hl_client::Grouping::Na,
            },
            cloids: smallvec![c],
        };
        let orders = paper_orders_from_post(&post, &registry, &table);
        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].coin, CoinId(0));
        assert_eq!(orders[0].side, Side::Buy);
        assert_eq!(orders[0].limit_px, ds("99.95"));
        assert_eq!(orders[0].size, ds("1"));
        assert_eq!(orders[0].tif, TimeInForce::Alo);
        assert_eq!(orders[0].cloid, c);

        let cancel_post = UnsignedPost {
            req_id: 2,
            action: mev_hl_client::Action::CancelByCloid {
                cancels: vec![mev_hl_client::CancelByCloidWire {
                    asset: 0,
                    cloid: c.to_hex(),
                }],
            },
            cloids: smallvec![c],
        };
        assert_eq!(paper_cancels_from_post(&cancel_post), vec![c]);
    }

    /// Determinism (SPEC-0010 G-6): the same input sequence twice gives the same
    /// action log, the same account updates, and the same account.
    #[test]
    fn same_inputs_produce_identical_action_logs_and_account_updates() {
        let first = run_sequence();
        let second = run_sequence();
        assert_eq!(first.log, second.log, "sequence is not deterministic");
        assert_eq!(fnv1a(&first.log), fnv1a(&second.log));
        assert!(
            first.log.contains("Fill"),
            "expected at least one fill: {}",
            first.log
        );
        assert!(first.log.contains("market_making"));
        assert_eq!(first.account.position_szi("BTC"), ds("1"));
        assert_eq!(second.account.position_szi("BTC"), ds("1"));
    }

    struct SequenceResult {
        log: String,
        account: AccountView,
    }

    /// Two book updates drive a `MarketMaker`; `PaperExec` fills against them.
    /// No wall clock and no randomness: all times are literals and all cloids
    /// come from the strategy.
    fn run_sequence() -> SequenceResult {
        let registry = registry();
        let mut mm = MarketMaker::new(mm_config());
        let mut paper = paper(PaperConfig::default());
        let book1 = slot_with("100", "100", "1000");
        let book2 = slot_with("99.8", "99.93", "5");
        let mut log = String::new();

        drive_strategy(&mut mm, &registry, &mut paper, &book1, 0, &mut log);
        advance(&mut paper, &registry, &book1, 20, &mut log);
        drive_strategy(&mut mm, &registry, &mut paper, &book2, 30, &mut log);
        advance(&mut paper, &registry, &book2, 30, &mut log);
        advance(&mut paper, &registry, &book2, 50, &mut log);

        log.push_str(&format!("account: {:?}\n", paper.account()));
        let account = paper.account().clone();
        SequenceResult { log, account }
    }

    /// Run one strategy dispatch and feed its places/cancels to `PaperExec`.
    fn drive_strategy(
        mm: &mut MarketMaker,
        registry: &CoinRegistry,
        paper: &mut PaperExec,
        slot: &MarketSlot,
        now_ms: u64,
        log: &mut String,
    ) {
        let account = AccountState::new(1);
        let markets = [slot.clone()];
        let ctx = Ctx {
            now: stamp(now_ms),
            markets: &markets,
            account: &account,
            registry,
        };
        let mut actions = Actions::new();
        mm.on_market(CoinId(0), &ctx, &mut actions);
        let actions = actions.take();
        for action in &actions {
            log.push_str(&format!("{action:?}\n"));
        }
        let orders: Vec<PaperOrder> = actions
            .iter()
            .filter_map(|action| action_to_paper_order(action, registry))
            .collect();
        for update in paper.submit(&orders, registry, &markets, now_ms) {
            log.push_str(&format!("{update:?}\n"));
        }
        let cancels: Vec<Cloid> = actions
            .iter()
            .filter_map(|action| match action {
                Action::Cancel { cloid } => Some(*cloid),
                _ => None,
            })
            .collect();
        for update in paper.cancel(&cancels, now_ms) {
            log.push_str(&format!("{update:?}\n"));
        }
    }

    /// Advance paper matching and record the emitted updates.
    fn advance(
        paper: &mut PaperExec,
        registry: &CoinRegistry,
        slot: &MarketSlot,
        now_ms: u64,
        log: &mut String,
    ) {
        let markets = [slot.clone()];
        for update in paper.on_market(registry, &markets, now_ms) {
            log.push_str(&format!("{update:?}\n"));
        }
    }

    /// A typed paper order from a strategy place action. Test-only: the engine
    /// assigns cloids in `plan_iteration` in production; `MarketMaker` supplies
    /// a deterministic cloid per quote, so no assigner and no randomness here.
    fn action_to_paper_order(action: &Action, registry: &CoinRegistry) -> Option<PaperOrder> {
        let Action::Place(intent) = action else {
            return None;
        };
        let coin = registry.id(&intent.coin)?;
        let limit_px = intent.limit_px?;
        let cloid = intent.cloid.as_deref().and_then(Cloid::from_hex)?;
        Some(PaperOrder {
            cloid,
            coin,
            side: match intent.side {
                mev_strategy::Side::Buy => Side::Buy,
                mev_strategy::Side::Sell => Side::Sell,
            },
            limit_px,
            size: intent.size,
            tif: intent.tif,
            reduce_only: intent.reduce_only,
        })
    }

    fn mm_config() -> MmConfig {
        MmConfig {
            coin: CoinId(0),
            sz_decimals: 3,
            levels: 2,
            half_spread_bps: ds("5"),
            level_step_bps: ds("5"),
            size_per_level: ds("1"),
            max_inventory: ds("10"),
            max_skew_bps: ds("5"),
            vol_pull_bps: ds("50"),
            refresh_bps: ds("2"),
        }
    }

    /// FNV-1a over the canonical log, so a difference surfaces as a hash too.
    fn fnv1a(input: &str) -> u64 {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in input.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash
    }
}
