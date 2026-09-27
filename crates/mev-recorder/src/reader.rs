//! Segment reader and `inspect`/`verify` analysis (SPEC-0008 §12.1, task R-7).
//!
//! Reading always tolerates a truncated tail: the zstd stream is decoded until
//! the last complete line, so a `.crashed` file yields every envelope the writer
//! managed to flush. The analysis functions are pure library code; wiring the
//! CLI is R-6's job.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use serde_json::Value;
use thiserror::Error;

use crate::envelope::{Envelope, Kind};
use crate::segment::ManifestEntry;

/// A reader failure. Truncated segments are not an error.
#[derive(Debug, Error)]
pub enum ReaderError {
    /// An I/O operation failed.
    #[error("segment i/o error: {0}")]
    Io(#[from] std::io::Error),
    /// A complete line could not be decoded.
    #[error("envelope decode error in {path} line {line}: {source}")]
    Decode {
        /// File being read.
        path: String,
        /// 1-based line number.
        line: usize,
        /// The serde error.
        source: serde_json::Error,
    },
    /// A manifest line could not be decoded.
    #[error("manifest parse error in {path}: {source}")]
    Manifest {
        /// Manifest file path.
        path: String,
        /// The serde error.
        source: serde_json::Error,
    },
    /// The requested date was not `YYYY-MM-DD`.
    #[error("invalid date `{0}`")]
    InvalidDate(String),
}

/// Per-connection `seq` holes found while reading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeqHoles {
    /// Source id.
    pub src: String,
    /// Connection id.
    pub conn: String,
    /// Sequence numbers missing between the observed minimum and maximum.
    pub missing: Vec<u64>,
}

/// Output of `hl record inspect` (SPEC-0008 §12.1).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InspectReport {
    /// Number of files read.
    pub files: usize,
    /// Total envelopes read.
    pub records: u64,
    /// Counts by `src`.
    pub by_src: BTreeMap<String, u64>,
    /// Counts by `conn`.
    pub by_conn: BTreeMap<String, u64>,
    /// Counts by envelope kind.
    pub by_kind: BTreeMap<String, u64>,
    /// Counts by decoded channel (frames) or kind (everything else).
    pub by_channel: BTreeMap<String, u64>,
    /// Earliest `t_ns` seen.
    pub first_t_ns: Option<i64>,
    /// Latest `t_ns` seen.
    pub last_t_ns: Option<i64>,
    /// Number of `gap_start` records.
    pub gap_count: u64,
    /// Total gap time in milliseconds, from paired `gap_start`/`gap_end`.
    pub gap_total_ms: u64,
    /// Per-connection `seq` holes.
    pub seq_holes: Vec<SeqHoles>,
    /// Number of `.crashed` files read.
    pub crashed_files: usize,
}

/// One file's manifest-vs-disk check (SPEC-0008 §12.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileCheck {
    /// Path relative to `out_dir`.
    pub file: String,
    /// Source id.
    pub src: String,
    /// Connection id.
    pub conn: String,
    /// Whether the file exists on disk.
    pub exists: bool,
    /// Compressed size recorded in the manifest.
    pub bytes_zst_manifest: u64,
    /// Compressed size on disk, if the file exists.
    pub bytes_zst_on_disk: Option<u64>,
    /// Whether the on-disk size matches the manifest.
    pub size_ok: bool,
    /// Record count recorded in the manifest.
    pub records_manifest: u64,
    /// Record count decoded from disk, if the file exists.
    pub records_on_disk: Option<u64>,
    /// Whether the decoded count matches the manifest (always true for crashes).
    pub records_ok: bool,
    /// Whether the segment is a crash recovery.
    pub crashed: bool,
}

/// Coverage of one stream (src/conn) for the verified day.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamCoverage {
    /// Source id.
    pub src: String,
    /// Connection id.
    pub conn: String,
    /// Milliseconds with data and no gap.
    pub covered_ms: u64,
    /// Day length in milliseconds.
    pub day_ms: u64,
    /// `covered_ms / day_ms`, as a percentage.
    pub coverage_pct: f64,
}

/// Output of `hl record verify` (SPEC-0008 §12.1).
#[derive(Debug, Clone, PartialEq)]
pub struct VerifyReport {
    /// The verified UTC date.
    pub date: String,
    /// Per-file checks.
    pub files: Vec<FileCheck>,
    /// Per-stream coverage.
    pub coverage: Vec<StreamCoverage>,
}

/// Inputs for [`verify`].
#[derive(Debug, Clone)]
pub struct VerifyConfig {
    /// Root of the recording tree (usually `data/rec`).
    pub out_dir: PathBuf,
    /// Network directory name, `mainnet` or `testnet`.
    pub network: String,
    /// UTC date to verify, `YYYY-MM-DD`.
    pub date: String,
}

/// Decode every envelope in a segment, tolerating a truncated tail.
pub fn read_envelopes(path: &Path) -> Result<Vec<Envelope>, ReaderError> {
    let file = File::open(path)?;
    let mut decoder = zstd::stream::read::Decoder::new(file)?;
    let mut buffer = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        match decoder.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buffer.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }
    parse_lines(path, &buffer)
}

/// Read several files and merge their envelopes by `(t_ns, conn, seq)`.
pub fn merge_segments(paths: &[PathBuf]) -> Result<Vec<Envelope>, ReaderError> {
    let mut all = Vec::new();
    for path in paths {
        all.extend(read_envelopes(path)?);
    }
    all.sort_by(|a, b| (a.t_ns, a.conn.as_str(), a.seq).cmp(&(b.t_ns, b.conn.as_str(), b.seq)));
    Ok(all)
}

/// Build the `inspect` report over one or more segment files.
pub fn inspect(paths: &[PathBuf]) -> Result<InspectReport, ReaderError> {
    let mut report = InspectReport::default();
    let mut seqs: BTreeMap<(String, String), BTreeSet<u64>> = BTreeMap::new();
    let mut open_gaps: BTreeMap<(String, String), i64> = BTreeMap::new();

    for path in paths {
        report.files += 1;
        if is_crashed(path) {
            report.crashed_files += 1;
        }
        for env in read_envelopes(path)? {
            report.records += 1;
            *report.by_src.entry(env.src.clone()).or_insert(0) += 1;
            *report.by_conn.entry(env.conn.clone()).or_insert(0) += 1;
            *report
                .by_kind
                .entry(env.kind.as_str().to_string())
                .or_insert(0) += 1;
            *report.by_channel.entry(channel_of(&env)).or_insert(0) += 1;
            report.first_t_ns = Some(report.first_t_ns.map_or(env.t_ns, |t| t.min(env.t_ns)));
            report.last_t_ns = Some(report.last_t_ns.map_or(env.t_ns, |t| t.max(env.t_ns)));

            let conn_key = (env.src.clone(), env.conn.clone());
            seqs.entry(conn_key.clone()).or_default().insert(env.seq);

            match env.kind {
                Kind::GapStart => {
                    report.gap_count += 1;
                    open_gaps.insert(conn_key, env.t_ns);
                }
                Kind::GapEnd => {
                    if let Some(start) = open_gaps.remove(&conn_key) {
                        report.gap_total_ms +=
                            env.t_ns.saturating_sub(start).max(0) as u64 / 1_000_000;
                    }
                }
                _ => {}
            }
        }
    }

    for ((src, conn), set) in seqs {
        let Some((&min, &max)) = set.iter().next().zip(set.iter().next_back()) else {
            continue;
        };
        let missing: Vec<u64> = (min..=max).filter(|seq| !set.contains(seq)).collect();
        if !missing.is_empty() {
            report.seq_holes.push(SeqHoles { src, conn, missing });
        }
    }

    Ok(report)
}

/// Check a day's manifest against the files on disk and compute coverage.
pub fn verify(config: &VerifyConfig) -> Result<VerifyReport, ReaderError> {
    let (day_start, day_end) =
        day_bounds(&config.date).ok_or_else(|| ReaderError::InvalidDate(config.date.clone()))?;

    let mut report = VerifyReport {
        date: config.date.clone(),
        files: Vec::new(),
        coverage: Vec::new(),
    };
    let mut coverage: BTreeMap<(String, String), CoverageAcc> = BTreeMap::new();

    for manifest in manifests_for(&config.out_dir, &config.network, &config.date)? {
        for entry in read_manifest(&manifest)? {
            let path = config.out_dir.join(&entry.file);
            let exists = path.exists();
            let bytes_zst_on_disk = if exists {
                Some(fs::metadata(&path)?.len())
            } else {
                None
            };
            let records_on_disk = if exists {
                Some(read_envelopes(&path)?.len() as u64)
            } else {
                None
            };
            let size_ok = bytes_zst_on_disk == Some(entry.bytes_zst);
            let records_ok = entry.crashed || records_on_disk == Some(entry.records);

            if exists {
                let acc = coverage
                    .entry((entry.src.clone(), entry.conn.clone()))
                    .or_default();
                for env in read_envelopes(&path)? {
                    acc.observe(&env);
                }
            }

            report.files.push(FileCheck {
                file: entry.file,
                src: entry.src,
                conn: entry.conn,
                exists,
                bytes_zst_manifest: entry.bytes_zst,
                bytes_zst_on_disk,
                size_ok,
                records_manifest: entry.records,
                records_on_disk,
                records_ok,
                crashed: entry.crashed,
            });
        }
    }
    report.files.sort_by(|a, b| a.file.cmp(&b.file));

    let day_ms = day_end.saturating_sub(day_start) as u64 / 1_000_000;
    for ((src, conn), mut acc) in coverage {
        let covered_ms = acc.covered_ms(day_start, day_end);
        report.coverage.push(StreamCoverage {
            src,
            conn,
            covered_ms,
            day_ms,
            coverage_pct: if day_ms == 0 {
                0.0
            } else {
                covered_ms as f64 * 100.0 / day_ms as f64
            },
        });
    }
    report
        .coverage
        .sort_by(|a, b| (&a.src, &a.conn).cmp(&(&b.src, &b.conn)));

    Ok(report)
}

fn parse_lines(path: &Path, buffer: &[u8]) -> Result<Vec<Envelope>, ReaderError> {
    let Some(last_newline) = buffer.iter().rposition(|&byte| byte == b'\n') else {
        return Ok(Vec::new());
    };
    let mut envelopes = Vec::new();
    for (line_no, line) in buffer[..=last_newline]
        .split(|&byte| byte == b'\n')
        .enumerate()
    {
        if line.is_empty() {
            continue;
        }
        let env = serde_json::from_slice(line).map_err(|source| ReaderError::Decode {
            path: path.display().to_string(),
            line: line_no + 1,
            source,
        })?;
        envelopes.push(env);
    }
    Ok(envelopes)
}

fn read_manifest(path: &Path) -> Result<Vec<ManifestEntry>, ReaderError> {
    let text = fs::read_to_string(path)?;
    let mut entries = Vec::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let entry = serde_json::from_str(line).map_err(|source| ReaderError::Manifest {
            path: path.display().to_string(),
            source,
        })?;
        entries.push(entry);
    }
    Ok(entries)
}

fn manifests_for(out_dir: &Path, network: &str, date: &str) -> Result<Vec<PathBuf>, ReaderError> {
    let root = out_dir.join(network);
    let mut manifests = Vec::new();
    let src_dirs = match fs::read_dir(&root) {
        Ok(dirs) => dirs,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(manifests),
        Err(err) => return Err(err.into()),
    };
    for src in src_dirs {
        let manifest = src?.path().join(date).join("manifest.jsonl");
        if manifest.is_file() {
            manifests.push(manifest);
        }
    }
    manifests.sort();
    Ok(manifests)
}

fn is_crashed(path: &Path) -> bool {
    path.to_string_lossy().ends_with(".crashed")
}

/// The channel an envelope belongs to: a frame's `channel` field, or the kind
/// name for everything else.
fn channel_of(env: &Envelope) -> String {
    match env.kind {
        Kind::Frame | Kind::FrameBin => env
            .raw
            .as_deref()
            .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
            .and_then(|value| {
                value
                    .get("channel")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "unknown".to_string()),
        other => other.as_str().to_string(),
    }
}

#[derive(Default)]
struct CoverageAcc {
    min_t_ns: Option<i64>,
    max_t_ns: Option<i64>,
    open_gap: Option<i64>,
    gaps: Vec<(i64, i64)>,
}

impl CoverageAcc {
    fn observe(&mut self, env: &Envelope) {
        self.min_t_ns = Some(self.min_t_ns.map_or(env.t_ns, |t| t.min(env.t_ns)));
        self.max_t_ns = Some(self.max_t_ns.map_or(env.t_ns, |t| t.max(env.t_ns)));
        match env.kind {
            Kind::GapStart => self.open_gap = Some(env.t_ns),
            Kind::GapEnd => {
                if let Some(start) = self.open_gap.take() {
                    self.gaps.push((start, env.t_ns));
                }
            }
            _ => {}
        }
    }

    fn covered_ms(&mut self, day_start: i64, day_end: i64) -> u64 {
        if let Some(start) = self.open_gap.take()
            && let Some(max) = self.max_t_ns
        {
            self.gaps.push((start, max));
        }
        let Some((min, max)) = self.min_t_ns.zip(self.max_t_ns) else {
            return 0;
        };
        let spans = clip(vec![(min, max)], day_start, day_end);
        let gaps = clip(std::mem::take(&mut self.gaps), day_start, day_end);
        let covered: i64 = subtract(&spans, &gaps)
            .into_iter()
            .map(|(a, b)| b - a)
            .sum();
        covered.max(0) as u64 / 1_000_000
    }
}

/// Merge overlapping intervals and clip them to `[lo, hi]`.
fn clip(mut intervals: Vec<(i64, i64)>, lo: i64, hi: i64) -> Vec<(i64, i64)> {
    intervals.retain(|(a, b)| *b > lo && *a < hi);
    for interval in &mut intervals {
        interval.0 = interval.0.max(lo);
        interval.1 = interval.1.min(hi);
    }
    intervals.sort();
    let mut merged: Vec<(i64, i64)> = Vec::new();
    for (start, end) in intervals {
        if let Some(last) = merged.last_mut()
            && start <= last.1
        {
            last.1 = last.1.max(end);
            continue;
        }
        merged.push((start, end));
    }
    merged
}

/// Subtract `subtrahends` from `intervals` (all merged).
fn subtract(intervals: &[(i64, i64)], subtrahends: &[(i64, i64)]) -> Vec<(i64, i64)> {
    let mut result: Vec<(i64, i64)> = Vec::new();
    for &(start, end) in intervals {
        let mut cursor = start;
        for &(sub_start, sub_end) in subtrahends {
            if sub_end <= cursor || sub_start >= end {
                continue;
            }
            if sub_start > cursor {
                result.push((cursor, sub_start.min(end)));
            }
            cursor = cursor.max(sub_end);
            if cursor >= end {
                break;
            }
        }
        if cursor < end {
            result.push((cursor, end));
        }
    }
    result
}

/// `(day_start_ns, day_end_ns)` for a `YYYY-MM-DD` UTC date.
fn day_bounds(date: &str) -> Option<(i64, i64)> {
    let mut parts = date.split('-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: i64 = parts.next()?.parse().ok()?;
    let day: i64 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let start = days.checked_mul(86_400_000_000_000)?;
    Some((start, start + 86_400_000_000_000))
}

/// Inverse of `civil_from_days` (Howard Hinnant): `(year, month, day)` to days
/// since 1970-01-01.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::envelope::{Envelope, FixedEnvelopeClock};
    use crate::segment::{SegmentConfig, SegmentWriter};

    use super::*;

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(tag: &str) -> PathBuf {
        let n = TEMP_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("mev-rec-reader-{tag}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
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

    #[test]
    fn truncated_crashed_file_reads_to_last_full_line() {
        let dir = temp_dir("crash");
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
    fn merge_orders_by_time_conn_seq() {
        let dir = temp_dir("merge");
        let clock = FixedEnvelopeClock::new(1_000, 0);
        let a = [
            Envelope::frame(&clock, "hl-ws", "hl-ws-01", 0, "a0"),
            Envelope::frame(&clock, "hl-ws", "hl-ws-01", 1, "a1"),
        ];
        let b = [
            Envelope::frame(&clock, "hl-ws", "hl-ws-02", 0, "b0"),
            Envelope::frame(&clock, "hl-ws", "hl-ws-01", 0, "c0"),
        ];
        let path_a = dir.join("a.jsonl.zst");
        let path_b = dir.join("b.jsonl.zst");
        fs::write(
            &path_a,
            zstd_frame(
                &a.iter()
                    .map(|e| serde_json::to_string(e).unwrap())
                    .collect::<Vec<_>>(),
            ),
        )
        .unwrap();
        fs::write(
            &path_b,
            zstd_frame(
                &b.iter()
                    .map(|e| serde_json::to_string(e).unwrap())
                    .collect::<Vec<_>>(),
            ),
        )
        .unwrap();

        let merged = merge_segments(&[path_b, path_a]).unwrap();
        assert_eq!(merged.len(), 4);
        let keys: Vec<(i64, &str, u64)> = merged
            .iter()
            .map(|e| (e.t_ns, e.conn.as_str(), e.seq))
            .collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted);
        assert_eq!(keys[0].1, "hl-ws-01");
        assert_eq!(keys[1].1, "hl-ws-01");
        assert_eq!(keys[2].1, "hl-ws-01");
        assert_eq!(keys[3].1, "hl-ws-02");
        assert_eq!(keys[3].2, 0);
    }

    #[test]
    fn inspect_detects_seq_holes_and_gaps() {
        let dir = temp_dir("inspect");
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
        assert_eq!(report.seq_holes[0].missing, vec![3]);
        assert_eq!(report.by_src.get("hl-ws"), Some(&5));
        assert_eq!(report.by_kind.get("frame"), Some(&3));
    }

    #[test]
    fn verify_checks_manifest_against_disk() {
        let dir = temp_dir("verify");
        let clock = Arc::new(FixedEnvelopeClock::new(1_767_227_400_000_000_000, 0));
        let config = SegmentConfig {
            out_dir: dir.clone(),
            network: "testnet".into(),
            src: "hl-ws".into(),
            conn: "hl-ws-01".into(),
            clock: clock.clone(),
            ..SegmentConfig::default()
        };
        let writer = SegmentWriter::spawn(config).unwrap();
        for i in 0..20u64 {
            clock.set_t_ns(1_767_227_400_000_000_000 + i as i64 * 1_000_000);
            assert!(writer.try_send(Envelope::frame(&*clock, "hl-ws", "hl-ws-01", i, "data")));
        }
        writer.shutdown();

        let report = verify(&VerifyConfig {
            out_dir: dir,
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
        assert_eq!(file.records_manifest, 22);
        assert_eq!(report.coverage.len(), 1);
        assert!(report.coverage[0].coverage_pct > 0.0);
    }

    #[test]
    fn verify_rejects_bad_date() {
        let err = verify(&VerifyConfig {
            out_dir: temp_dir("date"),
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
}
