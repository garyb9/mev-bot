//! Mount-loss detection and disk-space measurement (SPEC-0008 R-14).

use std::ffi::CString;
use std::io;
use std::path::Path;

use crate::mount_guard::MountGuard;

use super::SegmentError;

/// Whether an I/O error means the segment's device or file vanished, which must
/// stop the stream rather than be retried (R-14 fix1 §5).
///
/// Errnos that a lost mount returns directly stop immediately. Any other write
/// error also stops when the mount probe itself fails: the probe is the
/// authority, so a fresh EACCES/ENOSPC on a vanished mount is caught too. The
/// probe runs only on an actual write error, never per envelope.
#[cfg(unix)]
pub(super) fn is_lost_mount(err: &SegmentError, guard: &MountGuard) -> bool {
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
pub(super) fn is_lost_mount(_err: &SegmentError, guard: &MountGuard) -> bool {
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
