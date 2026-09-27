//! Single-writer actor for zstd-compressed segment files (SPEC-0008 §6).
//!
//! The hot path only ever hands an [`Envelope`] to a bounded channel; a
//! dedicated OS thread owns the compressor and file handle and writes envelopes
//! in order. A full channel drops the envelope (`try_send` returns `false`)
//! rather than applying backpressure to the socket reader.
//!
//! One writer owns one `(src, conn)` stream. Segments rotate at the top of a
//! UTC hour or when the uncompressed size passes [`SegmentConfig::max_raw_bytes`].

use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{debug, error, warn};

use crate::envelope::{Envelope, EnvelopeClock, SegmentOpenMeta, SystemEnvelopeClock};

/// Errors raised by the segment writer.
#[derive(Debug, Error)]
pub enum SegmentError {
    /// An I/O operation failed.
    #[error("segment i/o error: {0}")]
    Io(#[from] io::Error),
    /// An envelope could not be serialized.
    #[error("envelope serialization failed: {0}")]
    Encode(#[from] serde_json::Error),
}

/// A source of free-disk-space measurements, injectable for tests.
pub trait DiskSpace: Send + Sync + 'static {
    /// Free bytes available to the process at `path`.
    fn free_bytes(&self, path: &Path) -> io::Result<u64>;
}

/// Disk-space measurement backed by `statvfs(3)`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemDiskSpace;

impl DiskSpace for SystemDiskSpace {
    #[cfg(unix)]
    fn free_bytes(&self, path: &Path) -> io::Result<u64> {
        use std::os::unix::ffi::OsStrExt;
        let c_path = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
        // SAFETY: `c_path` is a valid NUL-terminated path and `stat` is a valid
        // out-parameter for the duration of the call.
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(stat.f_bavail as u64 * stat.f_frsize as u64)
    }

    #[cfg(not(unix))]
    fn free_bytes(&self, _path: &Path) -> io::Result<u64> {
        Ok(u64::MAX)
    }
}

/// Configuration for one `(src, conn)` segment writer.
pub struct SegmentConfig {
    /// Root of the recording tree (usually `data/rec`).
    pub out_dir: PathBuf,
    /// Network directory name, `mainnet` or `testnet`.
    pub network: String,
    /// Source id (SPEC-0008 §5.4).
    pub src: String,
    /// Connection id within the source.
    pub conn: String,
    /// zstd compression level (default 3).
    pub zstd_level: i32,
    /// Bounded channel capacity (default 65 536).
    pub channel_capacity: usize,
    /// Rotate when uncompressed bytes reach this value (default 1 GiB).
    pub max_raw_bytes: u64,
    /// Flush cadence (default 5 s).
    pub flush_interval: Duration,
    /// Stop the stream when free disk drops below this many GiB (default 20).
    pub min_free_gb: u64,
    /// Metadata written on the segment's `segment_open` line.
    pub segment_open_meta: SegmentOpenMeta,
    /// Clock used for records the writer originates itself.
    pub clock: Arc<dyn EnvelopeClock>,
    /// Free-disk-space source.
    pub disk: Arc<dyn DiskSpace>,
    /// Stream priority, lowest first; reserved for the multi-stream disk guard.
    pub priority: u8,
}

impl Default for SegmentConfig {
    fn default() -> Self {
        Self {
            out_dir: PathBuf::from("data/rec"),
            network: "mainnet".to_string(),
            src: String::new(),
            conn: String::new(),
            zstd_level: 3,
            channel_capacity: 65_536,
            max_raw_bytes: 1 << 30,
            flush_interval: Duration::from_secs(5),
            min_free_gb: 20,
            segment_open_meta: SegmentOpenMeta::default(),
            clock: Arc::new(SystemEnvelopeClock::new()),
            disk: Arc::new(SystemDiskSpace),
            priority: 0,
        }
    }
}

/// A command processed by the writer thread.
enum Msg {
    /// An envelope to append.
    Env(Envelope),
    /// Stop after draining everything queued ahead of it.
    Shutdown,
}

/// Handle to the segment writer thread.
///
/// Dropping the handle does not stop the thread; call
/// [`SegmentWriter::shutdown`] for a clean, fully-drained stop.
pub struct SegmentWriter {
    tx: SyncSender<Msg>,
    handle: Option<JoinHandle<()>>,
    src: String,
    conn: String,
}

impl SegmentWriter {
    /// Recover leftover `.partial` files for this connection and spawn the
    /// writer thread with a bounded queue of `channel_capacity` envelopes.
    pub fn spawn(config: SegmentConfig) -> Result<Self, SegmentError> {
        recover_crashed(&config)?;
        let (tx, rx) = sync_channel(config.channel_capacity.max(1));
        let src = config.src.clone();
        let conn = config.conn.clone();
        let name = format!("rec-seg-{src}-{conn}");
        let handle = thread::Builder::new()
            .name(name)
            .spawn(move || run(config, &rx))
            .map_err(SegmentError::Io)?;
        Ok(Self {
            tx,
            handle: Some(handle),
            src,
            conn,
        })
    }

    /// Enqueue an envelope without blocking. Returns `false` if the queue was
    /// full (envelope dropped) or the writer has stopped; the caller is
    /// responsible for the drop metric and gap record (SPEC-0008 R-2).
    pub fn try_send(&self, env: Envelope) -> bool {
        match self.tx.try_send(Msg::Env(env)) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                warn!(
                    src = %self.src,
                    conn = %self.conn,
                    "segment writer queue full; dropping envelope"
                );
                false
            }
            Err(TrySendError::Disconnected(_)) => {
                error!(src = %self.src, conn = %self.conn, "segment writer thread stopped");
                false
            }
        }
    }

    /// Drain the queue, finalize the current segment, stop the thread, and join
    /// it.
    pub fn shutdown(mut self) {
        let _ = self.tx.send(Msg::Shutdown);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for SegmentWriter {
    fn drop(&mut self) {
        // Best-effort: request shutdown and join if the caller forgot.
        let _ = self.tx.send(Msg::Shutdown);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn run(config: SegmentConfig, rx: &Receiver<Msg>) {
    let mut state = WriterState::new(config);
    loop {
        match rx.recv_timeout(state.config.flush_interval) {
            Ok(Msg::Env(env)) => {
                if let Err(err) = state.write(env) {
                    error!(error = %err, "segment write failed");
                }
                state.maintenance();
            }
            Ok(Msg::Shutdown) => break,
            Err(RecvTimeoutError::Timeout) => state.maintenance(),
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    state.finalize_current();
    debug!("segment writer stopped");
}

struct WriterState {
    config: SegmentConfig,
    current: Option<OpenSegment>,
    last_flush: Instant,
    last_disk_check: Instant,
    last_start_t_ns: i64,
    stopped: bool,
}

impl WriterState {
    fn new(config: SegmentConfig) -> Self {
        let now = Instant::now();
        Self {
            config,
            current: None,
            last_flush: now,
            last_disk_check: now,
            last_start_t_ns: i64::MIN,
            stopped: false,
        }
    }

    fn write(&mut self, env: Envelope) -> Result<(), SegmentError> {
        if self.stopped {
            return Ok(());
        }
        let mut line = serde_json::to_vec(&env)?;
        line.push(b'\n');
        let hour = hour_key(env.t_ns);
        let rotate = match &self.current {
            None => true,
            Some(seg) => {
                hour > seg.hour_key || seg.bytes_raw + line.len() as u64 > self.config.max_raw_bytes
            }
        };
        if rotate {
            self.finalize_current();
            let mut start_t_ns = env.t_ns;
            if start_t_ns <= self.last_start_t_ns {
                start_t_ns = self.last_start_t_ns.saturating_add(1);
            }
            self.last_start_t_ns = start_t_ns;
            self.current = Some(OpenSegment::create(&self.config, &env, start_t_ns)?);
        }
        if let Some(seg) = &mut self.current {
            seg.write_raw(&line, &env)?;
        }
        Ok(())
    }

    fn maintenance(&mut self) {
        if self.stopped {
            return;
        }
        if self.last_disk_check.elapsed() >= self.config.flush_interval {
            self.last_disk_check = Instant::now();
            if self.disk_low() {
                self.stop_for_disk();
                return;
            }
        }
        if self.last_flush.elapsed() >= self.config.flush_interval {
            self.flush();
        }
    }

    fn flush(&mut self) {
        if let Some(seg) = &mut self.current
            && let Err(err) = seg.flush()
        {
            error!(error = %err, "segment flush failed");
        }
        self.last_flush = Instant::now();
    }

    fn disk_low(&self) -> bool {
        match self.config.disk.free_bytes(&self.config.out_dir) {
            Ok(free) => free < self.config.min_free_gb.saturating_mul(1_000_000_000),
            Err(err) => {
                warn!(error = %err, "disk space check failed; continuing");
                false
            }
        }
    }

    fn stop_for_disk(&mut self) {
        self.stopped = true;
        let seq = self.current.as_ref().map_or(0, |seg| seg.last_seq);
        let gap = Envelope::gap_start(
            &*self.config.clock,
            &self.config.src,
            &self.config.conn,
            seq,
            "disk",
            "free disk below min_free_gb",
        );
        if let Some(seg) = &mut self.current
            && let Ok(mut line) = serde_json::to_vec(&gap)
        {
            line.push(b'\n');
            if let Err(err) = seg.write_raw(&line, &gap) {
                error!(error = %err, "failed to write disk gap record");
            }
        }
        warn!(
            src = %self.config.src,
            conn = %self.config.conn,
            "disk guard tripped; stopping stream"
        );
        self.finalize_current();
    }

    fn finalize_current(&mut self) {
        let Some(seg) = self.current.take() else {
            return;
        };
        match seg.finish(&self.config) {
            Ok(finished) => {
                let entry = ManifestEntry {
                    file: rel_path(&self.config.out_dir, &finished.final_path),
                    src: self.config.src.clone(),
                    conn: self.config.conn.clone(),
                    first_t_ns: finished.first_t_ns,
                    last_t_ns: finished.last_t_ns,
                    records: finished.records,
                    bytes_raw: finished.bytes_raw,
                    bytes_zst: finished.bytes_zst,
                    crashed: false,
                };
                let path = manifest_path_for_t_ns(&self.config, finished.first_t_ns);
                if let Err(err) = append_manifest_line(&path, &entry) {
                    error!(error = %err, "manifest append failed");
                }
            }
            Err(err) => error!(error = %err, "segment finalize failed"),
        }
    }
}

struct OpenSegment {
    encoder: zstd::stream::write::Encoder<'static, File>,
    partial_path: PathBuf,
    final_path: PathBuf,
    hour_key: i64,
    start_t_ns: i64,
    last_data_t_ns: i64,
    last_seq: u64,
    records: u64,
    bytes_raw: u64,
}

impl OpenSegment {
    fn create(
        config: &SegmentConfig,
        first_env: &Envelope,
        start_t_ns: i64,
    ) -> Result<Self, SegmentError> {
        let (year, month, day, hour) = utc_parts(start_t_ns);
        let dir = config
            .out_dir
            .join(&config.network)
            .join(&config.src)
            .join(format!("{year:04}-{month:02}-{day:02}"))
            .join(format!("{hour:02}"));
        fs::create_dir_all(&dir)?;
        let name = format!("{}-{start_t_ns}.jsonl.zst", config.conn);
        let final_path = dir.join(&name);
        let partial_path = dir.join(format!("{name}.partial"));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&partial_path)?;
        let mut encoder = zstd::stream::write::Encoder::new(file, config.zstd_level)?;
        let open = Envelope::segment_open(
            &*config.clock,
            &config.src,
            &config.conn,
            first_env.seq,
            &config.segment_open_meta,
        );
        let mut open_line = serde_json::to_vec(&open)?;
        open_line.push(b'\n');
        encoder.write_all(&open_line)?;
        Ok(Self {
            encoder,
            partial_path,
            final_path,
            hour_key: hour_key(start_t_ns),
            start_t_ns,
            last_data_t_ns: start_t_ns,
            last_seq: first_env.seq,
            records: 1,
            bytes_raw: open_line.len() as u64,
        })
    }

    fn write_raw(&mut self, line: &[u8], env: &Envelope) -> Result<(), SegmentError> {
        self.encoder.write_all(line)?;
        self.bytes_raw += line.len() as u64;
        self.records += 1;
        self.last_data_t_ns = env.t_ns;
        self.last_seq = env.seq;
        Ok(())
    }

    fn flush(&mut self) -> Result<(), SegmentError> {
        self.encoder.flush()?;
        self.encoder.get_ref().sync_data()?;
        Ok(())
    }

    fn finish(self, config: &SegmentConfig) -> Result<FinishedSegment, SegmentError> {
        let OpenSegment {
            mut encoder,
            partial_path,
            final_path,
            start_t_ns,
            last_data_t_ns,
            last_seq,
            records,
            bytes_raw,
            ..
        } = self;
        let close = Envelope::segment_close(
            &*config.clock,
            &config.src,
            &config.conn,
            last_seq,
            records,
            bytes_raw,
        );
        let mut close_line = serde_json::to_vec(&close)?;
        close_line.push(b'\n');
        encoder.write_all(&close_line)?;
        let records = records + 1;
        let bytes_raw = bytes_raw + close_line.len() as u64;
        let file = encoder.finish()?;
        file.sync_all()?;
        drop(file);
        fs::rename(&partial_path, &final_path)?;
        let bytes_zst = fs::metadata(&final_path)?.len();
        Ok(FinishedSegment {
            final_path,
            first_t_ns: start_t_ns,
            last_t_ns: last_data_t_ns,
            records,
            bytes_raw,
            bytes_zst,
        })
    }
}

struct FinishedSegment {
    final_path: PathBuf,
    first_t_ns: i64,
    last_t_ns: i64,
    records: u64,
    bytes_raw: u64,
    bytes_zst: u64,
}

/// One line of a day's `manifest.jsonl` (SPEC-0008 §6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEntry {
    /// Path of the finished segment, relative to `out_dir`.
    pub file: String,
    /// Source id.
    pub src: String,
    /// Connection id within the source.
    pub conn: String,
    /// First record's wall-clock time, ns since the epoch.
    pub first_t_ns: i64,
    /// Last record's wall-clock time, ns since the epoch.
    pub last_t_ns: i64,
    /// Number of lines in the file (including `segment_close` when clean).
    pub records: u64,
    /// Uncompressed bytes written.
    pub bytes_raw: u64,
    /// Compressed file size.
    pub bytes_zst: u64,
    /// Whether the segment was recovered from a crash.
    pub crashed: bool,
}

fn recover_crashed(config: &SegmentConfig) -> Result<(), SegmentError> {
    let root = config.out_dir.join(&config.network).join(&config.src);
    if !root.is_dir() {
        return Ok(());
    }
    let prefix = format!("{}-", config.conn);
    let mut partials = Vec::new();
    collect_partials(&root, &prefix, &mut partials)?;
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
        append_manifest_line(&manifest, &entry)?;
    }
    Ok(())
}

fn collect_partials(
    dir: &Path,
    conn_prefix: &str,
    out: &mut Vec<PathBuf>,
) -> Result<(), SegmentError> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_partials(&path, conn_prefix, out)?;
        } else if file_type.is_file()
            && let Some(name) = path.file_name().and_then(|name| name.to_str())
            && name.ends_with(".partial")
            && (conn_prefix.is_empty() || name.starts_with(conn_prefix))
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

fn append_manifest_line(path: &Path, entry: &ManifestEntry) -> Result<(), SegmentError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    let mut line = serde_json::to_vec(entry)?;
    line.push(b'\n');
    file.write_all(&line)?;
    file.sync_data()?;
    Ok(())
}

fn manifest_path_for_t_ns(config: &SegmentConfig, t_ns: i64) -> PathBuf {
    let (year, month, day, _) = utc_parts(t_ns);
    config
        .out_dir
        .join(&config.network)
        .join(&config.src)
        .join(format!("{year:04}-{month:02}-{day:02}"))
        .join("manifest.jsonl")
}

fn rel_path(out_dir: &Path, path: &Path) -> String {
    path.strip_prefix(out_dir)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn hour_key(t_ns: i64) -> i64 {
    t_ns.div_euclid(3_600_000_000_000)
}

fn utc_parts(t_ns: i64) -> (i32, u32, u32, u32) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::{FixedEnvelopeClock, Kind};
    use std::io::Read;
    use std::sync::atomic::{AtomicU64, Ordering};

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

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(tag: &str) -> PathBuf {
        let n = TEMP_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("mev-rec-{tag}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
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
        let dir = temp_dir("hour");
        let clock = Arc::new(FixedEnvelopeClock::new(H1, 0));
        let writer =
            SegmentWriter::spawn(config(&dir, clock.clone(), "hl-ws", "hl-ws-01")).unwrap();

        clock.set_t_ns(H1);
        assert!(writer.try_send(Envelope::frame(&*clock, "hl-ws", "hl-ws-01", 0, "a")));
        clock.set_t_ns(H2);
        assert!(writer.try_send(Envelope::frame(&*clock, "hl-ws", "hl-ws-01", 1, "b")));
        writer.shutdown();

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
        let dir = temp_dir("size");
        let clock = Arc::new(FixedEnvelopeClock::new(H1, 0));
        let mut cfg = config(&dir, clock.clone(), "hl-ws", "hl-ws-01");
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
        writer.shutdown();

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
        let dir = temp_dir("manifest");
        let clock = Arc::new(FixedEnvelopeClock::new(H1, 0));
        let mut cfg = config(&dir, clock.clone(), "hl-ws", "hl-ws-01");
        cfg.max_raw_bytes = 400;
        let writer = SegmentWriter::spawn(cfg).unwrap();
        for i in 0..10u64 {
            clock.set_t_ns(H1 + i as i64);
            assert!(writer.try_send(Envelope::frame(&*clock, "hl-ws", "hl-ws-01", i, "abcdef")));
        }
        writer.shutdown();

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
        let dir = temp_dir("crash");
        let hour_dir = dir.join("testnet/hl-ws/2023-11-14/22");
        fs::create_dir_all(&hour_dir).unwrap();
        let partial = hour_dir.join("hl-ws-01-1700000000000000000.jsonl.zst.partial");
        fs::write(&partial, b"truncated zstd bytes").unwrap();

        let clock = Arc::new(FixedEnvelopeClock::new(H1, 0));
        let writer = SegmentWriter::spawn(config(&dir, clock, "hl-ws", "hl-ws-01")).unwrap();
        writer.shutdown();

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
    fn records_written_equal_read_back() {
        let dir = temp_dir("roundtrip");
        let clock = Arc::new(FixedEnvelopeClock::new(H1, 0));
        let writer =
            SegmentWriter::spawn(config(&dir, clock.clone(), "hl-ws", "hl-ws-01")).unwrap();

        let total = 50u64;
        for i in 0..total {
            clock.set_t_ns(H1 + i as i64);
            assert!(writer.try_send(Envelope::frame(&*clock, "hl-ws", "hl-ws-01", i, "payload")));
        }
        writer.shutdown();

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
        let dir = temp_dir("disk");
        let clock = Arc::new(FixedEnvelopeClock::new(H1, 0));
        let mut cfg = config(&dir, clock.clone(), "hl-ws", "hl-ws-01");
        cfg.disk = Arc::new(FixedDiskSpace(0));
        cfg.flush_interval = Duration::ZERO;
        let writer = SegmentWriter::spawn(cfg).unwrap();

        assert!(writer.try_send(Envelope::frame(&*clock, "hl-ws", "hl-ws-01", 0, "x")));
        assert!(writer.try_send(Envelope::frame(&*clock, "hl-ws", "hl-ws-01", 1, "y")));
        writer.shutdown();

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
    fn throughput_meets_floor() {
        let dir = temp_dir("throughput");
        let clock = Arc::new(FixedEnvelopeClock::new(H1, 0));
        let mut cfg = config(&dir, clock.clone(), "hl-ws", "hl-ws-01");
        cfg.channel_capacity = 65_536;
        let writer = SegmentWriter::spawn(cfg).unwrap();

        let total: u64 = 100_000;
        let start = Instant::now();
        for i in 0..total {
            clock.set_mono_ns(i);
            let env = Envelope::frame(&*clock, "hl-ws", "hl-ws-01", i, "0123456789");
            while !writer.try_send(env.clone()) {
                std::hint::spin_loop();
            }
        }
        writer.shutdown();
        let elapsed = start.elapsed();
        let rate = total as f64 / elapsed.as_secs_f64();
        println!("segment writer throughput: {total} envelopes in {elapsed:?} ({rate:.0}/s)");
        assert!(rate >= 50_000.0, "throughput {rate:.0}/s below 50k/s floor");
    }
}
