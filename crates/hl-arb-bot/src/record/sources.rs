//! REST snapshotter, Deribit, and CEX source tasks (SPEC-0008 §8, §9).

use super::connection::{ConnMetrics, ConnState, RecorderHealth, emit};
use super::*;

// ---------------------------------------------------------------------------
// REST snapshotter task
// ---------------------------------------------------------------------------

/// An [`EnvelopeSink`] that counts records and marks REST liveness.
///
/// `account_hl_rest` gates the Hyperliquid-specific side effects (the §8 weight
/// counter and `/readyz` REST freshness); non-HL sources such as Deribit reuse
/// the sink with it off so their polls do not consume the HL budget or mask an
/// `hl-rest` outage.
pub(super) struct CountingSink {
    pub(super) writer: Arc<SegmentWriter>,
    pub(super) clock: Arc<dyn EnvelopeClock>,
    pub(super) health: Arc<RecorderHealth>,
    pub(super) metrics: ConnMetrics,
    pub(super) state: Arc<ConnState>,
    pub(super) account_hl_rest: bool,
}

impl EnvelopeSink for CountingSink {
    fn send(&self, env: Envelope) -> bool {
        // Mirror the §8 weights so the 300/min budget is observable. R-5 meters
        // its own bucket; this counter is the recorder-side view of it.
        let weight = if self.account_hl_rest {
            env.meta
                .as_ref()
                .and_then(|meta| meta.get("req"))
                .map(request_weight)
                .unwrap_or(0)
        } else {
            0
        };
        let mono_ns = self.clock.mono_ns();
        let sent = emit(&self.metrics, &self.writer, &self.state, &*self.clock, env);
        if sent && self.account_hl_rest {
            if weight > 0 {
                metrics::counter!(names::REST_WEIGHT_USED_TOTAL, "src" => "recorder".to_string())
                    .increment(weight as u64);
            }
            self.health.touch_rest(mono_ns);
        }
        sent
    }
}

/// The §8 weight of a `/info` request, mirroring the snapshotter's schedule.
///
/// Weights follow the V-1 facts (2026-09-28): `candleSnapshot` is
/// `20 + 1 per 60 items returned`, and `fundingHistory` is
/// `20 + 1 per 20 items returned`, pre-charged by its 500-item page maximum
/// (`20 + 25`) because the item count is not known before the response.
pub(super) fn request_weight(request: &Value) -> u32 {
    match request.get("type").and_then(Value::as_str) {
        Some("fundingHistory") => 20 + FUNDING_MAX_PAGE / 20,
        Some("candleSnapshot") => {
            let req = request.get("req");
            let interval = req
                .and_then(|req| req.get("interval"))
                .and_then(Value::as_str)
                .unwrap_or("1m");
            let start = req
                .and_then(|req| req.get("startTime"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let end = req
                .and_then(|req| req.get("endTime"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let step = match interval {
                "1m" => 60_000,
                "5m" => 300_000,
                "1h" => 3_600_000,
                _ => 60_000,
            };
            let candles = end.saturating_sub(start) / step.max(1);
            20 + (candles / 60) as u32
        }
        _ => 20,
    }
}

/// Run the REST snapshotter until `shutdown` is notified, then finalize.
pub(super) async fn run_rest(
    config: SnapshotterConfig,
    writer: Arc<SegmentWriter>,
    clock: Arc<dyn EnvelopeClock>,
    health: Arc<RecorderHealth>,
    shutdown: Arc<Notify>,
) {
    let state = ConnState::new();
    let sink = Arc::new(CountingSink {
        writer: writer.clone(),
        clock: clock.clone(),
        health,
        metrics: ConnMetrics::new("hl-rest", "hl-rest"),
        state,
        account_hl_rest: true,
    });
    match RestSnapshotter::new(config, sink, clock) {
        Ok(snapshotter) => snapshotter.run(shutdown).await,
        Err(err) => warn!(error = %err, "rest snapshotter failed to start"),
    }
    // Dropping the last `Arc` finalizes the segment (SegmentWriter::drop).
    drop(writer);
}

/// Run the Deribit options source until `shutdown` is notified, then finalize.
pub(super) async fn run_deribit(
    config: DeribitConfig,
    writer: Arc<SegmentWriter>,
    clock: Arc<dyn EnvelopeClock>,
    health: Arc<RecorderHealth>,
    shutdown: Arc<Notify>,
) {
    let state = ConnState::new();
    let sink = Arc::new(CountingSink {
        writer: writer.clone(),
        clock: clock.clone(),
        health,
        metrics: ConnMetrics::new("deribit", "deribit"),
        state,
        account_hl_rest: false,
    });
    DeribitSource::new(config, sink, clock).run(shutdown).await;
    // Dropping the last `Arc` finalizes the segment (SegmentWriter::drop).
    drop(writer);
}

/// Run one CEX reference source (SPEC-0008 §9, R-8) until shutdown, then
/// finalize its segment.
///
/// Takes a `watch` receiver, not a `Notify`, so every CEX source sharing the
/// recorder's shutdown signal wakes: `Notify::notify_one` woke only one waiter
/// (PERF-001).
pub(super) async fn run_cex(source: CexSource, shutdown: watch::Receiver<bool>) {
    source.run(shutdown).await;
}
