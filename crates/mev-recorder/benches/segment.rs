//! Segment-writer throughput (SPEC-0008 R-2).
//!
//! The unit tests cover correctness with realistic frame sizes; the
//! 50k envelopes/s floor lives here, away from the shared (often loaded) test
//! run. Run with `cargo bench -p mev-recorder`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use mev_recorder::envelope::{Envelope, FixedEnvelopeClock};
use mev_recorder::segment::{SegmentConfig, SegmentWriter};

/// A roughly 1 KiB `l2Book`-like frame.
fn realistic_frame() -> String {
    let level = |i: u32| {
        format!(
            "{{\"px\":\"{}.{:02}\",\"sz\":\"1.{:02}\",\"n\":{i}}}",
            60_000 + i,
            i,
            i
        )
    };
    let side: Vec<String> = (0..20).map(level).collect();
    format!(
        "{{\"channel\":\"l2Book\",\"data\":{{\"coin\":\"BTC\",\"time\":1700000000000,\"levels\":[[{}],[{}]]}}}}",
        side.join(","),
        side.join(",")
    )
}

fn temp_dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("mev-rec-seg-bench-")
        .tempdir()
        .unwrap()
}

fn config(dir: PathBuf, clock: Arc<FixedEnvelopeClock>) -> SegmentConfig {
    SegmentConfig {
        out_dir: dir,
        network: "testnet".into(),
        src: "hl-ws".into(),
        conn: "hl-ws-01".into(),
        channel_capacity: 65_536,
        clock,
        ..SegmentConfig::default()
    }
}

/// Run a fixed write burst and assert the R-2 floor of 50k envelopes/s.
fn assert_throughput_floor() {
    let clock = Arc::new(FixedEnvelopeClock::new(1_767_227_400_000_000_000, 0));
    let dir = temp_dir();
    let writer = SegmentWriter::spawn(config(dir.path().to_path_buf(), clock.clone())).unwrap();

    let total: u64 = 100_000;
    let payload = realistic_frame();
    let start = Instant::now();
    for i in 0..total {
        clock.set_mono_ns(i);
        let env = Envelope::frame(&*clock, "hl-ws", "hl-ws-01", i, payload.clone());
        while !writer.try_send(env.clone()) {
            std::hint::spin_loop();
        }
    }
    let elapsed = start.elapsed();
    writer.shutdown();

    let rate = total as f64 / elapsed.as_secs_f64();
    println!("segment writer throughput: {total} envelopes in {elapsed:?} ({rate:.0}/s)");
    assert!(
        rate >= 50_000.0,
        "throughput {rate:.0}/s below the 50k/s floor"
    );
}

fn segment_writer(c: &mut Criterion) {
    assert_throughput_floor();

    let clock = Arc::new(FixedEnvelopeClock::new(1_767_227_400_000_000_000, 0));
    let dir = temp_dir();
    let writer = SegmentWriter::spawn(config(dir.path().to_path_buf(), clock.clone())).unwrap();
    let payload = realistic_frame();
    let mut seq = 0u64;
    c.bench_function("segment/try_send_1k", |b| {
        b.iter(|| {
            let env = Envelope::frame(&*clock, "hl-ws", "hl-ws-01", seq, payload.clone());
            seq = seq.wrapping_add(1);
            black_box(writer.try_send(env));
        })
    });
    writer.shutdown();
}

criterion_group!(benches, segment_writer);
criterion_main!(benches);
