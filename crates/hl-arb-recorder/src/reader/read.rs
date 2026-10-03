//! Lazy segment reading, path enumeration, and k-way merging (SPEC-0008 §12.1).

use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

use crate::envelope::Envelope;

use super::{ReaderError, SegmentReader};

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
        super::day_bounds(from).ok_or_else(|| ReaderError::InvalidDate(from.to_string()))?;
    let (to_start, _) =
        super::day_bounds(to).ok_or_else(|| ReaderError::InvalidDate(to.to_string()))?;
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
            match super::day_bounds(name) {
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

pub(super) fn is_crashed(path: &Path) -> bool {
    path.to_string_lossy().ends_with(".crashed")
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

/// The recorder root (and the network sub-tree under it) must exist before an
/// analysis walks it. A missing root is an error, not a clean empty report, so a
/// wrong profile or network cannot look like "no data" (SPEC-0008 §17 #36).
/// This only reads; nothing is created.
pub(super) fn ensure_root_exists(out_dir: &Path, network: &str) -> Result<(), ReaderError> {
    if !out_dir.is_dir() {
        return Err(ReaderError::MissingRoot(out_dir.display().to_string()));
    }
    let root = out_dir.join(network);
    if !root.is_dir() {
        return Err(ReaderError::MissingRoot(root.display().to_string()));
    }
    Ok(())
}

/// Every `*.partial` file under the day's `{src}/{date}` trees, sorted.
///
/// A `.partial` means a recorder is writing a segment or crashed mid-segment,
/// so [`repair_manifest`](super::repair::repair_manifest) must not run concurrently with it. Unlike
/// [`segments_for`] a missing root is not an error here: the caller has already
/// checked the root, and "no partials" is the normal state.
pub(super) fn partials_for_day(
    out_dir: &Path,
    network: &str,
    date: &str,
) -> Result<Vec<PathBuf>, ReaderError> {
    let root = out_dir.join(network);
    let mut partials = Vec::new();
    let src_dirs = match fs::read_dir(&root) {
        Ok(dirs) => dirs,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(partials),
        Err(err) => return Err(err.into()),
    };
    for src in src_dirs {
        let day_dir = src?.path().join(date);
        if day_dir.is_dir() {
            collect_partials(&day_dir, &mut partials)?;
        }
    }
    partials.sort();
    Ok(partials)
}

/// Recursively collect `*.partial` files under `dir`.
fn collect_partials(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), ReaderError> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_partials(&path, out)?;
        } else if path.extension().is_some_and(|ext| ext == "partial") {
            out.push(path);
        }
    }
    Ok(())
}
