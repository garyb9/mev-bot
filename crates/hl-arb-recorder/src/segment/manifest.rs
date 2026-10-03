//! Serialized appends to a day manifest.jsonl (SPEC-0008 §6, R-2b).

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use super::paths::create_dir_below;
use super::{ManifestEntry, SegmentConfig, SegmentError};

/// Serializes manifest appends for one manifest path across every writer in the
/// process (R-2b).
///
/// Every `SegmentWriter` for a `(src, day)` appends to the same
/// `manifest.jsonl`, each with its own freshly-opened `O_APPEND` handle. On the
/// recorder's real output filesystem — a WSL 9p `drvfs` mount of the external
/// SSD — `O_APPEND` across independently-opened handles is **not** atomic: the
/// 9p client caches the file size, so two writers that open at the same instant
/// append at the same offset and one line is silently lost with no error.
/// Serializing the whole open→write→fsync→close per path means at most one
/// handle is appending at a time, which removes the race.
fn manifest_append_lock(path: &Path) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();
    let registry = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut locks = registry.lock().unwrap_or_else(|poison| poison.into_inner());
    locks.entry(path.to_path_buf()).or_default().clone()
}

pub(super) fn append_manifest_line(
    path: &Path,
    entry: &ManifestEntry,
    config: &SegmentConfig,
) -> Result<(), SegmentError> {
    // Hold one append at a time per manifest so concurrent writers cannot race
    // the 9p/drvfs append (R-2b). The lock is held across the guard check and
    // every create/open/write below, so the guard behaviour itself is unchanged.
    let lock = manifest_append_lock(path);
    let _append_guard = lock.lock().unwrap_or_else(|poison| poison.into_inner());
    #[cfg(test)]
    let _probe = super::append_probe::enter(path);
    // Re-check immediately before creating/opening (R-14 §2).
    config.mount_guard.check_or_trip()?;
    if let Some(parent) = path.parent() {
        create_dir_below(&config.out_dir, parent, &config.mount_guard)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    let mut line = serde_json::to_vec(entry)?;
    line.push(b'\n');
    file.write_all(&line)?;
    file.sync_data()?;
    Ok(())
}
