//! Clock discipline: the chrony offset/stratum probe (SPEC-0008 §11).

// ---------------------------------------------------------------------------
// Clock discipline (SPEC-0008 §11)
// ---------------------------------------------------------------------------

/// Parse a `chronyc -c tracking` CSV line into `(offset_ns, stratum)`.
///
/// The verified column order (V-1, 2026-09-28) is `RefID, RefName, Stratum,
/// RefTime, SystemTime, LastOffset, …`; `SystemTime` (the current offset in
/// seconds, `0.000012345` ≈ 12.3 µs) is column **4**. A line with fewer than
/// five columns is malformed and returns `None` rather than panicking.
pub(super) fn parse_chrony_tracking(line: &str) -> Option<(Option<i64>, Option<u8>)> {
    let columns: Vec<&str> = line.split(',').collect();
    // `SystemTime` (index 4) is the offset; index 3 is the RefTime epoch.
    let system_time = columns.get(4)?;
    let offset_ns = system_time
        .trim()
        .parse::<f64>()
        .ok()
        .map(|seconds| (seconds * 1_000_000_000.0) as i64);
    let stratum = columns
        .get(2)
        .and_then(|value| value.trim().parse::<u8>().ok());
    Some((offset_ns, stratum))
}

/// Read the chrony offset/stratum via `chronyc -c tracking`, if available.
pub(super) async fn chrony_tracking() -> (Option<i64>, Option<u8>) {
    let parsed = tokio::task::spawn_blocking(|| {
        let output = std::process::Command::new("chronyc")
            .args(["-c", "tracking"])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8(output.stdout).ok()?;
        let line = text.lines().next()?;
        parse_chrony_tracking(line)
    })
    .await
    .ok()
    .flatten();
    parsed.unwrap_or((None, None))
}
