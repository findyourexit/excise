//! Unix half of the sweep.
//!
//! Every step after the scratch parent is opened is relative to a descriptor, so nothing here
//! resolves a path a second time or follows a link: each candidate is opened with `O_NOFOLLOW`
//! and checked through the descriptor it returned, its lock file is opened the same way inside
//! it, and removal unlinks names inside directories it holds open. The only operation by name is
//! the last one, removing the candidate from the parent, and it first checks that the name still
//! refers to the directory that was examined (`rmdir` removes only an empty directory anyway).

use std::ffi::{CStr, CString};
use std::fs::{File, TryLockError};
use std::io::{self, Read as _};
use std::os::fd::AsFd;
use std::path::Path;

use rustix::fs::{
    AtFlags, Dir, FileType, Mode, OFlags, Stat, fstat, open, openat, statat, unlinkat,
};
use rustix::io::Errno;
use rustix::path::Arg;

use crate::scan_store::session_lock::{LOCK_FILE_NAME, OWNER_MARKER, is_owner_marker};

/// The deepest directory below a session that removal descends into. A session has two levels (a
/// subdirectory and its files), so a deeper tree is not something excise made: the session is left
/// alone instead of being walked without a bound.
const MAX_DEPTH: usize = 8;
/// Entries read from one directory per pass. Reading again after each pass bounds the memory a
/// wide directory costs, and finds entries that a file system skipped while the directory
/// changed during iteration.
const BATCH: usize = 256;
/// Passes over one directory before removal gives up on it as refilling as fast as it empties.
const MAX_PASSES: usize = 4_096;
/// How many bytes of a lock file the sweep reads: one more than the marker, so a longer file
/// cannot pass for it.
const MARKER_READ_LIMIT: u64 = OWNER_MARKER.len() as u64 + 1;

/// Removes dead sessions directly inside `parent`. See the module this is called from.
pub(super) fn sweep(parent: &Path, prefix: &str, limit: usize) -> usize {
    // `O_DIRECTORY` refuses anything else at once, where opening a FIFO named as the parent
    // would wait for a writer.
    let Ok(parent) = open(
        parent,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map(File::from) else {
        return 0;
    };
    let Ok(parent_stat) = fstat(&parent) else {
        return 0;
    };
    let user = nix::unistd::geteuid().as_raw();
    let mut removed = 0_usize;
    for name in candidates(&parent, prefix, limit) {
        if matches!(remove_if_dead(&parent, &parent_stat, &name, user), Ok(true)) {
            removed = removed.saturating_add(1);
        }
    }
    removed
}

/// The names of up to `limit` entries of `parent` that start with `prefix` and are, or might be,
/// directories. Collected first so removing one never disturbs the iteration.
fn candidates(parent: &File, prefix: &str, limit: usize) -> Vec<CString> {
    let Ok(entries) = Dir::read_from(parent) else {
        return Vec::new();
    };
    entries
        .map_while(Result::ok)
        .filter(|entry| {
            matches!(entry.file_type(), FileType::Directory | FileType::Unknown)
                && entry.file_name().to_bytes().starts_with(prefix.as_bytes())
        })
        .map(|entry| entry.file_name().to_owned())
        .take(limit)
        .collect()
}

/// Removes the directory `name` of `parent` if it is a verified session whose lock is free.
/// Returns `Ok(false)` for a directory that is left alone, whatever the reason.
fn remove_if_dead(parent: &File, parent_stat: &Stat, name: &CStr, user: u32) -> io::Result<bool> {
    // The candidate: a directory (a symbolic link is refused by `O_NOFOLLOW`) that only the
    // current user can reach, on the same file system as the parent.
    let session = open_nofollow(parent, name, OFlags::RDONLY | OFlags::DIRECTORY)?;
    let session_stat = fstat(&session)?;
    if !is_private_to_user(&session_stat, FileType::Directory, user)
        || session_stat.st_dev != parent_stat.st_dev
    {
        return Ok(false);
    }

    // Its lock file. `O_NONBLOCK` keeps a planted FIFO from blocking the open; the type check
    // below then refuses it.
    let mut lock = match open_nofollow(&session, LOCK_FILE_NAME, OFlags::RDWR | OFlags::NONBLOCK) {
        Ok(lock) => lock,
        Err(error) if is_errno(&error, Errno::NOENT) => return Ok(false),
        Err(error) => return Err(error),
    };
    let lock_stat = fstat(&lock)?;
    if !is_private_to_user(&lock_stat, FileType::RegularFile, user)
        || usize::try_from(lock_stat.st_size).ok() != Some(OWNER_MARKER.len())
    {
        return Ok(false);
    }

    // The marker is read before the lock is taken. A session sets itself up by creating the file
    // empty, locking it, and only then writing the marker, so a complete marker means its
    // session already held the lock, and an empty file is left alone without being locked, which
    // could otherwise make that session's own lock attempt fail.
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
    remove_contents(&session, &session_stat, 0, &[LOCK_FILE_NAME])?;
    // Release before unlinking, the order Windows needs; a sweep that starts in between finds
    // nothing left to remove.
    let _ = lock.unlock();
    drop(lock);
    unlink(&session, LOCK_FILE_NAME, AtFlags::empty())?;
    drop(session);

    // Remove only the directory that was examined, not whatever the name refers to by now.
    let current = match statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(current) => current,
        Err(Errno::NOENT) => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if current.st_dev != session_stat.st_dev || current.st_ino != session_stat.st_ino {
        return Ok(false);
    }
    match unlinkat(parent, name, AtFlags::REMOVEDIR) {
        Ok(()) => Ok(true),
        // Another sweep finished it first.
        Err(Errno::NOENT) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// Whether `stat` describes an object of `kind` owned by `user` that no group or other
/// permission bit reaches.
pub(super) fn is_private_to_user(stat: &Stat, kind: FileType, user: u32) -> bool {
    FileType::from_raw_mode(stat.st_mode) == kind
        && stat.st_uid == user
        && !Mode::from_raw_mode(stat.st_mode).intersects(Mode::RWXG | Mode::RWXO)
}

/// Removes every entry of `directory` except the names in `keep`, descending into subdirectories
/// of the same file system. Symbolic links are removed, never followed.
fn remove_contents(directory: &File, root: &Stat, depth: usize, keep: &[&str]) -> io::Result<()> {
    if depth > MAX_DEPTH {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "a session directory is never this deep",
        ));
    }
    for _ in 0..MAX_PASSES {
        let batch = read_batch(directory, keep)?;
        if batch.is_empty() {
            return Ok(());
        }
        let mut progress = false;
        let mut failure = None;
        for (name, file_type) in &batch {
            match remove_entry(directory, name, *file_type, root, depth) {
                Ok(()) => progress = true,
                Err(error) => failure = Some(error),
            }
        }
        if !progress {
            return Err(failure.unwrap_or_else(|| io::Error::other("nothing could be removed")));
        }
    }
    Err(io::Error::other(
        "a directory refilled as fast as it emptied",
    ))
}

/// Up to [`BATCH`] entries of `directory`, read from its start, other than `keep`.
fn read_batch(directory: &File, keep: &[&str]) -> io::Result<Vec<(CString, FileType)>> {
    let mut batch = Vec::new();
    for entry in Dir::read_from(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let bytes = name.to_bytes();
        if bytes == b"." || bytes == b".." || keep.iter().any(|kept| kept.as_bytes() == bytes) {
            continue;
        }
        batch.push((name.to_owned(), entry.file_type()));
        if batch.len() == BATCH {
            break;
        }
    }
    Ok(batch)
}

/// Removes one entry of `directory`, and everything below it when it is a directory.
fn remove_entry(
    directory: &File,
    name: &CStr,
    listed: FileType,
    root: &Stat,
    depth: usize,
) -> io::Result<()> {
    let file_type = if listed == FileType::Unknown {
        match statat(directory, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => FileType::from_raw_mode(stat.st_mode),
            Err(Errno::NOENT) => return Ok(()),
            Err(error) => return Err(error.into()),
        }
    } else {
        listed
    };
    if file_type != FileType::Directory {
        // A file, a link (the link itself, never its target), a socket, a device.
        return unlink(directory, name, AtFlags::empty());
    }
    let child = match open_nofollow(directory, name, OFlags::RDONLY | OFlags::DIRECTORY) {
        Ok(child) => child,
        Err(error) if is_errno(&error, Errno::NOENT) => return Ok(()),
        // Replaced after it was listed by something that is not a directory, or by a link that
        // `O_NOFOLLOW` refused to open: remove what is there now, never what it points to.
        Err(error) if is_errno(&error, Errno::NOTDIR) || is_errno(&error, Errno::LOOP) => {
            return unlink(directory, name, AtFlags::empty());
        }
        Err(error) => return Err(error),
    };
    if fstat(&child)?.st_dev != root.st_dev {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "a session directory holds a mount point",
        ));
    }
    remove_contents(&child, root, depth.saturating_add(1), &[])?;
    drop(child);
    unlink(directory, name, AtFlags::REMOVEDIR)
}

/// Opens `name` inside `directory` without following a symbolic link.
fn open_nofollow(directory: impl AsFd, name: impl Arg, flags: OFlags) -> io::Result<File> {
    let descriptor = openat(
        directory,
        name,
        flags | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    Ok(File::from(descriptor))
}

/// Removes `name` inside `directory`; an entry that is already gone counts as removed.
fn unlink(directory: impl AsFd, name: impl Arg, flags: AtFlags) -> io::Result<()> {
    match unlinkat(directory, name, flags) {
        Ok(()) | Err(Errno::NOENT) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn is_errno(error: &io::Error, errno: Errno) -> bool {
    error.raw_os_error() == Some(errno.raw_os_error())
}

#[cfg(test)]
mod tests {
    //! The listing already leaves links alone, so the tests above this one cannot reach the
    //! checks that matter when an entry is replaced between being listed and being opened. These
    //! call the removal steps directly with the type the listing would have recorded.

    use std::fs::{self, File};
    use std::os::unix::fs::symlink;

    use rustix::fs::{FileType, fstat};

    use super::{remove_entry, remove_if_dead};
    use crate::scan_store::session_lock::SessionLock;
    use crate::scan_store::storage::create_session_directory;

    fn user() -> u32 {
        nix::unistd::geteuid().as_raw()
    }

    #[test]
    fn a_directory_replaced_by_a_link_after_it_was_listed_is_unlinked_not_followed() {
        let scratch = tempfile::tempdir().expect("a scratch directory can be created");
        let outside = scratch.path().join("outside");
        fs::create_dir(&outside).expect("a directory can be created");
        fs::write(outside.join("keep.txt"), b"mine").expect("a file can be written");
        let inside = scratch.path().join("inside");
        fs::create_dir(&inside).expect("a directory can be created");
        symlink(&outside, inside.join("entry")).expect("a link can be created");
        let directory = File::open(&inside).expect("the directory can be opened");
        let stat = fstat(&directory).expect("the directory can be examined");

        remove_entry(&directory, c"entry", FileType::Directory, &stat, 0)
            .expect("a link is removed, not followed");

        assert!(
            fs::symlink_metadata(inside.join("entry")).is_err(),
            "the link is gone"
        );
        assert_eq!(
            fs::read(outside.join("keep.txt")).expect("the target survives"),
            b"mine"
        );
    }

    #[test]
    fn a_candidate_replaced_by_a_link_after_it_was_listed_is_refused() {
        let scratch = tempfile::tempdir().expect("a scratch directory can be created");
        let target = create_session_directory(Some(scratch.path()))
            .expect("a session directory can be created")
            .keep();
        SessionLock::establish(&target)
            .expect("a new directory can be locked and marked")
            .release();
        fs::write(target.join("keep.txt"), b"mine").expect("a file can be written");
        let parent = scratch.path().join("parent");
        fs::create_dir(&parent).expect("a directory can be created");
        symlink(&target, parent.join(".excise-scan-link")).expect("a link can be created");
        let directory = File::open(&parent).expect("the parent can be opened");
        let stat = fstat(&directory).expect("the parent can be examined");

        let outcome = remove_if_dead(&directory, &stat, c".excise-scan-link", user());

        assert!(
            !matches!(outcome, Ok(true)),
            "a link must never be removed as a session"
        );
        assert_eq!(
            fs::read(target.join("keep.txt")).expect("the target survives"),
            b"mine"
        );
        assert!(target.is_dir());
    }
}
