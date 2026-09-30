//! Removing a fixture tree, including the parts a fixture made deliberately hard to remove.
//!
//! The hostile class creates directories and files with mode `000`. `std::fs::remove_dir_all`
//! cannot enter such a directory, so a temporary directory holding one would stay behind for
//! good. [`remove_tree`] restores owner access to each directory before it descends, and works
//! through directory handles, so it also removes trees deeper than `PATH_MAX`.

use std::{
    ffi::OsStr,
    io,
    path::{Path, PathBuf},
};

use crate::fixture::{NodeKind, sys::Dir};

/// Removes the directory or file at `path`, and everything below it. A path that does not exist
/// is already removed and is not an error.
///
/// Symbolic links are removed, never followed. The removal does not cross into another file
/// system: a directory on a different device (a mount point) is an error, so a volume that is
/// still attached is never emptied by accident.
///
/// # Errors
///
/// Returns the first I/O error. What was removed before the error stays removed.
pub fn remove_tree(path: &Path) -> io::Result<()> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing to remove a path with no parent directory",
        ));
    };
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    let parent = match Dir::open_root(parent) {
        Ok(parent) => parent,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let name = os_str_bytes(name);
    let device = match parent.stat(&name) {
        Ok(stat) => stat.dev,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    remove_entry(&parent, &name, device)
}

pub(crate) fn remove_entry(parent: &Dir, name: &[u8], device: Option<u64>) -> io::Result<()> {
    let stat = match parent.stat(name) {
        Ok(stat) => stat,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if stat.kind != NodeKind::Directory {
        return parent.unlink(name);
    }
    if device.is_some() && stat.dev.is_some() && device != stat.dev {
        return Err(io::Error::other(
            "refusing to remove a directory on another file system (a mount point)",
        ));
    }
    if stat.mode.is_some_and(|mode| mode & 0o700 != 0o700) {
        parent.chmod(name, 0o700)?;
    }
    let directory = parent.open_dir(name)?;
    for entry in directory.list()? {
        match entry.kind {
            Some(NodeKind::File | NodeKind::Symlink | NodeKind::Other) => {
                directory.unlink(&entry.name)?;
            }
            Some(NodeKind::Directory) | None => remove_entry(&directory, &entry.name, device)?,
        }
    }
    drop(directory);
    parent.remove_dir(name)
}

/// The bytes of a file name, as [`Dir`] takes them.
pub(crate) fn os_str_bytes(name: &OsStr) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;

        name.as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        name.to_string_lossy().into_owned().into_bytes()
    }
}

/// Owns a directory tree and removes it, restoring permissions first, when dropped.
///
/// The guard also runs while a panic unwinds, so a failed test or run does not leave a tree that
/// the file system will not let anyone delete.
#[derive(Debug)]
pub struct TreeGuard {
    path: Option<PathBuf>,
}

impl TreeGuard {
    /// Takes ownership of the tree at `path`, which may not exist yet.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: Some(path.into()),
        }
    }

    /// The path of the tree.
    #[must_use]
    pub fn path(&self) -> &Path {
        self.path.as_deref().unwrap_or_else(|| Path::new(""))
    }

    /// Gives up ownership: the tree is left in place and its path returned.
    #[must_use]
    pub fn keep(mut self) -> PathBuf {
        self.path.take().unwrap_or_default()
    }

    /// Removes the tree now and reports what went wrong.
    ///
    /// # Errors
    ///
    /// Returns the error of [`remove_tree`].
    pub fn remove(mut self) -> io::Result<()> {
        match self.path.take() {
            Some(path) => remove_tree(&path),
            None => Ok(()),
        }
    }
}

impl Drop for TreeGuard {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            // Nothing useful can be done with a failure while dropping.
            let _ = remove_tree(&path);
        }
    }
}
