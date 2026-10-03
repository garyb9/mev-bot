//! Segment path layout, crash recovery, and directory creation (SPEC-0008 §6, R-14).

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use crate::mount_guard::{MountError, MountGuard};

use super::manifest::append_manifest_line;
use super::{ManifestEntry, SegmentConfig, SegmentError};

pub(super) fn recover_crashed(config: &SegmentConfig) -> Result<(), SegmentError> {
    let root = config.out_dir.join(&config.network).join(&config.src);
    if !root.is_dir() {
        return Ok(());
    }
    // Recovery renames `.partial` files and appends manifest lines: guard it.
    config.mount_guard.check_or_trip()?;
    let mut partials = Vec::new();
    collect_partials(&root, &mut partials)?;
    // Match the connection exactly by parsing the name: a prefix test would let
    // conn `hl-ws` claim `hl-ws-02`'s files.
    partials.retain(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .and_then(parse_partial_name)
            .is_some_and(|(conn, _)| conn == config.conn)
    });
    partials.sort();
    for partial in partials {
        let name = partial
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_string();
        let (conn, start_t_ns) =
            parse_partial_name(&name).unwrap_or_else(|| (config.conn.clone(), 0));
        let crashed = partial.with_extension("crashed");
        fs::rename(&partial, &crashed)?;
        let bytes_zst = fs::metadata(&crashed).map(|meta| meta.len()).unwrap_or(0);
        let entry = ManifestEntry {
            file: rel_path(&config.out_dir, &crashed),
            src: config.src.clone(),
            conn,
            first_t_ns: start_t_ns,
            last_t_ns: 0,
            records: 0,
            bytes_raw: 0,
            bytes_zst,
            crashed: true,
        };
        let manifest = crashed
            .parent()
            .and_then(Path::parent)
            .map(|day| day.join("manifest.jsonl"))
            .unwrap_or_else(|| manifest_path_for_t_ns(config, start_t_ns));
        append_manifest_line(&manifest, &entry, config)?;
    }
    Ok(())
}

fn collect_partials(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), SegmentError> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_partials(&path, out)?;
        } else if file_type.is_file()
            && let Some(name) = path.file_name().and_then(|name| name.to_str())
            && name.ends_with(".partial")
        {
            out.push(path);
        }
    }
    Ok(())
}

fn parse_partial_name(name: &str) -> Option<(String, i64)> {
    let stem = name.strip_suffix(".partial")?;
    let stem = stem.strip_suffix(".jsonl.zst")?;
    let (conn, timestamp) = stem.rsplit_once('-')?;
    let t_ns = timestamp.parse::<i64>().ok()?;
    Some((conn.to_string(), t_ns))
}

/// Create `dir` below `out_dir`, never creating `out_dir` itself (R-14 fix2 §3).
///
/// Unguarded this is `create_dir_all` (unchanged). Under a guard: re-check the
/// mount, fail closed (trip, create nothing) if `out_dir` itself is missing,
/// then create each level below it one at a time with `create_dir`.
pub(super) fn create_dir_below(
    out_dir: &Path,
    dir: &Path,
    guard: &MountGuard,
) -> Result<(), SegmentError> {
    if !guard.is_guarded() {
        fs::create_dir_all(dir)?;
        return Ok(());
    }
    guard.check_or_trip()?;
    if !out_dir.is_dir() {
        // `out_dir` vanished (or was never created): never recreate it.
        guard.trip();
        return Err(SegmentError::Mount(MountError::Missing {
            mount: out_dir.to_path_buf(),
        }));
    }
    let relative = dir.strip_prefix(out_dir).map_err(|_| {
        SegmentError::Mount(MountError::OutDirOutside {
            out_dir: dir.to_path_buf(),
            mount: out_dir.to_path_buf(),
        })
    })?;
    let mut level = out_dir.to_path_buf();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            continue;
        };
        level.push(part);
        match fs::create_dir(&level) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(SegmentError::Io(err)),
        }
    }
    Ok(())
}

pub(super) fn manifest_path_for_t_ns(config: &SegmentConfig, t_ns: i64) -> PathBuf {
    let (year, month, day, _) = utc_parts(t_ns);
    config
        .out_dir
        .join(&config.network)
        .join(&config.src)
        .join(format!("{year:04}-{month:02}-{day:02}"))
        .join("manifest.jsonl")
}

pub(super) fn rel_path(out_dir: &Path, path: &Path) -> String {
    path.strip_prefix(out_dir)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

pub(super) fn hour_key(t_ns: i64) -> i64 {
    t_ns.div_euclid(3_600_000_000_000)
}

pub(super) fn utc_parts(t_ns: i64) -> (i32, u32, u32, u32) {
    let secs = t_ns.div_euclid(1_000_000_000);
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    (year, month, day, (secs_of_day / 3600) as u32)
}

// Howard Hinnant's `civil_from_days`: days since 1970-01-01 to (year, month, day).
fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    (year as i32, month as u32, day as u32)
}
