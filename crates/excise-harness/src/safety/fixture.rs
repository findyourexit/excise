//! Fixture-relative path resolution, on top of the fixture generator's ownership check.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use thiserror::Error;

use crate::{
    fixture::{OwnershipError, verify_owned},
    scenario::{EntryKind, PathViolation, check_fixture_relative_path},
};

/// Why a directory cannot be used as a fixture root, or why a path cannot be resolved inside one.
#[derive(Debug, Error)]
pub enum SafetyError {
    /// The root is not a harness fixture: it cannot be opened as a directory (a symbolic link is
    /// refused too), or it has no ownership marker that is a regular file.
    #[error(transparent)]
    Unowned(#[from] OwnershipError),
    /// The file system could not be inspected.
    #[error("cannot inspect `{}`: {source}", path.display())]
    Io {
        /// The path being inspected.
        path: PathBuf,
        /// The underlying error.
        source: io::Error,
    },
    /// A scenario path breaks the fixture-relative path rule.
    #[error("`{path}` is not a fixture-relative path: {violation}")]
    InvalidPath {
        /// The offending path.
        path: String,
        /// The rule it breaks.
        violation: PathViolation,
    },
    /// A fixture-relative path passes through a symbolic link.
    #[error(
        "`{path}` passes through the symbolic link `{link}`; a runner never follows links inside \
         a fixture"
    )]
    SymlinkInPath {
        /// The scenario path.
        path: String,
        /// The link, relative to the fixture root.
        link: String,
    },
}

/// A directory that carries the ownership marker.
///
/// The root is canonical: it is the spelling `excise` shows for its scan root, which is what the
/// deletion dialog is compared with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixtureRoot {
    path: PathBuf,
}

impl FixtureRoot {
    /// Accepts `root` only if the fixture generator's `verify_owned` does: a directory that is not
    /// a symbolic link, holding a marker that is a regular file. That check is the one
    /// implementation every runner and mutator shares; this adds the canonical spelling. The
    /// check is made here, once: [`FixtureRoot::verify_owned`] makes it again.
    ///
    /// # Errors
    ///
    /// Returns [`SafetyError::Unowned`] for a directory that is not a harness fixture, and
    /// [`SafetyError::Io`] when its canonical path cannot be resolved.
    pub fn open(root: impl AsRef<Path>) -> Result<Self, SafetyError> {
        let root = root.as_ref();
        verify_owned(root)?;
        let path = fs::canonicalize(root).map_err(|source| SafetyError::Io {
            path: root.to_path_buf(),
            source,
        })?;
        Ok(Self { path })
    }

    /// Checks again that the root carries the ownership marker: the fixture generator's
    /// `verify_owned`, run on the canonical root once more.
    ///
    /// [`FixtureRoot::open`] verifies ownership once, and a `FixtureRoot` is only a path after
    /// that. A marker that vanishes, or is replaced by a directory or a link, while the root is in
    /// use goes unnoticed until something checks again, and the program under test can delete
    /// anything it shows, the marker included. A runner calls this immediately before a key that
    /// starts or confirms a deletion, so that a root that is no longer a fixture never goes
    /// through one.
    ///
    /// # Errors
    ///
    /// Returns [`SafetyError::Unowned`] when the root is no longer a harness fixture: it cannot be
    /// opened as a directory, the marker is missing, or the marker is not a regular file.
    pub fn verify_owned(&self) -> Result<(), SafetyError> {
        verify_owned(&self.path)?;
        Ok(())
    }

    /// The canonical root.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The fixture-relative `relative` joined onto the root.
    ///
    /// This only applies the textual rule; use [`FixtureRoot::lstat`] to look at the entry.
    ///
    /// # Errors
    ///
    /// Returns [`SafetyError::InvalidPath`] when `relative` breaks the fixture-relative path rule.
    pub fn join(&self, relative: &str) -> Result<PathBuf, SafetyError> {
        check_fixture_relative_path(relative).map_err(|violation| SafetyError::InvalidPath {
            path: relative.to_owned(),
            violation,
        })?;
        Ok(relative
            .split('/')
            .fold(self.path.clone(), |joined, component| {
                joined.join(component)
            }))
    }

    /// Looks up the fixture-relative `relative` without following symbolic links.
    ///
    /// Returns `Ok(None)` when the entry does not exist, including when a directory on the way is
    /// missing or is a plain file. The last component is inspected with `lstat`, so a symbolic
    /// link is an entry in its own right, even a dangling one.
    ///
    /// # Errors
    ///
    /// Returns [`SafetyError::SymlinkInPath`] when a directory component is a symbolic link, and
    /// [`SafetyError::InvalidPath`] or [`SafetyError::Io`] for the other failures.
    pub fn lstat(&self, relative: &str) -> Result<Option<fs::Metadata>, SafetyError> {
        check_fixture_relative_path(relative).map_err(|violation| SafetyError::InvalidPath {
            path: relative.to_owned(),
            violation,
        })?;
        let components: Vec<&str> = relative.split('/').collect();
        let mut current = self.path.clone();
        for (index, component) in components.iter().enumerate() {
            current.push(component);
            let metadata = match fs::symlink_metadata(&current) {
                Ok(metadata) => metadata,
                Err(source) if is_absent(&source) => return Ok(None),
                Err(source) => {
                    return Err(SafetyError::Io {
                        path: current,
                        source,
                    });
                }
            };
            if index + 1 == components.len() {
                return Ok(Some(metadata));
            }
            if metadata.file_type().is_symlink() {
                return Err(SafetyError::SymlinkInPath {
                    path: relative.to_owned(),
                    link: components[..=index].join("/"),
                });
            }
            if !metadata.is_dir() {
                return Ok(None);
            }
        }
        Ok(None)
    }

    /// Whether the fixture-relative `relative` exists, as [`FixtureRoot::lstat`] sees it.
    ///
    /// # Errors
    ///
    /// The errors of [`FixtureRoot::lstat`].
    pub fn exists(&self, relative: &str) -> Result<bool, SafetyError> {
        self.lstat(relative).map(|metadata| metadata.is_some())
    }

    /// Every entry called `name` that is a folder (`directory`) or is not one, anywhere under the
    /// root, as fixture-relative `/`-separated paths in name order.
    ///
    /// The walk takes each entry's kind from its directory entry, so a symbolic link is an entry
    /// that is not a folder and is never entered. A deletion that no dialog confirms uses this to
    /// tell whether a name and a kind point at one entry only.
    ///
    /// # Errors
    ///
    /// [`SafetyError::Io`] when a folder cannot be listed.
    pub fn entries_named(&self, name: &str, directory: bool) -> Result<Vec<String>, SafetyError> {
        let mut found = Vec::new();
        let mut pending = vec![(self.path.clone(), String::new())];
        while let Some((folder, prefix)) = pending.pop() {
            let io_error = |source| SafetyError::Io {
                path: folder.clone(),
                source,
            };
            for entry in fs::read_dir(&folder).map_err(io_error)? {
                let entry = entry.map_err(io_error)?;
                let is_folder = entry.file_type().map_err(io_error)?.is_dir();
                let entry_name = entry.file_name();
                let relative = format!("{prefix}{}", entry_name.to_string_lossy());
                if entry_name == name && is_folder == directory {
                    found.push(relative.clone());
                }
                if is_folder {
                    pending.push((entry.path(), format!("{relative}/")));
                }
            }
        }
        found.sort();
        Ok(found)
    }

    /// Checks that `relative` is the only entry called `name` of `kind` anywhere under the root.
    ///
    /// The selected-item panel shows a name and a kind and nothing else, so a deletion that no
    /// dialog confirms is bound to the entry the scenario names only when no other entry could
    /// be the one selected.
    ///
    /// # Errors
    ///
    /// Returns why not: the fixture cannot be searched, `relative` is not the entry, or another
    /// entry has the same name and kind.
    pub fn require_only_entry(
        &self,
        name: &str,
        kind: EntryKind,
        relative: &str,
    ) -> Result<(), String> {
        let found = self
            .entries_named(name, kind == EntryKind::Folder)
            .map_err(|error| {
                format!("the fixture cannot be searched for other entries called `{name}`: {error}")
            })?;
        match found.as_slice() {
            [only] if only == relative => Ok(()),
            [only] => Err(format!(
                "the only {kind} called `{name}` is `{only}`, but the step says it is at \
                 `{relative}`"
            )),
            _ => Err(format!(
                "the fixture has {} {kind}s called `{name}` ({}), and the selected-item panel \
                 shows a name and a kind only, so a deletion with no dialog cannot tell which one \
                 is selected; give each entry a name of its own",
                found.len(),
                found
                    .iter()
                    .map(|path| format!("`{path}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }
}

/// `NotFound`, or a path component that is not a directory.
fn is_absent(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{MARKER_FILE_NAME, NodeKind};

    fn marked_root() -> tempfile::TempDir {
        let root = tempfile::tempdir().expect("a temporary directory");
        fs::write(root.path().join(MARKER_FILE_NAME), b"owned").expect("the marker");
        root
    }

    #[test]
    fn a_directory_without_the_marker_is_refused() {
        let root = tempfile::tempdir().expect("a temporary directory");
        fs::write(root.path().join("file"), b"x").expect("a file");

        let error = FixtureRoot::open(root.path()).expect_err("an unmarked root must be refused");

        assert!(
            matches!(error, SafetyError::Unowned(OwnershipError::Missing { .. })),
            "{error}"
        );
        assert!(error.to_string().contains(MARKER_FILE_NAME));
    }

    #[test]
    fn a_marker_that_is_not_a_regular_file_is_refused() {
        let root = tempfile::tempdir().expect("a temporary directory");
        fs::create_dir(root.path().join(MARKER_FILE_NAME)).expect("a marker directory");

        let error = FixtureRoot::open(root.path()).expect_err("a marker directory must be refused");

        assert!(
            matches!(
                error,
                SafetyError::Unowned(OwnershipError::NotRegular {
                    kind: NodeKind::Directory,
                    ..
                })
            ),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_marker_that_is_a_symbolic_link_is_refused() {
        let elsewhere = marked_root();
        let root = tempfile::tempdir().expect("a temporary directory");
        std::os::unix::fs::symlink(
            elsewhere.path().join(MARKER_FILE_NAME),
            root.path().join(MARKER_FILE_NAME),
        )
        .expect("a marker link");

        let error = FixtureRoot::open(root.path()).expect_err("a linked marker must be refused");

        assert!(
            matches!(
                error,
                SafetyError::Unowned(OwnershipError::NotRegular {
                    kind: NodeKind::Symlink,
                    ..
                })
            ),
            "{error}"
        );
    }

    /// Canonicalizing first would resolve the link and hide it, so the ownership check must see
    /// the root as given.
    #[cfg(unix)]
    #[test]
    fn a_root_that_is_a_symbolic_link_is_refused_even_when_it_points_at_a_fixture() {
        let real = marked_root();
        let parent = tempfile::tempdir().expect("a temporary directory");
        let link = parent.path().join("link");
        std::os::unix::fs::symlink(real.path(), &link).expect("a root link");

        let error = FixtureRoot::open(&link).expect_err("a linked root must be refused");

        assert!(
            matches!(error, SafetyError::Unowned(OwnershipError::Root { .. })),
            "{error}"
        );
    }

    #[test]
    fn a_missing_root_is_not_a_fixture() {
        let parent = tempfile::tempdir().expect("a temporary directory");

        let error = FixtureRoot::open(parent.path().join("absent")).expect_err("no such root");

        assert!(
            matches!(error, SafetyError::Unowned(OwnershipError::Root { .. })),
            "{error}"
        );
    }

    #[test]
    fn a_marked_root_is_accepted_under_its_canonical_path() {
        let root = marked_root();

        let opened = FixtureRoot::open(root.path()).expect("a marked root");

        assert_eq!(
            opened.path(),
            fs::canonicalize(root.path()).expect("canonical path")
        );
    }

    #[test]
    fn a_root_that_still_carries_its_marker_verifies_again() {
        let root = marked_root();
        let opened = FixtureRoot::open(root.path()).expect("a marked root");

        opened.verify_owned().expect("the marker is still there");
    }

    #[test]
    fn a_marker_that_vanished_after_the_root_was_opened_is_caught() {
        let root = marked_root();
        let opened = FixtureRoot::open(root.path()).expect("a marked root");
        fs::remove_file(root.path().join(MARKER_FILE_NAME)).expect("the marker should go");

        let error = opened
            .verify_owned()
            .expect_err("a root that lost its marker must not be owned any more");

        assert!(
            matches!(error, SafetyError::Unowned(OwnershipError::Missing { .. })),
            "{error}"
        );
        assert!(error.to_string().contains(MARKER_FILE_NAME));
    }

    #[test]
    fn a_marker_replaced_by_a_directory_after_the_root_was_opened_is_caught() {
        let root = marked_root();
        let opened = FixtureRoot::open(root.path()).expect("a marked root");
        let marker = root.path().join(MARKER_FILE_NAME);
        fs::remove_file(&marker).expect("the marker should go");
        fs::create_dir(&marker).expect("a directory should take its place");

        let error = opened
            .verify_owned()
            .expect_err("a directory in the marker's place must not pass");

        assert!(
            matches!(
                error,
                SafetyError::Unowned(OwnershipError::NotRegular {
                    kind: NodeKind::Directory,
                    ..
                })
            ),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_marker_replaced_by_a_symbolic_link_after_the_root_was_opened_is_caught() {
        let elsewhere = marked_root();
        let root = marked_root();
        let opened = FixtureRoot::open(root.path()).expect("a marked root");
        let marker = root.path().join(MARKER_FILE_NAME);
        fs::remove_file(&marker).expect("the marker should go");
        std::os::unix::fs::symlink(elsewhere.path().join(MARKER_FILE_NAME), &marker)
            .expect("a marker link");

        let error = opened
            .verify_owned()
            .expect_err("a link in the marker's place must not pass");

        assert!(
            matches!(
                error,
                SafetyError::Unowned(OwnershipError::NotRegular {
                    kind: NodeKind::Symlink,
                    ..
                })
            ),
            "{error}"
        );
    }

    #[test]
    fn a_root_that_vanished_after_it_was_opened_is_caught() {
        let parent = tempfile::tempdir().expect("a temporary directory");
        let root = parent.path().join("root");
        fs::create_dir(&root).expect("a root");
        fs::write(root.join(MARKER_FILE_NAME), b"owned").expect("the marker");
        let opened = FixtureRoot::open(&root).expect("a marked root");
        fs::remove_dir_all(&root).expect("the root should go");

        let error = opened
            .verify_owned()
            .expect_err("a root that is gone must not be owned any more");

        assert!(
            matches!(error, SafetyError::Unowned(OwnershipError::Root { .. })),
            "{error}"
        );
    }

    #[test]
    fn lstat_reports_present_and_absent_entries() {
        let root = marked_root();
        fs::create_dir_all(root.path().join("a/b")).expect("directories");
        fs::write(root.path().join("a/b/file"), b"x").expect("a file");
        let fixture = FixtureRoot::open(root.path()).expect("a marked root");

        assert!(fixture.exists("a/b/file").expect("lstat"));
        assert!(!fixture.exists("a/missing").expect("lstat"));
        assert!(!fixture.exists("missing/deeper/file").expect("lstat"));
        assert!(
            !fixture.exists("a/b/file/below").expect("lstat"),
            "an entry below a plain file cannot exist"
        );
    }

    #[test]
    fn unsafe_paths_are_rejected_before_touching_the_file_system() {
        let root = marked_root();
        let fixture = FixtureRoot::open(root.path()).expect("a marked root");

        for path in ["../escape", "/etc/passwd", "a//b", ""] {
            let error = fixture
                .lstat(path)
                .expect_err("an unsafe path must be rejected");
            assert!(
                matches!(error, SafetyError::InvalidPath { .. }),
                "{path}: {error}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_symbolic_link_directory_is_never_followed() {
        let outside = tempfile::tempdir().expect("an outside directory");
        fs::write(outside.path().join("secret"), b"x").expect("a file outside");
        let root = marked_root();
        std::os::unix::fs::symlink(outside.path(), root.path().join("link")).expect("a link");
        let fixture = FixtureRoot::open(root.path()).expect("a marked root");

        let error = fixture
            .lstat("link/secret")
            .expect_err("resolution must not pass through the link");

        assert!(
            matches!(&error, SafetyError::SymlinkInPath { link, .. } if link == "link"),
            "{error}"
        );
        assert!(
            fixture.exists("link").expect("lstat"),
            "the link itself is an entry, even though its target is elsewhere"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_symbolic_link_counts_as_present() {
        let root = marked_root();
        std::os::unix::fs::symlink("nowhere", root.path().join("dangling")).expect("a link");
        let fixture = FixtureRoot::open(root.path()).expect("a marked root");

        assert!(fixture.exists("dangling").expect("lstat"));
    }

    #[test]
    fn entries_named_finds_every_entry_of_that_name_and_kind_at_any_depth() {
        let root = marked_root();
        fs::create_dir_all(root.path().join("a/b/twin")).expect("directories");
        fs::create_dir_all(root.path().join("twin")).expect("a folder at the root");
        fs::write(root.path().join("twin.bin"), b"x").expect("a file");
        fs::write(root.path().join("a/twin.bin"), b"x").expect("a nested file");
        fs::write(root.path().join("a/b/twin/twin.bin"), b"x").expect("a deeper file");
        fs::write(root.path().join("a/twins.bin"), b"x").expect("a name that only starts alike");
        let fixture = FixtureRoot::open(root.path()).expect("a marked root");

        assert_eq!(
            fixture.entries_named("twin.bin", false).expect("a walk"),
            ["a/b/twin/twin.bin", "a/twin.bin", "twin.bin"]
        );
        assert_eq!(
            fixture.entries_named("twin", true).expect("a walk"),
            ["a/b/twin", "twin"]
        );
        assert!(
            fixture
                .entries_named("twin", false)
                .expect("a walk")
                .is_empty(),
            "a folder is not a file of that name"
        );
        assert!(
            fixture
                .entries_named("twin.bin", true)
                .expect("a walk")
                .is_empty(),
            "a file is not a folder of that name"
        );
    }

    #[cfg(unix)]
    #[test]
    fn entries_named_never_enters_a_symbolic_link_and_counts_it_as_no_folder() {
        let outside = tempfile::tempdir().expect("an outside directory");
        fs::write(outside.path().join("twin.bin"), b"x").expect("a file outside");
        let root = marked_root();
        std::os::unix::fs::symlink(outside.path(), root.path().join("twin")).expect("a link");
        let fixture = FixtureRoot::open(root.path()).expect("a marked root");

        assert!(
            fixture
                .entries_named("twin.bin", false)
                .expect("a walk")
                .is_empty(),
            "what lies behind a link is out of reach"
        );
        assert!(
            fixture
                .entries_named("twin", true)
                .expect("a walk")
                .is_empty(),
            "a link to a folder is not a folder entry"
        );
        assert_eq!(
            fixture.entries_named("twin", false).expect("a walk"),
            ["twin"],
            "the link is an entry that is not a folder"
        );
    }
}
