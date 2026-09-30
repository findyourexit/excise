//! Shared helpers for the fixture tests.

use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::time::Duration;

use crate::fixture::{FixtureCache, FixtureSpec, MaterializeOptions, Materialized, remove_tree};

/// A private temporary directory that is removed when it drops, even when it holds directories
/// with mode `000`, and even while a failed test unwinds.
pub(super) struct Scratch {
    dir: Option<tempfile::TempDir>,
}

impl Scratch {
    pub(super) fn new() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("excise-fixture-test-")
            .tempdir()
            .unwrap_or_else(|error| panic!("cannot create a temporary directory: {error}"));
        Self { dir: Some(dir) }
    }

    pub(super) fn path(&self) -> &Path {
        self.dir
            .as_ref()
            .map_or_else(|| Path::new(""), tempfile::TempDir::path)
    }

    /// A subdirectory path that does not exist yet.
    pub(super) fn join(&self, name: &str) -> PathBuf {
        self.path().join(name)
    }

    /// A cache rooted inside the scratch directory.
    pub(super) fn cache(&self) -> FixtureCache {
        FixtureCache::at(self.join("cache"))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if let Some(dir) = self.dir.take() {
            let path = dir.path().to_path_buf();
            // Restores permissions first; `TempDir` alone could not remove a mode `000` tree.
            let _ = remove_tree(&path);
            drop(dir);
        }
    }
}

pub(super) fn spec(id: &str) -> FixtureSpec {
    FixtureSpec::load_bundled(id).unwrap_or_else(|error| panic!("{id}: {error}"))
}

/// A 40-entry spec with two top-level parts, for the tests that generate many times over. Cheap
/// enough that damaging and regenerating an entry costs milliseconds.
pub(super) fn tiny_spec() -> FixtureSpec {
    FixtureSpec::from_toml_str(
        r#"
        schema_version = 1
        id = "tiny"
        description = "Two small trees."
        seed = 5

        [[parts]]
        kind = "tree"
        root = "alpha"
        depth = 2
        fanout = 3
        files_per_dir = 2
        file_placement = "leaves"
        file_size = { min = 1, max = 50 }

        [[parts]]
        kind = "tree"
        root = "beta"
        depth = 1
        fanout = 2
        files_per_dir = 2
        file_names = { style = "hex", prefix = "b", length = 4 }
        "#,
    )
    .unwrap_or_else(|error| panic!("the tiny spec: {error}"))
}

/// The shape of `delete-folder` at unit-test size: a `victim/` tree of 49 entries and the
/// sentinels `keep-a.bin` and `keep-b/keep.txt`.
pub(super) fn victim_spec() -> FixtureSpec {
    FixtureSpec::from_toml_str(
        r#"
        schema_version = 1
        id = "victim-small"
        description = "A folder to delete and the siblings that must survive."
        seed = 9

        [[parts]]
        kind = "file"
        root = "keep-a.bin"
        size = 4096

        [[parts]]
        kind = "tree"
        root = "keep-b"
        depth = 0
        files_per_dir = 1
        file_names = { style = "literal", name = "keep.txt" }
        file_size = 64

        [[parts]]
        kind = "tree"
        root = "victim"
        depth = 2
        fanout = 3
        files_per_dir = 4
        file_placement = "leaves"
        dir_names = { style = "sequential", prefix = "part", width = 2 }
        file_names = { style = "hex", prefix = "blob-", suffix = ".bin", length = 6 }
        file_size = { min = 100, max = 8192 }
        "#,
    )
    .unwrap_or_else(|error| panic!("the victim spec: {error}"))
}

/// Materializes `spec` in `scratch`'s cache.
pub(super) fn master_of(scratch: &Scratch, spec: &FixtureSpec) -> Materialized {
    scratch
        .cache()
        .materialize(spec, &MaterializeOptions::default())
        .unwrap_or_else(|error| panic!("{}: {error}", spec.id))
}

/// Materializes the bundled spec `id` in `scratch`'s cache.
pub(super) fn master(scratch: &Scratch, id: &str) -> Materialized {
    scratch
        .cache()
        .materialize(&spec(id), &MaterializeOptions::default())
        .unwrap_or_else(|error| panic!("{id}: {error}"))
}

/// Runs `command` with a hard time limit and returns its standard output, or `None` when the
/// program is not available. Never waits longer than `limit`.
#[cfg(unix)]
pub(super) fn run_bounded(
    program: &str,
    args: &[&std::ffi::OsStr],
    limit: Duration,
) -> Option<(bool, String)> {
    use std::{
        io::Read as _,
        process::{Command, Stdio},
        time::Instant,
    };

    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < limit => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("`{program}` did not finish within {limit:?}");
            }
        }
    };
    let mut output = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        let _ = stdout.read_to_string(&mut output);
    }
    Some((status.success(), output))
}
