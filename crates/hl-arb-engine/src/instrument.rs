//! In-thread latency instrumentation (SPEC-0010 §17, task E-10).
//!
//! The engine thread carries one [`Stamps`] per decision through the hot path
//! (socket read → decode → dequeue → decide → risk → sign → handoff → write →
//! ack) and folds the derived spans into a [`LatencyRecorder`]. The recorder is
//! **owned by the engine thread**: every method takes `&mut self`, so recording
//! is a plain store with no atomics and no cross-thread lock on the hot path
//! (SPEC-0010 §4). A metrics-export task drains the recorder once per second
//! with [`LatencyRecorder::flush`], which returns plain values to be published
//! as Prometheus histograms/counters under the names in §17.
//!
//! ## Histograms
//!
//! `hdrhistogram` is **not** a workspace dependency, and AGENTS.md requires
//! justification for a new one, so this module ships a small fixed-bucket
//! histogram of its own (SPEC-0010 §17). It is an HDR-style base-2 layout:
//! exact 1 ns buckets for values `< 16 ns`, then 16 sub-buckets per octave
//! (≈ 4.4 % relative resolution). Recording is O(1), integer-only, and
//! allocation-free. Switching to `hdrhistogram` once a dependency is justified
//! (e.g. to get its serialization/quantile guarantees) is a deliberate
//! follow-up; the public API here would not change.
//!
//! ## Metric names (SPEC-0010 §17)
//!
//! Decision spans:
//!
//! | Metric | Span |
//! |---|---|
//! | `hl_engine_decode_seconds` | `t_decoded − t_recv` |
//! | `hl_engine_queue_seconds` | `t_dequeued − t_decoded` |
//! | `hl_engine_decide_seconds` | `t_decided − t_dequeued` |
//! | `hl_engine_risk_seconds` | `t_risked − t_decided` |
//! | `hl_engine_sign_seconds` | `t_signed − t_risked` |
//! | `hl_engine_handoff_seconds` | `t_written − t_signed` |
//! | `hl_engine_tick_to_order_seconds` | `t_written − t_recv` (engine-side headline) |
//! | `hl_engine_submit_ack_seconds` | `t_ack − t_written` (engine-side) |
//!
//! These are the engine's in-process spans. The live exec path records the
//! published end-to-end `hl_tick_to_order_seconds` and the transport
//! `hl_submit_ack_seconds{transport}` separately (SPEC-0002 H-7), so the
//! engine-side names deliberately carry the `hl_engine_` prefix.
//!
//! Loop health:
//!
//! | Metric | Meaning |
//! |---|---|
//! | `hl_engine_iteration_seconds` | wall time of one loop iteration |
//! | `hl_engine_events_per_iteration` | events drained per iteration |
//! | `hl_engine_market_drops_total` | market updates dropped on a full channel |
//! | `hl_engine_idle_ratio` | fraction of iterations that drained no events |
//!
//! ## Wiring (for the loop owner)
//!
//! `run.rs` is intentionally not modified by E-10. To instrument the real loop,
//! own a `LatencyRecorder` next to `EngineLoop` and:
//!
//! - at the top of `iterate`, note `let start = clock.mono_ns();` and count
//!   events; before returning call `recorder.record_iteration(start.elapsed(),
//!   n)`;
//! - in `InputHandles::send_market`'s drop branch (or wherever the producer
//!   counts `MarketSend::Dropped`), call `recorder.record_market_drop(coin)`;
//! - stamp `Stamps` along the decision path and call
//!   `recorder.record(&stamps)` where the signed payload is handed off.
//!
//! The 1 s exporter task calls `flush()` and publishes the result; it must clone
//! nothing from the engine thread except the returned `Vec`.

use hl_arb_metrics::names;

/// One decision's latency stamps, in nanoseconds (SPEC-0010 §17).
///
/// `0` means "unset": a stage that did not run for this decision (e.g. replay
/// without signing) leaves its stamp at zero, and the derived span is `None` so
/// it never pollutes a histogram.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stamps {
    /// Wall/monotonic ns at the socket read (or the recorded `t_ns` in replay).
    pub t_recv: u64,
    /// When the frame finished decoding.
    pub t_decoded: u64,
    /// When the engine thread dequeued the event.
    pub t_dequeued: u64,
    /// When the strategy returned a decision.
    pub t_decided: u64,
    /// When risk finished.
    pub t_risked: u64,
    /// When the payload was signed.
    pub t_signed: u64,
    /// When the signed payload was handed to the exec channel.
    pub t_handoff: u64,
    /// When the exec writer returned from the socket write.
    pub t_written: u64,
    /// When the venue reply/ack arrived.
    pub t_ack: u64,
}

/// A non-zero, non-negative span between two stamps, or `None` if either
/// endpoint is unset (0).
#[inline]
fn span(from: u64, to: u64) -> Option<u64> {
    if from == 0 || to == 0 || to < from {
        None
    } else {
        Some(to - from)
    }
}

impl Stamps {
    /// Decode span: `t_decoded − t_recv`.
    pub fn decode(&self) -> Option<u64> {
        span(self.t_recv, self.t_decoded)
    }

    /// Queue span: `t_dequeued − t_decoded`.
    pub fn queue(&self) -> Option<u64> {
        span(self.t_decoded, self.t_dequeued)
    }

    /// Decide span: `t_decided − t_dequeued`.
    pub fn decide(&self) -> Option<u64> {
        span(self.t_dequeued, self.t_decided)
    }

    /// Risk span: `t_risked − t_decided`.
    pub fn risk(&self) -> Option<u64> {
        span(self.t_decided, self.t_risked)
    }

    /// Sign span: `t_signed − t_risked`.
    pub fn sign(&self) -> Option<u64> {
        span(self.t_risked, self.t_signed)
    }

    /// Handoff span: `t_written − t_signed`.
    ///
    /// This is the §17 histogram span, which deliberately covers the engine
    /// handoff (`t_handoff`) *and* the exec write. Use [`Self::signed_to_handoff`]
    /// for the narrower `t_handoff − t_signed` when diagnosing the channel hop.
    pub fn handoff(&self) -> Option<u64> {
        span(self.t_signed, self.t_written)
    }

    /// Engine-to-exec channel span: `t_handoff − t_signed`.
    pub fn signed_to_handoff(&self) -> Option<u64> {
        span(self.t_signed, self.t_handoff)
    }

    /// Engine-side headline latency: `t_written − t_recv`
    /// (`hl_engine_tick_to_order_seconds`).
    pub fn tick_to_order(&self) -> Option<u64> {
        span(self.t_recv, self.t_written)
    }

    /// Submit-to-ack span: `t_ack − t_written` (network + venue).
    pub fn submit_ack(&self) -> Option<u64> {
        span(self.t_written, self.t_ack)
    }
}

/// Number of sub-buckets per power-of-two octave.
const SUB_BITS: u32 = 4;
/// Sub-buckets per octave (`1 << SUB_BITS`).
const SUB_COUNT: usize = 1 << SUB_BITS;
/// Total buckets: covers every exponent a `u64` ns value can take.
const BUCKETS: usize = 1024;

/// The bucket index for a nanosecond value (monotonic in `value`).
#[inline]
fn bucket_index(value: u64) -> usize {
    let v = value.max(1);
    let exp = (63 - v.leading_zeros()) as usize;
    if exp < SUB_BITS as usize {
        return v as usize;
    }
    let sub = ((v >> (exp - SUB_BITS as usize)) & (SUB_COUNT as u64 - 1)) as usize;
    ((exp - SUB_BITS as usize + 1) * SUB_COUNT + sub).min(BUCKETS - 1)
}

/// The exclusive upper bound a bucket represents (used for quantile estimates).
#[inline]
fn bucket_upper(index: usize) -> u64 {
    if index < SUB_COUNT {
        return index as u64 + 1;
    }
    let e = (index / SUB_COUNT - 1) as u32;
    let sub = (index % SUB_COUNT) as u128;
    ((SUB_COUNT as u128 + sub + 1) << e).min(u64::MAX as u128) as u64
}

/// A fixed-bucket, integer-only latency histogram (SPEC-0010 §17).
///
/// Recording is O(1) and allocation-free; only the length in nanoseconds is
/// stored, never a sample vector. Quantiles return the exclusive upper bound of
/// the bucket the quantile falls in, so they are a slight over-estimate (by at
/// most one bucket, ≈ 4.4 % relative).
#[derive(Debug, Clone, Copy)]
struct Histogram {
    buckets: [u64; BUCKETS],
    count: u64,
    sum: u64,
    min: u64,
    max: u64,
}

impl Histogram {
    const fn new() -> Self {
        Self {
            buckets: [0; BUCKETS],
            count: 0,
            sum: 0,
            min: 0,
            max: 0,
        }
    }

    #[inline]
    fn record(&mut self, value: u64) {
        self.buckets[bucket_index(value)] += 1;
        if self.count == 0 {
            self.min = value;
            self.max = value;
        } else {
            self.min = self.min.min(value);
            self.max = self.max.max(value);
        }
        self.count += 1;
        self.sum = self.sum.saturating_add(value);
    }

    fn reset(&mut self) {
        self.buckets = [0; BUCKETS];
        self.count = 0;
        self.sum = 0;
        self.min = 0;
        self.max = 0;
    }

    /// The bucket upper bound at `q` per-mille (0 when empty).
    fn quantile(&self, q_per_mille: u32) -> u64 {
        if self.count == 0 {
            return 0;
        }
        let target = (self.count as u128 * q_per_mille as u128).div_ceil(1000);
        let mut cumulative = 0u128;
        for (index, &count) in self.buckets.iter().enumerate() {
            if count == 0 {
                continue;
            }
            cumulative += count as u128;
            if cumulative >= target {
                return bucket_upper(index);
            }
        }
        self.max
    }

    fn percentiles(&self) -> Percentiles {
        Percentiles {
            count: self.count,
            p50_ns: self.quantile(500),
            p90_ns: self.quantile(900),
            p99_ns: self.quantile(990),
            p999_ns: self.quantile(999),
            max_ns: self.max,
            sum_ns: self.sum,
        }
    }
}

/// A latency histogram's percentile summary, in nanoseconds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Percentiles {
    /// Number of samples in the exported window.
    pub count: u64,
    /// 50th percentile (bucket upper bound).
    pub p50_ns: u64,
    /// 90th percentile (bucket upper bound).
    pub p90_ns: u64,
    /// 99th percentile (bucket upper bound).
    pub p99_ns: u64,
    /// 99.9th percentile (bucket upper bound).
    pub p999_ns: u64,
    /// Maximum observed value.
    pub max_ns: u64,
    /// Sum of all observed values.
    pub sum_ns: u64,
}

impl Percentiles {
    /// Integer mean in nanoseconds (0 when there are no samples).
    pub fn mean_ns(&self) -> u64 {
        self.sum_ns.checked_div(self.count).unwrap_or(0)
    }
}

/// One exported metric: a latency summary, a monotonic counter, or a ratio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    /// A latency histogram summary for the exported window.
    Latency(Percentiles),
    /// A cumulative counter (e.g. `hl_engine_market_drops_total`).
    Counter(u64),
    /// A ratio in parts-per-billion (exporter divides by `1e9`).
    RatioPpb(u64),
}

/// Per-thread latency recorder and loop-health counters (SPEC-0010 §17).
///
/// Owned by the engine thread. All recording methods take `&mut self` and never
/// touch shared state, so there is no lock, atomic, or allocation on the hot
/// path. [`Self::flush`] resets the window histograms (interval histograms) and
/// leaves cumulative counters intact.
#[derive(Debug, Clone)]
pub struct LatencyRecorder {
    decode: Histogram,
    queue: Histogram,
    decide: Histogram,
    risk: Histogram,
    sign: Histogram,
    handoff: Histogram,
    tick_to_order: Histogram,
    submit_ack: Histogram,
    iteration: Histogram,
    events_per_iteration: Histogram,
    market_drops: u64,
    window_iterations: u64,
    window_idle: u64,
}

impl Default for LatencyRecorder {
    fn default() -> Self {
        Self::new()
    }
}

impl LatencyRecorder {
    /// A fresh recorder with empty histograms.
    pub fn new() -> Self {
        Self {
            decode: Histogram::new(),
            queue: Histogram::new(),
            decide: Histogram::new(),
            risk: Histogram::new(),
            sign: Histogram::new(),
            handoff: Histogram::new(),
            tick_to_order: Histogram::new(),
            submit_ack: Histogram::new(),
            iteration: Histogram::new(),
            events_per_iteration: Histogram::new(),
            market_drops: 0,
            window_iterations: 0,
            window_idle: 0,
        }
    }

    /// Fold one decision's stamps into the latency histograms.
    ///
    /// Stages whose stamp is unset (0) are skipped, so a partial chain is safe.
    pub fn record(&mut self, stamps: &Stamps) {
        if let Some(v) = stamps.decode() {
            self.decode.record(v);
        }
        if let Some(v) = stamps.queue() {
            self.queue.record(v);
        }
        if let Some(v) = stamps.decide() {
            self.decide.record(v);
        }
        if let Some(v) = stamps.risk() {
            self.risk.record(v);
        }
        if let Some(v) = stamps.sign() {
            self.sign.record(v);
        }
        if let Some(v) = stamps.handoff() {
            self.handoff.record(v);
        }
        if let Some(v) = stamps.tick_to_order() {
            self.tick_to_order.record(v);
        }
        if let Some(v) = stamps.submit_ack() {
            self.submit_ack.record(v);
        }
    }

    /// Fold one loop iteration's duration and drained-event count (loop health).
    ///
    /// An iteration with `events == 0` counts toward the idle ratio.
    pub fn record_iteration(&mut self, iteration_ns: u64, events: usize) {
        self.iteration.record(iteration_ns);
        self.events_per_iteration.record(events as u64);
        self.window_iterations += 1;
        if events == 0 {
            self.window_idle += 1;
        }
    }

    /// Record one market update dropped on a full channel.
    pub fn record_market_drop(&mut self) {
        self.market_drops = self.market_drops.saturating_add(1);
    }

    /// The cumulative number of market drops since construction.
    pub fn market_drops(&self) -> u64 {
        self.market_drops
    }

    /// Export every §17 metric and reset the window histograms.
    ///
    /// The returned `Vec` is what the 1 s metrics task publishes; it is built at
    /// export time (off the hot path) and may allocate. `hl_engine_market_drops_total`
    /// stays cumulative, as a Prometheus counter must.
    pub fn flush(&mut self) -> Vec<(&'static str, Metric)> {
        let mut out = Vec::with_capacity(12);
        out.push((
            names::ENGINE_DECODE_SECONDS,
            Metric::Latency(self.decode.percentiles()),
        ));
        out.push((
            names::ENGINE_QUEUE_SECONDS,
            Metric::Latency(self.queue.percentiles()),
        ));
        out.push((
            names::ENGINE_DECIDE_SECONDS,
            Metric::Latency(self.decide.percentiles()),
        ));
        out.push((
            names::ENGINE_RISK_SECONDS,
            Metric::Latency(self.risk.percentiles()),
        ));
        out.push((
            names::ENGINE_SIGN_SECONDS,
            Metric::Latency(self.sign.percentiles()),
        ));
        out.push((
            names::ENGINE_HANDOFF_SECONDS,
            Metric::Latency(self.handoff.percentiles()),
        ));
        out.push((
            names::ENGINE_TICK_TO_ORDER_SECONDS,
            Metric::Latency(self.tick_to_order.percentiles()),
        ));
        out.push((
            names::ENGINE_SUBMIT_ACK_SECONDS,
            Metric::Latency(self.submit_ack.percentiles()),
        ));
        out.push((
            names::ENGINE_ITERATION_SECONDS,
            Metric::Latency(self.iteration.percentiles()),
        ));
        out.push((
            names::ENGINE_EVENTS_PER_ITERATION,
            Metric::Latency(self.events_per_iteration.percentiles()),
        ));
        out.push((
            names::ENGINE_MARKET_DROPS_TOTAL,
            Metric::Counter(self.market_drops),
        ));
        let idle_ppb = self
            .window_idle
            .saturating_mul(1_000_000_000)
            .checked_div(self.window_iterations)
            .unwrap_or(0);
        out.push((names::ENGINE_IDLE_RATIO, Metric::RatioPpb(idle_ppb)));

        self.decode.reset();
        self.queue.reset();
        self.decide.reset();
        self.risk.reset();
        self.sign.reset();
        self.handoff.reset();
        self.tick_to_order.reset();
        self.submit_ack.reset();
        self.iteration.reset();
        self.events_per_iteration.reset();
        self.window_iterations = 0;
        self.window_idle = 0;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_stamps() -> Stamps {
        Stamps {
            t_recv: 1_000,
            t_decoded: 1_030,
            t_dequeued: 1_040,
            t_decided: 1_060,
            t_risked: 1_075,
            t_signed: 1_500,
            t_handoff: 1_510,
            t_written: 1_520,
            t_ack: 1_900,
        }
    }

    #[test]
    fn spans_derive_from_set_stamps() {
        let s = full_stamps();
        assert_eq!(s.decode(), Some(30));
        assert_eq!(s.queue(), Some(10));
        assert_eq!(s.decide(), Some(20));
        assert_eq!(s.risk(), Some(15));
        assert_eq!(s.sign(), Some(425));
        assert_eq!(s.handoff(), Some(20));
        assert_eq!(s.signed_to_handoff(), Some(10));
        assert_eq!(s.tick_to_order(), Some(520));
        assert_eq!(s.submit_ack(), Some(380));
    }

    #[test]
    fn unset_or_backwards_stamps_yield_no_span() {
        let mut s = full_stamps();
        s.t_decoded = 0;
        assert_eq!(s.decode(), None);
        s = full_stamps();
        s.t_written = 5; // before t_recv: nonsense, must not underflow
        assert_eq!(s.tick_to_order(), None);
        assert_eq!(Stamps::default().decode(), None);
        assert_eq!(Stamps::default().risk(), None);
    }

    #[test]
    fn buckets_are_monotonic_and_cover_every_value() {
        let mut last = 0usize;
        for value in [
            0u64,
            1,
            15,
            16,
            17,
            31,
            32,
            1_000,
            1_000_000,
            1 << 40,
            u64::MAX,
        ] {
            let index = bucket_index(value);
            assert!(index < BUCKETS, "{value} -> {index}");
            assert!(index >= last, "{value} went backwards");
            last = index;
            let upper = bucket_upper(index);
            assert!(upper >= value, "{value} not covered by {upper}");
        }
    }

    #[test]
    fn histogram_reports_percentiles_within_bucket_error() {
        let mut h = Histogram::new();
        for v in 1..=1000u64 {
            h.record(v);
        }
        let p = h.percentiles();
        assert_eq!(p.count, 1000);
        assert!(p.p50_ns >= 500 && p.p50_ns <= 560, "{}", p.p50_ns);
        assert!(p.p99_ns >= 990 && p.p99_ns <= 1120, "{}", p.p99_ns);
        assert!(p.p999_ns >= 999, "{}", p.p999_ns);
        assert!(p.max_ns >= 1000 && p.max_ns <= 1100, "{}", p.max_ns);
        assert_eq!(p.sum_ns, 500_500);
        assert_eq!(p.mean_ns(), 500);
    }

    #[test]
    fn empty_histogram_is_all_zero() {
        let p = Histogram::new().percentiles();
        assert_eq!(p.count, 0);
        assert_eq!(p.p50_ns, 0);
        assert_eq!(p.max_ns, 0);
        assert_eq!(p.mean_ns(), 0);
    }

    #[test]
    fn record_only_histograms_set_spans() {
        let mut recorder = LatencyRecorder::new();
        let mut s = full_stamps();
        s.t_signed = 0; // signing did not run: sign + handoff + ack absent
        recorder.record(&s);
        let out = recorder.flush();
        let get = |name: &str| {
            out.iter()
                .find(|(n, _)| *n == name)
                .map(|(_, m)| *m)
                .unwrap_or_else(|| panic!("missing {name}"))
        };
        let Metric::Latency(risk) = get(names::ENGINE_RISK_SECONDS) else {
            panic!("risk not a latency")
        };
        assert_eq!(risk.count, 1);
        let Metric::Latency(sign) = get(names::ENGINE_SIGN_SECONDS) else {
            panic!("sign not a latency")
        };
        assert_eq!(sign.count, 0);
    }

    #[test]
    fn loop_health_counters_and_idle_ratio() {
        let mut recorder = LatencyRecorder::new();
        for _ in 0..3 {
            recorder.record_iteration(1_000, 5);
        }
        recorder.record_iteration(2_000, 0);
        recorder.record_market_drop();
        recorder.record_market_drop();

        let out = recorder.flush();
        let get = |name: &str| {
            out.iter()
                .find(|(n, _)| *n == name)
                .map(|(_, m)| *m)
                .unwrap_or_else(|| panic!("missing {name}"))
        };
        let Metric::Latency(iter) = get(names::ENGINE_ITERATION_SECONDS) else {
            panic!("iteration not latency")
        };
        assert_eq!(iter.count, 4);
        assert_eq!(iter.max_ns, 2_000);
        let Metric::Counter(drops) = get(names::ENGINE_MARKET_DROPS_TOTAL) else {
            panic!("drops not counter")
        };
        assert_eq!(drops, 2);
        // 1 idle iteration of 4 => 25% => 250_000_000 ppb.
        let Metric::RatioPpb(idle) = get(names::ENGINE_IDLE_RATIO) else {
            panic!("idle not ratio")
        };
        assert_eq!(idle, 250_000_000);
    }

    #[test]
    fn flush_resets_window_but_not_counters() {
        let mut recorder = LatencyRecorder::new();
        recorder.record(&full_stamps());
        recorder.record_market_drop();
        let first = recorder.flush();
        assert_eq!(first.len(), 12);
        let Metric::Latency(decode) = first[0].1 else {
            panic!("decode not latency")
        };
        assert_eq!(decode.count, 1);

        let second = recorder.flush();
        let Metric::Latency(decode) = second[0].1 else {
            panic!("decode not latency")
        };
        assert_eq!(decode.count, 0);
        let Metric::Counter(drops) = second[10].1 else {
            panic!("drops not counter")
        };
        assert_eq!(drops, 1, "counters are cumulative across windows");
    }
}
