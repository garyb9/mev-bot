//! Orphan-segment manifest repair (SPEC-0008 §17 #36).

use std::collections::BTreeSet;
use std::path::PathBuf;

use crate::envelope::Kind;
use crate::segment::ManifestEntry;

use super::ReaderError;
use super::manifest::{
    append_manifest_entry, manifest_path_for_segment, manifests_for, read_manifest, rel_path,
    segment_manifest_entry,
};
use super::read::{ensure_root_exists, is_crashed, partials_for_day, read_envelopes, segments_for};

/// Inputs for [`repair_manifest`].
#[derive(Debug, Clone)]
pub struct RepairConfig {
    /// Root of the recording tree (usually `data/rec`).
    pub out_dir: PathBuf,
    /// Network directory name, `mainnet` or `testnet`.
    pub network: String,
    /// UTC date whose orphans are repaired, `YYYY-MM-DD`.
    pub date: String,
    /// Print what would be appended without writing anything.
    pub dry_run: bool,
}

/// One action taken by [`repair_manifest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepairAction {
    /// A missing manifest line that was appended (or, in dry-run, would be).
    Append {
        /// The reconstructed line.
        entry: ManifestEntry,
    },
    /// A segment that was left alone, with the reason.
    Skip {
        /// Path relative to `out_dir`.
        file: String,
        /// Why it was not repaired.
        reason: String,
    },
}

/// Append the missing manifest line for every orphan finished segment of a day
/// (SPEC-0008 §17 #36).
///
/// Only segments that decode fully, end in `segment_close`, and are not
/// `.crashed` are repaired; anything else is reported and left alone.
/// Idempotent (a segment with an existing manifest line is skipped),
/// append-only (no existing line or segment is ever rewritten or deleted), and
/// it never creates a directory — only `manifest.jsonl` files inside the day
/// directories that already hold the segments.
///
/// # Safety
///
/// Run this only while the recorder is **stopped**. There is deliberately no OS
/// lock: the recorder's finalize window (rename the segment, then append its
/// manifest line) looks exactly like an orphan, so a concurrent repair could
/// append a duplicate line, and two processes appending to one
/// `manifest.jsonl` is the cross-process lost-append race that the in-process
/// `manifest_append_lock` cannot cover. As a cheap fail-closed guard a real run
/// therefore refuses when any `*.partial` file is present under the day's tree,
/// which means a writer may be mid-segment. `--dry-run` writes nothing and is
/// safe at any time. A missing recorder root is an error, not an empty result.
pub fn repair_manifest(config: &RepairConfig) -> Result<Vec<RepairAction>, ReaderError> {
    ensure_root_exists(&config.out_dir, &config.network)?;
    if !config.dry_run
        && let Some(partial) = partials_for_day(&config.out_dir, &config.network, &config.date)?
            .into_iter()
            .next()
    {
        return Err(ReaderError::PartialPresent {
            path: partial.display().to_string(),
        });
    }

    let mut known: BTreeSet<String> = BTreeSet::new();
    for path in manifests_for(&config.out_dir, &config.network, &config.date)? {
        for entry in read_manifest(&path)? {
            known.insert(entry.file);
        }
    }

    let mut actions = Vec::new();
    for path in segments_for(&config.out_dir, &config.network, &config.date, &config.date)? {
        let file = rel_path(&config.out_dir, &path);
        if known.contains(&file) {
            continue;
        }
        if is_crashed(&path) {
            actions.push(RepairAction::Skip {
                file,
                reason: "crashed segment: recover by restarting the recorder; \
                         not repairable by repair-manifest"
                    .to_string(),
            });
            continue;
        }
        let envelopes = match read_envelopes(&path) {
            Ok(envelopes) => envelopes,
            Err(err) => {
                actions.push(RepairAction::Skip {
                    file,
                    reason: format!("decode failed: {err}"),
                });
                continue;
            }
        };
        if envelopes.last().map(|env| env.kind) != Some(Kind::SegmentClose) {
            actions.push(RepairAction::Skip {
                file,
                reason: "no segment_close: recover by restarting the recorder; \
                         not repairable by repair-manifest"
                    .to_string(),
            });
            continue;
        }
        let entry = match segment_manifest_entry(&config.out_dir, &path, &envelopes) {
            Ok(entry) => entry,
            Err(err) => {
                actions.push(RepairAction::Skip {
                    file,
                    reason: format!("could not derive the manifest line: {err}"),
                });
                continue;
            }
        };
        let Some(manifest) = manifest_path_for_segment(&path) else {
            actions.push(RepairAction::Skip {
                file,
                reason: "cannot locate the day directory".to_string(),
            });
            continue;
        };
        if !config.dry_run
            && let Err(err) = append_manifest_entry(&manifest, &entry)
        {
            actions.push(RepairAction::Skip {
                file: entry.file.clone(),
                reason: format!("append failed: {err}"),
            });
            continue;
        }
        actions.push(RepairAction::Append { entry });
    }
    Ok(actions)
}
