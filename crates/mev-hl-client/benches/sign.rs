//! Signing and submit-latency benchmarks (SPEC-0002 §15 / task H-7).
//!
//! - `sign/one_order`: build + msgpack + EIP-712 sign of a single order.
//! - `sign/batch_10`: the same for one `order` action carrying 10 orders.
//! - `ws_post/round_trip`: a mock-WS `post` round trip against an in-process
//!   server (localhost only, no venue, no network).
//!
//! The signing key is the public, zero-funds throwaway key already used by this
//! crate's tests. It is never a secret and never leaves this process.
//!
//! ## Quick mode
//!
//! ```sh
//! cargo bench -p mev-hl-client --bench sign -- --quick
//! # or, to shrink the sample/measurement window like the engine bench:
//! MEV_BENCH_QUICK=1 cargo bench -p mev-hl-client --bench sign
//! ```

use std::sync::{Arc, Mutex};
use std::time::Duration;

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use futures_util::{SinkExt, StreamExt};
use mev_core::config::Mode;
use mev_core::db::Db;
use mev_hl_client::order::limit_order;
use mev_hl_client::signing::AgentSigner;
use mev_hl_client::{Action, ExchangeApi, Tif, WriteCore, WsExchange, build_request};
use tokio::net::TcpListener;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

/// Public, zero-funds throwaway key (the second default Anvil/Hardhat account);
/// already public in `mev-hl-client`'s test suite. Never a real key.
const BENCH_AGENT_KEY: &str = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";

fn criterion_config() -> Criterion {
    let quick = std::env::var("MEV_BENCH_QUICK")
        .map(|v| v != "0")
        .unwrap_or(false);
    let c = Criterion::default();
    if quick {
        c.sample_size(10)
            .warm_up_time(Duration::from_millis(200))
            .measurement_time(Duration::from_secs(1))
    } else {
        c
    }
}

/// One order action; `n` orders in the action when `n > 1`.
fn order_action(n: u32) -> Action {
    let orders = (0..n)
        .map(|i| limit_order(i, true, "50000", "0.1", Tif::Gtc, false, None))
        .collect();
    Action::order(orders)
}

fn sign_one(c: &mut Criterion) {
    let signer = AgentSigner::from_hex(BENCH_AGENT_KEY, true).expect("bench signer");
    let action = order_action(1);
    let mut nonce = 1_700_000_000_000u64;
    assert!(
        build_request(&action, &signer, nonce, None, None).is_ok(),
        "one order must sign before benchmarking"
    );

    c.bench_function("sign/one_order", |b| {
        b.iter(|| {
            let request = build_request(black_box(&action), &signer, nonce, None, None);
            nonce = nonce.wrapping_add(1);
            black_box(request.is_ok());
        })
    });
}

fn sign_batch_10(c: &mut Criterion) {
    let signer = AgentSigner::from_hex(BENCH_AGENT_KEY, true).expect("bench signer");
    let action = order_action(10);
    let mut nonce = 1_700_000_000_000u64;
    assert!(
        build_request(&action, &signer, nonce, None, None).is_ok(),
        "a batch of 10 must sign before benchmarking"
    );

    c.bench_function("sign/batch_10", |b| {
        b.iter(|| {
            let request = build_request(black_box(&action), &signer, nonce, None, None);
            nonce = nonce.wrapping_add(1);
            black_box(request.is_ok());
        })
    });
}

/// An in-process mock venue: accept one connection and answer every `post` with
/// a resting-order action reply, routed back on the request's own `id`.
async fn spawn_mock_venue() -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock venue");
    let addr = listener.local_addr().expect("mock venue addr");
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let mut ws = accept_async(stream).await.expect("ws handshake");
        while let Some(Ok(message)) = ws.next().await {
            if let Message::Text(text) = message {
                let request: serde_json::Value =
                    serde_json::from_str(&text).expect("parse post frame");
                let id = request["id"].as_u64().expect("post id");
                let frame = serde_json::json!({
                    "channel": "post",
                    "data": {
                        "id": id,
                        "response": {
                            "type": "action",
                            "payload": {
                                "status": "ok",
                                "response": {"data": {"statuses": ["resting"]}},
                            },
                        },
                    },
                })
                .to_string();
                ws.send(Message::Text(frame.into())).await.expect("reply");
            }
        }
    });
    format!("ws://{addr}")
}

/// Full `WriteCore::prepare` stage for a live send with a durable nonce store:
/// nonce reservation + persistence + build/msgpack/sign. This is the H-6 path
/// whose persistence moved off the hot path (SPEC-0002 H-6).
fn prepare_live_nonce_store(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("bench runtime");
    let signer = AgentSigner::from_hex(BENCH_AGENT_KEY, true).expect("bench signer");
    let db = Arc::new(Mutex::new(Db::open_in_memory().expect("nonce db")));
    let core = WriteCore::new(Mode::Live, Some(signer))
        .expect("write core")
        .with_nonce_db(db)
        .expect("nonce store");
    let action = order_action(1);
    assert!(
        runtime.block_on(core.prepare(&action)).is_ok(),
        "prepare must succeed before benchmarking"
    );

    c.bench_function("prepare/live_nonce_store", |b| {
        b.iter(|| {
            let prepared = runtime.block_on(core.prepare(black_box(&action)));
            black_box(prepared.is_ok());
        })
    });
}

fn ws_post_round_trip(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("bench runtime");
    let url = runtime.block_on(spawn_mock_venue());
    let signer = AgentSigner::from_hex(BENCH_AGENT_KEY, true).expect("bench signer");
    let exchange = WsExchange::with_url(url, Mode::Live, Some(signer)).expect("ws exchange");
    let action = order_action(1);
    assert!(
        runtime.block_on(exchange.submit(&action)).is_ok(),
        "the mock venue must round-trip one post before benchmarking"
    );

    c.bench_function("ws_post/round_trip", |b| {
        b.iter(|| {
            let result = runtime.block_on(exchange.submit(black_box(&action)));
            black_box(result.is_ok());
        })
    });
}

criterion_group! {
    name = benches;
    config = criterion_config();
    targets = sign_one, sign_batch_10, prepare_live_nonce_store, ws_post_round_trip
}
criterion_main!(benches);
