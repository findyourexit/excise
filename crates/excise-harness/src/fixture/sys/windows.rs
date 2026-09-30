//! Windows directory handles: absolute verbatim paths and `std::fs`.
//!
//! This layer covers the portable subset of the fixture classes: directories, regular files, hard
//! links, and paths beyond `MAX_PATH` (a verbatim `\\?\` path lifts the limit to about 32,767
//! UTF-16 units). It reports these operations as unsupported, and the generator turns each into
//! a capability flag instead of a failure:
//!
//! * symbolic links (creating one needs a privilege, and a link to a directory needs a
//!   different call than a link to a file);
//! * clones and sparse files (no safe API in `std`);
//! * permission masks such as mode `000` (Windows uses ACLs);
//! * names that are not valid UTF-8 (Windows names are UTF-16).
//!
//! The device, inode, and link count of an entry, and its allocated size, need
//! `GetFileInformationByHandle` and `GetCompressedFileSize`. Stable `std` exposes neither
//! (`MetadataExt::volume_serial_number` and friends are unstable), and this crate forbids
//! `unsafe`, so [`Stat`] carries `None` for them.
//!
//! Nothing in this module has been executed by the author of this slice; it is type-checked for
//! `x86_64-pc-windows-msvc` only.

use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io,
    path::{Path, PathBuf},
};

use super::{DirEntryInfo, Stat};
use crate::fixture::NodeKind;

/// An open directory: its absolute verbatim path.
#[derive(Debug)]
pub(crate) struct Dir {
    path: PathBuf,
}

fn unsupported(what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        format!("{what} is not supported by the Windows fixture layer"),
    )
}

/// One name as an `OsString`. A Windows name is UTF-16, so the bytes must be valid UTF-8.
fn component(name: &[u8]) -> io::Result<OsString> {
    std::str::from_utf8(name)
        .map(OsString::from)
        .map_err(|_| unsupported("a name that is not valid UTF-8"))
}

fn kind_of(file_type: fs::FileType) -> NodeKind {
    if file_type.is_symlink() {
        NodeKind::Symlink
    } else if file_type.is_dir() {
        NodeKind::Directory
    } else if file_type.is_file() {
        NodeKind::File
    } else {
        NodeKind::Other
    }
}

fn stat_of(metadata: &fs::Metadata) -> Stat {
    Stat {
        kind: kind_of(metadata.file_type()),
        size: metadata.len(),
        allocated: None,
        dev: None,
        ino: None,
        nlink: None,
        mode: None,
        uid: None,
    }
}

impl Dir {
    fn real_directory(path: &Path) -> io::Result<()> {
        let metadata = fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a real directory",
            ));
        }
        Ok(())
    }

    fn child(&self, name: &[u8]) -> io::Result<PathBuf> {
        Ok(self.path.join(component(name)?))
    }

    /// Opens an existing directory that is not a link, as a verbatim path.
    pub(crate) fn open_root(path: &Path) -> io::Result<Self> {
        Self::real_directory(path)?;
        Ok(Self {
            path: fs::canonicalize(path)?,
        })
    }

    /// Creates a new directory, which must not exist, and opens it.
    pub(crate) fn create_root(path: &Path) -> io::Result<Self> {
        fs::create_dir(path)?;
        Self::open_root(path)
    }

    /// Opens the subdirectory `name`, which must not be a link.
    pub(crate) fn open_dir(&self, name: &[u8]) -> io::Result<Self> {
        let path = self.child(name)?;
        Self::real_directory(&path)?;
        Ok(Self { path })
    }

    /// Opens the subdirectory `name`, creating it first if it does not exist.
    pub(crate) fn open_or_create_dir(&self, name: &[u8]) -> io::Result<Self> {
        match self.open_dir(name) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            other => return other,
        }
        match fs::create_dir(self.child(name)?) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        self.open_dir(name)
    }

    /// Creates the subdirectory `name`, which must not exist.
    pub(crate) fn create_dir(&self, name: &[u8]) -> io::Result<()> {
        fs::create_dir(self.child(name)?)
    }

    /// Creates the regular file `name`, which must not exist, and opens it for writing.
    pub(crate) fn create_file(&self, name: &[u8]) -> io::Result<File> {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(self.child(name)?)
    }

    /// Opens the existing regular file `name` for appending.
    pub(crate) fn open_regular_for_append(&self, name: &[u8]) -> io::Result<File> {
        let path = self.child(name)?;
        if kind_of(fs::symlink_metadata(&path)?.file_type()) != NodeKind::File {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a regular file",
            ));
        }
        OpenOptions::new().append(true).open(path)
    }

    /// Unsupported: see the module documentation.
    #[allow(
        clippy::unused_self,
        reason = "the signature matches the Unix layer, where this operation is supported"
    )]
    pub(crate) fn symlink(&self, _name: &[u8], _target: &[u8]) -> io::Result<()> {
        Err(unsupported("creating a symbolic link"))
    }

    /// Reads the text of the symbolic link `name`.
    pub(crate) fn read_link(&self, name: &[u8]) -> io::Result<Vec<u8>> {
        let target = fs::read_link(self.child(name)?)?;
        Ok(target.to_string_lossy().into_owned().into_bytes())
    }

    /// Makes `new_name` in `new_dir` another name for the file `name` in this directory.
    pub(crate) fn hard_link(&self, name: &[u8], new_dir: &Self, new_name: &[u8]) -> io::Result<()> {
        fs::hard_link(self.child(name)?, new_dir.child(new_name)?)
    }

    /// The facts about `name` itself, without following a link.
    pub(crate) fn stat(&self, name: &[u8]) -> io::Result<Stat> {
        Ok(stat_of(&fs::symlink_metadata(self.child(name)?)?))
    }

    /// The facts about the directory itself.
    pub(crate) fn stat_self(&self) -> io::Result<Stat> {
        Ok(stat_of(&fs::symlink_metadata(&self.path)?))
    }

    /// Lists the directory, in whatever order the file system returns. A name that is not valid
    /// Unicode is converted lossily.
    pub(crate) fn list(&self) -> io::Result<Vec<DirEntryInfo>> {
        let mut entries = Vec::new();
        for entry in fs::read_dir(&self.path)? {
            let entry = entry?;
            entries.push(DirEntryInfo {
                name: entry
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
                    .into_bytes(),
                kind: entry.file_type().ok().map(kind_of),
            });
        }
        Ok(entries)
    }

    /// Removes the non-directory `name`.
    pub(crate) fn unlink(&self, name: &[u8]) -> io::Result<()> {
        fs::remove_file(self.child(name)?)
    }

    /// Removes the empty directory `name`.
    pub(crate) fn remove_dir(&self, name: &[u8]) -> io::Result<()> {
        fs::remove_dir(self.child(name)?)
    }

    /// Renames `from` in this directory to `to` in `to_dir`, replacing a file of that name.
    pub(crate) fn rename(&self, from: &[u8], to_dir: &Self, to: &[u8]) -> io::Result<()> {
        fs::rename(self.child(from)?, to_dir.child(to)?)
    }

    /// Unsupported: see the module documentation.
    #[allow(
        clippy::unused_self,
        reason = "the signature matches the Unix layer, where this operation is supported"
    )]
    pub(crate) fn chmod(&self, _name: &[u8], _bits: u32) -> io::Result<()> {
        Err(unsupported("setting a permission mask"))
    }

    /// Whether the regular file `name` can be opened for reading by this process.
    pub(crate) fn can_read(&self, name: &[u8]) -> bool {
        self.child(name)
            .and_then(|path| File::open(path).map(drop))
            .is_ok()
    }

    /// Opens the existing regular file `name` for reading.
    pub(crate) fn open_regular_for_read(&self, name: &[u8]) -> io::Result<File> {
        let path = self.child(name)?;
        if kind_of(fs::symlink_metadata(&path)?.file_type()) != NodeKind::File {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a regular file",
            ));
        }
        File::open(path)
    }

    /// Opens this directory again, as an independent handle.
    #[allow(
        clippy::unnecessary_wraps,
        reason = "the Unix layer reopens by file descriptor, which can fail"
    )]
    pub(crate) fn reopen(&self) -> io::Result<Self> {
        Ok(Self {
            path: self.path.clone(),
        })
    }

    /// Unsupported: see the module documentation.
    #[allow(
        clippy::unused_self,
        reason = "the signature matches the Unix layer, where this operation is supported"
    )]
    pub(crate) fn clone_file(
        &self,
        _name: &[u8],
        _source_dir: &Self,
        _source: &[u8],
    ) -> io::Result<()> {
        Err(unsupported("cloning a file"))
    }
}
