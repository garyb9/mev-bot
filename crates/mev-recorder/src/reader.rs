//! Segment reader and `inspect`/`verify` analysis (SPEC-0008 §12.1, task R-7).
//!
//! Segments are read lazily, one envelope at a time ([`SegmentReader`]), so a
//! large file is never held in memory; several files merge through a k-way heap
//! ([`merge_segments_iter`]). A truncated zstd tail is tolerated only for
//! `.crashed` files, which yield every envelope the writer managed to flush;
//! for a finished segment a decode failure is an error. The analysis functions
//! are pure library code; wiring the CLI is R-6's job.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, VecDeque};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
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

/// An inclusive range of missing `seq` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeqRange {
    /// First missing `seq` (inclusive).
    pub start: u64,
    /// Last missing `seq` (inclusive).
    pub end: u64,
}

impl SeqRange {
    /// Number of `seq` values in the range.
    pub fn count(&self) -> u64 {
        self.end.saturating_sub(self.start).saturating_add(1)
    }
}

/// Missing `seq` values for one connection, stored compactly as ranges.
///
/// [`len`](MissingSeqs::len) returns the total number of missing values (not
/// the number of ranges), so callers that count holes keep their meaning while
/// a day-long outage costs one range instead of millions of numbers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MissingSeqs {
    ranges: Vec<SeqRange>,
}

impl MissingSeqs {
    /// The missing ranges, in ascending order and non-overlapping.
    pub fn ranges(&self) -> &[SeqRange] {
        &self.ranges
    }

    /// Total number of missing `seq` values.
    pub fn len(&self) -> usize {
        self.ranges.iter().map(|range| range.count() as usize).sum()
    }

    /// Whether no `seq` value is missing.
    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    fn push(&mut self, start: u64, end: u64) {
        if start <= end {
            self.ranges.push(SeqRange { start, end });
        }
    }
}

/// Per-connection `seq` holes found while reading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeqHoles {
    /// Source id.
    pub src: String,
    /// Connection id.
    pub conn: String,
    /// Number of `seq` runs seen (a new run starts when `seq` goes back down,
    /// as it does when the producing process restarts).
    pub runs: u64,
    /// Missing `seq` values, per run.
    pub missing: MissingSeqs,
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

/// A lazy reader over one segment file, yielding one envelope per line.
///
/// The file is decompressed and parsed incrementally, so a many-gigabyte
/// segment never has to be held in memory. A truncated zstd tail is tolerated
/// only for `.crashed` segments (it ends iteration at the last complete line);
/// for a finished segment a decode failure is an error.
pub struct SegmentReader {
    inner: BufReader<zstd::stream::read::Decoder<'static, BufReader<Box<dyn Read>>>>,
    path: PathBuf,
    crashed: bool,
    line: String,
    line_no: usize,
    done: bool,
}

impl SegmentReader {
    /// Open a segment file. A `.crashed` name tolerates a truncated tail.
    pub fn open(path: &Path) -> Result<Self, ReaderError> {
        let file = File::open(path)?;
        Self::with_reader(file, path, is_crashed(path))
    }

    fn with_reader<R: Read + 'static>(
        reader: R,
        path: &Path,
        crashed: bool,
    ) -> Result<Self, ReaderError> {
        let decoder = zstd::stream::read::Decoder::new(Box::new(reader) as Box<dyn Read>)?;
        Ok(Self {
            inner: BufReader::new(decoder),
            path: path.to_path_buf(),
            crashed,
            line: String::new(),
            line_no: 0,
            done: false,
        })
    }

    fn decode(&self, line: &str) -> Result<Envelope, ReaderError> {
        serde_json::from_str(line).map_err(|source| ReaderError::Decode {
            path: self.path.display().to_string(),
            line: self.line_no,
            source,
        })
    }
}

impl Iterator for SegmentReader {
    type Item = Result<Envelope, ReaderError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.done {
                return None;
            }
            self.line.clear();
            match self.inner.read_line(&mut self.line) {
                Ok(0) => {
                    self.done = true;
                    return None;
                }
                Ok(_) => {
                    self.line_no += 1;
                    if !self.line.ends_with('\n') {
                        // The tail was cut mid-line. A crashed segment stops at
                        // its last full line; a finished one still parses it if
                        // it happens to be complete.
                        self.done = true;
                        if self.crashed {
                            return None;
                        }
                        let trimmed = self.line.trim_end();
                        if trimmed.is_empty() {
                            return None;
                        }
                        return Some(self.decode(trimmed));
                    }
                    let trimmed = self.line.trim_end_matches(['\n', '\r']);
                    if trimmed.is_empty() {
                        continue;
                    }
                    return Some(self.decode(trimmed));
                }
                Err(err) => {
                    self.done = true;
                    if self.crashed {
                        // A truncated zstd tail is expected for a crash.
                        return None;
                    }
                    return Some(Err(ReaderError::Io(err)));
                }
            }
        }
    }
}

/// Decode every envelope in a segment, tolerating a truncated `.crashed` tail.
pub fn read_envelopes(path: &Path) -> Result<Vec<Envelope>, ReaderError> {
    SegmentReader::open(path)?.collect()
}

/// Enumerate every segment file under `out_dir/{network}` for the inclusive
/// UTC date range `[from, to]`, across all sources, sorted by path.
///
/// Includes `.crashed` segments (their readable prefix is still replay/verify
/// input) and never returns manifest files. A missing root yields an empty
/// list, so replay over a not-yet-recorded range is a clean no-op.
pub fn segments_for(
    out_dir: &Path,
    network: &str,
    from: &str,
    to: &str,
) -> Result<Vec<PathBuf>, ReaderError> {
    let (from_start, _) =
        day_bounds(from).ok_or_else(|| ReaderError::InvalidDate(from.to_string()))?;
    let (to_start, _) = day_bounds(to).ok_or_else(|| ReaderError::InvalidDate(to.to_string()))?;
    if from_start > to_start {
        return Ok(Vec::new());
    }

    let root = out_dir.join(network);
    let mut files = Vec::new();
    let src_dirs = match fs::read_dir(&root) {
        Ok(dirs) => dirs,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(files),
        Err(err) => return Err(err.into()),
    };
    for src in src_dirs {
        let src_path = src?.path();
        let date_dirs = match fs::read_dir(&src_path) {
            Ok(dirs) => dirs,
            Err(_) => continue,
        };
        for date in date_dirs {
            let date_path = date?.path();
            if !date_path.is_dir() {
                continue;
            }
            let Some(name) = date_path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            match day_bounds(name) {
                Some((day_start, _)) if day_start >= from_start && day_start <= to_start => {}
                _ => continue,
            }
            collect_segments(&date_path, &mut files)?;
        }
    }
    files.sort();
    Ok(files)
}

/// Recursively collect `.jsonl.zst`/`.jsonl.zst.crashed` files under `dir`.
fn collect_segments(dir: &Path, files: &mut Vec<PathBuf>) -> Result<(), ReaderError> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_segments(&path, files)?;
        } else if is_segment(&path) {
            files.push(path);
        }
    }
    Ok(())
}

fn is_segment(path: &Path) -> bool {
    let name = path.to_string_lossy();
    name.ends_with(".jsonl.zst") || name.ends_with(".jsonl.zst.crashed")
}

/// Lazily merge segment files by `(t_ns, conn, seq)`.
pub fn merge_segments_iter(paths: &[PathBuf]) -> Result<MergeIter, ReaderError> {
    MergeIter::new(paths)
}

/// Read several files and merge their envelopes by `(t_ns, conn, seq)`.
pub fn merge_segments(paths: &[PathBuf]) -> Result<Vec<Envelope>, ReaderError> {
    merge_segments_iter(paths)?.collect()
}

struct MergeNode {
    t_ns: i64,
    conn: String,
    seq: u64,
    source: usize,
    env: Envelope,
}

impl PartialEq for MergeNode {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == std::cmp::Ordering::Equal
    }
}

impl Eq for MergeNode {}

impl PartialOrd for MergeNode {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for MergeNode {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.t_ns, self.conn.as_str(), self.seq, self.source).cmp(&(
            other.t_ns,
            other.conn.as_str(),
            other.seq,
            other.source,
        ))
    }
}

/// A k-way merge of segment readers, ordered by `(t_ns, conn, seq)`.
pub struct MergeIter {
    readers: Vec<SegmentReader>,
    heap: BinaryHeap<Reverse<MergeNode>>,
    pending: VecDeque<ReaderError>,
}

impl MergeIter {
    fn new(paths: &[PathBuf]) -> Result<Self, ReaderError> {
        let mut readers = Vec::with_capacity(paths.len());
        for path in paths {
            readers.push(SegmentReader::open(path)?);
        }
        let mut heap = BinaryHeap::new();
        let mut pending = VecDeque::new();
        for (source, reader) in readers.iter_mut().enumerate() {
            if let Some(item) = reader.next() {
                match item {
                    Ok(env) => heap.push(Reverse(MergeNode {
                        t_ns: env.t_ns,
                        conn: env.conn.clone(),
                        seq: env.seq,
                        source,
                        env,
                    })),
                    Err(err) => pending.push_back(err),
                }
            }
        }
        Ok(Self {
            readers,
            heap,
            pending,
        })
    }
}

impl Iterator for MergeIter {
    type Item = Result<Envelope, ReaderError>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(err) = self.pending.pop_front() {
            return Some(Err(err));
        }
        let Reverse(node) = self.heap.pop()?;
        if let Some(item) = self.readers[node.source].next() {
            match item {
                Ok(env) => self.heap.push(Reverse(MergeNode {
                    t_ns: env.t_ns,
                    conn: env.conn.clone(),
                    seq: env.seq,
                    source: node.source,
                    env,
                })),
                Err(err) => self.pending.push_back(err),
            }
        }
        Some(Ok(node.env))
    }
}

/// Per-connection `seq` tracker that finds holes across process restarts.
#[derive(Default)]
struct SeqTracker {
    runs: u64,
    last_seq: Option<u64>,
    missing: MissingSeqs,
}

impl SeqTracker {
    fn observe(&mut self, seq: u64) {
        match self.last_seq {
            None => self.start_run(seq),
            Some(last) if seq < last => self.start_run(seq),
            Some(last) => {
                if seq > last.saturating_add(1) {
                    self.missing.push(last.saturating_add(1), seq - 1);
                }
            }
        }
        self.last_seq = Some(seq);
    }

    fn start_run(&mut self, seq: u64) {
        self.runs += 1;
        if seq > 0 {
            self.missing.push(0, seq - 1);
        }
    }
}

/// Whether a kind carries a producer `seq` (the writer's own bookkeeping lines
/// reuse the neighbouring envelope's `seq` and are not holes).
fn counts_toward_seq(kind: Kind) -> bool {
    !matches!(kind, Kind::SegmentOpen | Kind::SegmentClose)
}

/// Build the `inspect` report over one or more segment files.
pub fn inspect(paths: &[PathBuf]) -> Result<InspectReport, ReaderError> {
    let mut report = InspectReport::default();
    let mut seq: BTreeMap<(String, String), SeqTracker> = BTreeMap::new();
    let mut open_gaps: BTreeMap<(String, String), i64> = BTreeMap::new();

    for path in paths {
        report.files += 1;
        if is_crashed(path) {
            report.crashed_files += 1;
        }
        for env in SegmentReader::open(path)? {
            let env = env?;
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
            if counts_toward_seq(env.kind) {
                seq.entry(conn_key.clone()).or_default().observe(env.seq);
            }

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

    for ((src, conn), tracker) in seq {
        if !tracker.missing.is_empty() {
            report.seq_holes.push(SeqHoles {
                src,
                conn,
                runs: tracker.runs,
                missing: tracker.missing,
            });
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
                let envelopes = read_envelopes(&path)?;
                let acc = coverage
                    .entry((entry.src.clone(), entry.conn.clone()))
                    .or_default();
                acc.start_segment();
                for env in &envelopes {
                    acc.observe(env);
                }
                acc.end_segment();
                Some(envelopes.len() as u64)
            } else {
                None
            };
            let size_ok = bytes_zst_on_disk == Some(entry.bytes_zst);
            let records_ok = entry.crashed || records_on_disk == Some(entry.records);

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
    /// `[first_t_ns, last_t_ns]` of every segment file observed for the stream.
    ///
    /// Coverage is the union of these intervals (SPEC-0008 §5.3): the time
    /// between two segments is *not* covered, so a crash and a restart hours
    /// later show as an outage. A segment that lacks `segment_close` (a crash)
    /// simply ends its interval at its last record.
    segments: Vec<(i64, i64)>,
    /// Paired `gap_start`/`gap_end` intervals, across segment files.
    gaps: Vec<(i64, i64)>,
    /// A `gap_start` still awaiting its `gap_end`.
    open_gap: Option<i64>,
    /// Bounds of the records in the segment file currently being read.
    seg_min: Option<i64>,
    seg_max: Option<i64>,
    /// Bounds over every record, used to close an unpaired gap conservatively.
    min_t_ns: Option<i64>,
    max_t_ns: Option<i64>,
}

impl CoverageAcc {
    /// Begin observing a new segment file.
    fn start_segment(&mut self) {
        self.seg_min = None;
        self.seg_max = None;
    }

    /// Record one envelope of the current segment file.
    fn observe(&mut self, env: &Envelope) {
        self.min_t_ns = Some(self.min_t_ns.map_or(env.t_ns, |t| t.min(env.t_ns)));
        self.max_t_ns = Some(self.max_t_ns.map_or(env.t_ns, |t| t.max(env.t_ns)));
        // The writer's own bookkeeping lines are not coverage; only the records
        // a source produced (frames, REST bodies, gaps, …) define the span.
        if !matches!(env.kind, Kind::SegmentOpen | Kind::SegmentClose) {
            self.seg_min = Some(self.seg_min.map_or(env.t_ns, |t| t.min(env.t_ns)));
            self.seg_max = Some(self.seg_max.map_or(env.t_ns, |t| t.max(env.t_ns)));
        }
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

    /// Close the current segment file, adding its interval to the union.
    fn end_segment(&mut self) {
        if let Some(interval) = self.seg_min.zip(self.seg_max) {
            self.segments.push(interval);
        }
    }

    fn covered_ms(&mut self, day_start: i64, day_end: i64) -> u64 {
        // An unpaired `gap_start` is closed conservatively at the last record
        // seen on the stream. How a `drop` gap ends has no specified rule yet
        // (SPEC-0008 §17 open question); until then, marking the remainder
        // missing is the safe reading.
        if let Some(start) = self.open_gap.take()
            && let Some(max) = self.max_t_ns
        {
            self.gaps.push((start, max));
        }
        let spans = clip(std::mem::take(&mut self.segments), day_start, day_end);
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
    use std::io::{Read, Write};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::envelope::{Envelope, FixedEnvelopeClock};
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

        let result: Result<Vec<Envelope>, ReaderError> =
            SegmentReader::open(&path).unwrap().collect();
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
                let env =
                    Envelope::frame(&clock_at(), "hl-ws", "hl-ws-01", i, pseudo_random_hex(i));
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
}
