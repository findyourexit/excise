//! Fixtures for the scenario runner, and its file-system checks.
//!
//! A scenario's `fixture` id names a spec in the harness crate's `fixtures/` directory. The runner
//! itself never builds anything: it takes an already-materialized root and refuses one that does
//! not carry the harness ownership marker. [`materialize`] is what the suite calls to get such a
//! root: a disposable copy of the fixture, generated fresh by the harness in a scratch directory
//! of its own, and removed when the [`Fixture`] drops.

use std::fs;
use std::io::{self, ErrorKind};
use std::path::Path;

use excise_harness::fixture::{Fixtures, RunCopy};
use excise_harness::scenario::check_fixture_relative_path;
use tempfile::TempDir;

/// A disposable copy of a bundled fixture. It is removed when dropped.
pub struct Fixture {
    /// Declared first so that it drops first: the copy removes its tree, restoring permissions,
    /// before the directory it lives in goes.
    copy: RunCopy,
    _scratch: TempDir,
}

impl Fixture {
    /// The fixture root, which carries the ownership marker.
    pub fn root(&self) -> &Path {
        self.copy.root()
    }
}

/// Why a fixture could not be materialized.
#[derive(Debug, thiserror::Error)]
pub enum FixtureError {
    #[error("no scratch directory for the fixture: {0}")]
    Scratch(io::Error),
    #[error("cannot generate fixture `{id}`: {source}")]
    Generate {
        id: String,
        source: Box<excise_harness::fixture::FixtureError>,
    },
}

/// Generates a fresh, marked copy of the bundled fixture `id`.
///
/// # Errors
///
/// Returns why the scratch directory or the copy could not be made, for instance because no spec
/// has this id.
pub fn materialize(id: &str) -> Result<Fixture, FixtureError> {
    let scratch = tempfile::Builder::new()
        .prefix("excise-fixture-")
        .tempdir()
        .map_err(FixtureError::Scratch)?;
    let copy = Fixtures::bundled()
        .run_copy(id, scratch.path())
        .map_err(|source| FixtureError::Generate {
            id: id.to_owned(),
            source: Box::new(source),
        })?;
    Ok(Fixture {
        copy,
        _scratch: scratch,
    })
}

/// Whether the fixture-relative `path` names an entry now.
///
/// The path is resolved one component at a time and never through a symbolic link: a link inside
/// a fixture cannot redirect a check outside it. An entry behind a link is not reachable, so it
/// does not exist here. A final component that is itself a link exists, dangling or not.
///
/// # Errors
///
/// Returns a message when `path` is not a fixture-relative path or an entry cannot be inspected.
pub fn entry_exists(root: &Path, path: &str) -> Result<bool, String> {
    entry_metadata(root, path).map(|metadata| metadata.is_some())
}

/// What the fixture-relative `path` names now, without following a link (`entry_exists` says how
/// the path is resolved), or `None` when it names nothing.
///
/// # Errors
///
/// Returns a message when `path` is not a fixture-relative path or an entry cannot be inspected.
pub fn entry_metadata(root: &Path, path: &str) -> Result<Option<fs::Metadata>, String> {
    check_fixture_relative_path(path)
        .map_err(|violation| format!("`{path}` is not a fixture-relative path: {violation}"))?;
    let mut current = root.to_path_buf();
    let mut components = path.split('/').peekable();
    let mut found = None;
    while let Some(component) = components.next() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if components.peek().is_some() && !metadata.is_dir() {
                    return Ok(None);
                }
                found = Some(metadata);
            }
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("`{path}` cannot be inspected: {error}")),
        }
    }
    Ok(found)
}

/// The names of the entries directly under `directory`, or none when it does not exist.
///
/// # Errors
///
/// Returns the error from reading an existing directory.
pub fn entry_names(directory: &Path) -> io::Result<Vec<String>> {
    match fs::read_dir(directory) {
        Ok(entries) => entries
            .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
            .collect(),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fixture_path_names_an_entry_only_through_real_directories() {
        let fixture = materialize("delete-file").expect("the fixture should build");
        let root = fixture.root();
        assert_eq!(entry_exists(root, "victim.bin"), Ok(true));
        assert_eq!(entry_exists(root, "docs"), Ok(true));
        assert_eq!(entry_exists(root, "docs/keep-0.txt"), Ok(true));
        assert_eq!(entry_exists(root, "missing.bin"), Ok(false));
        assert_eq!(entry_exists(root, "docs/missing/deeper.txt"), Ok(false));
        // A file is not a directory, so nothing lies below it.
        assert_eq!(entry_exists(root, "victim.bin/child"), Ok(false));
    }

    #[test]
    fn a_path_that_could_leave_the_fixture_is_an_error_not_an_answer() {
        let fixture = materialize("delete-file").expect("the fixture should build");
        for path in [
            "../outside",
            "/etc/passwd",
            "docs//keep-0.txt",
            "docs/../victim.bin",
            "C:x",
        ] {
            let error = entry_exists(fixture.root(), path).expect_err("the path must be refused");
            assert!(
                error.contains("not a fixture-relative path"),
                "{path}: {error}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_link_is_an_entry_but_never_a_way_through() {
        use std::os::unix::fs::symlink;

        let fixture = materialize("delete-file").expect("the fixture should build");
        let root = fixture.root();
        let outside = tempfile::tempdir().expect("the outside directory should exist");
        fs::write(outside.path().join("secret.txt"), b"secret")
            .expect("the file should be written");
        symlink(outside.path(), root.join("linked-dir")).expect("the link should be created");
        symlink(root.join("no-such-target"), root.join("dangling"))
            .expect("the link should be created");

        // The link itself is an entry, dangling or not.
        assert_eq!(entry_exists(root, "linked-dir"), Ok(true));
        assert_eq!(entry_exists(root, "dangling"), Ok(true));
        // What lies behind it is out of reach.
        assert_eq!(entry_exists(root, "linked-dir/secret.txt"), Ok(false));
    }
}
