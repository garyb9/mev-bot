//! Zero-allocation test for the `Bbo` hot path (SPEC-0010 §17, §3 G-3).
//!
//! G-3 requires **0 allocations per `Bbo` event** in steady state. This test
//! installs a counting global allocator (thread-local, so the test harness's own
//! allocations and other tests do not pollute the count), warms up the path,
//! then measures a fixed run and asserts the delta is exactly zero.
//!
//! It covers both the engine-owned part — dequeue + apply, which runs on the
//! engine thread — and the full `Ingest::decode` + apply path.
//!
//! ## Finding (E-10, 2026-09-27): the ingest decode allocates 1× per event
//!
//! The engine-side apply is **0 allocations/event**
//! ([`bbo_apply_is_allocation_free`], G-3 for the engine thread). The *decode*
//! path is not: `decode_market` peeks the channel with a borrowed `ChannelTag`,
//! and `serde_json`'s ignored-value handling for the skipped `data` object
//! allocates once whenever that object contains an object nested in an array —
//! exactly the `bbo`/`l2Book`/`trades` shapes. The typed `BboFrame` decode is
//! itself allocation-free, so the cost is the tag pre-parse, not event
//! construction. [`bbo_decode_path_records_real_allocs`] pins the real number so
//! a regression cannot hide it.
//!
//! Fix (out of E-10's file scope, for E-2/E-12): drop the `ChannelTag`
//! pre-parse and dispatch from one typed, borrowed envelope parse, or scan the
//! channel tag without `serde_json`. Until then this is a recorded G-3 gap to
//! report in SPEC-0010 §21.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use hl_arb_engine::ingest::Ingest;
use hl_arb_engine::state::EngineState;
use hl_arb_engine::types::{CoinId, CoinRegistry, ConnId, Level, MarketUpdate, Stamp};

thread_local! {
    /// Allocations observed on the current thread since start.
    static ALLOCS: Cell<usize> = const { Cell::new(0) };
}

/// A `System` allocator that counts every allocation on the calling thread.
struct CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        bump();
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        bump();
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        bump();
        unsafe { System.alloc_zeroed(layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

#[inline]
fn bump() {
    // `try_with` never panics if the TLS slot is unavailable mid-teardown.
    let _ = ALLOCS.try_with(|cell| cell.set(cell.get() + 1));
}

#[inline]
fn allocations() -> usize {
    ALLOCS.try_with(Cell::get).unwrap_or(0)
}

/// Mimics `run::apply_market` for the `Bbo` variant using public state.
fn apply_bbo(state: &mut EngineState, update: &MarketUpdate, coin: CoinId) {
    if let MarketUpdate::Bbo {
        bid, ask, stamp, ..
    } = update
        && let Some(slot) = state.slot_mut(coin)
    {
        slot.bbo = Some((*bid, *ask, *stamp));
    }
}

fn bbo_update() -> MarketUpdate {
    MarketUpdate::Bbo {
        coin: CoinId(0),
        stamp: Stamp {
            t_recv_ns: 1,
            mono_ns: 2,
            ts_exch_ms: 0,
        },
        bid: Some(Level {
            px: rust_decimal::Decimal::from(100),
            sz: rust_decimal::Decimal::ONE,
            n: 1,
        }),
        ask: Some(Level {
            px: rust_decimal::Decimal::from(101),
            sz: rust_decimal::Decimal::ONE,
            n: 1,
        }),
    }
}

/// G-3 for the engine thread: dequeue + apply is allocation-free.
#[test]
fn bbo_apply_is_allocation_free() {
    let update = bbo_update();
    let mut state = EngineState::new(1);
    let coin = CoinId(0);

    for _ in 0..10_000 {
        apply_bbo(&mut state, &update, coin);
    }

    const EVENTS: usize = 100_000;
    let before = allocations();
    for _ in 0..EVENTS {
        apply_bbo(&mut state, std::hint::black_box(&update), coin);
    }
    let after = allocations();

    assert_eq!(
        after - before,
        0,
        "engine Bbo apply allocated {} times over {EVENTS} events",
        after - before
    );
}

/// Allocations per `Bbo` frame through the full `Ingest::decode` path.
///
/// The real, measured number — **not** the 0 G-3 asks for; see the module docs.
const DECODE_ALLOCS_PER_EVENT: usize = 1;

/// Records the real decode-path cost: the `ChannelTag` pre-parse allocates 1×.
#[test]
fn bbo_decode_path_records_real_allocs() {
    let registry = CoinRegistry::from_coins(&["BTC".into()]);
    let ingest = Ingest::new(ConnId(0), registry);
    let frame = r#"{"channel":"bbo","data":{"coin":"BTC","time":1790413349393,"bbo":[{"px":"84178.0","sz":"3.45456","n":21},{"px":"84179.0","sz":"0.04112","n":3}]}}"#;
    let stamp = Stamp {
        t_recv_ns: 1,
        mono_ns: 2,
        ts_exch_ms: 0,
    };
    let mut state = EngineState::new(1);
    let coin = CoinId(0);

    for _ in 0..10_000 {
        let update = ingest
            .decode(frame, stamp)
            .expect("warmup decode")
            .expect("warmup update");
        apply_bbo(&mut state, &update, coin);
    }

    const EVENTS: usize = 100_000;
    let before = allocations();
    for _ in 0..EVENTS {
        let update = ingest
            .decode(std::hint::black_box(frame), stamp)
            .expect("decode")
            .expect("update");
        apply_bbo(&mut state, std::hint::black_box(&update), coin);
    }
    let after = allocations();

    assert_eq!(
        after - before,
        EVENTS * DECODE_ALLOCS_PER_EVENT,
        "Bbo decode+apply: expected {DECODE_ALLOCS_PER_EVENT} alloc/event, \
         got {} over {EVENTS} events",
        after - before
    );
}
