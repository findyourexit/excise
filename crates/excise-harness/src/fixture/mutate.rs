//! Live mutators: the operations behind a scenario's `fs_mutate` step.
//!
//! A runner calls [`apply`] at a step boundary to change the fixture while `excise` is running:
//! `appear`, `change`, `vanish`, or `replace` the entry at a fixture-relative path.
//!
//! # Safety rules
//!
//! * **Owned roots only.** The root must carry the ownership marker ([`verify_owned`]); a root
//!   without one is refused before anything else happens.
//! * **Scenario paths only.** The path must satisfy the same rule `Scenario::validate` applies to
//!   every path in a scenario ([`check_fixture_relative_path`]): relative, `/`-separated, no `.`
//!   or `..`, nothing that Windows would read as a drive, a stream, or a device.
//! * **No symbolic links.** The path is resolved one component at a time relative to an open
//!   directory, and every component is opened without following links. A link anywhere on the
//!   path is an error, and the last component is never followed either: `vanish` removes a link,
//!   it never removes what the link points to.
//! * **The marker is untouchable.** No operation may name the marker or a path inside it
//!   ([`is_marker_path`]).
//!
//! # What each operation does
//!
//! | Operation | Effect |
//! |---|---|
//! | `appear` | Creates a new regular file of [`APPEAR_BYTES`] bytes, and any missing directories above it. Fails if the path exists, as a link too. |
//! | `change` | Appends [`CHANGE_BYTES`] bytes to an existing regular file: same inode, larger size. |
//! | `vanish` | Removes an existing entry. A directory goes with everything below it; its unreadable parts are made removable first. |
//! | `replace` | Gives an existing entry a new identity under the same name. A regular file is replaced atomically, through a rename, by a new file of [`REPLACE_BYTES`] bytes. A directory is swapped for a new empty directory: the old one is renamed aside, the new one takes the name, and the old tree is removed, so the name is briefly absent. |
//!
//! Content is a function of the path, not of the clock, so a repeated run makes the same bytes.

use std::{
    io,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

use thiserror::Error;

use crate::{
    fixture::{
        NodeKind,
        marker::{OwnershipError, is_marker_path, verify_owned},
        path::RelPath,
        rng::{SplitMix64, derive_seed},
        sys::Dir,
        tree::remove_entry,
    },
    scenario::{MutateOp, PathViolation},
};

/// The size of a file made by `appear`.
pub const APPEAR_BYTES: u64 = 4096;

/// The number of bytes `change` appends.
pub const CHANGE_BYTES: u64 = 4096;

/// The size of the file `replace` puts in place of a regular file.
pub const REPLACE_BYTES: u64 = 1024;

/// What a mutation did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mutation {
    /// The operation.
    pub op: MutateOp,
    /// The path it acted on, as the scenario wrote it.
    pub path: String,
    /// What was at the path before, if anything.
    pub before: Option<NodeKind>,
    /// What is at the path now, if anything.
    pub after: Option<NodeKind>,
    /// The size of the entry now, when it is a regular file.
    pub size_after: Option<u64>,
}

/// Why a mutation was refused or failed.
#[derive(Debug, Error)]
pub enum MutateError {
    /// The root is not a harness fixture.
    #[error(transparent)]
    Unowned(#[from] OwnershipError),
    /// The path breaks the fixture-relative path rule.
    #[error("`{path}` is not a fixture-relative path: {violation}")]
    InvalidPath {
        /// The path as given.
        path: String,
        /// The rule it breaks.
        violation: PathViolation,
    },
    /// The path is the ownership marker, or lies inside it ([`is_marker_path`]).
    #[error("`{path}` is the ownership marker, which no mutation may touch")]
    Protected {
        /// The path as given.
        path: String,
    },
    /// Nothing exists at the path, or at a directory above it.
    #[error("`{path}` does not exist")]
    NotFound {
        /// The path as given.
        path: String,
    },
    /// `appear` found something at the path.
    #[error("`{path}` already exists")]
    AlreadyExists {
        /// The path as given.
        path: String,
    },
    /// A component of the path is a symbolic link.
    #[error("`{component}` in `{path}` is a symbolic link, and mutations never follow links")]
    SymlinkTraversal {
        /// The path as given.
        path: String,
        /// The component that is a link.
        component: String,
    },
    /// A component above the last is not a directory.
    #[error("`{component}` in `{path}` is not a directory")]
    NotADirectory {
        /// The path as given.
        path: String,
        /// The component that is not a directory.
        component: String,
    },
    /// The operation does not apply to what is at the path.
    #[error("cannot {op} `{path}`, which is a {found}")]
    WrongKind {
        /// The operation.
        op: MutateOp,
        /// The path as given.
        path: String,
        /// What is there.
        found: NodeKind,
    },
    /// A file system call failed.
    #[error("cannot {operation} `{path}`: {source}")]
    Io {
        /// What was being done.
        operation: &'static str,
        /// The path as given.
        path: String,
        /// The underlying error.
        source: io::Error,
    },
}

/// Applies `op` to `path` below `root`.
///
/// # Errors
///
/// Returns why the mutation was refused, or the file system error that stopped it. See the module
/// documentation for the rules.
pub fn apply(root: &Path, op: MutateOp, path: &str) -> Result<Mutation, MutateError> {
    verify_owned(root)?;
    RelPath::from_scenario_path(path).map_err(|violation| MutateError::InvalidPath {
        path: path.to_owned(),
        violation,
    })?;
    if is_marker_path(path) {
        return Err(MutateError::Protected {
            path: path.to_owned(),
        });
    }
    let components: Vec<&str> = path.split('/').collect();
    let (name, parents) = components
        .split_last()
        .ok_or_else(|| MutateError::NotFound {
            path: path.to_owned(),
        })?;

    let io_error = |operation| {
        move |source| MutateError::Io {
            operation,
            path: path.to_owned(),
            source,
        }
    };
    let root_dir = Dir::open_root(root).map_err(io_error("open the fixture root"))?;
    let parent = open_parent(&root_dir, parents, op == MutateOp::Appear, path)?;
    let name_bytes = name.as_bytes();
    let before = match parent.stat(name_bytes) {
        Ok(stat) => Some(stat),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(io_error("inspect")(error)),
    };

    let missing = || MutateError::NotFound {
        path: path.to_owned(),
    };
    let wrong = |found| MutateError::WrongKind {
        op,
        path: path.to_owned(),
        found,
    };
    let size_after = match op {
        MutateOp::Appear => {
            if before.is_some() {
                return Err(MutateError::AlreadyExists {
                    path: path.to_owned(),
                });
            }
            let mut file = parent.create_file(name_bytes).map_err(io_error("create"))?;
            write_bytes(&mut file, path, b"appear", APPEAR_BYTES).map_err(io_error("write"))?;
            Some(APPEAR_BYTES)
        }
        MutateOp::Change => {
            let stat = before.ok_or_else(missing)?;
            if stat.kind != NodeKind::File {
                return Err(wrong(stat.kind));
            }
            let mut file = parent
                .open_regular_for_append(name_bytes)
                .map_err(io_error("open"))?;
            write_bytes(&mut file, path, b"change", CHANGE_BYTES).map_err(io_error("write"))?;
            Some(stat.size + CHANGE_BYTES)
        }
        MutateOp::Vanish => {
            let stat = before.ok_or_else(missing)?;
            remove_entry(&parent, name_bytes, stat.dev).map_err(io_error("remove"))?;
            None
        }
        MutateOp::Replace => {
            let stat = before.ok_or_else(missing)?;
            match stat.kind {
                NodeKind::File => {
                    replace_file(&parent, name_bytes, path).map_err(io_error("replace"))?;
                    Some(REPLACE_BYTES)
                }
                NodeKind::Directory => {
                    replace_directory(&parent, name_bytes, stat.dev)
                        .map_err(io_error("replace"))?;
                    None
                }
                other => return Err(wrong(other)),
            }
        }
    };

    let after = match op {
        MutateOp::Vanish => None,
        MutateOp::Appear | MutateOp::Change => Some(NodeKind::File),
        MutateOp::Replace => before.map(|stat| stat.kind),
    };
    Ok(Mutation {
        op,
        path: path.to_owned(),
        before: before.map(|stat| stat.kind),
        after,
        size_after,
    })
}

/// Resolves the directories above the last component, without following a link. With `create`,
/// missing directories are created.
fn open_parent(root: &Dir, parents: &[&str], create: bool, path: &str) -> Result<Dir, MutateError> {
    let io_error = |source| MutateError::Io {
        operation: "resolve",
        path: path.to_owned(),
        source,
    };
    let mut current = root.reopen().map_err(io_error)?;
    for component in parents {
        let name = component.as_bytes();
        let next = match current.open_dir(name) {
            Ok(directory) => directory,
            Err(error) => match current.stat(name) {
                Ok(stat) if stat.kind == NodeKind::Symlink => {
                    return Err(MutateError::SymlinkTraversal {
                        path: path.to_owned(),
                        component: (*component).to_owned(),
                    });
                }
                Ok(stat) if stat.kind != NodeKind::Directory => {
                    return Err(MutateError::NotADirectory {
                        path: path.to_owned(),
                        component: (*component).to_owned(),
                    });
                }
                Err(missing) if missing.kind() == io::ErrorKind::NotFound => {
                    if !create {
                        return Err(MutateError::NotFound {
                            path: path.to_owned(),
                        });
                    }
                    current.create_dir(name).map_err(io_error)?;
                    current.open_dir(name).map_err(io_error)?
                }
                Ok(_) | Err(_) => return Err(io_error(error)),
            },
        };
        current = next;
    }
    Ok(current)
}

/// Writes `length` bytes that are a function of `path` and `salt`.
fn write_bytes(file: &mut impl io::Write, path: &str, salt: &[u8], length: u64) -> io::Result<()> {
    let mut stream = SplitMix64::new(derive_seed(derive_seed(0, salt), path.as_bytes()));
    let mut buffer = vec![0_u8; usize::try_from(length).unwrap_or(0)];
    stream.fill(&mut buffer);
    file.write_all(&buffer)
}

/// A name for a temporary sibling that no scenario path can collide with: scenario paths cannot
/// end in `.` and this one starts with the marker's prefix.
fn temporary_name(kind: &str) -> Vec<u8> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        ".excise-harness-{kind}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
    .into_bytes()
}

/// Replaces a regular file by a new file under the same name, atomically.
fn replace_file(parent: &Dir, name: &[u8], path: &str) -> io::Result<()> {
    let temporary = temporary_name("replace");
    let mut file = parent.create_file(&temporary)?;
    let written = write_bytes(&mut file, path, b"replace", REPLACE_BYTES);
    drop(file);
    let renamed = written.and_then(|()| parent.rename(&temporary, parent, name));
    if renamed.is_err() {
        // Best effort: do not leave the temporary file behind.
        let _ = parent.unlink(&temporary);
    }
    renamed
}

/// Replaces a directory by a new empty one under the same name: the old one is renamed aside, the
/// new one takes the name, and the old tree is removed.
fn replace_directory(parent: &Dir, name: &[u8], device: Option<u64>) -> io::Result<()> {
    let aside = temporary_name("replaced");
    parent.rename(name, parent, &aside)?;
    if let Err(error) = parent.create_dir(name) {
        // Put the original back rather than leave the name empty.
        let _ = parent.rename(&aside, parent, name);
        return Err(error);
    }
    remove_entry(parent, &aside, device)
}
