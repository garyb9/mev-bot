//! Unit tests for the exchange client (SPEC-0002).

use super::*;
use crate::order::{Action, Grouping, Tif, limit_order};
use crate::test_metrics::{counter_value, histogram_samples};
use hl_arb_core::clock::FixedClock;
use hl_arb_metrics::names;
use metrics_util::debugging::DebuggingRecorder;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const KEY: &str = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";

fn simple_action() -> Action {
    Action::Order {
        orders: vec![limit_order(0, true, "50000", "0.1", Tif::Gtc, false, None)],
        grouping: Grouping::Na,
    }
}

fn signer() -> AgentSigner {
    AgentSigner::from_hex(KEY, true).unwrap()
}

#[tokio::test]
async fn observe_blocks_without_sending() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/exchange"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"status":"ok"}"#))
        .expect(0)
        .mount(&server)
        .await;

    let exchange = HttpExchange::with_base_url(server.uri(), Mode::Observe, None).unwrap();
    assert_eq!(exchange.gate(), WriteGate::Blocked);
    let err = exchange.submit(&simple_action()).await.unwrap_err();
    assert!(matches!(err, Error::Config(_)), "got {err:?}");
}

#[tokio::test]
async fn simulate_signs_but_never_posts() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/exchange"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"status":"ok"}"#))
        .expect(0)
        .mount(&server)
        .await;

    let exchange =
        HttpExchange::with_base_url(server.uri(), Mode::Simulate, Some(signer())).unwrap();
    assert_eq!(exchange.gate(), WriteGate::DryRun);
    let response = exchange.submit(&simple_action()).await.unwrap();
    assert_eq!(response.value["status"], "simulated");
    assert!(response.value["request"]["signature"]["r"].is_string());
}

#[tokio::test]
async fn live_posts_and_parses_order_statuses() {
    let server = MockServer::start().await;
    let body = r#"{"status":"ok","response":{"type":"order","data":{"statuses":["resting"]}}}"#;
    Mock::given(method("POST"))
        .and(path("/exchange"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .expect(1)
        .mount(&server)
        .await;

    let exchange = HttpExchange::with_base_url(server.uri(), Mode::Live, Some(signer())).unwrap();
    let response = exchange.submit(&simple_action()).await.unwrap();
    assert_eq!(
        response.order_response().unwrap().statuses,
        vec![OrderStatus::Resting]
    );
}

#[tokio::test]
async fn live_maps_error_status_to_typed_error() {
    let server = MockServer::start().await;
    let body = r#"{"status":"err","response":"Must deposit before trading."}"#;
    Mock::given(method("POST"))
        .and(path("/exchange"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&server)
        .await;

    let exchange = HttpExchange::with_base_url(server.uri(), Mode::Live, Some(signer())).unwrap();
    let err = exchange.submit(&simple_action()).await.unwrap_err();
    match err {
        Error::Exchange(message) => assert!(message.contains("Must deposit")),
        other => panic!("expected exchange error, got {other:?}"),
    }
}

#[tokio::test]
async fn nonce_is_monotonic_across_submits() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/exchange"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"status":"ok"}"#))
        .mount(&server)
        .await;
    let exchange = HttpExchange::with_base_url(server.uri(), Mode::Live, Some(signer())).unwrap();

    let before = exchange.last_nonce().await;
    exchange.submit(&simple_action()).await.unwrap();
    let first = exchange.last_nonce().await;
    exchange.submit(&simple_action()).await.unwrap();
    let second = exchange.last_nonce().await;
    assert!(
        first > before && second > first,
        "{before} {first} {second}"
    );
}

#[test]
fn parses_per_order_rejections() {
    let value = json!({
        "data": { "statuses": [
            {"resting": {"oid": 1}},
            "filled",
            {"error": "tickRejected"},
            "someUnknownStatus"
        ]}
    });
    let response = OrderResponse::from_value(&value).unwrap();
    assert_eq!(
        response.statuses,
        vec![
            OrderStatus::Resting,
            OrderStatus::Filled,
            OrderStatus::Rejected(RejectReason::TickRejected),
            OrderStatus::Other("someUnknownStatus".into()),
        ]
    );
}

#[test]
fn envelope_preserves_action_field_order() {
    // The venue re-encodes the received action to verify the hash, so the
    // JSON field order must match the msgpack order.
    let action = simple_action();
    let request = build_request(&action, &signer(), 1, None, None).unwrap();
    let json = serde_json::to_string(&request).unwrap();
    let action_json = json
        .split("\"action\":")
        .nth(1)
        .and_then(|rest| rest.find(",\"nonce\"").map(|end| &rest[..end]))
        .unwrap();
    assert_eq!(
        action_json,
        r#"{"type":"order","orders":[{"a":0,"b":true,"p":"50000","s":"0.1","r":false,"t":{"limit":{"tif":"Gtc"}}}],"grouping":"na"}"#
    );
}

#[tokio::test]
async fn nonce_persists_across_instances() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/exchange"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"status":"ok"}"#))
        .mount(&server)
        .await;

    let db = Arc::new(Mutex::new(Db::open_in_memory().unwrap()));
    let exchange = HttpExchange::with_base_url(server.uri(), Mode::Live, Some(signer()))
        .unwrap()
        .with_nonce_db(db.clone())
        .unwrap();
    exchange.submit(&simple_action()).await.unwrap();
    let first = exchange.last_nonce().await;
    // The durable value is the write-ahead lease, so it covers what was sent.
    let persisted = db.lock().unwrap().nonce_last().unwrap().unwrap();
    assert!(persisted > first, "{persisted} must cover {first}");

    // A fresh instance restores the persisted high-water mark and never
    // regresses. The first send may be refused until the write-behind lease
    // refreshes (accepted); every sent nonce must exceed the previous one.
    let restarted = HttpExchange::with_base_url(server.uri(), Mode::Live, Some(signer()))
        .unwrap()
        .with_nonce_db(db.clone())
        .unwrap();
    let resumed = restarted.last_nonce().await;
    assert!(resumed >= first, "{resumed} must not be below {first}");
    let mut advanced = false;
    for _ in 0..50 {
        match restarted.submit(&simple_action()).await {
            Ok(_) => {
                advanced = true;
                break;
            }
            Err(Error::NotSent(_)) => {}
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }
    assert!(advanced, "the restarted instance must eventually send");
    assert!(restarted.last_nonce().await > resumed);
}

#[tokio::test]
async fn write_behind_nonce_survives_a_crash_without_a_flush() {
    // A fixed clock: the burst runs faster than 1/ms, so sent nonces run
    // ahead of the wall clock and the clock never advances to catch up.
    let clock = Arc::new(FixedClock::new(1_000_000));
    let db = Arc::new(Mutex::new(Db::open_in_memory().unwrap()));

    let mut sent = Vec::new();
    let prime;
    {
        let core = WriteCore::new(Mode::Live, Some(signer()))
            .unwrap()
            .with_clock(clock.clone())
            .with_nonce_lease(100)
            .with_nonce_db(db.clone())
            .unwrap();
        prime = db.lock().unwrap().nonce_last().unwrap().unwrap();
        for _ in 0..40 {
            match core.prepare(&simple_action()).await.unwrap() {
                Prepared::Send(request) => sent.push(request.nonce),
                Prepared::DryRun(_) => panic!("live mode must send"),
            }
        }
        // The burst fit inside the startup lease, so nothing was enqueued
        // write-behind: the database still holds only the lease.
        assert_eq!(db.lock().unwrap().nonce_last().unwrap(), Some(prime));
        // "Crash": drop without a graceful flush. Crash safety must not
        // depend on the per-order nonces.
    }
    let max_sent = *sent.iter().max().unwrap();
    assert_eq!(sent.len(), 40);
    assert!(
        max_sent < prime,
        "{max_sent} must fit inside the lease {prime}"
    );

    let restarted = WriteCore::new(Mode::Live, Some(signer()))
        .unwrap()
        .with_clock(clock)
        .with_nonce_lease(100)
        .with_nonce_db(db.clone())
        .unwrap();
    // The restored lease covers every sent nonce, so the first send after a
    // fast restart may be refused until the urgent refresh lands (the
    // accepted liveness cost); every send that does go out must be higher.
    let mut advanced = 0;
    for _ in 0..200 {
        match restarted.prepare(&simple_action()).await {
            Ok(Prepared::Send(request)) => {
                assert!(
                    request.nonce > max_sent,
                    "restarted nonce {} reuses or regresses below {max_sent}",
                    request.nonce
                );
                advanced += 1;
                if advanced == 10 {
                    break;
                }
            }
            Ok(Prepared::DryRun(_)) => panic!("live mode must send"),
            Err(Error::NotSent(_)) => {}
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }
    assert_eq!(advanced, 10, "the restarted instance must resume sending");
}

#[tokio::test]
async fn lease_refresh_is_persisted_write_behind() {
    let lease_ms = 100u64;
    let restored = 1_000_000u64;
    let clock = Arc::new(FixedClock::new(restored));
    let horizon = restored.max(restored + lease_ms);
    let durable = Arc::new(AtomicU64::new(0));
    let (tx, rx) = std::sync::mpsc::sync_channel(64);
    let core = WriteCore::new(Mode::Live, Some(signer()))
        .unwrap()
        .with_clock(clock)
        .with_nonce_lease(lease_ms)
        .with_test_nonce_store(
            NonceLease::with_channel(tx, horizon, lease_ms, durable.clone(), restored),
            restored,
        );

    // Send enough to spend the half-lease: a refresh is scheduled
    // write-behind and the durable horizon advances past the prime.
    let mut sent = Vec::new();
    for _ in 0..80 {
        match core.prepare(&simple_action()).await.unwrap() {
            Prepared::Send(request) => sent.push(request.nonce),
            Prepared::DryRun(_) => panic!("live mode must send"),
        }
    }
    let max_sent = *sent.iter().max().unwrap();

    // The refresh went to the writer channel, not to SQLite (no writer
    // thread, no sleeps): the highest queued horizon covers every send.
    let mut requested = horizon;
    while let Ok(value) = rx.try_recv() {
        requested = requested.max(value);
    }
    assert!(
        requested > horizon,
        "a write-behind refresh must be enqueued"
    );
    assert!(requested > max_sent, "the refresh must cover the burst");
    // The writer commits it: the confirmed durable mark advances.
    durable.fetch_max(requested, Ordering::AcqRel);
    assert!(durable.load(Ordering::Acquire) > horizon);
}

#[tokio::test]
async fn restore_nonce_never_lowers_below_the_last_issued() {
    // Interleaving, set up deterministically (no sleeps): a concurrent
    // `prepare` reserved `sent` and scheduled a write-behind refresh, but
    // the confirmed durable mark still trails it. The floor is measured
    // under the nonce lock, so a restore to 0 cannot lower the manager
    // below `sent` and hand it out twice.
    let lease_ms = 100u64;
    let restored = 1_000_000u64;
    let clock = Arc::new(FixedClock::new(restored));
    let horizon = restored + lease_ms;
    let durable = Arc::new(AtomicU64::new(horizon));
    let (tx, _rx) = std::sync::mpsc::sync_channel(64);
    let core = WriteCore::new(Mode::Live, Some(signer()))
        .unwrap()
        .with_clock(clock)
        .with_nonce_lease(lease_ms)
        .with_test_nonce_store(
            NonceLease::with_channel(tx, horizon, lease_ms, durable, restored),
            restored,
        );

    let sent = core.nonce.lock().await.next(restored).unwrap();
    // The write-behind horizon moved, but nothing has confirmed it yet.
    core.nonce_store
        .as_ref()
        .unwrap()
        .force(sent + lease_ms, restored);

    core.restore_nonce(0).await.unwrap();
    assert!(
        core.last_nonce().await >= sent,
        "restore lowered below the reserved nonce {sent}"
    );
    let (durable, requested) = core.nonce_marks().unwrap();
    assert!(durable >= sent && requested >= sent);
    let next = core.nonce.lock().await.next(restored).unwrap();
    assert!(next > sent, "next nonce {next} must exceed {sent}");
}

#[tokio::test]
async fn restore_nonce_never_lowers_the_durable_mark() {
    // The durable mark is ahead of the in-memory manager (a committed
    // lease): a restore below it must clamp up to the mark.
    let lease_ms = 100u64;
    let restored = 1_000_000u64;
    let clock = Arc::new(FixedClock::new(restored));
    let horizon = restored + lease_ms;
    let (tx, _rx) = std::sync::mpsc::sync_channel(64);
    let durable = Arc::new(AtomicU64::new(horizon));
    let core = WriteCore::new(Mode::Live, Some(signer()))
        .unwrap()
        .with_clock(clock)
        .with_nonce_lease(lease_ms)
        .with_test_nonce_store(
            NonceLease::with_channel(tx, horizon, lease_ms, durable, restored),
            restored,
        );

    core.restore_nonce(0).await.unwrap();
    let (durable, requested) = core.nonce_marks().unwrap();
    assert!(durable >= horizon, "{durable} lowered below {horizon}");
    assert!(requested >= horizon, "{requested} lowered below {horizon}");
    assert!(core.last_nonce().await >= horizon);
}

#[test]
fn future_refusal_log_is_rate_limited() {
    let last = AtomicU64::new(0);
    assert!(refusal_log_due(&last, 1_000));
    assert!(!refusal_log_due(&last, 1_000));
    assert!(!refusal_log_due(&last, 1_000 + NONCE_WARN_INTERVAL_MS - 1));
    assert!(refusal_log_due(&last, 1_000 + NONCE_WARN_INTERVAL_MS));
    assert!(!refusal_log_due(&last, 1_000 + NONCE_WARN_INTERVAL_MS));
}

#[tokio::test]
async fn prepare_does_not_write_the_database_synchronously() {
    let db = Arc::new(Mutex::new(Db::open_in_memory().unwrap()));
    // A lease far larger than the test burst guarantees no refresh is due.
    let core = WriteCore::new(Mode::Live, Some(signer()))
        .unwrap()
        .with_nonce_lease(1_000_000)
        .with_nonce_db(db.clone())
        .unwrap();
    let prime = db.lock().unwrap().nonce_last().unwrap().unwrap();
    let before = core.last_nonce().await;

    for _ in 0..50 {
        assert!(matches!(
            core.prepare(&simple_action()).await.unwrap(),
            Prepared::Send(_)
        ));
    }

    // Nonces advanced in memory, but the durable value is still the startup
    // prime: persistence went to the writer channel, not the database.
    assert!(core.last_nonce().await > before);
    assert_eq!(db.lock().unwrap().nonce_last().unwrap(), Some(prime));
}

#[tokio::test]
async fn uncommitted_refresh_is_not_trusted_on_restart() {
    use std::sync::atomic::{AtomicU64, Ordering};

    let clock = Arc::new(FixedClock::new(1_000_000));
    let horizon = 1_000_100u64;
    let lease_ms = 100u64;
    let durable = Arc::new(AtomicU64::new(0));
    let (tx, rx) = std::sync::mpsc::sync_channel(64);
    // The startup prime is on disk; the sink accepts refreshes but never
    // commits them, simulating a writer killed mid-write.
    let lease = NonceLease::with_channel(tx, horizon, lease_ms, durable.clone(), 1_000_000);
    let core = WriteCore::new(Mode::Live, Some(signer()))
        .unwrap()
        .with_clock(clock.clone())
        .with_test_nonce_store(lease, 1_000_000);

    let mut sent = Vec::new();
    for _ in 0..200 {
        match core.prepare(&simple_action()).await {
            Ok(Prepared::Send(request)) => sent.push(request.nonce),
            Ok(Prepared::DryRun(_)) => panic!("live mode must send"),
            Err(Error::NotSent(_)) => break,
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }
    assert!(rx.try_recv().is_ok(), "a refresh must have been enqueued");
    assert_eq!(
        durable.load(Ordering::Acquire),
        horizon,
        "a killed writer must not have advanced the durable mark"
    );
    let max_sent = *sent.iter().max().unwrap();
    assert!(max_sent <= horizon, "{max_sent} must not exceed {horizon}");

    // Restart from the value the writer actually made durable.
    let restart_horizon = horizon.saturating_add(1).max(1_000_000 + lease_ms);
    let durable2 = Arc::new(AtomicU64::new(0));
    let (tx2, rx2) = std::sync::mpsc::sync_channel(64);
    let restarted = WriteCore::new(Mode::Live, Some(signer()))
        .unwrap()
        .with_clock(clock)
        .with_test_nonce_store(
            NonceLease::with_channel(tx2, restart_horizon, lease_ms, durable2.clone(), 1_000_000),
            horizon,
        );
    for _ in 0..10 {
        match restarted.prepare(&simple_action()).await.unwrap() {
            Prepared::Send(request) => assert!(
                request.nonce > max_sent,
                "{} must exceed {max_sent}",
                request.nonce
            ),
            Prepared::DryRun(_) => panic!("live mode must send"),
        }
        // Pretend the writer commits the queued refresh.
        if let Ok(committed) = rx2.try_recv() {
            durable2.fetch_max(committed, Ordering::AcqRel);
        }
    }
}

#[tokio::test]
async fn prepare_refuses_and_counts_when_the_writer_is_gone() {
    let recorder = DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();
    let clock = Arc::new(FixedClock::new(1_000_000));
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    drop(rx); // writer gone: every enqueue fails
    let core = metrics::with_local_recorder(&recorder, || {
        let durable = Arc::new(std::sync::atomic::AtomicU64::new(0));
        WriteCore::new(Mode::Live, Some(signer()))
            .unwrap()
            .with_clock(clock)
            .with_test_nonce_store(
                NonceLease::with_channel(tx, 1_000_100, 100, durable, 1_000_000),
                1_000_000,
            )
    });

    let mut refused = false;
    for _ in 0..200 {
        match core.prepare(&simple_action()).await {
            Ok(Prepared::Send(_)) => {}
            Ok(Prepared::DryRun(_)) => panic!("live mode must send"),
            Err(Error::NotSent(_)) => {
                refused = true;
                break;
            }
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }
    assert!(refused, "an absent writer must fail closed");
    assert_eq!(
        counter_value(
            snapshotter.snapshot(),
            names::NONCE_LEASE_REFUSALS_TOTAL,
            None
        ),
        1
    );
    assert!(
        counter_value(
            snapshotter.snapshot(),
            names::NONCE_PERSIST_DROPPED_TOTAL,
            None
        ) >= 1
    );
}

#[tokio::test]
async fn orders_resume_after_a_stalled_write_recovers() {
    use std::sync::atomic::{AtomicU64, Ordering};

    let clock = Arc::new(FixedClock::new(1_000_000));
    let durable = Arc::new(AtomicU64::new(0));
    let (tx, rx) = std::sync::mpsc::sync_channel::<u64>(1);
    let horizon = 1_000_100u64;
    let lease = NonceLease::with_channel(tx, horizon, 100, durable.clone(), 1_000_000);
    let core = WriteCore::new(Mode::Live, Some(signer()))
        .unwrap()
        .with_clock(clock)
        .with_test_nonce_store(lease, 1_000_000);

    let mut refused = false;
    for _ in 0..200 {
        match core.prepare(&simple_action()).await {
            Ok(Prepared::Send(_)) => {}
            Ok(Prepared::DryRun(_)) => panic!("live mode must send"),
            Err(Error::NotSent(_)) => {
                refused = true;
                break;
            }
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }
    assert!(refused, "a stalled writer must fail closed");

    // The writer recovers and commits the highest queued horizon.
    let mut committed = horizon;
    while let Ok(value) = rx.try_recv() {
        committed = committed.max(value);
    }
    assert!(committed > horizon, "a refresh must have been queued");
    durable.store(committed, Ordering::Release);

    match core.prepare(&simple_action()).await {
        Ok(Prepared::Send(_)) => {}
        other => panic!("orders should resume after the write recovers: {other:?}"),
    }
}

#[tokio::test]
async fn corrupt_persisted_nonce_fails_closed_until_reset() {
    let recorder = DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();
    let clock = Arc::new(FixedClock::new(1_000_000));
    let db = Arc::new(Mutex::new(Db::open_in_memory().unwrap()));
    // Beyond the venue's future window: it cannot be one we sent.
    db.lock()
        .unwrap()
        .set_nonce_last(1_000_000 + VENUE_MAX_FUTURE_MS + 1)
        .unwrap();

    let core = metrics::with_local_recorder(&recorder, || {
        WriteCore::new(Mode::Live, Some(signer()))
            .unwrap()
            .with_clock(clock)
            .with_nonce_db(db.clone())
            .unwrap()
    });

    // Fail closed: a typed refusal, and the corruption is counted.
    assert!(matches!(
        core.prepare(&simple_action()).await,
        Err(Error::NotSent(_))
    ));
    assert_eq!(
        counter_value(
            snapshotter.snapshot(),
            names::NONCE_RESUME_CORRUPT_TOTAL,
            None
        ),
        1
    );

    // The explicit operator reset clears it and orders flow again.
    core.reset_nonce().await.unwrap();
    assert!(matches!(
        core.prepare(&simple_action()).await.unwrap(),
        Prepared::Send(_)
    ));
}

#[tokio::test]
async fn runtime_future_refusal_uses_its_own_counter() {
    let recorder = DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();
    let now = 1_000_000u64;
    let clock = Arc::new(FixedClock::new(now));
    // A manager already beyond the venue future window: every `next` fails
    // closed until the clock catches up.
    let resume_from = now + VENUE_MAX_FUTURE_MS + 1;
    let (tx, _rx) = std::sync::mpsc::sync_channel(64);
    let core = metrics::with_local_recorder(&recorder, || {
        let durable = Arc::new(AtomicU64::new(resume_from));
        WriteCore::new(Mode::Live, Some(signer()))
            .unwrap()
            .with_clock(clock)
            .with_test_nonce_store(
                NonceLease::with_channel(tx, resume_from, DEFAULT_NONCE_LEASE_MS, durable, now),
                resume_from,
            )
    });

    for _ in 0..3 {
        assert!(matches!(
            core.prepare(&simple_action()).await,
            Err(Error::NotSent(_))
        ));
    }
    assert_eq!(
        counter_value(
            snapshotter.snapshot(),
            names::NONCE_FUTURE_REFUSALS_TOTAL,
            None
        ),
        3
    );
    assert_eq!(
        counter_value(
            snapshotter.snapshot(),
            names::NONCE_RESUME_CORRUPT_TOTAL,
            None
        ),
        0,
        "a runtime refusal must not reuse the boot corruption counter"
    );
}

#[tokio::test]
async fn reset_nonce_is_refused_while_not_corrupt() {
    let clock = Arc::new(FixedClock::new(1_000_000));
    let db = Arc::new(Mutex::new(Db::open_in_memory().unwrap()));
    let core = WriteCore::new(Mode::Live, Some(signer()))
        .unwrap()
        .with_clock(clock)
        .with_nonce_db(db)
        .unwrap();
    // Not corrupt: nonces may already have been issued, so a reset (which
    // may lower the value) could force a reuse and must be refused.
    match core.reset_nonce().await {
        Err(Error::Config(message)) => {
            assert!(message.contains("not flagged corrupt"), "{message}")
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    // The refusal must not disturb the send path.
    assert!(matches!(
        core.prepare(&simple_action()).await.unwrap(),
        Prepared::Send(_)
    ));
}

#[tokio::test]
async fn heal_nonce_is_refused_while_corrupt() {
    let clock = Arc::new(FixedClock::new(1_000_000));
    let db = Arc::new(Mutex::new(Db::open_in_memory().unwrap()));
    db.lock()
        .unwrap()
        .set_nonce_last(1_000_000 + VENUE_MAX_FUTURE_MS + 1)
        .unwrap();
    let core = WriteCore::new(Mode::Live, Some(signer()))
        .unwrap()
        .with_clock(clock)
        .with_nonce_db(db)
        .unwrap();
    match core.heal_nonce().await {
        Err(Error::NotSent(message)) => assert!(message.contains("corrupt"), "{message}"),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn reset_lowers_the_durable_and_requested_marks_together() {
    let clock = Arc::new(FixedClock::new(1_000_000));
    let db = Arc::new(Mutex::new(Db::open_in_memory().unwrap()));
    let corrupt = 1_000_000 + VENUE_MAX_FUTURE_MS + 1;
    db.lock().unwrap().set_nonce_last(corrupt).unwrap();
    let core = WriteCore::new(Mode::Live, Some(signer()))
        .unwrap()
        .with_clock(clock)
        .with_nonce_lease(100)
        .with_nonce_db(db.clone())
        .unwrap();
    // At boot the in-memory marks are the corrupt value and nothing was
    // sent, so the reset is allowed.
    assert_eq!(core.nonce_marks(), Some((corrupt, corrupt)));
    core.reset_nonce().await.unwrap();
    // The database, the durable atomic, and the requested horizon all move
    // together to the new lease: no stale higher request survives.
    assert_eq!(core.nonce_marks(), Some((1_000_100, 1_000_100)));
    assert_eq!(db.lock().unwrap().nonce_last().unwrap(), Some(1_000_100));
    assert!(matches!(
        core.prepare(&simple_action()).await.unwrap(),
        Prepared::Send(_)
    ));
}

#[tokio::test]
async fn crash_loop_never_reuses_a_nonce() {
    use std::sync::atomic::AtomicU64;

    // Deterministic and fast: a test channel stands in for the writer, the
    // clock advances a lease per boot, and the write-behind is never
    // committed (the worst case: every write-behind write is lost in the
    // crash). The lease is small so the test does a handful of signs, not
    // thousands.
    let lease_ms = 4u64;
    let mut restored = 1_000_000u64;
    let mut sent: Vec<u64> = Vec::new();
    for run in 0..100u64 {
        let now = 1_000_000 + run * lease_ms;
        let clock = Arc::new(FixedClock::new(now));
        // The boot prime `max(restored, now + lease)` is written
        // synchronously and is what the next boot restores.
        let horizon = restored.max(now + lease_ms);
        let durable = Arc::new(AtomicU64::new(0));
        let (tx, _rx) = std::sync::mpsc::sync_channel(64);
        let core = WriteCore::new(Mode::Live, Some(signer()))
            .unwrap()
            .with_clock(clock.clone())
            .with_nonce_lease(lease_ms)
            .with_test_nonce_store(
                NonceLease::with_channel(tx, horizon, lease_ms, durable, now),
                restored,
            );
        // Enough attempts to spend the boot headroom and hit the
        // write-behind/refusal path at least once.
        for _ in 0..(lease_ms + 2) {
            match core.prepare(&simple_action()).await {
                Ok(Prepared::Send(request)) => {
                    assert!(
                        sent.iter().all(|&previous| request.nonce > previous),
                        "nonce {} reused",
                        request.nonce
                    );
                    sent.push(request.nonce);
                }
                Ok(Prepared::DryRun(_)) => panic!("live mode must send"),
                Err(Error::NotSent(_)) => {}
                Err(other) => panic!("unexpected error: {other:?}"),
            }
        }
        // Crash: the next boot restores exactly the prime written above.
        restored = horizon;
    }
    assert!(!sent.is_empty(), "each boot must send from its prime");
}

#[tokio::test]
async fn restart_after_a_committed_refresh_never_reuses() {
    use std::sync::atomic::{AtomicU64, Ordering};

    let clock = Arc::new(FixedClock::new(1_000_000));
    let lease_ms = 100u64;
    let restored = 1_000_000u64;
    let horizon = restored.max(1_000_000 + lease_ms);
    let durable = Arc::new(AtomicU64::new(0));
    let (tx, rx) = std::sync::mpsc::sync_channel(64);
    let mut sent = Vec::new();
    {
        let core = WriteCore::new(Mode::Live, Some(signer()))
            .unwrap()
            .with_clock(clock.clone())
            .with_nonce_lease(lease_ms)
            .with_test_nonce_store(
                NonceLease::with_channel(tx, horizon, lease_ms, durable.clone(), 1_000_000),
                restored,
            );
        for _ in 0..160 {
            match core.prepare(&simple_action()).await {
                Ok(Prepared::Send(request)) => sent.push(request.nonce),
                Ok(Prepared::DryRun(_)) => panic!("live mode must send"),
                Err(Error::NotSent(_)) => {}
                Err(other) => panic!("unexpected error: {other:?}"),
            }
        }
    }
    // Explicitly commit the highest queued refresh, as the writer thread
    // does after a successful SQLite write.
    let mut committed = horizon;
    while let Ok(value) = rx.try_recv() {
        committed = committed.max(value);
    }
    assert!(committed > horizon, "a refresh must have been enqueued");
    durable.fetch_max(committed, Ordering::AcqRel);
    let max_sent = *sent.iter().max().unwrap();
    assert!(
        committed > max_sent,
        "the committed horizon {committed} must cover every sent nonce, max {max_sent}"
    );

    // Restart in the same frozen millisecond, restoring the committed mark.
    let durable2 = Arc::new(AtomicU64::new(0));
    let (tx2, rx2) = std::sync::mpsc::sync_channel(64);
    let restarted = WriteCore::new(Mode::Live, Some(signer()))
        .unwrap()
        .with_clock(clock)
        .with_nonce_lease(lease_ms)
        .with_test_nonce_store(
            NonceLease::with_channel(
                tx2,
                committed.max(1_000_000 + lease_ms),
                lease_ms,
                durable2.clone(),
                1_000_000,
            ),
            committed,
        );
    let mut next = Vec::new();
    for _ in 0..160 {
        match restarted.prepare(&simple_action()).await {
            Ok(Prepared::Send(request)) => next.push(request.nonce),
            Ok(Prepared::DryRun(_)) => panic!("live mode must send"),
            Err(Error::NotSent(_)) => {}
            Err(other) => panic!("unexpected error: {other:?}"),
        }
        // Explicitly commit any queued refresh (the writer thread's job).
        while let Ok(value) = rx2.try_recv() {
            durable2.fetch_max(value, Ordering::AcqRel);
        }
    }
    assert!(
        !next.is_empty(),
        "the restarted instance must resume sending"
    );
    for nonce in next {
        assert!(
            nonce > max_sent,
            "restart nonce {nonce} reused <= {max_sent}"
        );
    }
}

#[tokio::test]
async fn trait_wrappers_build_the_right_actions() {
    use crate::order::{CancelWire, OrderWire};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/exchange"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"status":"ok","response":{"data":{"statuses":["resting"]}}}"#),
        )
        .mount(&server)
        .await;

    let exchange = HttpExchange::with_base_url(server.uri(), Mode::Live, Some(signer())).unwrap();
    let orders = vec![OrderWire {
        a: 0,
        b: true,
        p: "50000".into(),
        s: "0.1".into(),
        r: false,
        t: crate::order::OrderType::limit(Tif::Gtc),
        c: None,
    }];
    assert_eq!(
        exchange.place(orders).await.unwrap().statuses,
        vec![OrderStatus::Resting]
    );
    exchange
        .cancel(vec![CancelWire { a: 0, o: 7 }])
        .await
        .unwrap();
    exchange
        .schedule_cancel(Some(1_700_000_000_000))
        .await
        .unwrap();
    exchange.schedule_cancel(None).await.unwrap();
}

#[test]
fn simulate_and_live_require_a_signer() {
    assert!(HttpExchange::with_base_url("http://x", Mode::Simulate, None).is_err());
    assert!(HttpExchange::with_base_url("http://x", Mode::Live, None).is_err());
    assert!(HttpExchange::with_base_url("http://x", Mode::Observe, None).is_ok());
}

#[tokio::test]
async fn prepare_records_the_sign_histogram() {
    let recorder = DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();
    // Construct inside the local recorder so the cached handle binds to it.
    let exchange = metrics::with_local_recorder(&recorder, || {
        HttpExchange::with_base_url("http://127.0.0.1:1", Mode::Simulate, Some(signer())).unwrap()
    });
    // `simulate` signs (the `prepare` stage) but never posts.
    exchange.submit(&simple_action()).await.unwrap();
    assert_eq!(
        histogram_samples(
            snapshotter.snapshot(),
            hl_arb_metrics::names::SIGN_SECONDS,
            None,
        ),
        1,
    );
}

#[tokio::test]
async fn live_post_records_the_rest_submit_ack_histogram() {
    let server = MockServer::start().await;
    let body = r#"{"status":"ok","response":{"data":{"statuses":["resting"]}}}"#;
    Mock::given(method("POST"))
        .and(path("/exchange"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&server)
        .await;

    let recorder = DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();
    // Construct inside the local recorder so the cached handle binds to it.
    let exchange = metrics::with_local_recorder(&recorder, || {
        HttpExchange::with_base_url(server.uri(), Mode::Live, Some(signer())).unwrap()
    });
    exchange.submit(&simple_action()).await.unwrap();
    assert_eq!(
        histogram_samples(
            snapshotter.snapshot(),
            hl_arb_metrics::names::SUBMIT_ACK_SECONDS,
            Some(("transport", "rest")),
        ),
        1,
    );
}
