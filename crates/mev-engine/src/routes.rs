//! Precomputed dispatch routes (SPEC-0010 §5, §8).
//!
//! Strategies declare which coins and streams they care about once at startup;
//! [`Routes`] turns that into per-coin index lists so the engine never scans
//! unrelated strategies on the hot path.

use crate::types::CoinId;

/// Which market stream a strategy is interested in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Stream {
    /// Best bid/offer updates.
    Bbo,
    /// Book snapshots.
    Book,
    /// Trade prints.
    Trades,
    /// Asset context (funding/mark/oracle/OI).
    Ctx,
}

/// A strategy's declared interests (all fields optional).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Interests {
    /// Coins the strategy reacts to; empty means "none".
    pub coins: Vec<CoinId>,
    /// Streams it reacts to; empty means "all market streams".
    pub streams: Vec<Stream>,
    /// Whether it needs a lossless trades tape (SPEC-0010 §5).
    pub lossless_trades: bool,
}

impl Interests {
    /// A strategy watching `coins` on all streams.
    pub fn coins(coins: impl IntoIterator<Item = CoinId>) -> Self {
        Self {
            coins: coins.into_iter().collect(),
            streams: Vec::new(),
            lossless_trades: false,
        }
    }

    /// Whether this interest set includes `stream`.
    pub fn wants(&self, stream: Stream) -> bool {
        self.streams.is_empty() || self.streams.contains(&stream)
    }
}

/// Per-coin dispatch lists, indexed by `CoinId.index()`.
#[derive(Debug, Clone, Default)]
pub struct Routes {
    /// For each coin, the strategy indices interested in it.
    by_coin: Vec<Vec<usize>>,
}

impl Routes {
    /// Build routes from a coordinator count and each strategy's interests.
    ///
    /// `interests[i]` is the interest set of strategy `i`; the number of coins
    /// is `coin_count`. The returned lists are sorted ascending, so dispatch
    /// order matches strategy registration order.
    pub fn build(coin_count: usize, interests: &[Interests]) -> Self {
        let mut by_coin = vec![Vec::new(); coin_count];
        for (index, interest) in interests.iter().enumerate() {
            for coin in &interest.coins {
                if let Some(list) = by_coin.get_mut(coin.index()) {
                    list.push(index);
                }
            }
        }
        for list in &mut by_coin {
            list.sort_unstable();
            list.dedup();
        }
        Self { by_coin }
    }

    /// The strategy indices interested in `coin`, or an empty slice.
    pub fn for_coin(&self, coin: CoinId) -> &[usize] {
        self.by_coin
            .get(coin.index())
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Number of coins covered.
    pub fn coin_count(&self) -> usize {
        self.by_coin.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_per_coin_lists_in_registration_order() {
        let interests = vec![
            Interests::coins([CoinId(0), CoinId(2)]), // strategy 0
            Interests::coins([CoinId(0)]),            // strategy 1
            Interests::coins([CoinId(1)]),            // strategy 2
        ];
        let routes = Routes::build(3, &interests);
        assert_eq!(routes.for_coin(CoinId(0)), &[0, 1]);
        assert_eq!(routes.for_coin(CoinId(1)), &[2]);
        assert_eq!(routes.for_coin(CoinId(2)), &[0]);
        assert_eq!(routes.for_coin(CoinId(9)), &[] as &[usize]);
    }

    #[test]
    fn out_of_range_coins_are_ignored() {
        let interests = vec![Interests::coins([CoinId(5)])];
        let routes = Routes::build(2, &interests);
        assert_eq!(routes.coin_count(), 2);
        assert!(routes.for_coin(CoinId(0)).is_empty());
    }

    #[test]
    fn empty_streams_means_all_streams() {
        let interest = Interests::coins([CoinId(0)]);
        for stream in [Stream::Bbo, Stream::Book, Stream::Trades, Stream::Ctx] {
            assert!(interest.wants(stream));
        }
        let filtered = Interests {
            streams: vec![Stream::Bbo],
            ..Interests::coins([CoinId(0)])
        };
        assert!(filtered.wants(Stream::Bbo));
        assert!(!filtered.wants(Stream::Trades));
    }
}
