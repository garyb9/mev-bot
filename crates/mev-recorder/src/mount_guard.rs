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

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use thiserror::Error;

/// How often the writer re-checks a required mount while running (R-14 §4).
pub const MOUNT_RECHECK_INTERVAL: Duration = Duration::from_secs(2);

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
    probe: Arc<dyn MountProbe>,
    tripped: AtomicBool,
}

impl MountGuard {
    /// Build a guard for `require_mount` (already resolved to an absolute path
    /// by the profile) using `probe`.
    pub fn new(require_mount: Option<PathBuf>, probe: Arc<dyn MountProbe>) -> Self {
        Self {
            require_mount,
            probe,
            tripped: AtomicBool::new(false),
        }
    }

    /// A guard that enforces nothing (the default when `require_mount` is unset).
    pub fn unguarded() -> Self {
        Self::new(None, Arc::new(SystemMountProbe))
    }

    /// The configured mount path, if any.
    pub fn require_mount(&self) -> Option<&Path> {
        self.require_mount.as_deref()
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
    }

    impl FakeMountProbe {
        /// Build a probe that reports `mount` as a real mount point.
        pub(crate) fn new(mount: &Path) -> Self {
            Self {
                mount: fs::canonicalize(mount).unwrap_or_else(|_| mount.to_path_buf()),
                mount_dev: 7,
                host_dev: 3,
                mounted: AtomicBool::new(true),
            }
        }

        /// Flip the simulated mount state.
        pub(crate) fn set_mounted(&self, mounted: bool) {
            self.mounted.store(mounted, Ordering::SeqCst);
        }
    }

    impl MountProbe for FakeMountProbe {
        fn stat(&self, path: &Path) -> io::Result<(u64, bool)> {
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
