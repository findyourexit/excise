use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tempfile::{Builder as TempBuilder, TempDir};

use crate::temporary_storage::TemporaryStorage;

const SESSION_PREFIX: &str = ".excise-scan-";

/// Private session directory and quota for one canonical scan store.
#[derive(Clone, Debug)]
pub(crate) struct ScanStoreStorage {
    quota: TemporaryStorage,
    session: Arc<SessionDirectory>,
    next_file: Arc<AtomicU64>,
}

#[derive(Debug)]
struct SessionDirectory {
    temporary: TempDir,
    root: PathBuf,
    runs: PathBuf,
    merge: PathBuf,
    index: PathBuf,
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
        Ok(Self {
            quota,
            session: Arc::new(SessionDirectory {
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
