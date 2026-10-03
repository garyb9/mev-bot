//! Tests for the segment reader and the inspect/verify/repair analysis.

use std::fs;
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::envelope::{Envelope, FixedEnvelopeClock, Kind, SCHEMA_VERSION};
use crate::segment::{ManifestEntry, SegmentConfig, SegmentWriter};

use super::*;

fn temp_dir(tag: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("mev-rec-reader-{tag}-"))
        .tempdir()
        .unwrap()
}

fn zstd_frame(lines: &[String]) -> Vec<u8> {
    let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 3).unwrap();
    for line in lines {
        encoder.write_all(line.as_bytes()).unwrap();
        encoder.write_all(b"\n").unwrap();
    }
    encoder.finish().unwrap()
}

fn frames() -> Vec<Envelope> {
    let clock = FixedEnvelopeClock::new(1_700_000_000_000_000_000, 0);
    (0..10)
        .map(|i| Envelope::frame(&clock, "hl-ws", "hl-ws-01", i, format!("payload-{i}")))
        .collect()
}

/// Write a synthetic `.jsonl.zst` (or `.crashed`) segment and its manifest
/// line, so `verify` has something to inspect without a running writer.
#[allow(clippy::too_many_arguments)]
fn write_segment(
    out_dir: &Path,
    src: &str,
    conn: &str,
    date: &str,
    start_t_ns: i64,
    envelopes: &[Envelope],
    crashed: bool,
) {
    let dir = out_dir.join("testnet").join(src).join(date).join("00");
    fs::create_dir_all(&dir).unwrap();
    let ext = if crashed {
        "jsonl.zst.crashed"
    } else {
        "jsonl.zst"
    };
    let path = dir.join(format!("{conn}-{start_t_ns}.{ext}"));
    let lines: Vec<String> = envelopes
        .iter()
        .map(|env| serde_json::to_string(env).unwrap())
        .collect();
    let bytes = zstd_frame(&lines);
    fs::write(&path, &bytes).unwrap();

    let entry = ManifestEntry {
        file: path
            .strip_prefix(out_dir)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/"),
        src: src.into(),
        conn: conn.into(),
        first_t_ns: start_t_ns,
        last_t_ns: envelopes.last().map(|env| env.t_ns).unwrap_or(start_t_ns),
        records: if crashed { 0 } else { envelopes.len() as u64 },
        bytes_raw: 0,
        bytes_zst: bytes.len() as u64,
        crashed,
    };
    let manifest = out_dir
        .join("testnet")
        .join(src)
        .join(date)
        .join("manifest.jsonl");
    fs::create_dir_all(manifest.parent().unwrap()).unwrap();
    let mut text = serde_json::to_string(&entry).unwrap();
    text.push('\n');
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&manifest)
        .unwrap()
        .write_all(text.as_bytes())
        .unwrap();
}

#[test]
fn truncated_crashed_file_reads_to_last_full_line() {
    let tmp = temp_dir("crash");
    let dir = tmp.path();
    let all = frames();
    let lines: Vec<String> = all
        .iter()
        .map(|env| serde_json::to_string(env).unwrap())
        .collect();
    let mut frame_a = zstd_frame(&lines[..6]);
    let frame_b = zstd_frame(&lines[6..]);
    frame_a.extend_from_slice(&frame_b[..frame_b.len() / 2]);
    let path = dir.join("hl-ws-01-0.jsonl.zst.crashed");
    fs::write(&path, &frame_a).unwrap();

    let read = read_envelopes(&path).unwrap();
    assert!(!read.is_empty());
    assert!(read.len() >= 6, "lost complete lines: {}", read.len());
    assert!(read.len() < all.len(), "truncation was not exercised");
    for (a, b) in read.iter().zip(all.iter()) {
        assert_eq!(a, b);
    }
}

#[test]
fn corrupted_finished_segment_is_an_error() {
    let tmp = temp_dir("corrupt");
    let dir = tmp.path();
    let lines: Vec<String> = frames()
        .iter()
        .map(|env| serde_json::to_string(env).unwrap())
        .collect();
    let mut bytes = zstd_frame(&lines);
    // Flip bytes in the middle of the compressed stream. A finished segment
    // must report the damage instead of silently stopping.
    let mid = bytes.len() / 2;
    for byte in &mut bytes[mid..mid + 8] {
        *byte ^= 0xff;
    }
    let path = dir.join("hl-ws-01-0.jsonl.zst");
    fs::write(&path, &bytes).unwrap();

    let result: Result<Vec<Envelope>, ReaderError> = SegmentReader::open(&path).unwrap().collect();
    assert!(result.is_err(), "corruption was decoded without error");
}

struct CountingReader {
    inner: File,
    bytes: Arc<AtomicU64>,
}

impl Read for CountingReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.bytes.fetch_add(n as u64, Ordering::SeqCst);
        Ok(n)
    }
}

fn pseudo_random_hex(seed: u64) -> String {
    let mut x = seed
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    let mut out = String::with_capacity(128);
    for _ in 0..8 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.push_str(&format!("{x:016x}"));
    }
    out
}

#[test]
fn large_file_read_lazily() {
    let tmp = temp_dir("lazy");
    let dir = tmp.path();
    let total = 20_000u64;
    let lines: Vec<String> = (0..total)
        .map(|i| {
            let env = Envelope::frame(&clock_at(), "hl-ws", "hl-ws-01", i, pseudo_random_hex(i));
            serde_json::to_string(&env).unwrap()
        })
        .collect();
    let bytes = zstd_frame(&lines);
    let path = dir.join("hl-ws-01-0.jsonl.zst");
    fs::write(&path, &bytes).unwrap();

    let counted = Arc::new(AtomicU64::new(0));
    let file = File::open(&path).unwrap();
    let reader = CountingReader {
        inner: file,
        bytes: counted.clone(),
    };
    let mut segment = SegmentReader::with_reader(reader, &path, false).unwrap();
    let first = segment.next().unwrap().unwrap();
    assert_eq!(first.seq, 0);

    let read = counted.load(Ordering::SeqCst);
    assert!(
        read < bytes.len() as u64 / 4,
        "one envelope read {read} of {} compressed bytes; the reader is not lazy",
        bytes.len()
    );
}

fn clock_at() -> FixedEnvelopeClock {
    FixedEnvelopeClock::new(1_700_000_000_000_000_000, 0)
}

fn label(kind_env: Envelope) -> String {
    channel_of(&kind_env)
}

fn frame_label_of(raw: &str) -> String {
    label(Envelope::frame(&clock_at(), "s", "c", 0, raw))
}

#[test]
fn channel_of_hyperliquid_channel_wins() {
    assert_eq!(
        frame_label_of(r#"{"channel":"l2Book","data":{}}"#),
        "l2Book"
    );
    assert_eq!(
        frame_label_of(r#"{"channel":"trades","stream":"a@b","topic":"x.y.z"}"#),
        "trades"
    );
}

#[test]
fn channel_of_binance_stream() {
    assert_eq!(
        frame_label_of(r#"{"stream":"btcusdt@bookTicker","data":{}}"#),
        "bookTicker"
    );
    assert_eq!(frame_label_of(r#"{"stream":"plain"}"#), "plain");
    assert_eq!(frame_label_of(r#"{"stream":"a@b@depth20"}"#), "depth20");
}

#[test]
fn channel_of_bybit_topic() {
    assert_eq!(
        frame_label_of(r#"{"topic":"orderbook.1.BTCUSDT","type":"snapshot"}"#),
        "orderbook.1"
    );
    assert_eq!(frame_label_of(r#"{"topic":"tickers"}"#), "tickers");
}

#[test]
fn channel_of_unlabelled_frames_are_unknown() {
    for raw in [
        "not json",
        r#"{"data":1}"#,
        r#"{"stream":5}"#,
        r#"{"topic":["a.b"]}"#,
        r#"{"success":true,"ret_msg":"pong","conn_id":"x","op":"ping"}"#,
    ] {
        assert_eq!(frame_label_of(raw), "unknown", "{raw}");
    }
    let bin = Envelope::frame_bin(&clock_at(), "s", "c", 0, "AAECAw==");
    assert_eq!(channel_of(&bin), "unknown");
}

#[test]
fn channel_of_non_frames_use_kind_name() {
    let gap = Envelope::gap_start(&clock_at(), "s", "c", 0, "close", "bye");
    assert_eq!(channel_of(&gap), "gap_start");
}

#[test]
fn holes_spanning_a_restart_are_found() {
    let tmp = temp_dir("restart-holes");
    let dir = tmp.path();
    let clock = clock_at();
    // Run A: seq 0,1,2. Run B (after a restart): seq 0,1,3. A naive union
    // over all seqs would see {0,1,2,3} and miss the hole in run B.
    let envelopes = [
        Envelope::frame(&clock, "hl-ws", "hl-ws-01", 0, "a"),
        Envelope::frame(&clock, "hl-ws", "hl-ws-01", 1, "b"),
        Envelope::frame(&clock, "hl-ws", "hl-ws-01", 2, "c"),
        Envelope::frame(&clock, "hl-ws", "hl-ws-01", 0, "d"),
        Envelope::frame(&clock, "hl-ws", "hl-ws-01", 1, "e"),
        Envelope::frame(&clock, "hl-ws", "hl-ws-01", 3, "f"),
    ];
    let lines: Vec<String> = envelopes
        .iter()
        .map(|env| serde_json::to_string(env).unwrap())
        .collect();
    let path = dir.join("s.jsonl.zst");
    fs::write(&path, zstd_frame(&lines)).unwrap();

    let report = inspect(&[path]).unwrap();
    assert_eq!(report.seq_holes.len(), 1);
    assert_eq!(report.seq_holes[0].runs, 2);
    assert_eq!(
        report.seq_holes[0].missing.ranges(),
        &[SeqRange { start: 2, end: 2 }]
    );
}

#[test]
fn merge_orders_by_time_conn_seq() {
    let tmp = temp_dir("merge");
    let dir = tmp.path();
    let clock = FixedEnvelopeClock::new(1_000, 0);
    // A real segment file holds one (src, conn) and is time-ordered, which
    // is the invariant the k-way merge relies on.
    clock.set_t_ns(1_000);
    let a0 = Envelope::frame(&clock, "hl-ws", "hl-ws-01", 0, "a0");
    clock.set_t_ns(2_000);
    let a1 = Envelope::frame(&clock, "hl-ws", "hl-ws-01", 1, "a1");
    clock.set_t_ns(1_000);
    let b0 = Envelope::frame(&clock, "hl-ws", "hl-ws-02", 0, "b0");

    let write = |name: &str, envelopes: &[Envelope]| {
        let path = dir.join(name);
        let lines: Vec<String> = envelopes
            .iter()
            .map(|env| serde_json::to_string(env).unwrap())
            .collect();
        fs::write(&path, zstd_frame(&lines)).unwrap();
        path
    };
    let path_a = write("a.jsonl.zst", &[a0, a1]);
    let path_b = write("b.jsonl.zst", &[b0]);

    let merged = merge_segments(&[path_b.clone(), path_a.clone()]).unwrap();
    let keys: Vec<(i64, &str, u64)> = merged
        .iter()
        .map(|e| (e.t_ns, e.conn.as_str(), e.seq))
        .collect();
    assert_eq!(
        keys,
        vec![
            (1_000, "hl-ws-01", 0),
            (1_000, "hl-ws-02", 0),
            (2_000, "hl-ws-01", 1),
        ]
    );

    // The lazy iterator yields the same order.
    let lazy: Vec<Envelope> = merge_segments_iter(&[path_b, path_a])
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(lazy, merged);
}

#[test]
fn inspect_detects_seq_holes_and_gaps() {
    let tmp = temp_dir("inspect");
    let dir = tmp.path();
    let clock = FixedEnvelopeClock::new(1_000, 0);
    let envelopes = vec![
        Envelope::frame(&clock, "hl-ws", "hl-ws-01", 0, "x"),
        Envelope::frame(&clock, "hl-ws", "hl-ws-01", 1, "y"),
        Envelope::gap_start(&clock, "hl-ws", "hl-ws-01", 2, "close", "bye"),
        Envelope::frame(&clock, "hl-ws", "hl-ws-01", 4, "z"),
    ];
    let clock = FixedEnvelopeClock::new(1_000, 0);
    let mut path_envelopes = envelopes.clone();
    clock.set_t_ns(1_500_001_000);
    path_envelopes.push(Envelope::gap_end(&clock, "hl-ws", "hl-ws-01", 5, 1_500));
    let path = dir.join("s.jsonl.zst");
    fs::write(
        &path,
        zstd_frame(
            &path_envelopes
                .iter()
                .map(|e| serde_json::to_string(e).unwrap())
                .collect::<Vec<_>>(),
        ),
    )
    .unwrap();

    let report = inspect(&[path]).unwrap();
    assert_eq!(report.records, 5);
    assert_eq!(report.gap_count, 1);
    assert_eq!(report.gap_total_ms, 1_500);
    assert_eq!(report.seq_holes.len(), 1);
    assert_eq!(report.seq_holes[0].runs, 1);
    assert_eq!(
        report.seq_holes[0].missing.ranges(),
        &[SeqRange { start: 3, end: 3 }]
    );
    assert_eq!(report.seq_holes[0].missing.len(), 1);
    assert_eq!(report.by_src.get("hl-ws"), Some(&5));
    assert_eq!(report.by_kind.get("frame"), Some(&3));
}

#[test]
fn verify_checks_manifest_against_disk() {
    let tmp = temp_dir("verify");
    let dir = tmp.path();
    let base = 1_767_227_400_000_000_000i64;
    let clock = Arc::new(FixedEnvelopeClock::new(base, 0));
    let config = SegmentConfig {
        out_dir: dir.to_path_buf(),
        network: "testnet".into(),
        src: "hl-ws".into(),
        conn: "hl-ws-01".into(),
        clock: clock.clone(),
        ..SegmentConfig::default()
    };
    let writer = SegmentWriter::spawn(config).unwrap();
    for i in 0..20u64 {
        clock.set_t_ns(base + i as i64 * 1_000_000);
        assert!(writer.try_send(Envelope::frame(&*clock, "hl-ws", "hl-ws-01", i, "data")));
    }
    // A `gap_start` is stamped at the disconnect instant, which is earlier
    // than the last frame's stamp (the frame was stamped when processed).
    // The writer must accept the out-of-order `t_ns`, and verify must
    // subtract the whole outage from coverage.
    let gap_start_ns = base + 5_000_000;
    let gap_end_ns = base + 30_000_000;
    assert!(
        gap_start_ns < base + 19_000_000,
        "precondition: the gap must predate the last frame"
    );
    assert!(writer.try_send(Envelope::gap_start_at(
        "hl-ws",
        "hl-ws-01",
        20,
        gap_start_ns,
        0,
        "closed",
        "server closed",
    )));
    assert!(writer.try_send(Envelope::gap_end_at(
        "hl-ws", "hl-ws-01", 21, gap_end_ns, 0, 25,
    )));
    writer.shutdown().unwrap();

    let report = verify(&VerifyConfig {
        out_dir: dir.to_path_buf(),
        network: "testnet".into(),
        date: "2026-01-01".into(),
    })
    .unwrap();
    assert_eq!(report.files.len(), 1);
    let file = &report.files[0];
    assert!(file.exists);
    assert!(file.size_ok);
    assert!(file.records_ok);
    assert!(!file.crashed);
    assert_eq!(file.records_manifest, 24);
    assert_eq!(report.coverage.len(), 1);
    // Only the 5 ms before the outage are covered.
    assert_eq!(report.coverage[0].covered_ms, 5);
    assert!(
        report.coverage[0].coverage_pct < 100.0,
        "the outage was counted as covered"
    );

    // `inspect` reports the recorded gap against the real timestamps.
    let files = segments_for(dir, "testnet", "2026-01-01", "2026-01-01").unwrap();
    assert_eq!(files.len(), 1, "{files:?}");
    let inspected = inspect(&files).unwrap();
    assert_eq!(inspected.gap_count, 1);
    assert_eq!(inspected.gap_total_ms, 25);
}

/// A shutdown during an open gap emits a second `gap_start` with no
/// `gap_end`. The reader must close the first gap at the shutdown, so
/// `[disconnect, shutdown)` stays a gap, matching the Python
/// `_GapTracker._open` (SPEC-0008 RW-3).
#[test]
fn verify_keeps_a_shutdown_gap_closed_at_the_shutdown() {
    let tmp = temp_dir("coverage-shutdown");
    let dir = tmp.path();
    let base = 1_767_227_400_000_000_000i64; // 2026-01-01T00:30:00Z
    // A 40 s segment: a frame at the start, a `closed` gap at +20 s, then a
    // `shutdown` gap at +40 s with no `gap_end`.
    let envelopes = vec![
        Envelope::frame(
            &FixedEnvelopeClock::new(base, 0),
            "hl-ws",
            "hl-ws-01",
            0,
            "a",
        ),
        Envelope::gap_start_at(
            "hl-ws",
            "hl-ws-01",
            1,
            base + 20_000_000_000,
            0,
            "closed",
            "server closed",
        ),
        Envelope::gap_start_at(
            "hl-ws",
            "hl-ws-01",
            2,
            base + 40_000_000_000,
            0,
            "shutdown",
            "shutdown requested",
        ),
    ];
    write_segment(
        dir,
        "hl-ws",
        "hl-ws-01",
        "2026-01-01",
        base,
        &envelopes,
        false,
    );

    let report = verify(&VerifyConfig {
        out_dir: dir.to_path_buf(),
        network: "testnet".into(),
        date: "2026-01-01".into(),
    })
    .unwrap();
    assert_eq!(report.coverage.len(), 1);
    // Only [base, base+20 s) is covered; the whole [20 s, 40 s) is a gap.
    assert_eq!(
        report.coverage[0].covered_ms, 20_000,
        "the [disconnect, shutdown) window was counted as covered"
    );
}

/// A run that ends with an unpaired `shutdown` gap leaves an open gap that
/// must be closed at the first frame of the next run (Python
/// `_GapTracker.observe`), not at the end of the day. The whole downtime is
/// a gap, but every frame of the restarted run is covered.
#[test]
fn restart_after_shutdown_counts_only_the_downtime_as_a_gap() {
    let tmp = temp_dir("coverage-restart");
    let dir = tmp.path();
    let base = 1_767_227_400_000_000_000i64; // 2026-01-01T00:30:00Z
    let frame_at = |t_ns: i64, seq: u64| {
        Envelope::new_at(
            "hl-ws",
            "hl-ws-01",
            seq,
            Kind::Frame,
            t_ns,
            0,
            Some("data".into()),
            None,
        )
    };
    let run1 = vec![
        frame_at(base, 0),
        Envelope::gap_start_at(
            "hl-ws",
            "hl-ws-01",
            1,
            base + 10_000_000_000,
            0,
            "shutdown",
            "shutdown requested",
        ),
    ];
    let run2 = vec![
        frame_at(base + 20_000_000_000, 0),
        frame_at(base + 30_000_000_000, 1),
    ];
    write_segment(dir, "hl-ws", "hl-ws-01", "2026-01-01", base, &run1, false);
    write_segment(
        dir,
        "hl-ws",
        "hl-ws-01",
        "2026-01-01",
        base + 20_000_000_000,
        &run2,
        false,
    );

    let report = verify(&VerifyConfig {
        out_dir: dir.to_path_buf(),
        network: "testnet".into(),
        date: "2026-01-01".into(),
    })
    .unwrap();
    assert_eq!(report.coverage.len(), 1);
    // 10 s from run 1 + the full 10 s of run 2; only (T1, T2) is a gap.
    assert_eq!(
        report.coverage[0].covered_ms, 20_000,
        "run 2 was counted as a gap after a shutdown restart"
    );
}

/// The real outage in the second run is still a gap; the unpaired shutdown
/// gap from the first run is closed at the restart and does not swallow the
/// second run.
#[test]
fn restart_after_shutdown_keeps_later_outages_as_gaps() {
    let tmp = temp_dir("coverage-restart-outage");
    let dir = tmp.path();
    let base = 1_767_227_400_000_000_000i64; // 2026-01-01T00:30:00Z
    let frame_at = |t_ns: i64, seq: u64| {
        Envelope::new_at(
            "hl-ws",
            "hl-ws-01",
            seq,
            Kind::Frame,
            t_ns,
            0,
            Some("data".into()),
            None,
        )
    };
    let run1 = vec![
        frame_at(base, 0),
        Envelope::gap_start_at(
            "hl-ws",
            "hl-ws-01",
            1,
            base + 10_000_000_000,
            0,
            "shutdown",
            "shutdown requested",
        ),
    ];
    let run2 = vec![
        frame_at(base + 20_000_000_000, 0),
        frame_at(base + 30_000_000_000, 1),
        Envelope::gap_start_at(
            "hl-ws",
            "hl-ws-01",
            2,
            base + 40_000_000_000,
            0,
            "close",
            "server closed",
        ),
        Envelope::gap_end_at("hl-ws", "hl-ws-01", 3, base + 50_000_000_000, 0, 10_000),
        frame_at(base + 60_000_000_000, 4),
    ];
    write_segment(dir, "hl-ws", "hl-ws-01", "2026-01-01", base, &run1, false);
    write_segment(
        dir,
        "hl-ws",
        "hl-ws-01",
        "2026-01-01",
        base + 20_000_000_000,
        &run2,
        false,
    );

    let report = verify(&VerifyConfig {
        out_dir: dir.to_path_buf(),
        network: "testnet".into(),
        date: "2026-01-01".into(),
    })
    .unwrap();
    assert_eq!(report.coverage.len(), 1);
    // 10 s (run 1) + 20 s before the outage + 10 s after it.
    assert_eq!(report.coverage[0].covered_ms, 40_000);
}

/// An unpaired gap at the very end of the data, with no later frame to
/// close it, is still closed at the last record (`max_t_ns`), the SPEC-0008
/// §17 conservative reading. The frames before it stay covered.
#[test]
fn final_unpaired_shutdown_gap_closes_at_last_record() {
    let tmp = temp_dir("coverage-restart-final");
    let dir = tmp.path();
    let base = 1_767_227_400_000_000_000i64; // 2026-01-01T00:30:00Z
    let frame_at = |t_ns: i64, seq: u64| {
        Envelope::new_at(
            "hl-ws",
            "hl-ws-01",
            seq,
            Kind::Frame,
            t_ns,
            0,
            Some("data".into()),
            None,
        )
    };
    let run1 = vec![
        frame_at(base, 0),
        Envelope::gap_start_at(
            "hl-ws",
            "hl-ws-01",
            1,
            base + 10_000_000_000,
            0,
            "shutdown",
            "shutdown requested",
        ),
    ];
    let run2 = vec![
        frame_at(base + 20_000_000_000, 0),
        frame_at(base + 40_000_000_000, 1),
        Envelope::gap_start_at(
            "hl-ws",
            "hl-ws-01",
            2,
            base + 50_000_000_000,
            0,
            "shutdown",
            "shutdown requested",
        ),
    ];
    write_segment(dir, "hl-ws", "hl-ws-01", "2026-01-01", base, &run1, false);
    write_segment(
        dir,
        "hl-ws",
        "hl-ws-01",
        "2026-01-01",
        base + 20_000_000_000,
        &run2,
        false,
    );

    let report = verify(&VerifyConfig {
        out_dir: dir.to_path_buf(),
        network: "testnet".into(),
        date: "2026-01-01".into(),
    })
    .unwrap();
    assert_eq!(report.coverage.len(), 1);
    // 10 s (run 1) + 30 s of run 2; the final gap is zero-length at the last
    // record, so it removes nothing.
    assert_eq!(report.coverage[0].covered_ms, 40_000);
}

/// A `gap_end` with no open gap is ignored, matching the Python
/// `_GapTracker._close` (which returns early when nothing is open).
#[test]
fn gap_end_without_gap_start_is_ignored() {
    let mut acc = CoverageAcc::default();
    acc.start_segment();
    acc.observe(&Envelope::gap_end_at(
        "hl-ws",
        "hl-ws-01",
        0,
        5_000_000_000,
        0,
        0,
    ));
    acc.end_segment();
    assert!(acc.gaps.is_empty());
}

#[test]
fn coverage_shows_crash_and_late_restart_as_missing() {
    let tmp = temp_dir("coverage-crash");
    let dir = tmp.path();
    let base = 1_767_227_400_000_000_000i64; // 2026-01-01T00:30:00Z
    let clock = FixedEnvelopeClock::new(base, 0);

    // A crashed segment covering 60 s, then a clean segment starting 3 h
    // later. The 3 h in between must not count as covered.
    clock.set_t_ns(base);
    let crashed_first = Envelope::frame(&clock, "hl-ws", "hl-ws-01", 0, "a");
    clock.set_t_ns(base + 60_000_000_000);
    let crashed_last = Envelope::frame(&clock, "hl-ws", "hl-ws-01", 1, "b");

    let restart = base + 3 * 3_600_000_000_000;
    clock.set_t_ns(restart);
    let clean_first = Envelope::frame(&clock, "hl-ws", "hl-ws-01", 0, "c");
    clock.set_t_ns(restart + 60_000_000_000);
    let clean_last = Envelope::frame(&clock, "hl-ws", "hl-ws-01", 1, "d");

    write_segment(
        dir,
        "hl-ws",
        "hl-ws-01",
        "2026-01-01",
        base,
        &[crashed_first, crashed_last],
        true,
    );
    write_segment(
        dir,
        "hl-ws",
        "hl-ws-01",
        "2026-01-01",
        restart,
        &[clean_first, clean_last],
        false,
    );

    let report = verify(&VerifyConfig {
        out_dir: dir.to_path_buf(),
        network: "testnet".into(),
        date: "2026-01-01".into(),
    })
    .unwrap();
    assert_eq!(report.coverage.len(), 1);
    // 60 s before the crash + 60 s after the restart, not the whole 4 minutes.
    assert_eq!(report.coverage[0].covered_ms, 120_000);
    assert!(
        report.coverage[0].coverage_pct < 1.0,
        "outage was counted as covered"
    );
}

#[test]
fn back_to_back_clean_segments_show_full_coverage() {
    let tmp = temp_dir("coverage-full");
    let dir = tmp.path();
    let (day_start, day_end) = day_bounds("2026-01-01").unwrap();
    let mid = day_start + 43_200_000_000_000; // 12:00:00Z
    let clock = FixedEnvelopeClock::new(day_start, 0);

    clock.set_t_ns(day_start);
    let a_first = Envelope::frame(&clock, "hl-ws", "hl-ws-01", 0, "a");
    clock.set_t_ns(mid);
    let a_last = Envelope::frame(&clock, "hl-ws", "hl-ws-01", 1, "b");
    clock.set_t_ns(mid);
    let b_first = Envelope::frame(&clock, "hl-ws", "hl-ws-01", 0, "c");
    clock.set_t_ns(day_end);
    let b_last = Envelope::frame(&clock, "hl-ws", "hl-ws-01", 1, "d");

    write_segment(
        dir,
        "hl-ws",
        "hl-ws-01",
        "2026-01-01",
        day_start,
        &[a_first, a_last],
        false,
    );
    write_segment(
        dir,
        "hl-ws",
        "hl-ws-01",
        "2026-01-01",
        mid,
        &[b_first, b_last],
        false,
    );

    let report = verify(&VerifyConfig {
        out_dir: dir.to_path_buf(),
        network: "testnet".into(),
        date: "2026-01-01".into(),
    })
    .unwrap();
    assert_eq!(report.coverage.len(), 1);
    assert_eq!(report.coverage[0].covered_ms, report.coverage[0].day_ms);
    assert_eq!(report.coverage[0].coverage_pct, 100.0);
}

#[test]
fn segments_for_enumerates_a_date_range_and_skips_manifests() {
    let tmp = temp_dir("segments-for");
    let dir = tmp.path();
    let clock = FixedEnvelopeClock::new(1_700_000_000_000_000_000, 0);
    let one = |n: u64| vec![Envelope::frame(&clock, "hl-ws", "hl-ws-01", n, "x")];
    write_segment(dir, "hl-ws", "hl-ws-01", "2026-01-01", 0, &one(0), false);
    write_segment(dir, "hl-ws", "hl-ws-01", "2026-01-02", 0, &one(1), true);
    write_segment(
        dir,
        "binance-usdm",
        "binance-usdm-01",
        "2026-01-03",
        0,
        &one(2),
        false,
    );
    write_segment(dir, "hl-ws", "hl-ws-01", "2026-02-01", 0, &one(3), false);

    let got = segments_for(dir, "testnet", "2026-01-01", "2026-01-02").unwrap();
    assert_eq!(got.len(), 2, "{got:?}");
    assert!(
        got.iter()
            .all(|path| !path.to_string_lossy().contains("manifest"))
    );
    assert!(
        got.iter()
            .any(|path| path.to_string_lossy().ends_with(".crashed"))
    );

    let wide = segments_for(dir, "testnet", "2026-01-01", "2026-02-01").unwrap();
    assert_eq!(wide.len(), 4, "{wide:?}");

    let empty_tmp = temp_dir("segments-for-empty");
    let empty = empty_tmp.path();
    assert!(
        segments_for(empty, "testnet", "2026-01-01", "2026-01-01")
            .unwrap()
            .is_empty()
    );
    assert!(segments_for(dir, "testnet", "2026-13-01", "2026-01-02").is_err());
}

#[test]
fn verify_rejects_bad_date() {
    let tmp = temp_dir("date");
    let err = verify(&VerifyConfig {
        out_dir: tmp.path().to_path_buf(),
        network: "testnet".into(),
        date: "2026-13-40".into(),
    })
    .unwrap_err();
    assert!(matches!(err, ReaderError::InvalidDate(_)));
}

#[test]
fn day_bounds_are_utc_midnight() {
    let (start, end) = day_bounds("1970-01-01").unwrap();
    assert_eq!(start, 0);
    assert_eq!(end, 86_400_000_000_000);
}

/// Build a single finalized segment with the real `SegmentWriter` so its
/// manifest line is exactly what the writer would emit.
fn write_finalized_segment(dir: &Path, frames: u64, base: i64) -> PathBuf {
    let clock = Arc::new(FixedEnvelopeClock::new(base, 0));
    let config = SegmentConfig {
        out_dir: dir.to_path_buf(),
        network: "testnet".into(),
        src: "hl-ws".into(),
        conn: "hl-ws-01".into(),
        clock: clock.clone(),
        ..SegmentConfig::default()
    };
    let writer = SegmentWriter::spawn(config).unwrap();
    for i in 0..frames {
        clock.set_t_ns(base + i as i64 * 1_000_000);
        assert!(writer.try_send(Envelope::frame(&*clock, "hl-ws", "hl-ws-01", i, "data")));
    }
    writer.shutdown().unwrap();
    dir.join("testnet/hl-ws/2026-01-01/manifest.jsonl")
}

/// A finished segment with no manifest line is an orphan: `verify` reports
/// it, still counts its records in coverage, and leaves the manifest check
/// empty (SPEC-0008 §17 #36).
#[test]
fn verify_flags_an_orphan_and_counts_its_records() {
    let tmp = temp_dir("verify-orphan");
    let dir = tmp.path();
    let base = 1_767_227_400_000_000_000i64; // 2026-01-01T00:30:00Z
    let manifest = write_finalized_segment(dir, 10, base);
    fs::remove_file(&manifest).unwrap();

    let report = verify(&VerifyConfig {
        out_dir: dir.to_path_buf(),
        network: "testnet".into(),
        date: "2026-01-01".into(),
    })
    .unwrap();
    assert!(report.files.is_empty(), "no manifest line must be checked");
    assert_eq!(report.orphans.len(), 1);
    assert_eq!(report.orphans[0].records, 12); // open + 10 frames + close
    assert_eq!(report.orphans[0].src, "hl-ws");
    assert_eq!(report.orphans[0].conn, "hl-ws-01");
    assert!(
        report.orphans[0].file.ends_with(".jsonl.zst"),
        "{}",
        report.orphans[0].file
    );
    // The orphan's own records still count: 10 ms of frames, minus the
    // one-segment interval's exclusive end bound (9 ms inclusive span).
    assert_eq!(report.coverage.len(), 1);
    assert_eq!(report.coverage[0].covered_ms, 9);
}

/// `repair_manifest` appends exactly the line the writer would have written,
/// and a second run is a no-op.
#[test]
fn repair_appends_the_writer_line_and_is_idempotent() {
    let tmp = temp_dir("repair");
    let dir = tmp.path();
    let manifest = write_finalized_segment(dir, 20, 1_767_227_400_000_000_000);
    let original = fs::read(&manifest).unwrap();
    fs::remove_file(&manifest).unwrap();

    let config = RepairConfig {
        out_dir: dir.to_path_buf(),
        network: "testnet".into(),
        date: "2026-01-01".into(),
        dry_run: false,
    };
    let actions = repair_manifest(&config).unwrap();
    assert_eq!(actions.len(), 1, "{actions:?}");
    assert!(matches!(actions[0], RepairAction::Append { .. }));
    assert_eq!(
        fs::read(&manifest).unwrap(),
        original,
        "the reconstructed line differs from the writer's"
    );

    // Idempotent: the repaired line is seen and nothing is appended.
    let again = repair_manifest(&config).unwrap();
    assert!(again.is_empty(), "{again:?}");
    assert_eq!(fs::read(&manifest).unwrap(), original);

    // And `verify` now passes.
    let report = verify(&VerifyConfig {
        out_dir: dir.to_path_buf(),
        network: "testnet".into(),
        date: "2026-01-01".into(),
    })
    .unwrap();
    assert!(report.orphans.is_empty());
    assert_eq!(report.files.len(), 1);
    assert!(report.files[0].records_ok && report.files[0].size_ok);
}

/// `--dry-run` reports the line it would append but writes nothing.
#[test]
fn repair_dry_run_writes_nothing() {
    let tmp = temp_dir("repair-dry");
    let dir = tmp.path();
    let manifest = write_finalized_segment(dir, 5, 1_767_227_400_000_000_000);
    fs::remove_file(&manifest).unwrap();

    let actions = repair_manifest(&RepairConfig {
        out_dir: dir.to_path_buf(),
        network: "testnet".into(),
        date: "2026-01-01".into(),
        dry_run: true,
    })
    .unwrap();
    assert_eq!(actions.len(), 1);
    assert!(matches!(actions[0], RepairAction::Append { .. }));
    assert!(!manifest.exists(), "dry run created the manifest");
}

/// A segment without `segment_close`, and a `.crashed` segment, are reported
/// and left alone (never repaired).
#[test]
fn repair_reports_unfinalized_and_crashed_segments() {
    let tmp = temp_dir("repair-skip");
    let dir = tmp.path();
    let base = 1_767_227_400_000_000_000i64;
    let clock = FixedEnvelopeClock::new(base, 0);
    let env = Envelope::frame(&clock, "hl-ws", "hl-ws-01", 0, "data");
    // A `.jsonl.zst` with no `segment_close` and a `.crashed` file; both get
    // a manifest line from `write_segment`, which we then delete.
    write_segment(
        dir,
        "hl-ws",
        "hl-ws-01",
        "2026-01-01",
        base,
        std::slice::from_ref(&env),
        false,
    );
    write_segment(
        dir,
        "hl-ws",
        "hl-ws-01",
        "2026-01-01",
        base + 10_000_000_000,
        std::slice::from_ref(&env),
        true,
    );
    let manifest = dir.join("testnet/hl-ws/2026-01-01/manifest.jsonl");
    fs::remove_file(&manifest).unwrap();

    let actions = repair_manifest(&RepairConfig {
        out_dir: dir.to_path_buf(),
        network: "testnet".into(),
        date: "2026-01-01".into(),
        dry_run: false,
    })
    .unwrap();
    assert_eq!(actions.len(), 2, "{actions:?}");
    assert!(
        actions
            .iter()
            .all(|action| matches!(action, RepairAction::Skip { .. })),
        "an unfinalized/crashed segment must not be repaired: {actions:?}"
    );
    assert!(!manifest.exists(), "skipped segments created a manifest");
}

/// A corrupted finished segment cannot be reconstructed from: `repair`
/// reports it instead of inventing a line.
#[test]
fn repair_reports_a_corrupt_finished_segment() {
    let tmp = temp_dir("repair-corrupt");
    let dir = tmp.path();
    let manifest = write_finalized_segment(dir, 3, 1_767_227_400_000_000_000);
    fs::remove_file(&manifest).unwrap();

    // Flip bytes in the middle of the compressed stream.
    let segment = segments_for(dir, "testnet", "2026-01-01", "2026-01-01")
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    let mut bytes = fs::read(&segment).unwrap();
    let mid = bytes.len() / 2;
    for byte in &mut bytes[mid..mid + 8] {
        *byte ^= 0xff;
    }
    fs::write(&segment, &bytes).unwrap();

    let actions = repair_manifest(&RepairConfig {
        out_dir: dir.to_path_buf(),
        network: "testnet".into(),
        date: "2026-01-01".into(),
        dry_run: false,
    })
    .unwrap();
    assert_eq!(actions.len(), 1, "{actions:?}");
    assert!(matches!(actions[0], RepairAction::Skip { .. }));
    assert!(!manifest.exists());
}

fn spawn_writer(
    dir: &Path,
    src: &str,
    conn: &str,
    base: i64,
) -> (SegmentWriter, Arc<FixedEnvelopeClock>) {
    let clock = Arc::new(FixedEnvelopeClock::new(base, 0));
    let config = SegmentConfig {
        out_dir: dir.to_path_buf(),
        network: "testnet".into(),
        src: src.into(),
        conn: conn.into(),
        clock: clock.clone(),
        ..SegmentConfig::default()
    };
    (SegmentWriter::spawn(config).unwrap(), clock)
}

/// A real (non-dry) `repair_manifest` refuses while any `*.partial` exists,
/// writes nothing, and succeeds once the partial is gone; `--dry-run` is
/// safe even then (SPEC-0008 §17 #36, review MV-1b).
#[test]
fn repair_refuses_when_a_partial_is_present() {
    let tmp = temp_dir("repair-partial");
    let dir = tmp.path();
    let manifest = write_finalized_segment(dir, 5, 1_767_227_400_000_000_000);
    fs::remove_file(&manifest).unwrap();

    let hour_dir = dir.join("testnet/hl-ws/2026-01-01/00");
    let partial = hour_dir.join("hl-ws-01-123.jsonl.zst.partial");
    fs::write(&partial, b"mid-segment bytes").unwrap();

    let real = RepairConfig {
        out_dir: dir.to_path_buf(),
        network: "testnet".into(),
        date: "2026-01-01".into(),
        dry_run: false,
    };
    let err = repair_manifest(&real).unwrap_err();
    match &err {
        ReaderError::PartialPresent { path } => {
            assert!(path.ends_with("hl-ws-01-123.jsonl.zst.partial"), "{path}");
        }
        other => panic!("expected PartialPresent, got {other:?}"),
    }
    assert!(!manifest.exists(), "a refused repair changed the manifest");

    // Dry-run is allowed with the partial present and still writes nothing.
    let dry = RepairConfig {
        dry_run: true,
        ..real.clone()
    };
    let actions = repair_manifest(&dry).unwrap();
    assert_eq!(actions.len(), 1, "{actions:?}");
    assert!(!manifest.exists(), "dry-run created the manifest");

    // Once the partial is gone the real repair proceeds.
    fs::remove_file(&partial).unwrap();
    let actions = repair_manifest(&real).unwrap();
    assert_eq!(actions.len(), 1, "{actions:?}");
    assert!(manifest.exists());
}

/// `verify` reports any `*.partial` files so the caller can downgrade orphan
/// findings to "possibly in flight".
#[test]
fn verify_reports_partials() {
    let tmp = temp_dir("verify-partial");
    let dir = tmp.path();
    let manifest = write_finalized_segment(dir, 3, 1_767_227_400_000_000_000);
    fs::remove_file(&manifest).unwrap();
    fs::write(
        dir.join("testnet/hl-ws/2026-01-01/00/hl-ws-01-77.jsonl.zst.partial"),
        b"mid-segment bytes",
    )
    .unwrap();

    let report = verify(&VerifyConfig {
        out_dir: dir.to_path_buf(),
        network: "testnet".into(),
        date: "2026-01-01".into(),
    })
    .unwrap();
    assert_eq!(report.partials.len(), 1, "{:?}", report.partials);
    assert_eq!(report.orphans.len(), 1);
}

/// A missing recorder root is an explicit error for both analyses, and
/// neither creates anything.
#[test]
fn absent_root_errors_and_creates_nothing() {
    let tmp = temp_dir("absent-root");
    let missing = tmp.path().join("does-not-exist");

    let verify_err = verify(&VerifyConfig {
        out_dir: missing.clone(),
        network: "testnet".into(),
        date: "2026-01-01".into(),
    })
    .unwrap_err();
    assert!(
        matches!(verify_err, ReaderError::MissingRoot(_)),
        "{verify_err:?}"
    );

    let repair_err = repair_manifest(&RepairConfig {
        out_dir: missing.clone(),
        network: "testnet".into(),
        date: "2026-01-01".into(),
        dry_run: true,
    })
    .unwrap_err();
    assert!(
        matches!(repair_err, ReaderError::MissingRoot(_)),
        "{repair_err:?}"
    );

    assert!(!missing.exists(), "an absent root was created");

    // An existing out_dir with no network sub-tree is also a missing root,
    // not a silent "no data" success.
    let net_err = verify(&VerifyConfig {
        out_dir: tmp.path().to_path_buf(),
        network: "testnet".into(),
        date: "2026-01-01".into(),
    })
    .unwrap_err();
    assert!(
        matches!(net_err, ReaderError::MissingRoot(_)),
        "{net_err:?}"
    );
}

/// `.crashed` and unfinalized orphans are classified apart from repairable
/// finished orphans; only the latter is `orphans` (SPEC-0008 §17 #36).
#[test]
fn verify_classifies_crashed_and_unfinalized() {
    let tmp = temp_dir("verify-classes");
    let dir = tmp.path();
    let base = 1_767_227_400_000_000_000i64;
    let clock = FixedEnvelopeClock::new(base, 0);
    let env = Envelope::frame(&clock, "hl-ws", "hl-ws-01", 0, "data");
    // A `.jsonl.zst` with no `segment_close`, and a `.crashed` file; both get
    // a manifest line from `write_segment`, which we then delete.
    write_segment(
        dir,
        "hl-ws",
        "hl-ws-01",
        "2026-01-01",
        base,
        std::slice::from_ref(&env),
        false,
    );
    write_segment(
        dir,
        "hl-ws",
        "hl-ws-01",
        "2026-01-01",
        base + 10_000_000_000,
        std::slice::from_ref(&env),
        true,
    );
    fs::remove_file(dir.join("testnet/hl-ws/2026-01-01/manifest.jsonl")).unwrap();

    let report = verify(&VerifyConfig {
        out_dir: dir.to_path_buf(),
        network: "testnet".into(),
        date: "2026-01-01".into(),
    })
    .unwrap();
    assert!(report.orphans.is_empty(), "{:?}", report.orphans);
    assert!(
        report.corrupt_orphans.is_empty(),
        "{:?}",
        report.corrupt_orphans
    );
    assert_eq!(report.crashed_no_manifest.len(), 1, "{report:?}");
    assert_eq!(report.unfinalized.len(), 1, "{report:?}");
}

/// A corrupt orphan is listed (path + error) and must not abort the rest of
/// the report: the manifest-backed segment is still verified.
#[test]
fn verify_lists_corrupt_orphans_and_keeps_going() {
    let tmp = temp_dir("verify-corrupt-orphan");
    let dir = tmp.path();
    // One healthy finalized segment, with its manifest line.
    write_finalized_segment(dir, 4, 1_767_227_400_000_000_000);
    // A corrupt `.jsonl.zst` with no manifest line.
    let bad = dir.join("testnet/hl-ws/2026-01-01/00/hl-ws-01-9999.jsonl.zst");
    fs::write(&bad, b"this is not a zstd stream at all, really it is not").unwrap();

    let report = verify(&VerifyConfig {
        out_dir: dir.to_path_buf(),
        network: "testnet".into(),
        date: "2026-01-01".into(),
    })
    .unwrap();
    assert_eq!(report.files.len(), 1, "{:?}", report.files);
    assert!(report.files[0].records_ok && report.files[0].size_ok);
    assert_eq!(report.corrupt_orphans.len(), 1, "{report:?}");
    assert!(
        report.corrupt_orphans[0]
            .file
            .ends_with("hl-ws-01-9999.jsonl.zst")
    );
    assert!(!report.corrupt_orphans[0].error.is_empty());
}

/// A mixed day: a manifest-backed segment and a finished orphan, in two
/// different hours, where the orphan contains `gap_start`/`gap_end` records.
/// Coverage unions both, the orphan is flagged, and the gap is excluded.
#[test]
fn verify_mixed_manifest_and_orphan_across_two_hours_with_gap() {
    let tmp = temp_dir("verify-mixed");
    let dir = tmp.path();
    let base_h0 = 1_767_227_400_000_000_000i64; // 2026-01-01T00:30:00Z
    let base_h1 = base_h0 + 3_600_000_000_000; // 01:30:00Z

    // Hour 00: a plain finalized segment, manifest line kept.
    write_finalized_segment(dir, 10, base_h0);

    // Hour 01: a finalized segment containing a gap, whose manifest line we
    // drop to make it an orphan.
    let (writer, clock) = spawn_writer(dir, "hl-ws", "hl-ws-01", base_h1);
    for i in 0..10u64 {
        clock.set_t_ns(base_h1 + i as i64 * 1_000_000);
        assert!(writer.try_send(Envelope::frame(&*clock, "hl-ws", "hl-ws-01", i, "data")));
    }
    assert!(writer.try_send(Envelope::gap_start_at(
        "hl-ws",
        "hl-ws-01",
        10,
        base_h1 + 10_000_000,
        0,
        "closed",
        "server closed",
    )));
    assert!(writer.try_send(Envelope::gap_end_at(
        "hl-ws",
        "hl-ws-01",
        11,
        base_h1 + 40_000_000,
        0,
        30,
    )));
    clock.set_t_ns(base_h1 + 50_000_000);
    assert!(writer.try_send(Envelope::frame(&*clock, "hl-ws", "hl-ws-01", 12, "data")));
    writer.shutdown().unwrap();

    let manifest = dir.join("testnet/hl-ws/2026-01-01/manifest.jsonl");
    let text = fs::read_to_string(&manifest).unwrap();
    let kept: String = text
        .lines()
        .filter(|line| !line.contains("/01/"))
        .map(|line| format!("{line}\n"))
        .collect();
    fs::write(&manifest, kept).unwrap();

    let report = verify(&VerifyConfig {
        out_dir: dir.to_path_buf(),
        network: "testnet".into(),
        date: "2026-01-01".into(),
    })
    .unwrap();
    assert_eq!(report.files.len(), 1, "{:?}", report.files);
    assert!(
        report.files[0].file.contains("/00/"),
        "{}",
        report.files[0].file
    );
    assert_eq!(report.orphans.len(), 1, "{report:?}");
    assert!(report.orphans[0].file.contains("/01/"), "{report:?}");
    assert_eq!(report.coverage.len(), 1, "{:?}", report.coverage);
    assert!(
        report.coverage[0].covered_ms > 0,
        "the orphan's records were not counted: {:?}",
        report.coverage
    );
}

// ---- Reader hardening: line cap, manifest paths, versioning, unknown kinds ----

fn write_zst(path: &Path, lines: &[String]) {
    fs::write(path, zstd_frame(lines)).unwrap();
}

fn raw_envelope(v: u8, kind: &str) -> String {
    format!(
        r#"{{"v":{v},"src":"hl-ws","conn":"hl-ws-01","seq":0,"t_ns":1000,"mono_ns":0,"kind":"{kind}","raw":"x"}}"#
    )
}

fn manifest_json(file: &str, v: Option<u64>) -> String {
    let mut value = serde_json::json!({
        "file": file,
        "src": "hl-ws",
        "conn": "hl-ws-01",
        "first_t_ns": 1000,
        "last_t_ns": 2000,
        "records": 3,
        "bytes_raw": 10,
        "bytes_zst": 5,
        "crashed": false,
    });
    if let Some(v) = v {
        value["v"] = serde_json::json!(v);
    }
    value.to_string()
}

#[test]
fn reader_rejects_a_line_over_the_cap() {
    let tmp = temp_dir("line-cap");
    let dir = tmp.path();

    // A normal file keeps reading exactly as before.
    let normal = dir.join("normal.jsonl.zst");
    write_zst(
        &normal,
        &[
            raw_envelope(SCHEMA_VERSION, "frame"),
            raw_envelope(SCHEMA_VERSION, "frame"),
        ],
    );
    let read: Vec<Envelope> = SegmentReader::open(&normal)
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(read.len(), 2);

    // One line larger than the cap is a typed error, not an abort or an
    // unbounded allocation.
    let huge = "a".repeat(MAX_LINE_BYTES + 1024);
    let path = dir.join("huge.jsonl.zst");
    write_zst(&path, &[huge]);
    let result: Result<Vec<Envelope>, ReaderError> = SegmentReader::open(&path).unwrap().collect();
    match result {
        Err(ReaderError::LineTooLong { limit, .. }) => assert_eq!(limit, MAX_LINE_BYTES),
        other => panic!("expected LineTooLong, got {other:?}"),
    }
}

#[test]
fn manifest_file_path_is_validated() {
    use super::manifest::validate_manifest_file;

    for bad in [
        "",
        "/etc/passwd",
        "../evil.jsonl.zst",
        "a/../../evil.jsonl.zst",
        "..",
        r"..\..\evil.jsonl.zst",
    ] {
        match validate_manifest_file(bad) {
            Err(ReaderError::InvalidManifestPath { path }) => assert_eq!(path, bad),
            other => panic!("expected InvalidManifestPath for {bad:?}, got {other:?}"),
        }
    }

    for good in [
        "x.jsonl.zst",
        "00/x.jsonl.zst",
        "testnet/hl-ws/2026-01-01/00/hl-ws-01-0.jsonl.zst",
    ] {
        assert!(
            validate_manifest_file(good).is_ok(),
            "rejected a safe path {good:?}"
        );
    }
}

#[test]
fn read_manifest_rejects_an_unsafe_file_before_opening_it() {
    let tmp = temp_dir("manifest-path");
    let dir = tmp.path();
    let manifest = dir.join("manifest.jsonl");
    fs::write(
        &manifest,
        format!("{}\n", manifest_json("/etc/passwd", None)),
    )
    .unwrap();

    match super::manifest::read_manifest(&manifest) {
        Err(ReaderError::InvalidManifestPath { path }) => assert_eq!(path, "/etc/passwd"),
        other => panic!("expected InvalidManifestPath, got {other:?}"),
    }
}

#[test]
fn reader_rejects_a_newer_envelope_version() {
    let tmp = temp_dir("envelope-version");
    let dir = tmp.path();
    let path = dir.join("s.jsonl.zst");
    write_zst(&path, &[raw_envelope(SCHEMA_VERSION + 1, "frame")]);
    let result: Result<Vec<Envelope>, ReaderError> = SegmentReader::open(&path).unwrap().collect();
    match result {
        Err(ReaderError::UnsupportedVersion { found, supported }) => {
            assert_eq!(found, SCHEMA_VERSION + 1);
            assert_eq!(supported, SCHEMA_VERSION);
        }
        other => panic!("expected UnsupportedVersion, got {other:?}"),
    }

    // An equal or older version still reads.
    for v in [0, SCHEMA_VERSION] {
        let ok = dir.join(format!("v{v}.jsonl.zst"));
        write_zst(&ok, &[raw_envelope(v, "frame")]);
        let read: Vec<Envelope> = SegmentReader::open(&ok)
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(read.len(), 1);
    }
}

#[test]
fn unknown_kind_is_skipped_and_tallied() {
    let tmp = temp_dir("unknown-kind");
    let dir = tmp.path();
    let path = dir.join("s.jsonl.zst");
    write_zst(
        &path,
        &[
            raw_envelope(SCHEMA_VERSION, "frame"),
            raw_envelope(SCHEMA_VERSION, "future_kind"),
            raw_envelope(SCHEMA_VERSION, "frame"),
        ],
    );

    // The bare enum maps an unrecognized kind to Unknown...
    let kind: Kind = serde_json::from_str("\"future_kind\"").unwrap();
    assert_eq!(kind, Kind::Unknown);

    // ...the iterator skips those lines and counts them...
    let mut reader = SegmentReader::open(&path).unwrap();
    let mut kinds = Vec::new();
    for env in reader.by_ref() {
        kinds.push(env.unwrap().kind);
    }
    assert_eq!(kinds, vec![Kind::Frame, Kind::Frame]);
    assert_eq!(reader.unknown_count(), 1);

    // ...and `inspect` reports the tally without failing.
    let report = inspect(&[path]).unwrap();
    assert_eq!(report.records, 2);
    assert_eq!(report.unknown_kinds, 1);
    assert_eq!(report.by_kind.get("future_kind"), None);
}

#[test]
fn manifest_version_is_optional_and_checked() {
    let tmp = temp_dir("manifest-version");
    let dir = tmp.path();

    for (tag, v) in [("absent", None), ("v1", Some(1u64))] {
        let path = dir.join(format!("{tag}.jsonl"));
        fs::write(&path, format!("{}\n", manifest_json("x.jsonl.zst", v))).unwrap();
        let entries = super::manifest::read_manifest(&path).unwrap();
        assert_eq!(entries.len(), 1, "{tag}");
        assert_eq!(entries[0].file, "x.jsonl.zst", "{tag}");
    }

    let path = dir.join("newer.jsonl");
    fs::write(
        &path,
        format!("{}\n", manifest_json("x.jsonl.zst", Some(2))),
    )
    .unwrap();
    match super::manifest::read_manifest(&path) {
        Err(ReaderError::UnsupportedManifestVersion {
            found, supported, ..
        }) => {
            assert_eq!(found, 2);
            assert_eq!(supported, super::manifest::MANIFEST_VERSION);
        }
        other => panic!("expected UnsupportedManifestVersion, got {other:?}"),
    }
}

#[test]
fn verify_tallies_skipped_unknown_kinds() {
    let tmp = temp_dir("verify-unknown");
    let dir = tmp.path();
    let date = "2026-01-01";
    let base = 1_767_227_400_000_000_000i64;
    let clock = FixedEnvelopeClock::new(base, 0);
    let lines: Vec<String> = vec![
        serde_json::to_string(&Envelope::segment_open(
            &clock,
            "hl-ws",
            "hl-ws-01",
            0,
            &Default::default(),
        ))
        .unwrap(),
        serde_json::to_string(&Envelope::frame(&clock, "hl-ws", "hl-ws-01", 1, "x")).unwrap(),
        raw_envelope(SCHEMA_VERSION, "future_kind"),
        serde_json::to_string(&Envelope::segment_close(
            &clock, "hl-ws", "hl-ws-01", 3, 4, 100,
        ))
        .unwrap(),
    ];
    let hour = dir.join("testnet/hl-ws").join(date).join("00");
    fs::create_dir_all(&hour).unwrap();
    let path = hour.join(format!("hl-ws-01-{base}.jsonl.zst"));
    write_zst(&path, &lines);

    let report = verify(&VerifyConfig {
        out_dir: dir.to_path_buf(),
        network: "testnet".into(),
        date: date.into(),
    })
    .unwrap();
    assert_eq!(report.unknown_kinds, 1, "{report:?}");
    assert_eq!(report.orphans.len(), 1, "{report:?}");
}
