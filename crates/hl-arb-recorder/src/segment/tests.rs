//! Tests for the segment writer, rotation, recovery, and mount guard.

use super::*;
use crate::envelope::{FixedEnvelopeClock, Kind};
use crate::mount_guard::test_support::FakeMountProbe;
use std::io::Read;

fn decode_records(path: &Path) -> Vec<Envelope> {
    let file = File::open(path).unwrap();
    let mut decoder = zstd::stream::read::Decoder::new(file).unwrap();
    let mut bytes = Vec::new();
    decoder.read_to_end(&mut bytes).unwrap();
    let text = String::from_utf8_lossy(&bytes);
    text.lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

const H1: i64 = 1_767_227_400_000_000_000; // 2026-01-01T00:30:00Z
const H2: i64 = 1_767_231_000_000_000_000; // 2026-01-01T01:30:00Z

fn temp_dir(tag: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("mev-rec-{tag}-"))
        .tempdir()
        .unwrap()
}

fn config(dir: &Path, clock: Arc<FixedEnvelopeClock>, src: &str, conn: &str) -> SegmentConfig {
    SegmentConfig {
        out_dir: dir.to_path_buf(),
        network: "testnet".into(),
        src: src.into(),
        conn: conn.into(),
        clock,
        ..SegmentConfig::default()
    }
}

fn read_records(path: &Path) -> Vec<Envelope> {
    decode_records(path)
}

fn read_manifest(path: &Path) -> Vec<ManifestEntry> {
    let text = fs::read_to_string(path).unwrap();
    text.lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_dir() {
            walk(&path, out);
        } else {
            out.push(path);
        }
    }
}

fn files_with_ext(dir: &Path, ext: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk(dir, &mut out);
    out.retain(|path| path.extension().and_then(|e| e.to_str()) == Some(ext));
    out.sort();
    out
}

struct FixedDiskSpace(u64);

impl DiskSpace for FixedDiskSpace {
    fn free_bytes(&self, _path: &Path) -> io::Result<u64> {
        Ok(self.0)
    }
}

#[test]
fn rotates_on_hour_boundary() {
    let tmp = temp_dir("hour");
    let dir = tmp.path();
    let clock = Arc::new(FixedEnvelopeClock::new(H1, 0));
    let writer = SegmentWriter::spawn(config(dir, clock.clone(), "hl-ws", "hl-ws-01")).unwrap();

    clock.set_t_ns(H1);
    assert!(writer.try_send(Envelope::frame(&*clock, "hl-ws", "hl-ws-01", 0, "a")));
    clock.set_t_ns(H2);
    assert!(writer.try_send(Envelope::frame(&*clock, "hl-ws", "hl-ws-01", 1, "b")));
    writer.shutdown().unwrap();

    let files = files_with_ext(&dir.join("testnet/hl-ws"), "zst");
    assert_eq!(files.len(), 2);
    assert!(
        files
            .iter()
            .any(|p| p.to_string_lossy().contains("/2026-01-01/00/"))
    );
    assert!(
        files
            .iter()
            .any(|p| p.to_string_lossy().contains("/2026-01-01/01/"))
    );
    for file in &files {
        let records = read_records(file);
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].kind, Kind::SegmentOpen);
        assert_eq!(records[1].kind, Kind::Frame);
        assert_eq!(records[2].kind, Kind::SegmentClose);
    }

    let manifest = read_manifest(&dir.join("testnet/hl-ws/2026-01-01/manifest.jsonl"));
    assert_eq!(manifest.len(), 2);
    assert!(manifest.iter().all(|entry| !entry.crashed));
}

#[test]
fn rotates_on_size() {
    let tmp = temp_dir("size");
    let dir = tmp.path();
    let clock = Arc::new(FixedEnvelopeClock::new(H1, 0));
    let mut cfg = config(dir, clock.clone(), "hl-ws", "hl-ws-01");
    cfg.max_raw_bytes = 500;
    let writer = SegmentWriter::spawn(cfg).unwrap();

    let total = 20u64;
    for i in 0..total {
        clock.set_t_ns(H1 + i as i64);
        let env = Envelope::frame(
            &*clock,
            "hl-ws",
            "hl-ws-01",
            i,
            "0123456789012345678901234567890123456789",
        );
        assert!(writer.try_send(env));
    }
    writer.shutdown().unwrap();

    let files = files_with_ext(&dir.join("testnet/hl-ws"), "zst");
    assert!(
        files.len() > 1,
        "expected size rotation, got {} segment(s)",
        files.len()
    );
    let frames: usize = files
        .iter()
        .flat_map(|file| read_records(file))
        .filter(|env| env.kind == Kind::Frame)
        .count();
    assert_eq!(frames as u64, total);
}

#[test]
fn manifest_line_matches_file() {
    let tmp = temp_dir("manifest");
    let dir = tmp.path();
    let clock = Arc::new(FixedEnvelopeClock::new(H1, 0));
    let mut cfg = config(dir, clock.clone(), "hl-ws", "hl-ws-01");
    cfg.max_raw_bytes = 400;
    let writer = SegmentWriter::spawn(cfg).unwrap();
    for i in 0..10u64 {
        clock.set_t_ns(H1 + i as i64);
        assert!(writer.try_send(Envelope::frame(&*clock, "hl-ws", "hl-ws-01", i, "abcdef")));
    }
    writer.shutdown().unwrap();

    let files = files_with_ext(&dir.join("testnet/hl-ws"), "zst");
    let manifest = read_manifest(&dir.join("testnet/hl-ws/2026-01-01/manifest.jsonl"));
    assert_eq!(manifest.len(), files.len());

    for entry in &manifest {
        let path = dir.join(&entry.file);
        assert!(path.exists(), "manifest file {} missing", entry.file);
        let records = read_records(&path);
        assert_eq!(entry.records, records.len() as u64);
        assert_eq!(entry.bytes_zst, fs::metadata(&path).unwrap().len());
        let decoded_len = decode_records(&path)
            .iter()
            .map(|env| serde_json::to_string(env).unwrap().len() as u64 + 1)
            .sum::<u64>();
        assert_eq!(entry.bytes_raw, decoded_len);

        let data: Vec<&Envelope> = records
            .iter()
            .filter(|env| env.kind == Kind::Frame)
            .collect();
        assert_eq!(entry.first_t_ns, data.first().unwrap().t_ns);
        assert_eq!(entry.last_t_ns, data.last().unwrap().t_ns);
    }
}

#[test]
fn partial_becomes_crashed_on_restart() {
    let tmp = temp_dir("crash");
    let dir = tmp.path();
    let hour_dir = dir.join("testnet/hl-ws/2023-11-14/22");
    fs::create_dir_all(&hour_dir).unwrap();
    let partial = hour_dir.join("hl-ws-01-1700000000000000000.jsonl.zst.partial");
    fs::write(&partial, b"truncated zstd bytes").unwrap();

    let clock = Arc::new(FixedEnvelopeClock::new(H1, 0));
    let writer = SegmentWriter::spawn(config(dir, clock, "hl-ws", "hl-ws-01")).unwrap();
    writer.shutdown().unwrap();

    assert!(!partial.exists());
    let crashed = hour_dir.join("hl-ws-01-1700000000000000000.jsonl.zst.crashed");
    assert!(crashed.exists());
    assert_eq!(
        fs::read(&crashed).unwrap(),
        b"truncated zstd bytes".to_vec()
    );

    let manifest = read_manifest(&dir.join("testnet/hl-ws/2023-11-14/manifest.jsonl"));
    assert_eq!(manifest.len(), 1);
    assert!(manifest[0].crashed);
    assert!(manifest[0].file.ends_with(".jsonl.zst.crashed"));
    assert_eq!(manifest[0].conn, "hl-ws-01");
    assert_eq!(manifest[0].first_t_ns, 1_700_000_000_000_000_000);
}

#[test]
fn recover_crashed_matches_the_exact_conn() {
    let tmp = temp_dir("crash-conn");
    let dir = tmp.path();
    let hour_dir = dir.join("testnet/hl-ws/2023-11-14/22");
    fs::create_dir_all(&hour_dir).unwrap();
    let mine = hour_dir.join("hl-ws-1700000000000000000.jsonl.zst.partial");
    let other = hour_dir.join("hl-ws-02-1700000000000000000.jsonl.zst.partial");
    fs::write(&mine, b"mine").unwrap();
    fs::write(&other, b"other").unwrap();

    let clock = Arc::new(FixedEnvelopeClock::new(H1, 0));
    let writer = SegmentWriter::spawn(config(dir, clock, "hl-ws", "hl-ws")).unwrap();
    writer.shutdown().unwrap();

    assert!(!mine.exists());
    assert!(
        hour_dir
            .join("hl-ws-1700000000000000000.jsonl.zst.crashed")
            .exists()
    );
    assert!(
        other.exists(),
        "conn `hl-ws` claimed `hl-ws-02`'s partial file"
    );
}

#[test]
fn records_written_equal_read_back() {
    let tmp = temp_dir("roundtrip");
    let dir = tmp.path();
    let clock = Arc::new(FixedEnvelopeClock::new(H1, 0));
    let writer = SegmentWriter::spawn(config(dir, clock.clone(), "hl-ws", "hl-ws-01")).unwrap();

    let total = 50u64;
    for i in 0..total {
        clock.set_t_ns(H1 + i as i64);
        assert!(writer.try_send(Envelope::frame(&*clock, "hl-ws", "hl-ws-01", i, "payload")));
    }
    writer.shutdown().unwrap();

    let files = files_with_ext(&dir.join("testnet/hl-ws"), "zst");
    assert_eq!(files.len(), 1);
    let records = read_records(&files[0]);
    assert_eq!(records.len() as u64, total + 2);
    assert_eq!(
        records.iter().filter(|env| env.kind == Kind::Frame).count() as u64,
        total
    );
    let manifest = read_manifest(&dir.join("testnet/hl-ws/2026-01-01/manifest.jsonl"));
    assert_eq!(manifest.len(), 1);
    assert_eq!(manifest[0].records, total + 2);
}

#[test]
fn disk_guard_stops_stream_and_emits_gap() {
    let tmp = temp_dir("disk");
    let dir = tmp.path();
    let clock = Arc::new(FixedEnvelopeClock::new(H1, 0));
    let mut cfg = config(dir, clock.clone(), "hl-ws", "hl-ws-01");
    cfg.disk = Arc::new(FixedDiskSpace(0));
    cfg.flush_interval = Duration::ZERO;
    let writer = SegmentWriter::spawn(cfg).unwrap();

    assert!(writer.try_send(Envelope::frame(&*clock, "hl-ws", "hl-ws-01", 0, "x")));
    assert!(writer.try_send(Envelope::frame(&*clock, "hl-ws", "hl-ws-01", 1, "y")));
    writer.shutdown().unwrap();

    let files = files_with_ext(&dir.join("testnet/hl-ws"), "zst");
    assert_eq!(files.len(), 1);
    let records = read_records(&files[0]);
    let gap = records
        .iter()
        .find(|env| env.kind == Kind::GapStart)
        .expect("disk gap record missing");
    assert_eq!(gap.meta.as_ref().unwrap()["reason"], "disk");
    assert_eq!(records.last().unwrap().kind, Kind::SegmentClose);
    assert!(
        records
            .iter()
            .any(|env| env.kind == Kind::Frame && env.seq == 0)
    );
    assert!(
        !records
            .iter()
            .any(|env| env.kind == Kind::Frame && env.seq == 1),
        "envelopes after the disk guard must not be written"
    );
}

#[test]
fn disk_guard_does_not_trip_before_any_segment_is_open() {
    let tmp = temp_dir("disk-idle");
    let dir = tmp.path();
    let clock = Arc::new(FixedEnvelopeClock::new(H1, 0));
    let mut cfg = config(dir, clock.clone(), "hl-ws", "hl-ws-01");
    cfg.disk = Arc::new(FixedDiskSpace(0));
    cfg.flush_interval = Duration::ZERO;

    // An idle pass with no open segment must not stop the stream: the gap
    // record is written into the current segment, so stopping here would
    // discard every later envelope without ever recording a gap.
    let mut state = WriterState::new(cfg);
    state.maintenance();
    assert!(!state.stopped, "disk guard tripped with no open segment");

    // The first envelope opens a segment and is written; the next pass
    // trips the guard and records the gap in that segment.
    state
        .write(Envelope::frame(&*clock, "hl-ws", "hl-ws-01", 0, "x"))
        .unwrap();
    state.maintenance();
    assert!(
        state.stopped,
        "disk guard did not trip once a segment was open"
    );
    assert!(state.current.is_none());

    let files = files_with_ext(&dir.join("testnet/hl-ws"), "zst");
    assert_eq!(files.len(), 1);
    assert!(
        read_records(&files[0])
            .iter()
            .any(|env| env.kind == Kind::GapStart)
    );
}

/// A roughly 1 KiB `l2Book`-like frame, the size real market-data frames
/// actually reach. `/benches/segment.rs` asserts the throughput floor.
fn realistic_frame() -> String {
    let level = |i: u32| {
        format!(
            "{{\"px\":\"{}.{:02}\",\"sz\":\"1.{:02}\",\"n\":{i}}}",
            60_000 + i,
            i,
            i
        )
    };
    let side: Vec<String> = (0..20).map(level).collect();
    format!(
        "{{\"channel\":\"l2Book\",\"data\":{{\"coin\":\"BTC\",\"time\":1700000000000,\"levels\":[[{}],[{}]]}}}}",
        side.join(","),
        side.join(",")
    )
}

#[test]
fn writes_realistic_frame_sizes() {
    let tmp = temp_dir("realistic");
    let dir = tmp.path();
    let clock = Arc::new(FixedEnvelopeClock::new(H1, 0));
    let mut cfg = config(dir, clock.clone(), "hl-ws", "hl-ws-01");
    cfg.channel_capacity = 65_536;
    let writer = SegmentWriter::spawn(cfg).unwrap();

    let total: u64 = 5_000;
    let payload = realistic_frame();
    for i in 0..total {
        clock.set_mono_ns(i);
        let env = Envelope::frame(&*clock, "hl-ws", "hl-ws-01", i, payload.clone());
        while !writer.try_send(env.clone()) {
            std::hint::spin_loop();
        }
    }
    writer.shutdown().unwrap();

    let frames: usize = files_with_ext(&dir.join("testnet/hl-ws"), "zst")
        .iter()
        .flat_map(|file| read_records(file))
        .filter(|env| env.kind == Kind::Frame)
        .count();
    assert_eq!(frames as u64, total);
}

// -- R-14 mount guard ---------------------------------------------------

/// Every path under `root` (directories and files), sorted. Used to prove
/// the writer creates nothing once the required mount is gone.
fn path_set(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_dir() {
                stack.push(path.clone());
            }
            out.push(path);
        }
    }
    out.sort();
    out
}

/// A guarded config rooted at `mount/mev-rec` with `mount` as the required
/// mount, built around a fake probe.
fn guarded_config(
    mount: &Path,
    clock: Arc<FixedEnvelopeClock>,
) -> (Arc<FakeMountProbe>, Arc<MountGuard>, SegmentConfig) {
    let out = mount.join("mev-rec");
    fs::create_dir_all(&out).unwrap();
    let probe = Arc::new(FakeMountProbe::new(mount));
    let guard = Arc::new(MountGuard::new(Some(mount.to_path_buf()), probe.clone()));
    guard.validate_startup(&out).unwrap();
    let mut cfg = config(&out, clock, "hl-ws", "hl-ws-01");
    cfg.mount_guard = guard.clone();
    (probe, guard, cfg)
}

#[test]
fn unset_require_mount_is_unguarded() {
    let cfg = SegmentConfig::default();
    assert!(
        !cfg.mount_guard.is_guarded(),
        "default config must not require a mount"
    );
}

#[test]
fn mount_guard_blocks_the_first_segment_without_creating_anything() {
    let tmp = temp_dir("mount-first");
    let mount = tmp.path();
    let clock = Arc::new(FixedEnvelopeClock::new(H1, 0));
    let (probe, guard, cfg) = guarded_config(mount, clock.clone());
    let mut state = WriterState::new(cfg);

    probe.set_mounted(false);
    let before = path_set(mount);
    state
        .write(Envelope::frame(&*clock, "hl-ws", "hl-ws-01", 0, "a"))
        .unwrap();

    assert!(
        state.stopped,
        "writer did not stop after the mount vanished"
    );
    assert!(guard.is_tripped());
    assert_eq!(
        path_set(mount),
        before,
        "a directory or file was created while unmounted"
    );
}

#[test]
fn mount_guard_stops_on_rotation_when_mount_disappears() {
    let tmp = temp_dir("mount-rotation");
    let mount = tmp.path();
    let clock = Arc::new(FixedEnvelopeClock::new(H1, 0));
    let (probe, guard, cfg) = guarded_config(mount, clock.clone());
    let mut state = WriterState::new(cfg);

    state
        .write(Envelope::frame(&*clock, "hl-ws", "hl-ws-01", 0, "a"))
        .unwrap();
    assert!(state.current.is_some());
    let before = path_set(mount);

    probe.set_mounted(false);
    clock.set_t_ns(H2); // force an hour rotation, which creates a new file
    state
        .write(Envelope::frame(&*clock, "hl-ws", "hl-ws-01", 1, "b"))
        .unwrap();

    assert!(
        state.stopped,
        "writer did not stop after the mount vanished"
    );
    assert!(state.current.is_none());
    assert!(guard.is_tripped());
    assert_eq!(
        path_set(mount),
        before,
        "a directory or file was created while unmounted"
    );
}

#[test]
fn mount_watchdog_stops_without_a_rotation() {
    let tmp = temp_dir("mount-watchdog");
    let mount = tmp.path();
    let clock = Arc::new(FixedEnvelopeClock::new(H1, 0));
    let (probe, guard, cfg) = guarded_config(mount, clock.clone());
    let mut state = WriterState::new(cfg);

    state
        .write(Envelope::frame(&*clock, "hl-ws", "hl-ws-01", 0, "a"))
        .unwrap();
    assert!(state.current.is_some());
    let before = path_set(mount);

    probe.set_mounted(false);
    // Force the 2 s watchdog to be due without sleeping.
    state.last_mount_check = Instant::now() - MOUNT_RECHECK_INTERVAL - Duration::from_secs(1);
    state.maintenance();

    assert!(state.stopped, "watchdog did not stop the stream");
    assert!(state.current.is_none());
    assert!(guard.is_tripped());
    assert_eq!(
        path_set(mount),
        before,
        "a directory or file was created while unmounted"
    );
}

#[test]
fn mount_guard_allows_a_normal_run_while_mounted() {
    let tmp = temp_dir("mount-ok");
    let mount = tmp.path();
    let clock = Arc::new(FixedEnvelopeClock::new(H1, 0));
    let (_probe, guard, cfg) = guarded_config(mount, clock.clone());
    let out = mount.join("mev-rec");
    let writer = SegmentWriter::spawn(cfg).unwrap();

    for i in 0..5u64 {
        clock.set_t_ns(H1 + i as i64);
        assert!(writer.try_send(Envelope::frame(&*clock, "hl-ws", "hl-ws-01", i, "x")));
    }
    writer.shutdown().unwrap();

    assert!(!guard.is_tripped());
    let files = files_with_ext(&out.join("testnet/hl-ws"), "zst");
    assert_eq!(
        files.len(),
        1,
        "mounted writer did not finalize its segment"
    );
    assert_eq!(
        read_records(&files[0])
            .iter()
            .filter(|env| env.kind == Kind::Frame)
            .count(),
        5
    );
}

#[cfg(unix)]
#[test]
fn lost_mount_io_errors_are_recognized() {
    let guard = MountGuard::unguarded();
    for code in [
        libc::EIO,
        libc::ENOENT,
        libc::ENODEV,
        libc::ENOTCONN,
        libc::ESTALE,
        libc::EACCES,
    ] {
        let err = SegmentError::Io(io::Error::from_raw_os_error(code));
        assert!(is_lost_mount(&err, &guard), "errno {code} not recognized");
    }
    // A non-mount error with a healthy (unguarded) probe is not a loss.
    let other = SegmentError::Io(io::Error::new(io::ErrorKind::PermissionDenied, "no"));
    assert!(!is_lost_mount(&other, &guard));
    let mount_err = SegmentError::Mount(MountError::Missing {
        mount: PathBuf::from("/mnt/e"),
    });
    assert!(!is_lost_mount(&mount_err, &guard));
}

#[test]
fn any_write_error_is_a_loss_when_the_probe_fails() {
    let tmp = temp_dir("lost-probe");
    let mount = tmp.path();
    let clock = Arc::new(FixedEnvelopeClock::new(H1, 0));
    let (probe, guard, _cfg) = guarded_config(mount, clock);
    probe.set_mounted(false);
    // An error that is not a lost-mount errno still trips when the probe
    // says the mount is gone.
    let other = SegmentError::Io(io::Error::new(io::ErrorKind::BrokenPipe, "gone"));
    assert!(is_lost_mount(&other, &guard));
}

// -- R-14 fix2 ----------------------------------------------------------

fn manifest_entry() -> ManifestEntry {
    ManifestEntry {
        file: "testnet/hl-ws/2026-01-01/00/hl-ws-01-1.jsonl.zst".to_string(),
        src: "hl-ws".to_string(),
        conn: "hl-ws-01".to_string(),
        first_t_ns: H1,
        last_t_ns: H1,
        records: 2,
        bytes_raw: 10,
        bytes_zst: 5,
        crashed: false,
    }
}

/// Item 1: an unknown errno repeated more than 3 times on a guarded stream
/// trips the mount guard, and the warning is rate-limited (one line for the
/// first three errors).
#[test]
fn repeated_unknown_errors_trip_the_guard_and_log_once() {
    let tmp = temp_dir("repeat-errors");
    let mount = tmp.path();
    let clock = Arc::new(FixedEnvelopeClock::new(H1, 0));
    let (_probe, guard, cfg) = guarded_config(mount, clock);
    let mut state = WriterState::new(cfg);
    let err = || SegmentError::Io(io::Error::new(io::ErrorKind::PermissionDenied, "x"));

    for _ in 0..3 {
        assert!(state.on_stream_error(&err()), "guarded error not handled");
    }
    assert!(!state.stopped, "three errors must not stop the stream yet");
    assert!(!guard.is_tripped());
    assert_eq!(state.consecutive_errors, 3);
    assert_eq!(
        state.error_log.suppressed, 2,
        "only the first of three errors should have logged a line"
    );

    // The fourth consecutive error fails closed.
    assert!(state.on_stream_error(&err()));
    assert!(state.stopped);
    assert!(guard.is_tripped());
}

#[test]
fn error_log_rate_limits_suppressed_lines() {
    let mut log = StreamErrorLog::new();
    assert_eq!(log.record(), Some(0), "first error logs immediately");
    assert_eq!(log.record(), None);
    assert_eq!(log.record(), None);
    assert_eq!(log.suppressed, 2);
    log.force_due();
    assert_eq!(log.record(), Some(2), "next window reports the suppressed");
    assert_eq!(log.suppressed, 0);
}

/// Item 3: `out_dir` must never be recreated by the writer; if it is
/// missing the stream fails closed and trips the guard.
#[test]
fn writer_never_recreates_out_dir() {
    let tmp = temp_dir("no-recreate");
    let mount = tmp.path();
    let clock = Arc::new(FixedEnvelopeClock::new(H1, 0));
    let (_probe, guard, cfg) = guarded_config(mount, clock.clone());
    let out = mount.join("mev-rec");
    // Simulate `out_dir` vanishing between the check and the create.
    fs::remove_dir(&out).unwrap();
    let mut state = WriterState::new(cfg);

    state
        .write(Envelope::frame(&*clock, "hl-ws", "hl-ws-01", 0, "a"))
        .unwrap();

    assert!(state.stopped);
    assert!(guard.is_tripped());
    assert!(
        !out.exists(),
        "out_dir must never be recreated by the writer"
    );
}

/// Item 6: removing the guard before the manifest append would create the
/// day directory and the manifest file.
#[test]
fn manifest_append_is_guarded_when_the_mount_is_gone() {
    let tmp = temp_dir("manifest-guard");
    let mount = tmp.path();
    let clock = Arc::new(FixedEnvelopeClock::new(H1, 0));
    let (probe, guard, cfg) = guarded_config(mount, clock);
    probe.set_mounted(false);

    let before = path_set(mount);
    let path = cfg.out_dir.join("testnet/hl-ws/2026-01-01/manifest.jsonl");
    let err = append_manifest_line(&path, &manifest_entry(), &cfg).unwrap_err();

    assert!(matches!(err, SegmentError::Mount(_)), "{err}");
    assert!(guard.is_tripped());
    assert_eq!(path_set(mount), before, "manifest append created something");
}

/// Item 6: removing the guard before recovery would rename the `.partial`
/// and create a manifest on the lost mount.
#[test]
fn recover_crashed_is_guarded_when_the_mount_is_gone() {
    let tmp = temp_dir("recover-guard");
    let mount = tmp.path();
    let out = mount.join("mev-rec");
    let hour_dir = out.join("testnet/hl-ws/2023-11-14/22");
    fs::create_dir_all(&hour_dir).unwrap();
    let partial = hour_dir.join("hl-ws-01-1700000000000000000.jsonl.zst.partial");
    fs::write(&partial, b"truncated").unwrap();

    let clock = Arc::new(FixedEnvelopeClock::new(H1, 0));
    let probe = Arc::new(FakeMountProbe::new(mount));
    let guard = Arc::new(MountGuard::new(Some(mount.to_path_buf()), probe.clone()));
    guard.validate_startup(&out).unwrap();
    let mut cfg = config(&out, clock, "hl-ws", "hl-ws-01");
    cfg.mount_guard = guard.clone();

    probe.set_mounted(false);
    let before = path_set(mount);
    let err = recover_crashed(&cfg).unwrap_err();

    assert!(matches!(err, SegmentError::Mount(_)), "{err}");
    assert!(guard.is_tripped());
    assert!(partial.exists(), "partial must not be renamed");
    assert_eq!(path_set(mount), before, "recovery wrote something");
}

/// Item 2: a writer thread that hangs in a mount probe must not block
/// shutdown; the join times out, the guard trips and a typed error returns.
#[test]
fn shutdown_times_out_and_trips_the_guard() {
    let tmp = temp_dir("shutdown-timeout");
    let mount = tmp.path();
    let clock = Arc::new(FixedEnvelopeClock::new(H1, 0));
    let (probe, guard, mut cfg) = guarded_config(mount, clock.clone());
    cfg.shutdown_join_timeout = Duration::from_millis(50);
    let writer = SegmentWriter::spawn(cfg).unwrap();

    // Block the writer thread inside its next mount probe.
    probe.set_blocking(true);
    assert!(writer.try_send(Envelope::frame(&*clock, "hl-ws", "hl-ws-01", 0, "a")));
    std::thread::sleep(Duration::from_millis(50));

    let err = writer.shutdown().unwrap_err();
    assert!(matches!(err, SegmentError::ShutdownTimeout { .. }), "{err}");
    assert!(guard.is_tripped());

    // Release the abandoned thread so it can finish.
    probe.set_blocking(false);
}

// -- R-2b: manifest append concurrency ----------------------------------

/// Concurrent appends to one manifest must not overlap. On the recorder's
/// 9p/drvfs SSD mount, overlapping `O_APPEND` handles silently drop lines
/// (R-2b); this test fails before the per-path append lock because the
/// probe observes several appends in flight at once.
#[test]
fn concurrent_manifest_appends_are_serialized() {
    let tmp = temp_dir("manifest-race");
    let dir = tmp.path().to_path_buf();
    let path = dir.join("testnet/hl-ws/2026-01-01/manifest.jsonl");
    let entry = manifest_entry();
    let threads = 8;
    append_probe::arm(&path, 25);

    let barrier = Arc::new(std::sync::Barrier::new(threads));
    let handles: Vec<_> = (0..threads)
        .map(|_| {
            let path = path.clone();
            let entry = entry.clone();
            let dir = dir.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let cfg = config(
                    &dir,
                    Arc::new(FixedEnvelopeClock::new(H1, 0)),
                    "hl-ws",
                    "hl-ws-01",
                );
                barrier.wait();
                append_manifest_line(&path, &entry, &cfg).unwrap();
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }
    append_probe::disarm();

    assert_eq!(
        append_probe::max_in_flight(),
        1,
        "manifest appends for one path overlapped; concurrent 9p appends can drop lines (R-2b)"
    );
    assert_eq!(read_manifest(&path).len(), threads);
}

/// Eight writers for one `(src, day)` finalize at once through independent
/// `SegmentWriter`s; every finalized segment must have a manifest line.
#[test]
fn every_finalized_segment_gets_a_manifest_line_under_concurrency() {
    let tmp = temp_dir("manifest-concurrent");
    let dir = tmp.path().to_path_buf();
    let threads = 8u64;
    let barrier = Arc::new(std::sync::Barrier::new(threads as usize));
    let handles: Vec<_> = (0..threads)
        .map(|i| {
            let conn = format!("hl-ws-{i:02}");
            let dir = dir.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let clock = Arc::new(FixedEnvelopeClock::new(H1, 0));
                let writer = SegmentWriter::spawn(config(&dir, clock.clone(), "hl-ws", &conn))
                    .expect("spawn writer");
                barrier.wait();
                for seq in 0..20u64 {
                    clock.set_t_ns(H1 + seq as i64);
                    assert!(writer.try_send(Envelope::frame(&*clock, "hl-ws", &conn, seq, "data")));
                }
                writer.shutdown().expect("finalize writer");
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }

    let segments = files_with_ext(&dir.join("testnet/hl-ws"), "zst");
    assert_eq!(segments.len(), threads as usize);
    let manifest = read_manifest(&dir.join("testnet/hl-ws/2026-01-01/manifest.jsonl"));
    assert_eq!(
        manifest.len(),
        segments.len(),
        "a finalized segment is missing its manifest line"
    );
    let listed: std::collections::HashSet<String> =
        manifest.iter().map(|entry| entry.file.clone()).collect();
    for segment in &segments {
        let rel = rel_path(&dir, segment);
        assert!(listed.contains(&rel), "{rel} has no manifest line");
    }
}
