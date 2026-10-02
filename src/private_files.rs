//! The files Excise itself keeps open inside the tree the reader is looking at, by exact path.
//!
//! On Windows the spill of a deletion's plan and report is a named file in the folder that holds
//! the target: an anonymous temporary file is not available there, and the folder that holds the
//! target is structurally outside it, so the deletion never has to remove its own spill. The file
//! lives as long as the report that owns it stays in the deletion history, and for all that time it
//! is a file in the user's tree that Excise created and holds open with no sharing. Whatever lists
//! or scans that folder must not treat it as the user's: the scan would record Excise's own file
//! (or report it as unreadable), and the map that follows a deletion is checked against a listing
//! of the folder that held the removed entry, which would then hold an entry the map does not.
//!
//! Each such file is therefore registered here, by its exact path and *before* it is created, and
//! unregistered when the spill that owns it is dropped. The scanner and the overlay's listing skip
//! a registered path without reading it. The match is by path and never by name pattern: a
//! user's own file may carry any name, so the shape of a name proves nothing about who made it.
//!
//! A registry belongs to a session: the storage the deletion spills are charged to and the storage
//! the scanner and the scan store use share one ([`crate::temporary_storage::TemporaryStorage`]).

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, PoisonError, RwLock};

/// A set of paths, shared by every clone. Cheap to ask while it is empty, which is nearly always:
/// one atomic load, no lock.
#[derive(Clone, Debug, Default)]
pub(crate) struct PrivateFiles {
    shared: Arc<Shared>,
}

#[derive(Debug, Default)]
struct Shared {
    /// How many registrations are live: zero lets [`PrivateFiles::contains`] answer without taking
    /// the lock, which the scanner asks once for every entry it reads.
    live: AtomicUsize,
    /// Each registered path with the number of registrations that name it, so that a path
    /// registered twice stays registered until both are dropped.
    paths: RwLock<HashMap<PathBuf, usize>>,
}

impl PrivateFiles {
    /// Registers `path`, a file this session is about to create or has just created in the user's
    /// tree. The path stays registered until the returned guard is dropped.
    ///
    /// Register before the file exists: a reader that can see the file then also sees that it is
    /// registered.
    #[cfg_attr(
        not(any(windows, test)),
        allow(
            dead_code,
            reason = "only Windows keeps a named spill file in the user's tree"
        )
    )]
    pub(crate) fn register(&self, path: PathBuf) -> PrivateFileGuard {
        {
            let mut paths = self
                .shared
                .paths
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            *paths.entry(path.clone()).or_insert(0) += 1;
            self.shared.live.fetch_add(1, Ordering::Release);
        }
        PrivateFileGuard {
            files: self.clone(),
            path,
        }
    }

    /// Whether `path` is registered: a file Excise holds open there, which no reader of the tree
    /// may read or list as the user's.
    pub(crate) fn contains(&self, path: &Path) -> bool {
        if self.shared.live.load(Ordering::Acquire) == 0 {
            return false;
        }
        self.shared
            .paths
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .contains_key(path)
    }

    /// Whether `name` in `directory` is registered: [`Self::contains`] for the path they make,
    /// which is not built at all while nothing is registered.
    pub(crate) fn contains_in(&self, directory: &Path, name: &OsStr) -> bool {
        self.shared.live.load(Ordering::Acquire) != 0 && self.contains(&directory.join(name))
    }
}

/// A registration; dropping it unregisters the path.
#[derive(Debug)]
#[cfg_attr(
    not(any(windows, test)),
    allow(
        dead_code,
        reason = "only Windows keeps a named spill file in the user's tree"
    )
)]
pub(crate) struct PrivateFileGuard {
    files: PrivateFiles,
    path: PathBuf,
}

impl Drop for PrivateFileGuard {
    fn drop(&mut self) {
        let mut paths = self
            .files
            .shared
            .paths
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(count) = paths.get_mut(&self.path) {
            *count -= 1;
            if *count == 0 {
                paths.remove(&self.path);
            }
            self.files.shared.live.fetch_sub(1, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_is_registered_from_the_registration_to_its_drop() {
        let files = PrivateFiles::default();
        let path = PathBuf::from("/tree/folder/.spill");

        assert!(!files.contains(&path), "nothing is registered yet");
        let guard = files.register(path.clone());
        assert!(files.contains(&path));
        assert!(
            files.contains_in(Path::new("/tree/folder"), OsStr::new(".spill")),
            "a name in its folder is the same path"
        );
        assert!(
            !files.contains(Path::new("/tree/folder/.other")),
            "only the exact path is registered, not its neighbours"
        );
        assert!(
            !files.contains(Path::new("/tree/folder")),
            "nor the folder that holds it"
        );

        drop(guard);
        assert!(
            !files.contains(&path),
            "the registration ended with its guard"
        );
    }

    #[test]
    fn a_path_registered_twice_stays_registered_until_both_are_dropped() {
        let files = PrivateFiles::default();
        let path = PathBuf::from("/tree/.spill");

        let first = files.register(path.clone());
        let second = files.register(path.clone());
        drop(first);
        assert!(files.contains(&path));
        drop(second);
        assert!(!files.contains(&path));
    }

    #[test]
    fn every_clone_sees_the_same_registrations() {
        let files = PrivateFiles::default();
        let clone = files.clone();
        let path = PathBuf::from("/tree/.spill");

        let guard = clone.register(path.clone());

        assert!(
            files.contains(&path),
            "a registration through a clone is seen by the original"
        );
        drop(guard);
        assert!(!files.contains(&path));
    }

    #[test]
    fn registries_of_different_sessions_do_not_share_paths() {
        let first = PrivateFiles::default();
        let second = PrivateFiles::default();
        let path = PathBuf::from("/tree/.spill");

        let _guard = first.register(path.clone());

        assert!(!second.contains(&path));
    }
}
