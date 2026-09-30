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
    spec::FixtureSpec,
    tree::{TreeGuard, remove_tree},
};

/// The name of the cache directory below the target directory.
pub const CACHE_DIR_NAME: &str = "excise-fixtures.noindex";

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

    /// Where the entry for `plan` lives.
    #[must_use]
    pub fn entry_path(&self, plan: &Plan) -> PathBuf {
        self.root
            .join(format!("{}-{GENERATOR_VERSION}", &plan.spec_hash()[..16]))
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
            self.root.join(format!(
                ".partial-{}-{}-{}",
                &plan.spec_hash()[..16],
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
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
