//! Per-connection health, metrics, and the raw WebSocket task
//! (SPEC-0008 §7.4, §12.2).

use super::clock::chrony_tracking;
use super::*;
use hl_arb_client::raw_ws::ReconnectPolicy;

// ---------------------------------------------------------------------------
// WebSocket connection task
// ---------------------------------------------------------------------------

/// Liveness state for one planned connection, read by the readiness monitor.
#[derive(Debug, Default)]
pub(super) struct ConnState {
    connected: AtomicBool,
    last_ns: AtomicU64,
}

impl ConnState {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub(super) fn set_connected(&self, connected: bool) {
        self.connected.store(connected, Ordering::Relaxed);
    }

    pub(super) fn touch(&self, clock: &dyn EnvelopeClock) {
        self.last_ns.store(clock.mono_ns(), Ordering::Relaxed);
    }
}

/// Process-wide readiness input.
pub(super) struct RecorderHealth {
    /// Gating connections: `/readyz` is not ready if any of these is down.
    conns: Vec<(String, Arc<ConnState>)>,
    /// Non-gating reference streams (R-8 CEX). A dead one is reported (a
    /// rate-limited WARN and the `hl_ws_connected{src}` gauge) but does not
    /// turn `/readyz` red, so a flaky reference feed cannot take the recorder
    /// out of rotation.
    cex: Vec<(String, Arc<ConnState>)>,
    rest_last_ns: AtomicU64,
}

impl RecorderHealth {
    pub(super) fn new(
        conns: Vec<(String, Arc<ConnState>)>,
        cex: Vec<(String, Arc<ConnState>)>,
    ) -> Arc<Self> {
        Arc::new(Self {
            conns,
            cex,
            rest_last_ns: AtomicU64::new(0),
        })
    }

    pub(super) fn touch_rest(&self, mono_ns: u64) {
        self.rest_last_ns.store(mono_ns, Ordering::Relaxed);
    }

    /// Every planned connection is connected and fed within the watchdog, and
    /// the REST stream has produced data recently.
    pub(super) fn ready(&self, now_ns: u64, watchdog_ns: u64, rest_stale_ns: u64) -> bool {
        let rest_last = self.rest_last_ns.load(Ordering::Relaxed);
        let rest_ok = rest_last == 0 || now_ns.saturating_sub(rest_last) <= rest_stale_ns;
        if !rest_ok {
            return false;
        }
        self.conns.iter().all(|(_, state)| {
            if !state.connected.load(Ordering::Relaxed) {
                return false;
            }
            let last = state.last_ns.load(Ordering::Relaxed);
            last != 0 && now_ns.saturating_sub(last) <= watchdog_ns
        })
    }

    /// The first CEX stream that has not produced data within the watchdog
    /// (or never has), for reporting. Non-gating: never affects `ready`.
    pub(super) fn cex_down(&self, now_ns: u64, watchdog_ns: u64) -> Option<&str> {
        self.cex.iter().find_map(|(name, state)| {
            let last = state.last_ns.load(Ordering::Relaxed);
            if last != 0 && now_ns.saturating_sub(last) <= watchdog_ns {
                None
            } else {
                Some(name.as_str())
            }
        })
    }
}

/// Per-connection metric handles (fixed labels, no per-event allocation).
pub(super) struct ConnMetrics {
    records: HashMap<&'static str, metrics::Counter>,
    dropped: metrics::Counter,
}

impl ConnMetrics {
    pub(super) fn new(src: &str, conn: &str) -> Self {
        let mut records = HashMap::new();
        for kind in [
            Kind::Frame,
            Kind::FrameBin,
            Kind::Rest,
            Kind::Sub,
            Kind::ConnOpen,
            Kind::GapStart,
            Kind::GapEnd,
            Kind::Clock,
            Kind::SegmentOpen,
            Kind::SegmentClose,
        ] {
            records.insert(
                kind.as_str(),
                metrics::counter!(names::REC_RECORDS_TOTAL, "src" => src.to_string(), "conn" => conn.to_string(), "kind" => kind.as_str()),
            );
        }
        Self {
            records,
            dropped: metrics::counter!(names::REC_DROPPED_TOTAL, "src" => src.to_string(), "conn" => conn.to_string()),
        }
    }

    pub(super) fn record(&self, kind: &'static str) {
        if let Some(counter) = self.records.get(kind) {
            counter.increment(1);
        }
    }
}

/// Send one envelope, update metrics, and mark the connection as alive.
///
/// Returns whether the envelope reached the writer queue.
pub(super) fn emit(
    metrics: &ConnMetrics,
    writer: &SegmentWriter,
    state: &ConnState,
    clock: &dyn EnvelopeClock,
    env: Envelope,
) -> bool {
    let kind = env.kind.as_str();
    if writer.try_send(env) {
        metrics.record(kind);
        state.touch(clock);
        true
    } else {
        metrics.dropped.increment(1);
        false
    }
}

/// Consume one raw WebSocket connection and write its envelopes until shutdown.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_ws_conn(
    conn: Connection,
    protocol: Box<dyn Fn() -> Box<dyn Protocol> + Send>,
    writer: SegmentWriter,
    clock: Arc<dyn EnvelopeClock>,
    state: Arc<ConnState>,
    mut shutdown: watch::Receiver<bool>,
    start_delay: Duration,
    policy: ReconnectPolicy,
) {
    let src = "hl-ws";
    let conn_id = conn.id.clone();
    let url = protocol().url();
    let metrics = ConnMetrics::new(src, conn_id.as_str());
    let mut seq: u64 = 0;

    // Pace new connections (§7.4 step 7).
    if !start_delay.is_zero() {
        tokio::select! {
            _ = tokio::time::sleep(start_delay) => {}
            _ = shutdown.changed() => {
                shutdown_writer(writer);
                return;
            }
        }
    }

    let subs: Vec<String> = conn
        .subs
        .iter()
        .map(|sub| sub.to_json().to_string())
        .collect();

    // Retry the initial dial; `RawWsConn` handles reconnects after that.
    //
    // A gap is stamped at the wall-clock/monotonic instant it began (the raw
    // `Gap` carries the disconnect instant, `SPEC-0008 RW-2`) so
    // `gap_end.t_ns - gap_start.t_ns` measures the real downtime. Only the
    // first failure of an outage emits a `gap_start`.
    let mut gap_started: Option<(i64, u64)> = None;
    let mut gap_reason = String::new();
    let mut raw = loop {
        match RawWsConn::connect_with_policy(
            protocol(),
            subs.clone(),
            hl_arb_client::raw_ws::DEFAULT_WATCHDOG,
            hl_arb_client::raw_ws::DEFAULT_PING_INTERVAL,
            policy,
        )
        .await
        {
            Ok(raw) => break raw,
            Err(err) => {
                warn!(conn = %conn_id, error = %err, "websocket connect failed; retrying");
                if gap_started.is_none() {
                    let t_ns = clock.t_ns();
                    let mono_ns = clock.mono_ns();
                    seq = emit_gap_start(
                        &metrics,
                        &writer,
                        &state,
                        &*clock,
                        &conn_id,
                        t_ns,
                        mono_ns,
                        "error",
                        &err.to_string(),
                        seq,
                    );
                    gap_started = Some((t_ns, mono_ns));
                }
                gap_reason = "error".to_string();
                tokio::select! {
                    _ = tokio::time::sleep(CONNECT_RETRY) => {}
                    _ = shutdown.changed() => {
                        shutdown_writer(writer);
                        return;
                    }
                }
            }
        }
    };
    state.set_connected(true);

    seq = write_conn_open(&metrics, &writer, &state, &*clock, &conn_id, &url, 1, seq);
    for sub in &conn.subs {
        let env = Envelope::sub(&*clock, src, conn_id.as_str(), seq, sub.to_json());
        emit(&metrics, &writer, &state, &*clock, env);
        seq += 1;
    }
    if let Some((started_t_ns, _)) = gap_started.take() {
        let t_ns = clock.t_ns();
        let gap_ms = t_ns.saturating_sub(started_t_ns).max(0) as u64 / 1_000_000;
        let env = Envelope::gap_end_at(src, conn_id.as_str(), seq, t_ns, clock.mono_ns(), gap_ms);
        emit(&metrics, &writer, &state, &*clock, env);
        seq += 1;
        record_gap_seconds(src, &conn_id, &gap_reason, gap_ms);
    }

    let mut clock_tick = tokio::time::interval(Duration::from_secs(60));
    clock_tick.tick().await; // consume the immediate first tick

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                let t_ns = clock.t_ns();
                let mono_ns = clock.mono_ns();
                emit_gap_start(&metrics, &writer, &state, &*clock, &conn_id, t_ns, mono_ns, "shutdown", "shutdown requested", seq);
                break;
            }
            _ = clock_tick.tick() => {
                let (offset_ns, stratum) = chrony_tracking().await;
                if let Some(offset) = offset_ns {
                    metrics::gauge!(names::REC_CLOCK_OFFSET_NS).set(offset as f64);
                }
                let env = Envelope::clock(&*clock, src, conn_id.as_str(), seq, offset_ns, stratum);
                emit(&metrics, &writer, &state, &*clock, env);
                seq += 1;
            }
            event = raw.next() => match event {
                Ok(RawEvent::Text { text, .. }) => {
                    let env = Envelope::frame(&*clock, src, conn_id.as_str(), seq, text);
                    emit(&metrics, &writer, &state, &*clock, env);
                    seq += 1;
                }
                Ok(RawEvent::Binary { bytes, .. }) => {
                    let env = Envelope::frame_bin(&*clock, src, conn_id.as_str(), seq, base64_encode(&bytes));
                    emit(&metrics, &writer, &state, &*clock, env);
                    seq += 1;
                }
                Ok(RawEvent::Opened { attempt }) => {
                    state.set_connected(true);
                    seq = write_conn_open(&metrics, &writer, &state, &*clock, &conn_id, &url, attempt, seq);
                    for sub in &conn.subs {
                        let env = Envelope::sub(&*clock, src, conn_id.as_str(), seq, sub.to_json());
                        emit(&metrics, &writer, &state, &*clock, env);
                        seq += 1;
                    }
                    if let Some((started_t_ns, _)) = gap_started.take() {
                        let t_ns = clock.t_ns();
                        let gap_ms = t_ns.saturating_sub(started_t_ns).max(0) as u64 / 1_000_000;
                        let env = Envelope::gap_end_at(
                            src,
                            conn_id.as_str(),
                            seq,
                            t_ns,
                            clock.mono_ns(),
                            gap_ms,
                        );
                        emit(&metrics, &writer, &state, &*clock, env);
                        seq += 1;
                        record_gap_seconds(src, conn_id.as_str(), &gap_reason, gap_ms);
                    }
                }
                Ok(RawEvent::Gap {
                    reason,
                    detail,
                    disconnect_ns,
                    t_ns,
                }) => {
                    state.set_connected(false);
                    // Exactly one `gap_start` per outage: `RawWsConn` now emits
                    // a single `Gap` for the whole outage (it stays cancel-safe
                    // across clock ticks), so keep the first disconnect instant
                    // rather than overwriting it if another `Gap` ever slipped
                    // through. `gap_end` then covers the whole outage.
                    if gap_started.is_none() {
                        seq = emit_gap_start(
                            &metrics,
                            &writer,
                            &state,
                            &*clock,
                            &conn_id,
                            t_ns,
                            disconnect_ns,
                            &reason,
                            &detail,
                            seq,
                        );
                        gap_reason = reason;
                        gap_started = Some((t_ns, disconnect_ns));
                    }
                }
                Err(err) => {
                    state.set_connected(false);
                    let t_ns = clock.t_ns();
                    let mono_ns = clock.mono_ns();
                    emit_gap_start(&metrics, &writer, &state, &*clock, &conn_id, t_ns, mono_ns, "error", &err.to_string(), seq);
                    break;
                }
            }
        }
    }

    state.set_connected(false);
    shutdown_writer(writer);
}

/// Stop a segment writer, surfacing a bounded-join timeout.
///
/// `SegmentWriter::shutdown` returns [`SegmentError::ShutdownTimeout`] and trips
/// the mount guard when the writer thread did not stop in time, so `run` exits
/// non-zero without waiting for the hung thread (R-14 fix2 §2).
pub(super) fn shutdown_writer(writer: SegmentWriter) {
    if let Err(err) = writer.shutdown() {
        warn!(error = %err, "segment writer shutdown timed out; mount guard tripped");
    }
}

/// Write a `conn_open` envelope and return the next `seq`.
#[allow(clippy::too_many_arguments)]
pub(super) fn write_conn_open(
    metrics: &ConnMetrics,
    writer: &SegmentWriter,
    state: &ConnState,
    clock: &dyn EnvelopeClock,
    conn: &str,
    url: &str,
    attempt: u32,
    seq: u64,
) -> u64 {
    let env = Envelope::conn_open(clock, "hl-ws", conn, seq, url, attempt);
    emit(metrics, writer, state, clock, env);
    seq + 1
}

/// Write a `gap_start` envelope stamped at `t_ns`/`mono_ns` and return the next
/// `seq`.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_gap_start(
    metrics: &ConnMetrics,
    writer: &SegmentWriter,
    state: &ConnState,
    clock: &dyn EnvelopeClock,
    conn: &str,
    t_ns: i64,
    mono_ns: u64,
    reason: &str,
    detail: &str,
    seq: u64,
) -> u64 {
    let env = Envelope::gap_start_at("hl-ws", conn, seq, t_ns, mono_ns, reason, detail);
    emit(metrics, writer, state, clock, env);
    seq + 1
}

/// Record gap time in the §12.2 counter.
pub(super) fn record_gap_seconds(src: &str, conn: &str, reason: &str, gap_ms: u64) {
    metrics::counter!(
        names::REC_GAP_SECONDS_TOTAL,
        "src" => src.to_string(),
        "conn" => conn.to_string(),
        "reason" => reason.to_string(),
    )
    .increment(gap_ms / 1_000);
}

/// Encode bytes with the standard base64 alphabet (SPEC-0008 §5.1).
pub(super) fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((triple >> 18) & 63) as usize] as char);
        out.push(TABLE[((triple >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((triple >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(triple & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}
