//! `hl record inspect` / `verify` / `repair-manifest` command bodies
//! (SPEC-0008 §12.1, §17 #36).

use super::config::{load_profile, network_dir, profile_network};
use super::*;

// ---------------------------------------------------------------------------
// `hl record inspect` / `hl record verify`
// ---------------------------------------------------------------------------

/// Print the `inspect` report for one or more segment files or directories.
pub fn inspect(paths: &[PathBuf]) -> Result<()> {
    if paths.is_empty() {
        bail!("no paths given");
    }
    let mut files: Vec<PathBuf> = Vec::new();
    for path in paths {
        collect_segments(path, &mut files)?;
    }
    if files.is_empty() {
        bail!("no segment files found");
    }
    files.sort();
    let report = reader::inspect(&files)?;
    println!("files: {}", report.files);
    println!("records: {}", report.records);
    println!("crashed files: {}", report.crashed_files);
    if let (Some(first), Some(last)) = (report.first_t_ns, report.last_t_ns) {
        println!("first_t_ns: {first}");
        println!("last_t_ns: {last}");
    }
    println!("gaps: {} ({} ms)", report.gap_count, report.gap_total_ms);
    println!("by src:");
    for (src, count) in &report.by_src {
        println!("  {src:<16} {count}");
    }
    println!("by kind:");
    for (kind, count) in &report.by_kind {
        println!("  {kind:<16} {count}");
    }
    println!("by channel:");
    for (channel, count) in &report.by_channel {
        println!("  {channel:<16} {count}");
    }
    for holes in &report.seq_holes {
        println!(
            "seq holes: {}/{} missing {} value(s)",
            holes.src,
            holes.conn,
            holes.missing.len()
        );
    }
    Ok(())
}

/// Print the `verify` report for a UTC date.
pub fn verify(
    profile: Option<String>,
    network: Option<Network>,
    date: &str,
    allow_orphans: bool,
    strict: bool,
) -> Result<()> {
    let (name, profile) = load_profile(profile.as_deref(), network)?;
    let network = profile_network(&profile)?;
    let report = reader::verify(&reader::VerifyConfig {
        out_dir: profile.out_dir.clone(),
        network: network_dir(network).to_string(),
        date: date.to_string(),
    })?;
    println!("profile: {name}  network: {}", network_dir(network));
    println!("date: {}  files: {}", report.date, report.files.len());
    for file in &report.files {
        println!(
            "  {:<70} records={}{} size={}{}",
            file.file,
            file.records_manifest,
            if file.records_ok { "" } else { " MISMATCH" },
            file.bytes_zst_manifest,
            if file.size_ok { "" } else { " MISMATCH" }
        );
    }
    println!("partials: {}", report.partials.len());
    for partial in &report.partials {
        println!("  PARTIAL {partial}");
    }
    if !report.partials.is_empty() {
        println!(
            "  note: `.partial` files are present, so a recorder may be running; \
             orphan findings below are reported as possibly in flight and do not \
             fail verify"
        );
    }
    println!("orphans: {}", report.orphans.len());
    for orphan in &report.orphans {
        println!(
            "  ORPHAN {:<70} {}/{} records={}",
            orphan.file, orphan.src, orphan.conn, orphan.records
        );
    }
    println!(
        "crashed, no manifest line: {}",
        report.crashed_no_manifest.len()
    );
    for seg in &report.crashed_no_manifest {
        println!(
            "  CRASHED {:<70} {}/{} records={}",
            seg.file, seg.src, seg.conn, seg.records
        );
    }
    println!(
        "unfinalized, no manifest line: {}",
        report.unfinalized.len()
    );
    for seg in &report.unfinalized {
        println!(
            "  UNFINALIZED {:<70} {}/{} records={}",
            seg.file, seg.src, seg.conn, seg.records
        );
    }
    println!("corrupt orphans: {}", report.corrupt_orphans.len());
    for seg in &report.corrupt_orphans {
        println!("  CORRUPT {:<70} {}", seg.file, seg.error);
    }
    println!("coverage:");
    for stream in &report.coverage {
        println!(
            "  {}/{}: {:.2}% ({} ms)",
            stream.src, stream.conn, stream.coverage_pct, stream.covered_ms
        );
    }
    verify_exit(&report, allow_orphans, strict)
}

/// Decide `verify`'s exit status from the report (SPEC-0008 §17 #36).
///
/// Failing findings: repairable finished orphans (unless `--allow-orphans`, or
/// `*.partial` files are present and they may be in flight), corrupt orphans and
/// unfinalized segments (unless `--allow-orphans`). Warning only, unless
/// `--strict`: `.crashed` segments with no manifest line, which are recovered by
/// restarting the recorder.
pub(super) fn verify_exit(
    report: &reader::VerifyReport,
    allow_orphans: bool,
    strict: bool,
) -> Result<()> {
    let in_flight = !report.partials.is_empty();
    if in_flight {
        warn!(
            partials = report.partials.len(),
            "`.partial` files are present; treating orphan findings as possibly in flight"
        );
    }

    if !report.orphans.is_empty() {
        if allow_orphans || in_flight {
            warn!(
                orphans = report.orphans.len(),
                "finished orphans ignored ({})",
                if in_flight {
                    "possibly in flight; a `.partial` file is present"
                } else {
                    "--allow-orphans"
                }
            );
        } else {
            bail!(
                "{} finished segment(s) on disk have no manifest line (orphans); run \
                 `hl record repair-manifest --date {}` or pass --allow-orphans",
                report.orphans.len(),
                report.date
            );
        }
    }

    if !report.corrupt_orphans.is_empty() {
        if allow_orphans {
            warn!(
                corrupt_orphans = report.corrupt_orphans.len(),
                "corrupt orphan segments ignored because --allow-orphans was passed"
            );
        } else {
            let first = &report.corrupt_orphans[0];
            bail!(
                "{} segment(s) on disk have no manifest line and did not decode \
                 (corrupt orphans), e.g. `{}`: {}; not repairable by repair-manifest, \
                 recover by restarting the recorder; pass --allow-orphans to ignore",
                report.corrupt_orphans.len(),
                first.file,
                first.error
            );
        }
    }

    // A finalized-name segment without `segment_close` cannot be repaired by
    // `repair-manifest` (restarting the recorder only recovers `.partial`
    // files), so it fails like a corrupt orphan unless allowed.
    if !report.unfinalized.is_empty() {
        if allow_orphans {
            warn!(
                unfinalized = report.unfinalized.len(),
                "unfinalized segments ignored because --allow-orphans was passed"
            );
        } else {
            let first = &report.unfinalized[0];
            bail!(
                "{} segment(s) on disk have no manifest line and no segment_close \
                 (unfinalized), e.g. `{}`; not repairable by repair-manifest; pass \
                 --allow-orphans to ignore",
                report.unfinalized.len(),
                first.file
            );
        }
    }

    // A `.crashed` file without a manifest line is expected after a crash whose
    // manifest append was lost: warn, and fail only with `--strict`.
    if !report.crashed_no_manifest.is_empty() {
        if strict {
            bail!(
                "{} crashed segment(s) on disk have no manifest line; not repairable by \
                 repair-manifest, recover by restarting the recorder",
                report.crashed_no_manifest.len()
            );
        }
        warn!(
            crashed = report.crashed_no_manifest.len(),
            "crashed segments with no manifest line are not repairable by repair-manifest; \
             recover by restarting the recorder (pass --strict to fail)"
        );
    }
    Ok(())
}

/// Append the missing manifest line for each orphan finished segment (SPEC-0008
/// §17 #36). A maintenance tool: it never runs automatically and never creates a
/// directory.
pub fn repair_manifest(
    profile: Option<String>,
    network: Option<Network>,
    date: &str,
    dry_run: bool,
) -> Result<()> {
    let (name, profile) = load_profile(profile.as_deref(), network)?;
    let network = profile_network(&profile)?;
    let actions = reader::repair_manifest(&reader::RepairConfig {
        out_dir: profile.out_dir.clone(),
        network: network_dir(network).to_string(),
        date: date.to_string(),
        dry_run,
    })?;
    println!("profile: {name}  network: {}", network_dir(network));
    println!("date: {date}");
    let mut appended = 0usize;
    for action in &actions {
        match action {
            reader::RepairAction::Append { entry } => {
                appended += 1;
                if dry_run {
                    println!("would append: {}", serde_json::to_string(entry)?);
                } else {
                    println!("appended: {}", entry.file);
                }
            }
            reader::RepairAction::Skip { file, reason } => {
                println!("skipped: {file} ({reason})");
            }
        }
    }
    println!(
        "repaired: {appended}{}",
        if dry_run { " (dry run)" } else { "" }
    );
    Ok(())
}

/// Collect segment files under `path` (a file is used as-is; a directory is
/// walked recursively for `*.zst` / `*.crashed`).
pub(super) fn collect_segments(path: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    if path.is_dir() {
        for entry in
            std::fs::read_dir(path).with_context(|| format!("reading {}", path.display()))?
        {
            let entry = entry?;
            collect_segments(&entry.path(), out)?;
        }
    } else if is_segment_file(path) {
        out.push(path.to_path_buf());
    }
    Ok(())
}

pub(super) fn is_segment_file(path: &Path) -> bool {
    let name = path.to_string_lossy();
    name.ends_with(".jsonl.zst") || name.ends_with(".jsonl.zst.crashed")
}
