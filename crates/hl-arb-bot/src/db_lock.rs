//! Exclusive advisory lock on a SQLite database path (SPEC-0002 H-6).
//!
//! A running bot holds `<db>.lock` for its lifetime and `hl nonce reset` takes
//! the same lock before rewriting the nonce row, so an operator reset cannot
//! race a live process. `flock(2)` is released automatically when the process
//! exits, so a crashed bot never leaves a stale lock behind.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

/// An exclusive advisory lock held for as long as the value is alive.
pub struct DbLock {
    /// The open lock file; closing it (on drop) releases the `flock`.
    _file: File,
}

impl DbLock {
    /// The lock path for `db_path`: the database path plus `.lock`.
    pub fn path_for(db_path: &Path) -> PathBuf {
        let mut path = db_path.as_os_str().to_owned();
        path.push(".lock");
        PathBuf::from(path)
    }

    /// Take the exclusive lock, failing if another process already holds it.
    pub fn acquire(db_path: &Path) -> Result<Self> {
        let path = Self::path_for(db_path);
        // Mirror `Db::open`: create the database directory if it is missing, so
        // `observe` (which never opens the database) still starts on a fresh
        // checkout.
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening the nonce lockfile {}", path.display()))?;
        lock_exclusive(&file).with_context(|| {
            format!(
                "the nonce database {} is in use by a running bot; stop it first",
                db_path.display()
            )
        })?;
        Ok(Self { _file: file })
    }
}

/// Take a non-blocking exclusive `flock` on `file`.
#[cfg(unix)]
fn lock_exclusive(file: &File) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    // SAFETY: `file` owns a valid file descriptor for the duration of the call.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// No advisory locking outside unix; the bot only ships on unix.
#[cfg(not(unix))]
fn lock_exclusive(_file: &File) -> std::io::Result<()> {
    Ok(())
}
