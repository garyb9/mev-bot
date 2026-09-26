//! Ported strategies on the v2 synchronous API (SPEC-0010 E-4).
//!
//! `FundingBasis` and `MarketMaker` moved here from `mev-strategy` when their
//! interface became the engine-owned [`crate::strategy::Strategy`]: their
//! decision context is engine state (interned coins, market slots), which
//! `mev-strategy` cannot see without a dependency cycle. Their pure math (the
//! [`mev_strategy::CostModel`], sizing, views) still lives in `mev-strategy`.

pub mod funding;
pub mod mm;

use mev_strategy::BookView;

use crate::state::MarketSlot;

#[cfg(test)]
pub(crate) mod test_support;

/// Build a strategy-facing [`BookView`] from an engine [`MarketSlot`].
///
/// Returns `None` when the slot has no book snapshot or it is stale (a stale
/// book must not be quoted against; SPEC-0010 §7).
pub fn book_view(slot: &MarketSlot, sz_decimals: u32) -> Option<BookView> {
    if slot.stale {
        return None;
    }
    let (book, _) = slot.book.as_ref()?;
    Some(BookView::from_levels(
        book.bids.iter().map(|level| (level.px, level.sz)),
        book.asks.iter().map(|level| (level.px, level.sz)),
        sz_decimals,
        book.time_ms,
    ))
}
