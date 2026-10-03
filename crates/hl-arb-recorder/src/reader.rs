//! Segment reader and `inspect`/`verify` analysis (SPEC-0008 §12.1, task R-7).
//!
//! Segments are read lazily, one envelope at a time ([`SegmentReader`]), so a
//! large file is never held in memory; several files merge through a k-way heap
//! ([`merge_segments_iter`]). A truncated zstd tail is tolerated only for
//! `.crashed` files, which yield every envelope the writer managed to flush;
//! for a finished segment a decode failure is an error. The analysis functions
//! are pure library code; wiring the CLI is R-6's job. [`verify`] additionally
//! compares the manifest against the files on disk and [`repair_manifest`]
//! appends the missing line for an orphan finished segment (SPEC-0008 §17 #36).

use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use serde_json::Value;
use thiserror::Error;

use crate::envelope::{Envelope, Kind};

mod inspect;
mod manifest;
mod read;
mod repair;
mod verify;

use self::read::is_crashed;

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
    /// A reconstructed manifest line could not be encoded.
    #[error("manifest encode error for {path}: {source}")]
    ManifestEncode {
        /// Manifest file path.
        path: String,
        /// The serde error.
        source: serde_json::Error,
    },
    /// A segment path does not follow the recorder's naming scheme.
    #[error("invalid segment path `{path}`")]
    InvalidSegment {
        /// The offending path.
        path: String,
    },
    /// The recorder root directory does not exist.
    #[error("recorder root `{0}` does not exist; nothing to do (check the profile out_dir)")]
    MissingRoot(String),
    /// A `.partial` file is present, so a recorder may be running or have
    /// crashed mid-segment.
    #[error(
        "refusing to repair: found `{path}`; a recorder may be running or crashed \
         mid-segment. Stop the recorder first, or run with --dry-run (a leftover \
         `.partial` is recovered by restarting the recorder)"
    )]
    PartialPresent {
        /// The offending `.partial` path.
        path: String,
    },
    /// The requested date was not `YYYY-MM-DD`.
    #[error("invalid date `{0}`")]
    InvalidDate(String),
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

#[derive(Default)]
struct CoverageAcc {
    /// `[first_t_ns, last_t_ns]` of every segment file observed for the stream.
    ///
    /// Coverage is the union of these intervals (SPEC-0008 §5.3): the time
    /// between two segments is *not* covered, so a crash and a restart hours
    /// later show as an outage. A segment that lacks `segment_close` (a crash)
    /// simply ends its interval at its last record.
    segments: Vec<(i64, i64)>,
    /// Closed gap intervals, across segment files.
    gaps: Vec<(i64, i64)>,
    /// A `gap_start` still awaiting its end.
    ///
    /// An open gap is closed at the next data envelope (`frame`, `frame_bin`,
    /// `rest`) or `gap_end` on the same stream, matching the Python
    /// `_GapTracker` (SPEC-0008 §5.3). This matters when a run ends with an
    /// unpaired `shutdown` gap and a later run restarts on the same stream:
    /// the downtime stays a gap and the new run's frames are covered. Segments
    /// are visited in manifest/file order and the state is per `(src, conn)`,
    /// so a later run's first frame closes the previous run's gap.
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
    ///
    /// Only the interval bounds reset. The open-gap state deliberately carries
    /// across segment files and runs (state is per `(src, conn)`), so a gap
    /// opened at the end of one run is closed by the first frame of the next.
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
            Kind::GapStart => {
                // A second `gap_start` with no `gap_end` between (for example a
                // `shutdown` arriving mid-outage) closes the current gap at the
                // new start before opening it, matching the Python
                // `_GapTracker._open`. Overwriting the start instead would
                // count `[disconnect, shutdown)` as covered.
                self.close_gap(env.t_ns);
                self.open_gap = Some(env.t_ns);
            }
            Kind::GapEnd => {
                self.close_gap(env.t_ns);
            }
            Kind::Frame | Kind::FrameBin | Kind::Rest => {
                // Data resumed: an unpaired gap (`drop`/`shutdown`/`crash`) ends
                // here, at the first data envelope that follows it in stream
                // order. This keeps a restart's downtime a gap (never counted as
                // coverage) while covering every frame of the new run. Matching
                // the Python `_GapTracker.observe`, a `gap_end` envelope also
                // ends the gap even though it is not itself data.
                self.close_gap(env.t_ns);
            }
            _ => {}
        }
    }

    /// Close an open gap at `end_ns`, never before its start (Python
    /// `_GapTracker._close`).
    fn close_gap(&mut self, end_ns: i64) {
        if let Some(start) = self.open_gap.take() {
            self.gaps.push((start, end_ns.max(start)));
        }
    }

    /// Close the current segment file, adding its interval to the union.
    fn end_segment(&mut self) {
        if let Some(interval) = self.seg_min.zip(self.seg_max) {
            self.segments.push(interval);
        }
    }

    fn covered_ms(&mut self, day_start: i64, day_end: i64) -> u64 {
        // A `gap_start` that is still open at the end of the data is closed
        // conservatively at the last record seen on the stream. How a `drop`
        // gap ends has no specified rule yet (SPEC-0008 §17 open question);
        // until then, marking the remainder missing is the safe reading.
        if let Some(max) = self.max_t_ns {
            self.close_gap(max);
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

/// The channel an envelope belongs to: a label for a frame, or the kind name
/// for everything else.
///
/// For `frame` / `frame_bin` the raw text is parsed once as JSON and the label
/// is the first of these that applies:
///
/// 1. a top-level string `channel` (Hyperliquid), used as is, even when other
///    keys are present;
/// 2. else a top-level string `stream` (Binance, e.g. `btcusdt@bookTicker`):
///    the part after the last `@`, or the whole string when there is no `@`;
/// 3. else a top-level string `topic` (Bybit, e.g. `orderbook.1.BTCUSDT`): the
///    topic without its final `.`-separated segment (`orderbook.1`), or the
///    whole string when there is no `.`;
/// 4. else `"unknown"` (non-JSON raw, JSON without those string keys, Bybit
///    `op` acks, and `frame_bin`, whose raw is base64).
fn channel_of(env: &Envelope) -> String {
    match env.kind {
        Kind::Frame | Kind::FrameBin => env
            .raw
            .as_deref()
            .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
            .and_then(|value| frame_label(&value))
            .unwrap_or_else(|| "unknown".to_string()),
        other => other.as_str().to_string(),
    }
}

/// Steps 1-3 of [`channel_of`] on an already parsed frame.
fn frame_label(value: &Value) -> Option<String> {
    if let Some(channel) = value.get("channel").and_then(Value::as_str) {
        return Some(channel.to_string());
    }
    if let Some(stream) = value.get("stream").and_then(Value::as_str) {
        let label = stream.rsplit_once('@').map_or(stream, |(_, tail)| tail);
        return Some(label.to_string());
    }
    let topic = value.get("topic").and_then(Value::as_str)?;
    let label = topic.rsplit_once('.').map_or(topic, |(head, _)| head);
    Some(label.to_string())
}

pub use self::inspect::{InspectReport, MissingSeqs, SeqHoles, SeqRange, inspect};
pub use self::manifest::{append_manifest_entry, segment_manifest_entry};
pub use self::read::{
    MergeIter, merge_segments, merge_segments_iter, read_envelopes, segments_for,
};
pub use self::repair::{RepairAction, RepairConfig, repair_manifest};
pub use self::verify::{
    CorruptOrphan, FileCheck, StreamCoverage, UnmanifestedSegment, VerifyConfig, VerifyReport,
    verify,
};

#[cfg(test)]
mod tests;
