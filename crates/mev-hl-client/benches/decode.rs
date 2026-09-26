//! WS decode benchmarks over recorded live frames (SPEC-0001 §10).
//!
//! Run: `cargo bench -p mev-hl-client`. Fixtures are recorded with
//! `cargo run -p mev-hl-client --example capture`.

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use mev_hl_client::ws::decode;

const CHANNELS: [&str; 4] = ["l2Book", "trades", "activeAssetCtx", "allMids"];

fn fixtures(channel: &str) -> &'static str {
    match channel {
        "l2Book" => include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/benches/fixtures/l2Book.jsonl"
        )),
        "trades" => include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/benches/fixtures/trades.jsonl"
        )),
        "activeAssetCtx" => include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/benches/fixtures/activeAssetCtx.jsonl"
        )),
        "allMids" => include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/benches/fixtures/allMids.jsonl"
        )),
        other => panic!("unknown channel {other}"),
    }
}

fn decode_bench(c: &mut Criterion) {
    for channel in CHANNELS {
        let frames: Vec<&str> = fixtures(channel).lines().collect();

        c.bench_function(&format!("decode/{channel}"), |b| {
            b.iter(|| {
                for frame in &frames {
                    black_box(decode(black_box(frame)).ok());
                }
            })
        });

        c.bench_function(&format!("json_value/{channel}"), |b| {
            b.iter(|| {
                for frame in &frames {
                    black_box(serde_json::from_str::<serde_json::Value>(black_box(frame)).ok());
                }
            })
        });
    }
}

criterion_group!(benches, decode_bench);
criterion_main!(benches);
