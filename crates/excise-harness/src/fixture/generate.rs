//! The executor: turns a [`Plan`] into a tree on disk.
//!
//! Creation is parallel with a bounded number of `std` threads. The plan is cut into contiguous
//! chunks of its canonical order; each worker takes chunks from a shared counter and creates the
//! entries of its chunk through a [`Cursor`], which keeps the directory it last worked in open.
//! Any worker may create a directory another worker needs (creation is idempotent), so chunks
//! are independent and may run in any order.
//!
//! Three things cannot be independent and run afterwards, on one thread:
//!
//! 1. hard links and clones, which need their source file to exist;
//! 2. permission overrides, applied deepest entry first so a restricted directory is never
//!    closed before its contents are done.
//!
//! All names are resolved one component at a time relative to an open directory, never as a
//! whole path, so a fixture deeper than `PATH_MAX` is generated like any other.

use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, Write as _},
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use thiserror::Error;

use crate::fixture::{
    NodeKind,
    caps::{Capabilities, Capability},
    path::RelPath,
    plan::{ManifestEntry, Plan},
    rng::{SplitMix64, derive_seed},
    sys::Dir,
};

/// The most worker threads the generator will start.
pub const MAX_THREADS: usize = 16;

/// The largest number of threads used when the caller does not choose.
const DEFAULT_THREAD_CAP: usize = 8;

/// The size of a worker's write buffer.
const BUFFER_BYTES: usize = 64 * 1024;

/// How to generate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GenerateOptions {
    /// The number of worker threads, `1..=`[`MAX_THREADS`]. `None` uses the available
    /// parallelism, capped at 8.
    pub threads: Option<usize>,
}

impl GenerateOptions {
    fn thread_count(self, work: usize) -> usize {
        let wanted = self.threads.unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map_or(1, std::num::NonZeroUsize::get)
                .min(DEFAULT_THREAD_CAP)
        });
        wanted.clamp(1, MAX_THREADS).min(work.max(1))
    }
}

/// What a generation did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenerateReport {
    /// The entries created below the root.
    pub created: u64,
    /// The entries skipped, counted by the capability they needed.
    pub skipped: BTreeMap<Capability, u64>,
    /// The worker threads used.
    pub threads: usize,
    /// The wall-clock time of the whole generation.
    pub elapsed: Duration,
}

/// Why a generation failed.
#[derive(Debug, Error)]
pub enum GenerateError {
    /// The fixture root could not be created.
    #[error("cannot create the fixture root `{}`: {source}", path.display())]
    Root {
        /// The root that was to be created.
        path: PathBuf,
        /// The underlying error.
        source: io::Error,
    },
    /// An entry could not be created.
    #[error("cannot create {what} `{path}`: {source}")]
    Entry {
        /// What was being created.
        what: &'static str,
        /// The fixture-relative path, escaped for display.
        path: RelPath,
        /// The underlying error.
        source: io::Error,
    },
}

/// Creates the tree of `plan` at `root`, which must not exist yet. Its parent must.
///
/// Entries whose capability is missing from `capabilities` are skipped and counted. The ownership
/// marker is not written here: a root without one is not yet a usable fixture, and the caller
/// seals the fixture once generation has succeeded.
///
/// # Errors
///
/// Returns the first entry that could not be created. The partly generated tree stays where it
/// is for the caller to remove.
pub fn generate(
    plan: &Plan,
    root: &Path,
    capabilities: &Capabilities,
    options: GenerateOptions,
) -> Result<GenerateReport, GenerateError> {
    let started = Instant::now();
    let root_dir = Dir::create_root(root).map_err(|source| GenerateError::Root {
        path: root.to_path_buf(),
        source,
    })?;
    execute(
        &root_dir,
        plan.entries().iter(),
        plan.seed(),
        capabilities,
        options,
        started,
    )
}

/// Creates the top-level entry `part` of `plan` inside the existing fixture root `root`, which
/// must not contain it: the entries of one part, and only those. A runner that has deleted its
/// victim uses this, after removing what is left of the part, to get a fresh one without a new
/// copy of the whole fixture.
///
/// # Errors
///
/// Returns the first entry that could not be created (including one that already exists).
pub fn generate_part(
    plan: &Plan,
    root: &Path,
    part: &str,
    capabilities: &Capabilities,
    options: GenerateOptions,
) -> Result<GenerateReport, GenerateError> {
    let started = Instant::now();
    let root_dir = Dir::open_root(root).map_err(|source| GenerateError::Root {
        path: root.to_path_buf(),
        source,
    })?;
    let entries = plan
        .entries()
        .iter()
        .filter(|entry| entry.path.components().next() == Some(part.as_bytes()));
    execute(
        &root_dir,
        entries,
        plan.seed(),
        capabilities,
        options,
        started,
    )
}

/// Runs the phases over `entries`, in canonical order, below the open directory `root`.
fn execute<'a>(
    root_dir: &Dir,
    entries: impl Iterator<Item = &'a ManifestEntry>,
    seed: u64,
    capabilities: &Capabilities,
    options: GenerateOptions,
    started: Instant,
) -> Result<GenerateReport, GenerateError> {
    let mut schedule = Schedule::default();
    for entry in entries {
        schedule.add(entry, capabilities);
    }

    let threads = options.thread_count(schedule.parallel.len());
    run_parallel(root_dir, &schedule.parallel, seed, threads)?;
    run_dependent(root_dir, &schedule)?;
    run_modes(root_dir, &schedule.modes)?;

    Ok(GenerateReport {
        created: (schedule.parallel.len() + schedule.links.len() + schedule.clones.len()) as u64,
        skipped: schedule.skipped,
        threads,
        elapsed: started.elapsed(),
    })
}

/// The plan's entries sorted into the phases they run in.
#[derive(Default)]
struct Schedule<'a> {
    /// Directories, files, and symbolic links that depend on nothing.
    parallel: Vec<&'a ManifestEntry>,
    /// Hard links, with the path of the file each links to.
    links: Vec<(&'a ManifestEntry, &'a RelPath)>,
    clones: Vec<&'a ManifestEntry>,
    /// Entries with a permission override.
    modes: Vec<&'a ManifestEntry>,
    skipped: BTreeMap<Capability, u64>,
    /// The first member of every hard-link group seen so far, by group number.
    link_sources: BTreeMap<u32, &'a RelPath>,
}

impl<'a> Schedule<'a> {
    fn add(&mut self, entry: &'a ManifestEntry, capabilities: &Capabilities) {
        if !entry.is_created_with(capabilities) {
            if let Some(capability) = entry.requires {
                *self.skipped.entry(capability).or_default() += 1;
            }
            return;
        }
        if entry.mode.is_some() {
            self.modes.push(entry);
        }
        if entry.clone_of.is_some() {
            self.clones.push(entry);
            return;
        }
        if let Some(group) = entry.link_group {
            // Canonical order puts the group's file before its other names.
            match self.link_sources.get(&group) {
                Some(source) => {
                    self.links.push((entry, source));
                    return;
                }
                None => {
                    self.link_sources.insert(group, &entry.path);
                }
            }
        }
        self.parallel.push(entry);
    }
}

/// Runs the independent entries on `threads` workers.
fn run_parallel(
    root: &Dir,
    entries: &[&ManifestEntry],
    seed: u64,
    threads: usize,
) -> Result<(), GenerateError> {
    let chunk_size = (entries.len() / (threads * 4)).clamp(64, 4096);
    let chunks: Vec<&[&ManifestEntry]> = entries.chunks(chunk_size).collect();
    if threads == 1 {
        let mut worker = Worker::new(root, seed);
        return chunks.iter().try_for_each(|chunk| worker.run(chunk));
    }

    let next = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    let first_error = Mutex::new(None);
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                let mut worker = Worker::new(root, seed);
                while !failed.load(Ordering::Relaxed) {
                    let Some(chunk) = chunks.get(next.fetch_add(1, Ordering::Relaxed)) else {
                        break;
                    };
                    if let Err(error) = worker.run(chunk) {
                        failed.store(true, Ordering::Relaxed);
                        if let Ok(mut slot) = first_error.lock() {
                            slot.get_or_insert(error);
                        }
                        break;
                    }
                }
            });
        }
    });
    match first_error.into_inner() {
        Ok(Some(error)) => Err(error),
        _ => Ok(()),
    }
}

/// One worker's state: its cursor, its write buffer, and the fixture seed.
struct Worker<'r> {
    cursor: Cursor<'r>,
    buffer: Vec<u8>,
    seed: u64,
}

impl<'r> Worker<'r> {
    fn new(root: &'r Dir, seed: u64) -> Self {
        Self {
            cursor: Cursor {
                root,
                current: None,
            },
            buffer: Vec::new(),
            seed,
        }
    }

    fn run(&mut self, chunk: &[&ManifestEntry]) -> Result<(), GenerateError> {
        for entry in chunk {
            self.create(entry)
                .map_err(|(what, source)| GenerateError::Entry {
                    what,
                    path: entry.path.clone(),
                    source,
                })?;
        }
        Ok(())
    }

    fn create(&mut self, entry: &ManifestEntry) -> Result<(), (&'static str, io::Error)> {
        let name = entry.path.file_name().unwrap_or_default();
        let parent = self
            .cursor
            .enter(entry.path.parent_bytes())
            .map_err(|error| ("its parent directory", error))?;
        match entry.kind {
            NodeKind::Directory => match parent.create_dir(name) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
                Err(error) => Err(("directory", error)),
            },
            NodeKind::File => {
                let mut file = parent.create_file(name).map_err(|error| ("file", error))?;
                let key = derive_seed(self.seed, entry.path.as_bytes());
                write_content(
                    &mut file,
                    entry.size,
                    entry.sparse_data,
                    key,
                    &mut self.buffer,
                )
                .map_err(|error| ("file contents", error))
            }
            NodeKind::Symlink => {
                let target = entry
                    .target
                    .as_ref()
                    .map(crate::fixture::path::LinkTarget::as_bytes)
                    .unwrap_or_default();
                parent
                    .symlink(name, target)
                    .map_err(|error| ("symbolic link", error))
            }
            NodeKind::Other => Err((
                "entry",
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "a plan never creates special files",
                ),
            )),
        }
    }
}

/// Writes `size` bytes of deterministic content, or, for a sparse file, `sparse_data` bytes of it
/// followed by a hole up to `size`.
fn write_content(
    file: &mut File,
    size: u64,
    sparse_data: Option<u64>,
    key: u64,
    buffer: &mut Vec<u8>,
) -> io::Result<()> {
    let mut remaining = sparse_data.unwrap_or(size);
    let mut stream = SplitMix64::new(key);
    while remaining > 0 {
        let chunk =
            usize::try_from(remaining).map_or(BUFFER_BYTES, |bytes| bytes.min(BUFFER_BYTES));
        buffer.resize(chunk, 0);
        stream.fill(buffer);
        file.write_all(buffer)?;
        remaining -= chunk as u64;
    }
    if sparse_data.is_some() {
        file.set_len(size)?;
    }
    Ok(())
}

/// Keeps the directory a worker last worked in open, so the run of entries that share a parent
/// (which canonical order makes contiguous) costs one path walk rather than one per entry.
struct Cursor<'r> {
    root: &'r Dir,
    /// The parent path and handle of the last entry.
    current: Option<(Vec<u8>, Dir)>,
}

impl Cursor<'_> {
    /// The directory `parent` (a `/`-separated relative path, empty for the root), creating any
    /// missing directories on the way.
    fn enter(&mut self, parent: &[u8]) -> io::Result<&Dir> {
        if parent.is_empty() {
            return Ok(self.root);
        }
        let stale = !matches!(&self.current, Some((path, _)) if path == parent);
        if stale {
            let mut components = parent.split(|byte| *byte == b'/');
            let mut directory = match components.next() {
                Some(first) => self.root.open_or_create_dir(first)?,
                None => return Ok(self.root),
            };
            for component in components {
                directory = directory.open_or_create_dir(component)?;
            }
            self.current = Some((parent.to_vec(), directory));
        }
        match &self.current {
            Some((_, directory)) => Ok(directory),
            None => Err(io::Error::other("the cursor lost its directory")),
        }
    }
}

/// Opens an existing directory of the fixture by walking `path` one component at a time.
pub(crate) fn open_existing(root: &Dir, path: &[u8]) -> io::Result<Dir> {
    let mut components = path
        .split(|byte| *byte == b'/')
        .filter(|part| !part.is_empty());
    let Some(first) = components.next() else {
        return root.reopen();
    };
    let mut directory = root.open_dir(first)?;
    for component in components {
        directory = directory.open_dir(component)?;
    }
    Ok(directory)
}

/// Creates the hard links and clones, which need their source file to exist.
fn run_dependent(root: &Dir, schedule: &Schedule<'_>) -> Result<(), GenerateError> {
    let fail = |what, path: &RelPath, source| GenerateError::Entry {
        what,
        path: path.clone(),
        source,
    };
    for (entry, source) in &schedule.links {
        let link = || -> io::Result<()> {
            let source_dir = open_existing(root, source.parent_bytes())?;
            let target_dir = open_existing(root, entry.path.parent_bytes())?;
            source_dir.hard_link(
                source.file_name().unwrap_or_default(),
                &target_dir,
                entry.path.file_name().unwrap_or_default(),
            )
        };
        link().map_err(|error| fail("hard link", &entry.path, error))?;
    }
    for entry in &schedule.clones {
        let Some(source) = &entry.clone_of else {
            continue;
        };
        let clone = || -> io::Result<()> {
            let source_dir = open_existing(root, source.parent_bytes())?;
            let target_dir = open_existing(root, entry.path.parent_bytes())?;
            target_dir.clone_file(
                entry.path.file_name().unwrap_or_default(),
                &source_dir,
                source.file_name().unwrap_or_default(),
            )
        };
        clone().map_err(|error| fail("clone", &entry.path, error))?;
    }
    Ok(())
}

/// Applies the permission overrides, deepest entries first.
fn run_modes(root: &Dir, entries: &[&ManifestEntry]) -> Result<(), GenerateError> {
    for entry in entries.iter().rev() {
        let Some(mode) = entry.mode else {
            continue;
        };
        let change = || -> io::Result<()> {
            open_existing(root, entry.path.parent_bytes())?
                .chmod(entry.path.file_name().unwrap_or_default(), mode)
        };
        change().map_err(|source| GenerateError::Entry {
            what: "permission mask",
            path: entry.path.clone(),
            source,
        })?;
    }
    Ok(())
}
