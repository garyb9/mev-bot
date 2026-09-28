//! Live I/O tasks for `hl` (extracted from `main.rs`, task D).
//!
//! The exec writer (signed posts + reply routing + `Unknown` recovery), the
//! H-3 account stream, and the kill-switch control task live here. The engine
//! thread owns the trading state; these tasks are the async edges that feed it
//! and drain its outbound channel (SPEC-0010 §5, §12, §15/§16).

use std::{path::Path, time::Instant};

use anyhow::{Context as _, Result};
use futures_util::{StreamExt, stream::FuturesUnordered};
use mev_core::config::Network;
use mev_engine::ingest::Ingest;
use mev_engine::{
    AccountUpdate, Cloid, CoinRegistry, Control, OrderAck, PostResult, Stamp, VenueOrderStatus,
    channels::InputHandles, exec::UnsignedPost,
};
use mev_hl_client::{
    ExchangeApi, HlProtocol, InfoApi, OrderStatus, RawEvent, RawWsConn, ReplyHandle, Subscription,
};
use smallvec::SmallVec;
use tracing::{error, info};

use crate::engine;

/// Bridge the engine's synchronous exec channel to the async WS writer.
///
/// The engine hands [`UnsignedPost`]s to the crossbeam `Receiver` off the engine
/// thread; a small std thread moves them onto a tokio channel. The writer loop
/// **signs and enqueues** each post in receive order (cancels before places, per
/// the builder) and pushes only the reply wait into a [`FuturesUnordered`], so
/// the loop keeps taking posts while earlier replies are in flight: a post
/// arriving during a slow reply no longer waits a round trip, and a lost reply
/// cannot stall later posts or the kill switch (SPEC-0002 H-1, SPEC-0010 §12).
pub(crate) fn spawn_exec_writer(
    posts: crossbeam_channel::Receiver<UnsignedPost>,
    exchange: std::sync::Arc<dyn ExchangeApi>,
    info: std::sync::Arc<dyn InfoApi>,
    address: Option<String>,
    handles: InputHandles,
) -> tokio::task::JoinHandle<()> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<UnsignedPost>();
    let _ = std::thread::Builder::new()
        .name("mev-exec-bridge".to_string())
        .spawn(move || {
            while let Ok(post) = posts.recv() {
                if tx.send(post).is_err() {
                    break;
                }
            }
        });

    // Pre-created once, then moved into the writer task. Cached handles avoid a
    // per-post key/label lookup; the Prometheus recorder may still buffer or
    // lock internally, so this is not a lock-free guarantee.
    let exec_queue = metrics::histogram!(mev_metrics::names::EXEC_QUEUE_SECONDS);
    tokio::spawn(async move {
        let mut inflight = FuturesUnordered::new();
        loop {
            tokio::select! {
                maybe = rx.recv() => {
                    let Some(post) = maybe else { break };
                    let queued = Instant::now();
                    let recv_mono_ns = post.recv_mono_ns;
                    // Sign and enqueue now, in order; only the reply is awaited
                    // later. No lock is held across this await. The transport
                    // stamps the end-to-end tick-to-order span from
                    // `recv_mono_ns` (SPEC-0002 H-7).
                    match exchange.enqueue_timed(&post.action, recv_mono_ns).await {
                        Ok(handle) => {
                            exec_queue.record(queued.elapsed().as_secs_f64());
                            inflight.push(finish_post(
                                handle,
                                info.clone(),
                                address.clone(),
                                handles.clone(),
                                post,
                            ));
                        }
                        Err(err) => {
                            // A pre-send failure: the orders are terminal.
                            let result = mev_engine::exec::post_result_from_error(&err);
                            let _ = handles.send_account(AccountUpdate::PostAck {
                                stamp: engine::now_stamp(),
                                req_id: post.req_id,
                                result,
                            });
                        }
                    }
                }
                Some(_) = inflight.next(), if !inflight.is_empty() => {}
            }
        }
        // Drain any replies still in flight before the task exits.
        while inflight.next().await.is_some() {}
    })
}

/// Await one enqueued post's reply and report it to the engine.
///
/// A successful reply becomes a [`PostResult::Statuses`]. A definitive failure
/// becomes [`PostResult::Rejected`] (terminal); a sent request with no reliable
/// answer becomes [`PostResult::Error`] (`Unknown`), and its `orderStatus`
/// reconciliation is spawned so it cannot delay the writer (SPEC-0002 H-2,
/// SPEC-0010 §10/§16). Never resends.
async fn finish_post(
    handle: ReplyHandle,
    info: std::sync::Arc<dyn InfoApi>,
    address: Option<String>,
    handles: InputHandles,
    post: UnsignedPost,
) {
    let req_id = post.req_id;
    let (result, unknown) = match handle.wait().await {
        Ok(response) => {
            let result = match response.order_response() {
                Ok(orders) => PostResult::Statuses(
                    orders
                        .statuses
                        .iter()
                        .zip(orders.oids.iter())
                        .map(|(status, oid)| OrderAck {
                            status: map_venue_status(status),
                            oid: *oid,
                        })
                        .collect(),
                ),
                // Non-order actions (cancels) carry no per-order statuses; their
                // outcome arrives on the account stream.
                Err(_) => PostResult::Statuses(SmallVec::new()),
            };
            (result, false)
        }
        Err(err) => {
            let result = mev_engine::exec::post_result_from_error(&err);
            let unknown = matches!(result, PostResult::Error(_));
            (result, unknown)
        }
    };
    let update = AccountUpdate::PostAck {
        stamp: engine::now_stamp(),
        req_id,
        result,
    };
    if !handles.send_account(update) {
        return;
    }
    // A lost reply is reconciled by cloid off the writer's critical path: the
    // dispatcher has already marked the orders `Unknown` (the PostAck is ahead
    // of these on the same FIFO account channel).
    if unknown
        && let Some(address) = address
        && !post.cloids.is_empty()
    {
        tokio::spawn(recover_unknown(
            info,
            address,
            handles,
            post.cloids,
            RecoveryPolicy::default(),
        ));
    }
}

/// Bounded `orderStatus` retry policy for `Unknown` orders (SPEC-0002 H-2).
#[derive(Debug, Clone, Copy)]
pub(crate) struct RecoveryPolicy {
    /// Number of `orderStatus` queries per cloid before giving up.
    attempts: usize,
    /// First backoff delay; doubles per attempt, capped.
    base_delay: std::time::Duration,
}

impl Default for RecoveryPolicy {
    fn default() -> Self {
        Self {
            attempts: 5,
            base_delay: std::time::Duration::from_millis(250),
        }
    }
}

/// Resolve `Unknown` orders by `cloid` through `orderStatus`, with capped
/// backoff.
///
/// A found order is applied only while the engine still holds it `Unknown`
/// (through [`AccountUpdate::ResolveUnknown`]); after the retry bound an order
/// the venue never saw is resolved as `Rejected`
/// ([`AccountUpdate::UnknownExpired`]). Never resends an order.
async fn recover_unknown(
    info: std::sync::Arc<dyn InfoApi>,
    address: String,
    handles: InputHandles,
    cloids: SmallVec<[Cloid; 8]>,
    policy: RecoveryPolicy,
) {
    for cloid in cloids {
        let mut resolved = false;
        for attempt in 0..policy.attempts {
            if let Ok(status) = info.order_status_by_cloid(&address, &cloid.to_hex()).await
                && status.is_found()
            {
                resolved = handles.send_account(AccountUpdate::ResolveUnknown {
                    stamp: engine::now_stamp(),
                    cloid,
                    status,
                });
                break;
            }
            if attempt + 1 < policy.attempts {
                let shift = attempt.min(3) as u32;
                tokio::time::sleep(policy.base_delay * (1u32 << shift)).await;
            }
        }
        if !resolved
            && !handles.send_account(AccountUpdate::UnknownExpired {
                stamp: engine::now_stamp(),
                cloid,
            })
        {
            return;
        }
    }
}

/// Map a wire per-order status to the engine's typed status.
fn map_venue_status(status: &OrderStatus) -> VenueOrderStatus {
    match status {
        OrderStatus::Resting => VenueOrderStatus::Resting,
        OrderStatus::Filled => VenueOrderStatus::Filled,
        OrderStatus::Rejected(_) => VenueOrderStatus::Rejected,
        OrderStatus::Other(_) => VenueOrderStatus::Other,
    }
}

/// Consume the H-3 account channels into lossless [`AccountUpdate`]s.
///
/// Runs on its own connection, separate from market data, and is never lossy:
/// the engine resolves live cancels, fills, and order state from this stream
/// (SPEC-0010 §15, SPEC-0002 H-3). On a reconnect the venue's `userFills`
/// snapshot resyncs state; the REST reconciler is the 30 s backstop.
pub(crate) async fn account_stream(
    handles: InputHandles,
    subscriptions: Vec<Subscription>,
    registry: CoinRegistry,
    network: Network,
) {
    let ingester = Ingest::new(mev_engine::types::ConnId(1), registry);
    let planned: Vec<String> = subscriptions
        .iter()
        .map(|sub| serde_json::to_string(sub).unwrap_or_default())
        .collect();
    let mut backoff = std::time::Duration::from_secs(1);

    loop {
        match RawWsConn::connect(Box::new(HlProtocol::new(network)), planned.clone()).await {
            Ok(conn) => {
                info!("account stream connected");
                if account_stream_conn(conn, ingester.clone(), handles.clone()).await {
                    return;
                }
            }
            Err(err) => {
                tracing::warn!(error = %err, "account stream connect failed");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(std::time::Duration::from_secs(30));
            }
        }
    }
}

/// Consume one account connection. Returns `true` on a shutdown gap.
pub(crate) async fn account_stream_conn(
    mut conn: RawWsConn,
    ingester: Ingest,
    handles: InputHandles,
) -> bool {
    loop {
        match conn.next().await {
            Ok(RawEvent::Text {
                t_ns,
                mono_ns,
                text,
            }) => {
                let stamp = Stamp {
                    t_recv_ns: t_ns.saturating_mul(1_000_000),
                    mono_ns,
                    ts_exch_ms: 0,
                };
                match ingester.decode_account(&text, stamp) {
                    Ok(updates) => {
                        for update in updates {
                            if !handles.send_account(update) {
                                return true;
                            }
                        }
                    }
                    Err(err) => {
                        tracing::debug!(error = %err, "undecodable account frame");
                    }
                }
            }
            Ok(RawEvent::Gap { reason, detail }) => {
                tracing::warn!(reason, detail, "account feed gap");
                if reason == "shutdown" {
                    return true;
                }
            }
            Ok(_) => {}
            Err(err) => {
                tracing::warn!(error = %err, "account stream ended");
                return false;
            }
        }
    }
}

/// What a `SIGUSR2` should do given the kill flag file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResumeAction {
    /// Send `Control::Resume`.
    Resume,
    /// Ignore the signal because the flag file still exists.
    Ignore,
}

/// Decide a `SIGUSR2` (SPEC-0004 K-3).
///
/// Never resume while the kill flag file exists: the next 250 ms poll would
/// re-trip the kill, and orders could go out in between. Otherwise resume
/// whatever the local state says (a kill sent by the dead-man task, which the
/// control task did not itself set, must still be resumable).
fn resume_action(flag_file_exists: bool) -> ResumeAction {
    if flag_file_exists {
        ResumeAction::Ignore
    } else {
        ResumeAction::Resume
    }
}

/// Send an initial `KillSwitch` if the flag file exists at startup, so the
/// engine begins halted (SPEC-0004 K-3). Returns whether it did.
pub(crate) fn initial_kill(handles: &InputHandles, kill_file: &Path) -> bool {
    if mev_risk::kill::check_flag_file(kill_file) {
        info!(path = %kill_file.display(), "kill flag present at startup; starting killed");
        let _ = handles.send_account(AccountUpdate::Control(Control::KillSwitch));
        true
    } else {
        false
    }
}

/// Poll the kill-switch triggers and drive [`Control`] into the engine.
///
/// Triggers (SPEC-0004 K-3): `SIGUSR1`, the kill flag file, and `hl panic`
/// (which writes that file). `SIGUSR2` sends `Control::Resume` unless the flag
/// file still exists (the two-key rule); a kill from any source is resumable.
#[cfg(unix)]
pub(crate) async fn control(handles: InputHandles, kill_file: std::path::PathBuf) {
    use tokio::signal::unix::{SignalKind, signal};
    let mut sigusr1 = match signal(SignalKind::user_defined1()) {
        Ok(sig) => sig,
        Err(err) => {
            error!(error = %err, "failed to register SIGUSR1");
            return;
        }
    };
    let mut sigusr2 = match signal(SignalKind::user_defined2()) {
        Ok(sig) => sig,
        Err(err) => {
            error!(error = %err, "failed to register SIGUSR2");
            return;
        }
    };
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));
    let mut killed = false;
    loop {
        tokio::select! {
            _ = crate::shutdown_signal() => break,
            _ = sigusr1.recv() => {
                if !killed {
                    if !handles.send_account(AccountUpdate::Control(Control::KillSwitch)) {
                        break;
                    }
                    killed = true;
                }
            }
            _ = sigusr2.recv() => {
                match resume_action(mev_risk::kill::check_flag_file(&kill_file)) {
                    ResumeAction::Ignore => {
                        tracing::warn!("SIGUSR2 ignored: the kill flag file still exists");
                    }
                    ResumeAction::Resume => {
                        if !handles.send_account(AccountUpdate::Control(Control::Resume)) {
                            break;
                        }
                        killed = false;
                    }
                }
            }
            _ = tick.tick() => {
                if !killed && mev_risk::kill::check_flag_file(&kill_file) {
                    if !handles.send_account(AccountUpdate::Control(Control::KillSwitch)) {
                        break;
                    }
                    killed = true;
                }
            }
        }
    }
}

/// Poll the kill-switch flag file on platforms without `SIGUSR1`/`SIGUSR2`.
#[cfg(not(unix))]
pub(crate) async fn control(handles: InputHandles, kill_file: std::path::PathBuf) {
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));
    let mut killed = false;
    loop {
        tokio::select! {
            _ = crate::shutdown_signal() => break,
            _ = tick.tick() => {
                if !killed && mev_risk::kill::check_flag_file(&kill_file) {
                    if !handles.send_account(AccountUpdate::Control(Control::KillSwitch)) {
                        break;
                    }
                    killed = true;
                }
            }
        }
    }
}

/// Write (`trip`) or remove (`clear`) the kill-switch flag file (SPEC-0004 K-3).
pub(crate) fn set_kill_switch(path: &Path, trip: bool) -> Result<()> {
    if trip {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(path, b"kill\n").with_context(|| format!("writing {}", path.display()))?;
        println!("kill switch tripped: {}", path.display());
    } else if path.exists() {
        std::fs::remove_file(path).with_context(|| format!("removing {}", path.display()))?;
        println!("kill switch flag removed: {}", path.display());
    } else {
        println!("kill switch flag already absent: {}", path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use mev_engine::channels::{ACCOUNT_CHANNEL_CAP, MARKET_CHANNEL_CAP, inputs};
    use mev_engine::exec::UnsignedPost;
    use mev_hl_client::{Action, ActionResponse, ExchangeApi, Grouping, HttpInfo, ReplyHandle};
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;

    use super::*;

    /// One scripted reply for [`MockExchange`].
    #[derive(Clone, Copy)]
    struct Outcome {
        delay: Duration,
        fail: bool,
    }

    /// An [`ExchangeApi`] whose `enqueue` records order and returns a scripted
    /// reply handle, so the writer's split enqueue/await can be tested.
    struct MockExchange {
        enqueued: Arc<Mutex<Vec<Action>>>,
        outcomes: Arc<Mutex<VecDeque<Outcome>>>,
        recv_hints: Arc<Mutex<Vec<u64>>>,
    }

    #[async_trait::async_trait]
    impl ExchangeApi for MockExchange {
        async fn submit(&self, action: &Action) -> mev_core::error::Result<ActionResponse> {
            self.enqueue(action).await?.wait().await
        }

        async fn enqueue(&self, action: &Action) -> mev_core::error::Result<ReplyHandle> {
            self.enqueued.lock().unwrap().push(action.clone());
            let outcome = self
                .outcomes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Outcome {
                    delay: Duration::ZERO,
                    fail: false,
                });
            Ok(ReplyHandle::new(async move {
                tokio::time::sleep(outcome.delay).await;
                if outcome.fail {
                    Err(mev_core::error::Error::UnknownOutcome("lost reply".into()))
                } else {
                    Ok(ActionResponse {
                        value: serde_json::json!({"data": {"statuses": ["resting"]}}),
                    })
                }
            }))
        }

        async fn enqueue_timed(
            &self,
            action: &Action,
            recv_mono_ns: u64,
        ) -> mev_core::error::Result<ReplyHandle> {
            self.recv_hints.lock().unwrap().push(recv_mono_ns);
            self.enqueue(action).await
        }
    }

    fn mock_exchange(outcomes: Vec<Outcome>) -> Arc<MockExchange> {
        Arc::new(MockExchange {
            enqueued: Arc::new(Mutex::new(Vec::new())),
            outcomes: Arc::new(Mutex::new(outcomes.into_iter().collect())),
            recv_hints: Arc::new(Mutex::new(Vec::new())),
        })
    }

    fn test_post(req_id: u64, action: Action) -> UnsignedPost {
        UnsignedPost {
            req_id,
            action,
            cloids: SmallVec::new(),
            recv_mono_ns: 0,
        }
    }

    fn cloid_with(byte: u8) -> Cloid {
        Cloid([byte; 16])
    }

    /// A never-dialing info client; recovery calls just fail and retry.
    fn dead_info() -> Arc<dyn InfoApi> {
        Arc::new(HttpInfo::with_base_url("http://127.0.0.1:1"))
    }

    /// Poll the synchronous account channel for up to `timeout`.
    async fn recv_account(
        inputs: &mev_engine::channels::Inputs,
        timeout: Duration,
    ) -> Option<AccountUpdate> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(update) = inputs.account.try_recv() {
                return Some(update);
            }
            if Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    #[tokio::test]
    async fn a_post_during_a_slow_reply_is_enqueued_immediately() {
        let exchange = mock_exchange(vec![
            Outcome {
                delay: Duration::from_secs(5),
                fail: false,
            },
            Outcome {
                delay: Duration::ZERO,
                fail: false,
            },
        ]);
        let (handles, _inputs) = inputs(MARKET_CHANNEL_CAP, ACCOUNT_CHANNEL_CAP);
        let (tx, rx) = crossbeam_channel::unbounded();
        let writer = spawn_exec_writer(rx, exchange.clone(), dead_info(), None, handles);

        tx.send(test_post(1, Action::CancelByCloid { cancels: vec![] }))
            .unwrap();
        let started = Instant::now();
        tx.send(test_post(
            2,
            Action::Order {
                orders: vec![],
                grouping: Grouping::Na,
            },
        ))
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        while exchange.enqueued.lock().unwrap().len() < 2 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let queue_to_socket = started.elapsed();
        assert_eq!(
            exchange.enqueued.lock().unwrap().len(),
            2,
            "the second post must reach the socket while the first reply is pending"
        );
        assert!(
            queue_to_socket < Duration::from_secs(1),
            "queue-to-socket took {queue_to_socket:?}"
        );

        drop(tx);
        writer.abort();
    }

    #[tokio::test]
    async fn a_cancel_is_enqueued_before_a_place_in_a_batch() {
        let exchange = mock_exchange(vec![]);
        let (handles, _inputs) = inputs(MARKET_CHANNEL_CAP, ACCOUNT_CHANNEL_CAP);
        let (tx, rx) = crossbeam_channel::unbounded();
        let writer = spawn_exec_writer(rx, exchange.clone(), dead_info(), None, handles);

        tx.send(test_post(1, Action::CancelByCloid { cancels: vec![] }))
            .unwrap();
        tx.send(test_post(
            2,
            Action::Order {
                orders: vec![],
                grouping: Grouping::Na,
            },
        ))
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        while exchange.enqueued.lock().unwrap().len() < 2 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let enqueued = exchange.enqueued.lock().unwrap();
        assert_eq!(enqueued.len(), 2);
        assert!(
            matches!(enqueued[0], Action::CancelByCloid { .. }),
            "the cancel goes first"
        );
        assert!(
            matches!(enqueued[1], Action::Order { .. }),
            "the place goes second"
        );

        drop(tx);
        writer.abort();
    }

    #[tokio::test]
    async fn a_lost_reply_and_its_recovery_do_not_delay_the_next_post() {
        let exchange = mock_exchange(vec![
            Outcome {
                delay: Duration::from_millis(200),
                fail: true,
            },
            Outcome {
                delay: Duration::ZERO,
                fail: false,
            },
        ]);
        let (handles, _inputs) = inputs(MARKET_CHANNEL_CAP, ACCOUNT_CHANNEL_CAP);
        let (tx, rx) = crossbeam_channel::unbounded();
        let writer = spawn_exec_writer(
            rx,
            exchange.clone(),
            dead_info(),
            Some("0xabc".into()),
            handles,
        );

        let mut first = test_post(1, Action::CancelByCloid { cancels: vec![] });
        first.cloids.push(cloid_with(7));
        tx.send(first).unwrap();
        tx.send(test_post(
            2,
            Action::Order {
                orders: vec![],
                grouping: Grouping::Na,
            },
        ))
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        while exchange.enqueued.lock().unwrap().len() < 2 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert_eq!(
            exchange.enqueued.lock().unwrap().len(),
            2,
            "recovery must not delay the next post"
        );

        drop(tx);
        writer.abort();
    }

    /// A tiny policy so the recovery tests don't sleep for seconds.
    fn fast_policy() -> RecoveryPolicy {
        RecoveryPolicy {
            attempts: 2,
            base_delay: Duration::from_millis(1),
        }
    }

    fn one(cloid: Cloid) -> SmallVec<[Cloid; 8]> {
        let mut out = SmallVec::new();
        out.push(cloid);
        out
    }

    #[tokio::test]
    async fn recovery_bounds_a_never_seen_order_to_expired() {
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/info"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"status":"unknownOid"}"#))
            .mount(&server)
            .await;

        let info: Arc<dyn InfoApi> = Arc::new(HttpInfo::with_base_url(server.uri()));
        let (handles, inputs) = inputs(MARKET_CHANNEL_CAP, ACCOUNT_CHANNEL_CAP);
        let cloid = cloid_with(1);
        recover_unknown(info, "0xabc".into(), handles, one(cloid), fast_policy()).await;

        match inputs.account.try_recv() {
            Ok(AccountUpdate::UnknownExpired { cloid: got, .. }) => assert_eq!(got, cloid),
            other => panic!("expected UnknownExpired, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn recovery_applies_a_found_order_status() {
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };

        let body = r#"{"status":"order","order":{"order":{"coin":"BTC","side":"B","limitPx":"100","sz":"1","oid":7,"timestamp":0,"origSz":"1","reduceOnly":false},"status":"open","statusTimestamp":0}}"#;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/info"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        let info: Arc<dyn InfoApi> = Arc::new(HttpInfo::with_base_url(server.uri()));
        let (handles, inputs) = inputs(MARKET_CHANNEL_CAP, ACCOUNT_CHANNEL_CAP);
        let cloid = cloid_with(2);
        recover_unknown(info, "0xabc".into(), handles, one(cloid), fast_policy()).await;

        match inputs.account.try_recv() {
            Ok(AccountUpdate::ResolveUnknown { cloid: got, .. }) => assert_eq!(got, cloid),
            other => panic!("expected ResolveUnknown, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn creating_the_flag_file_trips_the_kill_switch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kill.flag");
        let (handles, inputs) = inputs(MARKET_CHANNEL_CAP, ACCOUNT_CHANNEL_CAP);
        let task = tokio::spawn(control(handles, path.clone()));

        std::fs::write(&path, b"kill\n").unwrap();
        let got = recv_account(&inputs, Duration::from_secs(2)).await;
        task.abort();

        match got {
            Some(AccountUpdate::Control(Control::KillSwitch)) => {}
            other => panic!("expected Control::KillSwitch, got {other:?}"),
        }
    }

    #[test]
    fn sigusr2_resumes_only_without_the_flag_file() {
        assert_eq!(resume_action(true), ResumeAction::Ignore);
        assert_eq!(resume_action(false), ResumeAction::Resume);
    }

    #[test]
    fn startup_flag_file_sends_an_initial_kill() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kill.flag");
        let (handles, inputs) = inputs(MARKET_CHANNEL_CAP, ACCOUNT_CHANNEL_CAP);

        // Present at startup: send KillSwitch before the loop runs.
        std::fs::write(&path, b"kill\n").unwrap();
        assert!(initial_kill(&handles, &path));
        assert!(matches!(
            inputs.account.try_recv(),
            Ok(AccountUpdate::Control(Control::KillSwitch))
        ));

        // Absent: nothing is sent.
        let _ = std::fs::remove_file(&path);
        assert!(!initial_kill(&handles, &path));
        assert!(inputs.account.try_recv().is_err());
    }

    #[test]
    fn panic_and_resume_create_and_remove_the_flag_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kill.flag");

        set_kill_switch(&path, true).unwrap();
        assert!(path.exists(), "hl panic writes the flag file");
        set_kill_switch(&path, false).unwrap();
        assert!(!path.exists(), "hl resume removes the flag file");
    }

    /// A protocol that points at a test URL.
    #[derive(Clone)]
    struct TestProtocol {
        url: String,
    }

    impl mev_hl_client::Protocol for TestProtocol {
        fn name(&self) -> &'static str {
            "test"
        }
        fn url(&self) -> String {
            self.url.clone()
        }
        fn subscribe_frame(&self, sub: &str) -> String {
            format!(r#"{{"method":"subscribe","subscription":{sub}}}"#)
        }
    }

    #[tokio::test]
    async fn mock_ws_account_frames_arrive_on_the_account_channel() {
        use futures_util::SinkExt;
        use tokio_tungstenite::tungstenite::Message;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            // Wait for the subscribe frame, then push one streaming fill.
            if let Some(Ok(Message::Text(_))) = ws.next().await {
                let frame = r#"{"channel":"userFills","data":{"isSnapshot":false,"user":"0xabc","fills":[{"coin":"BTC","px":"60000","sz":"0.01","side":"B","time":7,"oid":42,"fee":"0.27","tid":7}]}}"#;
                ws.send(Message::Text(frame.into())).await.unwrap();
            }
            // Hold the socket open so the reader keeps draining.
            std::future::pending::<()>().await;
        });

        let conn = RawWsConn::connect(
            Box::new(TestProtocol {
                url: format!("ws://{addr}"),
            }),
            vec![r#"{"type":"userFills","user":"0xabc"}"#.to_string()],
        )
        .await
        .unwrap();
        let registry = CoinRegistry::from_coins(&["BTC".into()]);
        let ingester = Ingest::new(mev_engine::types::ConnId(1), registry);
        let (handles, inputs) = inputs(MARKET_CHANNEL_CAP, ACCOUNT_CHANNEL_CAP);
        let task = tokio::spawn(account_stream_conn(conn, ingester, handles));

        let got = recv_account(&inputs, Duration::from_secs(5)).await;
        task.abort();
        server.abort();

        match got {
            Some(AccountUpdate::Fill { tid: 7, coin, .. }) => {
                assert_eq!(coin, mev_engine::types::CoinId(0));
            }
            other => panic!("expected a decoded fill, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn exec_writer_forwards_the_recv_time_hint() {
        let exchange = mock_exchange(vec![Outcome {
            delay: Duration::ZERO,
            fail: false,
        }]);
        let (handles, inputs) = inputs(MARKET_CHANNEL_CAP, ACCOUNT_CHANNEL_CAP);
        let (tx, rx) = crossbeam_channel::unbounded();
        let writer = spawn_exec_writer(rx, exchange.clone(), dead_info(), None, handles);

        let mut post = test_post(
            2,
            Action::Order {
                orders: vec![],
                grouping: Grouping::Na,
            },
        );
        // The engine carries the market frame's read time; the transport needs it
        // to stamp the end-to-end tick-to-order span (SPEC-0002 H-7).
        post.recv_mono_ns = 12_345;
        tx.send(post).unwrap();

        let got = recv_account(&inputs, Duration::from_secs(2)).await;
        assert!(
            matches!(got, Some(AccountUpdate::PostAck { .. })),
            "expected a post ack, got {got:?}"
        );
        assert_eq!(
            *exchange.recv_hints.lock().unwrap(),
            vec![12_345],
            "the exec writer must forward the frame's recv time to the transport"
        );

        drop(tx);
        writer.abort();
    }
}
