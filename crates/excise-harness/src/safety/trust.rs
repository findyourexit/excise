//! Whether another user can change what a directory holds.
//!
//! The read-only soak runs a copy of the binary under test, by path, from a directory that it makes
//! in its scratch base, once for each scan and session, and the program it runs works on a real
//! tree. A scratch base in which another user can rename entries lets that user put another
//! program where the copy was, between one run of it and the next: a rename inside a directory
//! needs only write and search permission on that directory, whatever the mode of what is renamed.
//! So the base must be a directory that only its owner can change, and so must every directory
//! above it, which is what OpenSSH's `StrictModes` asks of the files it trusts: each directory on
//! the path is owned by the user running the soak or by root, and is not writable by its group or
//! by everybody, unless its sticky bit is set, which stops a user from renaming or removing what
//! another user made (`/tmp` is such a directory).
//!
//! The owner and the mode are what `stat` reports, and the check trusts them only where the file
//! system enforces them. On macOS a volume can be mounted with "Ignore ownership", which is the
//! default for an external disk: `stat` then shows the caller as the owner of every entry, and
//! every user is treated as the owner, so no mode makes a directory there private. The check
//! refuses a directory on such a volume (`MNT_IGNORE_OWNERSHIP`, read with `statfs`). Linux has no
//! such flag, and for its local file systems the kernel's permission check uses the owner and the
//! mode that `stat` reports, so the walk is as strong there as the rule says; the check does not
//! recognize a file system that decides for itself (FUSE with `allow_other` and without
//! `default_permissions`, network and virtual-machine shares), and the documents say so.
//!
//! The check reads owners and mode bits. It does not read access control lists, which can grant
//! more than the mode says, and it is a look at one moment: what it does not prevent is a change
//! that comes later by a process that already has the owner's rights.

use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
};

use crate::run_support::safe_path_text;

/// The sticky bit.
#[cfg(unix)]
const STICKY: u32 = 0o1000;
/// The write bit of the group.
#[cfg(unix)]
const GROUP_WRITE: u32 = 0o020;
/// The write bit of everybody else.
#[cfg(unix)]
const OTHERS_WRITE: u32 = 0o002;

/// What is wrong with a directory that is not private to its owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Untrusted {
    /// It cannot be looked at: the text says why.
    Unreadable(String),
    /// Another user, who is neither the one running the soak nor root, owns it and so can change
    /// its mode and everything in it.
    OwnedBy(u32),
    /// Its group can write it, and its sticky bit is not set.
    GroupWritable,
    /// Everybody can write it, and its sticky bit is not set.
    WorldWritable,
    /// The file system that holds it does not enforce ownership (on macOS, a volume mounted with
    /// "Ignore ownership"): every user is treated as the owner of everything on it, whatever
    /// `stat` says, so no mode keeps another user out.
    IgnoresOwnership,
}

/// A directory that another user can change, with the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UntrustedDirectory {
    /// The directory that fails: the one that was asked about, or one of the directories above it.
    pub directory: PathBuf,
    /// What is wrong with it.
    pub why: Untrusted,
}

impl fmt::Display for UntrustedDirectory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let directory = safe_path_text(&self.directory);
        match &self.why {
            Untrusted::Unreadable(why) => {
                write!(formatter, "`{directory}` cannot be inspected: {why}")
            }
            Untrusted::OwnedBy(user) => write!(
                formatter,
                "`{directory}` is owned by user {user}, who is neither you nor root, and so can \
                 change what is in it"
            ),
            Untrusted::GroupWritable => write!(
                formatter,
                "`{directory}` can be written by its group and is not sticky, so the members of \
                 the group can rename and replace what is in it"
            ),
            Untrusted::WorldWritable => write!(
                formatter,
                "`{directory}` can be written by everybody and is not sticky, so any user can \
                 rename and replace what is in it"
            ),
            Untrusted::IgnoresOwnership => write!(
                formatter,
                "`{directory}` is on a file system that ignores ownership (a macOS volume \
                 mounted with \"Ignore ownership\", the default for an external disk), so every \
                 user is treated as its owner and no mode keeps another user out"
            ),
        }
    }
}

impl UntrustedDirectory {
    /// What to do about it, as a clause that follows the reason. The soak's own settings cannot
    /// make such a directory private: the way out is another scratch directory.
    #[must_use]
    pub const fn way_out(&self) -> &'static str {
        match self.why {
            Untrusted::GroupWritable | Untrusted::WorldWritable => {
                "make it private (`chmod go-w` is the usual fix), or point EXCISE_E2E_TMPDIR at a \
                 directory that only you can write, below directories that only you or root can \
                 write (unless they are sticky, like /tmp)"
            }
            Untrusted::OwnedBy(_) => {
                "point EXCISE_E2E_TMPDIR at a directory that only you can write, below \
                 directories that only you or root can write (unless they are sticky, like /tmp)"
            }
            Untrusted::IgnoresOwnership => {
                "point EXCISE_E2E_TMPDIR at a directory on a file system that enforces ownership: \
                 an internal disk, or an external one after `sudo diskutil enableOwnership` on \
                 its volume (a mode, such as `chmod go-w`, changes nothing there)"
            }
            Untrusted::Unreadable(_) => {
                "point EXCISE_E2E_TMPDIR at a directory that you can inspect, below directories \
                 that only you or root can write (unless they are sticky, like /tmp)"
            }
        }
    }
}

impl std::error::Error for UntrustedDirectory {}

/// Refuses `path` unless only its owner can change what it holds: it, and every directory above it,
/// is on a file system that enforces ownership, is owned by the user who runs this or by root, and
/// is not writable by its group or by everybody unless its sticky bit is set. A directory that is
/// not there yet is judged by the nearest one that is, since that is where it will be made. Every
/// link is followed first, so what is judged is where the path leads.
///
/// Whether a file system enforces ownership is asked where the platform can say: macOS volumes
/// mounted with "Ignore ownership" are refused. Elsewhere every file system is taken to enforce
/// what `stat` reports, which holds for the local file systems of Linux and not for ones that
/// decide for themselves (see the module documentation).
///
/// Nothing outside Unix has this ownership model, and there it accepts everything: the soak that
/// asks does not run there.
///
/// # Errors
///
/// Returns the first directory, from `path` upwards, that another user can change, and why; or one
/// that cannot be inspected.
pub fn check_private_directory(path: &Path) -> Result<(), UntrustedDirectory> {
    #[cfg(unix)]
    {
        unix::check(path)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

/// What identifies a file as the one that was made: where it is on the disk (its device and its
/// inode), who owns it, and when it last changed (its change time, in seconds and nanoseconds).
/// Nothing outside Unix has it, and there every file is the one.
///
/// It is read from the open file once the file is whole, after its last write and its last change
/// of mode, and not from its path, and it is checked against what is at the path later, or against
/// another open file. A directory that was renamed away and made again, a file that was replaced,
/// and a link that was put there are not the file that was made, and neither is one that was
/// written in place, which keeps its inode, its owner, and its mode but not its change time. (The
/// change time, unlike the modification time, cannot be set back by a program that writes the
/// file.) The soak asks it of the copy of the binary under test before each scan and session, and
/// of the build's copy when it opens it, as a second line behind the refusal of a directory that
/// other users can change ([`check_private_directory`]).
///
/// Whatever changes the file's inode afterwards moves the change time, a change of its mode or of
/// an extended attribute included, and makes it another file: a file that is checked is left alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileIdentity {
    #[cfg(unix)]
    file: (u64, u64),
    #[cfg(unix)]
    owner: u32,
    /// The change time: seconds and nanoseconds.
    #[cfg(unix)]
    changed: (i64, i64),
}

/// How the file at a path is not the file that was made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotTheFile {
    /// There is nothing there, or what is there cannot be looked at.
    Missing,
    /// What is there is not a regular file (a link, a directory).
    NotRegular,
    /// It is another file: another place on the disk.
    Another,
    /// Another user owns it.
    OtherOwner,
    /// Its group or everybody can write it.
    Writable,
    /// It is the same place on the disk, and it was written to or changed after it was made: its
    /// change time moved.
    Changed,
}

impl NotTheFile {
    /// What is wrong, as a phrase for a sentence.
    #[must_use]
    pub const fn why(self) -> &'static str {
        match self {
            Self::Missing => "it cannot be found",
            Self::NotRegular => "it is not a regular file",
            Self::Another => "it is another file",
            Self::OtherOwner => "another user owns it",
            Self::Writable => "its group or everybody can write it",
            Self::Changed => "it was changed after it was made",
        }
    }
}

impl FileIdentity {
    /// The identity of `file`, which is open: its own, whatever is at its path. Take it once the
    /// file is whole: a write or a change of mode after it moves the change time.
    ///
    /// # Errors
    ///
    /// Returns the error of looking at the open file.
    pub fn of(file: &fs::File) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;

            let metadata = file.metadata()?;
            Ok(Self {
                file: (metadata.dev(), metadata.ino()),
                owner: metadata.uid(),
                changed: (metadata.ctime(), metadata.ctime_nsec()),
            })
        }
        #[cfg(not(unix))]
        {
            let _ = file;
            Ok(Self {})
        }
    }

    /// Whether the file at `path`, looked at without following a link, is still that file, owned
    /// by the same user, writable by nobody else, and unchanged.
    ///
    /// # Errors
    ///
    /// Returns how the file at `path` is not the file.
    pub fn check(self, path: &Path) -> Result<(), NotTheFile> {
        #[cfg(unix)]
        {
            let metadata = fs::symlink_metadata(path).map_err(|_| NotTheFile::Missing)?;
            self.compare(&metadata)
        }
        #[cfg(not(unix))]
        {
            let _ = (self, path);
            Ok(())
        }
    }

    /// Whether `file`, which is open, is that file, owned by the same user, writable by nobody
    /// else, and unchanged: the question of [`FileIdentity::check`] asked of a file that is already
    /// open, so that no path is looked at and none can be swapped. Asked before a file is read and
    /// again after, it says that nothing wrote to it in between.
    ///
    /// # Errors
    ///
    /// Returns how `file` is not the file.
    pub fn check_open(self, file: &fs::File) -> Result<(), NotTheFile> {
        #[cfg(unix)]
        {
            let metadata = file.metadata().map_err(|_| NotTheFile::Missing)?;
            self.compare(&metadata)
        }
        #[cfg(not(unix))]
        {
            let _ = (self, file);
            Ok(())
        }
    }

    /// What `metadata` says of a file, against what was recorded of the one that was made.
    #[cfg(unix)]
    fn compare(self, metadata: &fs::Metadata) -> Result<(), NotTheFile> {
        use std::os::unix::fs::MetadataExt as _;

        if !metadata.is_file() {
            return Err(NotTheFile::NotRegular);
        }
        if (metadata.dev(), metadata.ino()) != self.file {
            return Err(NotTheFile::Another);
        }
        if metadata.uid() != self.owner {
            return Err(NotTheFile::OtherOwner);
        }
        if metadata.mode() & (GROUP_WRITE | OTHERS_WRITE) != 0 {
            return Err(NotTheFile::Writable);
        }
        if (metadata.ctime(), metadata.ctime_nsec()) != self.changed {
            return Err(NotTheFile::Changed);
        }
        Ok(())
    }
}

#[cfg(unix)]
mod unix {
    use std::{
        fs, io,
        os::unix::fs::MetadataExt as _,
        path::{Path, PathBuf},
    };

    use super::{GROUP_WRITE, OTHERS_WRITE, STICKY, Untrusted, UntrustedDirectory};

    /// `MNT_IGNORE_OWNERSHIP` of `<sys/mount.h>`: the flag of a macOS volume that ignores the
    /// ownership recorded on it ("Ignore ownership on this volume").
    #[cfg(target_os = "macos")]
    const MNT_IGNORE_OWNERSHIP: u32 = 0x0020_0000;

    pub(super) fn check(path: &Path) -> Result<(), UntrustedDirectory> {
        let user = rustix::process::geteuid().as_raw();
        #[cfg(target_os = "macos")]
        let enforces = ownership_is_enforced;
        // Linux says nothing of the kind: see the module documentation.
        #[cfg(not(target_os = "macos"))]
        let enforces = |_: &Path| -> io::Result<bool> { Ok(true) };
        check_with(path, user, &enforces)
    }

    /// [`check`] for `user`, with the question of whether the file system that holds a directory
    /// enforces ownership given, so that a test can stand in for a volume that does not.
    ///
    /// For each directory from `path` upwards, the question is asked before the owner and the
    /// mode are judged: on a file system that ignores ownership they say nothing, and the advice
    /// that suits a mode would not help.
    pub(super) fn check_with(
        path: &Path,
        user: u32,
        enforces: &dyn Fn(&Path) -> io::Result<bool>,
    ) -> Result<(), UntrustedDirectory> {
        let unreadable = |directory: &Path, error: &io::Error| UntrustedDirectory {
            directory: directory.to_path_buf(),
            why: Untrusted::Unreadable(error.to_string()),
        };
        let canonical =
            nearest_existing_directory(path).map_err(|error| unreadable(path, &error))?;
        for directory in canonical.ancestors() {
            let metadata =
                fs::symlink_metadata(directory).map_err(|error| unreadable(directory, &error))?;
            match enforces(directory) {
                Ok(true) => {}
                Ok(false) => {
                    return Err(UntrustedDirectory {
                        directory: directory.to_path_buf(),
                        why: Untrusted::IgnoresOwnership,
                    });
                }
                Err(error) => return Err(unreadable(directory, &error)),
            }
            judge(directory, metadata.uid(), metadata.mode(), user)?;
        }
        Ok(())
    }

    /// Whether the file system that holds `directory` enforces the ownership it records. A volume
    /// that macOS mounted with "Ignore ownership" does not: `stat` shows the caller as the owner of
    /// every entry on it, and every user is treated as the owner.
    #[cfg(target_os = "macos")]
    fn ownership_is_enforced(directory: &Path) -> io::Result<bool> {
        let file_system = rustix::fs::statfs(directory)?;
        Ok(file_system.f_flags & MNT_IGNORE_OWNERSHIP == 0)
    }

    /// The canonical path of `path`, or of the nearest directory above it that exists: where a
    /// directory that is not made yet will be made.
    fn nearest_existing_directory(path: &Path) -> io::Result<PathBuf> {
        let mut candidate = std::path::absolute(path)?;
        loop {
            match fs::canonicalize(&candidate) {
                Ok(canonical) => return Ok(canonical),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    if !candidate.pop() {
                        return Err(error);
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Whether the directory, owned by `owner` and with `mode`, is one that only `user` and root
    /// can change.
    pub(super) fn judge(
        directory: &Path,
        owner: u32,
        mode: u32,
        user: u32,
    ) -> Result<(), UntrustedDirectory> {
        let refuse = |why| {
            Err(UntrustedDirectory {
                directory: directory.to_path_buf(),
                why,
            })
        };
        if owner != user && owner != 0 {
            return refuse(Untrusted::OwnedBy(owner));
        }
        if mode & STICKY == 0 {
            if mode & OTHERS_WRITE != 0 {
                return refuse(Untrusted::WorldWritable);
            }
            if mode & GROUP_WRITE != 0 {
                return refuse(Untrusted::GroupWritable);
            }
        }
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::{
        fs,
        os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _, symlink},
        thread,
        time::{Duration, Instant},
    };

    use super::{
        unix::{check_with, judge},
        *,
    };

    const ME: u32 = 501;
    const SOMEBODY_ELSE: u32 = 502;

    fn refused(owner: u32, mode: u32) -> Option<Untrusted> {
        judge(Path::new("/d"), owner, mode, ME).err().map(|e| e.why)
    }

    #[test]
    fn a_directory_that_only_its_owner_can_write_is_private_whoever_the_owner_is_among_me_and_root()
    {
        for owner in [ME, 0] {
            for mode in [0o700, 0o755, 0o750, 0o555, 0o711] {
                assert_eq!(refused(owner, mode), None, "{owner} {mode:o}");
            }
        }
    }

    #[test]
    fn a_directory_owned_by_somebody_else_is_refused_whatever_its_mode() {
        for mode in [0o700, 0o755, 0o1777, 0o1700] {
            assert_eq!(
                refused(SOMEBODY_ELSE, mode),
                Some(Untrusted::OwnedBy(SOMEBODY_ELSE)),
                "{mode:o}"
            );
        }
    }

    #[test]
    fn a_directory_that_its_group_or_everybody_can_write_is_refused_unless_it_is_sticky() {
        for owner in [ME, 0] {
            assert_eq!(refused(owner, 0o770), Some(Untrusted::GroupWritable));
            assert_eq!(refused(owner, 0o775), Some(Untrusted::GroupWritable));
            assert_eq!(refused(owner, 0o707), Some(Untrusted::WorldWritable));
            assert_eq!(refused(owner, 0o777), Some(Untrusted::WorldWritable));
            // The sticky bit stops a user from renaming or removing what another made.
            for mode in [0o1770, 0o1775, 0o1777, 0o1707] {
                assert_eq!(refused(owner, mode), None, "{mode:o}");
            }
        }
    }

    /// A directory made with exactly `mode`, whatever the umask.
    fn directory_in(parent: &Path, name: &str, mode: u32) -> PathBuf {
        let path = parent.join(name);
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .expect("a directory");
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).expect("its mode");
        path
    }

    /// A private directory to work in, below the system's temporary one.
    fn private() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("xt-trust-")
            .tempdir()
            .expect("a directory")
    }

    #[test]
    fn a_private_directory_below_a_private_one_is_accepted() {
        let base = private();
        let inside = directory_in(base.path(), "inside", 0o700);

        assert_eq!(check_private_directory(&inside), Ok(()));
        assert_eq!(check_private_directory(base.path()), Ok(()));
    }

    #[test]
    fn a_directory_that_everybody_or_the_group_can_write_is_refused_and_named() {
        let base = private();
        let world = directory_in(base.path(), "world", 0o777);
        let group = directory_in(base.path(), "group", 0o770);

        let by_world = check_private_directory(&world).expect_err("world-writable");
        let by_group = check_private_directory(&group).expect_err("group-writable");

        assert_eq!(by_world.why, Untrusted::WorldWritable);
        assert_eq!(by_group.why, Untrusted::GroupWritable);
        assert_eq!(
            by_world.directory,
            fs::canonicalize(&world).expect("a canonical path")
        );
        let text = by_world.to_string();
        assert!(text.contains("can be written by everybody"), "{text}");
        assert!(text.contains("rename and replace"), "{text}");
    }

    #[test]
    fn a_sticky_directory_that_everybody_can_write_is_accepted() {
        let base = private();
        let sticky = directory_in(base.path(), "sticky", 0o1777);

        assert_eq!(check_private_directory(&sticky), Ok(()));
    }

    #[test]
    fn a_private_directory_below_one_that_everybody_can_write_is_refused_for_the_one_above() {
        let base = private();
        let open = directory_in(base.path(), "open", 0o777);
        let below = directory_in(&open, "below", 0o700);

        let refusal = check_private_directory(&below).expect_err("the directory above is open");

        assert_eq!(refusal.why, Untrusted::WorldWritable);
        assert_eq!(
            refusal.directory,
            fs::canonicalize(&open).expect("a canonical path"),
            "it is the directory above that is named"
        );
    }

    #[test]
    fn a_directory_that_is_not_made_yet_is_judged_by_the_one_it_will_be_made_in() {
        let base = private();
        let open = directory_in(base.path(), "open", 0o777);
        let private_one = directory_in(base.path(), "private", 0o700);

        let in_the_open = check_private_directory(&open.join("not/yet/made"));
        let in_the_private = check_private_directory(&private_one.join("not/yet/made"));

        assert_eq!(
            in_the_open
                .expect_err("made in an open directory")
                .directory,
            fs::canonicalize(&open).expect("a canonical path")
        );
        assert_eq!(in_the_private, Ok(()));
    }

    #[test]
    fn a_link_is_followed_and_it_is_where_it_leads_that_is_judged() {
        let base = private();
        let open = directory_in(base.path(), "open", 0o777);
        let private_one = directory_in(base.path(), "private", 0o700);
        let to_open = base.path().join("to-open");
        let to_private = base.path().join("to-private");
        symlink(&open, &to_open).expect("a link");
        symlink(&private_one, &to_private).expect("a link");

        let refused = check_private_directory(&to_open).expect_err("it leads to an open directory");

        assert_eq!(refused.why, Untrusted::WorldWritable);
        assert_eq!(check_private_directory(&to_private), Ok(()));
    }

    #[test]
    fn a_path_that_cannot_be_looked_at_is_refused_and_not_taken_for_private() {
        let base = private();
        let shut = directory_in(base.path(), "shut", 0o000);

        let refusal = check_private_directory(&shut.join("below/that"));

        fs::set_permissions(&shut, fs::Permissions::from_mode(0o700)).expect("opened again");
        // A user that can search everything (root) can look at it, and has no refusal to give.
        if let Err(refusal) = refusal {
            assert!(matches!(refusal.why, Untrusted::Unreadable(_)), "{refusal}");
        }
    }

    /// A file that the test made, open, and its identity.
    fn made_file(directory: &Path, name: &str) -> (PathBuf, FileIdentity) {
        let path = directory.join(name);
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .expect("a file");
        file.set_permissions(fs::Permissions::from_mode(0o700))
            .expect("its mode");
        let identity = FileIdentity::of(&file).expect("its identity");
        (path, identity)
    }

    /// Writes to the file at `path` in place, over and over, until `identity` says that it is not
    /// the file any more, and returns what it said. It says so as soon as the clock of the file
    /// system has moved on from the moment the file was made: that clock ticks in steps (a
    /// millisecond or ten, a second on an old file system), so one write can land in the same
    /// tick as the last change. Five seconds at most.
    fn written_until_it_shows(path: &Path, identity: FileIdentity) -> Result<(), NotTheFile> {
        use std::io::Write as _;

        let give_up = Instant::now() + Duration::from_secs(5);
        loop {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .open(path)
                .expect("opened for writing");
            file.write_all(b"changed in place").expect("written");
            drop(file);
            let verdict = identity.check(path);
            if verdict.is_err() || Instant::now() >= give_up {
                return verdict;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn the_file_that_was_made_is_the_file_until_something_writes_to_it_in_place() {
        let base = private();
        let (path, identity) = made_file(base.path(), "excise");
        let before = fs::metadata(&path).expect("its metadata");
        assert_eq!(identity.check(&path), Ok(()));

        let verdict = written_until_it_shows(&path, identity);

        // Written in place, it is the same place on the disk, with the same owner and the same
        // mode: only the change time says that it was written, and a program that writes a file
        // cannot set that back.
        let after = fs::metadata(&path).expect("its metadata");
        assert_eq!(
            (after.dev(), after.ino(), after.uid(), after.mode()),
            (before.dev(), before.ino(), before.uid(), before.mode())
        );
        assert_eq!(verdict, Err(NotTheFile::Changed));
        assert_eq!(
            NotTheFile::Changed.why(),
            "it was changed after it was made"
        );
    }

    #[test]
    fn a_file_that_is_open_is_asked_the_same_questions_and_no_path_is_looked_at() {
        let base = private();
        let (path, identity) = made_file(base.path(), "excise");
        let open = fs::File::open(&path).expect("opened for reading");
        assert_eq!(identity.check_open(&open), Ok(()));

        // Another file put in its place by a rename is another file, through a handle as through
        // a path.
        let (other, _) = made_file(base.path(), "other");
        fs::rename(&other, &path).expect("replaced by a rename");
        let replacement = fs::File::open(&path).expect("opened for reading");
        assert_eq!(identity.check_open(&replacement), Err(NotTheFile::Another));

        // Written in place while it is open: the change time moves.
        let (written, written_identity) = made_file(base.path(), "written");
        let written_open = fs::File::open(&written).expect("opened for reading");
        assert_eq!(
            written_until_it_shows(&written, written_identity),
            Err(NotTheFile::Changed)
        );
        assert_eq!(
            written_identity.check_open(&written_open),
            Err(NotTheFile::Changed)
        );
    }

    #[test]
    fn a_file_put_in_its_place_by_a_rename_is_another_file() {
        let base = private();
        let (path, identity) = made_file(base.path(), "excise");
        let (other, _) = made_file(base.path(), "other");

        fs::rename(&other, &path).expect("replaced by a rename");

        assert_eq!(identity.check(&path), Err(NotTheFile::Another));
    }

    #[test]
    fn a_directory_renamed_away_and_made_again_with_another_file_in_it_is_not_the_file() {
        let base = private();
        let directory = directory_in(base.path(), "bin", 0o700);
        let (path, identity) = made_file(&directory, "excise");

        fs::rename(&directory, base.path().join("moved")).expect("renamed away");
        let again = directory_in(base.path(), "bin", 0o700);
        let (substitute, _) = made_file(&again, "excise");

        assert_eq!(substitute, path);
        assert_eq!(identity.check(&path), Err(NotTheFile::Another));
    }

    #[test]
    fn a_link_a_directory_or_nothing_where_the_file_was_is_not_the_file() {
        let base = private();
        let (path, identity) = made_file(base.path(), "excise");
        let (target, _) = made_file(base.path(), "target");

        fs::remove_file(&path).expect("removed");
        assert_eq!(identity.check(&path), Err(NotTheFile::Missing));
        symlink(&target, &path).expect("a link");
        assert_eq!(identity.check(&path), Err(NotTheFile::NotRegular));
        fs::remove_file(&path).expect("removed");
        fs::create_dir(&path).expect("a directory");
        assert_eq!(identity.check(&path), Err(NotTheFile::NotRegular));
    }

    #[test]
    fn a_file_that_the_group_or_everybody_can_write_is_not_the_file_the_soak_made() {
        let base = private();
        let (path, identity) = made_file(base.path(), "excise");

        for (mode, expected) in [(0o770, NotTheFile::Writable), (0o707, NotTheFile::Writable)] {
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).expect("its mode");
            assert_eq!(identity.check(&path), Err(expected), "{mode:o}");
        }
        // A mode that nobody else can write is no write by others. It is still a change of the
        // file, which moves the change time: a file that is checked is left alone.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("its mode");
        assert_eq!(identity.check(&path), Err(NotTheFile::Changed));
    }

    #[test]
    fn a_directory_on_a_file_system_that_ignores_ownership_is_refused_whatever_its_mode() {
        let base = private();
        // What is at or below `volume` stands for a volume that macOS mounted with "Ignore
        // ownership": `stat` shows the caller as the owner of all of it, and its mode is as open
        // as it likes.
        let volume = directory_in(base.path(), "volume", 0o777);
        let below = directory_in(&volume, "scratch", 0o700);
        let on_the_volume = fs::canonicalize(&volume).expect("a canonical path");
        let enforces = |directory: &Path| Ok(!directory.starts_with(&on_the_volume));
        let me = rustix::process::geteuid().as_raw();

        let refused_below = check_with(&below, me, &enforces).expect_err("on the volume");
        let refused_at = check_with(&volume, me, &enforces).expect_err("the volume itself");
        let beside = check_with(base.path(), me, &enforces);

        // The first directory from the path upwards that is on such a file system is named, and
        // what is said of it is not what a mode would say: the mode of the volume is 0777, and
        // that is not why it is refused.
        assert_eq!(refused_below.why, Untrusted::IgnoresOwnership);
        assert_eq!(
            refused_below.directory,
            fs::canonicalize(&below).expect("a canonical path")
        );
        assert_eq!(refused_at.why, Untrusted::IgnoresOwnership);
        assert_eq!(refused_at.directory, on_the_volume);
        // What is above the volume is on another file system and is judged as it always was.
        assert_eq!(beside, Ok(()));
        let text = refused_below.to_string();
        assert!(
            text.contains("ignores ownership") && text.contains("no mode keeps another user out"),
            "{text}"
        );
        let way_out = refused_below.way_out();
        assert!(
            way_out.contains("EXCISE_E2E_TMPDIR") && way_out.contains("diskutil enableOwnership"),
            "{way_out}"
        );
        assert!(
            !way_out.starts_with("make it private"),
            "a mode does not help there: {way_out}"
        );
    }

    #[test]
    fn a_file_system_that_cannot_be_asked_about_ownership_is_refused_and_not_taken_for_private() {
        let base = private();
        let directory = directory_in(base.path(), "directory", 0o700);
        let me = rustix::process::geteuid().as_raw();
        let cannot = |_: &Path| Err(io::Error::other("statfs failed"));

        let refusal = check_with(&directory, me, &cannot).expect_err("the question failed");

        assert!(
            matches!(&refusal.why, Untrusted::Unreadable(why) if why.contains("statfs failed")),
            "{refusal}"
        );
    }

    #[test]
    fn every_reason_has_a_way_out_that_names_the_setting_and_only_a_mode_gets_the_advice_of_a_mode()
    {
        let directory = PathBuf::from("/d");
        for why in [
            Untrusted::Unreadable("denied".to_owned()),
            Untrusted::OwnedBy(65534),
            Untrusted::GroupWritable,
            Untrusted::WorldWritable,
            Untrusted::IgnoresOwnership,
        ] {
            let refusal = UntrustedDirectory {
                directory: directory.clone(),
                why: why.clone(),
            };

            let way_out = refusal.way_out();

            assert!(way_out.contains("EXCISE_E2E_TMPDIR"), "{why:?}: {way_out}");
            assert_eq!(
                way_out.starts_with("make it private"),
                matches!(why, Untrusted::GroupWritable | Untrusted::WorldWritable),
                "{why:?}: {way_out}"
            );
            assert!(refusal.to_string().contains("`/d`"), "{why:?}");
        }
    }

    #[test]
    fn a_real_directory_is_asked_the_question_of_its_file_system_and_passes_it() {
        // Where the platform can say whether a file system enforces ownership (macOS), the check
        // asks it of every directory it looks at, for real. A directory made for the test is on
        // the system's own volume, which does.
        let base = private();
        let inside = directory_in(base.path(), "inside", 0o700);

        assert_eq!(check_private_directory(&inside), Ok(()));
    }
}
