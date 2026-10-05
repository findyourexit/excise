//! The platform layer: directory-relative file system operations.
//!
//! Every operation takes a directory handle and one name, never a path. On Unix a [`Dir`] holds a
//! file descriptor and each call is an `*at` system call through `rustix`, so a fixture can be
//! deeper than `PATH_MAX`: the kernel resolves one component at a time and no call ever has to
//! carry the whole path. Opening a component uses `O_NOFOLLOW`, so a symbolic link inside a
//! fixture can never redirect an operation. On Windows a [`Dir`] holds an absolute verbatim
//! (`\\?\`) path and each call is an ordinary `std::fs` call; see the `windows` module for what
//! that layer leaves unsupported.
//!
//! Names are raw bytes, the form the manifest and the plans use.

use crate::fixture::NodeKind;

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub(crate) use unix::{Dir, open_regular_file};
#[cfg(windows)]
pub(crate) use windows::{Dir, open_regular_file};

/// What `lstat` reports about one entry. A field is `None` where the platform layer cannot
/// provide it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Stat {
    pub kind: NodeKind,
    /// `st_size`: the apparent size in bytes. For a symbolic link, the length of its target.
    pub size: u64,
    /// `st_blocks * 512`: the bytes allocated to the entry.
    pub allocated: Option<u64>,
    /// `st_dev`: the device the entry lives on.
    pub dev: Option<u64>,
    /// `st_ino`: the inode number.
    pub ino: Option<u64>,
    /// `st_nlink`: the number of names the inode has.
    pub nlink: Option<u64>,
    /// The permission bits, `st_mode & 0o7777`.
    pub mode: Option<u32>,
    /// `st_uid`: the owner. A file this process just created is owned by its effective user.
    pub uid: Option<u32>,
}

/// One name in a directory listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DirEntryInfo {
    pub name: Vec<u8>,
    /// The kind the directory itself reports, when it does. Not every file system fills it in.
    pub kind: Option<NodeKind>,
}
