//! Shared helpers for the fixture tests.

#[cfg(unix)]
use std::time::Duration;
use std::{
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
};

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

    /// The path of the scratch directory as it resolves, with every link in it expanded: below
    /// `/var` on macOS, a link to `/private/var`, it is 8 bytes longer than it is written, and on
    /// Windows it is the verbatim form. A cache measures its root that way too, so a test that
    /// builds a root of an exact length builds it here.
    pub(super) fn resolved_path(&self) -> PathBuf {
        fs::canonicalize(self.path())
            .unwrap_or_else(|error| panic!("cannot resolve the scratch directory: {error}"))
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

/// A symbolic link at `link` to `target`.
#[cfg(unix)]
pub(super) fn link_to(link: &Path, target: &Path) {
    std::os::unix::fs::symlink(target, link).unwrap_or_else(|error| {
        panic!(
            "cannot link {} to {}: {error}",
            link.display(),
            target.display()
        )
    });
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

/// A shaped part 32 levels deep, a chain of folders with names of 255 bytes, with a file and
/// links at the bottom: the longest path it plans is over 7,900 bytes, which no path-based call
/// can take. Its id is `shaped-deep-names`.
pub(super) fn deep_names_spec() -> String {
    let mut text = String::from(
        "schema_version = 1\nid = \"shaped-deep-names\"\ndescription = \"A deep chain.\"\nseed = 9\n\n\
         [[parts]]\nkind = \"shaped\"\nroot = \"home\"\nmax_file_bytes = 100\ndangling_symlinks = 1\n",
    );
    for _ in 1..32 {
        text.push_str("\n[[parts.levels]]\ndirectories = 1\n");
    }
    text.push_str("\n[[parts.levels]]\nfiles = 2\nsymlinks = 2\n");
    text.push_str(
        "\n[parts.subdirectories_per_directory]\n1 = 1\n\n\
         [parts.files_per_directory]\n2 = 1\n\n\
         [parts.file_sizes]\n1 = 1\n\n\
         [parts.directory_name_lengths]\n255 = 1\n\n\
         [parts.file_name_lengths]\n8 = 1\n\n\
         [parts.symlink_name_lengths]\n8 = 1\n",
    );
    text
}

/// The text of a spec with a shaped part that is a chain of `folders` folders with names of
/// `folder_name` bytes and one file at the bottom with a name of `file_name` bytes, below the root
/// `home`: the longest path it plans is `4 + folders * (folder_name + 1) + 1 + file_name` bytes,
/// whatever the seed.
pub(crate) fn chain_spec(id: &str, folders: u32, folder_name: u32, file_name: u32) -> String {
    let mut text = format!(
        "schema_version = 1\nid = \"{id}\"\ndescription = \"A chain.\"\nseed = 3\n\n\
         [[parts]]\nkind = \"shaped\"\nroot = \"home\"\nmax_file_bytes = 100\n"
    );
    for _ in 0..folders {
        text.push_str("\n[[parts.levels]]\ndirectories = 1\n");
    }
    text.push_str("\n[[parts.levels]]\nfiles = 1\n");
    write!(
        text,
        "\n[parts.subdirectories_per_directory]\n1 = 1\n\n\
         [parts.files_per_directory]\n1 = 1\n\n\
         [parts.file_sizes]\n1 = 1\n\n\
         [parts.directory_name_lengths]\n{folder_name} = 1\n\n\
         [parts.file_name_lengths]\n{file_name} = 1\n"
    )
    .expect("text");
    text
}

/// The text of a spec with a `tree` part, a chain of `folders` folders with names of `folder_name`
/// bytes and one file at the bottom with a name of `file_name` bytes, below the root `home`: the
/// longest path it plans is `4 + folders * (folder_name + 1) + 1 + file_name` bytes, as that of
/// [`chain_spec`] is, but this part is a `tree`, whose paths the cache has to hold to the limit
/// as well.
pub(crate) fn tree_chain_spec(
    id: &str,
    folders: u32,
    folder_name: usize,
    file_name: usize,
) -> String {
    format!(
        "schema_version = 1\nid = \"{id}\"\ndescription = \"A chain of folders.\"\nseed = 3\n\n\
         [[parts]]\nkind = \"tree\"\nroot = \"home\"\ndepth = {folders}\nfanout = 1\n\
         files_per_dir = 1\nfile_placement = \"leaves\"\n\
         dir_names = {{ style = \"literal\", name = \"{}\" }}\n\
         file_names = {{ style = \"literal\", name = \"{}\" }}\n",
        "d".repeat(folder_name),
        "f".repeat(file_name)
    )
}

/// A path below `base` that is `bytes` bytes long in all, made of names of at most 201 bytes (a
/// name can have 255): the root of a cache that is as deep as a test needs. Nothing is created.
pub(crate) fn path_of_len(base: &Path, bytes: usize) -> PathBuf {
    let mut path = base.to_path_buf();
    let mut room = bytes
        .checked_sub(path.as_os_str().len())
        .filter(|room| *room >= 2)
        .expect("the base leaves room for a name and its separator");
    while room > 0 {
        // A separator and a name that is never empty: when little is left it takes all of it
        // but the separator, so that no single byte is ever left that nothing could fill.
        let name = if room > 202 { 200 } else { room - 1 };
        path.push("a".repeat(name));
        room -= name + 1;
    }
    path
}

/// A path that is short as it is written and as it resolves, and long for the system: see
/// [`long_way_round`].
#[cfg(unix)]
pub(super) struct LongWay {
    /// The folder the path ends in, directly below the base.
    pub(super) near: PathBuf,
    /// A link to `near`, in a real folder deep below the base. Its path is the content of
    /// `start`.
    pub(super) back: PathBuf,
    /// A short link, directly below the base, to `back`.
    pub(super) start: PathBuf,
}

/// Makes a long way round to a short folder: `near`, directly below `base`; a real folder
/// `deep_bytes` bytes below it, with `back` in it, a link to `near`, which makes the path of
/// `back` `deep_bytes + 5` bytes; and `start`, directly below `base`, a link to `back`. A path that
/// begins with `start` is short as it is written and short as it resolves (below `near`), but to
/// reach what is below it the system works on a pathname that is the content of `start`, the
/// `deep_bytes + 5` bytes of `back`, and what is left of the path after `start`.
#[cfg(unix)]
pub(super) fn long_way_round(base: &Path, deep_bytes: usize) -> LongWay {
    let near = base.join("near");
    fs::create_dir(&near).expect("a short folder");
    let deep = path_of_len(base, deep_bytes);
    fs::create_dir_all(&deep).expect("a deep folder");
    let back = deep.join("back");
    link_to(&back, &near);
    let start = base.join("start");
    link_to(&start, &back);
    LongWay { near, back, start }
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
