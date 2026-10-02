//! The lock and owner marker that let a later start recognize a dead session's directory.
//!
//! A session directory (`.excise-scan-<random>` in the scratch parent) holds one file of its own,
//! `session.lock`, which is both halves of the session's identity:
//!
//! - **The lock.** The session takes an exclusive advisory lock on the file (`flock` on Unix,
//!   `LockFileEx` on Windows, both through [`File::try_lock`]) and keeps it until it tears its
//!   directory down. The operating system drops the lock when the process ends however it ends
//!   (a normal exit, a crash, `SIGKILL`, power loss), so a lock that can be taken means no process
//!   holds the session.
//! - **The owner marker.** The file's contents are [`OWNER_MARKER`]. A directory that carries them
//!   was made by an excise session. A directory that does not is not excise's to remove.
//!
//! The order of the first steps is what makes sweeping safe (`super::sweep`): the session creates
//! the file empty, takes the lock, and only then writes the marker. A sweeper trusts a directory
//! only once it has read the complete marker, and takes the lock only after that. So a complete
//! marker always implies that its session already held the lock, and a session that is still
//! being set up shows an empty file, which a sweeper leaves alone without touching its lock. No
//! moment exists at which a live session looks both marked and unlocked.

use std::fs::{File, OpenOptions};
use std::io::{self, Write as _};
use std::path::Path;

/// The file inside a session directory that holds its lock and its owner marker.
pub(super) const LOCK_FILE_NAME: &str = "session.lock";

/// The complete contents of a session's lock file. The first line names the format, so a later
/// format is a different line that this version declines to treat as its own.
pub(super) const OWNER_MARKER: &[u8] = b"excise scan-store session 1\n";

/// An exclusive lock on a session's lock file, held for the life of the session.
///
/// Dropping it closes the file, which releases the lock. [`SessionLock::release`] does the same
/// explicitly and first, so a caller about to remove the directory can state that order.
#[derive(Debug)]
pub(super) struct SessionLock {
    file: File,
}

impl SessionLock {
    /// Locks and marks the new session directory `root`.
    ///
    /// # Errors
    /// Returns an error when the directory cannot be made private, the lock file cannot be
    /// created exclusively (it already exists), the lock cannot be taken, or the marker cannot
    /// be written. The caller keeps the session either way, unmarked, which makes it a directory
    /// no sweep will ever remove.
    pub(super) fn establish(root: &Path) -> io::Result<Self> {
        Self::establish_observed(root, || {})
    }

    /// [`SessionLock::establish`], calling `observe` after every step that changes what a
    /// concurrent sweep could see, so a test can sweep at each of those moments.
    pub(super) fn establish_observed(root: &Path, mut observe: impl FnMut()) -> io::Result<Self> {
        // A sweeper on Windows accepts only a directory whose access control list names the
        // current user alone, so the session restricts its directory and file the same way.
        #[cfg(windows)]
        {
            crate::os::windows::restrict_private_path(root, true)?;
            observe();
        }
        let path = root.join(LOCK_FILE_NAME);
        let mut file = create_private_file(&path)?;
        observe();
        #[cfg(windows)]
        {
            crate::os::windows::restrict_private_path(&path, false)?;
            observe();
        }
        file.try_lock().map_err(io::Error::from)?;
        observe();
        file.write_all(OWNER_MARKER)?;
        observe();
        Ok(Self { file })
    }

    /// Releases the lock now. The lock file stays in the directory.
    pub(super) fn release(self) {
        // Unlocking before the handle closes is what Windows documents for a prompt release.
        // Either way, closing the handle on drop releases the lock too.
        let _ = self.file.unlock();
    }
}

/// Whether `contents` is exactly the owner marker.
pub(super) fn is_owner_marker(contents: &[u8]) -> bool {
    contents == OWNER_MARKER
}

/// Creates `path`, which must not exist, readable and writable by the current user only.
fn create_private_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;

        options.mode(0o600);
    }
    options.open(path)
}
