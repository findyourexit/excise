use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tempfile::{Builder as TempBuilder, TempDir};

use super::session_lock::SessionLock;
use crate::temporary_storage::TemporaryStorage;

/// The name every session directory starts with. The startup sweep (`super::sweep`) looks for it.
pub(super) const SESSION_PREFIX: &str = ".excise-scan-";

/// Private session directory and quota for one canonical scan store.
#[derive(Clone, Debug)]
pub(crate) struct ScanStoreStorage {
    quota: TemporaryStorage,
    session: Arc<SessionDirectory>,
    next_file: Arc<AtomicU64>,
}

/// One session's directory. Dropping it ends the session in a fixed order
/// ([`SessionDirectory::tear_down`]).
#[derive(Debug)]
struct SessionDirectory {
    /// Held for the session's whole life. `None` when the directory could not be locked and
    /// marked: it still works as a session, and no sweep will ever remove it.
    lock: Option<SessionLock>,
    temporary: TempDir,
    root: PathBuf,
    runs: PathBuf,
    merge: PathBuf,
    index: PathBuf,
}

/// What [`SessionDirectory::tear_down`] has done at a point a caller can observe.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TearDownStep {
    /// The lock is released and nothing is removed yet.
    LockReleased,
    /// The working directories are removed. The lock file and its marker remain, so a process
    /// that ended now would leave a verified session for the next start's sweep to finish.
    WorkingFilesRemoved,
}

impl SessionDirectory {
    /// Ends the session: releases its lock, then removes its working directories, and leaves the
    /// lock file and the directory itself to `temporary`, which drops right after.
    ///
    /// The order is the contract. Windows cannot delete a file while it is locked, so the lock
    /// goes first. The lock file, which carries the owner marker, goes last, so a session killed
    /// partway through its own removal is still a verified directory that the next start's sweep
    /// finishes, instead of an unmarked one nothing would ever remove. On Windows the lock file
    /// is deleted only while this session holds its directory open
    /// ([`remove_marker_and_directory`]). `observe` runs at each step, for tests.
    fn tear_down(&mut self, mut observe: impl FnMut(TearDownStep, &Path)) {
        #[cfg(windows)]
        let marked = self.lock.is_some();
        if let Some(lock) = self.lock.take() {
            lock.release();
        }
        observe(TearDownStep::LockReleased, &self.root);
        for directory in [&self.runs, &self.merge, &self.index] {
            let _ = fs::remove_dir_all(directory);
        }
        observe(TearDownStep::WorkingFilesRemoved, &self.root);
        #[cfg(windows)]
        if marked && !remove_marker_and_directory(&self.root) {
            // The lock file stays where it is: the directory is then still a verified session
            // for the next start's sweep, which `temporary` would otherwise strip of its marker.
            self.temporary.disable_cleanup(true);
        }
    }
}

/// Windows: deletes the lock file and then the directory while holding the directory open
/// (`open_verified_private_directory_for_cleanup`).
///
/// A sweep that is looking at this directory holds it open the same way for a moment, which makes
/// a deletion of the directory fail. If the lock file, the only thing that marks the directory as
/// Excise's, were already gone by then, nothing would ever remove what was left. Holding the
/// directory first means the lock file is deleted only when the directory can be deleted right
/// after. Returns `false` when it gave up with the lock file still in place, so that a later
/// sweep can finish the job, and `true` when the directory is gone or is `temporary`'s to remove.
#[cfg(windows)]
fn remove_marker_and_directory(root: &Path) -> bool {
    use std::thread;
    use std::time::Duration;

    use super::session_lock::LOCK_FILE_NAME;
    use crate::os::windows::{
        delete_verified_private_file, open_verified_private_directory_for_cleanup,
    };

    const ERROR_SHARING_VIOLATION: i32 = 32;
    const ATTEMPTS: u32 = 20;
    const PAUSE: Duration = Duration::from_millis(5);

    // A handle that a sweep holds for a moment is the usual reason an open or a deletion fails
    // here, and it closes by itself.
    fn retrying<T>(mut attempt: impl FnMut() -> io::Result<T>) -> io::Result<T> {
        for _ in 1..ATTEMPTS {
            match attempt() {
                Err(error) if error.raw_os_error() == Some(ERROR_SHARING_VIOLATION) => {
                    thread::sleep(PAUSE);
                }
                result => return result,
            }
        }
        attempt()
    }

    let mut directory = match retrying(|| open_verified_private_directory_for_cleanup(root)) {
        Ok(directory) => directory,
        // Held by another handle for good: give up with everything in place.
        Err(error) if error.raw_os_error() == Some(ERROR_SHARING_VIOLATION) => return false,
        // Already gone, or not a directory this session restricted: nothing to protect.
        Err(_) => return true,
    };
    let lock_file = root.join(LOCK_FILE_NAME);
    let lock_file_is_gone = match retrying(|| delete_verified_private_file(&lock_file)) {
        Ok(()) => true,
        Err(error) => error.kind() == io::ErrorKind::NotFound,
    };
    if lock_file_is_gone {
        // Best effort: `temporary` tries again if this fails.
        let _ = directory.delete_on_close();
    }
    directory.close();
    lock_file_is_gone
}

impl Drop for SessionDirectory {
    fn drop(&mut self) {
        self.tear_down(|_, _| {});
    }
}

/// Creates an empty session directory below `parent`, or the system temporary directory.
///
/// On Unix only the current user can open it. `tempfile` makes a directory with `0o777` minus the
/// umask, which is `0o755` under the usual one and leaves the session's scan data readable by
/// every other user of a shared temporary directory; a sweep also accepts only a private one.
pub(super) fn create_session_directory(parent: Option<&Path>) -> io::Result<TempDir> {
    let mut builder = TempBuilder::new();
    builder.prefix(SESSION_PREFIX);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        builder.permissions(fs::Permissions::from_mode(0o700));
    }
    match parent {
        Some(parent) => builder.tempdir_in(parent),
        None => builder.tempdir(),
    }
}

impl ScanStoreStorage {
    /// Creates a private session below `parent`, or the system temporary directory.
    pub(crate) fn new(quota: TemporaryStorage, parent: Option<&Path>) -> io::Result<Self> {
        Self::new_with_quota(parent, |_| Ok(quota))
    }

    /// Creates a private session with an explicit limit or an adaptive default
    /// derived from the scratch volume's safe free capacity.
    pub(crate) fn new_for_scan_store_mib(
        mib: Option<usize>,
        reserve_mib: Option<usize>,
        parent: Option<&Path>,
    ) -> io::Result<Self> {
        Self::new_with_quota(parent, |root| {
            TemporaryStorage::scan_store_from_mib_for_scratch(mib, reserve_mib, root)
        })
    }

    fn new_with_quota(
        parent: Option<&Path>,
        quota_for: impl FnOnce(&Path) -> io::Result<TemporaryStorage>,
    ) -> io::Result<Self> {
        if let Some(parent) = parent {
            fs::create_dir_all(parent)?;
        }
        let temporary = create_session_directory(parent)?;
        let root = temporary.path().to_path_buf();
        // Lock and mark the directory before anything else is created in it: a session killed
        // earlier than this is indistinguishable from a foreign directory, and no sweep removes
        // it. A directory that cannot be locked (a file system without locks, a full disk) still
        // works as a session; it is only never swept, which is how every session behaved before.
        let lock = SessionLock::establish(&root).ok();
        let runs = root.join("runs");
        let merge = root.join("merge");
        let index = root.join("index");
        fs::create_dir(&runs)?;
        fs::create_dir(&merge)?;
        fs::create_dir(&index)?;
        let quota = quota_for(&root)?;
        Ok(Self {
            quota,
            session: Arc::new(SessionDirectory {
                lock,
                temporary,
                root,
                runs,
                merge,
                index,
            }),
            next_file: Arc::new(AtomicU64::new(0)),
        })
    }

    #[must_use]
    pub(crate) fn quota(&self) -> TemporaryStorage {
        self.quota.clone()
    }

    #[must_use]
    pub(crate) fn root(&self) -> &Path {
        let _ = &self.session.temporary;
        &self.session.root
    }

    #[must_use]
    pub(crate) fn internal_paths(&self) -> Vec<PathBuf> {
        vec![self.root().to_path_buf()]
    }

    pub(crate) fn create_run_file(&self, run_id: u64, merged: bool) -> io::Result<(File, PathBuf)> {
        let directory = if merged {
            &self.session.merge
        } else {
            &self.session.runs
        };
        let path = directory.join(format!("{run_id:016x}.run"));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)?;
        Ok((file, path))
    }

    pub(crate) fn run_path(&self, run_id: u64, merged: bool) -> PathBuf {
        let directory = if merged {
            &self.session.merge
        } else {
            &self.session.runs
        };
        directory.join(format!("{run_id:016x}.run"))
    }

    pub(crate) fn open_run_file(&self, run_id: u64, merged: bool) -> io::Result<(File, PathBuf)> {
        let path = self.run_path(run_id, merged);
        let file = OpenOptions::new().read(true).write(true).open(&path)?;
        Ok((file, path))
    }

    pub(crate) fn create_index_file(&self) -> io::Result<(File, PathBuf)> {
        let id = self.next_file.fetch_add(1, Ordering::AcqRel);
        let path = self.session.index.join(format!("{id:016x}.redb"));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)?;
        Ok((file, path))
    }
}

#[cfg(test)]
mod tests {
    use std::fs::{self, OpenOptions, TryLockError};
    use std::sync::Arc;

    use super::{ScanStoreStorage, TearDownStep};
    use crate::scan_store::session_lock::{LOCK_FILE_NAME, OWNER_MARKER};
    use crate::temporary_storage::TemporaryStorage;

    fn session_in(parent: &tempfile::TempDir) -> ScanStoreStorage {
        ScanStoreStorage::new(TemporaryStorage::default(), Some(parent.path()))
            .expect("a session can be created")
    }

    #[test]
    fn a_session_holds_its_lock_and_marker_until_it_ends() {
        let parent = tempfile::tempdir().expect("a scratch parent can be created");
        let session = session_in(&parent);
        let root = session.root().to_path_buf();
        let lock_file = root.join(LOCK_FILE_NAME);

        // The marker is complete. It is measured, not read: Windows refuses to read a file
        // that another handle has locked.
        assert_eq!(
            fs::metadata(&lock_file).map(|meta| meta.len()).ok(),
            u64::try_from(OWNER_MARKER.len()).ok()
        );
        let other = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_file)
            .expect("the lock file can be opened");
        assert!(
            matches!(other.try_lock(), Err(TryLockError::WouldBlock)),
            "another handle must not be able to lock a live session"
        );
        drop(other);

        // Clones share one session; only the last one ends it.
        let clone = session.clone();
        drop(session);
        assert!(lock_file.is_file());
        drop(clone);
        assert!(!root.exists(), "ending the session removes its directory");
    }

    #[test]
    fn a_session_releases_its_lock_before_removing_anything_and_removes_the_marker_last() {
        let parent = tempfile::tempdir().expect("a scratch parent can be created");
        let ScanStoreStorage { session, .. } = session_in(&parent);
        let mut directory =
            Arc::try_unwrap(session).expect("this is the only handle on the session");
        let root = directory.root.clone();
        fs::write(directory.runs.join("0001.run"), b"run").expect("a run can be written");
        let mut steps = Vec::new();

        directory.tear_down(|step, root| {
            steps.push(step);
            match step {
                TearDownStep::LockReleased => {
                    // Windows cannot delete a locked file, so nothing may be removed while the
                    // lock is held, and by now it must be free.
                    assert!(root.join("runs").join("0001.run").is_file());
                    let probe = OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open(root.join(LOCK_FILE_NAME))
                        .expect("the lock file is still there");
                    probe.try_lock().expect("the lock is already released");
                }
                TearDownStep::WorkingFilesRemoved => {
                    for gone in ["runs", "merge", "index"] {
                        assert!(!root.join(gone).exists(), "{gone} is removed");
                    }
                    // A process killed here leaves a directory the next sweep can finish.
                    assert_eq!(
                        fs::read(root.join(LOCK_FILE_NAME)).expect("the marker outlasts the files"),
                        OWNER_MARKER
                    );
                }
            }
        });

        assert_eq!(
            steps,
            [
                TearDownStep::LockReleased,
                TearDownStep::WorkingFilesRemoved
            ]
        );
        drop(directory);
        assert!(!root.exists());
    }
}
