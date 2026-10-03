//! Test fixtures shared by the ported strategies (SPEC-0010 E-4).

use rust_decimal::Decimal;

use crate::state::{AccountState, MarketSlot};
use crate::strategy::Ctx;
use crate::types::{AssetCtxLite, BookSnapshot, CoinId, CoinRegistry, Level, Stamp};

/// Coin 0 is the perp (`BTC`), coin 1 the spot pair (`@1`).
pub(crate) fn registry() -> CoinRegistry {
    CoinRegistry::from_coins(&["BTC".into(), "@1".into()])
}

fn ds_i64(value: i64) -> Decimal {
    Decimal::from(value)
}

fn book(bid: &str, ask: &str, sz: &str) -> MarketSlot {
    let mut bids = [Level::default(); crate::types::BOOK_DEPTH];
    bids[0] = Level {
        px: bid.parse().unwrap(),
        sz: sz.parse().unwrap(),
        n: 1,
    };
    let mut asks = [Level::default(); crate::types::BOOK_DEPTH];
    asks[0] = Level {
        px: ask.parse().unwrap(),
        sz: sz.parse().unwrap(),
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

/// The standard two-coin market (`BTC` perp + `@1` spot) with funding on BTC.
pub(crate) fn market_btc_spot(funding: &str, spot_bid: &str, spot_ask: &str) -> Vec<MarketSlot> {
    let mut perp = book("59990", "60010", "100");
    perp.ctx = Some((
        AssetCtxLite {
            funding: funding.parse().unwrap(),
            mark_px: ds_i64(60000),
            oracle_px: ds_i64(60000),
            open_interest: ds_i64(1),
        },
        Stamp::default(),
    ));
    vec![perp, book(spot_bid, spot_ask, "100")]
}

/// A market with one coin (`CoinId(0)`) and a single book level per side.
pub(crate) fn market_one_bbo(coin_bid: &str, coin_ask: &str) -> Vec<MarketSlot> {
    vec![book(coin_bid, coin_ask, "100")]
}

/// Build a [`Ctx`] for `now_ms` over the given market and account.
pub(crate) fn ctx_with<'a>(
    markets: &'a [MarketSlot],
    account: &'a AccountState,
    now_ms: u64,
) -> Ctx<'a> {
    Ctx {
        now: Stamp {
            t_recv_ns: (now_ms as i64) * 1_000_000,
            ..Default::default()
        },
        markets,
        account,
        registry: &COINS,
    }
}

/// Build a [`Ctx`] with a caller-supplied registry (for market-maker tests).
pub(crate) fn ctx_with_registry<'a>(
    markets: &'a [MarketSlot],
    account: &'a AccountState,
    now_ms: u64,
    registry: &'a CoinRegistry,
) -> Ctx<'a> {
    let mut ctx = ctx_with(markets, account, now_ms);
    ctx.registry = registry;
    ctx
}

/// An account with a perp position on `coin`.
pub(crate) fn account_with(coin: CoinId, szi: &str) -> AccountState {
    let mut account = AccountState::new(coin.index() + 1);
    account.set_position_szi(coin, szi.parse().unwrap());
    account
}

/// The shared registry used by [`ctx_with`].
pub(crate) static COINS: std::sync::LazyLock<CoinRegistry> = std::sync::LazyLock::new(registry);
