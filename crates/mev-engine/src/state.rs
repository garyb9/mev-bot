//! Engine-owned state and the dirty-coin set (SPEC-0010 §7, §9).
//!
//! E-3 lands the market slots and the conflation bookkeeping the loop needs;
//! the account, order manager, and risk state arrive in E-5/E-8/E-9 and will
//! extend this struct.

use std::collections::BTreeMap;

use rust_decimal::Decimal;

use crate::types::{AccountUpdate, AssetCtxLite, BookSnapshot, CoinId, Level, Px, Stamp};

/// Per-coin market state, indexed by `CoinId`.
#[derive(Debug, Clone, Default)]
pub struct MarketSlot {
    /// Best bid/offer with its receive stamp. A side is `None` when the venue
    /// reports that side of the book empty; that is a real quote state, not a
    /// zero price.
    pub bbo: Option<(Option<Level>, Option<Level>, Stamp)>,
    /// Latest book snapshot with its receive stamp.
    pub book: Option<(BookSnapshot, Stamp)>,
    /// Latest asset context with its receive stamp.
    pub ctx: Option<(AssetCtxLite, Stamp)>,
    /// Whether the coin's inputs are stale or inside a feed gap.
    pub stale: bool,
    /// Whether the coin's asset context (funding rate, mark) is stale: set by a
    /// feed gap and cleared only by a `Ctx` update received strictly after the
    /// gap.
    ///
    /// Kept separate from [`MarketSlot::stale`] because a fresh book snapshot
    /// (which clears book staleness and gates orders in the risk gate) does not
    /// refresh the `Ctx`. A strategy that reads the ctx (the funding rate) must
    /// treat a stale ctx as "no data", while strategies that never read it
    /// (market making) and the risk gate stay unaffected (SPEC-0010 §23
    /// Q-Gap-Edge (c)).
    pub ctx_stale: bool,
}

impl MarketSlot {
    /// Whether the `bbo` source is at least as fresh as the `book` source.
    ///
    /// The receive stamp is per source, so both sides of a slot share this
    /// decision (a tie goes to `bbo`). `None` when the slot has neither source.
    /// Callers that need a single side use this to pick the source before
    /// reading it: an empty side in the fresher source is information and must
    /// not fall back to the older one.
    pub fn bbo_is_fresher(&self) -> Option<bool> {
        match (&self.bbo, &self.book) {
            (Some((_, _, bstamp)), Some((_, kstamp))) => Some(bstamp.mono_ns >= kstamp.mono_ns),
            (Some(_), None) => Some(true),
            (None, Some(_)) => Some(false),
            (None, None) => None,
        }
    }

    /// The freshest best bid (prefers the later of `bbo` and the book top).
    ///
    /// Whichever source is fresher wins outright: an empty bid side in the
    /// fresher source returns `None` rather than falling back to the older
    /// source's level, because an empty fresh side is information.
    pub fn best_bid(&self) -> Option<Level> {
        if self.bbo_is_fresher()? {
            self.bbo.as_ref().and_then(|(bid, _, _)| *bid)
        } else {
            self.book.as_ref().and_then(|(book, _)| book.best_bid())
        }
    }

    /// The stamp of the freshest of `bbo`, `book`, and `ctx` (by monotonic
    /// time), or `None` when the slot has no data yet.
    ///
    /// Used as the event time for a coin's dispatch so strategies see the time
    /// of the update that triggered them (SPEC-0010 §8).
    pub fn latest_stamp(&self) -> Option<Stamp> {
        let mut best: Option<Stamp> = None;
        let mut fold = |stamp: Stamp| {
            if stamp.mono_ns > 0 && best.is_none_or(|b| stamp.mono_ns >= b.mono_ns) {
                best = Some(stamp);
            }
        };
        if let Some((_, _, stamp)) = &self.bbo {
            fold(*stamp);
        }
        if let Some((_, stamp)) = &self.book {
            fold(*stamp);
        }
        if let Some((_, stamp)) = &self.ctx {
            fold(*stamp);
        }
        best
    }

    /// The freshest best ask (prefers the later of `bbo` and the book top).
    ///
    /// Whichever source is fresher wins outright: an empty ask side in the
    /// fresher source returns `None` rather than falling back to the older
    /// source's level, because an empty fresh side is information.
    pub fn best_ask(&self) -> Option<Level> {
        if self.bbo_is_fresher()? {
            self.bbo.as_ref().and_then(|(_, ask, _)| *ask)
        } else {
            self.book.as_ref().and_then(|(book, _)| book.best_ask())
        }
    }
}

/// The engine's market state plus the set of dirty coins to dispatch.
#[derive(Debug, Default)]
pub struct EngineState {
    slots: Vec<MarketSlot>,
    dirty: Vec<bool>,
    dirty_any: bool,
    /// Monotonic time of the most recent market-feed gap (0 before the first).
    /// A book snapshot is only allowed to clear staleness if it is newer than
    /// this, so a pre-gap snapshot that is still queued when the gap is noticed
    /// cannot resurrect a coin with old data.
    gap_mono_ns: u64,
}

impl EngineState {
    /// Build state for `coin_count` coins.
    pub fn new(coin_count: usize) -> Self {
        Self {
            slots: vec![MarketSlot::default(); coin_count],
            dirty: vec![false; coin_count],
            dirty_any: false,
            gap_mono_ns: 0,
        }
    }

    /// The market slot for a coin, if in range.
    pub fn slot(&self, coin: CoinId) -> Option<&MarketSlot> {
        self.slots.get(coin.index())
    }

    /// Mutable access to a coin's slot.
    pub fn slot_mut(&mut self, coin: CoinId) -> Option<&mut MarketSlot> {
        self.slots.get_mut(coin.index())
    }

    /// All slots, for read-only strategy context.
    pub fn slots(&self) -> &[MarketSlot] {
        &self.slots
    }

    /// Mark a coin dirty so the loop dispatches it once after draining.
    pub fn mark_dirty(&mut self, coin: CoinId) {
        if let Some(flag) = self.dirty.get_mut(coin.index())
            && !*flag
        {
            *flag = true;
            self.dirty_any = true;
        }
    }

    /// Whether any coin is dirty.
    pub fn has_dirty(&self) -> bool {
        self.dirty_any
    }

    /// Mark every coin stale at monotonic time `mono_ns` (a market-feed gap
    /// opened on a shared connection, so we cannot attribute it to one coin —
    /// SPEC-0010 §16). The time is retained: only a book snapshot received
    /// strictly after the latest gap may clear a coin's staleness.
    ///
    /// `mono_ns` must be the time the drop was **detected**, not the engine's
    /// iteration start. The market drain can still be pulling frames that were
    /// queued on the socket before it died, so an iteration-start reading could
    /// predate them and let a pre-gap book clear the flag. The live caller
    /// passes the `RawEvent::Gap::disconnect_ns` the ingest task received
    /// (stamped by [`mev_hl_client::raw_ws::mono_ns`], the same clock as frame
    /// stamps: `raw_ws.rs:388` and `raw_ws.rs:331`; the engine's
    /// [`crate::clock::LiveClock`] maps its monotonic lane to that clock at
    /// `clock.rs:65`). When only the untimestamped fail-closed latch fired, the
    /// caller passes the control-drain time instead; see
    /// [`crate::run::EngineLoop`]'s control drain.
    ///
    /// A book received just after the drop on a not-yet-closed socket cannot
    /// exist: [`mev_hl_client::RawEvent::Gap`] is yielded the moment the socket
    /// breaks, before any reconnect, and a single ingest task processes frames
    /// in order, so no frame from the dead socket is read after the drop. Any
    /// frame after the drop comes from the reconnected socket and is stamped at
    /// its new receive time (`> disconnect_ns`), so it correctly clears
    /// staleness.
    pub fn mark_all_stale(&mut self, mono_ns: u64) {
        self.gap_mono_ns = self.gap_mono_ns.max(mono_ns);
        for slot in &mut self.slots {
            slot.stale = true;
            slot.ctx_stale = true;
        }
    }

    /// Clear a coin's ctx-stale flag when an asset-context update is received
    /// strictly after the latest gap.
    ///
    /// `Ctx` carries the funding rate and mark price. Unlike a book snapshot it
    /// neither prices nor sizes an order, so it does **not** clear
    /// [`MarketSlot::stale`]; but a strategy that reads it must not decide on a
    /// pre-gap rate while the book looks fresh, so the ctx gets its own
    /// freshness cut with the same strict-greater rule as [`Self::mark_fresh`].
    /// See `MarketSlot::ctx_stale`.
    pub fn mark_ctx_fresh(&mut self, coin: CoinId, mono_ns: u64) {
        if mono_ns <= self.gap_mono_ns {
            return;
        }
        if let Some(slot) = self.slots.get_mut(coin.index()) {
            slot.ctx_stale = false;
        }
    }

    /// Clear a coin's stale flag when a full **l2 book snapshot** received since
    /// the latest gap arrives.
    ///
    /// This is the one place that decides what "fresh" means for a coin.
    /// Staleness is cleared only by a book snapshot, because that is the
    /// input the strategies consume to price and size (`book_view` reads
    /// [`MarketSlot::book`]): a fresh `bbo`, trade, or asset-context update
    /// leaves the l2 book pre-gap, so clearing on one of those would let a
    /// strategy quote against pre-gap depth. A fresh book also supersedes any
    /// older `bbo` in [`MarketSlot::best_bid`]/[`MarketSlot::best_ask`] by
    /// timestamp, so every strategy read is then post-gap.
    ///
    /// `mono_ns` is the book snapshot's receive time. The rule is: after a gap,
    /// only a book whose receive stamp is **strictly greater** than the gap's
    /// detection time may clear staleness. A book queued before the drop always
    /// has a stamp `<=` the detection time on the same monotonic clock, so it
    /// is ignored here even if the engine only drains it after the gap signal.
    /// A book received after the drop comes from the reconnected socket and is
    /// stamped later, so it clears. See [`Self::mark_all_stale`] for the clock
    /// domain and the fail-closed drain-time fallback.
    pub fn mark_fresh(&mut self, coin: CoinId, mono_ns: u64) {
        if mono_ns <= self.gap_mono_ns {
            return;
        }
        if let Some(slot) = self.slots.get_mut(coin.index()) {
            slot.stale = false;
        }
    }

    /// Drain dirty coins in `CoinId` order (deterministic dispatch order).
    pub fn drain_dirty(&mut self) -> Vec<CoinId> {
        let mut out = Vec::new();
        if !self.dirty_any {
            return out;
        }
        for (index, flag) in self.dirty.iter_mut().enumerate() {
            if *flag {
                *flag = false;
                out.push(CoinId(index as u16));
            }
        }
        self.dirty_any = false;
        out
    }
}

/// Account state read by strategies and (later) risk (SPEC-0010 §7, §11).
///
/// Perp positions are indexed by [`CoinId`]; spot balances are keyed by token
/// symbol (tokens are not interned). E-5/E-8/E-9 extend this with margin,
/// open orders, and in-flight exposure.
#[derive(Debug, Clone, Default)]
pub struct AccountState {
    /// Signed perp size per coin (positive long, negative short).
    pub positions: Vec<Decimal>,
    /// Spot balances by token symbol.
    pub spot: BTreeMap<String, Decimal>,
    /// Perp account value in USD.
    pub account_value: Decimal,
    /// Margin currently used.
    pub margin_used: Decimal,
}

impl AccountState {
    /// Build account state sized for `coin_count` coins.
    pub fn new(coin_count: usize) -> Self {
        Self {
            positions: vec![Decimal::ZERO; coin_count],
            ..Default::default()
        }
    }

    /// Signed position size for a coin (zero when flat or out of range).
    pub fn position_szi(&self, coin: CoinId) -> Decimal {
        self.positions
            .get(coin.index())
            .copied()
            .unwrap_or(Decimal::ZERO)
    }

    /// Set a coin's signed position size (grows the vector if needed).
    pub fn set_position_szi(&mut self, coin: CoinId, szi: Decimal) {
        if self.positions.len() <= coin.index() {
            self.positions.resize(coin.index() + 1, Decimal::ZERO);
        }
        self.positions[coin.index()] = szi;
    }

    /// Spot balance for a token (zero when absent).
    pub fn spot_balance(&self, token: &str) -> Decimal {
        self.spot.get(token).copied().unwrap_or(Decimal::ZERO)
    }

    /// Confirmed exposure for a coin at `reference_px`: `|position| * px`.
    ///
    /// This is only the confirmed position; in-flight orders are accounted by
    /// [`crate::orders::OrderManager::pending_notional`] (SPEC-0010 §10, §11).
    pub fn projected_notional(&self, coin: CoinId, reference_px: Px) -> Decimal {
        self.position_szi(coin).abs() * reference_px
    }
}

/// Health and halt latches for the H-3 account stream (SPEC-0010 §15, §16).
///
/// The stream is the source of truth; a gap halts new places account-wide and
/// requests an immediate reconcile. Places resume only after a clean reconcile
/// with no `Unknown` orders and no remaining drift. All state is a handful of
/// booleans, so the steady path allocates nothing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AccountStreamState {
    healthy: bool,
    places_halted: bool,
    needs_reconcile: bool,
}

impl AccountStreamState {
    /// A fresh, healthy stream that is not halted (startup reconciles before
    /// trading; E-10 wires the initial reconcile).
    pub fn new() -> Self {
        Self {
            healthy: true,
            places_halted: false,
            needs_reconcile: false,
        }
    }

    /// Whether the account stream is delivering updates.
    pub fn is_healthy(&self) -> bool {
        self.healthy
    }

    /// Whether new (non-reduce-only) places are halted account-wide.
    pub fn places_halted(&self) -> bool {
        self.places_halted
    }

    /// Whether an immediate reconcile is outstanding.
    pub fn needs_reconcile(&self) -> bool {
        self.needs_reconcile
    }

    /// Record an account-stream gap: unhealthy, halt places, request reconcile.
    pub fn on_gap(&mut self) {
        self.healthy = false;
        self.places_halted = true;
        self.needs_reconcile = true;
    }

    /// Record that the stream reconnected. Places stay halted until a clean
    /// reconcile clears the halt.
    pub fn on_stream_resumed(&mut self) {
        self.healthy = true;
    }

    /// Apply an account update to stream health.
    ///
    /// Any update proves the stream is live. A `Reconcile` snapshot satisfies
    /// the outstanding reconcile request; whether places resume is decided by
    /// [`Self::on_clean_reconcile`] once the engine has checked for unknown
    /// orders and remaining drift.
    pub fn on_account_update(&mut self, update: &AccountUpdate) {
        self.healthy = true;
        if matches!(update, AccountUpdate::Reconcile { .. }) {
            self.needs_reconcile = false;
        }
    }

    /// Apply the outcome of a reconcile pass.
    ///
    /// Places resume only when there are no `Unknown` orders and no remaining
    /// drift. Returns whether places are now resumed.
    pub fn on_clean_reconcile(&mut self, has_unknown: bool, drift_remaining: bool) -> bool {
        self.needs_reconcile = false;
        if !has_unknown && !drift_remaining {
            self.places_halted = false;
        }
        !self.places_halted
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;

    fn level(px: i64) -> Level {
        Level {
            px: Decimal::from(px),
            sz: Decimal::ONE,
            n: 1,
        }
    }

    fn stamp(mono_ns: u64) -> Stamp {
        Stamp {
            mono_ns,
            ..Default::default()
        }
    }

    #[test]
    fn best_bid_prefers_the_fresher_source() {
        let mut bids = [Level::default(); crate::types::BOOK_DEPTH];
        bids[0] = level(102);
        let mut asks = [Level::default(); crate::types::BOOK_DEPTH];
        asks[0] = level(103);
        let book = BookSnapshot {
            n_bids: 1,
            n_asks: 1,
            bids,
            asks,
            ..Default::default()
        };
        // bbo is older than the book: the book top wins.
        let mut slot = MarketSlot {
            bbo: Some((Some(level(100)), Some(level(101)), stamp(10))),
            book: Some((book, stamp(20))),
            ..Default::default()
        };
        assert_eq!(slot.best_bid().unwrap().px, Decimal::from(102));
        assert_eq!(slot.best_ask().unwrap().px, Decimal::from(103));

        // bbo is newer: it wins.
        slot.bbo = Some((Some(level(200)), Some(level(201)), stamp(30)));
        assert_eq!(slot.best_bid().unwrap().px, Decimal::from(200));
        assert_eq!(slot.best_ask().unwrap().px, Decimal::from(201));
    }

    fn book_with(bid: Option<i64>, ask: Option<i64>) -> BookSnapshot {
        let mut bids = [Level::default(); crate::types::BOOK_DEPTH];
        let mut asks = [Level::default(); crate::types::BOOK_DEPTH];
        let mut n_bids = 0;
        if let Some(px) = bid {
            bids[0] = level(px);
            n_bids = 1;
        }
        let mut n_asks = 0;
        if let Some(px) = ask {
            asks[0] = level(px);
            n_asks = 1;
        }
        BookSnapshot {
            bids,
            asks,
            n_bids,
            n_asks,
            ..Default::default()
        }
    }

    #[test]
    fn empty_fresher_bbo_side_is_none_and_does_not_fall_back() {
        // Fresh bbo has an empty bid side while the (older) book still has a
        // top: the empty fresh side is information and must not surface the
        // stale book top or a zero price.
        let mut slot = MarketSlot {
            bbo: Some((None, Some(level(101)), stamp(20))),
            book: Some((book_with(Some(100), Some(101)), stamp(10))),
            ..Default::default()
        };
        assert_eq!(slot.best_bid(), None);
        assert_eq!(slot.best_ask().unwrap().px, Decimal::from(101));

        // The ask side may be the empty one.
        slot.bbo = Some((Some(level(100)), None, stamp(20)));
        assert_eq!(slot.best_ask(), None);
        assert_eq!(slot.best_bid().unwrap().px, Decimal::from(100));
    }

    #[test]
    fn empty_fresher_book_side_does_not_fall_back_to_older_bbo() {
        // The book is fresher and its bid side is empty: the older bbo bid is
        // stale and must not be returned.
        let slot = MarketSlot {
            bbo: Some((Some(level(99)), Some(level(102)), stamp(10))),
            book: Some((book_with(None, Some(101)), stamp(20))),
            ..Default::default()
        };
        assert_eq!(slot.best_bid(), None);
        assert_eq!(slot.best_ask().unwrap().px, Decimal::from(101));
    }

    #[test]
    fn older_empty_bbo_side_falls_back_to_the_fresher_book() {
        // The book is fresher, so its top is used even though the older bbo
        // reported an empty bid side.
        let slot = MarketSlot {
            bbo: Some((None, Some(level(99)), stamp(10))),
            book: Some((book_with(Some(100), Some(101)), stamp(20))),
            ..Default::default()
        };
        assert_eq!(slot.best_bid().unwrap().px, Decimal::from(100));
        assert_eq!(slot.best_ask().unwrap().px, Decimal::from(101));
    }

    #[test]
    fn dirty_coins_drain_in_id_order_and_once() {
        let mut state = EngineState::new(4);
        assert!(!state.has_dirty());
        state.mark_dirty(CoinId(3));
        state.mark_dirty(CoinId(1));
        state.mark_dirty(CoinId(1)); // duplicate collapses
        assert!(state.has_dirty());
        assert_eq!(state.drain_dirty(), vec![CoinId(1), CoinId(3)]);
        assert!(!state.has_dirty());
        assert_eq!(state.drain_dirty(), Vec::<CoinId>::new());
    }

    #[test]
    fn out_of_range_dirty_coins_are_ignored() {
        let mut state = EngineState::new(1);
        state.mark_dirty(CoinId(9));
        assert!(!state.has_dirty());
    }

    #[test]
    fn ctx_staleness_is_separate_from_book_staleness() {
        let mut state = EngineState::new(1);
        state.mark_all_stale(1_000);
        assert!(state.slot(CoinId(0)).unwrap().stale);
        assert!(state.slot(CoinId(0)).unwrap().ctx_stale);

        // A post-gap book clears book staleness, not the ctx's.
        state.mark_fresh(CoinId(0), 1_001);
        assert!(!state.slot(CoinId(0)).unwrap().stale);
        assert!(state.slot(CoinId(0)).unwrap().ctx_stale);

        // A post-gap ctx clears the ctx flag and does not touch book staleness.
        state.mark_ctx_fresh(CoinId(0), 1_002);
        assert!(!state.slot(CoinId(0)).unwrap().ctx_stale);
        assert!(!state.slot(CoinId(0)).unwrap().stale);

        // A pre-gap ctx is ignored; a post-gap one clears.
        state.mark_all_stale(2_000);
        state.mark_ctx_fresh(CoinId(0), 1_999);
        assert!(state.slot(CoinId(0)).unwrap().ctx_stale);
        state.mark_ctx_fresh(CoinId(0), 2_001);
        assert!(!state.slot(CoinId(0)).unwrap().ctx_stale);
    }

    #[test]
    fn account_state_reads_positions_and_spot() {
        let mut account = AccountState::new(2);
        assert_eq!(account.position_szi(CoinId(0)), Decimal::ZERO);
        account.set_position_szi(CoinId(1), Decimal::from(-3));
        assert_eq!(account.position_szi(CoinId(1)), Decimal::from(-3));
        account.set_position_szi(CoinId(5), Decimal::from(9));
        assert_eq!(account.position_szi(CoinId(5)), Decimal::from(9));
        account.spot.insert("UBTC".into(), Decimal::from(2));
        assert_eq!(account.spot_balance("UBTC"), Decimal::from(2));
        assert_eq!(account.spot_balance("NOPE"), Decimal::ZERO);
    }

    #[test]
    fn account_gap_halts_until_a_clean_reconcile() {
        let mut stream = AccountStreamState::new();
        assert!(stream.is_healthy());
        assert!(!stream.places_halted());
        assert!(!stream.needs_reconcile());

        stream.on_gap();
        assert!(!stream.is_healthy());
        assert!(stream.places_halted());
        assert!(stream.needs_reconcile());

        // Reconnect alone does not resume places.
        stream.on_stream_resumed();
        assert!(stream.is_healthy());
        assert!(stream.places_halted());

        // A reconcile with unknowns pending keeps the halt.
        assert!(!stream.on_clean_reconcile(true, false));
        assert!(stream.places_halted());
        assert!(!stream.needs_reconcile());

        // Drift remaining keeps the halt.
        assert!(!stream.on_clean_reconcile(false, true));
        assert!(stream.places_halted());

        // No unknowns, no drift: resume.
        assert!(stream.on_clean_reconcile(false, false));
        assert!(!stream.places_halted());
    }

    #[test]
    fn account_updates_mark_stream_healthy_and_satisfy_reconcile() {
        let mut stream = AccountStreamState::new();
        stream.on_gap();
        let update = AccountUpdate::Reconcile {
            stamp: Stamp::default(),
            snapshot: crate::types::AccountSnapshot::default(),
        };
        stream.on_account_update(&update);
        assert!(stream.is_healthy());
        assert!(!stream.needs_reconcile());
        // The halt is cleared only by the explicit clean-reconcile outcome.
        assert!(stream.places_halted());
    }
}
