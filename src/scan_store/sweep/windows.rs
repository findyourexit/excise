//! Windows half of the sweep.
//!
//! A candidate is opened through the held handle `open_verified_private_directory_for_cleanup`
//! returns. It checks that the object is a real directory (not a reparse point) owned by the
//! current user whose access control list is protected and names that user alone, and its share
//! mode keeps the directory from being renamed or removed until the handle closes. The sweep keeps
//! that handle through classification and every deletion, then deletes the directory through it.

use std::fs::{self, OpenOptions, TryLockError};
use std::io::{self, Read as _};
use std::os::windows::fs::FileTypeExt as _;
use std::path::{Path, PathBuf};

use crate::os::windows::{
    delete_verified_private_file, open_verified_private_directory_for_cleanup, verify_private_path,
};
use crate::scan_store::session_lock::{LOCK_FILE_NAME, OWNER_MARKER, is_owner_marker};

/// The length of a complete owner marker, in bytes.
const MARKER_LENGTH: u64 = OWNER_MARKER.len() as u64;
/// How many bytes of a lock file the sweep reads: one more than the marker, so a longer file
/// cannot pass for it.
const MARKER_READ_LIMIT: u64 = MARKER_LENGTH + 1;

/// Removes dead sessions directly inside `parent`. See the module this is called from.
pub(super) fn sweep(parent: &Path, prefix: &str, limit: usize) -> usize {
    let Ok(entries) = fs::read_dir(parent) else {
        return 0;
    };
    // Collected first so removing one never disturbs the iteration.
    let candidates: Vec<PathBuf> = entries
        .map_while(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(prefix))
                && entry.file_type().is_ok_and(|kind| kind.is_dir())
        })
        .map(|entry| entry.path())
        .take(limit)
        .collect();
    let mut removed = 0_usize;
    for candidate in candidates {
        if matches!(remove_if_dead(&candidate), Ok(true)) {
            removed = removed.saturating_add(1);
        }
    }
    removed
}

/// Removes the directory `candidate` if it is a verified session whose lock is free. Returns
/// `Ok(false)` for a directory that is left alone, whatever the reason.
fn remove_if_dead(candidate: &Path) -> io::Result<bool> {
    // A directory whose lock file does not yet hold a whole marker is not a session that has
    // finished setting itself up. It is skipped without being opened: a session that is still
    // restricting its own directory would find an open handle that shares no deletion in its
    // way. This only decides what to open; everything below verifies it.
    let lock_path = candidate.join(LOCK_FILE_NAME);
    if !fs::metadata(&lock_path).is_ok_and(|meta| meta.is_file() && meta.len() == MARKER_LENGTH) {
        return Ok(false);
    }

    // The candidate: a real directory that only the current user can reach, held from here on.
    let mut directory = open_verified_private_directory_for_cleanup(candidate)?;

    // Its lock file, held to the same standard.
    verify_private_path(&lock_path, false)?;
    let mut lock = OpenOptions::new().read(true).write(true).open(&lock_path)?;

    // The marker is read before the lock is taken. A session sets itself up by creating the file
    // empty, locking it, and only then writing the marker, so a complete marker means its session
    // already held the lock, and an empty file is left alone without being locked, which could
    // otherwise make that session's own lock attempt fail. A live session's lock covers the whole
    // file, so reading it fails here and the directory is left alone, as it should be.
    let mut contents = Vec::with_capacity(OWNER_MARKER.len());
    lock.by_ref()
        .take(MARKER_READ_LIMIT)
        .read_to_end(&mut contents)?;
    if !is_owner_marker(&contents) {
        return Ok(false);
    }
    match lock.try_lock() {
        Ok(()) => {}
        // A live session holds it.
        Err(TryLockError::WouldBlock) => return Ok(false),
        Err(TryLockError::Error(error)) => return Err(error),
    }

    // Everything but the lock file goes first, so a removal that fails or is interrupted leaves
    // the directory still verifiable and a later sweep finishes it.
    remove_contents(candidate, LOCK_FILE_NAME)?;
    // The handle closes before the file is deleted: Windows cannot delete a file that is locked.
    let _ = lock.unlock();
    drop(lock);
    // Gone already counts as removed: a session that is ending deletes the same files.
    match delete_verified_private_file(&lock_path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    directory.delete_on_close()?;
    directory.close();
    Ok(true)
}

/// Removes every entry of `directory` except the one named `keep`. Links are removed, never
/// followed: the standard library's `remove_dir_all` does not follow them either. An entry that
/// is already gone counts as removed, as it does on Unix: a session that is ending deletes the
/// same files.
fn remove_contents(directory: &Path, keep: &str) -> io::Result<()> {
    // Collected first so removing one never disturbs the iteration.
    let entries = fs::read_dir(directory)?.collect::<io::Result<Vec<_>>>()?;
    for entry in entries {
        if entry.file_name() == keep {
            continue;
        }
        let path = entry.path();
        let removed = entry.file_type().and_then(|kind| {
            if kind.is_dir() {
                fs::remove_dir_all(&path)
            } else if kind.is_symlink_dir() {
                fs::remove_dir(&path)
            } else {
                fs::remove_file(&path)
            }
        });
        match removed {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
    }
    Ok(())
}
