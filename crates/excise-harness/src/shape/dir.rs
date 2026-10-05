//! The folder handle of the walk, one for each platform, and the measure of a name.
//!
//! On Unix a handle is the fixture layer's [`Dir`](crate::fixture::sys::Dir): a file descriptor
//! with every call relative to it and no path anywhere, so a tree can be deeper than `PATH_MAX`,
//! and a name is its raw bytes. On Windows a folder can be listed and its entries reached only
//! by a path, so a handle is the verbatim path of the folder together with a handle held open on
//! it, opened as a reparse point (a junction or a link is opened itself, and never its target),
//! with the right to list the folder and every sharing mode but delete: while the walk is in a
//! folder, nothing can rename it, delete it, or put a junction in its place. The right to list
//! does more than let the walk read the folder: the system leaves a handle that holds no data
//! access out of its sharing checks, so one opened for attributes alone could be opened beside
//! by anyone who asks for delete, which a rename does, whatever sharing it was opened with. A
//! folder it has not opened yet is not held, and Windows gives stable Rust no file ID to compare,
//! so another ordinary folder put in its place is opened. A name is the native `OsString`, which
//! can hold a
//! UTF-16 surrogate that is not half of a pair; the fixture layer converts names lossily, which
//! would make such an entry unreachable, and the generator and the oracle, whose names are always
//! valid, keep using it unchanged.

use std::path::{Component, Path, PathBuf};

#[cfg(unix)]
pub(super) use unix::{Name, WalkDir, name_len};
#[cfg(windows)]
pub(super) use windows::{Name, WalkDir, name_len};

/// The length in bytes of a name given as UTF-16 code units, in WTF-8: the length UTF-8 gives a
/// valid name, and 3 bytes for each code unit that is a surrogate with no partner.
#[cfg(any(windows, test))]
fn wtf8_len(units: impl IntoIterator<Item = u16>) -> usize {
    char::decode_utf16(units)
        .map(|unit| unit.map_or(3, char::len_utf8))
        .sum()
}

/// The entry that `path` names, without a separator or a `.` after the name of its last
/// component: `link/`, `link//`, and `link/.` are `link`.
///
/// A name that has a separator after it is the name of a folder, and the system resolves a link at
/// that name before it looks at the flags of the open: POSIX resolves a path that ends in a
/// separator as if a `.` were appended to it, which follows a link before it, whatever
/// `O_NOFOLLOW` says. The walk relies on no platform's treatment of a separator after a name: it
/// takes the separators off before it opens the root, so that what is opened, and checked not to
/// be a link, is the entry the name stands for.
///
/// std drops a `.` that ends a path when it splits the path into components, except for a verbatim
/// path of Windows (`\\?\C:\x\link\.`, or one with a `UNC` share), where it stays a component of
/// its own: `file_name` of such a path is `None`, and the system, given the path as it is,
/// resolves `link` as a folder on the way to the `.`, which follows it. Those components are
/// taken off first, one by one, so that the name before them is the last.
///
/// A path that has no last name to keep, because it ends in `..` or is nothing but a root, a
/// drive, a share, or `.`, is returned as it is, with any `.` it has after a drive or a share
/// (`\\?\C:\.`): a name that cannot be a link needs no help.
fn without_trailing_separators(path: &Path) -> PathBuf {
    let mut components = path.components();
    while components.clone().next_back() == Some(Component::CurDir) {
        components.next_back();
    }
    let named = components.as_path();
    match (named.parent(), named.file_name()) {
        (Some(parent), Some(name)) => parent.join(name),
        _ => path.to_path_buf(),
    }
}

#[cfg(unix)]
mod unix {
    use std::{io, path::Path};

    use crate::fixture::sys::{Dir, Stat};

    /// A name, as the bytes the file system holds.
    pub(in crate::shape) type Name = Vec<u8>;

    /// The length of a name in bytes.
    pub(in crate::shape) fn name_len(name: &Name) -> usize {
        name.len()
    }

    /// An open folder.
    #[derive(Debug)]
    pub(in crate::shape) struct WalkDir(Dir);

    impl WalkDir {
        /// Opens the root. The path is resolved as it is written, apart from any separator after
        /// its last component, which is taken off (see [`without_trailing_separators`]); that
        /// last component must not be a link.
        pub(in crate::shape) fn open_root(path: &Path) -> io::Result<Self> {
            Dir::open_root(&super::without_trailing_separators(path)).map(Self)
        }

        /// Opens the subfolder `name` without following a link.
        pub(in crate::shape) fn open_child(&self, name: &Name) -> io::Result<Self> {
            self.0.open_dir(name).map(Self)
        }

        /// The device and inode of the folder that was opened, from its handle.
        pub(in crate::shape) fn stat_self(&self) -> io::Result<Stat> {
            self.0.stat_self()
        }

        /// `lstat` of the entry `name`.
        pub(in crate::shape) fn stat(&self, name: &Name) -> io::Result<Stat> {
            self.0.stat(name)
        }

        /// The names in the folder.
        pub(in crate::shape) fn list(&self) -> io::Result<Vec<Name>> {
            Ok(self.0.list()?.into_iter().map(|entry| entry.name).collect())
        }
    }
}

#[cfg(windows)]
mod windows {
    use std::{
        ffi::OsString,
        fs::{self, File, OpenOptions},
        io,
        os::windows::{ffi::OsStrExt as _, fs::OpenOptionsExt as _},
        path::{Path, PathBuf},
    };

    use super::{without_trailing_separators, wtf8_len};
    use crate::fixture::{NodeKind, sys::Stat};

    /// `FILE_SHARE_READ`.
    const SHARE_READ: u32 = 0x1;
    /// `FILE_SHARE_WRITE`. `FILE_SHARE_DELETE` is left out on purpose.
    const SHARE_WRITE: u32 = 0x2;
    /// `FILE_LIST_DIRECTORY` (the bit that is `FILE_READ_DATA` for a file): the right to list the
    /// folder, which the walk needs anyway. It is also what makes the handle count when the
    /// system decides who may share the folder: that check leaves out a handle that holds no
    /// read, write, append, execute, or delete access, so with `FILE_READ_ATTRIBUTES` alone
    /// leaving out `FILE_SHARE_DELETE` would stop nobody from opening the folder for delete,
    /// which a rename does.
    const LIST_DIRECTORY: u32 = 0x1;
    /// `FILE_READ_ATTRIBUTES`: to ask what was opened.
    const READ_ATTRIBUTES: u32 = 0x80;
    /// `FILE_FLAG_BACKUP_SEMANTICS`: what opening a folder takes.
    const BACKUP_SEMANTICS: u32 = 0x0200_0000;
    /// `FILE_FLAG_OPEN_REPARSE_POINT`: open a link or a junction itself, not its target.
    const OPEN_REPARSE_POINT: u32 = 0x0020_0000;

    /// A name, as the platform holds it: UTF-16, which may hold a surrogate that is half of no
    /// pair.
    pub(in crate::shape) type Name = OsString;

    /// The length of a name in bytes, as WTF-8: the length of the UTF-8 of a valid name, and 3
    /// bytes for each surrogate that is half of no pair.
    pub(in crate::shape) fn name_len(name: &Name) -> usize {
        wtf8_len(name.encode_wide())
    }

    /// An open folder: its absolute verbatim path, and a handle held on it.
    #[derive(Debug)]
    pub(in crate::shape) struct WalkDir {
        path: PathBuf,
        handle: File,
    }

    impl WalkDir {
        /// Opens the root. The path is resolved as it is written, apart from any separator after
        /// its last component, which is taken off (see [`without_trailing_separators`]); that
        /// last component must not be a link or a junction.
        pub(in crate::shape) fn open_root(path: &Path) -> io::Result<Self> {
            let path = without_trailing_separators(path);
            let handle = open_folder(&path)?;
            Ok(Self {
                path: fs::canonicalize(&path)?,
                handle,
            })
        }

        /// Opens the subfolder `name`, which must not be a link or a junction. Nothing says
        /// whether it is the folder that was inspected: stable Rust has no file ID on Windows and
        /// this crate has no `unsafe` code, so another ordinary folder put in its place is opened
        /// as well.
        pub(in crate::shape) fn open_child(&self, name: &Name) -> io::Result<Self> {
            let path = self.path.join(name);
            let handle = open_folder(&path)?;
            Ok(Self { path, handle })
        }

        /// What the handle that was opened says about its folder.
        pub(in crate::shape) fn stat_self(&self) -> io::Result<Stat> {
            Ok(stat_of(&self.handle.metadata()?))
        }

        /// The facts about the entry `name`, without following a link.
        pub(in crate::shape) fn stat(&self, name: &Name) -> io::Result<Stat> {
            Ok(stat_of(&fs::symlink_metadata(self.path.join(name))?))
        }

        /// The names in the folder, as the platform holds them.
        pub(in crate::shape) fn list(&self) -> io::Result<Vec<Name>> {
            fs::read_dir(&self.path)?
                .map(|entry| entry.map(|entry| entry.file_name()))
                .collect()
        }
    }

    /// Opens the folder at `path` as it is, and holds it: a link or a junction at `path` is
    /// opened itself and refused, and while the handle lives nothing can rename the folder,
    /// delete it, or replace it. It is opened with the right to list it (see
    /// [`LIST_DIRECTORY`]), so a folder that cannot be listed cannot be opened, and the walk
    /// counts it as one it could not read.
    fn open_folder(path: &Path) -> io::Result<File> {
        let handle = OpenOptions::new()
            .access_mode(LIST_DIRECTORY | READ_ATTRIBUTES)
            .share_mode(SHARE_READ | SHARE_WRITE)
            .custom_flags(BACKUP_SEMANTICS | OPEN_REPARSE_POINT)
            .open(path)?;
        let metadata = handle.metadata()?;
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            Ok(handle)
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a folder, or a link to one",
            ))
        }
    }

    fn stat_of(metadata: &fs::Metadata) -> Stat {
        let file_type = metadata.file_type();
        let kind = if file_type.is_symlink() {
            NodeKind::Symlink
        } else if file_type.is_dir() {
            NodeKind::Directory
        } else if file_type.is_file() {
            NodeKind::File
        } else {
            NodeKind::Other
        };
        Stat {
            kind,
            size: metadata.len(),
            allocated: None,
            dev: None,
            ino: None,
            nlink: None,
            mode: None,
            uid: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::OsStr,
        path::{MAIN_SEPARATOR, Path},
    };

    use super::{without_trailing_separators, wtf8_len};

    #[test]
    fn a_name_is_measured_in_wtf8_bytes() {
        // ASCII, a letter of 2 bytes, a letter of 3, and a pair of surrogates for one of 4.
        assert_eq!(
            wtf8_len("a\u{e9}\u{20ac}\u{1f600}".encode_utf16()),
            1 + 2 + 3 + 4
        );
        // A surrogate that is half of no pair is 3 bytes, wherever it stands.
        assert_eq!(wtf8_len([0x61, 0xD800, 0x62]), 1 + 3 + 1);
        assert_eq!(wtf8_len([0xDC00]), 3);
        assert_eq!(wtf8_len([0xD83D]), 3);
        assert_eq!(wtf8_len([0xD83D, 0x61]), 3 + 1);
        assert_eq!(wtf8_len([0xDE00, 0xD83D]), 3 + 3);
        // A pair is one letter of 4 bytes.
        assert_eq!(wtf8_len([0xD83D, 0xDE00]), 4);
        assert_eq!(wtf8_len([]), 0);
    }

    /// What the walk opens for `typed`, as text with every separator written `/`. It is text
    /// because `Path` equality does not tell `link/` from `link`, and the separators are spelled
    /// alike because the folder above the name keeps what was typed and `Path::join` writes the
    /// separator of the platform: `./link/` is `.\link` on Windows.
    fn opened_as_text(typed: &str) -> String {
        without_trailing_separators(Path::new(typed))
            .to_string_lossy()
            .replace(MAIN_SEPARATOR, "/")
    }

    /// The root is opened by its last name, with nothing after it.
    #[test]
    fn the_root_is_opened_by_its_last_name_and_not_by_a_separator_after_it() {
        for (typed, opened) in [
            ("link", "link"),
            ("link/", "link"),
            ("link//", "link"),
            ("link/.", "link"),
            ("./link/", "./link"),
            ("a/b/", "a/b"),
            ("a//b//", "a/b"),
            ("/a/b/", "/a/b"),
            ("a/b/./", "a/b"),
            ("/link/", "/link"),
        ] {
            assert_eq!(opened_as_text(typed), opened, "{typed}");
        }
        // A path with no last name to keep is opened as it is: a root, the folder `.`, a path that
        // ends in `..`, and nothing.
        for typed in ["/", "//", ".", "./", "..", "a/..", "a/../", ""] {
            assert_eq!(
                without_trailing_separators(Path::new(typed)).as_os_str(),
                OsStr::new(typed),
                "{typed}"
            );
        }
    }

    /// The same on Windows, where a junction is named with either separator, and a drive and a
    /// share are roots that keep theirs. A verbatim path (`\\?\C:\...`, or `\\?\UNC\...`) keeps
    /// each `.` as a component of its own, one at its end included, which is what hid the name
    /// from `file_name`.
    #[cfg(windows)]
    #[test]
    fn a_junction_root_loses_its_trailing_backslash_and_a_drive_keeps_its_own() {
        for (typed, opened) in [
            (r"C:\x\junction\", r"C:\x\junction"),
            (r"C:\x\junction\\", r"C:\x\junction"),
            (r"C:\x\junction/", r"C:\x\junction"),
            (r"C:\x\junction\.", r"C:\x\junction"),
            (r"junction\", "junction"),
            (r"\\?\C:\x\junction", r"\\?\C:\x\junction"),
            (r"\\?\C:\x\junction\", r"\\?\C:\x\junction"),
            (r"\\?\C:\x\junction\.", r"\\?\C:\x\junction"),
            (r"\\?\C:\x\junction\.\", r"\\?\C:\x\junction"),
            (r"\\?\C:\x\junction\.\.", r"\\?\C:\x\junction"),
            (r"\\?\C:\x\junction\.\.\", r"\\?\C:\x\junction"),
            (
                r"\\?\UNC\server\share\x\junction\.",
                r"\\?\UNC\server\share\x\junction",
            ),
            (
                r"\\?\UNC\server\share\x\junction\.\",
                r"\\?\UNC\server\share\x\junction",
            ),
        ] {
            assert_eq!(
                without_trailing_separators(Path::new(typed)).as_os_str(),
                OsStr::new(opened),
                "{typed}"
            );
        }
        // A `.` before the name is part of the path as it was written, and stays.
        for typed in [r"\\?\C:\x\.\junction", r"\\?\C:\x\.\junction\.\"] {
            let opened = without_trailing_separators(Path::new(typed));
            assert_eq!(
                opened.as_os_str(),
                OsStr::new(r"\\?\C:\x\.\junction"),
                "{typed}"
            );
        }
        for typed in [
            r"C:\",
            r"\\server\share\",
            ".",
            r"\\?\C:\",
            r"\\?\C:\.",
            r"\\?\UNC\server\share\",
            r"\\?\UNC\server\share\.",
            r"\\?\C:\x\..",
            r"\\?\C:\x\..\.",
        ] {
            assert_eq!(
                without_trailing_separators(Path::new(typed)).as_os_str(),
                OsStr::new(typed),
                "{typed}"
            );
        }
    }
}
