//! The `verify` report: manifest-vs-disk checks and per-stream coverage (SPEC-0008 §12.1).

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use crate::envelope::Kind;
use crate::segment::ManifestEntry;

use super::manifest::{
    manifests_for, read_manifest, rel_path, segment_identity, segment_manifest_entry,
};
use super::read::{ensure_root_exists, is_crashed, partials_for_day, read_envelopes, segments_for};
use super::{CoverageAcc, ReaderError, day_bounds};

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

/// A segment on disk with no manifest line that [`verify`] does **not** classify
/// as a repairable finished orphan (SPEC-0008 §17 #36).
///
/// Such a segment is recovered by restarting the recorder, not by
/// [`repair_manifest`](super::repair::repair_manifest).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnmanifestedSegment {
    /// Path relative to `out_dir`.
    pub file: String,
    /// Source id.
    pub src: String,
    /// Connection id.
    pub conn: String,
    /// Decoded record count.
    pub records: u64,
}

/// A segment on disk with no manifest line that could not be decoded at all,
/// so [`verify`] can say nothing else about it (SPEC-0008 §17 #36).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorruptOrphan {
    /// Path relative to `out_dir`.
    pub file: String,
    /// The decode error.
    pub error: String,
}

/// Output of `hl record verify` (SPEC-0008 §12.1).
#[derive(Debug, Clone, PartialEq)]
pub struct VerifyReport {
    /// The verified UTC date.
    pub date: String,
    /// Per-file checks.
    pub files: Vec<FileCheck>,
    /// Finished segments on disk with no manifest line (SPEC-0008 §17 #36).
    ///
    /// Each entry is the line the segment *should* have; coverage already
    /// counts its records, so a lost append no longer undercounts the day.
    /// These are repairable by [`repair_manifest`](super::repair::repair_manifest).
    pub orphans: Vec<ManifestEntry>,
    /// `.crashed` segments with no manifest line. A distinct category: they are
    /// recovered by restarting the recorder, never by [`repair_manifest`](super::repair::repair_manifest), and
    /// are a warning unless the caller passes `--strict`.
    pub crashed_no_manifest: Vec<UnmanifestedSegment>,
    /// Segments that decoded fully but do not end in `segment_close` and have no
    /// manifest line. Not finalizable by [`repair_manifest`](super::repair::repair_manifest) either: restarting
    /// the recorder only recovers `.partial` files, so `verify` fails unless
    /// `--allow-orphans`.
    pub unfinalized: Vec<UnmanifestedSegment>,
    /// Segments with no manifest line that did not decode at all. Repair skips
    /// them; `verify` fails unless `--allow-orphans`.
    pub corrupt_orphans: Vec<CorruptOrphan>,
    /// `*.partial` files present under the day's tree, if any. Their presence
    /// means a recorder may be running, so orphan findings are "possibly in
    /// flight".
    pub partials: Vec<String>,
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

/// One segment to verify: its manifest line, or an orphan found on disk.
struct SegmentTask {
    /// Path relative to `out_dir` (the manifest `file` field format).
    file: String,
    src: String,
    conn: String,
    first_t_ns: i64,
    /// The manifest line, when one exists.
    manifest: Option<ManifestEntry>,
}

/// Check a day's manifest against the files on disk and compute coverage.
///
/// The check is two-sided (SPEC-0008 §17 #36): it reports manifest lines whose
/// file is missing, size/record mismatches, and segments present on disk with
/// **no** manifest line. Coverage is computed over the union of manifest and
/// orphan segments, so a lost manifest append no longer undercounts the day.
/// Segments with no manifest line are split into the repairable finished
/// orphans ([`VerifyReport::orphans`]) and the distinct not-repairable
/// categories (crashed, unfinalized, corrupt), so the caller can warn instead of
/// fail where that is right ([`VerifyReport::crashed_no_manifest`]). Any
/// `*.partial` files found are reported so the caller can treat the orphans as
/// possibly in flight.
///
/// The recorder root must exist: a missing root is an error rather than a clean
/// empty report, so a typo in the profile cannot masquerade as "no data".
pub fn verify(config: &VerifyConfig) -> Result<VerifyReport, ReaderError> {
    let (day_start, day_end) =
        day_bounds(&config.date).ok_or_else(|| ReaderError::InvalidDate(config.date.clone()))?;
    ensure_root_exists(&config.out_dir, &config.network)?;

    let mut report = VerifyReport {
        date: config.date.clone(),
        files: Vec::new(),
        orphans: Vec::new(),
        crashed_no_manifest: Vec::new(),
        unfinalized: Vec::new(),
        corrupt_orphans: Vec::new(),
        partials: Vec::new(),
        coverage: Vec::new(),
    };

    // Manifest lines for the day, keyed by their (relative) file path so the
    // disk walk can tell which files already have a line.
    let mut manifest: BTreeMap<String, ManifestEntry> = BTreeMap::new();
    for path in manifests_for(&config.out_dir, &config.network, &config.date)? {
        for entry in read_manifest(&path)? {
            manifest.insert(entry.file.clone(), entry);
        }
    }

    // Every segment on disk for the day: manifest-known ones and orphans.
    let mut plan: Vec<SegmentTask> = Vec::new();
    for (file, entry) in &manifest {
        plan.push(SegmentTask {
            file: file.clone(),
            src: entry.src.clone(),
            conn: entry.conn.clone(),
            first_t_ns: entry.first_t_ns,
            manifest: Some(entry.clone()),
        });
    }
    for path in segments_for(&config.out_dir, &config.network, &config.date, &config.date)? {
        let file = rel_path(&config.out_dir, &path);
        if manifest.contains_key(&file) {
            continue;
        }
        let (src, conn, first_t_ns) = segment_identity(&config.out_dir, &path)?;
        plan.push(SegmentTask {
            file,
            src,
            conn,
            first_t_ns,
            manifest: None,
        });
    }

    // Visit each stream in time order so the gap state carried across segments
    // (`CoverageAcc`) sees the same sequence a replay would.
    plan.sort_by(|a, b| {
        (&a.src, &a.conn, a.first_t_ns, &a.file).cmp(&(&b.src, &b.conn, b.first_t_ns, &b.file))
    });

    let mut coverage: BTreeMap<(String, String), CoverageAcc> = BTreeMap::new();
    for task in &plan {
        let path = config.out_dir.join(&task.file);
        let Some(entry) = &task.manifest else {
            // An orphan: read it, feed coverage from its envelopes, and classify
            // it. A corrupt one is listed and skipped; it must not abort the
            // whole report (SPEC-0008 §17 #36).
            let envelopes = match read_envelopes(&path) {
                Ok(envelopes) => envelopes,
                Err(err) => {
                    report.corrupt_orphans.push(CorruptOrphan {
                        file: task.file.clone(),
                        error: err.to_string(),
                    });
                    continue;
                }
            };
            let acc = coverage
                .entry((task.src.clone(), task.conn.clone()))
                .or_default();
            acc.start_segment();
            for env in &envelopes {
                acc.observe(env);
            }
            acc.end_segment();
            if is_crashed(&path) {
                report.crashed_no_manifest.push(UnmanifestedSegment {
                    file: task.file.clone(),
                    src: task.src.clone(),
                    conn: task.conn.clone(),
                    records: envelopes.len() as u64,
                });
            } else if envelopes.last().map(|env| env.kind) == Some(Kind::SegmentClose) {
                match segment_manifest_entry(&config.out_dir, &path, &envelopes) {
                    Ok(entry) => report.orphans.push(entry),
                    Err(err) => report.corrupt_orphans.push(CorruptOrphan {
                        file: task.file.clone(),
                        error: err.to_string(),
                    }),
                }
            } else {
                report.unfinalized.push(UnmanifestedSegment {
                    file: task.file.clone(),
                    src: task.src.clone(),
                    conn: task.conn.clone(),
                    records: envelopes.len() as u64,
                });
            }
            continue;
        };
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
            file: entry.file.clone(),
            src: entry.src.clone(),
            conn: entry.conn.clone(),
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
    report.files.sort_by(|a, b| a.file.cmp(&b.file));
    report.orphans.sort_by(|a, b| a.file.cmp(&b.file));
    report
        .crashed_no_manifest
        .sort_by(|a, b| a.file.cmp(&b.file));
    report.unfinalized.sort_by(|a, b| a.file.cmp(&b.file));
    report.corrupt_orphans.sort_by(|a, b| a.file.cmp(&b.file));
    report.partials = partials_for_day(&config.out_dir, &config.network, &config.date)?
        .iter()
        .map(|path| rel_path(&config.out_dir, path))
        .collect();

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
