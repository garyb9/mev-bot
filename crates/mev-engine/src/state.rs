//! Engine-owned state and the dirty-coin set (SPEC-0010 §7, §9).
//!
//! E-3 lands the market slots and the conflation bookkeeping the loop needs;
//! the account, order manager, and risk state arrive in E-5/E-8/E-9 and will
//! extend this struct.

use std::collections::BTreeMap;

use rust_decimal::Decimal;

use crate::types::{AssetCtxLite, BookSnapshot, CoinId, Level, Px, Stamp};

/// Per-coin market state, indexed by `CoinId`.
#[derive(Debug, Clone, Default)]
pub struct MarketSlot {
    /// Best bid/offer with its receive stamp.
    pub bbo: Option<(Level, Level, Stamp)>,
    /// Latest book snapshot with its receive stamp.
    pub book: Option<(BookSnapshot, Stamp)>,
    /// Latest asset context with its receive stamp.
    pub ctx: Option<(AssetCtxLite, Stamp)>,
    /// Whether the coin's inputs are stale or inside a feed gap.
    pub stale: bool,
}

impl MarketSlot {
    /// The freshest best bid (prefers the later of `bbo` and the book top).
    pub fn best_bid(&self) -> Option<Level> {
        match (&self.bbo, &self.book) {
            (Some((bid, _, bstamp)), Some((book, kstamp))) => {
                let book_bid = book.best_bid();
                if bstamp.mono_ns >= kstamp.mono_ns {
                    Some(*bid)
                } else {
                    book_bid.or(Some(*bid))
                }
            }
            (Some((bid, _, _)), None) => Some(*bid),
            (None, Some((book, _))) => book.best_bid(),
            (None, None) => None,
        }
    }

    /// The freshest best ask (prefers the later of `bbo` and the book top).
    pub fn best_ask(&self) -> Option<Level> {
        match (&self.bbo, &self.book) {
            (Some((_, ask, bstamp)), Some((book, kstamp))) => {
                let book_ask = book.best_ask();
                if bstamp.mono_ns >= kstamp.mono_ns {
                    Some(*ask)
                } else {
                    book_ask.or(Some(*ask))
                }
            }
            (Some((_, ask, _)), None) => Some(*ask),
            (None, Some((book, _))) => book.best_ask(),
            (None, None) => None,
        }
    }
}

/// The engine's market state plus the set of dirty coins to dispatch.
#[derive(Debug, Default)]
pub struct EngineState {
    slots: Vec<MarketSlot>,
    dirty: Vec<bool>,
    dirty_any: bool,
}

impl EngineState {
    /// Build state for `coin_count` coins.
    pub fn new(coin_count: usize) -> Self {
        Self {
            slots: vec![MarketSlot::default(); coin_count],
            dirty: vec![false; coin_count],
            dirty_any: false,
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
            bbo: Some((level(100), level(101), stamp(10))),
            book: Some((book, stamp(20))),
            ..Default::default()
        };
        assert_eq!(slot.best_bid().unwrap().px, Decimal::from(102));
        assert_eq!(slot.best_ask().unwrap().px, Decimal::from(103));

        // bbo is newer: it wins.
        slot.bbo = Some((level(200), level(201), stamp(30)));
        assert_eq!(slot.best_bid().unwrap().px, Decimal::from(200));
        assert_eq!(slot.best_ask().unwrap().px, Decimal::from(201));
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
}
