//! Single-writer actor for zstd-compressed segment files (SPEC-0008 §6).
//!
//! The hot path only ever hands an [`Envelope`] to a bounded channel; a
//! dedicated OS thread owns the compressor and file handle and writes envelopes
//! in order. A full channel drops the envelope (`try_send` returns `false`)
//! rather than applying backpressure to the socket reader.
//!
//! One writer owns one `(src, conn)` stream. Segments rotate at the top of a
//! UTC hour or when the uncompressed size passes [`SegmentConfig::max_raw_bytes`].

use std::collections::HashMap;
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{
    Receiver, RecvTimeoutError, SyncSender, TrySendError, channel, sync_channel,
};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{debug, error, warn};

use crate::envelope::{Envelope, EnvelopeClock, SegmentOpenMeta, SystemEnvelopeClock};
use crate::mount_guard::{MOUNT_RECHECK_INTERVAL, MountError, MountGuard};

/// At most one "queue full" warning per this interval.
const DROP_WARN_INTERVAL: Duration = Duration::from_secs(5);

/// At most one per-stream recording-error warning per this interval after the
/// first; the rest are counted and reported with the next line (R-14 fix2 §1).
const ERROR_WARN_INTERVAL: Duration = Duration::from_secs(10);

/// How many consecutive I/O errors a guarded stream tolerates before it is
/// treated as a lost mount and stops (R-14 fix2 §1). An unknown errno must not
/// produce an unbounded retry loop.
const MAX_CONSECUTIVE_ERRORS: u32 = 3;

/// Default bounded wait for the writer thread to finish during shutdown.
const DEFAULT_SHUTDOWN_JOIN_TIMEOUT: Duration = Duration::from_secs(10);

/// Errors raised by the segment writer.
#[derive(Debug, Error)]
pub enum SegmentError {
    /// An I/O operation failed.
    #[error("segment i/o error: {0}")]
    Io(#[from] io::Error),
    /// An envelope could not be serialized.
    #[error("envelope serialization failed: {0}")]
    Encode(#[from] serde_json::Error),
    /// The `require_mount` guard refused a directory or file creation.
    #[error("mount guard: {0}")]
    Mount(#[from] MountError),
    /// The writer thread did not stop within the shutdown join timeout; it was
    /// abandoned and the mount guard was tripped (R-14 fix2 §2).
    #[error("segment writer did not stop within {timeout:?}")]
    ShutdownTimeout {
        /// The bounded join timeout that elapsed.
        timeout: Duration,
    },
}

/// Whether an I/O error means the segment's device or file vanished, which must
/// stop the stream rather than be retried (R-14 fix1 §5).
///
/// Errnos that a lost mount returns directly stop immediately. Any other write
/// error also stops when the mount probe itself fails: the probe is the
/// authority, so a fresh EACCES/ENOSPC on a vanished mount is caught too. The
/// probe runs only on an actual write error, never per envelope.
#[cfg(unix)]
fn is_lost_mount(err: &SegmentError, guard: &MountGuard) -> bool {
    let SegmentError::Io(io) = err else {
        return false;
    };
    if matches!(
        io.raw_os_error(),
        Some(libc::EIO)
            | Some(libc::ENOENT)
            | Some(libc::ENODEV)
            | Some(libc::ENOTCONN)
            | Some(libc::ESTALE)
            | Some(libc::EACCES)
    ) {
        return true;
    }
    guard.check().is_err()
}

#[cfg(not(unix))]
fn is_lost_mount(_err: &SegmentError, guard: &MountGuard) -> bool {
    guard.check().is_err()
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
    /// Optional `require_mount` guard (R-14). Defaults to unguarded.
    pub mount_guard: Arc<MountGuard>,
    /// Bounded wait for the writer thread during shutdown (R-14 fix2 §2).
    pub shutdown_join_timeout: Duration,
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
            mount_guard: Arc::new(MountGuard::unguarded()),
            shutdown_join_timeout: DEFAULT_SHUTDOWN_JOIN_TIMEOUT,
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

/// Rate-limits the per-drop warning so a persistently full queue cannot flood
/// the logs. The caller still owns the `hl_rec_dropped_total` metric.
struct DropLog {
    start: Instant,
    /// Monotonic ns of the last warning; `u64::MAX` until the first.
    last_warn_ns: AtomicU64,
    dropped: AtomicU64,
}

impl DropLog {
    fn new() -> Self {
        Self {
            start: Instant::now(),
            last_warn_ns: AtomicU64::new(u64::MAX),
            dropped: AtomicU64::new(0),
        }
    }

    /// Whether a warning is due now; wins the race for at most one per
    /// `DROP_WARN_INTERVAL`.
    fn should_warn(&self) -> bool {
        let now_ns = self.start.elapsed().as_nanos() as u64;
        let last = self.last_warn_ns.load(Ordering::Relaxed);
        if last != u64::MAX && now_ns.saturating_sub(last) < DROP_WARN_INTERVAL.as_nanos() as u64 {
            return false;
        }
        self.last_warn_ns
            .compare_exchange(last, now_ns, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    }

    /// Count a dropped envelope and return the running total.
    fn record_drop(&self) -> u64 {
        self.dropped.fetch_add(1, Ordering::Relaxed) + 1
    }
}

/// Rate-limits the per-stream recording-error warning. The first error logs
/// immediately; later ones are suppressed for `ERROR_WARN_INTERVAL` and their
/// count is reported with the next line that is allowed through.
struct StreamErrorLog {
    last_log: Option<Instant>,
    suppressed: u64,
}

impl StreamErrorLog {
    fn new() -> Self {
        Self {
            last_log: None,
            suppressed: 0,
        }
    }

    /// Record an error. Returns `Some(suppressed)` when a line is due now (the
    /// value is how many were suppressed since the previous line), else `None`.
    fn record(&mut self) -> Option<u64> {
        let now = Instant::now();
        match self.last_log {
            None => {
                self.last_log = Some(now);
                Some(0)
            }
            Some(last) if now.saturating_duration_since(last) >= ERROR_WARN_INTERVAL => {
                self.last_log = Some(now);
                Some(std::mem::take(&mut self.suppressed))
            }
            Some(_) => {
                self.suppressed = self.suppressed.saturating_add(1);
                None
            }
        }
    }

    /// Backdate the last log so the next `record` is due (test hook).
    #[cfg(test)]
    fn force_due(&mut self) {
        self.last_log = Instant::now().checked_sub(ERROR_WARN_INTERVAL + Duration::from_secs(1));
    }
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
    drop_log: DropLog,
    mount_guard: Arc<MountGuard>,
    shutdown_join_timeout: Duration,
}

impl SegmentWriter {
    /// Recover leftover `.partial` files for this connection and spawn the
    /// writer thread with a bounded queue of `channel_capacity` envelopes.
    pub fn spawn(config: SegmentConfig) -> Result<Self, SegmentError> {
        // Refuse to start before touching disk if the required mount is not a
        // real mount or `out_dir` escapes it (R-14 §1). Creates nothing.
        config.mount_guard.validate_startup(&config.out_dir)?;
        recover_crashed(&config)?;
        let (tx, rx) = sync_channel(config.channel_capacity.max(1));
        let src = config.src.clone();
        let conn = config.conn.clone();
        let mount_guard = config.mount_guard.clone();
        let shutdown_join_timeout = config.shutdown_join_timeout;
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
            drop_log: DropLog::new(),
            mount_guard,
            shutdown_join_timeout,
        })
    }

    /// Enqueue an envelope without blocking. Returns `false` if the queue was
    /// full (envelope dropped) or the writer has stopped; the caller is
    /// responsible for the drop metric and gap record (SPEC-0008 R-2).
    pub fn try_send(&self, env: Envelope) -> bool {
        match self.tx.try_send(Msg::Env(env)) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                let dropped = self.drop_log.record_drop();
                if self.drop_log.should_warn() {
                    warn!(
                        src = %self.src,
                        conn = %self.conn,
                        dropped,
                        "segment writer queue full; dropping envelopes"
                    );
                }
                false
            }
            Err(TrySendError::Disconnected(_)) => {
                error!(src = %self.src, conn = %self.conn, "segment writer thread stopped");
                false
            }
        }
    }

    /// Drain the queue, finalize the current segment, and stop the thread.
    ///
    /// The join is bounded by [`SegmentConfig::shutdown_join_timeout`]: a hung
    /// mount must not keep the process from exiting. On timeout the thread is
    /// abandoned, the guard is tripped, and [`SegmentError::ShutdownTimeout`]
    /// is returned so the recorder can exit non-zero without waiting (R-14
    /// fix2 §2).
    pub fn shutdown(mut self) -> Result<(), SegmentError> {
        self.stop_bounded()
    }

    fn stop_bounded(&mut self) -> Result<(), SegmentError> {
        let _ = self.tx.send(Msg::Shutdown);
        let Some(handle) = self.handle.take() else {
            return Ok(());
        };
        // Join from a helper thread so a hung writer cannot block us.
        let (done_tx, done_rx) = channel();
        let joiner = thread::Builder::new()
            .name(format!("rec-join-{}-{}", self.src, self.conn))
            .spawn(move || {
                let _ = handle.join();
                let _ = done_tx.send(());
            });
        let finished = match joiner {
            Ok(joiner) => match done_rx.recv_timeout(self.shutdown_join_timeout) {
                Ok(()) | Err(RecvTimeoutError::Disconnected) => {
                    let _ = joiner.join();
                    true
                }
                Err(RecvTimeoutError::Timeout) => false,
            },
            // Could not spawn the helper: fall back to a direct join.
            Err(_) => {
                let _ = done_rx.recv_timeout(self.shutdown_join_timeout);
                false
            }
        };
        if finished {
            return Ok(());
        }
        self.mount_guard.trip();
        warn!(
            src = %self.src,
            conn = %self.conn,
            timeout = ?self.shutdown_join_timeout,
            "segment writer did not stop within the join timeout; abandoning the thread"
        );
        Err(SegmentError::ShutdownTimeout {
            timeout: self.shutdown_join_timeout,
        })
    }
}

impl Drop for SegmentWriter {
    fn drop(&mut self) {
        // Best-effort: request shutdown and bound the join if the caller forgot.
        let _ = self.stop_bounded();
    }
}

fn run(config: SegmentConfig, rx: &Receiver<Msg>) {
    let mut state = WriterState::new(config);
    // A guarded writer must notice a disconnection within the 2 s watchdog even
    // when no segment rotates, so wake at least that often.
    let poll = if state.config.mount_guard.is_guarded() {
        state.config.flush_interval.min(MOUNT_RECHECK_INTERVAL)
    } else {
        state.config.flush_interval
    };
    loop {
        match rx.recv_timeout(poll) {
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
    last_mount_check: Instant,
    last_start_t_ns: i64,
    stopped: bool,
    /// Consecutive I/O errors on this stream; reset by any success.
    consecutive_errors: u32,
    /// Rate limiter for the recording-error warning.
    error_log: StreamErrorLog,
}

impl WriterState {
    fn new(config: SegmentConfig) -> Self {
        let now = Instant::now();
        Self {
            config,
            current: None,
            last_flush: now,
            last_disk_check: now,
            last_mount_check: now,
            last_start_t_ns: i64::MIN,
            stopped: false,
            consecutive_errors: 0,
            error_log: StreamErrorLog::new(),
        }
    }

    /// Handle an I/O error from any create/open/write/flush/finish/rename/
    /// manifest operation (R-14 fix2 §1).
    ///
    /// Returns `true` when the error was handled here (guarded stream), `false`
    /// when the caller should propagate it (unguarded stream, unchanged
    /// behaviour). Under a guard: the first error logs immediately and later
    /// ones at most once per [`ERROR_WARN_INTERVAL`] with a suppressed count; the
    /// mount probe is re-run on every error; the guard trips when the errno is a
    /// lost-mount errno, when the probe fails, or when the error repeats more
    /// than [`MAX_CONSECUTIVE_ERRORS`] times. Tripping stops the stream and
    /// returns `true`.
    fn on_stream_error(&mut self, err: &SegmentError) -> bool {
        if !self.config.mount_guard.is_guarded() {
            return false;
        }
        self.consecutive_errors = self.consecutive_errors.saturating_add(1);
        // `is_lost_mount` re-runs the probe only for non-lost errnos.
        let lost = is_lost_mount(err, &self.config.mount_guard)
            || self.consecutive_errors > MAX_CONSECUTIVE_ERRORS;
        if lost {
            self.stop_for_mount(&err.to_string());
            return true;
        }
        if let Some(suppressed) = self.error_log.record() {
            warn!(
                src = %self.config.src,
                conn = %self.config.conn,
                suppressed,
                error = %err,
                "recording i/o error under require_mount; retrying"
            );
        }
        true
    }

    /// Clear the consecutive-error state after a successful operation.
    fn clear_errors(&mut self) {
        self.consecutive_errors = 0;
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
            match OpenSegment::create(&self.config, &env, start_t_ns) {
                Ok(seg) => {
                    self.current = Some(seg);
                    self.clear_errors();
                }
                Err(SegmentError::Mount(err)) => {
                    self.stop_for_mount(&err.to_string());
                    return Ok(());
                }
                Err(err) => {
                    if self.on_stream_error(&err) {
                        return Ok(());
                    }
                    return Err(err);
                }
            }
        }
        if let Some(seg) = &mut self.current {
            match seg.write_raw(&line, &env) {
                Ok(()) => self.clear_errors(),
                Err(err) => {
                    if self.on_stream_error(&err) {
                        return Ok(());
                    }
                    return Err(err);
                }
            }
        }
        Ok(())
    }

    fn maintenance(&mut self) {
        if self.stopped {
            return;
        }
        // Re-check a required mount on a timer so a disconnect is noticed even
        // when no rotation is due (R-14 §4).
        if self.config.mount_guard.is_guarded()
            && self.last_mount_check.elapsed() >= MOUNT_RECHECK_INTERVAL
        {
            self.last_mount_check = Instant::now();
            if let Err(err) = self.config.mount_guard.check_or_trip() {
                self.stop_for_mount(&err.to_string());
                return;
            }
        }
        // The disk guard can only be acted on while a segment is open: the
        // `gap_start{reason:"disk"}` record is written into the current
        // segment, so tripping before the first envelope has opened one would
        // stop the stream and silently discard every later envelope with no
        // gap at all (SPEC-0008 R-2). An idle writer with no segment has
        // nothing to stop; the next envelope opens a segment and is written,
        // then the guard trips on the following maintenance pass.
        if self.current.is_some() && self.last_disk_check.elapsed() >= self.config.flush_interval {
            self.last_disk_check = Instant::now();
            if self.disk_low() {
                self.stop_for_disk();
                return;
            }
        }
        if self.last_flush.elapsed() >= self.config.flush_interval {
            match self.flush() {
                Ok(()) => self.clear_errors(),
                Err(err) => {
                    if !self.on_stream_error(&err) {
                        error!(error = %err, "segment flush failed");
                    }
                }
            }
        }
    }

    fn flush(&mut self) -> Result<(), SegmentError> {
        if let Some(seg) = &mut self.current {
            seg.flush()?;
        }
        self.last_flush = Instant::now();
        Ok(())
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

    /// Stop the stream because the required mount vanished (R-14 §2).
    ///
    /// The `gap_start{reason:"disk"}` is written through the *already open*
    /// file descriptor, which can only reach the original device, never the
    /// root disk; no path-based create, rename, or manifest append happens.
    /// The open segment is then dropped without finalizing: finalization is
    /// path-based (`fsync` + rename + manifest) and would risk the wrong disk,
    /// so the leftover `.partial` is left for crash recovery on the next start.
    fn stop_for_mount(&mut self, detail: &str) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        self.config.mount_guard.trip();
        let seq = self.current.as_ref().map_or(0, |seg| seg.last_seq);
        let gap = Envelope::gap_start(
            &*self.config.clock,
            &self.config.src,
            &self.config.conn,
            seq,
            "disk",
            "require_mount is no longer mounted",
        );
        if let Some(seg) = &mut self.current
            && let Ok(mut line) = serde_json::to_vec(&gap)
        {
            line.push(b'\n');
            if let Err(err) = seg.write_raw(&line, &gap) {
                debug!(error = %err, "failed to write mount gap record");
            }
            // Best effort: push the buffered compressed bytes to the existing
            // file handle so the gap can reach the (detached) file. This
            // creates nothing new and errors are ignored (R-14 fix2 §5).
            if let Err(err) = seg.flush_buffer() {
                debug!(error = %err, "failed to flush mount gap record");
            }
        }
        // Exactly one warning per stream; later envelopes are dropped silently
        // because `stopped` short-circuits `write` (R-14 fix1 §5).
        warn!(
            src = %self.config.src,
            conn = %self.config.conn,
            detail,
            "mount guard tripped; stopping stream"
        );
        self.current = None;
    }

    fn finalize_current(&mut self) {
        let Some(seg) = self.current.take() else {
            return;
        };
        match seg.finish(&self.config) {
            Ok(finished) => {
                self.clear_errors();
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
                if let Err(err) = append_manifest_line(&path, &entry, &self.config)
                    && !self.on_stream_error(&err)
                {
                    error!(error = %err, "manifest append failed");
                }
            }
            Err(err) => {
                if !self.on_stream_error(&err) {
                    error!(error = %err, "segment finalize failed");
                }
            }
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
        // Re-check the mount and create the level(s) without ever creating
        // `out_dir` itself (R-14 fix2 §3).
        create_dir_below(&config.out_dir, &dir, &config.mount_guard)?;
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

    /// Flush the compressor to the underlying file without an `fsync`. Used to
    /// try to land the final gap record on a lost mount; best effort only.
    fn flush_buffer(&mut self) -> io::Result<()> {
        self.encoder.flush()
    }

    fn finish(self, config: &SegmentConfig) -> Result<FinishedSegment, SegmentError> {
        // `fsync` + rename are path-based writes; re-check the mount first
        // (R-14 §2). On failure the segment is dropped and left as `.partial`.
        config.mount_guard.check_or_trip()?;
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

fn append_manifest_line(
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
    let _probe = append_probe::enter(path);
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

/// Create `dir` below `out_dir`, never creating `out_dir` itself (R-14 fix2 §3).
///
/// Unguarded this is `create_dir_all` (unchanged). Under a guard: re-check the
/// mount, fail closed (trip, create nothing) if `out_dir` itself is missing,
/// then create each level below it one at a time with `create_dir`.
fn create_dir_below(out_dir: &Path, dir: &Path, guard: &MountGuard) -> Result<(), SegmentError> {
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

/// Test-only instrumentation that records how many `append_manifest_line` calls
/// for one armed path overlap in time. Production code always sees a no-op.
///
/// The R-2b regression cannot be reproduced as lost bytes on the repository's
/// test filesystem (ext4 `O_APPEND` is atomic); instead the test proves the
/// invariant that prevents the 9p loss: appends for one manifest never overlap.
#[cfg(test)]
mod append_probe {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Mutex, OnceLock};
    use std::time::Duration;

    static HOLD_MS: AtomicUsize = AtomicUsize::new(0);
    static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
    static MAX_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
    static ARMED_PATH: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();

    fn armed_path() -> &'static Mutex<Option<PathBuf>> {
        ARMED_PATH.get_or_init(|| Mutex::new(None))
    }

    /// Arm the probe for `path`, holding each matching append for `hold_ms`.
    pub(super) fn arm(path: &Path, hold_ms: u64) {
        *armed_path()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(path.to_path_buf());
        HOLD_MS.store(hold_ms as usize, Ordering::SeqCst);
        IN_FLIGHT.store(0, Ordering::SeqCst);
        MAX_IN_FLIGHT.store(0, Ordering::SeqCst);
    }

    /// Stop recording and remove the artificial hold.
    pub(super) fn disarm() {
        *armed_path()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = None;
        HOLD_MS.store(0, Ordering::SeqCst);
    }

    /// Peak number of overlapping appends observed while armed.
    pub(super) fn max_in_flight() -> usize {
        MAX_IN_FLIGHT.load(Ordering::SeqCst)
    }

    /// A recorded append in progress; dropping it records the departure.
    pub(super) struct Guard;

    /// Record entry into an append for `path`, returning a guard that records
    /// departure. Only active while armed for exactly this path.
    pub(super) fn enter(path: &Path) -> Option<Guard> {
        let armed = {
            let armed = armed_path()
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            armed.as_deref() == Some(path)
        };
        if !armed {
            return None;
        }
        let concurrent = IN_FLIGHT.fetch_add(1, Ordering::SeqCst) + 1;
        MAX_IN_FLIGHT.fetch_max(concurrent, Ordering::SeqCst);
        let hold = HOLD_MS.load(Ordering::SeqCst);
        if hold > 0 {
            std::thread::sleep(Duration::from_millis(hold as u64));
        }
        Some(Guard)
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

#[cfg(test)]
mod tests;
