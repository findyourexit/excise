//! A tree nested past `PATH_MAX`, created, changed, and removed only through directory handles
//! (`mkdirat`, `openat`, `unlinkat`), the way the harness builds its deep fixture: no path in it
//! can be named whole, so a test cannot reach it any other way. The scanner's and the deletion
//! planner's tests share it.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::Write as _;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

use rustix::fs::{
    AtFlags, CWD, Dir, FileType, Mode, OFlags, mkdirat, openat, statat, symlinkat, unlinkat,
};

/// Folders in the chain. A folder name is `NAME_LEN` bytes plus a separator, so the deepest
/// path passes 4,096 bytes, `PATH_MAX` on Linux (1,024 on macOS), whatever the root's name.
pub(crate) const LEVELS: usize = 44;
const NAME_LEN: usize = 100;
const FILE_NAME: &str = "leaf.dat";
const LINK_NAME: &str = "loop";
const LINK_TARGET: &str = "..";
const FOLDER_MODE: Mode = Mode::from_raw_mode(0o755);
const FILE_MODE: Mode = Mode::from_raw_mode(0o644);
const OPEN_FOLDER: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
const CREATE_FILE: OFlags = OFlags::WRONLY
    .union(OFlags::CREATE)
    .union(OFlags::EXCL)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

fn folder_name(level: usize) -> OsString {
    let mut name = format!("{level:03}-");
    while name.len() < NAME_LEN {
        name.push('x');
    }
    OsString::from(name)
}

/// The size of the file written in the folder at `level`: distinct at every level.
fn file_size(level: usize) -> u64 {
    u64::try_from(level * 7 + 1).expect("a small size fits a u64")
}

fn write_file(folder: &OwnedFd, name: impl rustix::path::Arg, size: u64) {
    let handle = openat(folder, name, CREATE_FILE, FILE_MODE).expect("the file should be created");
    File::from(handle)
        .write_all(&vec![
            b'x';
            usize::try_from(size).expect("a small size fits")
        ])
        .expect("the file should be written");
}

/// Lists everything below `folder` into `found`, as paths below `prefix`.
fn list_below(folder: &OwnedFd, prefix: &Path, found: &mut Vec<PathBuf>) {
    let mut listing = Dir::read_from(folder).expect("a folder of the tree should list");
    let mut names = Vec::new();
    while let Some(entry) = listing.read() {
        let name = entry
            .expect("an entry of the tree should list")
            .file_name()
            .to_owned();
        if name.as_bytes() != b"." && name.as_bytes() != b".." {
            names.push(name);
        }
    }
    for name in names {
        let path = prefix.join(OsStr::from_bytes(name.as_bytes()));
        found.push(path.clone());
        let is_folder = statat(folder, &name, AtFlags::SYMLINK_NOFOLLOW)
            .is_ok_and(|stat| FileType::from_raw_mode(stat.st_mode) == FileType::Directory);
        if is_folder {
            let child = openat(folder, &name, OPEN_FOLDER, Mode::empty())
                .expect("a folder of the tree should open");
            list_below(&child, &path, found);
        }
    }
}

/// Removes everything below `folder` through its handle, so a tree of any depth goes.
fn remove_below(folder: &OwnedFd) {
    let Ok(mut listing) = Dir::read_from(folder) else {
        return;
    };
    let mut names = Vec::new();
    while let Some(Ok(entry)) = listing.read() {
        let name = entry.file_name().to_owned();
        if name.as_bytes() != b"." && name.as_bytes() != b".." {
            names.push(name);
        }
    }
    for name in names {
        let is_folder = statat(folder, &name, AtFlags::SYMLINK_NOFOLLOW)
            .is_ok_and(|stat| FileType::from_raw_mode(stat.st_mode) == FileType::Directory);
        if is_folder {
            if let Ok(child) = openat(folder, &name, OPEN_FOLDER, Mode::empty()) {
                remove_below(&child);
            }
            let _ = unlinkat(folder, &name, AtFlags::REMOVEDIR);
        } else {
            let _ = unlinkat(folder, &name, AtFlags::empty());
        }
    }
}

pub(crate) struct DeepTree {
    /// The root as a scan sees it, resolved: no link above it can pass for a replaced
    /// folder. (The temporary directory itself may sit behind one, as macOS's does.)
    root: PathBuf,
    names: Vec<OsString>,
    _temporary: tempfile::TempDir,
}

impl DeepTree {
    /// One chain of `LEVELS` folders, each holding a file of its own size, and a symbolic
    /// link in the deepest, aimed back up the chain, that a scan must not follow.
    pub(crate) fn build() -> Self {
        let temporary = tempfile::tempdir().expect("scan root should exist");
        // Dropped on a panic part-way, the tree removes what was made so far.
        let mut tree = Self {
            root: std::fs::canonicalize(temporary.path()).expect("the scan root should resolve"),
            names: Vec::new(),
            _temporary: temporary,
        };
        let mut folder =
            openat(CWD, &tree.root, OPEN_FOLDER, Mode::empty()).expect("the root should open");
        for level in 1..=LEVELS {
            let name = folder_name(level);
            mkdirat(&folder, name.as_os_str(), FOLDER_MODE).expect("the folder should be created");
            folder = openat(&folder, name.as_os_str(), OPEN_FOLDER, Mode::empty())
                .expect("the new folder should open");
            write_file(&folder, FILE_NAME, file_size(level));
            tree.names.push(name);
        }
        symlinkat(LINK_TARGET, &folder, LINK_NAME).expect("the link should be created");
        tree
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// The folder at `level` of the chain (1 is its top), relative to the root.
    pub(crate) fn relative_folder(&self, level: usize) -> PathBuf {
        self.names.iter().take(level).collect()
    }

    /// Every folder of the chain, relative to the root.
    pub(crate) fn folders(&self) -> Vec<PathBuf> {
        (1..=LEVELS)
            .map(|level| self.relative_folder(level))
            .collect()
    }

    /// Every file, relative to the root, with the size it was written with.
    pub(crate) fn files(&self) -> Vec<(PathBuf, u64)> {
        (1..=LEVELS)
            .map(|level| {
                (
                    self.relative_folder(level).join(FILE_NAME),
                    file_size(level),
                )
            })
            .collect()
    }

    /// The file in the folder at `level`, relative to the root.
    pub(crate) fn relative_file(&self, level: usize) -> PathBuf {
        self.relative_folder(level).join(FILE_NAME)
    }

    /// The symbolic link in the deepest folder, relative to the root.
    pub(crate) fn link(&self) -> PathBuf {
        self.relative_folder(LEVELS).join(LINK_NAME)
    }

    /// The apparent size of everything in the tree: every file, and the link, whose size
    /// is the length of its target.
    pub(crate) fn apparent_bytes(&self) -> u64 {
        let link = u64::try_from(LINK_TARGET.len()).expect("a short target fits a u64");
        self.files().iter().map(|(_, size)| size).sum::<u64>() + link
    }

    /// The full path of the deepest folder: longer than any system call accepts.
    pub(crate) fn deepest(&self) -> PathBuf {
        self.root().join(self.relative_folder(LEVELS))
    }

    /// Opens the folder at `level` (0 is the root) through the handles of the folders
    /// above it.
    pub(crate) fn open_folder(&self, level: usize) -> File {
        let mut folder =
            openat(CWD, self.root(), OPEN_FOLDER, Mode::empty()).expect("the root should open");
        for name in self.names.iter().take(level) {
            folder = openat(&folder, name.as_os_str(), OPEN_FOLDER, Mode::empty())
                .expect("a folder of the chain should open");
        }
        File::from(folder)
    }

    /// Removes the deepest folder, freeing its name for something else. Removed rather
    /// than renamed aside: on macOS 14, renaming a folder whose path is longer than
    /// `PATH_MAX` fails with "No space left on device", while removing one works. What
    /// takes the name is reported as a replacement for its kind (a link, or not a
    /// folder), not for its identity, so the inode it gets does not matter.
    fn displace_deepest(&self) -> OwnedFd {
        self.remove_deepest();
        OwnedFd::from(self.open_folder(LEVELS - 1))
    }

    /// Puts a symbolic link under the deepest folder's name, as a swap by an attacker would.
    pub(crate) fn replace_deepest_with_link(&self) {
        let parent = self.displace_deepest();
        symlinkat("..", &parent, self.names[LEVELS - 1].as_os_str())
            .expect("the replacement link should be created");
    }

    /// Puts a regular file under the deepest folder's name.
    pub(crate) fn replace_deepest_with_file(&self) {
        let parent = self.displace_deepest();
        write_file(&parent, self.names[LEVELS - 1].as_os_str(), 1);
    }

    /// Removes the deepest folder and everything in it.
    pub(crate) fn remove_deepest(&self) {
        let parent = OwnedFd::from(self.open_folder(LEVELS - 1));
        let name = self.names[LEVELS - 1].as_os_str();
        if let Ok(folder) = openat(&parent, name, OPEN_FOLDER, Mode::empty()) {
            remove_below(&folder);
        }
        unlinkat(&parent, name, AtFlags::REMOVEDIR).expect("the emptied deepest folder should go");
    }

    /// Opens the file or folder at `relative` (a link would not open) through the handles of
    /// the folders above it.
    pub(crate) fn open_entry(&self, relative: &Path) -> File {
        let names: Vec<&OsStr> = relative.iter().collect();
        let (name, folders) = names.split_last().expect("a relative path names an entry");
        let mut folder =
            openat(CWD, self.root(), OPEN_FOLDER, Mode::empty()).expect("the root should open");
        for name in folders {
            folder = openat(&folder, *name, OPEN_FOLDER, Mode::empty())
                .expect("a folder above the entry should open");
        }
        let entry = OFlags::RDONLY
            .union(OFlags::NOFOLLOW)
            .union(OFlags::CLOEXEC);
        File::from(openat(&folder, *name, entry, Mode::empty()).expect("the entry should open"))
    }

    /// Every entry now in the tree, relative to the root and sorted, found through the folder
    /// handles so that any depth lists.
    pub(crate) fn listing(&self) -> Vec<PathBuf> {
        let root =
            openat(CWD, self.root(), OPEN_FOLDER, Mode::empty()).expect("the root should open");
        let mut found = Vec::new();
        list_below(&root, Path::new(""), &mut found);
        found.sort();
        found
    }
}

impl Drop for DeepTree {
    fn drop(&mut self) {
        if let Ok(root) = openat(CWD, &self.root, OPEN_FOLDER, Mode::empty()) {
            remove_below(&root);
        }
    }
}
