//! The fixture cache: generate once, verify, reuse.
//!
//! Cached masters live at
//!
//! ```text
//! <target dir>/excise-fixtures.noindex/<spec-hash>-<generator-version>/
//! ```
//!
//! where `<target dir>` is `CARGO_TARGET_DIR` or the workspace `target`, `<spec-hash>` is the
//! first 16 hex digits of the spec hash (seed included), and `<generator-version>` is
//! [`GENERATOR_VERSION`]. The directory is the fixture root: it holds the generated tree and the
//! ownership marker, and nothing else. The `.noindex` suffix keeps Spotlight out.
//!
//! * **Publishing.** A fixture is generated into a `.partial-*` sibling, its marker is written
//!   last, and the directory is renamed into place. A reader therefore sees either nothing or a
//!   complete, sealed entry, and two processes racing to build the same entry cannot corrupt each
//!   other: the loser finds the winner's entry, verifies it, and discards its own.
//! * **Reuse.** An existing entry is verified before it is used (see [`crate::fixture::integrity`]).
//!   A hit skips generation entirely. An entry that fails verification is removed and generated
//!   again.
//! * **Masters are read-only by convention.** A runner that mutates or deletes must work on a
//!   [`RunCopy`](crate::fixture::run::RunCopy).
//!
//! Tests never use the shared cache: they pass a temporary directory to [`FixtureCache::at`].

use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use crate::fixture::{
    GENERATOR_VERSION,
    caps::Capabilities,
    error::FixtureError,
    generate::{GenerateOptions, GenerateReport, generate},
    integrity::{IntegrityFailure, Verified, Verify, verify_master},
    marker::{Marker, Role, write_marker},
    plan::Plan,
    resolve::resolve,
    spec::FixtureSpec,
    tree::{TreeGuard, remove_tree},
};

/// The name of the cache directory below the target directory.
pub const CACHE_DIR_NAME: &str = "excise-fixtures.noindex";

/// How many hexadecimal digits of the spec hash name an entry, and a fixture that is still being
/// generated.
const NAME_DIGITS: usize = 16;

/// The digits of `spec_hash` that name a directory of the cache.
fn name_digits(spec_hash: &str) -> &str {
    &spec_hash[..NAME_DIGITS]
}

/// The name of the entry of the spec whose digits are `digits`: the digits and the generator
/// version.
fn entry_name(digits: &str) -> String {
    format!("{digits}-{GENERATOR_VERSION}")
}

/// The name a fixture has while it is generated, until it is renamed into place: the digits, then
/// the number of the process and of the call, so that two never share one.
fn partial_name(digits: &str, process: u32, call: u64) -> String {
    format!(".partial-{digits}-{process}-{call}")
}

/// The length in bytes of the longest name the cache gives a directory directly below its root:
/// that of an entry, or that of a partial one, which is longer, at the largest process number and
/// call count there can be, so that it holds for every process and every call.
fn longest_name_bytes() -> usize {
    let digits = "0".repeat(NAME_DIGITS);
    entry_name(&digits)
        .len()
        .max(partial_name(&digits, u32::MAX, u64::MAX).len())
}

/// How to materialize a fixture.
#[derive(Debug, Clone, Copy, Default)]
pub struct MaterializeOptions {
    /// A seed to use instead of the spec's default. The seed is part of the spec hash, so each
    /// seed has its own cache entry.
    pub seed: Option<u64>,
    /// How to generate.
    pub generate: GenerateOptions,
    /// How thoroughly to verify an existing entry before reusing it.
    pub verify: Verify,
}

/// The cache of generated fixtures.
#[derive(Debug, Clone)]
pub struct FixtureCache {
    root: PathBuf,
}

impl FixtureCache {
    /// A cache rooted at `root`. Tests pass a temporary directory.
    #[must_use]
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The default cache: `excise-fixtures.noindex` in `CARGO_TARGET_DIR`, or in the `target`
    /// directory of this workspace.
    #[must_use]
    pub fn in_target_dir() -> Self {
        Self::at(default_root(std::env::var_os("CARGO_TARGET_DIR")))
    }

    /// The directory that holds the entries.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The length in bytes of the longest absolute path that a fixture root in this cache can
    /// have: the path of the cache directory, a separator, and the longest name the cache gives a
    /// directory directly below it, which is that of a fixture while it is generated, in a
    /// `.partial-` directory, before it is renamed into place. A path-based removal of the
    /// directory that holds the cache (`cargo clean`, `git worktree remove`) is handed the whole
    /// path, from the root of the file system, so what the fixture holds has to fit in what this
    /// leaves of `PATH_MAX`: see
    /// [`FixtureSpec::removable_by_path`](crate::fixture::FixtureSpec::removable_by_path), which
    /// takes it.
    ///
    /// The path of the cache directory counts as the longest of three: the path as it is
    /// spelled; every pathname the system works on while it expands a symbolic link in it, which
    /// is the content of the link and what is left of the path after the link; and the path as it
    /// resolves, with every link expanded (on Windows the verbatim form, `\\?\C:\...`, which is
    /// four bytes longer than the drive form). The spelling counts as it is: on Unix the system is
    /// handed the text of the root, and counts every `.` name and repeated separator in it, which
    /// [`std::path::absolute`] would drop, so `/dir//cache` is a byte longer than `/dir/cache`
    /// and `/dir/./cache` two; a relative root is the current directory and the text written
    /// after it. (On Windows [`std::path::absolute`] is the spelling the system sees: see the
    /// resolver.) The system counts the expansion of a link against `PATH_MAX`: macOS refuses a
    /// path when the content of a link and the rest of the path are together longer than that. A
    /// root reached through a short link to a deep directory, through a link whose content goes
    /// down a long way and comes back up with `..`, or through a short link to a long path that
    /// ends in a link back to a short directory, can be short as it is written and short as it
    /// resolves, and still make the system work on a pathname much longer than either to reach
    /// what is below it. The names of the root that do not exist yet count as they are written,
    /// below the longest ancestor that does, so the cache directory need not exist.
    ///
    /// # Errors
    ///
    /// Returns why the path cannot be resolved: the root is empty, a relative root has no current
    /// directory to start from, a name on the way cannot be searched, a name that exists is not
    /// a folder (the root itself included, and a link that leads to a file), a symbolic link on
    /// the way leads to a name that is not there, or the links loop or are too many. A name that
    /// does not exist, with no link leading to it, is not an error: the cache makes it. A fixture
    /// is never cached in a cache whose path cannot be resolved: see
    /// [`Fixtures::is_cacheable`](crate::fixture::Fixtures::is_cacheable).
    pub fn longest_entry_path_bytes(&self) -> io::Result<u64> {
        let resolved = resolve(&self.root)?;
        let root = resolved.longest.max(resolved.path.as_os_str().len());
        Ok(u64::try_from(root + 1 + longest_name_bytes()).unwrap_or(u64::MAX))
    }

    /// Where the entry for `plan` lives.
    #[must_use]
    pub fn entry_path(&self, plan: &Plan) -> PathBuf {
        self.root.join(entry_name(name_digits(plan.spec_hash())))
    }

    /// Returns the master fixture for `spec`: the cached entry if it verifies, a freshly
    /// generated one otherwise.
    ///
    /// # Errors
    ///
    /// Returns why the spec is invalid, or why generating, sealing, or publishing failed.
    pub fn materialize(
        &self,
        spec: &FixtureSpec,
        options: &MaterializeOptions,
    ) -> Result<Materialized, FixtureError> {
        let spec = options
            .seed
            .map_or_else(|| spec.clone(), |seed| spec.with_seed(seed));
        let plan = Arc::new(Plan::new(&spec)?);
        let entry = self.entry_path(&plan);
        let io_error = |context: &str, source: io::Error| FixtureError::Io {
            context: context.to_owned(),
            source,
        };

        fs::create_dir_all(&self.root)
            .map_err(|source| io_error("cannot create the fixture cache directory", source))?;

        let mut regenerated_because = None;
        match verify_master(&entry, &plan, options.verify) {
            Ok((marker, verified)) => {
                return Ok(Materialized {
                    root: entry,
                    marker,
                    cache_hit: true,
                    regenerated_because: None,
                    generation: None,
                    verified: Some(verified),
                    plan,
                });
            }
            Err(IntegrityFailure::Absent) => {}
            Err(failure) => {
                regenerated_because = Some(failure.to_string());
                remove_tree(&entry)
                    .map_err(|source| io_error("cannot remove the unusable cache entry", source))?;
            }
        }

        let partial = {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            self.root.join(partial_name(
                name_digits(plan.spec_hash()),
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed),
            ))
        };
        let guard = TreeGuard::new(&partial);
        let capabilities = Capabilities::probe(&self.root)
            .map_err(|source| io_error("cannot probe the cache file system", source))?;
        let report = generate(&plan, &partial, &capabilities, options.generate)?;
        let marker = Marker::new(&plan, Role::Master, &capabilities, &report);
        write_marker(&partial, &marker)
            .map_err(|source| io_error("cannot write the ownership marker", source))?;

        if fs::rename(&partial, &entry).is_ok() {
            let _ = guard.keep();
        } else {
            // Another process published the same entry first, or a leftover blocks the name.
            if let Ok((theirs, verified)) = verify_master(&entry, &plan, options.verify) {
                drop(guard);
                return Ok(Materialized {
                    root: entry,
                    marker: theirs,
                    cache_hit: true,
                    regenerated_because,
                    generation: None,
                    verified: Some(verified),
                    plan,
                });
            }
            remove_tree(&entry)
                .map_err(|source| io_error("cannot remove the blocking cache entry", source))?;
            fs::rename(&partial, &entry)
                .map_err(|source| io_error("cannot publish the generated fixture", source))?;
            let _ = guard.keep();
        }
        Ok(Materialized {
            root: entry,
            marker,
            cache_hit: false,
            regenerated_because,
            generation: Some(report),
            verified: None,
            plan,
        })
    }
}

/// The cache directory for a `CARGO_TARGET_DIR` value: `excise-fixtures.noindex` inside it, or
/// inside the `target` directory of this workspace when the variable is not set.
pub(crate) fn default_root(target_dir: Option<std::ffi::OsString>) -> PathBuf {
    let target = target_dir.map_or_else(
        || {
            let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
            manifest
                .ancestors()
                .nth(2)
                .unwrap_or(manifest)
                .join("target")
        },
        PathBuf::from,
    );
    target.join(CACHE_DIR_NAME)
}

/// A master fixture in the cache.
#[derive(Debug, Clone)]
pub struct Materialized {
    /// The fixture root, which carries the ownership marker. Treat it as read-only.
    pub root: PathBuf,
    /// The marker: what was generated, the hashes, and the capabilities of the file system.
    pub marker: Marker,
    /// Whether an existing entry was reused, which skips generation.
    pub cache_hit: bool,
    /// Why an existing entry was discarded and generated again, if it was.
    pub regenerated_because: Option<String>,
    /// What generation did, or `None` on a cache hit.
    pub generation: Option<GenerateReport>,
    /// What verification did, or `None` when the entry was just generated.
    pub verified: Option<Verified>,
    pub(crate) plan: Arc<Plan>,
}

impl Materialized {
    /// The plan of the fixture: its manifest.
    #[must_use]
    pub fn plan(&self) -> &Plan {
        &self.plan
    }

    /// The manifest hash of the whole plan: the same on every machine.
    #[must_use]
    pub fn manifest_sha256(&self) -> &str {
        &self.marker.manifest_sha256
    }

    /// The time generation took, or `None` on a cache hit.
    #[must_use]
    pub fn generation_time(&self) -> Option<Duration> {
        self.generation.as_ref().map(|report| report.elapsed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `.partial-`, 16 hexadecimal digits, `-`, a process number of at most 10 digits (`u32`), `-`,
    /// and a call count of at most 20 (`u64`).
    const LONGEST_NAME: usize = ".partial-".len() + 16 + 1 + 10 + 1 + 20;

    #[test]
    fn no_name_the_cache_gives_a_directory_is_longer_than_the_longest_it_counts() {
        let digits = "0123456789abcdef";
        assert_eq!(LONGEST_NAME, 57);
        assert_eq!(longest_name_bytes(), LONGEST_NAME);

        // An entry, a partial one now, and a partial one at the largest process number and call
        // count: the last is the longest, and none is longer than what the cache counts.
        assert_eq!(entry_name(digits), format!("{digits}-{GENERATOR_VERSION}"));
        assert!(entry_name(digits).len() < LONGEST_NAME);
        assert!(partial_name(digits, std::process::id(), 0).len() <= LONGEST_NAME);
        assert_eq!(
            partial_name(digits, u32::MAX, u64::MAX).len(),
            LONGEST_NAME,
            "{}",
            partial_name(digits, u32::MAX, u64::MAX)
        );
    }
}
