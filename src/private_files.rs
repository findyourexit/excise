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
//! unregistered when the file that owns it is dropped. The scanner and the overlay's listing skip
//! a registered path without reading it. The match is by path and never by name pattern: a
//! user's own file may carry any name, so the shape of a name proves nothing about who made it.
//!
//! The registration and the file go together ([`PrivateFile`]), and the file is closed, which on
//! Windows removes its name, with the registry locked: the lock is let go only once the path is
//! released, and every lookup takes it. A lookup therefore finds either the live file with its
//! path registered or a free path, never a path that another process has taken since the file
//! went, still registered as Excise's own, which would hide a user's file from the scan.
//!
//! A registry belongs to a session: the storage the deletion spills are charged to and the storage
//! the scanner and the scan store use share one ([`crate::temporary_storage::TemporaryStorage`]).

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::File;
use std::ops::{Deref, DerefMut};
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
    /// registered twice stays registered until both are dropped. Registering, releasing, closing a
    /// registered file, and every lookup go through this lock.
    paths: RwLock<HashMap<PathBuf, usize>>,
}

impl PrivateFiles {
    /// Registers `path`, a file this session is about to create in the user's tree. The path stays
    /// registered until the returned [`PrivateFile`] is dropped, and the file the path names is
    /// handed to it as soon as it exists ([`PrivateFile::hold`]).
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
    pub(crate) fn register(&self, path: PathBuf) -> PrivateFile {
        PrivateFile::new(self, path)
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

    /// Every lookup waits for as long as the guard lives, as it does while a registered file is
    /// closed.
    #[cfg(all(test, windows))]
    pub(crate) fn hold_lookups(&self) -> impl Drop + '_ {
        self.shared
            .paths
            .read()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// A registered path, and the file it names once that exists. Dropping it closes the file, which
/// on Windows removes the file's name, and releases the path, as one step with respect to every
/// lookup: the registry is locked before the file is closed and unlocked after the path is
/// released.
///
/// `T` is the file; it is a [`File`] everywhere but in the tests of that guarantee.
#[derive(Debug)]
pub(crate) struct PrivateFile<T = File> {
    files: PrivateFiles,
    path: PathBuf,
    /// Taken only by `drop`, under the registry's lock.
    held: Option<T>,
}

impl<T> PrivateFile<T> {
    /// Registers `path` in `files`, with no file held yet ([`PrivateFiles::register`]).
    pub(crate) fn new(files: &PrivateFiles, path: PathBuf) -> Self {
        {
            let mut paths = files
                .shared
                .paths
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            *paths.entry(path.clone()).or_insert(0) += 1;
            files.shared.live.fetch_add(1, Ordering::Release);
        }
        Self {
            files: files.clone(),
            path,
            held: None,
        }
    }

    /// Hands over the file that the registered path now names, which this keeps open until it is
    /// dropped. Hand it over before anything else can fail, so that every way out closes the file
    /// and releases the path together.
    #[cfg_attr(
        not(any(windows, test)),
        allow(
            dead_code,
            reason = "only Windows keeps a named spill file in the user's tree"
        )
    )]
    pub(crate) fn hold(&mut self, held: T) {
        let previous = self.held.replace(held);
        debug_assert!(previous.is_none(), "a registration holds one file");
    }
}

impl<T> Deref for PrivateFile<T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.held
            .as_ref()
            .expect("a registered file is held from the moment it exists until it is dropped")
    }
}

impl<T> DerefMut for PrivateFile<T> {
    fn deref_mut(&mut self) -> &mut T {
        self.held
            .as_mut()
            .expect("a registered file is held from the moment it exists until it is dropped")
    }
}

impl<T> Drop for PrivateFile<T> {
    fn drop(&mut self) {
        // Locked first and let go last: the file closes, and with it its name goes, while no
        // lookup can be answered.
        let mut paths = self
            .files
            .shared
            .paths
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        drop(self.held.take());
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
    use std::sync::atomic::AtomicBool;
    use std::sync::{TryLockError, mpsc};
    use std::thread;
    use std::time::Duration;

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

    /// What a file sees of the registry as it closes.
    struct SeesTheRegistryWhenItCloses {
        files: PrivateFiles,
        closed: Arc<AtomicBool>,
        registry_was_locked: Arc<AtomicBool>,
    }

    impl Drop for SeesTheRegistryWhenItCloses {
        fn drop(&mut self) {
            // The thread that closes the file holds the registry's write lock, so nothing can
            // read the registry now, and this asks without waiting.
            let locked = matches!(
                self.files.shared.paths.try_read(),
                Err(TryLockError::WouldBlock)
            );
            self.registry_was_locked.store(locked, Ordering::SeqCst);
            self.closed.store(true, Ordering::SeqCst);
        }
    }

    /// On Windows closing a file removes its name, and another process can take the name at once.
    /// If the path were released after the file closed, a lookup in between would find a path
    /// that is free and still registered as Excise's own, and a scan would skip a user's file that
    /// was made there. The file therefore closes with the registry locked, which every lookup
    /// waits for, and the path is released before the lock is.
    #[test]
    fn a_file_closes_with_the_registry_locked_and_its_path_is_released_before_the_lock_is() {
        let files = PrivateFiles::default();
        let path = PathBuf::from("/tree/folder/.spill");
        let closed = Arc::new(AtomicBool::new(false));
        let registry_was_locked = Arc::new(AtomicBool::new(false));
        let mut private = PrivateFile::<SeesTheRegistryWhenItCloses>::new(&files, path.clone());
        private.hold(SeesTheRegistryWhenItCloses {
            files: files.clone(),
            closed: Arc::clone(&closed),
            registry_was_locked: Arc::clone(&registry_was_locked),
        });
        assert!(files.contains(&path), "registered while the file lives");

        drop(private);

        assert!(closed.load(Ordering::SeqCst), "the file closed");
        assert!(
            registry_was_locked.load(Ordering::SeqCst),
            "the file closed with the registry unlocked: a lookup could have been answered between the file going and its path being released"
        );
        assert!(!files.contains(&path), "the path was released");
    }

    /// The same, from the other side: a lookup that is waiting for the registry holds the file
    /// open, and the path registered, until it has been answered.
    #[test]
    fn a_file_is_not_closed_while_a_lookup_holds_the_registry() {
        let files = PrivateFiles::default();
        let path = PathBuf::from("/tree/folder/.spill");
        let closed = Arc::new(AtomicBool::new(false));
        let mut private = PrivateFile::<SeesTheRegistryWhenItCloses>::new(&files, path.clone());
        private.hold(SeesTheRegistryWhenItCloses {
            files: files.clone(),
            closed: Arc::clone(&closed),
            registry_was_locked: Arc::new(AtomicBool::new(false)),
        });
        let reading = files
            .shared
            .paths
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        let (started, running) = mpsc::channel();
        let closing = thread::spawn(move || {
            started.send(()).expect("the test is waiting");
            drop(private);
        });
        running.recv().expect("the closing thread should start");
        thread::sleep(Duration::from_millis(100));

        assert!(
            !closed.load(Ordering::SeqCst),
            "the file closed while a lookup held the registry"
        );

        drop(reading);
        closing.join().expect("the closing thread should end");
        assert!(closed.load(Ordering::SeqCst));
        assert!(!files.contains(&path));
    }
}
