//! Typed ingest benchmarks (SPEC-0010 E-2).
//!
//! - `decode/*` measures the typed decoder against recorded live frames
//!   (compared in ADR-0001 to the previous `Value`-based path).
//! - `handoff` measures an async `try_send` into a bounded channel received by
//!   a thread (the ingest → engine handoff), targeting p99 ≤ 10 µs.

use std::time::{Duration, Instant};

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use crossbeam_channel as _;
use mev_engine::ingest::Ingest;
use mev_engine::types::{CoinRegistry, ConnId, Stamp};

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

fn ingester() -> Ingest {
    Ingest::new(
        ConnId(0),
        CoinRegistry::from_coins(&["BTC".into(), "ETH".into(), "SOL".into(), "xyz:TSLA".into()]),
    )
}

fn decode_bench(c: &mut Criterion) {
    let ingest = ingester();
    let stamp = Stamp::default();
    for (name, raw) in [("l2Book", L2BOOK), ("trades", TRADES), ("ctx", CTX)] {
        let frames: Vec<&str> = raw.lines().collect();
        c.bench_function(&format!("decode/{name}"), |b| {
            b.iter(|| {
                for frame in &frames {
                    black_box(ingest.decode(black_box(frame), stamp).ok());
                }
            })
        });
    }
}

fn handoff_bench(c: &mut Criterion) {
    // A bounded channel standing in for the engine's market input; a receiver
    // thread drains it with the blocking `recv`, so `try_send` never blocks on
    // a full queue during the measurement.
    let (tx, rx) = crossbeam_channel::bounded::<u64>(65_536);
    let handle = std::thread::spawn(move || while rx.recv().is_ok() {});

    c.bench_function("handoff/try_send", |b| {
        let mut value = 0u64;
        b.iter(|| {
            value = value.wrapping_add(1);
            black_box(tx.try_send(black_box(value)).is_ok());
        })
    });

    drop(tx);
    let _ = handle.join();
}

fn ring_latency(c: &mut Criterion) {
    // Measure the round-trip the spec cares about: send → thread receive.
    let (tx, rx) = crossbeam_channel::bounded::<Stamp>(65_536);
    let (ack_tx, ack_rx) = crossbeam_channel::bounded::<u64>(65_536);
    let handle = std::thread::spawn(move || {
        while let Ok(stamp) = rx.recv() {
            let arrived = now_ns();
            if ack_tx.send(arrived.saturating_sub(stamp.mono_ns)).is_err() {
                break;
            }
        }
    });

    c.bench_function("handoff/thread_latency", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let stamp = Stamp {
                    mono_ns: now_ns(),
                    ..Default::default()
                };
                let _ = tx.try_send(stamp);
                if let Ok(ns) = ack_rx.recv() {
                    total += Duration::from_nanos(ns);
                }
            }
            total
        })
    });

    drop(tx);
    let _ = handle.join();
}

fn now_ns() -> u64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_nanos() as u64
}

criterion_group!(benches, decode_bench, handoff_bench, ring_latency);
criterion_main!(benches);
