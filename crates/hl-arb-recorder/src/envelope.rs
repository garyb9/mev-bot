//! The recorder envelope: the on-disk record format (SPEC-0008 §5.1, §5.3).
//!
//! Every line in a segment file is one JSON-serialized [`Envelope`]. A frame's
//! `raw` text is stored byte-for-byte as a JSON string and is never decoded or
//! re-serialized by the recorder.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Envelope schema version written to new envelopes (SPEC-0008 §5.1).
pub const SCHEMA_VERSION: u8 = 1;

/// The kind of an envelope (SPEC-0008 §5.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// A text frame received on a socket.
    Frame,
    /// A binary frame received on a socket (`raw` is base64).
    FrameBin,
    /// A REST response from the snapshotter.
    Rest,
    /// A subscribe message that was sent.
    Sub,
    /// A socket connected (initial or reconnect).
    ConnOpen,
    /// A socket error/close, watchdog timeout, or dropped envelope.
    GapStart,
    /// A reconnect completed and all resubscribes were sent.
    GapEnd,
    /// Periodic clock offset record.
    Clock,
    /// First line of every segment file.
    SegmentOpen,
    /// Last line of every cleanly closed segment.
    SegmentClose,
    /// A kind this build does not know (forward compatibility).
    ///
    /// Deserialization maps any unrecognized `kind` string here instead of
    /// failing, so a segment written by a newer recorder can still be read.
    /// The reader skips these envelopes and counts them (SPEC-0008 §5.3).
    #[serde(other)]
    Unknown,
}

impl Kind {
    /// The stable wire name of this kind (matches the serialized JSON).
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Frame => "frame",
            Kind::FrameBin => "frame_bin",
            Kind::Rest => "rest",
            Kind::Sub => "sub",
            Kind::ConnOpen => "conn_open",
            Kind::GapStart => "gap_start",
            Kind::GapEnd => "gap_end",
            Kind::Clock => "clock",
            Kind::SegmentOpen => "segment_open",
            Kind::SegmentClose => "segment_close",
            Kind::Unknown => "unknown",
        }
    }
}

/// Metadata written as the first line of every segment (SPEC-0008 §5.3).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentOpenMeta {
    /// Recorder host name.
    pub host: String,
    /// Git commit the recorder was built from.
    pub git_sha: String,
    /// Recording profile name.
    pub profile: String,
    /// Recorder version.
    pub recorder_version: String,
}

/// One JSON object per line in a segment file (SPEC-0008 §5.1).
///
/// `raw` is present for `frame`, `frame_bin`, and `rest` kinds; `meta` is
/// present for the non-frame kinds. Both are omitted from the serialized JSON
/// when `None`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    /// Envelope schema version.
    pub v: u8,
    /// Source id (SPEC-0008 §5.4).
    pub src: String,
    /// Connection id within the source.
    pub conn: String,
    /// Per-connection counter, starting at 0 and incremented per envelope.
    pub seq: u64,
    /// Local wall-clock receive time, ns since the Unix epoch.
    pub t_ns: i64,
    /// Local monotonic time in ns since the process-start anchor.
    pub mono_ns: u64,
    /// Envelope kind.
    pub kind: Kind,
    /// Exact raw text received, unmodified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
    /// Kind-specific metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

impl Envelope {
    /// Build an envelope of `kind` stamped by `clock`, with explicit `raw` and
    /// `meta`.
    pub fn new(
        clock: &dyn EnvelopeClock,
        src: impl Into<String>,
        conn: impl Into<String>,
        seq: u64,
        kind: Kind,
        raw: Option<String>,
        meta: Option<Value>,
    ) -> Self {
        Self {
            v: SCHEMA_VERSION,
            src: src.into(),
            conn: conn.into(),
            seq,
            t_ns: clock.t_ns(),
            mono_ns: clock.mono_ns(),
            kind,
            raw,
            meta,
        }
    }

    /// A text frame (`kind:"frame"`).
    pub fn frame(
        clock: &dyn EnvelopeClock,
        src: impl Into<String>,
        conn: impl Into<String>,
        seq: u64,
        raw: impl Into<String>,
    ) -> Self {
        Self::new(clock, src, conn, seq, Kind::Frame, Some(raw.into()), None)
    }

    /// A binary frame (`kind:"frame_bin"`), with `raw` holding base64.
    pub fn frame_bin(
        clock: &dyn EnvelopeClock,
        src: impl Into<String>,
        conn: impl Into<String>,
        seq: u64,
        raw: impl Into<String>,
    ) -> Self {
        Self::new(
            clock,
            src,
            conn,
            seq,
            Kind::FrameBin,
            Some(raw.into()),
            None,
        )
    }

    /// A REST response (`kind:"rest"`).
    ///
    /// `meta` must be an object with `req`, `status`, and `latency_us`.
    pub fn rest(
        clock: &dyn EnvelopeClock,
        src: impl Into<String>,
        conn: impl Into<String>,
        seq: u64,
        raw: impl Into<String>,
        meta: Value,
    ) -> Self {
        Self::new(
            clock,
            src,
            conn,
            seq,
            Kind::Rest,
            Some(raw.into()),
            Some(meta),
        )
    }

    /// A subscribe message (`kind:"sub"`).
    pub fn sub(
        clock: &dyn EnvelopeClock,
        src: impl Into<String>,
        conn: impl Into<String>,
        seq: u64,
        sub: Value,
    ) -> Self {
        Self::new(
            clock,
            src,
            conn,
            seq,
            Kind::Sub,
            None,
            Some(json!({ "sub": sub })),
        )
    }

    /// A socket connection (`kind:"conn_open"`).
    pub fn conn_open(
        clock: &dyn EnvelopeClock,
        src: impl Into<String>,
        conn: impl Into<String>,
        seq: u64,
        url: impl Into<String>,
        attempt: u32,
    ) -> Self {
        Self::new(
            clock,
            src,
            conn,
            seq,
            Kind::ConnOpen,
            None,
            Some(json!({ "url": url.into(), "attempt": attempt })),
        )
    }

    /// The start of a data gap (`kind:"gap_start"`).
    pub fn gap_start(
        clock: &dyn EnvelopeClock,
        src: impl Into<String>,
        conn: impl Into<String>,
        seq: u64,
        reason: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self::new(
            clock,
            src,
            conn,
            seq,
            Kind::GapStart,
            None,
            Some(json!({ "reason": reason.into(), "detail": detail.into() })),
        )
    }

    /// The end of a data gap (`kind:"gap_end"`).
    pub fn gap_end(
        clock: &dyn EnvelopeClock,
        src: impl Into<String>,
        conn: impl Into<String>,
        seq: u64,
        gap_ms: u64,
    ) -> Self {
        Self::new(
            clock,
            src,
            conn,
            seq,
            Kind::GapEnd,
            None,
            Some(json!({ "gap_ms": gap_ms })),
        )
    }

    /// Build an envelope with **explicit** timestamps instead of reading the
    /// clock (SPEC-0008 RW-2).
    ///
    /// Used to stamp a `gap_start` at the disconnect instant and a `gap_end` at
    /// the reopen instant, so `gap_end.t_ns - gap_start.t_ns` (and the derived
    /// `gap_ms`/coverage) measures the real outage rather than the moment the
    /// recorder happened to write the line. Callers pass both the wall-clock
    /// `t_ns` and the process-monotonic `mono_ns` for that same instant.
    #[allow(clippy::too_many_arguments)]
    pub fn new_at(
        src: impl Into<String>,
        conn: impl Into<String>,
        seq: u64,
        kind: Kind,
        t_ns: i64,
        mono_ns: u64,
        raw: Option<String>,
        meta: Option<Value>,
    ) -> Self {
        Self {
            v: SCHEMA_VERSION,
            src: src.into(),
            conn: conn.into(),
            seq,
            t_ns,
            mono_ns,
            kind,
            raw,
            meta,
        }
    }

    /// The start of a data gap stamped at `t_ns`/`mono_ns` (`kind:"gap_start"`).
    pub fn gap_start_at(
        src: impl Into<String>,
        conn: impl Into<String>,
        seq: u64,
        t_ns: i64,
        mono_ns: u64,
        reason: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self::new_at(
            src,
            conn,
            seq,
            Kind::GapStart,
            t_ns,
            mono_ns,
            None,
            Some(json!({ "reason": reason.into(), "detail": detail.into() })),
        )
    }

    /// The end of a data gap stamped at `t_ns`/`mono_ns` (`kind:"gap_end"`).
    pub fn gap_end_at(
        src: impl Into<String>,
        conn: impl Into<String>,
        seq: u64,
        t_ns: i64,
        mono_ns: u64,
        gap_ms: u64,
    ) -> Self {
        Self::new_at(
            src,
            conn,
            seq,
            Kind::GapEnd,
            t_ns,
            mono_ns,
            None,
            Some(json!({ "gap_ms": gap_ms })),
        )
    }

    /// A periodic clock record (`kind:"clock"`).
    pub fn clock(
        clock: &dyn EnvelopeClock,
        src: impl Into<String>,
        conn: impl Into<String>,
        seq: u64,
        chrony_offset_ns: Option<i64>,
        chrony_stratum: Option<u8>,
    ) -> Self {
        Self::new(
            clock,
            src,
            conn,
            seq,
            Kind::Clock,
            None,
            Some(json!({
                "chrony_offset_ns": chrony_offset_ns,
                "chrony_stratum": chrony_stratum,
            })),
        )
    }

    /// The first line of a segment (`kind:"segment_open"`).
    pub fn segment_open(
        clock: &dyn EnvelopeClock,
        src: impl Into<String>,
        conn: impl Into<String>,
        seq: u64,
        meta: &SegmentOpenMeta,
    ) -> Self {
        Self::new(
            clock,
            src,
            conn,
            seq,
            Kind::SegmentOpen,
            None,
            Some(json!({
                "host": &meta.host,
                "git_sha": &meta.git_sha,
                "profile": &meta.profile,
                "recorder_version": &meta.recorder_version,
            })),
        )
    }

    /// The last line of a cleanly closed segment (`kind:"segment_close"`).
    pub fn segment_close(
        clock: &dyn EnvelopeClock,
        src: impl Into<String>,
        conn: impl Into<String>,
        seq: u64,
        records: u64,
        bytes_raw: u64,
    ) -> Self {
        Self::new(
            clock,
            src,
            conn,
            seq,
            Kind::SegmentClose,
            None,
            Some(json!({ "records": records, "bytes_raw": bytes_raw })),
        )
    }
}

/// A source of envelope receive timestamps.
///
/// Extends [`hl_arb_core::clock::Clock`] (the millisecond wall clock shared with
/// the nonce and DB code) with the nanosecond wall clock and the
/// process-relative monotonic clock the recorder needs (SPEC-0008 §5.1).
pub trait EnvelopeClock: hl_arb_core::clock::Clock {
    /// Wall-clock receive time in nanoseconds since the Unix epoch.
    fn t_ns(&self) -> i64 {
        self.now_ms() as i64 * 1_000_000
    }

    /// Monotonic time in nanoseconds since the process-start anchor.
    fn mono_ns(&self) -> u64;
}

/// A monotonic nanosecond counter anchored at construction.
#[derive(Debug, Clone, Copy)]
pub struct MonoClock {
    anchor: Instant,
}

impl MonoClock {
    /// Anchor the counter to the current instant.
    pub fn new() -> Self {
        Self {
            anchor: Instant::now(),
        }
    }

    /// Nanoseconds elapsed since the anchor.
    pub fn now_ns(&self) -> u64 {
        self.anchor.elapsed().as_nanos() as u64
    }
}

impl Default for MonoClock {
    fn default() -> Self {
        Self::new()
    }
}

/// Wall-clock and monotonic source backed by the system clocks.
#[derive(Debug, Clone, Copy)]
pub struct SystemEnvelopeClock {
    wall: hl_arb_core::clock::SystemClock,
    mono: MonoClock,
}

impl SystemEnvelopeClock {
    /// Build a clock anchored at the current instant.
    pub fn new() -> Self {
        Self {
            wall: hl_arb_core::clock::SystemClock,
            mono: MonoClock::new(),
        }
    }
}

impl Default for SystemEnvelopeClock {
    fn default() -> Self {
        Self::new()
    }
}

impl hl_arb_core::clock::Clock for SystemEnvelopeClock {
    fn now_ms(&self) -> u64 {
        self.wall.now_ms()
    }
}

impl EnvelopeClock for SystemEnvelopeClock {
    fn t_ns(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0)
    }

    fn mono_ns(&self) -> u64 {
        self.mono.now_ns()
    }
}

/// Deterministic clock for tests: reads and advances an injected wall time and
/// monotonic time.
#[derive(Debug, Clone)]
pub struct FixedEnvelopeClock {
    t_ns: Arc<AtomicI64>,
    mono_ns: Arc<AtomicU64>,
}

impl FixedEnvelopeClock {
    /// Pin both clocks to the given values.
    pub fn new(t_ns: i64, mono_ns: u64) -> Self {
        Self {
            t_ns: Arc::new(AtomicI64::new(t_ns)),
            mono_ns: Arc::new(AtomicU64::new(mono_ns)),
        }
    }

    /// Set the wall-clock time.
    pub fn set_t_ns(&self, t_ns: i64) {
        self.t_ns.store(t_ns, Ordering::SeqCst);
    }

    /// Set the monotonic time.
    pub fn set_mono_ns(&self, mono_ns: u64) {
        self.mono_ns.store(mono_ns, Ordering::SeqCst);
    }

    /// Advance both clocks by `delta_ns`.
    pub fn advance_ns(&self, delta_ns: u64) {
        self.t_ns.fetch_add(delta_ns as i64, Ordering::SeqCst);
        self.mono_ns.fetch_add(delta_ns, Ordering::SeqCst);
    }
}

impl hl_arb_core::clock::Clock for FixedEnvelopeClock {
    fn now_ms(&self) -> u64 {
        (self.t_ns.load(Ordering::SeqCst).max(0) as u64) / 1_000_000
    }
}

impl EnvelopeClock for FixedEnvelopeClock {
    fn t_ns(&self) -> i64 {
        self.t_ns.load(Ordering::SeqCst)
    }

    fn mono_ns(&self) -> u64 {
        self.mono_ns.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T_NS: i64 = 1_700_000_000_000_000_000;
    const MONO_NS: u64 = 987_654_321_098_765;

    fn clk() -> FixedEnvelopeClock {
        FixedEnvelopeClock::new(T_NS, MONO_NS)
    }

    #[test]
    fn golden_frame() {
        let env = Envelope::frame(&clk(), "hl-ws", "hl-ws-01", 0, r#"{"a":1}"#);
        assert_eq!(
            serde_json::to_string(&env).unwrap(),
            r#"{"v":1,"src":"hl-ws","conn":"hl-ws-01","seq":0,"t_ns":1700000000000000000,"mono_ns":987654321098765,"kind":"frame","raw":"{\"a\":1}"}"#
        );
    }

    #[test]
    fn golden_frame_bin() {
        let env = Envelope::frame_bin(&clk(), "hyperevm", "hyperevm", 7, "AAECAw==");
        assert_eq!(
            serde_json::to_string(&env).unwrap(),
            r#"{"v":1,"src":"hyperevm","conn":"hyperevm","seq":7,"t_ns":1700000000000000000,"mono_ns":987654321098765,"kind":"frame_bin","raw":"AAECAw=="}"#
        );
    }

    #[test]
    fn golden_rest() {
        let env = Envelope::rest(
            &clk(),
            "hl-rest",
            "hl-rest",
            3,
            r#"{"type":"meta"}"#,
            json!({ "req": { "type": "meta" }, "status": 200, "latency_us": 1234 }),
        );
        assert_eq!(
            serde_json::to_string(&env).unwrap(),
            r#"{"v":1,"src":"hl-rest","conn":"hl-rest","seq":3,"t_ns":1700000000000000000,"mono_ns":987654321098765,"kind":"rest","raw":"{\"type\":\"meta\"}","meta":{"latency_us":1234,"req":{"type":"meta"},"status":200}}"#
        );
    }

    #[test]
    fn golden_sub() {
        let env = Envelope::sub(
            &clk(),
            "hl-ws",
            "hl-ws-01",
            1,
            json!({ "type": "bbo", "coin": "BTC" }),
        );
        assert_eq!(
            serde_json::to_string(&env).unwrap(),
            r#"{"v":1,"src":"hl-ws","conn":"hl-ws-01","seq":1,"t_ns":1700000000000000000,"mono_ns":987654321098765,"kind":"sub","meta":{"sub":{"coin":"BTC","type":"bbo"}}}"#
        );
    }

    #[test]
    fn golden_conn_open() {
        let env = Envelope::conn_open(
            &clk(),
            "hl-ws",
            "hl-ws-01",
            2,
            "wss://api.hyperliquid.xyz/ws",
            2,
        );
        assert_eq!(
            serde_json::to_string(&env).unwrap(),
            r#"{"v":1,"src":"hl-ws","conn":"hl-ws-01","seq":2,"t_ns":1700000000000000000,"mono_ns":987654321098765,"kind":"conn_open","meta":{"attempt":2,"url":"wss://api.hyperliquid.xyz/ws"}}"#
        );
    }

    #[test]
    fn golden_gap_start() {
        let env = Envelope::gap_start(&clk(), "hl-ws", "hl-ws-01", 9, "close", "server closed");
        assert_eq!(
            serde_json::to_string(&env).unwrap(),
            r#"{"v":1,"src":"hl-ws","conn":"hl-ws-01","seq":9,"t_ns":1700000000000000000,"mono_ns":987654321098765,"kind":"gap_start","meta":{"detail":"server closed","reason":"close"}}"#
        );
    }

    #[test]
    fn golden_gap_end() {
        let env = Envelope::gap_end(&clk(), "hl-ws", "hl-ws-01", 10, 1500);
        assert_eq!(
            serde_json::to_string(&env).unwrap(),
            r#"{"v":1,"src":"hl-ws","conn":"hl-ws-01","seq":10,"t_ns":1700000000000000000,"mono_ns":987654321098765,"kind":"gap_end","meta":{"gap_ms":1500}}"#
        );
    }

    #[test]
    fn explicit_gap_timestamps_are_used() {
        let start_ns = 1_700_000_000_000_000_000i64;
        let end_ns = start_ns + 2_000_000_000;
        let start = Envelope::gap_start_at(
            "hl-ws",
            "hl-ws-01",
            4,
            start_ns,
            11,
            "close",
            "server closed",
        );
        let end = Envelope::gap_end_at("hl-ws", "hl-ws-01", 5, end_ns, 22, 2_000);
        assert_eq!(start.t_ns, start_ns);
        assert_eq!(start.mono_ns, 11);
        assert_eq!(start.kind, Kind::GapStart);
        assert_eq!(end.t_ns, end_ns);
        assert_eq!(end.mono_ns, 22);
        assert_eq!(end.kind, Kind::GapEnd);
        assert_eq!(end.meta.unwrap()["gap_ms"], 2_000);
        assert_eq!((end.t_ns - start.t_ns) as u64 / 1_000_000, 2_000);
    }

    #[test]
    fn golden_clock() {
        let env = Envelope::clock(&clk(), "hl-ws", "hl-ws-01", 11, Some(123), Some(2));
        assert_eq!(
            serde_json::to_string(&env).unwrap(),
            r#"{"v":1,"src":"hl-ws","conn":"hl-ws-01","seq":11,"t_ns":1700000000000000000,"mono_ns":987654321098765,"kind":"clock","meta":{"chrony_offset_ns":123,"chrony_stratum":2}}"#
        );
        let env = Envelope::clock(&clk(), "hl-ws", "hl-ws-01", 12, None, None);
        assert_eq!(
            serde_json::to_string(&env).unwrap(),
            r#"{"v":1,"src":"hl-ws","conn":"hl-ws-01","seq":12,"t_ns":1700000000000000000,"mono_ns":987654321098765,"kind":"clock","meta":{"chrony_offset_ns":null,"chrony_stratum":null}}"#
        );
    }

    #[test]
    fn golden_segment_open() {
        let meta = SegmentOpenMeta {
            host: "rec-01".into(),
            git_sha: "abc123".into(),
            profile: "default".into(),
            recorder_version: "0.1.0".into(),
        };
        let env = Envelope::segment_open(&clk(), "hl-ws", "hl-ws-01", 0, &meta);
        assert_eq!(
            serde_json::to_string(&env).unwrap(),
            r#"{"v":1,"src":"hl-ws","conn":"hl-ws-01","seq":0,"t_ns":1700000000000000000,"mono_ns":987654321098765,"kind":"segment_open","meta":{"git_sha":"abc123","host":"rec-01","profile":"default","recorder_version":"0.1.0"}}"#
        );
    }

    #[test]
    fn golden_segment_close() {
        let env = Envelope::segment_close(&clk(), "hl-ws", "hl-ws-01", 42, 42, 8192);
        assert_eq!(
            serde_json::to_string(&env).unwrap(),
            r#"{"v":1,"src":"hl-ws","conn":"hl-ws-01","seq":42,"t_ns":1700000000000000000,"mono_ns":987654321098765,"kind":"segment_close","meta":{"bytes_raw":8192,"records":42}}"#
        );
    }

    #[test]
    fn every_kind_round_trips() {
        let meta = SegmentOpenMeta::default();
        let envelopes = vec![
            Envelope::frame(&clk(), "s", "c", 0, "text"),
            Envelope::frame_bin(&clk(), "s", "c", 1, "AA=="),
            Envelope::rest(&clk(), "s", "c", 2, "{}", json!({ "status": 200 })),
            Envelope::sub(&clk(), "s", "c", 3, json!({ "type": "bbo" })),
            Envelope::conn_open(&clk(), "s", "c", 4, "wss://x", 1),
            Envelope::gap_start(&clk(), "s", "c", 5, "error", "boom"),
            Envelope::gap_end(&clk(), "s", "c", 6, 250),
            Envelope::clock(&clk(), "s", "c", 7, Some(-5), None),
            Envelope::segment_open(&clk(), "s", "c", 8, &meta),
            Envelope::segment_close(&clk(), "s", "c", 9, 9, 100),
        ];
        let kinds: Vec<Kind> = envelopes.iter().map(|e| e.kind).collect();
        for env in envelopes {
            let line = serde_json::to_string(&env).unwrap();
            let back: Envelope = serde_json::from_str(&line).unwrap();
            assert_eq!(env, back);
        }
        assert_eq!(
            kinds,
            vec![
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
            ]
        );
    }

    #[test]
    fn raw_survives_round_trip_byte_exact() {
        let raw = "quote\" backslash\\ newline\n tab\t unicode: ✓ é 日本語 emoji: 😀 \u{7}";
        let env = Envelope::frame(&clk(), "s", "c", 0, raw);
        let line = serde_json::to_string(&env).unwrap();
        assert!(
            !line.contains('\n'),
            "the serialized envelope must stay on one line"
        );
        let back: Envelope = serde_json::from_str(&line).unwrap();
        assert_eq!(back.raw.as_deref(), Some(raw));
    }

    #[test]
    fn non_frame_omits_raw_and_frames_omit_meta() {
        let frame = Envelope::frame(&clk(), "s", "c", 0, "x");
        let json = serde_json::to_value(&frame).unwrap();
        assert!(json.get("raw").is_some());
        assert!(json.get("meta").is_none());

        let open = Envelope::segment_open(&clk(), "s", "c", 0, &SegmentOpenMeta::default());
        let json = serde_json::to_value(&open).unwrap();
        assert!(json.get("raw").is_none());
        assert!(json.get("meta").is_some());
    }

    #[test]
    fn fixed_clock_is_injectable() {
        let clock = FixedEnvelopeClock::new(1, 2);
        clock.set_t_ns(100);
        clock.set_mono_ns(200);
        assert_eq!(clock.t_ns(), 100);
        assert_eq!(clock.mono_ns(), 200);
        clock.advance_ns(5);
        assert_eq!(clock.t_ns(), 105);
        assert_eq!(clock.mono_ns(), 205);
        assert_eq!(hl_arb_core::clock::Clock::now_ms(&clock), 0);
    }
}
