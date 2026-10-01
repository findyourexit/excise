use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tempfile::{Builder as TempBuilder, NamedTempFile, TempDir, TempPath};

use crate::temporary_storage::{TemporaryStorage, TemporaryStorageReservation};

const SESSION_PREFIX: &str = ".excise-scan-";
const MANIFEST_FILE: &str = "manifest.bin";

/// Private session directory and quota for one canonical scan store.
#[derive(Clone, Debug)]
pub(crate) struct ScanStoreStorage {
    quota: TemporaryStorage,
    session: Arc<SessionDirectory>,
    next_file: Arc<AtomicU64>,
}

#[derive(Debug)]
struct SessionDirectory {
    temporary: Option<TempDir>,
    cleanup_recovered_root: bool,
    root: PathBuf,
    runs: PathBuf,
    merge: PathBuf,
    index: PathBuf,
    manifest_reservation: Mutex<TemporaryStorageReservation>,
}

impl Drop for SessionDirectory {
    fn drop(&mut self) {
        if self.temporary.is_none() && self.cleanup_recovered_root {
            let _ = fs::remove_dir_all(&self.root);
        }
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
        let temporary = match parent {
            Some(parent) => TempBuilder::new()
                .prefix(SESSION_PREFIX)
                .tempdir_in(parent)?,
            None => TempBuilder::new().prefix(SESSION_PREFIX).tempdir()?,
        };
        let root = temporary.path().to_path_buf();
        let runs = root.join("runs");
        let merge = root.join("merge");
        let index = root.join("index");
        fs::create_dir(&runs)?;
        fs::create_dir(&merge)?;
        fs::create_dir(&index)?;
        let quota = quota_for(&root)?;
        let manifest_reservation = quota.reservation(0)?;
        Ok(Self {
            quota,
            session: Arc::new(SessionDirectory {
                temporary: Some(temporary),
                cleanup_recovered_root: false,
                root,
                runs,
                merge,
                index,
                manifest_reservation: Mutex::new(manifest_reservation),
            }),
            next_file: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Reopens an interrupted private session after its manifest was verified.
    pub(crate) fn reopen(quota: TemporaryStorage, root: impl AsRef<Path>) -> io::Result<Self> {
        let root = root.as_ref().to_path_buf();
        let runs = root.join("runs");
        let merge = root.join("merge");
        let index = root.join("index");
        if !root.is_dir() || !runs.is_dir() || !merge.is_dir() || !index.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "private scan session layout is incomplete",
            ));
        }
        let manifest_bytes = fs::metadata(root.join(MANIFEST_FILE))?.len();
        let manifest_reservation = quota.reservation(manifest_bytes)?;
        Ok(Self {
            quota,
            session: Arc::new(SessionDirectory {
                temporary: None,
                cleanup_recovered_root: true,
                root,
                runs,
                merge,
                index,
                manifest_reservation: Mutex::new(manifest_reservation),
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

    pub(crate) fn persist_manifest(&self, encoded: &[u8]) -> io::Result<()> {
        let bytes = u64::try_from(encoded.len()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "scan manifest is too large")
        })?;
        let mut reservation = self
            .session
            .manifest_reservation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reservation.grow_to(bytes)?;
        let mut temporary = NamedTempFile::new_in(self.root())?;
        temporary.write_all(encoded)?;
        temporary.flush()?;
        temporary.as_file().sync_data()?;
        #[cfg(any(test, feature = "internal"))]
        self.quota.record_durable_sync();
        persist_replacing_transient_lock(
            temporary.into_temp_path(),
            &self.root().join(MANIFEST_FILE),
        )?;
        #[cfg(feature = "internal")]
        self.quota.record_manifest_persist();
        Ok(())
    }

    pub(crate) fn read_manifest(&self) -> io::Result<Vec<u8>> {
        fs::read(self.root().join(MANIFEST_FILE))
    }

    #[cfg(test)]
    pub(crate) fn preserve_for_recovery(self) -> PathBuf {
        let mut session = Arc::try_unwrap(self.session)
            .expect("test recovery storage must have one remaining owner");
        let root = session.root.clone();
        if let Some(temporary) = session.temporary.as_mut() {
            temporary.disable_cleanup(true);
        }
        drop(session);
        root
    }

    #[cfg(test)]
    pub(crate) fn manifest_path(&self) -> PathBuf {
        self.root().join(MANIFEST_FILE)
    }
}

/// The most times [`persist_replacing_transient_lock`] attempts the rename. Attempts are
/// immediate (no sleep), so the bound only limits how long a lock that never clears is
/// retried. Ten immediate attempts were enough on Windows CI runners.
const TRANSIENT_LOCK_RETRY_ATTEMPTS: u32 = 10;

/// Renames `temporary` onto `target`, retrying immediately when the destination is
/// transiently locked.
///
/// `target` is a single, stable path that every manifest persist overwrites in place.
/// On Windows, a file a moment after it is written is briefly open to another process,
/// such as the filter driver behind antivirus or search indexing, which answers the next
/// rename over that same path with `ERROR_ACCESS_DENIED` (`PermissionDenied`) rather than
/// blocking for it to clear. POSIX rename has no such transient failure; there, a
/// persistent `PermissionDenied` costs nine more immediate attempts before it surfaces.
///
/// # Errors
///
/// Returns the underlying I/O error unchanged, including after the retry bound is spent on
/// a persistent `PermissionDenied`, and for any other error on the first attempt that sees
/// it.
fn persist_replacing_transient_lock(temporary: TempPath, target: &Path) -> io::Result<()> {
    let mut temporary = Some(temporary);
    retry_transient_permission_denied(TRANSIENT_LOCK_RETRY_ATTEMPTS, || {
        let this_attempt = temporary
            .take()
            .expect("called again after a prior attempt returned success or gave up");
        this_attempt.persist(target).map_err(|error| {
            temporary = Some(error.path);
            error.error
        })
    })
}

/// Calls `attempt` up to `attempts` times, retrying immediately (no sleep) only while it
/// fails with [`io::ErrorKind::PermissionDenied`]. Any other error, or exhausting the
/// attempts, returns that call's error unchanged.
fn retry_transient_permission_denied<T>(
    attempts: u32,
    mut attempt: impl FnMut() -> io::Result<T>,
) -> io::Result<T> {
    assert!(
        attempts > 0,
        "retry_transient_permission_denied needs at least one attempt"
    );
    for try_number in 1..=attempts {
        match attempt() {
            Ok(value) => return Ok(value),
            Err(error)
                if try_number < attempts && error.kind() == io::ErrorKind::PermissionDenied => {}
            Err(error) => return Err(error),
        }
    }
    unreachable!("the loop above returns on its final attempt")
}

#[cfg(test)]
mod tests {
    use super::{io, retry_transient_permission_denied};

    fn permission_denied() -> io::Error {
        io::Error::new(io::ErrorKind::PermissionDenied, "access is denied")
    }

    #[test]
    fn retry_transient_permission_denied_succeeds_on_the_first_try_without_retrying() {
        let mut calls = 0;
        let result = retry_transient_permission_denied(10, || {
            calls += 1;
            Ok::<_, io::Error>(())
        });
        assert!(result.is_ok());
        assert_eq!(calls, 1);
    }

    #[test]
    fn retry_transient_permission_denied_retries_past_a_transient_lock_and_then_succeeds() {
        let mut calls = 0;
        let result = retry_transient_permission_denied(10, || {
            calls += 1;
            if calls < 3 {
                Err(permission_denied())
            } else {
                Ok(())
            }
        });
        assert!(result.is_ok());
        assert_eq!(
            calls, 3,
            "should stop retrying as soon as an attempt succeeds"
        );
    }

    #[test]
    fn retry_transient_permission_denied_gives_up_after_its_bound_on_a_persistent_lock() {
        let mut calls = 0;
        let result = retry_transient_permission_denied(3, || {
            calls += 1;
            Err::<(), _>(permission_denied())
        });
        assert_eq!(
            result
                .expect_err("a permission-denied error that never clears must surface")
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(calls, 3, "must not retry past its bound");
    }

    #[test]
    fn retry_transient_permission_denied_does_not_retry_a_different_error_kind() {
        let mut calls = 0;
        let result = retry_transient_permission_denied(10, || {
            calls += 1;
            Err::<(), _>(io::Error::new(io::ErrorKind::NotFound, "no such file"))
        });
        assert_eq!(
            result
                .expect_err("a non-transient error must surface")
                .kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(
            calls, 1,
            "only a transient PermissionDenied is worth retrying"
        );
    }
}
