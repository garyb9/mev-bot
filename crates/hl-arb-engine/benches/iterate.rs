//! Idle-loop cost benchmark (SPEC-0010 §23 Q-Gap-Edge loop-cost note).
//!
//! Measures one [`EngineLoop::iterate`] on an idle engine: no queued input, no
//! due timers, no dirty coins. This isolates the loop's per-iteration overhead
//! — the control-channel `try_recv` and the fail-closed latch check — from the
//! market/account work so the cost of the latch check is visible.

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use hl_arb_engine::channels::inputs;
use hl_arb_engine::routes::Interests;
use hl_arb_engine::run::{Dispatcher, EngineLoop, LoopConfig};

/// A dispatcher with no interests and no work.
struct Idle;

impl Dispatcher for Idle {
    fn interests(&self) -> Vec<Interests> {
        Vec::new()
    }
}

fn iterate_idle(c: &mut Criterion) {
    // Keep the producer handle alive so the channels stay connected: an
    // `iterate` over a disconnected channel is a different (also cheap) path.
    let (_handles, input) = inputs(64, 64);
    let (_stop_tx, stop_rx) = crossbeam_channel::bounded(1);
    let mut engine = EngineLoop::new(
        input,
        Idle,
        LoopConfig {
            spin_us: 0,
            coin_count: 0,
        },
        stop_rx,
    );

    c.bench_function("iterate/idle", |b| {
        let mut now = 0u64;
        b.iter(|| {
            now = now.wrapping_add(1_000);
            black_box(engine.iterate(black_box(now)));
        })
    });
}

criterion_group!(benches, iterate_idle);
criterion_main!(benches);
