//! Mount guard for the recorder output directory (SPEC-0008 §7 disk guard, R-14).
//!
//! The recorder may be pointed at an external drive (for example a WSL `drvfs`
//! mount at `/mnt/e`). If the drive disconnects, the mount point becomes an
//! ordinary empty directory on the root disk, so any path-based write under it
//! would silently land on the wrong disk. [`MountGuard`] makes that impossible:
//! when a profile sets `require_mount`, the writer refuses to start unless the
//! path really is a mount, refuses to start unless `out_dir` is inside it, and
//! re-checks the mount before every directory or file creation, stopping the
//! stream outright (never falling back to another directory) if it disappears.
//!
//! The device check is cheap (a few `stat` calls) and deliberately boring: a
//! path is a mount point when it exists, is a directory, its `st_dev` differs
//! from its parent's, and that `st_dev` also differs from `/`'s (belt and
//! braces against a same-device bind-like setup). No external commands and no
//! `unsafe` beyond what the crate already relies on for `statvfs`.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use thiserror::Error;

/// How often the writer re-checks a required mount while running (R-14 §4).
pub const MOUNT_RECHECK_INTERVAL: Duration = Duration::from_secs(2);

/// Linux mount table read to verify `require_mount_source`.
const MOUNTINFO: &str = "/proc/self/mountinfo";

/// Errors raised by the mount guard.
#[derive(Debug, Error)]
pub enum MountError {
    /// The required mount path does not exist.
    #[error("require_mount `{mount}` does not exist")]
    Missing {
        /// The configured `require_mount` path.
        mount: PathBuf,
    },
    /// The required mount path exists but is not a directory.
    #[error("require_mount `{mount}` is not a directory")]
    NotADirectory {
        /// The configured `require_mount` path.
        mount: PathBuf,
    },
    /// The required mount path is not a mount point (same device as its parent).
    #[error("require_mount `{mount}` is not a mount point (same device as its parent)")]
    NotAMount {
        /// The configured `require_mount` path.
        mount: PathBuf,
    },
    /// The required mount path is on the same device as `/`.
    #[error("require_mount `{mount}` is on the same device as `/`")]
    SameDeviceAsRoot {
        /// The configured `require_mount` path.
        mount: PathBuf,
    },
    /// `out_dir` is not lexically inside `require_mount`.
    #[error("out_dir `{out_dir}` is not inside require_mount `{mount}`")]
    OutDirOutside {
        /// The configured `out_dir`.
        out_dir: PathBuf,
        /// The configured `require_mount`.
        mount: PathBuf,
    },
    /// `out_dir` is the mount root itself; it must be strictly inside.
    #[error("out_dir `{mount}` is the require_mount root itself; it must be a child path")]
    OutDirIsMount {
        /// The configured `require_mount`, equal to `out_dir`.
        mount: PathBuf,
    },
    /// `out_dir` canonicalizes to a path outside `require_mount`.
    #[error("out_dir `{out_dir}` escapes require_mount `{mount}` (canonical path `{canonical}`)")]
    SymlinkEscape {
        /// The configured `out_dir`.
        out_dir: PathBuf,
        /// The configured `require_mount`.
        mount: PathBuf,
        /// The canonicalized path that left the mount.
        canonical: PathBuf,
    },
    /// `require_mount_source` was set but the mount table could not be read or
    /// the mount point was not listed (a parse/read failure is a failed check).
    #[error("require_mount_source `{expected}` could not be verified for `{mount}`: {detail}")]
    SourceUnreadable {
        /// The configured mount path.
        mount: PathBuf,
        /// The configured expected source.
        expected: String,
        /// Why the source could not be read.
        detail: String,
    },
    /// The mount's source does not match `require_mount_source`.
    #[error("require_mount `{mount}` source is `{actual}`, expected `{expected}`")]
    SourceMismatch {
        /// The configured mount path.
        mount: PathBuf,
        /// The configured expected source.
        expected: String,
        /// The source reported by the mount table.
        actual: String,
    },
    /// A filesystem inspection failed.
    #[error("mount check i/o error at `{path}`: {source}")]
    Io {
        /// The path being inspected.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
}

/// A cheap device/directory probe, injectable so tests can fake a mount.
pub trait MountProbe: Send + Sync + 'static {
    /// `(st_dev, is_directory)` for `path` (following symlinks). One `stat`, so
    /// the mount-point check costs three calls.
    fn stat(&self, path: &Path) -> io::Result<(u64, bool)>;
}

/// [`MountProbe`] backed by `std::fs::Metadata`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemMountProbe;

impl MountProbe for SystemMountProbe {
    #[cfg(unix)]
    fn stat(&self, path: &Path) -> io::Result<(u64, bool)> {
        use std::os::unix::fs::MetadataExt;
        let meta = fs::metadata(path)?;
        Ok((meta.dev(), meta.is_dir()))
    }

    #[cfg(not(unix))]
    fn stat(&self, path: &Path) -> io::Result<(u64, bool)> {
        let meta = fs::metadata(path)?;
        Ok((0, meta.is_dir()))
    }
}

/// Enforces an optional `require_mount` on every recorder write.
///
/// A guard with no `require_mount` is a no-op: [`MountGuard::check`] succeeds,
/// [`MountGuard::validate_startup`] creates nothing, and the writer behaves
/// exactly as before R-14.
pub struct MountGuard {
    require_mount: Option<PathBuf>,
    require_source: Option<String>,
    probe: Arc<dyn MountProbe>,
    tripped: AtomicBool,
}

impl fmt::Debug for MountGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MountGuard")
            .field("require_mount", &self.require_mount)
            .field("require_source", &self.require_source)
            .field("tripped", &self.is_tripped())
            .finish_non_exhaustive()
    }
}

impl MountGuard {
    /// Build a guard for `require_mount` (already resolved to an absolute path
    /// by the profile) using `probe`.
    pub fn new(require_mount: Option<PathBuf>, probe: Arc<dyn MountProbe>) -> Self {
        Self {
            require_mount,
            require_source: None,
            probe,
            tripped: AtomicBool::new(false),
        }
    }

    /// A guard that enforces nothing (the default when `require_mount` is unset).
    pub fn unguarded() -> Self {
        Self::new(None, Arc::new(SystemMountProbe))
    }

    /// Also require the mount's source (from the mount table) to match,
    /// case-insensitively and ignoring trailing separators (R-14 fix1 §7).
    ///
    /// Ignored unless `require_mount` is set.
    pub fn with_source(mut self, source: Option<String>) -> Self {
        self.require_source = source;
        self
    }

    /// The configured mount path, if any.
    pub fn require_mount(&self) -> Option<&Path> {
        self.require_mount.as_deref()
    }

    /// The configured mount source, if any.
    pub fn require_source(&self) -> Option<&str> {
        self.require_source.as_deref()
    }

    /// Whether this guard enforces a `require_mount`.
    pub fn is_guarded(&self) -> bool {
        self.require_mount.is_some()
    }

    /// Whether the guard has ever tripped.
    pub fn is_tripped(&self) -> bool {
        self.tripped.load(Ordering::SeqCst)
    }

    /// Record that the guard tripped.
    pub fn trip(&self) {
        self.tripped.store(true, Ordering::SeqCst);
    }

    /// Validate `require_mount` and the containment of `out_dir` at startup.
    ///
    /// This creates nothing, so it is safe to call before `out_dir` (or any
    /// parent of it) exists. `out_dir` must be lexically inside the mount and
    /// must not canonicalize outside it (symlink escape).
    pub fn validate_startup(&self, out_dir: &Path) -> Result<(), MountError> {
        let Some(mount) = &self.require_mount else {
            return Ok(());
        };
        self.mount_status(mount)?;
        self.check_containment(mount, out_dir)
    }

    /// Re-check that the required mount is still a mount. A no-op when unguarded.
    pub fn check(&self) -> Result<(), MountError> {
        match &self.require_mount {
            None => Ok(()),
            Some(mount) => self.mount_status(mount),
        }
    }

    /// [`MountGuard::check`] that records a trip on failure.
    pub fn check_or_trip(&self) -> Result<(), MountError> {
        match self.check() {
            Ok(()) => Ok(()),
            Err(err) => {
                self.trip();
                Err(err)
            }
        }
    }

    /// The mount-point check of R-14 §3.
    fn mount_status(&self, mount: &Path) -> Result<(), MountError> {
        let (device, is_dir) = self.probe.stat(mount).map_err(|source| {
            if source.kind() == io::ErrorKind::NotFound {
                MountError::Missing {
                    mount: mount.to_path_buf(),
                }
            } else {
                MountError::Io {
                    path: mount.to_path_buf(),
                    source,
                }
            }
        })?;
        if !is_dir {
            return Err(MountError::NotADirectory {
                mount: mount.to_path_buf(),
            });
        }
        let parent = mount.parent().ok_or_else(|| MountError::NotAMount {
            mount: mount.to_path_buf(),
        })?;
        let (parent_device, _) = self.probe.stat(parent).map_err(|source| MountError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
        if device == parent_device {
            return Err(MountError::NotAMount {
                mount: mount.to_path_buf(),
            });
        }
        let root = Path::new("/");
        let (root_device, _) = self.probe.stat(root).map_err(|source| MountError::Io {
            path: root.to_path_buf(),
            source,
        })?;
        if device == root_device {
            return Err(MountError::SameDeviceAsRoot {
                mount: mount.to_path_buf(),
            });
        }
        if let Some(expected) = &self.require_source {
            let actual = mount_source(mount).map_err(|detail| MountError::SourceUnreadable {
                mount: mount.to_path_buf(),
                expected: expected.clone(),
                detail,
            })?;
            if !source_matches(expected, &actual) {
                return Err(MountError::SourceMismatch {
                    mount: mount.to_path_buf(),
                    expected: expected.clone(),
                    actual,
                });
            }
        }
        Ok(())
    }

    /// Lexical and canonical containment of `out_dir` inside `mount`.
    fn check_containment(&self, mount: &Path, out_dir: &Path) -> Result<(), MountError> {
        if !out_dir.starts_with(mount) {
            return Err(MountError::OutDirOutside {
                out_dir: out_dir.to_path_buf(),
                mount: mount.to_path_buf(),
            });
        }
        let mount_canon = fs::canonicalize(mount).map_err(|source| MountError::Io {
            path: mount.to_path_buf(),
            source,
        })?;
        let out_canon = canonicalize_allow_missing(out_dir).map_err(|source| MountError::Io {
            path: out_dir.to_path_buf(),
            source,
        })?;
        if out_canon == mount_canon {
            return Err(MountError::OutDirIsMount {
                mount: mount.to_path_buf(),
            });
        }
        if !out_canon.starts_with(&mount_canon) {
            return Err(MountError::SymlinkEscape {
                out_dir: out_dir.to_path_buf(),
                mount: mount.to_path_buf(),
                canonical: out_canon,
            });
        }
        Ok(())
    }
}

/// Canonicalize `path`, tolerating the non-existent tail that a not-yet-created
/// `out_dir` has: the deepest existing ancestor is canonicalized and the
/// remaining components are appended. Symlinks in the existing prefix are
/// therefore resolved, and `..` in the tail is resolved lexically, so neither a
/// symlink nor `<mount>/x/../..` can smuggle `out_dir` out of the mount.
fn canonicalize_allow_missing(path: &Path) -> io::Result<PathBuf> {
    if path.exists() {
        return fs::canonicalize(path);
    }
    let mut ancestor = path;
    while let Some(parent) = ancestor.parent() {
        if parent.exists() {
            let canonical = fs::canonicalize(parent)?;
            let tail = path.strip_prefix(parent).unwrap_or(Path::new(""));
            return Ok(normalize_join(&canonical, tail));
        }
        ancestor = parent;
    }
    fs::canonicalize(path)
}

/// Append `tail` to `base`, resolving `.` and `..` so the result cannot climb
/// above `base`'s root. `base` is already canonical.
fn normalize_join(base: &Path, tail: &Path) -> PathBuf {
    let mut out = base.to_path_buf();
    for component in tail.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
        }
    }
    out
}

/// One parsed `/proc/self/mountinfo` line.
struct MountInfo {
    /// The mount point, octal escapes resolved.
    mount_point: String,
    /// The mount source, octal escapes resolved (e.g. `E:\` on WSL `drvfs`).
    source: String,
}

/// The mount source for `mount`, from the mount table, or a human-readable
/// reason it could not be determined (a failed check).
#[cfg(target_os = "linux")]
fn mount_source(mount: &Path) -> Result<String, String> {
    let text = fs::read_to_string(MOUNTINFO).map_err(|err| format!("{MOUNTINFO}: {err}"))?;
    let canonical = fs::canonicalize(mount).unwrap_or_else(|_| mount.to_path_buf());
    source_from_mountinfo(&text, &canonical)
}

/// The source of the mount at `mount` in a mountinfo body.
///
/// The **last** matching line wins: with stacked mounts on one mount point the
/// last entry is the one actually in use. A malformed line is a failed check.
fn source_from_mountinfo(text: &str, mount: &Path) -> Result<String, String> {
    let mut found: Option<String> = None;
    for line in text.lines() {
        let Some(entry) = parse_mountinfo_line(line) else {
            return Err(format!("malformed mountinfo line: {line}"));
        };
        if Path::new(&entry.mount_point) == mount {
            found = Some(entry.source);
        }
    }
    found.ok_or_else(|| format!("`{}` is not a mount point in {MOUNTINFO}", mount.display()))
}

/// Non-Linux hosts have no `/proc/self/mountinfo`, so a configured source
/// cannot be verified and the check fails closed.
#[cfg(not(target_os = "linux"))]
fn mount_source(_mount: &Path) -> Result<String, String> {
    Err(format!("{MOUNTINFO} is only available on Linux"))
}

/// Parse one mountinfo line into its mount point and source.
///
/// Format: `id parent major:minor root mount_point opts [optional…] - fstype
/// source superopts`. Malformed lines yield `None`, which the caller treats as
/// a failed check.
fn parse_mountinfo_line(line: &str) -> Option<MountInfo> {
    let fields: Vec<&str> = line.split_ascii_whitespace().collect();
    let separator = fields.iter().position(|field| *field == "-")?;
    let mount_point = unescape_mountinfo(fields.get(4)?)?;
    let source = unescape_mountinfo(fields.get(separator + 2)?)?;
    Some(MountInfo {
        mount_point,
        source,
    })
}

/// Resolve the octal escapes (`\040` space, `\011` tab, `\012` newline,
/// `\134` backslash) mountinfo uses for special characters.
fn unescape_mountinfo(field: &str) -> Option<String> {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' {
            let octal = field.get(index + 1..index + 4)?;
            out.push(u8::from_str_radix(octal, 8).ok()?);
            index += 4;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Compare a configured source with the reported one, case-insensitively and
/// ignoring trailing `/` or `\` so `E:` and `E:\` are equivalent.
fn source_matches(expected: &str, actual: &str) -> bool {
    normalize_source(expected).eq_ignore_ascii_case(&normalize_source(actual))
}

fn normalize_source(source: &str) -> String {
    source.trim_end_matches(['/', '\\']).to_string()
}

#[cfg(test)]
pub(crate) mod test_support {
    //! A fake [`MountProbe`] for tests in this crate.

    use super::*;

    /// A probe where `mount` reports a distinct device until flipped to
    /// "unmounted", after which it reports the host device (as an unmounted
    /// `drvfs` path would once it is just a directory on `/`).
    pub(crate) struct FakeMountProbe {
        mount: PathBuf,
        mount_dev: u64,
        host_dev: u64,
        mounted: AtomicBool,
        blocking: AtomicBool,
    }

    impl FakeMountProbe {
        /// Build a probe that reports `mount` as a real mount point.
        pub(crate) fn new(mount: &Path) -> Self {
            Self {
                mount: fs::canonicalize(mount).unwrap_or_else(|_| mount.to_path_buf()),
                mount_dev: 7,
                host_dev: 3,
                mounted: AtomicBool::new(true),
                blocking: AtomicBool::new(false),
            }
        }

        /// Flip the simulated mount state.
        pub(crate) fn set_mounted(&self, mounted: bool) {
            self.mounted.store(mounted, Ordering::SeqCst);
        }

        /// Make `stat` block until cleared, to simulate a hung filesystem.
        pub(crate) fn set_blocking(&self, blocking: bool) {
            self.blocking.store(blocking, Ordering::SeqCst);
        }
    }

    impl MountProbe for FakeMountProbe {
        fn stat(&self, path: &Path) -> io::Result<(u64, bool)> {
            while self.blocking.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
            let meta = fs::metadata(path)?;
            if !self.mounted.load(Ordering::SeqCst) {
                return Ok((self.host_dev, meta.is_dir()));
            }
            let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
            let device = if canonical.starts_with(&self.mount) {
                self.mount_dev
            } else {
                self.host_dev
            };
            Ok((device, meta.is_dir()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::FakeMountProbe;
    use super::*;

    fn temp_dir(tag: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("mev-mount-{tag}-"))
            .tempdir()
            .unwrap()
    }

    /// Every path under `root`, sorted (files and directories).
    fn tree(root: &Path) -> Vec<PathBuf> {
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

    #[test]
    fn plain_directory_is_not_a_mount() {
        let tmp = temp_dir("plain");
        let mount = tmp.path().to_path_buf();
        let out_dir = mount.join("mev-rec");
        let before = tree(tmp.path());

        let guard = MountGuard::new(Some(mount.clone()), Arc::new(SystemMountProbe));
        let err = guard.validate_startup(&out_dir).unwrap_err();
        assert!(
            matches!(err, MountError::NotAMount { .. }),
            "unexpected error: {err}"
        );
        assert!(!out_dir.exists(), "out_dir must not be created");
        assert_eq!(tree(tmp.path()), before, "startup created something");
    }

    #[test]
    fn missing_mount_is_rejected() {
        let tmp = temp_dir("missing");
        let mount = tmp.path().join("gone");
        let guard = MountGuard::new(Some(mount), Arc::new(SystemMountProbe));
        let err = guard.validate_startup(&tmp.path().join("rec")).unwrap_err();
        assert!(matches!(err, MountError::Missing { .. }), "{err}");
    }

    #[test]
    fn a_file_is_not_a_directory() {
        let tmp = temp_dir("file");
        let mount = tmp.path().join("file");
        fs::write(&mount, b"x").unwrap();
        let guard = MountGuard::new(Some(mount), Arc::new(SystemMountProbe));
        let err = guard.validate_startup(&tmp.path().join("rec")).unwrap_err();
        assert!(matches!(err, MountError::NotADirectory { .. }), "{err}");
    }

    #[test]
    fn refuses_out_dir_outside_the_mount() {
        let mount_tmp = temp_dir("outside-mount");
        let mount = mount_tmp.path();
        let outside = temp_dir("outside-out");
        let out_dir = outside.path().join("mev-rec");
        let probe = Arc::new(FakeMountProbe::new(mount));
        let guard = MountGuard::new(Some(mount.to_path_buf()), probe);

        let err = guard.validate_startup(&out_dir).unwrap_err();
        assert!(
            matches!(err, MountError::OutDirOutside { .. }),
            "unexpected error: {err}"
        );
        assert!(!out_dir.exists(), "out_dir must not be created");
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlink_escape() {
        let mount_tmp = temp_dir("symlink-mount");
        let mount = mount_tmp.path();
        let outside = temp_dir("symlink-outside");
        let escape = mount.join("escape");
        std::os::unix::fs::symlink(outside.path(), &escape).unwrap();
        let out_dir = escape.join("mev-rec");

        let probe = Arc::new(FakeMountProbe::new(mount));
        let guard = MountGuard::new(Some(mount.to_path_buf()), probe);
        let err = guard.validate_startup(&out_dir).unwrap_err();
        assert!(
            matches!(err, MountError::SymlinkEscape { .. }),
            "unexpected error: {err}"
        );
        assert!(!out_dir.exists(), "out_dir must not be created");
        assert!(
            !outside.path().join("mev-rec").exists(),
            "escape path must not be created"
        );
    }

    #[test]
    fn out_dir_equal_to_the_mount_root_is_rejected() {
        let mount_tmp = temp_dir("equal-mount");
        let mount = mount_tmp.path();
        let probe = Arc::new(FakeMountProbe::new(mount));
        let guard = MountGuard::new(Some(mount.to_path_buf()), probe);

        let before = tree(mount);
        let err = guard.validate_startup(mount).unwrap_err();
        assert!(
            matches!(err, MountError::OutDirIsMount { .. }),
            "unexpected error: {err}"
        );
        assert_eq!(tree(mount), before, "validate_startup created something");
    }

    #[test]
    fn parses_a_mountinfo_line() {
        let line = "36 35 98:0 /mnt1 /mnt2 rw,noatime master:1 - ext3 /dev/root rw,errors=continue";
        let info = parse_mountinfo_line(line).unwrap();
        assert_eq!(info.mount_point, "/mnt2");
        assert_eq!(info.source, "/dev/root");
    }

    #[test]
    fn parses_mountinfo_octal_escapes() {
        // Mount point `/mnt/my drive` (space is `\040`); source `E:\` (backslash
        // is `\134`), as a WSL drvfs entry would render.
        let line = "36 35 8:1 / /mnt/my\\040drive rw - drvfs E:\\134 rw";
        let info = parse_mountinfo_line(line).unwrap();
        assert_eq!(info.mount_point, "/mnt/my drive");
        assert_eq!(info.source, "E:\\");
        assert!(parse_mountinfo_line("too short").is_none());
    }

    #[test]
    fn stacked_mounts_use_the_last_line() {
        // Two mounts on the same mount point; the last is the one in use.
        let text = "\
36 35 8:1 / /mnt/e rw - drvfs E:\\134 rw\n\
37 36 8:2 / /mnt/e rw - ext4 /dev/sdb1 rw\n";
        let source = source_from_mountinfo(text, Path::new("/mnt/e")).unwrap();
        assert_eq!(source, "/dev/sdb1", "the last stacked line must win");
        assert!(
            !source_matches("E:", &source),
            "a different device at the mount point must be rejected"
        );
        assert!(source_matches("E:", "E:\\"));
    }

    #[test]
    fn source_matching_ignores_case_and_trailing_separators() {
        assert!(source_matches("E:\\", "e:"));
        assert!(source_matches("e:", "E:\\"));
        assert!(source_matches("/dev/sdb1", "/dev/sdb1"));
        assert!(source_matches("E:", "E:\\"));
        assert!(!source_matches("E:", "F:"));
    }

    #[test]
    fn guarded_source_that_cannot_be_verified_is_rejected() {
        // A plain tempdir is not a mount point in /proc/self/mountinfo, so an
        // explicit source check fails closed.
        let tmp = temp_dir("source");
        let probe = Arc::new(FakeMountProbe::new(tmp.path()));
        let guard = MountGuard::new(Some(tmp.path().to_path_buf()), probe)
            .with_source(Some("E:\\".to_string()));
        assert_eq!(guard.require_source(), Some("E:\\"));

        let before = tree(tmp.path());
        let err = guard
            .validate_startup(&tmp.path().join("mev-rec"))
            .unwrap_err();
        assert!(
            matches!(
                err,
                MountError::SourceUnreadable { .. } | MountError::SourceMismatch { .. }
            ),
            "unexpected error: {err}"
        );
        assert_eq!(
            tree(tmp.path()),
            before,
            "validate_startup created something"
        );
    }

    #[test]
    fn refuses_dotdot_escape_from_the_mount() {
        let mount_tmp = temp_dir("dotdot-mount");
        let mount = mount_tmp.path();
        // Lexically starts with the mount, but climbs out with `..`.
        let out_dir = mount.join("mev-rec/../../elsewhere");
        let probe = Arc::new(FakeMountProbe::new(mount));
        let guard = MountGuard::new(Some(mount.to_path_buf()), probe);

        let err = guard.validate_startup(&out_dir).unwrap_err();
        assert!(
            matches!(err, MountError::SymlinkEscape { .. }),
            "unexpected error: {err}"
        );
        assert!(!out_dir.exists(), "escape path must not be created");
    }

    #[test]
    fn accepts_out_dir_under_the_mount_without_creating_it() {
        let mount_tmp = temp_dir("accept");
        let mount = mount_tmp.path();
        let out_dir = mount.join("mev-rec");
        let probe = Arc::new(FakeMountProbe::new(mount));
        let guard = MountGuard::new(Some(mount.to_path_buf()), probe);

        let before = tree(mount);
        guard.validate_startup(&out_dir).unwrap();
        assert!(!out_dir.exists(), "validate_startup must create nothing");
        assert_eq!(tree(mount), before);
        assert!(!guard.is_tripped());
    }

    #[test]
    fn unset_key_is_unguarded_and_accepts_anything() {
        let tmp = temp_dir("unguarded");
        let guard = MountGuard::unguarded();
        assert!(!guard.is_guarded());
        guard
            .validate_startup(&tmp.path().join("any/where"))
            .unwrap();
        guard.check().unwrap();
        assert!(guard.require_mount().is_none());
        assert!(!guard.is_tripped());
    }

    #[test]
    fn check_fails_after_the_mount_disappears() {
        let tmp = temp_dir("flip");
        let mount = tmp.path();
        let probe = Arc::new(FakeMountProbe::new(mount));
        let guard = MountGuard::new(Some(mount.to_path_buf()), probe.clone());
        guard.validate_startup(&mount.join("mev-rec")).unwrap();
        assert!(guard.check().is_ok());

        probe.set_mounted(false);
        let err = guard.check().unwrap_err();
        assert!(matches!(err, MountError::NotAMount { .. }), "{err}");
        assert!(!guard.is_tripped(), "check does not trip by itself");

        let err = guard.check_or_trip().unwrap_err();
        assert!(matches!(err, MountError::NotAMount { .. }), "{err}");
        assert!(guard.is_tripped());
    }
}
