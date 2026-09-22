use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tempfile::{Builder as TempBuilder, NamedTempFile, TempDir};

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
        temporary
            .persist(self.root().join(MANIFEST_FILE))
            .map_err(|error| error.error)?;
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
