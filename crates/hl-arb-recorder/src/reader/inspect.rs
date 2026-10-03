//! The `inspect` report: per-file counts, channels, gaps, and seq holes (SPEC-0008 §12.1).

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::envelope::Kind;

use super::read::is_crashed;
use super::{ReaderError, SegmentReader, channel_of};

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
    /// Unknown-kind envelopes skipped (from a newer recorder).
    pub unknown_kinds: u64,
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
        let mut reader = SegmentReader::open(path)?;
        for env in reader.by_ref() {
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
        report.unknown_kinds += reader.unknown_count();
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
