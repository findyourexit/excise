//! The startup sweep: removes the scratch directories that dead scan-store sessions left behind.
//!
//! A session that is killed, crashes, or loses power cannot remove its own directory, and nothing
//! else ever did. Every start now sweeps the scratch parent (`scan_store_dir`,
//! `EXCISE_SCAN_STORE_DIR`, or the system temporary directory) before it creates its own session.
//!
//! # What is removed
//!
//! A `.excise-scan-*` directory is removed only when all of these hold:
//!
//! - It is a directory, not a symbolic link or a Windows reparse point, on the parent's file system.
//! - The current user owns it and nobody else can reach it: on Unix, no group or other permission
//!   bit; on Windows, a protected access control list that names the current user alone.
//! - Its `session.lock` is a regular file under the same rules and holds exactly the owner marker
//!   (`super::session_lock`).
//! - The lock can be taken. A live session holds it for as long as it lives, and the operating
//!   system drops it when the process ends however it ends.
//!
//! Everything else in the parent is left exactly as found: live sessions, directories no excise
//! made (including those an earlier version left, which have no lock file), other users'
//! directories, and any entry without the prefix. A directory that cannot be removed completely
//! keeps its lock file, so it stays verifiable and a later sweep finishes it. A sweep never reports
//! anything and never fails the run that started it.
//!
//! # Why two processes starting together are safe
//!
//! A sweep can only remove a directory whose lock it can take, and a live session never gives its
//! lock up. What remains is the moment a session is being set up, and its order closes that: the
//! session creates `session.lock` empty, takes the lock, and only then writes the marker, while a
//! sweep reads the marker first and takes the lock only after finding it complete. So:
//!
//! - A directory that shows a complete marker already had its lock taken by its session, and a
//!   sweep that finds the lock free is looking at a session that has ended.
//! - A directory that has no complete marker yet is left alone, and its lock is never touched, so
//!   a sweep cannot make the session's own lock attempt fail either.
//!
//! The one case that follows is a session killed in the instants between creating its directory
//! and writing the marker. It cannot be told from a foreign directory, so it is left alone.
//!
//! A session that ends normally releases its lock before removing its directory (Windows cannot
//! delete a locked file), so a sweep can overlap that removal. Both remove with every missing
//! entry counting as removed, so the overlap is harmless.
//!
//! # How a directory is removed
//!
//! The directory is removed without leaving it and without following a link: on Unix with
//! descriptor-relative calls on the directory that was verified (`unix`), on Windows through the
//! held handle that verified it (`windows`). Everything but the lock file goes first and the lock
//! file goes last, so an interrupted removal can be finished.

use std::path::Path;

use super::storage::SESSION_PREFIX;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix as platform;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as platform;

/// The most prefixed directories one sweep examines, so a parent full of them cannot make every
/// start slow. Directories past the limit wait for a later start.
const MAX_CANDIDATES: usize = 256;

/// Removes the dead sessions' directories from `parent`, or from the system temporary directory
/// when there is none, which is the parent a new session is created in. Returns how many
/// removals it completed, which two sweeps racing for one directory can both claim. Never fails
/// and never reports: whatever cannot be verified or removed is left.
pub(crate) fn sweep_dead_sessions(parent: Option<&Path>) -> usize {
    match parent {
        Some(parent) => sweep_directory(parent, MAX_CANDIDATES),
        None => sweep_directory(&std::env::temp_dir(), MAX_CANDIDATES),
    }
}

fn sweep_directory(parent: &Path, limit: usize) -> usize {
    platform::sweep(parent, SESSION_PREFIX, limit)
}

#[cfg(test)]
mod tests {
    use std::fs::{self, OpenOptions};
    use std::path::{Path, PathBuf};
    use std::sync::Barrier;
    use std::thread;

    use super::{MAX_CANDIDATES, sweep_dead_sessions, sweep_directory};
    use crate::scan_store::session_lock::{LOCK_FILE_NAME, OWNER_MARKER, SessionLock};
    use crate::scan_store::storage::{SESSION_PREFIX, ScanStoreStorage, create_session_directory};
    use crate::temporary_storage::TemporaryStorage;

    fn parent() -> tempfile::TempDir {
        tempfile::tempdir().expect("a scratch parent can be created")
    }

    /// A session directory exactly as a killed session leaves it: its working files, its lock
    /// file holding the marker, and no process holding the lock.
    fn dead_session(parent: &Path) -> PathBuf {
        let root = create_session_directory(Some(parent))
            .expect("a session directory can be created")
            .keep();
        SessionLock::establish(&root)
            .expect("a new directory can be locked and marked")
            .release();
        fs::create_dir(root.join("runs")).expect("a subdirectory can be created");
        fs::write(root.join("runs").join("0001.run"), b"run bytes").expect("a run can be written");
        fs::create_dir(root.join("index")).expect("a subdirectory can be created");
        fs::write(root.join("index").join("0001.redb"), b"index").expect("an index can be written");
        root
    }

    fn live_session(parent: &Path) -> ScanStoreStorage {
        ScanStoreStorage::new(TemporaryStorage::default(), Some(parent))
            .expect("a session can be created")
    }

    fn names(parent: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(parent)
            .expect("the parent can be read")
            .map(|entry| {
                entry
                    .expect("an entry can be read")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    fn lock_file(session: &Path) -> PathBuf {
        session.join(LOCK_FILE_NAME)
    }

    /// Whether the session's lock file holds the whole owner marker. Measured, not read:
    /// Windows refuses to read a file that another handle has locked.
    fn marker_is_complete(session: &Path) -> bool {
        fs::metadata(lock_file(session)).map(|meta| meta.len()).ok()
            == u64::try_from(OWNER_MARKER.len()).ok()
    }

    #[test]
    fn a_dead_session_is_removed_with_everything_in_it() {
        let parent = parent();
        let dead = dead_session(parent.path());

        assert_eq!(sweep_directory(parent.path(), MAX_CANDIDATES), 1);

        assert!(
            !dead.exists(),
            "the dead session's directory should be gone"
        );
        assert!(names(parent.path()).is_empty());
    }

    #[test]
    fn a_live_session_is_never_removed() {
        let parent = parent();
        let live = live_session(parent.path());
        let dead = dead_session(parent.path());

        assert_eq!(sweep_directory(parent.path(), MAX_CANDIDATES), 1);

        assert!(!dead.exists());
        assert!(
            live.root().join("runs").is_dir(),
            "the live session must keep its files"
        );
        assert!(
            marker_is_complete(live.root()),
            "the live session keeps its lock file and marker"
        );
        drop(live);
        assert!(
            names(parent.path()).is_empty(),
            "a normal end removes the session itself"
        );
    }

    #[test]
    fn a_directory_is_removed_only_once_its_lock_is_free() {
        let parent = parent();
        let dead = dead_session(parent.path());
        let holder = OpenOptions::new()
            .read(true)
            .write(true)
            .open(lock_file(&dead))
            .expect("the lock file can be opened");
        holder.try_lock().expect("a dead session's lock is free");

        assert_eq!(sweep_directory(parent.path(), MAX_CANDIDATES), 0);
        assert!(
            dead.join("runs").is_dir(),
            "a held lock means a live session"
        );

        holder.unlock().expect("the lock can be released");
        drop(holder);
        assert_eq!(sweep_directory(parent.path(), MAX_CANDIDATES), 1);
        assert!(!dead.exists());
    }

    #[test]
    fn a_prefixed_directory_without_a_lock_file_is_left_alone() {
        let parent = parent();
        let unverified = parent
            .path()
            .join(format!("{SESSION_PREFIX}not-made-by-excise"));
        fs::create_dir(&unverified).expect("the directory can be created");
        fs::write(unverified.join("keep.txt"), b"mine").expect("a file can be written");

        assert_eq!(sweep_directory(parent.path(), MAX_CANDIDATES), 0);

        assert_eq!(
            fs::read(unverified.join("keep.txt")).expect("the file survives"),
            b"mine"
        );
    }

    #[test]
    fn a_lock_file_that_is_not_the_owner_marker_is_left_alone() {
        let parent = parent();
        let mut longer = OWNER_MARKER.to_vec();
        longer.extend_from_slice(b"more\n");
        let imposters: [&[u8]; 6] = [
            b"",
            b"not a marker",
            &OWNER_MARKER[..OWNER_MARKER.len() - 1],
            b"excise scan-store session 2\n",
            &longer,
            b"excise scan-store session 1\r\n",
        ];
        for contents in imposters {
            let dead = dead_session(parent.path());
            fs::write(lock_file(&dead), contents).expect("the lock file can be rewritten");

            assert_eq!(
                sweep_directory(parent.path(), MAX_CANDIDATES),
                0,
                "{contents:?}"
            );

            assert!(
                dead.join("runs").is_dir(),
                "{contents:?} is not the owner marker"
            );
            fs::remove_dir_all(&dead).expect("the imposter can be cleaned up");
        }
    }

    #[test]
    fn entries_without_the_prefix_or_that_are_not_directories_are_left_alone() {
        let parent = parent();
        let dead = dead_session(parent.path());
        let renamed = parent.path().join("renamed-session");
        fs::rename(&dead, &renamed).expect("the session can be renamed");
        let file = parent.path().join(format!("{SESSION_PREFIX}a-file"));
        fs::write(&file, OWNER_MARKER).expect("a file can be written");

        assert_eq!(sweep_directory(parent.path(), MAX_CANDIDATES), 0);

        assert!(renamed.join("runs").is_dir());
        assert!(file.is_file());
    }

    #[test]
    fn a_session_whose_removal_was_interrupted_is_finished() {
        let parent = parent();
        let dead = dead_session(parent.path());
        fs::remove_file(dead.join("runs").join("0001.run")).expect("a run can be removed");
        fs::remove_dir(dead.join("runs")).expect("a directory can be removed");

        assert_eq!(sweep_directory(parent.path(), MAX_CANDIDATES), 1);

        assert!(!dead.exists());
    }

    #[test]
    fn a_sweep_examines_a_bounded_number_of_directories() {
        let parent = parent();
        for _ in 0..5 {
            dead_session(parent.path());
        }

        assert_eq!(sweep_directory(parent.path(), 3), 3);
        assert_eq!(names(parent.path()).len(), 2);
        assert_eq!(sweep_directory(parent.path(), 3), 2);
        assert!(names(parent.path()).is_empty());
    }

    #[test]
    fn a_missing_parent_is_not_created_or_reported() {
        let parent = parent();
        let missing = parent.path().join("missing");

        assert_eq!(sweep_dead_sessions(Some(&missing)), 0);

        assert!(!missing.exists());
    }

    #[test]
    fn sweeping_starts_from_the_given_parent_only() {
        let parent = parent();
        let other = tempfile::tempdir().expect("a second parent can be created");
        let dead = dead_session(parent.path());
        let elsewhere = dead_session(other.path());

        assert_eq!(sweep_dead_sessions(Some(parent.path())), 1);

        assert!(!dead.exists());
        assert!(elsewhere.is_dir(), "a sweep stays inside its parent");
    }

    #[test]
    fn a_session_is_never_removed_while_it_is_being_set_up() {
        // The sweep runs after every step that changes what it can see. A session that is
        // visible as marked and unlocked at any of them would be removed from under its owner.
        let parent = parent();
        let root = create_session_directory(Some(parent.path()))
            .expect("a session directory can be created");
        let runs = root.path().join("runs");
        let mut sweeps = 0;

        let lock = SessionLock::establish_observed(root.path(), || {
            sweeps += 1;
            if sweeps == 1 {
                // Made at the first step, as a session makes its working directories once its own
                // directory is private. On Windows one made earlier loses its inherited
                // permissions when the directory is restricted, and a process without the backup
                // privilege cannot remove it.
                fs::create_dir(&runs).expect("a subdirectory can be created");
            }
            assert_eq!(
                sweep_directory(parent.path(), MAX_CANDIDATES),
                0,
                "sweep {sweeps}"
            );
            assert!(
                runs.is_dir(),
                "sweep {sweeps} removed a session being set up"
            );
        })
        .expect("a new directory can be locked and marked");

        assert!(sweeps >= 3, "every step is observed, saw {sweeps}");
        assert!(marker_is_complete(root.path()), "the marker is written");
        lock.release();
        assert_eq!(
            sweep_directory(parent.path(), MAX_CANDIDATES),
            1,
            "released, it is dead"
        );
        assert!(!root.path().exists());
    }

    #[test]
    fn a_complete_marker_always_means_the_lock_is_held() {
        // The stronger form of the same fact, checked directly instead of through a sweep: at
        // every step, a lock file that holds the whole marker cannot be locked by anyone else.
        let parent = parent();
        let root = create_session_directory(Some(parent.path()))
            .expect("a session directory can be created");

        let lock = SessionLock::establish_observed(root.path(), || {
            // Some steps come before the lock file exists (Windows restricts the directory
            // first), so there is nothing to probe until the marker is complete.
            if marker_is_complete(root.path()) {
                let probe = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(lock_file(root.path()))
                    .expect("a complete marker means the lock file exists");
                assert!(
                    probe.try_lock().is_err(),
                    "a complete marker was visible while its lock was free"
                );
            }
        })
        .expect("a new directory can be locked and marked");
        lock.release();
    }

    #[test]
    fn a_lock_file_is_never_reused() {
        let parent = parent();
        let dead = dead_session(parent.path());

        let error = SessionLock::establish(&dead).expect_err("the lock file already exists");

        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(
            fs::read(lock_file(&dead)).expect("the lock file is intact"),
            OWNER_MARKER
        );
    }

    #[test]
    fn sweeps_racing_for_the_same_dead_sessions_remove_all_of_them() {
        const DEAD: usize = 12;
        const SWEEPERS: usize = 4;
        let parent = parent();
        for _ in 0..DEAD {
            dead_session(parent.path());
        }
        let barrier = Barrier::new(SWEEPERS);

        thread::scope(|scope| {
            for _ in 0..SWEEPERS {
                scope.spawn(|| {
                    barrier.wait();
                    let _ = sweep_directory(parent.path(), MAX_CANDIDATES);
                });
            }
        });

        // Whichever sweep reached a directory first took its lock and removed it; the others
        // found the lock held or the directory gone and moved on. None may leave a directory half
        // removed by getting in another's way.
        #[cfg(unix)]
        assert!(
            names(parent.path()).is_empty(),
            "{:?}",
            names(parent.path())
        );
        // On Windows a sweep skips a directory that another sweep holds open (the handle shares no
        // deletion), so one can survive the race. The next sweep, with nothing else running,
        // finishes it.
        let _ = sweep_directory(parent.path(), MAX_CANDIDATES);
        assert!(
            names(parent.path()).is_empty(),
            "{:?}",
            names(parent.path())
        );
    }

    #[test]
    fn starting_sessions_never_lose_their_directory_to_each_others_sweeps() {
        // The shape of two excise processes starting together: each sweeps the shared parent,
        // then creates its session, then works in it. Every session must still be there, with
        // its files, right up to the moment its owner ends it, however the others' sweeps
        // interleave with its creation and its end.
        const STARTERS: usize = 3;
        const ROUNDS: usize = 40;
        let parent = parent();
        let barrier = Barrier::new(STARTERS);

        thread::scope(|scope| {
            for starter in 0..STARTERS {
                let (parent, barrier) = (parent.path(), &barrier);
                scope.spawn(move || {
                    barrier.wait();
                    for round in 0..ROUNDS {
                        let _ = sweep_directory(parent, MAX_CANDIDATES);
                        let session = live_session(parent);
                        for _ in 0..3 {
                            thread::yield_now();
                            let _ = sweep_directory(parent, MAX_CANDIDATES);
                            assert!(
                                session.root().join("runs").is_dir(),
                                "starter {starter}, round {round}: a sweep removed a live session"
                            );
                        }
                    }
                });
            }
        });

        // A session that ended while another starter's sweep held its directory open can be
        // left for a later sweep on Windows. Nothing may be left once one more has run.
        let _ = sweep_directory(parent.path(), MAX_CANDIDATES);
        assert!(
            names(parent.path()).is_empty(),
            "{:?}",
            names(parent.path())
        );
    }

    #[cfg(unix)]
    mod unix {
        use std::fs::{self, Permissions};
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        use rustix::fs::{FileType, fstat};

        use super::{
            MAX_CANDIDATES, SESSION_PREFIX, dead_session, lock_file, names, parent, sweep_directory,
        };
        use crate::scan_store::sweep::unix::is_private_to_user;

        fn user() -> u32 {
            nix::unistd::geteuid().as_raw()
        }

        #[test]
        fn a_symbolic_link_named_like_a_session_is_not_followed() {
            let parent = parent();
            let elsewhere = tempfile::tempdir().expect("a second directory can be created");
            let target = dead_session(elsewhere.path());
            let link = parent.path().join(format!("{SESSION_PREFIX}link"));
            symlink(&target, &link).expect("a symbolic link can be created");

            assert_eq!(sweep_directory(parent.path(), MAX_CANDIDATES), 0);

            assert!(fs::symlink_metadata(&link).is_ok_and(|meta| meta.file_type().is_symlink()));
            assert!(
                target.join("runs").is_dir(),
                "the link's target must be untouched"
            );
        }

        #[test]
        fn links_inside_a_dead_session_are_removed_and_never_followed() {
            let parent = parent();
            let precious = tempfile::tempdir().expect("a directory outside the session");
            fs::write(precious.path().join("keep.txt"), b"mine").expect("a file can be written");
            let dead = dead_session(parent.path());
            symlink(precious.path(), dead.join("runs").join("to-directory"))
                .expect("a link to a directory can be created");
            symlink(
                precious.path().join("keep.txt"),
                dead.join("runs").join("to-file"),
            )
            .expect("a link to a file can be created");
            symlink(
                precious.path().join("missing"),
                dead.join("index").join("dangling"),
            )
            .expect("a dangling link can be created");

            assert_eq!(sweep_directory(parent.path(), MAX_CANDIDATES), 1);

            assert!(!dead.exists());
            assert_eq!(
                fs::read(precious.path().join("keep.txt")).expect("the target survives"),
                b"mine"
            );
        }

        #[test]
        fn a_session_other_users_can_reach_is_left_alone() {
            let parent = parent();
            for mode in [0o755, 0o750, 0o705, 0o770, 0o707] {
                let dead = dead_session(parent.path());
                fs::set_permissions(&dead, Permissions::from_mode(mode))
                    .expect("permissions can be changed");

                assert_eq!(
                    sweep_directory(parent.path(), MAX_CANDIDATES),
                    0,
                    "{mode:o}"
                );

                assert!(dead.join("runs").is_dir(), "mode {mode:o} is not private");
                fs::set_permissions(&dead, Permissions::from_mode(0o700))
                    .expect("permissions can be restored");
                fs::remove_dir_all(&dead).expect("the directory can be cleaned up");
            }
        }

        #[test]
        fn a_lock_file_other_users_can_reach_is_left_alone() {
            let parent = parent();
            for mode in [0o644, 0o660, 0o606] {
                let dead = dead_session(parent.path());
                fs::set_permissions(lock_file(&dead), Permissions::from_mode(mode))
                    .expect("permissions can be changed");

                assert_eq!(
                    sweep_directory(parent.path(), MAX_CANDIDATES),
                    0,
                    "{mode:o}"
                );

                assert!(dead.join("runs").is_dir(), "mode {mode:o} is not private");
                fs::remove_dir_all(&dead).expect("the directory can be cleaned up");
            }
        }

        #[test]
        fn a_lock_file_that_is_a_link_is_left_alone() {
            let parent = parent();
            let dead = dead_session(parent.path());
            let real = dead.join("real.lock");
            fs::rename(lock_file(&dead), &real).expect("the lock file can be moved");
            symlink(&real, lock_file(&dead)).expect("a link can take its place");

            assert_eq!(sweep_directory(parent.path(), MAX_CANDIDATES), 0);

            assert!(dead.join("runs").is_dir());
        }

        #[test]
        fn ownership_decides_which_directories_are_ours() {
            let parent = parent();
            let session = dead_session(parent.path());
            let stat = fstat(fs::File::open(&session).expect("the session can be opened"))
                .expect("the session can be examined");
            assert!(is_private_to_user(&stat, FileType::Directory, user()));
            assert!(
                !is_private_to_user(&stat, FileType::Directory, user().wrapping_add(1)),
                "another user's directory is not ours"
            );
            assert!(
                !is_private_to_user(&stat, FileType::RegularFile, user()),
                "a directory is not a file"
            );
        }

        #[test]
        fn a_tree_deeper_than_a_session_ever_is_left_in_place() {
            let parent = parent();
            let dead = dead_session(parent.path());
            let mut deep = dead.join("runs");
            for level in 0..12 {
                deep.push(format!("level-{level}"));
            }
            fs::create_dir_all(&deep).expect("a deep tree can be created");

            assert_eq!(sweep_directory(parent.path(), MAX_CANDIDATES), 0);

            assert!(deep.is_dir(), "a tree no session makes is not walked");
            assert!(lock_file(&dead).is_file(), "the session stays verifiable");
        }

        #[test]
        fn a_removal_that_fails_keeps_the_session_verifiable_for_the_next_sweep() {
            if user() == 0 {
                // Permission bits do not stop the superuser, so there is nothing to fail.
                return;
            }
            let parent = parent();
            let dead = dead_session(parent.path());
            let stuck = dead.join("runs").join("stuck");
            fs::create_dir(&stuck).expect("a subdirectory can be created");
            fs::write(stuck.join("file"), b"x").expect("a file can be written");
            fs::set_permissions(&stuck, Permissions::from_mode(0o500))
                .expect("permissions can be changed");

            assert_eq!(sweep_directory(parent.path(), MAX_CANDIDATES), 0);
            assert!(
                lock_file(&dead).is_file(),
                "the lock file stays until everything else is gone"
            );

            fs::set_permissions(&stuck, Permissions::from_mode(0o700))
                .expect("permissions can be restored");
            assert_eq!(sweep_directory(parent.path(), MAX_CANDIDATES), 1);
            assert!(names(parent.path()).is_empty());
        }

        #[test]
        fn a_wide_directory_is_removed_in_full() {
            let parent = parent();
            let dead = dead_session(parent.path());
            for index in 0..700 {
                fs::write(dead.join("runs").join(format!("{index:04}.run")), b"x")
                    .expect("a run can be written");
            }

            assert_eq!(sweep_directory(parent.path(), MAX_CANDIDATES), 1);

            assert!(!dead.exists());
        }
    }
}
