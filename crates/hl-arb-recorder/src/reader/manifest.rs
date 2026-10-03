//! Segment identity, manifest-line reconstruction, and manifest I/O (SPEC-0008 §6).

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::envelope::{Envelope, Kind};
use crate::segment::ManifestEntry;

use super::ReaderError;
use super::read::is_crashed;

/// `(src, conn, first_t_ns)` for a segment path, from its directory and name.
///
/// The name is `{conn}-{first_t_ns}.jsonl.zst[.crashed]`; `first_t_ns` is the
/// same timestamp `segment.rs` uses for the manifest's `first_t_ns`, and the
/// source is the directory level under the network root.
pub(super) fn segment_identity(
    out_dir: &Path,
    path: &Path,
) -> Result<(String, String, i64), ReaderError> {
    let invalid = || ReaderError::InvalidSegment {
        path: path.display().to_string(),
    };
    let src = path
        .strip_prefix(out_dir)
        .ok()
        .and_then(|rel| rel.components().nth(1))
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .ok_or_else(invalid)?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(invalid)?;
    let stem = name.strip_suffix(".crashed").unwrap_or(name);
    let stem = stem.strip_suffix(".jsonl.zst").ok_or_else(invalid)?;
    let (conn, timestamp) = stem.rsplit_once('-').ok_or_else(invalid)?;
    let first_t_ns = timestamp.parse::<i64>().map_err(|_| invalid())?;
    Ok((src, conn.to_string(), first_t_ns))
}

/// Reconstruct the manifest line a finished (or recovered) segment must have,
/// from its own records and its size on disk (SPEC-0008 §6, §17 #36).
///
/// `records` is the total decoded line count and `bytes_raw` the summed
/// uncompressed line lengths including each newline, which matches what
/// `segment.rs` writes. `first_t_ns` comes from the file name and `last_t_ns`
/// from the last envelope written before `segment_close` (or `first_t_ns` when
/// the file holds only `segment_open`/`segment_close`). Nothing is invented: a
/// field the segment cannot supply is an error.
pub fn segment_manifest_entry(
    out_dir: &Path,
    path: &Path,
    envelopes: &[Envelope],
) -> Result<ManifestEntry, ReaderError> {
    let (src, conn, first_t_ns) = segment_identity(out_dir, path)?;
    let mut bytes_raw = 0u64;
    for env in envelopes {
        let len = serde_json::to_vec(env)
            .map_err(|source| ReaderError::ManifestEncode {
                path: path.display().to_string(),
                source,
            })?
            .len() as u64
            + 1;
        bytes_raw += len;
    }
    let last_t_ns = envelopes
        .iter()
        .rev()
        .find(|env| !matches!(env.kind, Kind::SegmentOpen | Kind::SegmentClose))
        .map_or(first_t_ns, |env| env.t_ns);
    Ok(ManifestEntry {
        file: rel_path(out_dir, path),
        src,
        conn,
        first_t_ns,
        last_t_ns,
        records: envelopes.len() as u64,
        bytes_raw,
        bytes_zst: fs::metadata(path)?.len(),
        crashed: is_crashed(path),
    })
}

/// Append one reconstructed manifest line, creating `manifest.jsonl` if needed.
///
/// The line is byte-identical to the writer's output (the same `serde_json`
/// serialization of [`ManifestEntry`]). No parent directory is created; callers
/// only pass a manifest path whose day directory already exists.
pub fn append_manifest_entry(path: &Path, entry: &ManifestEntry) -> Result<(), ReaderError> {
    let mut line = serde_json::to_vec(entry).map_err(|source| ReaderError::ManifestEncode {
        path: path.display().to_string(),
        source,
    })?;
    line.push(b'\n');
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(&line)?;
    file.sync_data()?;
    Ok(())
}

/// The day `manifest.jsonl` that a `…/{date}/{hour}/…` segment belongs to.
pub(super) fn manifest_path_for_segment(segment: &Path) -> Option<PathBuf> {
    segment
        .parent()
        .and_then(Path::parent)
        .map(|day| day.join("manifest.jsonl"))
}

/// A segment path relative to `out_dir`, with `/` separators (the manifest
/// `file` format).
pub(super) fn rel_path(out_dir: &Path, path: &Path) -> String {
    path.strip_prefix(out_dir)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

pub(super) fn read_manifest(path: &Path) -> Result<Vec<ManifestEntry>, ReaderError> {
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

pub(super) fn manifests_for(
    out_dir: &Path,
    network: &str,
    date: &str,
) -> Result<Vec<PathBuf>, ReaderError> {
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
