//! Unix directory handles: file descriptors and `*at` system calls through `rustix`.

use std::{fs::File, io, os::fd::OwnedFd, path::Path};

use rustix::{
    fs::{self as rfs, AtFlags, FileType, Mode, OFlags, RawMode},
    io::Errno,
};

use super::{DirEntryInfo, Stat};
use crate::fixture::NodeKind;

/// Flags for opening a directory without following a link in its last component.
const OPEN_DIR: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// Flags for opening a file only to read or inspect it. `NONBLOCK` keeps the open of a FIFO from
/// waiting for a writer.
const OPEN_READ: OFlags = OFlags::RDONLY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK)
    .union(OFlags::CLOEXEC);

/// Flags for creating a file that must not exist.
const CREATE_NEW: OFlags = OFlags::WRONLY
    .union(OFlags::CREATE)
    .union(OFlags::EXCL)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

const DIR_MODE: u32 = 0o755;
const FILE_MODE: u32 = 0o644;

/// An open directory.
#[derive(Debug)]
pub(crate) struct Dir {
    fd: OwnedFd,
}

/// Opens the regular file at `path` for reading, as a person typed the path: a link among its
/// directories is followed, but its last component must not be a link, and what is opened must be
/// a regular file. A FIFO or a device is refused without being waited on (`NONBLOCK`: a FIFO opened
/// for reading with no writer would otherwise wait for one), and a link, a folder, or anything else
/// says what it is.
pub(crate) fn open_regular_file(path: &Path) -> io::Result<File> {
    let fd = match rfs::openat(rfs::CWD, path, OPEN_READ, Mode::empty()) {
        Ok(fd) => fd,
        // `NOFOLLOW` on a link, whether it leads somewhere or not.
        Err(Errno::LOOP) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a symbolic link, which is not followed",
            ));
        }
        Err(error) => return Err(error.into()),
    };
    if stat_from_raw(&rfs::fstat(&fd)?).kind != NodeKind::File {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file (a folder, a FIFO, or a device)",
        ));
    }
    Ok(File::from(fd))
}

impl Dir {
    /// Opens an existing directory. The last component of `path` must not be a symbolic link;
    /// earlier components are resolved normally.
    pub(crate) fn open_root(path: &Path) -> io::Result<Self> {
        Ok(Self {
            fd: rfs::openat(rfs::CWD, path, OPEN_DIR, Mode::empty())?,
        })
    }

    /// Creates a new directory, which must not exist, and opens it.
    pub(crate) fn create_root(path: &Path) -> io::Result<Self> {
        rfs::mkdirat(rfs::CWD, path, mode(DIR_MODE)?)?;
        Self::open_root(path)
    }

    /// Opens the subdirectory `name` without following a link.
    pub(crate) fn open_dir(&self, name: &[u8]) -> io::Result<Self> {
        Ok(Self {
            fd: rfs::openat(&self.fd, name, OPEN_DIR, Mode::empty())?,
        })
    }

    /// Opens the subdirectory `name`, creating it first if it does not exist. Safe to race with
    /// another thread creating the same directory.
    pub(crate) fn open_or_create_dir(&self, name: &[u8]) -> io::Result<Self> {
        match self.open_dir(name) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            other => return other,
        }
        match rfs::mkdirat(&self.fd, name, mode(DIR_MODE)?) {
            Ok(()) | Err(Errno::EXIST) => {}
            Err(error) => return Err(error.into()),
        }
        self.open_dir(name)
    }

    /// Creates the subdirectory `name`, which must not exist.
    pub(crate) fn create_dir(&self, name: &[u8]) -> io::Result<()> {
        Ok(rfs::mkdirat(&self.fd, name, mode(DIR_MODE)?)?)
    }

    /// Creates the regular file `name`, which must not exist, and opens it for writing.
    pub(crate) fn create_file(&self, name: &[u8]) -> io::Result<File> {
        let fd = rfs::openat(&self.fd, name, CREATE_NEW, mode(FILE_MODE)?)?;
        Ok(File::from(fd))
    }

    /// Opens the existing regular file `name` for appending. Refuses anything that is not a
    /// regular file (a link, a FIFO, a device) without waiting on it.
    pub(crate) fn open_regular_for_append(&self, name: &[u8]) -> io::Result<File> {
        let fd = rfs::openat(
            &self.fd,
            name,
            OFlags::WRONLY
                .union(OFlags::APPEND)
                .union(OFlags::NOFOLLOW)
                .union(OFlags::NONBLOCK)
                .union(OFlags::CLOEXEC),
            Mode::empty(),
        )?;
        if stat_from_raw(&rfs::fstat(&fd)?).kind != NodeKind::File {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a regular file",
            ));
        }
        Ok(File::from(fd))
    }

    /// Creates the symbolic link `name` holding `target`.
    pub(crate) fn symlink(&self, name: &[u8], target: &[u8]) -> io::Result<()> {
        Ok(rfs::symlinkat(target, &self.fd, name)?)
    }

    /// Reads the text of the symbolic link `name`.
    pub(crate) fn read_link(&self, name: &[u8]) -> io::Result<Vec<u8>> {
        Ok(rfs::readlinkat(&self.fd, name, Vec::new())?.into_bytes())
    }

    /// Makes `new_name` in `new_dir` another name for the file `name` in this directory.
    pub(crate) fn hard_link(&self, name: &[u8], new_dir: &Self, new_name: &[u8]) -> io::Result<()> {
        Ok(rfs::linkat(
            &self.fd,
            name,
            &new_dir.fd,
            new_name,
            AtFlags::empty(),
        )?)
    }

    /// `lstat`: the facts about `name` itself, without following a link.
    pub(crate) fn stat(&self, name: &[u8]) -> io::Result<Stat> {
        Ok(stat_from_raw(&rfs::statat(
            &self.fd,
            name,
            AtFlags::SYMLINK_NOFOLLOW,
        )?))
    }

    /// `fstat` on the directory itself.
    pub(crate) fn stat_self(&self) -> io::Result<Stat> {
        Ok(stat_from_raw(&rfs::fstat(&self.fd)?))
    }

    /// Lists the directory, without `.` and `..`, in whatever order the file system returns.
    pub(crate) fn list(&self) -> io::Result<Vec<DirEntryInfo>> {
        let mut reader = rfs::Dir::read_from(&self.fd)?;
        let mut entries = Vec::new();
        while let Some(entry) = reader.read() {
            let entry = entry?;
            let name = entry.file_name().to_bytes();
            if name == b"." || name == b".." {
                continue;
            }
            entries.push(DirEntryInfo {
                name: name.to_vec(),
                kind: kind_from_file_type(entry.file_type()),
            });
        }
        Ok(entries)
    }

    /// Removes the non-directory `name`.
    pub(crate) fn unlink(&self, name: &[u8]) -> io::Result<()> {
        Ok(rfs::unlinkat(&self.fd, name, AtFlags::empty())?)
    }

    /// Removes the empty directory `name`.
    pub(crate) fn remove_dir(&self, name: &[u8]) -> io::Result<()> {
        Ok(rfs::unlinkat(&self.fd, name, AtFlags::REMOVEDIR)?)
    }

    /// Renames `from` in this directory to `to` in `to_dir`, replacing a file of that name.
    pub(crate) fn rename(&self, from: &[u8], to_dir: &Self, to: &[u8]) -> io::Result<()> {
        Ok(rfs::renameat(&self.fd, from, &to_dir.fd, to)?)
    }

    /// Sets the permission bits of `name`, which must not be a symbolic link.
    pub(crate) fn chmod(&self, name: &[u8], bits: u32) -> io::Result<()> {
        if self.stat(name)?.kind == NodeKind::Symlink {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "refusing to change the permissions of a symbolic link",
            ));
        }
        Ok(rfs::chmodat(&self.fd, name, mode(bits)?, AtFlags::empty())?)
    }

    /// Whether the regular file `name` can be opened for reading by this process.
    pub(crate) fn can_read(&self, name: &[u8]) -> bool {
        rfs::openat(&self.fd, name, OPEN_READ, Mode::empty()).is_ok()
    }

    /// Opens the existing regular file `name` for reading. Refuses anything that is not a regular
    /// file (a link, a FIFO, a device) without waiting on it.
    pub(crate) fn open_regular_for_read(&self, name: &[u8]) -> io::Result<File> {
        let fd = rfs::openat(&self.fd, name, OPEN_READ, Mode::empty())?;
        if stat_from_raw(&rfs::fstat(&fd)?).kind != NodeKind::File {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a regular file",
            ));
        }
        Ok(File::from(fd))
    }

    /// Opens this directory again, as an independent handle.
    pub(crate) fn reopen(&self) -> io::Result<Self> {
        self.open_dir(b".")
    }

    /// Makes `name` a copy-on-write clone of `source` in `source_dir`: an APFS clone on macOS,
    /// a reflink on Linux file systems that share extents. Fails, leaving nothing behind, where
    /// the file system cannot.
    #[cfg(target_vendor = "apple")]
    pub(crate) fn clone_file(
        &self,
        name: &[u8],
        source_dir: &Self,
        source: &[u8],
    ) -> io::Result<()> {
        let source = rfs::openat(&source_dir.fd, source, OPEN_READ, Mode::empty())?;
        Ok(rfs::fclonefileat(
            &source,
            &self.fd,
            name,
            rfs::CloneFlags::NOFOLLOW,
        )?)
    }

    /// See the macOS variant.
    #[cfg(all(
        target_os = "linux",
        not(any(target_arch = "sparc", target_arch = "sparc64"))
    ))]
    pub(crate) fn clone_file(
        &self,
        name: &[u8],
        source_dir: &Self,
        source: &[u8],
    ) -> io::Result<()> {
        let source = rfs::openat(&source_dir.fd, source, OPEN_READ, Mode::empty())?;
        let target = rfs::openat(&self.fd, name, CREATE_NEW, mode(FILE_MODE)?)?;
        if let Err(error) = rfs::ioctl_ficlone(&target, &source) {
            drop(target);
            // Best effort: the failed clone left an empty file that must not stay.
            let _ = rfs::unlinkat(&self.fd, name, AtFlags::empty());
            return Err(error.into());
        }
        Ok(())
    }

    /// See the macOS variant.
    #[cfg(not(any(
        target_vendor = "apple",
        all(
            target_os = "linux",
            not(any(target_arch = "sparc", target_arch = "sparc64"))
        )
    )))]
    pub(crate) fn clone_file(
        &self,
        _name: &[u8],
        _source_dir: &Self,
        _source: &[u8],
    ) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "cloning files is not implemented on this platform",
        ))
    }
}

/// A permission mask as the platform's `mode_t`.
fn mode(bits: u32) -> io::Result<Mode> {
    fn convert<T: TryInto<RawMode>>(bits: T) -> Option<RawMode> {
        bits.try_into().ok()
    }
    convert(bits)
        .map(Mode::from_raw_mode)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "mode does not fit `mode_t`"))
}

/// Widens whatever integer type the platform uses for a `stat` field. Negative values (a
/// signed `dev_t`) wrap into the upper range instead of failing.
fn unsigned<T: Into<i128>>(value: T) -> u64 {
    u64::try_from(value.into().rem_euclid(1_i128 << 64)).unwrap_or(0)
}

fn stat_from_raw(raw: &rfs::Stat) -> Stat {
    let kind = match FileType::from_raw_mode(raw.st_mode) {
        FileType::RegularFile => NodeKind::File,
        FileType::Directory => NodeKind::Directory,
        FileType::Symlink => NodeKind::Symlink,
        _ => NodeKind::Other,
    };
    Stat {
        kind,
        size: unsigned(raw.st_size),
        allocated: Some(unsigned(raw.st_blocks).saturating_mul(512)),
        dev: Some(unsigned(raw.st_dev)),
        ino: Some(unsigned(raw.st_ino)),
        nlink: Some(unsigned(raw.st_nlink)),
        mode: Some(u32::try_from(unsigned(raw.st_mode) & 0o7777).unwrap_or(0)),
        uid: Some(u32::try_from(unsigned(raw.st_uid)).unwrap_or(u32::MAX)),
    }
}

const fn kind_from_file_type(file_type: FileType) -> Option<NodeKind> {
    match file_type {
        FileType::RegularFile => Some(NodeKind::File),
        FileType::Directory => Some(NodeKind::Directory),
        FileType::Symlink => Some(NodeKind::Symlink),
        FileType::Unknown => None,
        _ => Some(NodeKind::Other),
    }
}
