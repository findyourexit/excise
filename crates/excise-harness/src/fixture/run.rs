//! Disposable per-run copies of a fixture, and the facade a runner calls.
//!
//! A cached master is never handed to something that changes it. A run that mutates or deletes
//! works on a [`RunCopy`]: the same plan generated fresh into a directory of its own, carrying
//! its own marker. Generating from the plan rather than copying files makes the copy exact by
//! construction (hostile modes, special names, and links included) and costs the generation time
//! of the fixture. A run that only reads can use the master directly.
//!
//! A copy removes itself, restoring permissions first, when it is dropped. [`RunCopy::keep`]
//! opts out, for a run that must leave its fixture behind for inspection.
//!
//! # Volumes
//!
//! The master holds only the empty mount-point directory of a `volume` part. Attaching the volume
//! is a privileged, explicit step of a run copy: [`RunCopy::attach_volumes`] needs a
//! [`PrivilegedOptIn`], creates a size-limited volume at each mount point, and writes the part's
//! files onto it. The copy detaches its volumes before it removes its tree, and never removes
//! anything across a mount boundary.

use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use crate::fixture::{
    cache::{FixtureCache, MaterializeOptions, Materialized},
    caps::Capabilities,
    error::FixtureError,
    generate::{GenerateError, GenerateOptions, GenerateReport, generate, generate_part},
    marker::{Marker, Role, write_marker},
    oracle::{Oracle, OracleError},
    plan::Plan,
    rng::{SplitMix64, derive_seed},
    spec::{FixtureSpec, SpecError},
    tree::{TreeGuard, remove_tree},
    volume::{PrivilegedOptIn, Volume, VolumeSpec},
};

/// A disposable copy of a fixture for one run.
///
/// The fields drop in order: volumes detach first, then their scratch directory goes, then the
/// tree.
#[derive(Debug)]
pub struct RunCopy {
    volumes: Vec<Volume>,
    scratch: Option<TreeGuard>,
    guard: TreeGuard,
    root: PathBuf,
    plan: Arc<Plan>,
    marker: Marker,
    options: GenerateOptions,
}

impl RunCopy {
    /// Generates `plan` into a new uniquely named directory below `parent`, which must exist.
    pub(crate) fn create(
        plan: Arc<Plan>,
        parent: &Path,
        options: GenerateOptions,
    ) -> Result<Self, FixtureError> {
        static NEXT: AtomicU64 = AtomicU64::new(0);

        let io_error = |context: &str, source: io::Error| FixtureError::Io {
            context: context.to_owned(),
            source,
        };
        let capabilities = Capabilities::probe(parent)
            .map_err(|source| io_error("cannot probe the run directory's file system", source))?;
        for _ in 0..8 {
            let root = parent.join(format!(
                "{}-{}-{}",
                plan.spec().id,
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let report = match generate(&plan, &root, &capabilities, options) {
                Ok(report) => report,
                // The name is taken by something that is not ours: never remove it.
                Err(GenerateError::Root { source, .. })
                    if source.kind() == io::ErrorKind::AlreadyExists =>
                {
                    continue;
                }
                Err(error) => {
                    // The root was ours and generation stopped part-way.
                    let _ = remove_tree(&root);
                    return Err(error.into());
                }
            };
            let guard = TreeGuard::new(&root);
            let marker = Marker::new(&plan, Role::RunCopy, &capabilities, &report);
            write_marker(&root, &marker)
                .map_err(|source| io_error("cannot write the ownership marker", source))?;
            return Ok(Self {
                volumes: Vec::new(),
                scratch: None,
                guard,
                root,
                plan,
                marker,
                options,
            });
        }
        Err(io_error(
            "cannot find an unused name for the run copy",
            io::Error::from(io::ErrorKind::AlreadyExists),
        ))
    }

    /// The fixture root, which carries the ownership marker: what `excise` is pointed at.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The marker of this copy.
    #[must_use]
    pub const fn marker(&self) -> &Marker {
        &self.marker
    }

    /// The plan the copy was generated from: its manifest.
    #[must_use]
    pub fn plan(&self) -> &Plan {
        &self.plan
    }

    /// Walks the copy as it is now. After a run this shows what the run did, and while volumes
    /// are attached it walks across their mount boundaries.
    ///
    /// # Errors
    ///
    /// Returns the error of the walk.
    pub fn oracle(&self) -> Result<Oracle, OracleError> {
        Oracle::collect(&self.root)
    }

    /// The mount points that currently have a volume attached.
    #[must_use]
    pub fn attached_mount_points(&self) -> Vec<&Path> {
        self.volumes.iter().map(Volume::mount_point).collect()
    }

    /// Attaches a size-limited volume at the mount point of every `volume` part and writes the
    /// part's files onto it, so the copy has a real mount boundary. Returns the number of
    /// volumes attached; a plan without volume parts attaches none.
    ///
    /// # Errors
    ///
    /// Returns why a volume could not be created or filled. Volumes attached before the failure
    /// stay attached until the copy drops.
    pub fn attach_volumes(&mut self, opt_in: PrivilegedOptIn) -> Result<usize, FixtureError> {
        let plan = Arc::clone(&self.plan);
        if plan.volumes().is_empty() {
            return Ok(0);
        }
        let io_error = |context: String, source: io::Error| FixtureError::Io { context, source };
        // The images live beside the fixture root, never inside it, so a scan of the root does
        // not see them.
        let scratch = self.root.with_extension("volumes");
        fs::create_dir(&scratch).map_err(|source| {
            io_error(
                "cannot create the volume scratch directory".to_owned(),
                source,
            )
        })?;
        self.scratch = Some(TreeGuard::new(&scratch));

        for (index, plan_volume) in plan.volumes().iter().enumerate() {
            let mount = plan_volume.mount.to_path_buf(&self.root);
            let work = scratch.join(index.to_string());
            fs::create_dir(&work).map_err(|source| {
                io_error(format!("cannot create `{}`", work.display()), source)
            })?;
            let spec = VolumeSpec::new(plan_volume.size_mib, format!("Fx{index}"))?;
            let volume = Volume::attach(opt_in, &spec, &work, &mount)?;
            for name in plan_volume.file_names() {
                let path = mount.join(String::from_utf8_lossy(&name).into_owned());
                let mut content = vec![0_u8; usize::try_from(plan_volume.file_bytes).unwrap_or(0)];
                SplitMix64::new(derive_seed(plan.seed(), &name)).fill(&mut content);
                fs::write(&path, content).map_err(|source| {
                    io_error(format!("cannot write `{}`", path.display()), source)
                })?;
            }
            self.volumes.push(volume);
        }
        Ok(plan.volumes().len())
    }

    /// Removes the top-level entry `part` and generates it again, so that a deletion scenario
    /// that has run in this copy can run again without a new copy of the whole fixture. A part
    /// is named by its `root` in the spec.
    ///
    /// # Errors
    ///
    /// Returns an error if the plan has no such part, or if removing or generating fails.
    pub fn regenerate(&self, part: &str) -> Result<GenerateReport, FixtureError> {
        let known: Vec<String> = self
            .plan
            .top_level_names()
            .iter()
            .map(|name| String::from_utf8_lossy(name).into_owned())
            .collect();
        if !known.iter().any(|name| name == part) {
            return Err(FixtureError::UnknownPart {
                part: part.to_owned(),
                known,
            });
        }
        remove_tree(&self.root.join(part)).map_err(|source| FixtureError::Io {
            context: format!("cannot remove `{part}` before regenerating it"),
            source,
        })?;
        Ok(generate_part(
            &self.plan,
            &self.root,
            part,
            &self.marker.capabilities,
            self.options,
        )?)
    }

    /// Gives up ownership: volumes are detached, and the tree is left in place and its path
    /// returned.
    #[must_use]
    pub fn keep(self) -> PathBuf {
        let Self {
            volumes,
            scratch,
            guard,
            root,
            ..
        } = self;
        drop(volumes);
        drop(scratch);
        let _ = guard.keep();
        root
    }

    /// Detaches any volumes and removes the copy now, and reports what went wrong.
    ///
    /// # Errors
    ///
    /// Returns the error of removing the tree.
    pub fn remove(self) -> io::Result<()> {
        let Self {
            volumes,
            scratch,
            guard,
            ..
        } = self;
        drop(volumes);
        drop(scratch);
        guard.remove()
    }
}

impl Materialized {
    /// A disposable copy of this fixture in a new directory below `parent`, generated fresh and
    /// carrying its own marker. The master is untouched.
    ///
    /// # Errors
    ///
    /// Returns why generating the copy failed.
    pub fn run_copy(&self, parent: &Path) -> Result<RunCopy, FixtureError> {
        RunCopy::create(Arc::clone(&self.plan), parent, GenerateOptions::default())
    }
}

/// Where the specs live and where fixtures are cached: the one thing a runner needs to turn a
/// scenario's `fixture` id into a root.
#[derive(Debug, Clone)]
pub struct Fixtures {
    specs_dir: PathBuf,
    cache: FixtureCache,
    options: MaterializeOptions,
}

impl Fixtures {
    /// The specs this crate ships, cached in the workspace `target` directory (or
    /// `CARGO_TARGET_DIR`).
    #[must_use]
    pub fn bundled() -> Self {
        Self::new(FixtureSpec::bundled_dir(), FixtureCache::in_target_dir())
    }

    /// Specs from `specs_dir`, cached in `cache`. Tests pass a temporary directory for both.
    #[must_use]
    pub fn new(specs_dir: impl Into<PathBuf>, cache: FixtureCache) -> Self {
        Self {
            specs_dir: specs_dir.into(),
            cache,
            options: MaterializeOptions::default(),
        }
    }

    /// The same, materializing with `options` (a seed, a thread count, a verification level).
    #[must_use]
    pub fn with_options(mut self, options: MaterializeOptions) -> Self {
        self.options = options;
        self
    }

    /// The cache.
    #[must_use]
    pub const fn cache(&self) -> &FixtureCache {
        &self.cache
    }

    /// The ids of the specs this facade can load, sorted.
    ///
    /// # Errors
    ///
    /// Returns the error of reading the specs directory.
    pub fn ids(&self) -> io::Result<Vec<String>> {
        FixtureSpec::ids_in(&self.specs_dir)
    }

    /// Loads and validates the spec `id`.
    ///
    /// # Errors
    ///
    /// Returns why the spec cannot be loaded.
    pub fn spec(&self, id: &str) -> Result<FixtureSpec, SpecError> {
        FixtureSpec::load(&self.specs_dir, id)
    }

    /// The cached master of the spec `id`, generated if it is not there or does not verify. Treat
    /// it as read-only.
    ///
    /// # Errors
    ///
    /// Returns why the spec cannot be loaded or the fixture cannot be generated.
    pub fn master(&self, id: &str) -> Result<Materialized, FixtureError> {
        self.cache.materialize(&self.spec(id)?, &self.options)
    }

    /// A disposable copy of the fixture `id` in a new directory below `run_parent`, which must
    /// exist. This is what a runner calls: the returned root is marked and fresh, and the copy is
    /// removed when it drops.
    ///
    /// # Errors
    ///
    /// Returns why the spec cannot be loaded or the copy cannot be generated.
    pub fn run_copy(&self, id: &str, run_parent: &Path) -> Result<RunCopy, FixtureError> {
        let spec = self.spec(id)?;
        let spec = self
            .options
            .seed
            .map_or_else(|| spec.clone(), |seed| spec.with_seed(seed));
        let plan = Arc::new(Plan::new(&spec)?);
        RunCopy::create(plan, run_parent, self.options.generate)
    }
}
