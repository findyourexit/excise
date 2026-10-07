use std::collections::{BTreeSet, HashSet};
use std::error::Error as StdError;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use thiserror::Error;

use super::directory_summary::{
    DirectorySummaryRunError, reduce_path_observation_run, visit_directory_summaries,
};
use super::folder_digest::FolderDigest;
use super::identity_observation::{
    IdentityObservation, IdentityObservationRunError, IdentityReductionError,
    append_identity_observation, compare_identity_observations, reduce_identity_observations,
};
use super::manifest::{ManifestError, RunManifestEntry, ScanManifest};
use super::page::{
    MATERIALIZE_STEPS, PageIndexError, PageRequest, ProvisionalPage, ScanPage, ScanPageEntry,
    ScanPageError, materialize_child_queries, read_page_entry, visit_child_query_entries,
    visit_child_query_page_entries,
};
use super::path_catalog::{
    PathCatalogEntry, PathCatalogError, build_path_catalog, read_path_catalog_entry,
};
use super::path_observation::{PathObservationRunError, append_path_observation};
use super::path_reducer::{
    Coverage, PathEntryKind, PathObservation, SummaryMetrics, UnreadableDirectories,
};
use super::run_file::{DetachedRun, RunDescriptor, RunError, RunKind, RunWriter, SealedRun};
use super::run_merge::{RunMergeError, merge_sorted_runs};
use crate::scan_coordinator::{RelativePath, ScanGeneration};
use crate::scan_coordinator::{WorkKind, WorkLease};
use crate::scan_session::{ScanGenerationState, ScanSessionId};
use crate::scan_store::storage::ScanStoreStorage;
use crate::temporary_storage::TemporaryStorage;

/// Each tier folds this many similarly sized runs before passing one run upward.
/// This bounds reader fan-in without repeatedly rewriting the full scan on every batch.
const MAX_ACTIVE_INPUT_RUNS: usize = 8;
/// Eight to the twenty-second power exceeds the addressable run-record count.
const MAX_RUN_LEVELS: usize = 22;
const RUN_BLOCK_BYTES: usize = 64 * 1024;

/// Scanner workers emit bounded batches. Keep this input cap below the event
/// channel's worst-case payload so the owner never needs an unbounded sort.
pub(crate) const MAX_OBSERVATIONS_PER_BATCH: usize = 128;

/// Bounds the per-session set of directories the scanner could not open or
/// list (`ScanStore::record_unreadable_directory`). A real permission
/// boundary (a locked-down system tree, a handful of misconfigured
/// directories) produces at most dozens to a few hundred such paths; this
/// cap is two orders of magnitude past that. At the cap, even a worst-case
/// deeply nested path keeps the set a low single-digit number of megabytes:
/// a small fraction of the 25 percent of the default 512 MiB process budget
/// `docs/architecture/overview.md` reserves for working data. Past the cap,
/// `ScanStore` stops tracking individual paths and instead reports every
/// folder `Uncertain`, the same conservative direction `unrecorded_path_count`
/// already takes for the root alone when a fact cannot be retained.
pub(crate) const MAX_TRACKED_UNREADABLE_DIRECTORIES: usize = 4096;

/// Runs retained for as long as a generation is published or summary-only: the concrete
/// entries (`child_queries`), their compact parent/name lookup (`path_catalog`), physical-
/// byte dedup facts (`identity_index`), and per-folder roll-ups (`directory_summary_index`).
/// `publish` replaces all of a prior generation's runs together with the next one's, so this
/// count is the scan store's descriptor floor once a scan completes, fixed regardless of how
/// many entries or directories were scanned. Before that, the same bound this module already
/// enforces on admitted input (`MAX_ACTIVE_INPUT_RUNS`/`MAX_RUN_LEVELS`) is what limits a
/// generation in progress: a sealed run holds no descriptor merely by existing in the
/// scanner-to-owner channel or an unmerged level (`SealedRun::ensure_resident`, `run_file.rs`
/// derives why), so neither a deep admission backlog nor a wide tree adds descriptors by
/// itself. The scanner's directory-relative walk (`runtime/scanner.rs`) adds descriptors of
/// its own, beside this budget rather than inside it, and holds none per directory level: a
/// worker reaches a folder by opening each name from the scan root's handle in turn and
/// closing the handle above it as it goes, so a path at any depth takes at most two at once.
/// The folder it then lists and measures takes two more, its own handle and the listing's,
/// and an entry's metadata (`fstatat` against the listing's handle) opens none. Re-checking
/// the folder's path during the listing repeats the first step on top of those two, so a
/// worker's peak is four, whatever the depth, path length, or tree size.
const RESIDENT_PUBLISHED_RUNS: usize = 4;

/// The stages `ScanStore::reduce_active_generation` reports: the path and identity families
/// merged, then the directory summaries, the path catalog, and the allocation contributions
/// reduced from them.
const REDUCE_STAGES: u8 = 5;

/// Scanner batches sealed but not yet admitted, capped independently of the event channel's
/// own capacity (`event_buffer`, `config.rs`), which also bounds memory and latency for every
/// kind of worker event, not just these two. Removing the per-run durable sync let the
/// scanner outrun admission more, growing this backlog's `Vec<ScannedEntry>` and run-file
/// memory past the pre-removal baseline on `tiny-files-250k` (20.1-23.4 MB peak footprint
/// against 17.7-20.9 MB before); 16 keeps it within that baseline again (`cargo xtask
/// bench-e2e --baseline bf63697 --fixture tiny-files-250k`), with ample headroom below the
/// channel's own default 256-event capacity.
pub(crate) const MAX_INFLIGHT_SCAN_BATCHES: usize = 16;

/// Shared between the scanner, which takes one credit for every batch it seals for admission
/// ([`ScanInputRunFactory::seal_observation_batch`]), and whatever finally disposes of that
/// batch. Backpressure, not a correctness guarantee: its job is smoothing memory and descriptor
/// use, so `acquire` gives up after a bounded wait and lets the batch through without a credit
/// rather than stall the scanner.
#[derive(Clone, Debug)]
struct InflightBatchBudget(Arc<AtomicUsize>);

impl InflightBatchBudget {
    fn new() -> Self {
        Self(Arc::new(AtomicUsize::new(0)))
    }

    fn acquire(&self) -> InflightCredit {
        const POLL: Duration = Duration::from_millis(1);
        const MAX_WAIT: Duration = Duration::from_millis(50);
        let mut waited = Duration::ZERO;
        loop {
            let current = self.0.load(Ordering::Acquire);
            if current < MAX_INFLIGHT_SCAN_BATCHES {
                if self
                    .0
                    .compare_exchange(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    return InflightCredit {
                        budget: Some(self.clone()),
                    };
                }
                continue;
            }
            if waited >= MAX_WAIT {
                return InflightCredit { budget: None };
            }
            thread::sleep(POLL);
            waited += POLL;
        }
    }
}

/// One credit of the in-flight cap. It returns to the budget when dropped, exactly once, so no
/// path that consumes a [`SealedBatch`] has a release to forget: a credit that `acquire` gave up
/// waiting for was never taken, and returns nothing.
#[derive(Debug)]
struct InflightCredit {
    budget: Option<InflightBatchBudget>,
}

impl Drop for InflightCredit {
    fn drop(&mut self) {
        if let Some(budget) = self.budget.take() {
            budget.0.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

/// The sealed runs of one scanner batch, and the in-flight credit that batch holds.
///
/// The credit returns when the batch is dropped, or when iterating it ends: after admission, or
/// on whatever path discards the batch unadmitted (a stale lease, a store that became
/// unavailable, a closed channel, a cancelled scan). A consumer that admits on the thread that
/// received the batch returns it at once with [`Self::into_runs_returning_credit`].
#[derive(Debug, Default)]
pub(crate) struct SealedBatch {
    runs: Vec<SealedRun>,
    credit: Option<InflightCredit>,
}

impl SealedBatch {
    /// A batch with no runs: it holds no credit.
    #[must_use]
    pub(crate) fn empty() -> Self {
        Self::default()
    }

    #[must_use]
    pub(crate) fn len(&self) -> usize {
        self.runs.len()
    }

    #[must_use]
    pub(crate) fn is_empty(&self) -> bool {
        self.runs.is_empty()
    }

    /// The runs, with the batch's in-flight credit returned now instead of when the last run is
    /// admitted. For a consumer that admits on the thread that received the batch, as the
    /// headless scan does: the credit has always come back as the batch arrived there, which is
    /// how that scan paces the scanner, so the time admission takes does not change it.
    #[must_use]
    pub(crate) fn into_runs_returning_credit(self) -> Vec<SealedRun> {
        let Self { runs, credit } = self;
        drop(credit);
        runs
    }
}

impl IntoIterator for SealedBatch {
    type Item = SealedRun;
    type IntoIter = SealedBatchRuns;

    fn into_iter(self) -> SealedBatchRuns {
        SealedBatchRuns {
            runs: self.runs.into_iter(),
            _credit: self.credit,
        }
    }
}

/// The runs of a [`SealedBatch`], still holding its credit until the iterator is dropped.
pub(crate) struct SealedBatchRuns {
    runs: std::vec::IntoIter<SealedRun>,
    _credit: Option<InflightCredit>,
}

impl Iterator for SealedBatchRuns {
    type Item = SealedRun;

    fn next(&mut self) -> Option<SealedRun> {
        self.runs.next()
    }
}

#[derive(Debug, Error)]
pub(crate) enum ScanStoreError {
    /// Not `transparent`: a bare `io::Error` has no source of its own, so a transparent
    /// wrapper's `source()` (which forwards to the wrapped value's source, not the wrapped
    /// value itself) would make it unreachable to a caller walking the chain for its
    /// `io::ErrorKind`. `"{0}"` keeps the same display text while keeping this variant
    /// reachable as a real link in the chain.
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error(transparent)]
    Run(#[from] RunError),
    #[error(transparent)]
    Merge(#[from] RunMergeError),
    #[error(transparent)]
    PathReduction(#[from] DirectorySummaryRunError),
    #[error(transparent)]
    IdentityReduction(#[from] IdentityReductionError),
    #[error(transparent)]
    PageIndex(#[from] PageIndexError),
    #[error(transparent)]
    Page(#[from] ScanPageError),
    #[error(transparent)]
    PathObservation(#[from] PathObservationRunError),
    #[error(transparent)]
    IdentityObservation(#[from] IdentityObservationRunError),
    #[error(transparent)]
    PathCatalog(#[from] PathCatalogError),
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    #[error("could not generate scan session identifier: {0}")]
    Session(String),
    #[error("could not recover private scan session: {0}")]
    Recovery(String),
    #[error("scan store has no active generation")]
    NoActiveGeneration,
    #[error("scan store has no published generation to use as an overlay base")]
    NoPublishedGeneration,
    #[error("scan store generation is not accepting scanner input")]
    ClosedGeneration,
    #[error("scan store accepted a run for a different generation")]
    GenerationMismatch,
    #[error("scan store accepts only raw path or identity observation runs")]
    InvalidInputRunKind,
    #[error("scan store generation must advance monotonically")]
    NonMonotonicGeneration,
    #[error("scan store exhausted generation identifiers")]
    GenerationOverflow,
    #[error("scan store exhausted run identifiers")]
    RunIdOverflow,
    #[error("scan store exhausted bounded merge tiers")]
    RunLevelOverflow,
    #[error("scan store observation batch exceeds {MAX_OBSERVATIONS_PER_BATCH} entries")]
    BatchTooLarge,
    #[error("scan store rejected a run for a different session lease")]
    LeaseSessionMismatch,
    #[error("scan store rejected a run whose lease generation does not match")]
    LeaseGenerationMismatch,
    #[error("scan store rejected a run from a non-enumeration lease")]
    LeaseKindMismatch,
    #[error("scan store accepted this sealed input run already")]
    DuplicateInputRun,
    /// What a deletion left cannot be put into the map without scanning again: the map would
    /// record something the file system no longer has, and nothing in the map says what the file
    /// system has instead.
    #[error("the map cannot be updated in place: {0}")]
    OverlayNeedsRescan(String),
}

/// Sealed-run I/O used by the internal benchmark harness.
///
/// The byte counters measure serialized run bytes rather than operating-system
/// syscalls, so they stay deterministic across page-cache behavior.
#[cfg(feature = "internal")]
#[allow(
    clippy::struct_field_names,
    reason = "the serialized-I/O unit must remain explicit at every metric call site"
)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ScanStoreIoMetrics {
    pub(crate) input_written_bytes: u64,
    pub(crate) merge_read_bytes: u64,
    pub(crate) merge_written_bytes: u64,
    pub(crate) reduction_read_bytes: u64,
    pub(crate) reduction_written_bytes: u64,
    pub(crate) publication_read_bytes: u64,
    pub(crate) publication_written_bytes: u64,
}

/// A coherent, immutable scan generation retained after successful reduction.
///
/// The sparse child-query run is the sole retained scan representation. It
/// carries page metrics plus the original facts needed to rebuild a focused
pub(crate) struct PublishedGeneration {
    manifest: ScanManifest,
    child_queries: SealedRun,
    path_catalog: SealedRun,
    identity_index: SealedRun,
    directory_summary_index: SealedRun,
    root_page_metrics: SummaryMetrics,
    root_page_coverage: Coverage,
    /// Paths the scanner could not encode into canonical runs.
    unrecorded_path_count: u64,
    /// Directories the scanner could not list, while under
    /// `MAX_TRACKED_UNREADABLE_DIRECTORIES`. Every ancestor of one is also
    /// uncertain in the published generation's coverage and bounds; carried
    /// forward into an overlay generation so an unrelated deletion refresh
    /// cannot make a still-unreadable directory look complete again.
    unreadable_directories: BTreeSet<RelativePath>,
    /// Set once the generation's unreadable-directory count passed the cap;
    /// `unreadable_directories` is then empty and every folder is uncertain.
    unreadable_directories_overflowed: bool,
}

impl PublishedGeneration {
    #[must_use]
    pub(crate) const fn manifest(&self) -> &ScanManifest {
        &self.manifest
    }

    #[must_use]
    pub(crate) const fn generation(&self) -> ScanGeneration {
        self.manifest.generation()
    }

    /// Returns the number of paths omitted from the canonical hierarchy.
    #[must_use]
    pub(crate) const fn unrecorded_path_count(&self) -> u64 {
        self.unrecorded_path_count
    }

    /// Returns the directories the scanner could not list in this
    /// generation, or `Overflowed` if there were more than the tracking cap.
    #[must_use]
    pub(crate) fn unreadable_directories(&self) -> UnreadableDirectories<'_> {
        unreadable_directories_view(
            &self.unreadable_directories,
            self.unreadable_directories_overflowed,
        )
    }

    #[must_use]
    pub(crate) const fn root_page_metrics(&self) -> SummaryMetrics {
        self.root_page_metrics
    }

    #[must_use]
    pub(crate) const fn root_page_coverage(&self) -> Coverage {
        self.root_page_coverage
    }

    #[must_use]
    pub(crate) const fn child_query_run(&self) -> &SealedRun {
        &self.child_queries
    }

    /// Resolves a canonical path through its compact parent/name catalog.
    pub(crate) fn path_catalog_entry(
        &self,
        path: &crate::scan_coordinator::RelativePath,
    ) -> Result<Option<PathCatalogEntry>, PathCatalogError> {
        read_path_catalog_entry(&self.path_catalog, path)
    }

    #[must_use]
    pub(crate) const fn identity_index_run(&self) -> &SealedRun {
        &self.identity_index
    }

    #[must_use]
    pub(crate) const fn directory_summary_index_run(&self) -> &SealedRun {
        &self.directory_summary_index
    }

    /// Reads one concrete entry from the immutable published generation.
    pub(crate) fn page_entry(
        &self,
        path: &crate::scan_coordinator::RelativePath,
    ) -> Result<Option<ScanPageEntry>, ScanPageError> {
        read_page_entry(&self.child_queries, path)
    }

    /// What a successor of this generation needs to copy its facts: a description another
    /// thread can read through descriptors of its own, so reading it never touches this
    /// generation's. `None` for a generation whose runs have no files (tests and fuzzing).
    ///
    /// The description stays valid for as long as this generation does.
    #[must_use]
    pub(crate) fn overlay_base(&self) -> Option<OverlayBase> {
        Some(OverlayBase {
            child_queries: self.child_queries.detached()?,
            unrecorded_path_count: self.unrecorded_path_count,
            unreadable_directories: self.unreadable_directories.clone(),
            unreadable_directories_overflowed: self.unreadable_directories_overflowed,
        })
    }

    /// Visits every concrete entry with its publication-time page metrics.
    pub(crate) fn visit_page_entries<E>(
        &mut self,
        visit: impl FnMut(ScanPageEntry) -> Result<(), E>,
    ) -> Result<(), E>
    where
        E: From<RunError> + From<PageIndexError>,
    {
        visit_child_query_page_entries(&mut self.child_queries, visit)
    }
}

/// Deterministic directory-only result retained when page materialization runs
/// out of scan-store capacity after canonical reduction completed.
pub(crate) struct SummaryOnlyGeneration {
    manifest: ScanManifest,
    directory_summaries: SealedRun,
    root_metrics: SummaryMetrics,
    root_coverage: Coverage,
    unrecorded_path_count: u64,
}

impl SummaryOnlyGeneration {
    #[must_use]
    pub(crate) const fn generation(&self) -> ScanGeneration {
        self.manifest.generation()
    }

    #[must_use]
    pub(crate) const fn root_metrics(&self) -> SummaryMetrics {
        self.root_metrics
    }

    #[must_use]
    pub(crate) const fn root_coverage(&self) -> Coverage {
        self.root_coverage
    }

    #[must_use]
    pub(crate) const fn unrecorded_path_count(&self) -> u64 {
        self.unrecorded_path_count
    }
}

/// A published generation as its successor reads it: the facts an overlay generation copies
/// (`ScanStore::begin_overlay_generation`), and the coverage notes it inherits.
///
/// Reading it does not need the generation itself, only its files, so the thread that builds
/// the overlay need not own, borrow, or share the generation the reader navigates. It stays
/// valid for as long as that generation does: the reader sends the one it has installed with
/// each overlay, and keeps the generation until its successor arrives.
#[derive(Clone, Debug)]
pub(crate) struct OverlayBase {
    child_queries: DetachedRun,
    unrecorded_path_count: u64,
    unreadable_directories: BTreeSet<RelativePath>,
    unreadable_directories_overflowed: bool,
}

/// A folder as the file system has it after a deletion changed it, to take the place of what the
/// map recorded of it ([`ScanStore::begin_overlay_generation`]).
#[derive(Clone, Debug)]
pub(crate) struct RefreshedFolder {
    pub(crate) path: RelativePath,
    pub(crate) snapshot: crate::model::EntrySnapshot,
    /// The entries the folder holds now, read in the same moment as the snapshot. The snapshot
    /// may replace the recorded one only when these are the entries the overlay publishes.
    pub(crate) entries: FolderDigest,
    /// Removals the map still lists that are already gone from the file system, each with an
    /// overlay of its own to follow this one. The entries of the folder that lie below one of them
    /// are not what the folder holds now, so they are left out of what `entries` is compared with:
    /// the folder holds what the map lists of it, less every removal already made.
    pub(crate) removed_later: Vec<RelativePath>,
}

impl RefreshedFolder {
    /// Puts the snapshot into the folder's observation, unless it is another folder's: one
    /// replaced since the scan is not one the map can describe.
    fn refresh(&self, observation: &mut PathObservation) -> Result<(), ScanStoreError> {
        let Some(recorded) = observation
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.identity.as_ref())
        else {
            // Nothing was recorded to check a deletion against, and none is made up here.
            return Ok(());
        };
        if self
            .snapshot
            .identity
            .as_ref()
            .is_none_or(|current| current.file_id != recorded.file_id)
        {
            return Err(ScanStoreError::OverlayNeedsRescan(
                "the folder that held the removed entry was replaced".to_string(),
            ));
        }
        observation.snapshot = Some(self.snapshot.clone());
        Ok(())
    }
}

/// What one successful `ScanStore::publish` produced, moved out of the store
/// ([`ScanStore::take_publication`]) for the thread that reads it. Both generations are boxed:
/// each is several hundred bytes, and the value moves through a queue.
pub(crate) enum Publication {
    Published(Box<PublishedGeneration>),
    SummaryOnly(Box<SummaryOnlyGeneration>),
}

/// A bounded base-eight merge pyramid for one externally sorted fact family.
///
/// A level holds at most seven completed runs until eight peers are merged into
/// one run at the next level. This retains a logarithmically bounded number
/// of handles and avoids repeatedly merging a large historical run with one fresh scanner batch.
struct RunLevels {
    levels: [Vec<SealedRun>; MAX_RUN_LEVELS],
}

impl Default for RunLevels {
    fn default() -> Self {
        Self {
            levels: std::array::from_fn(|_| Vec::new()),
        }
    }
}

impl RunLevels {
    fn push(&mut self, run: SealedRun) {
        self.levels[0].push(run);
    }

    fn take_full_level(&mut self) -> Option<(usize, Vec<SealedRun>)> {
        for (level, runs) in self.levels.iter_mut().enumerate() {
            if runs.len() >= MAX_ACTIVE_INPUT_RUNS {
                return Some((level, std::mem::take(runs)));
            }
        }
        None
    }

    fn push_merged(&mut self, level: usize, run: SealedRun) -> Result<(), ScanStoreError> {
        let next_level = level
            .checked_add(1)
            .filter(|next| *next < MAX_RUN_LEVELS)
            .ok_or(ScanStoreError::RunLevelOverflow)?;
        self.levels[next_level].push(run);
        Ok(())
    }

    fn into_runs(self) -> Vec<SealedRun> {
        let mut runs = Vec::new();
        for level in self.levels {
            runs.extend(level);
        }
        runs
    }

    fn populate_provisional_page(
        &mut self,
        page: &mut ProvisionalPage,
    ) -> Result<(), ScanStoreError> {
        for runs in &mut self.levels {
            for run in runs {
                page.observe_run(run)?;
            }
        }
        Ok(())
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.levels.iter().map(Vec::len).sum()
    }
}

/// Bounded run staging for the one scan generation currently being assembled.
struct ActiveGeneration {
    manifest: ScanManifest,
    path_runs: RunLevels,
    identity_runs: RunLevels,
    unrecorded_path_count: u64,
    unreadable_directories: BTreeSet<RelativePath>,
    unreadable_directories_overflowed: bool,
    accepted_run_ids: HashSet<u64>,
    provisional_page: Option<ProvisionalPage>,
}

impl ActiveGeneration {
    fn new(session: ScanSessionId, generation: ScanGeneration) -> Self {
        let mut manifest = ScanManifest::new(session, generation);
        manifest
            .transition(ScanGenerationState::Scanning)
            .expect("a newly created generation must begin scanning");
        Self {
            manifest,
            path_runs: RunLevels::default(),
            identity_runs: RunLevels::default(),
            unrecorded_path_count: 0,
            unreadable_directories: BTreeSet::new(),
            unreadable_directories_overflowed: false,
            accepted_run_ids: HashSet::new(),
            provisional_page: None,
        }
    }

    fn unreadable_directories(&self) -> UnreadableDirectories<'_> {
        unreadable_directories_view(
            &self.unreadable_directories,
            self.unreadable_directories_overflowed,
        )
    }
    fn input_runs_mut(&mut self, kind: RunKind) -> Result<&mut RunLevels, ScanStoreError> {
        match kind {
            RunKind::PathObservation => Ok(&mut self.path_runs),
            RunKind::IdentityObservation => Ok(&mut self.identity_runs),
            RunKind::AllocationContribution
            | RunKind::DirectorySummary
            | RunKind::ChildQuery
            | RunKind::PathCatalog => Err(ScanStoreError::InvalidInputRunKind),
        }
    }
}

/// Builds the view the reducer uses from one generation's tracked set and
/// overflow flag: `Tracked` while under the cap, `Overflowed` past it.
fn unreadable_directories_view(
    tracked: &BTreeSet<RelativePath>,
    overflowed: bool,
) -> UnreadableDirectories<'_> {
    if overflowed {
        UnreadableDirectories::Overflowed
    } else {
        UnreadableDirectories::Tracked(tracked)
    }
}

/// Cloneable worker-side factory for bounded, sorted scanner input runs.
///
/// Every factory shares the store's run identifier counter. Workers can seal a
/// run without borrowing the owner, while the owner remains the sole admission
/// point through [`ScanStore::accept_input_run`].
#[derive(Clone, Debug)]
pub(crate) struct ScanInputRunFactory {
    generation: ScanGeneration,
    temporary_storage: TemporaryStorage,
    session_storage: Option<ScanStoreStorage>,
    next_run_id: Arc<AtomicU64>,
    budget: InflightBatchBudget,
}

impl ScanInputRunFactory {
    #[must_use]
    pub(crate) const fn generation(&self) -> ScanGeneration {
        self.generation
    }

    /// The same factory for another generation of the same store: the one a rebuild begins.
    #[must_use]
    pub(crate) fn for_generation(&self, generation: ScanGeneration) -> Self {
        Self {
            generation,
            ..self.clone()
        }
    }

    /// How many sealed batches hold a credit of the in-flight cap right now.
    #[cfg(test)]
    pub(crate) fn in_flight_batches(&self) -> usize {
        self.budget.0.load(Ordering::Acquire)
    }

    /// Seals the two bounded raw fact families for one scanner batch.
    ///
    /// The batch holds one credit of the in-flight cap from here until it is dropped, so the
    /// scanner waits (a bounded time) once [`MAX_INFLIGHT_SCAN_BATCHES`] batches are sealed and
    /// not yet disposed of.
    ///
    /// # Errors
    ///
    /// Returns an error when a batch is oversized, a run cannot be written, or
    /// the shared run identifier sequence is exhausted.
    pub(crate) fn seal_observation_batch(
        &self,
        paths: Vec<PathObservation>,
        identities: Vec<IdentityObservation>,
    ) -> Result<SealedBatch, ScanStoreError> {
        if paths.len() > MAX_OBSERVATIONS_PER_BATCH || identities.len() > MAX_OBSERVATIONS_PER_BATCH
        {
            return Err(ScanStoreError::BatchTooLarge);
        }
        if paths.is_empty() && identities.is_empty() {
            return Ok(SealedBatch::empty());
        }
        // Taken before sealing, by the scanner; a failure below drops it with the batch that
        // never shipped.
        let credit = self.budget.acquire();
        let runs = self.seal_sorted_batch(paths, identities)?;
        Ok(SealedBatch {
            runs,
            credit: Some(credit),
        })
    }

    fn seal_sorted_batch(
        &self,
        mut paths: Vec<PathObservation>,
        mut identities: Vec<IdentityObservation>,
    ) -> Result<Vec<SealedRun>, ScanStoreError> {
        paths.sort_unstable_by(|left, right| left.path.cmp(&right.path));
        identities.sort_unstable_by(compare_identity_observations);
        let mut runs = Vec::with_capacity(2);
        if !paths.is_empty() {
            let mut writer = self.new_writer(RunKind::PathObservation)?;
            let mut key = Vec::new();
            let mut value = Vec::new();
            for observation in &paths {
                append_path_observation(&mut writer, observation, &mut key, &mut value)?;
            }
            runs.push(writer.seal()?);
        }
        if !identities.is_empty() {
            let mut writer = self.new_writer(RunKind::IdentityObservation)?;
            let mut key = Vec::new();
            let mut value = Vec::new();
            for observation in &identities {
                append_identity_observation(&mut writer, observation, &mut key, &mut value)?;
            }
            runs.push(writer.seal()?);
        }
        Ok(runs)
    }

    fn new_writer(&self, kind: RunKind) -> Result<RunWriter, ScanStoreError> {
        if !is_input_kind(kind) {
            return Err(ScanStoreError::InvalidInputRunKind);
        }
        new_run_writer(
            &self.temporary_storage,
            self.session_storage.as_ref(),
            &self.next_run_id,
            self.generation,
            kind,
            false,
        )
    }
}

/// Session-local external scan pipeline.
///
/// The owner receives small sorted scanner runs, folds each run family before
/// the family grows beyond a fixed fan-in, then retains one immutable result.
pub(crate) struct ScanStore {
    temporary_storage: TemporaryStorage,
    session: ScanSessionId,
    next_run_id: Arc<AtomicU64>,
    active: Option<ActiveGeneration>,
    session_storage: Option<ScanStoreStorage>,
    published: Option<PublishedGeneration>,
    summary_only: Option<SummaryOnlyGeneration>,
    retired_generation: Option<ScanGeneration>,
    /// The newest generation [`Self::take_publication`] moved out of this store: it still counts
    /// for generation ordering, though the store no longer holds it.
    handed_off_generation: Option<ScanGeneration>,
    inflight_batches: InflightBatchBudget,
    #[cfg(feature = "internal")]
    io_metrics: ScanStoreIoMetrics,
}

impl ScanStore {
    /// # Errors
    ///
    /// Returns an error if the operating system cannot create a session ID.
    pub(crate) fn new(
        generation: ScanGeneration,
        temporary_storage: TemporaryStorage,
    ) -> Result<Self, ScanStoreError> {
        Self::new_with_optional_storage(generation, temporary_storage, None)
    }

    pub(crate) fn new_with_storage(
        generation: ScanGeneration,
        storage: ScanStoreStorage,
    ) -> Result<Self, ScanStoreError> {
        let temporary_storage = storage.quota();
        Self::new_with_optional_storage(generation, temporary_storage, Some(storage))
    }

    fn new_with_optional_storage(
        generation: ScanGeneration,
        temporary_storage: TemporaryStorage,
        session_storage: Option<ScanStoreStorage>,
    ) -> Result<Self, ScanStoreError> {
        let session =
            ScanSessionId::random().map_err(|error| ScanStoreError::Session(error.to_string()))?;
        let active = ActiveGeneration::new(session, generation);
        Ok(Self {
            temporary_storage,
            session,
            session_storage,
            next_run_id: Arc::new(AtomicU64::new(0)),
            active: Some(active),
            published: None,
            summary_only: None,
            retired_generation: None,
            handed_off_generation: None,
            inflight_batches: InflightBatchBudget::new(),
            #[cfg(feature = "internal")]
            io_metrics: ScanStoreIoMetrics::default(),
        })
    }

    #[cfg(feature = "internal")]
    #[must_use]
    pub(crate) fn io_metrics(&self) -> ScanStoreIoMetrics {
        ScanStoreIoMetrics { ..self.io_metrics }
    }

    #[cfg(feature = "internal")]
    fn record_publication_io(
        &mut self,
        path_observations: &SealedRun,
        identity_observations: &SealedRun,
        allocation_contributions: &SealedRun,
        directory_summaries: &SealedRun,
        child_queries: &SealedRun,
    ) {
        self.io_metrics.publication_read_bytes = self
            .io_metrics
            .publication_read_bytes
            .saturating_add(path_observations.bytes())
            .saturating_add(identity_observations.bytes())
            .saturating_add(allocation_contributions.bytes())
            .saturating_add(directory_summaries.bytes());
        self.io_metrics.publication_written_bytes = self
            .io_metrics
            .publication_written_bytes
            .saturating_add(child_queries.bytes());
    }

    #[must_use]
    pub(crate) fn internal_paths(&self) -> Vec<PathBuf> {
        self.session_storage
            .as_ref()
            .map_or_else(Vec::new, ScanStoreStorage::internal_paths)
    }
    #[must_use]
    pub(crate) fn storage_stats(&self) -> (u64, u64) {
        (
            self.temporary_storage.used(),
            self.temporary_storage.limit(),
        )
    }

    /// The session's quota, shared with every run this store and its factories write.
    #[must_use]
    pub(crate) fn quota(&self) -> TemporaryStorage {
        self.temporary_storage.clone()
    }

    #[must_use]
    pub(crate) const fn session(&self) -> ScanSessionId {
        self.session
    }

    /// Records one path omitted from canonical page data while preserving every
    /// successfully observed path.
    pub(crate) fn record_unrecorded_path(&mut self) {
        self.record_unrecorded_paths(1);
    }

    /// Records `count` paths omitted from canonical page data.
    pub(crate) fn record_unrecorded_paths(&mut self, count: u64) {
        if let Some(active) = self.active.as_mut() {
            active.unrecorded_path_count = active.unrecorded_path_count.saturating_add(count);
        }
    }

    /// Records one directory the scanner could not list. Every ancestor up to
    /// the root becomes `Uncertain` with an open upper bound once this
    /// generation reduces (`reduce_path_observation_run`), regardless of the
    /// `Coverage::Complete` seed its own entry carried when its parent first
    /// discovered it, before the failure to open or list it was known.
    pub(crate) fn record_unreadable_directory(&mut self, path: RelativePath) {
        self.record_unreadable_directory_with_cap(path, MAX_TRACKED_UNREADABLE_DIRECTORIES);
    }

    /// `record_unreadable_directory`, with its tracking cap as a parameter so
    /// a test can reach the overflow fallback without creating thousands of
    /// real unreadable directories.
    fn record_unreadable_directory_with_cap(&mut self, path: RelativePath, cap: usize) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        if active.unreadable_directories_overflowed {
            return;
        }
        active.unreadable_directories.insert(path);
        if active.unreadable_directories.len() > cap {
            active.unreadable_directories.clear();
            active.unreadable_directories_overflowed = true;
        }
    }

    #[must_use]
    pub(crate) fn active_generation(&self) -> Option<ScanGeneration> {
        self.active
            .as_ref()
            .map(|active| active.manifest.generation())
    }

    #[must_use]
    pub(crate) fn summary_only(&self) -> Option<&SummaryOnlyGeneration> {
        self.summary_only.as_ref()
    }

    #[must_use]
    pub(crate) fn is_summary_only(&self) -> bool {
        self.summary_only.is_some()
    }

    #[must_use]
    pub(crate) fn published_generation(&self) -> Option<ScanGeneration> {
        self.published.as_ref().map(PublishedGeneration::generation)
    }

    #[must_use]
    pub(crate) fn published(&self) -> Option<&PublishedGeneration> {
        self.published.as_ref()
    }

    #[must_use]
    pub(crate) fn published_mut(&mut self) -> Option<&mut PublishedGeneration> {
        self.published.as_mut()
    }

    /// Transfers the deterministic directory-only capacity result.
    pub(crate) fn into_summary_only(mut self) -> Result<SummaryOnlyGeneration, ScanStoreError> {
        self.summary_only
            .take()
            .ok_or(ScanStoreError::NoPublishedGeneration)
    }

    /// Transfers the immutable published generation to a report or another
    /// read-only consumer.
    pub(crate) fn into_published(mut self) -> Result<PublishedGeneration, ScanStoreError> {
        self.published
            .take()
            .ok_or(ScanStoreError::NoPublishedGeneration)
    }

    /// Moves the newest publication out of the store, for a store whose reader is another
    /// thread (the interactive session's store thread). The store keeps only the generation's
    /// number, which still counts for ordering. It keeps nothing to build a successor from: the
    /// reader that holds the generation says which one a successor derives from
    /// ([`Self::begin_overlay_generation`]), so a generation the reader drops, or never
    /// installs, cannot be the one a later overlay is built from.
    pub(crate) fn take_publication(&mut self) -> Option<Publication> {
        if let Some(summary) = self.summary_only.take() {
            self.note_handed_off(summary.generation());
            return Some(Publication::SummaryOnly(Box::new(summary)));
        }
        let published = self.published.take()?;
        self.note_handed_off(published.generation());
        Some(Publication::Published(Box::new(published)))
    }

    fn note_handed_off(&mut self, generation: ScanGeneration) {
        self.handed_off_generation = self.handed_off_generation.max(Some(generation));
    }

    /// Abandons unsealed work and begins a strictly newer generation. The last
    /// published snapshot remains queryable until its replacement publishes.
    ///
    /// # Errors
    ///
    /// Returns an error when `generation` does not advance the session's
    /// active, published, summary-only, or retired generation.
    pub(crate) fn begin_generation(
        &mut self,
        generation: ScanGeneration,
    ) -> Result<(), ScanStoreError> {
        if self
            .latest_generation()
            .is_some_and(|latest| generation <= latest)
        {
            return Err(ScanStoreError::NonMonotonicGeneration);
        }
        let active = ActiveGeneration::new(self.session, generation);
        self.active = Some(active);
        Ok(())
    }

    fn latest_generation(&self) -> Option<ScanGeneration> {
        [
            self.active_generation(),
            self.published_generation(),
            self.summary_only
                .as_ref()
                .map(SummaryOnlyGeneration::generation),
            self.retired_generation,
            self.handed_off_generation,
        ]
        .into_iter()
        .flatten()
        .max()
    }

    pub(crate) fn next_generation(&self) -> Result<ScanGeneration, ScanStoreError> {
        self.latest_generation()
            .map_or(Ok(ScanGeneration::initial()), |generation| {
                generation
                    .value()
                    .checked_add(1)
                    .map(ScanGeneration::from_value)
                    .ok_or(ScanStoreError::GenerationOverflow)
            })
    }

    /// Marks an unpublishable active generation incomplete while retaining the
    /// last immutable published snapshot.
    ///
    /// An incomplete generation may have accepted only a prefix of scanner
    /// input, including when its scan-store capacity was exhausted. It must
    /// never be confused with a user-cancelled generation.
    pub(crate) fn discard_active(&mut self) -> Result<(), ScanStoreError> {
        self.close_active(ScanGenerationState::Incomplete)
    }

    /// Marks an actively scanning generation as cancelled.
    ///
    /// Cancellation is a deliberate user action, distinct from an incomplete
    /// scan whose canonical representation could not be built.
    pub(crate) fn cancel_active(&mut self) -> Result<(), ScanStoreError> {
        self.close_active(ScanGenerationState::Cancelled)
    }
    fn close_active(&mut self, terminal: ScanGenerationState) -> Result<(), ScanStoreError> {
        let Some(mut active) = self.active.take() else {
            return Ok(());
        };
        if active.manifest.state() != terminal {
            active.manifest.transition(terminal)?;
        }
        self.retired_generation = Some(active.manifest.generation());
        Ok(())
    }

    /// Starts a newer generation by rebuilding every fact outside `replaced_prefix` from the
    /// compact child-query run of `base`, the published generation it derives from. Callers may
    /// then publish a deletion overlay or add replacement facts from live data.
    ///
    /// `base` is a description of the generation's files ([`OverlayBase`]), never the generation
    /// itself: the thread that builds the overlay need not own it, and the reader that does goes
    /// on navigating it. The reader says which generation that is, because only it knows which
    /// one it kept.
    ///
    /// Entries are copied as the scan recorded them, which is right for all but what the removal
    /// itself changed on disk. Removing an entry moves the modification time of the folder that
    /// held it, and that folder's link count when the entry was a folder (or a file, on APFS,
    /// which counts a folder's files in it), and the check that the reader is deleting what they
    /// saw compares both, so `folder` (that folder as the file system has it now) replaces the
    /// snapshot the map recorded. That time describes every change to the folder's entries, not
    /// only the removal: another process that made, removed, renamed, or replaced an entry
    /// beside it moved it too, and a map that recorded
    /// the new time would pass a later deletion of the folder as unchanged while the folder
    /// holds what the map never showed. So `folder` also carries what the folder holds now (a
    /// [`FolderDigest`] of the names, kinds, and identities of its entries), and the snapshot
    /// replaces the recorded one only when that is exactly the base's entries of the folder
    /// less the removed one. The links to a hard-linked file are another matter: the removal
    /// changed the link count of every other link to it, and the map has no way to say where
    /// they are. A removed hard link therefore ends the overlay with
    /// [`ScanStoreError::OverlayNeedsRescan`], as does a folder that is not what the map
    /// recorded, or that holds other entries than it lists.
    ///
    /// # Errors
    ///
    /// Returns an error when generation ordering is invalid, a retained query record is corrupt,
    /// the copy cannot fit the shared temporary-storage budget, or the overlay cannot be exact.
    /// Copy failure leaves the new generation incomplete and preserves the prior published
    /// snapshot.
    pub(crate) fn begin_overlay_generation(
        &mut self,
        base: &OverlayBase,
        generation: ScanGeneration,
        replaced_prefix: &crate::scan_coordinator::RelativePath,
        folder: Option<&RefreshedFolder>,
    ) -> Result<(), ScanStoreError> {
        self.start_overlay_generation(base, generation)?;
        if replaced_prefix.is_root() {
            return Ok(());
        }
        if let Err(error) = self.copy_overlay_facts(base, replaced_prefix, folder) {
            self.mark_active_incomplete();
            return Err(error);
        }
        Ok(())
    }

    fn copy_overlay_facts(
        &mut self,
        base: &OverlayBase,
        replaced_prefix: &crate::scan_coordinator::RelativePath,
        folder: Option<&RefreshedFolder>,
    ) -> Result<(), ScanStoreError> {
        let storage = self.temporary_storage.clone();
        let mut paths = Vec::with_capacity(MAX_OBSERVATIONS_PER_BATCH);
        let mut identities = Vec::with_capacity(MAX_OBSERVATIONS_PER_BATCH);
        // The scan root has no record of its own to refresh: when it is the folder, it is checked
        // and its recorded snapshot is left as it is.
        let mut folder_copied = folder.is_none_or(|folder| folder.path.is_root());
        // The entries of the refreshed folder that this overlay publishes: the ones the base has,
        // less the removed one.
        let mut folder_entries = FolderDigest::default();
        base.child_queries.with_reader(&storage, |reader| {
            visit_child_query_entries(reader, |mut path, identity| -> Result<(), ScanStoreError> {
                if path.path.starts_with(replaced_prefix) {
                    if may_have_other_links(&path, identity.as_ref()) {
                        return Err(ScanStoreError::OverlayNeedsRescan(
                            "the removed entries include a hard link, and the map does not \
                                 say where the others are"
                                .to_string(),
                        ));
                    }
                    return Ok(());
                }
                if let Some(folder) = folder {
                    if folder.path == path.path {
                        folder.refresh(&mut path)?;
                        folder_copied = true;
                    } else if path.path.is_direct_child_of(&folder.path)
                        && !folder
                            .removed_later
                            .iter()
                            .any(|later| path.path.starts_with(later))
                        && let Some(name) = path.path.components().last()
                    {
                        folder_entries.add(
                            name,
                            path.kind,
                            path.snapshot
                                .as_ref()
                                .and_then(|snapshot| snapshot.identity.as_ref())
                                .map(|identity| &identity.file_id),
                        );
                    }
                }
                // A folder's page record keeps its scan-time identity only for the deletion
                // check. It is not an identity fact to reduce: only a file's hard-link facts
                // are, and indexing one for a folder fails the whole overlay.
                if let Some(identity) = identity.filter(|identity| {
                    path.kind != PathEntryKind::Directory
                        && identity_requires_reduction(&path, identity)
                }) {
                    identities.push(identity);
                }
                paths.push(path);
                if paths.len() == MAX_OBSERVATIONS_PER_BATCH {
                    self.append_overlay_batch(&mut paths, &mut identities)?;
                }
                Ok(())
            })
        })?;
        if let Some(folder) = folder {
            if !folder_copied {
                return Err(ScanStoreError::OverlayNeedsRescan(
                    "the folder that held the removed entry is not in the map".to_string(),
                ));
            }
            if folder.entries != folder_entries {
                return Err(ScanStoreError::OverlayNeedsRescan(
                    "the folder that held the removed entry holds entries the map does not list, \
                     or lacks ones it lists"
                        .to_string(),
                ));
            }
        }
        self.append_overlay_batch(&mut paths, &mut identities)
    }

    fn append_overlay_batch(
        &mut self,
        paths: &mut Vec<PathObservation>,
        identities: &mut Vec<IdentityObservation>,
    ) -> Result<(), ScanStoreError> {
        if paths.is_empty() {
            return Ok(());
        }
        let paths = std::mem::replace(paths, Vec::with_capacity(MAX_OBSERVATIONS_PER_BATCH));
        let identities =
            std::mem::replace(identities, Vec::with_capacity(MAX_OBSERVATIONS_PER_BATCH));
        self.append_observation_batch(paths, identities)
    }

    fn start_overlay_generation(
        &mut self,
        base: &OverlayBase,
        generation: ScanGeneration,
    ) -> Result<(), ScanStoreError> {
        self.begin_generation(generation)?;
        if let Some(active) = self.active.as_mut() {
            active.unrecorded_path_count = base.unrecorded_path_count;
            active
                .unreadable_directories
                .clone_from(&base.unreadable_directories);
            active.unreadable_directories_overflowed = base.unreadable_directories_overflowed;
        }
        Ok(())
    }

    /// Creates one empty scanner-input run for the active generation.
    ///
    /// The caller writes a locally sorted bounded batch, seals it, then returns
    /// it through [`Self::accept_input_run`].
    ///
    /// # Errors
    ///
    /// Returns an error if no generation is accepting input, temporary storage
    /// cannot reserve the run, or run identifiers are exhausted.
    pub(crate) fn begin_input_run(&mut self, kind: RunKind) -> Result<RunWriter, ScanStoreError> {
        if !is_input_kind(kind) {
            return Err(ScanStoreError::InvalidInputRunKind);
        }
        let generation = self.accepting_generation()?;
        self.new_input_writer(generation, kind)
    }

    /// Creates a worker-safe factory for sealed input runs in the active generation.
    ///
    /// The caller must return every sealed run through [`Self::accept_input_run`].
    pub(crate) fn input_run_factory(&self) -> Result<ScanInputRunFactory, ScanStoreError> {
        Ok(ScanInputRunFactory {
            generation: self.accepting_generation()?,
            temporary_storage: self.temporary_storage.clone(),
            session_storage: self.session_storage.clone(),
            next_run_id: Arc::clone(&self.next_run_id),
            budget: self.inflight_batches.clone(),
        })
    }

    /// Adds one sealed raw scanner run to the active generation.
    ///
    /// The run enters the durable manifest before it becomes eligible for
    /// compaction, so a crash cannot leave an admitted file unreferenced.
    pub(crate) fn accept_input_run(&mut self, mut run: SealedRun) -> Result<(), ScanStoreError> {
        let descriptor = run.descriptor();
        if !is_input_kind(descriptor.kind()) {
            return Err(ScanStoreError::InvalidInputRunKind);
        }
        let generation = self.accepting_generation()?;
        if descriptor.generation() != generation {
            return Err(ScanStoreError::GenerationMismatch);
        }
        let mut active = self
            .active
            .take()
            .ok_or(ScanStoreError::NoActiveGeneration)?;
        if !active.accepted_run_ids.insert(descriptor.run_id()) {
            self.active = Some(active);
            return Err(ScanStoreError::DuplicateInputRun);
        }
        if descriptor.kind() == RunKind::PathObservation
            && let Some(page) = active.provisional_page.as_mut()
            && let Err(error) = page.observe_run(&mut run)
        {
            active.provisional_page = None;
            active.accepted_run_ids.remove(&descriptor.run_id());
            self.active = Some(active);
            return Err(error.into());
        }
        if let Err(error) = Self::record_added_run(&mut active, &run) {
            if descriptor.kind() == RunKind::PathObservation {
                active.provisional_page = None;
            }
            active.accepted_run_ids.remove(&descriptor.run_id());
            self.active = Some(active);
            return Err(error);
        }
        #[cfg(feature = "internal")]
        {
            self.io_metrics.input_written_bytes = self
                .io_metrics
                .input_written_bytes
                .saturating_add(run.bytes());
        }
        active.input_runs_mut(descriptor.kind())?.push(run);
        self.active = Some(active);
        if let Err(error) = self.compact_input_family(descriptor.kind()) {
            self.mark_active_incomplete();
            return Err(error);
        }
        Ok(())
    }

    /// Admits one worker-owned sealed run only when its exact enumeration
    /// lease belongs to this active session and generation.
    ///
    /// Returns false for an already-admitted run, making a replay harmless
    /// without allowing its facts to enter the generation twice.
    pub(crate) fn accept_leased_input_run(
        &mut self,
        lease: &WorkLease,
        run: SealedRun,
    ) -> Result<bool, ScanStoreError> {
        if lease.key().session() != self.session {
            return Err(ScanStoreError::LeaseSessionMismatch);
        }
        if lease.key().kind() != WorkKind::EnumerateDirectory {
            return Err(ScanStoreError::LeaseKindMismatch);
        }
        let descriptor = run.descriptor();
        if descriptor.generation() != lease.key().generation() {
            return Err(ScanStoreError::LeaseGenerationMismatch);
        }
        if self
            .active
            .as_ref()
            .is_some_and(|active| active.accepted_run_ids.contains(&descriptor.run_id()))
        {
            return Ok(false);
        }
        self.accept_input_run(run)?;
        Ok(true)
    }

    /// Sorts and persists one bounded scanner batch in both raw fact families.
    ///
    /// Physical bytes belong only to identity observations. Path metrics retain
    /// hierarchy, apparent size, and coverage. Publishing derives once-per-
    /// identity physical totals from the companion run.
    ///
    /// # Errors
    ///
    /// Returns an error for an oversized batch, stale generation, duplicate
    /// canonical key, or temporary-storage failure. Any partial batch failure
    /// marks the active generation incomplete instead of permitting a false
    /// exact result.
    pub(crate) fn append_observation_batch(
        &mut self,
        paths: Vec<PathObservation>,
        identities: Vec<IdentityObservation>,
    ) -> Result<(), ScanStoreError> {
        let result = self.append_sealed_observation_batch(paths, identities);
        if result.is_err() {
            self.mark_active_incomplete();
        }
        result
    }

    fn append_sealed_observation_batch(
        &mut self,
        paths: Vec<PathObservation>,
        identities: Vec<IdentityObservation>,
    ) -> Result<(), ScanStoreError> {
        let factory = self.input_run_factory()?;
        // The batch holds its credit for this loop, and returns it when the loop ends: after
        // the last run is admitted, or at the first failure.
        for run in factory.seal_observation_batch(paths, identities)? {
            self.accept_input_run(run)?;
        }
        Ok(())
    }

    /// Returns a bounded, explicitly incomplete page from the active generation.
    ///
    /// The page is seeded from retained canonical input runs on the first query,
    /// then incrementally updated as workers admit later path runs.
    pub(crate) fn provisional_page(
        &mut self,
        request: &PageRequest,
    ) -> Result<ScanPage, ScanStoreError> {
        let active = self
            .active
            .as_mut()
            .ok_or(ScanStoreError::NoActiveGeneration)?;
        if active.manifest.state() != ScanGenerationState::Scanning {
            return Err(ScanStoreError::ClosedGeneration);
        }
        if active
            .provisional_page
            .as_ref()
            .is_none_or(|page| !page.matches(request))
        {
            let mut page = ProvisionalPage::new(active.manifest.generation(), request)?;
            active.path_runs.populate_provisional_page(&mut page)?;
            active.provisional_page = Some(page);
        }
        active
            .provisional_page
            .as_ref()
            .expect("active provisional page should be installed")
            .page(active.unrecorded_path_count)
            .map_err(ScanStoreError::from)
    }

    /// The stages [`Self::publish_reporting`] reports, in order: the two merges and the three
    /// reductions of the raw facts, each of the steps that materialize the child-query run
    /// ([`MATERIALIZE_STEPS`]), and keeping the published runs open. A stage is one pass over the
    /// scan's facts, or a part of one.
    pub(crate) const PUBLISH_STAGES: u8 = REDUCE_STAGES + MATERIALIZE_STEPS + 1;

    /// Seals all raw runs, reduces them, and atomically installs the resulting
    /// immutable generation as the newest published snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error for an incomplete/stale active generation, malformed
    /// records, merge failure, or temporary-storage exhaustion. Failure leaves
    /// the active generation marked incomplete rather than publishing a mix of
    /// partial output and prior data.
    pub(crate) fn publish(&mut self) -> Result<ScanGeneration, ScanStoreError> {
        self.publish_reporting(&mut |_| {})
    }

    /// [`Self::publish`], reporting each completed stage (`1` through [`Self::PUBLISH_STAGES`])
    /// to `report` as it ends. Publishing a large scan takes long enough that the reader should
    /// see it make progress.
    ///
    /// # Errors
    ///
    /// As [`Self::publish`].
    pub(crate) fn publish_reporting(
        &mut self,
        report: &mut dyn FnMut(u8),
    ) -> Result<ScanGeneration, ScanStoreError> {
        let mut active = self
            .active
            .take()
            .ok_or(ScanStoreError::NoActiveGeneration)?;
        if active.manifest.state() != ScanGenerationState::Scanning {
            self.active = Some(active);
            return Err(ScanStoreError::ClosedGeneration);
        }
        active.manifest.transition(ScanGenerationState::Reducing)?;
        let generation = active.manifest.generation();
        let (
            mut path_observations,
            mut identity_observations,
            mut allocation_contributions,
            mut directory_summaries,
            mut path_catalog,
        ) = match self.reduce_active_generation(&mut active, report) {
            Ok(reduced) => reduced,
            Err(error) => return self.finish_incomplete_generation(active, error),
        };

        let child_result = (|| -> Result<_, ScanStoreError> {
            let mut child_writer = self.new_writer(generation, RunKind::ChildQuery)?;
            let page_metadata = materialize_child_queries(
                &self.temporary_storage,
                self.session_storage.as_ref(),
                &mut path_observations,
                &mut identity_observations,
                &mut directory_summaries,
                &mut allocation_contributions,
                &mut child_writer,
                &mut |step| report(REDUCE_STAGES + step),
            )?;
            Ok((child_writer.seal()?, page_metadata))
        })();
        let (mut child_queries, page_metadata) = match child_result {
            Ok(result) => result,
            Err(error) if is_scan_store_capacity_error(&error) => {
                return self.finish_summary_only_generation(
                    &active,
                    directory_summaries,
                    (
                        path_observations,
                        identity_observations,
                        allocation_contributions,
                        path_catalog,
                    ),
                );
            }
            Err(error) => return self.finish_incomplete_generation(active, error),
        };
        // Warm the four runs this generation keeps for as long as it stays published: an
        // interactive page read (`PublishedGeneration::page_entry`/`path_catalog_entry`) uses
        // `SealedRun::range_reader`, which cannot cache a lazily reopened file behind a shared
        // `&self` borrow, so opening it once here keeps every later read as cheap as before
        // this run stopped holding its file from `seal` onward. Best-effort: a failed warm
        // still serves correctly, just by reopening on that first read instead.
        let resident: [&mut SealedRun; RESIDENT_PUBLISHED_RUNS] = [
            &mut child_queries,
            &mut path_catalog,
            &mut identity_observations,
            &mut directory_summaries,
        ];
        for run in resident {
            let _ = run.ensure_resident();
        }
        report(Self::PUBLISH_STAGES);
        #[cfg(feature = "internal")]
        self.record_publication_io(
            &path_observations,
            &identity_observations,
            &allocation_contributions,
            &directory_summaries,
            &child_queries,
        );
        let unrecorded_path_count = active.unrecorded_path_count;
        let unreadable_directories = std::mem::take(&mut active.unreadable_directories);
        let unreadable_directories_overflowed = active.unreadable_directories_overflowed;
        let mut manifest = active.manifest.clone();
        manifest.replace_all_runs(RunManifestEntry {
            descriptor: child_queries.descriptor(),
            bytes: child_queries.bytes(),
        })?;
        for run in [&path_catalog, &identity_observations, &directory_summaries] {
            manifest.add_run(RunManifestEntry {
                descriptor: run.descriptor(),
                bytes: run.bytes(),
            })?;
        }
        manifest.transition(ScanGenerationState::Published)?;
        drop((path_observations, allocation_contributions));
        self.summary_only = None;
        self.published = Some(PublishedGeneration {
            manifest,
            child_queries,
            path_catalog,
            identity_index: identity_observations,
            directory_summary_index: directory_summaries,
            root_page_metrics: page_metadata.root_metrics,
            root_page_coverage: page_metadata.root_coverage,
            unrecorded_path_count,
            unreadable_directories,
            unreadable_directories_overflowed,
        });
        Ok(generation)
    }

    fn finish_incomplete_generation(
        &mut self,
        mut active: ActiveGeneration,
        error: ScanStoreError,
    ) -> Result<ScanGeneration, ScanStoreError> {
        let _ = active.manifest.transition(ScanGenerationState::Incomplete);
        self.active = Some(active);
        Err(error)
    }

    fn finish_summary_only_generation(
        &mut self,
        active: &ActiveGeneration,
        mut directory_summaries: SealedRun,
        sources: (SealedRun, SealedRun, SealedRun, SealedRun),
    ) -> Result<ScanGeneration, ScanStoreError> {
        let (root_metrics, root_coverage) = summary_root(&mut directory_summaries)?;
        let generation = active.manifest.generation();
        let mut manifest = active.manifest.clone();
        manifest.replace_all_runs(RunManifestEntry {
            descriptor: directory_summaries.descriptor(),
            bytes: directory_summaries.bytes(),
        })?;
        manifest.transition(ScanGenerationState::SummaryOnly)?;
        drop(sources);
        // Best-effort for the same reason as the published path above; summary-only serves
        // `directory_summaries` the same way.
        let _ = directory_summaries.ensure_resident();
        self.summary_only = Some(SummaryOnlyGeneration {
            manifest,
            directory_summaries,
            root_metrics,
            root_coverage,
            unrecorded_path_count: active.unrecorded_path_count,
        });
        Ok(generation)
    }

    fn reduce_active_generation(
        &mut self,
        active: &mut ActiveGeneration,
        report: &mut dyn FnMut(u8),
    ) -> Result<(SealedRun, SealedRun, SealedRun, SealedRun, SealedRun), ScanStoreError> {
        let generation = active.manifest.generation();
        let path_runs = std::mem::take(&mut active.path_runs);
        let path_observations =
            self.merge_tiered_family(active, RunKind::PathObservation, path_runs)?;
        report(1);
        let identity_runs = std::mem::take(&mut active.identity_runs);
        let identity_observations =
            self.merge_tiered_family(active, RunKind::IdentityObservation, identity_runs)?;
        report(2);

        #[cfg(feature = "internal")]
        {
            self.io_metrics.reduction_read_bytes = self
                .io_metrics
                .reduction_read_bytes
                .saturating_add(path_observations.bytes());
        }
        let mut path_reader = path_observations.into_reader()?;
        let mut directory_writer = self.new_writer(generation, RunKind::DirectorySummary)?;
        reduce_path_observation_run(
            &mut path_reader,
            &mut directory_writer,
            active.unreadable_directories(),
        )?;
        let path_observations = path_reader.into_sealed();
        let directory_summaries = directory_writer.seal()?;
        report(3);
        #[cfg(feature = "internal")]
        {
            self.io_metrics.reduction_written_bytes = self
                .io_metrics
                .reduction_written_bytes
                .saturating_add(directory_summaries.bytes());
        }
        Self::record_added_run(active, &directory_summaries)?;

        #[cfg(feature = "internal")]
        {
            self.io_metrics.reduction_read_bytes = self
                .io_metrics
                .reduction_read_bytes
                .saturating_add(path_observations.bytes());
        }
        let mut catalog_reader = path_observations.into_reader()?;
        let mut catalog_writer = self.new_writer(generation, RunKind::PathCatalog)?;
        build_path_catalog(&mut catalog_reader, &mut catalog_writer)?;
        let path_observations = catalog_reader.into_sealed();
        let path_catalog = catalog_writer.seal()?;
        report(4);
        #[cfg(feature = "internal")]
        {
            self.io_metrics.reduction_written_bytes = self
                .io_metrics
                .reduction_written_bytes
                .saturating_add(path_catalog.bytes());
        }
        Self::record_added_run(active, &path_catalog)?;

        #[cfg(feature = "internal")]
        {
            self.io_metrics.reduction_read_bytes = self
                .io_metrics
                .reduction_read_bytes
                .saturating_add(identity_observations.bytes());
        }
        let mut identity_reader = identity_observations.into_reader()?;
        let mut allocation_writer = self.new_writer(generation, RunKind::AllocationContribution)?;
        reduce_identity_observations(&mut identity_reader, &mut allocation_writer)?;
        let identity_observations = identity_reader.into_sealed();
        let allocation_contributions = allocation_writer.seal()?;
        report(5);
        #[cfg(feature = "internal")]
        {
            self.io_metrics.reduction_written_bytes = self
                .io_metrics
                .reduction_written_bytes
                .saturating_add(allocation_contributions.bytes());
        }
        Self::record_added_run(active, &allocation_contributions)?;

        Ok((
            path_observations,
            identity_observations,
            allocation_contributions,
            directory_summaries,
            path_catalog,
        ))
    }

    fn compact_input_family(&mut self, kind: RunKind) -> Result<(), ScanStoreError> {
        let mut active = self
            .active
            .take()
            .ok_or(ScanStoreError::NoActiveGeneration)?;
        let result = self.compact_active_input_family(&mut active, kind);
        self.active = Some(active);
        result
    }

    fn compact_active_input_family(
        &mut self,
        active: &mut ActiveGeneration,
        kind: RunKind,
    ) -> Result<(), ScanStoreError> {
        let generation = active.manifest.generation();
        loop {
            let Some((level, runs)) = active.input_runs_mut(kind)?.take_full_level() else {
                return Ok(());
            };
            let merged = self.merge_family(active, generation, kind, runs)?;
            active.input_runs_mut(kind)?.push_merged(level, merged)?;
        }
    }

    /// Consolidates the bounded level pyramid in small merge groups. Each
    /// successor is manifested before its consumed predecessors are released.
    fn merge_tiered_family(
        &mut self,
        active: &mut ActiveGeneration,
        kind: RunKind,
        levels: RunLevels,
    ) -> Result<SealedRun, ScanStoreError> {
        let generation = active.manifest.generation();
        let mut pending = levels.into_runs();
        if pending.is_empty() {
            return self.merge_family(active, generation, kind, pending);
        }
        while pending.len() > 1 {
            let groups = pending
                .len()
                .saturating_add(MAX_ACTIVE_INPUT_RUNS.saturating_sub(1))
                / MAX_ACTIVE_INPUT_RUNS;
            let mut next = Vec::with_capacity(groups);
            let mut inputs = pending.into_iter();
            while let Some(first) = inputs.next() {
                let mut group = Vec::with_capacity(MAX_ACTIVE_INPUT_RUNS);
                group.push(first);
                for _ in 1..MAX_ACTIVE_INPUT_RUNS {
                    let Some(run) = inputs.next() else {
                        break;
                    };
                    group.push(run);
                }
                next.push(self.merge_family(active, generation, kind, group)?);
            }
            pending = next;
        }
        Ok(pending
            .pop()
            .expect("a nonempty merge tier must retain one run"))
    }

    fn merge_family(
        &mut self,
        active: &mut ActiveGeneration,
        generation: ScanGeneration,
        kind: RunKind,
        runs: Vec<SealedRun>,
    ) -> Result<SealedRun, ScanStoreError> {
        match runs.len() {
            0 => {
                let output = self.new_writer(generation, kind)?.seal()?;
                Self::record_added_run(active, &output)?;
                Ok(output)
            }
            1 => Ok(runs.into_iter().next().expect("one run was checked above")),
            _ => {
                #[cfg(feature = "internal")]
                let input_bytes = runs
                    .iter()
                    .fold(0_u64, |total, run| total.saturating_add(run.bytes()));
                let descriptors = runs.iter().map(SealedRun::descriptor).collect::<Vec<_>>();
                let mut readers = Vec::with_capacity(runs.len());
                for run in runs {
                    readers.push(run.into_reader()?);
                }
                let mut output = self.new_writer(generation, kind)?;
                merge_sorted_runs(&mut readers, &mut output)?;
                let output = output.seal()?;
                #[cfg(feature = "internal")]
                {
                    self.io_metrics.merge_read_bytes =
                        self.io_metrics.merge_read_bytes.saturating_add(input_bytes);
                    self.io_metrics.merge_written_bytes = self
                        .io_metrics
                        .merge_written_bytes
                        .saturating_add(output.bytes());
                }
                Self::record_replaced_runs(active, &descriptors, &output)?;
                drop(readers);
                Ok(output)
            }
        }
    }

    fn record_added_run(
        active: &mut ActiveGeneration,
        run: &SealedRun,
    ) -> Result<(), ScanStoreError> {
        let mut manifest = active.manifest.clone();
        manifest.add_run(RunManifestEntry {
            descriptor: run.descriptor(),
            bytes: run.bytes(),
        })?;
        active.manifest = manifest;
        Ok(())
    }

    fn record_replaced_runs(
        active: &mut ActiveGeneration,
        removed: &[RunDescriptor],
        output: &SealedRun,
    ) -> Result<(), ScanStoreError> {
        let mut manifest = active.manifest.clone();
        manifest.replace_runs(
            removed,
            RunManifestEntry {
                descriptor: output.descriptor(),
                bytes: output.bytes(),
            },
        )?;
        active.manifest = manifest;
        Ok(())
    }

    fn accepting_generation(&self) -> Result<ScanGeneration, ScanStoreError> {
        let active = self
            .active
            .as_ref()
            .ok_or(ScanStoreError::NoActiveGeneration)?;
        if active.manifest.state() != ScanGenerationState::Scanning {
            return Err(ScanStoreError::ClosedGeneration);
        }
        Ok(active.manifest.generation())
    }

    fn new_input_writer(
        &mut self,
        generation: ScanGeneration,
        kind: RunKind,
    ) -> Result<RunWriter, ScanStoreError> {
        new_run_writer(
            &self.temporary_storage,
            self.session_storage.as_ref(),
            &self.next_run_id,
            generation,
            kind,
            false,
        )
    }

    fn new_writer(
        &mut self,
        generation: ScanGeneration,
        kind: RunKind,
    ) -> Result<RunWriter, ScanStoreError> {
        new_run_writer(
            &self.temporary_storage,
            self.session_storage.as_ref(),
            &self.next_run_id,
            generation,
            kind,
            true,
        )
    }
}
/// Whether this error means the scan store ran out of room to admit or compact input,
/// whatever the proximate cause: the session's own tracked quota (`TemporaryStorage::reserve`)
/// or a raw out-of-space condition on the volume underneath it. Both report through an
/// `io::Error`, so this walks the causal chain for an `io::ErrorKind` that means "no room
/// left" instead of matching any message text. A filesystem-level quota (`QuotaExceeded`,
/// distinct from this session's own tracked budget) gets the same answer: the remedy is the
/// same for both, a scratch location outside whichever limit was hit.
pub(crate) fn is_scan_store_capacity_error(error: &ScanStoreError) -> bool {
    let mut source: &(dyn StdError + 'static) = error;
    loop {
        if let Some(io_error) = source.downcast_ref::<io::Error>() {
            return matches!(
                io_error.kind(),
                io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded
            );
        }
        let Some(next) = source.source() else {
            return false;
        };
        source = next;
    }
}

/// Recognized by every consumer that only sees a string, not the typed error: the scanner's
/// worker threads (crossing the event channel) and the owner's own admission call both use
/// this exact text once [`is_scan_store_capacity_error`] recognizes their error, so a raw
/// out-of-space error (OS-specific wording, no mention of `--scan-store-mib`) reaches the user
/// exactly like the session's own tracked quota.
pub(crate) const SCAN_STORE_CAPACITY_MESSAGE: &str =
    "scan store capacity exhausted; increase --scan-store-mib";

/// The message a scan-store admission or compaction error becomes once a caller can no longer
/// keep the typed error (a worker thread sending it down the event channel, or an owner
/// storing it as a terminal failure string). Classification happens once, here, while the
/// error is still typed. The session's own tracked quota already explains itself (exact byte
/// counts, which flag to raise), so that detail survives; a raw out-of-space error (no mention
/// of "capacity exhausted") is replaced with the same explanation instead. Any other error
/// keeps its own message unchanged.
pub(crate) fn scan_store_capacity_message(error: &ScanStoreError) -> String {
    let message = error.to_string();
    if is_scan_store_capacity_error(error) && !message.contains("scan store capacity exhausted") {
        SCAN_STORE_CAPACITY_MESSAGE.to_string()
    } else {
        message
    }
}

fn summary_root(run: &mut SealedRun) -> Result<(SummaryMetrics, Coverage), ScanStoreError> {
    let mut root = None;
    run.with_reader(|reader| {
        visit_directory_summaries(reader, |_, summary| {
            if summary.path.is_root() {
                root = Some((summary.metrics, summary.coverage));
            }
            Ok(())
        })
    })?;
    root.ok_or_else(|| {
        ScanStoreError::Recovery("directory summary index omitted the root".to_string())
    })
}

fn new_run_writer(
    temporary_storage: &TemporaryStorage,
    session_storage: Option<&ScanStoreStorage>,
    next_run_id: &Arc<AtomicU64>,
    generation: ScanGeneration,
    kind: RunKind,
    merged: bool,
) -> Result<RunWriter, ScanStoreError> {
    let run_id = next_run_id
        .try_update(Ordering::AcqRel, Ordering::Acquire, |run_id| {
            run_id.checked_add(1)
        })
        .map_err(|_| ScanStoreError::RunIdOverflow)?;
    let reservation = temporary_storage.reservation(0)?;
    let (file, path) = match session_storage {
        Some(storage) => {
            let (file, path) = storage.create_run_file(run_id, merged)?;
            (file, Some(path))
        }
        None => (tempfile::tempfile()?, None),
    };
    Ok(RunWriter::new_with_path(
        file,
        path,
        reservation,
        RunDescriptor::new(generation, run_id, kind),
        RUN_BLOCK_BYTES,
    )?)
}

impl ScanStore {
    fn mark_active_incomplete(&mut self) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        if matches!(
            active.manifest.state(),
            ScanGenerationState::Scanning | ScanGenerationState::Reducing
        ) {
            let _ = active.manifest.transition(ScanGenerationState::Incomplete);
        }
    }

    #[cfg(test)]
    pub(crate) fn active_run_counts(&self) -> Option<(usize, usize)> {
        self.active
            .as_ref()
            .map(|active| (active.path_runs.len(), active.identity_runs.len()))
    }
}

/// Whether removing this entry changes what the map says of another one: a file with, or not
/// known to have, a second link. The map has the files by path, so it cannot say where the other
/// links are, and each of them now has one link fewer than the scan recorded. This reads what
/// the scan observed. A link the file gained since is known only to the deletion that removed
/// it, which reports the link count it read as it removed the file
/// (`DeletionReport::deleted_files_may_have_other_links`): the owner asks for no overlay then.
fn may_have_other_links(path: &PathObservation, identity: Option<&IdentityObservation>) -> bool {
    path.kind != PathEntryKind::Directory
        && (identity.is_some_and(|identity| identity.declared_links != Some(1))
            || path
                .snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.identity.as_ref())
                .and_then(|identity| identity.link_count)
                .is_some_and(|links| links > 1))
}

fn identity_requires_reduction(path: &PathObservation, identity: &IdentityObservation) -> bool {
    identity.declared_links != Some(1)
        || path.metrics.allocated_bytes != identity.allocated_bytes
        || path.metrics.reclaimable_bytes != identity.allocated_bytes
}

fn is_input_kind(kind: RunKind) -> bool {
    matches!(
        kind,
        RunKind::PathObservation | RunKind::IdentityObservation
    )
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use super::*;
    use crate::model::{ByteBounds, EntrySnapshot, NodeKind};
    use crate::native_path::NativeIdentity;
    use crate::scan_coordinator::RelativePath;
    use crate::scan_coordinator::{ScanCoordinator, WorkKey, WorkKind, WorkPriority};
    use crate::scan_store::directory_summary::visit_directory_summaries;
    use crate::scan_store::identity_observation::{
        IdentityObservation, append_identity_observation, visit_identity_observations,
    };
    use crate::scan_store::page::{PageCursor, PageRequest};
    use crate::scan_store::path_observation::append_path_observation;
    use crate::scan_store::path_reducer::{
        Coverage, PathEntryKind, PathObservation, SummaryMetrics,
    };

    fn path(value: &str) -> RelativePath {
        RelativePath::from_path(Path::new(value)).expect("fixture path should be relative")
    }
    const INDEXED_PUBLICATION_STORAGE_BYTES: u64 = 8 * 1024 * 1024;

    fn path_observation(
        path_text: &str,
        kind: PathEntryKind,
        apparent_bytes: u128,
    ) -> PathObservation {
        PathObservation::new(
            path(path_text),
            kind,
            SummaryMetrics::leaf(apparent_bytes, ByteBounds::exact(0), ByteBounds::exact(0)),
            Coverage::Complete,
        )
    }

    fn single_link_path_observation(path_text: &str, file_id: file_id::FileId) -> PathObservation {
        PathObservation::with_snapshot(
            path(path_text),
            PathEntryKind::File,
            SummaryMetrics::leaf(1, ByteBounds::exact(8), ByteBounds::exact(8)),
            Coverage::Complete,
            Some(EntrySnapshot {
                identity: Some(NativeIdentity {
                    file_id,
                    link_count: Some(1),
                    reparse_point: false,
                }),
                kind: NodeKind::File,
                apparent_bytes: 1,
                allocated_bytes: Some(8),
                modified_nanos: None,
            }),
        )
    }

    fn identity_observation(path_text: &str, file_id: file_id::FileId) -> IdentityObservation {
        IdentityObservation {
            path: path(path_text),
            file_id,
            declared_links: Some(1),
            allocated_bytes: ByteBounds::exact(8),
        }
    }

    fn store(storage: TemporaryStorage) -> ScanStore {
        ScanStore::new(ScanGeneration::initial(), storage).expect("store should initialize")
    }

    /// A store whose runs are files of a private session directory below `parent`. An overlay
    /// generation reads the published generation through the paths of its files, so only a store
    /// that keeps its runs in files can build one.
    fn session_directory_store(parent: &Path) -> ScanStore {
        let storage = ScanStoreStorage::new(
            TemporaryStorage::scan_store_with_limit_bytes(INDEXED_PUBLICATION_STORAGE_BYTES),
            Some(parent),
        )
        .expect("private scan session should initialize");
        ScanStore::new_with_storage(ScanGeneration::initial(), storage)
            .expect("store should initialize")
    }

    fn add_path_run(store: &mut ScanStore, observations: &[PathObservation]) {
        let mut writer = store
            .begin_input_run(RunKind::PathObservation)
            .expect("path run should begin");
        let mut key = Vec::new();
        let mut value = Vec::new();
        for observation in observations {
            append_path_observation(&mut writer, observation, &mut key, &mut value)
                .expect("path observations should remain sorted");
        }
        store
            .accept_input_run(writer.seal().expect("path run should seal"))
            .expect("path run should be accepted");
    }

    fn add_identity_run(store: &mut ScanStore, observations: &[IdentityObservation]) {
        let mut writer = store
            .begin_input_run(RunKind::IdentityObservation)
            .expect("identity run should begin");
        let mut key = Vec::new();
        let mut value = Vec::new();
        for observation in observations {
            append_identity_observation(&mut writer, observation, &mut key, &mut value)
                .expect("identity observations should remain sorted");
        }
        store
            .accept_input_run(writer.seal().expect("identity run should seal"))
            .expect("identity run should be accepted");
    }

    /// The description of the generation a store published, as the thread that builds an overlay
    /// receives it from the reader that installed that generation.
    fn overlay_base_of(store: &ScanStore) -> OverlayBase {
        store
            .published()
            .expect("a generation should be published")
            .overlay_base()
            .expect("a store that keeps its runs in files has a base to describe")
    }

    #[test]
    fn provisional_page_replays_active_runs_and_tracks_later_admissions() {
        let mut store = store(TemporaryStorage::with_limit_bytes(
            INDEXED_PUBLICATION_STORAGE_BYTES,
        ));
        let observation = |path_text, kind, bytes| {
            PathObservation::new(
                path(path_text),
                kind,
                SummaryMetrics::leaf(bytes, ByteBounds::exact(bytes), ByteBounds::exact(bytes)),
                Coverage::Complete,
            )
        };
        store
            .append_observation_batch(
                vec![
                    observation("alpha", PathEntryKind::Directory, 0),
                    observation("alpha/leaf", PathEntryKind::File, 7),
                    observation("beta", PathEntryKind::File, 5),
                ],
                Vec::new(),
            )
            .expect("initial canonical facts should append");

        let root = store
            .provisional_page(&PageRequest::first(RelativePath::root(), 2))
            .expect("root provisional page should build from active runs");
        assert_eq!(root.root_coverage, Coverage::Uncertain);
        assert_eq!(root.root_metrics.allocated_bytes.lower, 12);
        assert_eq!(root.root_metrics.descendants, 3);
        assert_eq!(
            root.entries
                .iter()
                .map(|entry| entry.path.clone())
                .collect::<Vec<_>>(),
            vec![path("alpha"), path("beta")]
        );
        assert_eq!(root.entries[0].metrics.allocated_bytes.lower, 7);
        assert_eq!(root.entries[0].metrics.descendants, 1);
        assert_eq!(root.entries[0].coverage, Coverage::Uncertain);

        store
            .append_observation_batch(
                vec![observation("alpha/later", PathEntryKind::File, 11)],
                Vec::new(),
            )
            .expect("later canonical facts should append");
        let root = store
            .provisional_page(&PageRequest::first(RelativePath::root(), 2))
            .expect("cached root provisional page should update");
        assert_eq!(root.entries[0].metrics.allocated_bytes.lower, 18);
        assert_eq!(root.entries[0].metrics.descendants, 2);
        assert_eq!(root.root_metrics.allocated_bytes.lower, 23);
        assert_eq!(root.root_metrics.descendants, 4);

        let alpha = store
            .provisional_page(&PageRequest::first(path("alpha"), 2))
            .expect("a newly selected folder should replay prior active runs");
        assert_eq!(alpha.folder_metrics.allocated_bytes.lower, 18);
        assert_eq!(alpha.folder_metrics.descendants, 2);
        assert_eq!(
            alpha
                .entries
                .iter()
                .map(|entry| entry.path.clone())
                .collect::<Vec<_>>(),
            vec![path("alpha/later"), path("alpha/leaf")]
        );

        let bounded = store
            .provisional_page(&PageRequest::first(RelativePath::root(), 1))
            .expect("live page should honor its fixed entry bound");
        assert_eq!(bounded.entries.len(), 1);
        assert_eq!(bounded.entries[0].path, path("alpha"));
        assert!(matches!(
            store.provisional_page(&PageRequest::after(
                RelativePath::root(),
                PageCursor::at(path("alpha"), 18),
                1,
            )),
            Err(ScanStoreError::Page(ScanPageError::LivePagePagination))
        ));
    }

    #[test]
    fn summary_only_generation_retains_root_directory_metrics() {
        let parent = tempfile::tempdir().expect("session parent should exist");
        let storage = ScanStoreStorage::new(
            TemporaryStorage::scan_store_with_limit_bytes(INDEXED_PUBLICATION_STORAGE_BYTES),
            Some(parent.path()),
        )
        .expect("summary-only session should initialize");
        let mut store = ScanStore::new_with_storage(ScanGeneration::initial(), storage)
            .expect("summary-only store should initialize");
        add_path_run(
            &mut store,
            &[
                path_observation("alpha", PathEntryKind::File, 3),
                path_observation("folder", PathEntryKind::Directory, 0),
                path_observation("folder/beta", PathEntryKind::File, 5),
            ],
        );

        let mut active = store.active.take().expect("generation should be active");
        let (paths, identities, allocations, directories, catalog) = store
            .reduce_active_generation(&mut active, &mut |_| {})
            .expect("directory summaries should reduce");
        store
            .finish_summary_only_generation(
                &active,
                directories,
                (paths, identities, allocations, catalog),
            )
            .expect("summary-only publication should succeed");
        let summary = store
            .summary_only()
            .expect("summary-only generation should be retained");
        assert_eq!(summary.generation(), ScanGeneration::initial());
        assert_eq!(summary.root_metrics().apparent_bytes, 8);
        assert_eq!(summary.root_coverage(), Coverage::Complete);
        assert_eq!(summary.manifest.state(), ScanGenerationState::SummaryOnly);
    }

    #[test]
    fn leased_run_admission_rejects_a_foreign_session_and_accepts_the_active_lease() {
        let storage = TemporaryStorage::with_limit_bytes(INDEXED_PUBLICATION_STORAGE_BYTES);
        let mut store = store(storage);
        let factory = store
            .input_run_factory()
            .expect("initial generation should accept worker runs");
        let generation = factory.generation();
        let foreign_session = ScanSessionId::from_bytes([9; 16]);
        let foreign_lease = lease_for(foreign_session, generation, "entry");
        let foreign_run = factory
            .seal_observation_batch(
                vec![path_observation("entry", PathEntryKind::File, 1)],
                Vec::new(),
            )
            .expect("worker run should seal")
            .into_iter()
            .next()
            .expect("path observation should produce one run");
        assert!(matches!(
            store.accept_leased_input_run(&foreign_lease, foreign_run),
            Err(ScanStoreError::LeaseSessionMismatch)
        ));

        let active_lease = lease_for(store.session(), generation, "entry");
        let active_run = factory
            .seal_observation_batch(
                vec![path_observation("entry", PathEntryKind::File, 1)],
                Vec::new(),
            )
            .expect("worker run should seal")
            .into_iter()
            .next()
            .expect("path observation should produce one run");
        assert!(
            store
                .accept_leased_input_run(&active_lease, active_run)
                .expect("active lease should admit its run")
        );
    }

    fn lease_for(session: ScanSessionId, generation: ScanGeneration, path_text: &str) -> WorkLease {
        let mut coordinator = ScanCoordinator::new(session, generation);
        let key = WorkKey::new(
            session,
            generation,
            WorkKind::EnumerateDirectory,
            path(path_text),
        );
        assert_eq!(
            coordinator.schedule(key, WorkPriority::Background),
            crate::scan_coordinator::ScheduleOutcome::Enqueued
        );
        coordinator
            .lease_next()
            .expect("scheduled test work should receive a lease")
    }

    #[test]
    fn a_sealed_batch_returns_its_in_flight_credit_exactly_once_however_it_ends() {
        let store = store(TemporaryStorage::with_limit_bytes(
            INDEXED_PUBLICATION_STORAGE_BYTES,
        ));
        let factory = store
            .input_run_factory()
            .expect("initial generation should accept worker runs");
        let in_flight = || factory.budget.0.load(Ordering::Acquire);
        let seal = |path_text: &str| {
            factory
                .seal_observation_batch(
                    vec![path_observation(path_text, PathEntryKind::File, 1)],
                    vec![identity_observation(
                        path_text,
                        file_id::FileId::new_inode(1, 1),
                    )],
                )
                .expect("worker batch should seal")
        };
        let discarded = seal("discarded");
        let consumed = seal("consumed");
        let abandoned = seal("abandoned");
        let received = seal("received");
        assert_eq!(in_flight(), 4);

        drop(discarded);
        assert_eq!(in_flight(), 3);

        consumed.into_iter().for_each(drop);
        assert_eq!(in_flight(), 2);

        let first_run = abandoned.into_iter().next();
        assert!(first_run.is_some());
        assert_eq!(in_flight(), 1);

        // A consumer that admits on the thread that received the batch gets the credit back as
        // the batch arrives, while the runs are still to be admitted.
        let runs = received.into_runs_returning_credit();
        assert_eq!(in_flight(), 0);
        assert!(!runs.is_empty(), "the runs themselves are untouched");
        drop(runs);
        assert_eq!(
            in_flight(),
            0,
            "dropping them returns nothing a second time"
        );
    }

    #[test]
    fn private_session_tracks_only_complete_manifest_transitions() {
        let parent = tempfile::tempdir().expect("session parent should exist");
        let storage = ScanStoreStorage::new(
            TemporaryStorage::scan_store_with_limit_bytes(INDEXED_PUBLICATION_STORAGE_BYTES),
            Some(parent.path()),
        )
        .expect("private scan session should initialize");
        let root = storage.root().to_path_buf();
        assert!(root.join("runs").is_dir());
        assert!(root.join("merge").is_dir());

        assert!(root.join("index").is_dir());
        let mut store = ScanStore::new_with_storage(ScanGeneration::initial(), storage.clone())
            .expect("scan store should initialize");
        assert_eq!(
            store
                .active
                .as_ref()
                .expect("a fresh store has an active generation")
                .manifest
                .state(),
            ScanGenerationState::Scanning
        );
        store
            .append_observation_batch(
                vec![path_observation("entry", PathEntryKind::File, 1)],
                Vec::new(),
            )
            .expect("canonical observation should append");
        {
            let active = &store
                .active
                .as_ref()
                .expect("generation should remain active while scanning")
                .manifest;
            assert_eq!(active.state(), ScanGenerationState::Scanning);
            assert_eq!(active.runs().len(), 1);
            assert_eq!(
                active.runs()[0].descriptor.kind(),
                RunKind::PathObservation,
                "an accepted raw run must be tracked before compaction"
            );
        }
        assert_eq!(
            fs::read_dir(root.join("runs"))
                .expect("raw run directory should be readable")
                .count(),
            1,
            "active observations should use a named private-session run"
        );
        store.publish().expect("generation should publish");
        assert_eq!(
            fs::read_dir(root.join("runs"))
                .expect("raw run directory should be readable")
                .count(),
            0,
            "merged raw runs should be released promptly"
        );
        assert!(
            fs::read_dir(root.join("merge"))
                .expect("merged run directory should be readable")
                .next()
                .is_some(),
            "the published generation should retain a named merged run"
        );
        assert!(
            fs::read_dir(root.join("index"))
                .expect("page index directory should be readable")
                .next()
                .is_some(),
            "the published generation should retain a private page index"
        );
        let published = store
            .published
            .as_ref()
            .expect("generation should be retained after publish");
        assert_eq!(published.manifest.state(), ScanGenerationState::Published);
        assert_eq!(
            published.manifest.runs().len(),
            4,
            "the published manifest should retain compact canonical query indexes"
        );
        drop(store);
        drop(storage);
        assert!(!root.exists(), "private session should clean up on drop");
    }
    #[test]
    fn discarded_generations_remain_monotonic_across_rebuilds() {
        let mut store = store(TemporaryStorage::with_limit_bytes(
            INDEXED_PUBLICATION_STORAGE_BYTES,
        ));
        store
            .discard_active()
            .expect("initial generation should close as incomplete");
        assert_eq!(
            store
                .next_generation()
                .expect("next generation should exist"),
            ScanGeneration::from_value(1)
        );
        store
            .begin_generation(ScanGeneration::from_value(1))
            .expect("first rebuild generation should begin");
        store
            .discard_active()
            .expect("first rebuild generation should close as incomplete");
        assert!(matches!(
            store.begin_generation(ScanGeneration::from_value(1)),
            Err(ScanStoreError::NonMonotonicGeneration)
        ));
        assert_eq!(
            store
                .next_generation()
                .expect("second generation should exist"),
            ScanGeneration::from_value(2)
        );
    }

    #[test]
    fn discarding_or_cancelling_the_active_generation_closes_it() {
        let parent = tempfile::tempdir().expect("session parent should exist");

        let incomplete_storage = ScanStoreStorage::new(
            TemporaryStorage::scan_store_with_limit_bytes(512),
            Some(parent.path()),
        )
        .expect("capacity-limited session should initialize");
        let mut incomplete =
            ScanStore::new_with_storage(ScanGeneration::initial(), incomplete_storage)
                .expect("capacity-limited store should initialize");
        let paths = (0..32)
            .map(|index| path_observation(&format!("entry-{index:02}"), PathEntryKind::File, 1))
            .collect();
        let error = incomplete
            .append_observation_batch(paths, Vec::new())
            .expect_err("the bounded input run should exhaust scan-store capacity");
        assert!(is_scan_store_capacity_error(&error));
        incomplete
            .discard_active()
            .expect("an exhausted generation should discard");
        assert!(
            matches!(
                incomplete.input_run_factory(),
                Err(ScanStoreError::NoActiveGeneration)
            ),
            "a discarded generation must no longer accept input"
        );

        let cancelled_storage = ScanStoreStorage::new(
            TemporaryStorage::scan_store_with_limit_bytes(INDEXED_PUBLICATION_STORAGE_BYTES),
            Some(parent.path()),
        )
        .expect("cancellable session should initialize");
        let mut cancelled =
            ScanStore::new_with_storage(ScanGeneration::initial(), cancelled_storage)
                .expect("cancellable store should initialize");
        cancelled
            .cancel_active()
            .expect("an active generation should cancel");
        assert!(
            matches!(
                cancelled.input_run_factory(),
                Err(ScanStoreError::NoActiveGeneration)
            ),
            "a cancelled generation must no longer accept input"
        );
    }

    /// Proves classification is by `io::ErrorKind`, not message text: a raw out-of-space
    /// error with OS-native wording (no mention of "capacity exhausted" or
    /// `--scan-store-mib`) is still recognized, nested exactly as it would be inside a sealed
    /// run's write failure, and both out-of-room kinds normalize to the same message a
    /// filesystem-level quota does too, while an already self-describing quota message keeps
    /// its own detail. An unrelated error kind, and a non-I/O `ScanStoreError` variant, are
    /// both left alone.
    #[test]
    fn capacity_classification_ignores_message_text_and_keys_on_error_kind() {
        let raw_enospc = ScanStoreError::Run(RunError::Io(io::Error::new(
            io::ErrorKind::StorageFull,
            "No space left on device (os error 28)",
        )));
        assert!(is_scan_store_capacity_error(&raw_enospc));
        assert_eq!(
            scan_store_capacity_message(&raw_enospc),
            SCAN_STORE_CAPACITY_MESSAGE
        );

        let raw_edquot = ScanStoreError::Io(io::Error::new(
            io::ErrorKind::QuotaExceeded,
            "Disc quota exceeded (os error 69)",
        ));
        assert!(is_scan_store_capacity_error(&raw_edquot));
        assert_eq!(
            scan_store_capacity_message(&raw_edquot),
            SCAN_STORE_CAPACITY_MESSAGE
        );

        let self_describing = ScanStoreError::Run(RunError::Io(io::Error::new(
            io::ErrorKind::StorageFull,
            "scan store capacity exhausted: 4096 bytes exceed the 2048 byte session limit; \
             increase --scan-store-mib",
        )));
        assert!(is_scan_store_capacity_error(&self_describing));
        assert_eq!(
            scan_store_capacity_message(&self_describing),
            self_describing.to_string(),
            "a message that already names the limit and the flag should keep its detail"
        );

        let unrelated_io = ScanStoreError::Run(RunError::Io(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Permission denied (os error 13)",
        )));
        assert!(!is_scan_store_capacity_error(&unrelated_io));
        assert_eq!(
            scan_store_capacity_message(&unrelated_io),
            unrelated_io.to_string()
        );

        assert!(!is_scan_store_capacity_error(
            &ScanStoreError::BatchTooLarge
        ));
    }
    #[test]
    fn private_manifest_replaces_compacted_runs_before_releasing_inputs() {
        let parent = tempfile::tempdir().expect("session parent should exist");
        let storage = ScanStoreStorage::new(
            TemporaryStorage::scan_store_with_limit_bytes(INDEXED_PUBLICATION_STORAGE_BYTES),
            Some(parent.path()),
        )
        .expect("private scan session should initialize");
        let root = storage.root().to_path_buf();
        let mut store = ScanStore::new_with_storage(ScanGeneration::initial(), storage.clone())
            .expect("scan store should initialize");
        for index in 0..MAX_ACTIVE_INPUT_RUNS {
            store
                .append_observation_batch(
                    vec![path_observation(
                        &format!("entry-{index:02}"),
                        PathEntryKind::File,
                        1,
                    )],
                    Vec::new(),
                )
                .expect("input run should append");
        }

        let active = &store
            .active
            .as_ref()
            .expect("generation should remain active")
            .manifest;
        assert_eq!(active.runs().len(), 1);
        assert_eq!(active.runs()[0].descriptor.kind(), RunKind::PathObservation);
        assert_eq!(
            fs::read_dir(root.join("runs"))
                .expect("raw run directory should be readable")
                .count(),
            0,
            "consumed raw inputs must disappear after their merged successor commits"
        );
        assert_eq!(
            fs::read_dir(root.join("merge"))
                .expect("derived run directory should be readable")
                .count(),
            1,
            "the manifested merged successor should remain available"
        );
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "The publication contract verifies every retained canonical index in one fixture."
    )]
    fn publisher_installs_one_complete_immutable_generation() {
        let mut store = store(TemporaryStorage::with_limit_bytes(
            INDEXED_PUBLICATION_STORAGE_BYTES,
        ));
        add_path_run(
            &mut store,
            &[
                path_observation("alpha", PathEntryKind::Directory, 0),
                path_observation("alpha/file", PathEntryKind::File, 4),
            ],
        );
        let file_id = file_id::FileId::new_inode(1, 2);
        add_identity_run(&mut store, &[identity_observation("alpha/file", file_id)]);

        assert_eq!(
            store.publish().expect("generation should publish"),
            ScanGeneration::initial()
        );
        let published = store
            .published
            .as_ref()
            .expect("generation should be retained");
        assert_eq!(published.manifest.state(), ScanGenerationState::Published);
        assert_eq!(published.manifest.runs().len(), 4);
        assert_eq!(
            published
                .manifest
                .runs()
                .iter()
                .map(|entry| entry.descriptor.kind())
                .collect::<Vec<_>>(),
            vec![
                RunKind::ChildQuery,
                RunKind::PathCatalog,
                RunKind::IdentityObservation,
                RunKind::DirectorySummary,
            ]
        );
        let mut identities = Vec::new();
        store
            .published_mut()
            .expect("published generation should be retained")
            .identity_index
            .with_reader(|reader| {
                visit_identity_observations(reader, |identity| {
                    identities.push(identity);
                    Ok(())
                })
            })
            .expect("identity index should validate");
        assert_eq!(identities.len(), 1);
        assert_eq!(identities[0].path, path("alpha/file"));
        let mut summaries = Vec::new();
        store
            .published_mut()
            .expect("published generation should be retained")
            .directory_summary_index
            .with_reader(|reader| {
                visit_directory_summaries(reader, |_, summary| {
                    summaries.push(summary);
                    Ok(())
                })
            })
            .expect("directory summary index should validate");
        assert!(
            summaries
                .iter()
                .any(|summary| summary.path == path("alpha")),
            "post-order directory summaries should survive publication"
        );
        let page = store
            .published_mut()
            .expect("published generation should be retained")
            .page(PageRequest::first(path("alpha"), 8))
            .expect("indexed page should load");
        assert_eq!(page.entries.len(), 1);
        assert_eq!(
            page.entries[0].metrics.allocated_bytes,
            ByteBounds::exact(8)
        );
        let entry = store
            .published()
            .expect("published generation should remain available")
            .page_entry(&path("alpha/file"))
            .expect("exact canonical lookup should succeed")
            .expect("published child should be found");
        assert_eq!(entry.path, path("alpha/file"));
        assert_eq!(entry.kind, crate::scan_store::page::PageEntryKind::File);
        assert_eq!(entry.metrics.allocated_bytes, ByteBounds::exact(8));
        let catalog = store
            .published()
            .expect("published generation should remain available")
            .path_catalog_entry(&path("alpha/file"))
            .expect("catalog lookup should succeed")
            .expect("catalog row should exist");
        assert_eq!(catalog.parent_id.value(), 1);
        assert_eq!(catalog.name, "file");
        assert!(
            store
                .published()
                .expect("published generation should remain available")
                .page_entry(&RelativePath::root())
                .expect("root lookup should succeed")
                .is_none(),
            "the root is not a deletable concrete entry"
        );
    }

    #[test]
    fn publication_releases_raw_runs_after_building_child_queries() {
        let storage = TemporaryStorage::with_limit_bytes(INDEXED_PUBLICATION_STORAGE_BYTES);
        {
            let mut store = store(storage.clone());
            add_path_run(
                &mut store,
                &[
                    path_observation("folder", PathEntryKind::Directory, 0),
                    path_observation("folder/file", PathEntryKind::File, 4),
                ],
            );
            add_identity_run(
                &mut store,
                &[identity_observation(
                    "folder/file",
                    file_id::FileId::new_inode(7, 9),
                )],
            );
            store.publish().expect("generation should publish");
            let retained = store
                .published()
                .expect("published generation should remain")
                .manifest()
                .runs()
                .iter()
                .map(|run| run.bytes)
                .sum::<u64>();
            assert_eq!(storage.used(), retained);
        }
        assert_eq!(storage.used(), 0);
    }

    #[test]
    fn large_publication_keeps_only_the_child_query_reservation() {
        const ENTRIES: usize = 16_384;
        let storage = TemporaryStorage::with_limit_bytes(64 * 1024 * 1024);
        let mut store = store(storage.clone());
        for first in (0..ENTRIES).step_by(MAX_OBSERVATIONS_PER_BATCH) {
            let paths = (first..first + MAX_OBSERVATIONS_PER_BATCH)
                .map(|index| {
                    PathObservation::new(
                        path(&format!("entry-{index:05}")),
                        PathEntryKind::File,
                        SummaryMetrics::leaf(1, ByteBounds::exact(0), ByteBounds::exact(0)),
                        Coverage::Complete,
                    )
                })
                .collect::<Vec<_>>();
            let identities = (first..first + MAX_OBSERVATIONS_PER_BATCH)
                .map(|index| IdentityObservation {
                    path: path(&format!("entry-{index:05}")),
                    file_id: file_id::FileId::new_inode(
                        7,
                        u64::try_from(index).expect("fixture index fits file identity"),
                    ),
                    declared_links: Some(1),
                    allocated_bytes: ByteBounds::exact(8),
                })
                .collect::<Vec<_>>();
            store
                .append_observation_batch(paths, identities)
                .expect("large bounded batch should append");
        }
        store.publish().expect("large generation should publish");
        let retained = store
            .published()
            .expect("published generation should remain")
            .manifest()
            .runs()
            .iter()
            .map(|run| run.bytes)
            .sum::<u64>();
        assert_eq!(storage.used(), retained);
        let late = store
            .published()
            .expect("published generation should remain")
            .page(PageRequest::after(
                RelativePath::root(),
                PageCursor::at(path("entry-16000"), 8),
                16,
            ))
            .expect("late concrete page should remain available");
        assert_eq!(late.entries[0].path, path("entry-16001"));
        assert_eq!(
            late.entries[0].metrics.allocated_bytes,
            ByteBounds::exact(8)
        );
    }

    #[test]
    fn large_single_link_input_skips_duplicate_identity_runs() {
        const ENTRIES: usize = 16_384;
        let direct_storage = TemporaryStorage::with_limit_bytes(64 * 1024 * 1024);
        let mut direct = store(direct_storage.clone());
        for first in (0..ENTRIES).step_by(MAX_OBSERVATIONS_PER_BATCH) {
            let paths = (first..first + MAX_OBSERVATIONS_PER_BATCH)
                .map(|index| {
                    single_link_path_observation(
                        &format!("entry-{index:05}"),
                        file_id::FileId::new_inode(
                            7,
                            u64::try_from(index).expect("fixture index fits file identity"),
                        ),
                    )
                })
                .collect::<Vec<_>>();
            direct
                .append_observation_batch(paths, Vec::new())
                .expect("single-link batch should append without identity records");
        }
        assert_eq!(
            direct.active_run_counts().map(|(_, identities)| identities),
            Some(0)
        );
        let direct_input_bytes = direct_storage.used();

        let duplicate_storage = TemporaryStorage::with_limit_bytes(64 * 1024 * 1024);
        let mut duplicate = store(duplicate_storage.clone());
        for first in (0..ENTRIES).step_by(MAX_OBSERVATIONS_PER_BATCH) {
            let paths = (first..first + MAX_OBSERVATIONS_PER_BATCH)
                .map(|index| {
                    single_link_path_observation(
                        &format!("entry-{index:05}"),
                        file_id::FileId::new_inode(
                            7,
                            u64::try_from(index).expect("fixture index fits file identity"),
                        ),
                    )
                })
                .collect::<Vec<_>>();
            let identities = (first..first + MAX_OBSERVATIONS_PER_BATCH)
                .map(|index| {
                    identity_observation(
                        &format!("entry-{index:05}"),
                        file_id::FileId::new_inode(
                            7,
                            u64::try_from(index).expect("fixture index fits file identity"),
                        ),
                    )
                })
                .collect::<Vec<_>>();
            duplicate
                .append_observation_batch(paths, identities)
                .expect("duplicate input batch should append");
        }
        assert!(duplicate_storage.used() > direct_input_bytes);

        direct
            .publish()
            .expect("large single-link generation should publish");
        let late = direct
            .published()
            .expect("published generation should remain")
            .page(PageRequest::after(
                RelativePath::root(),
                PageCursor::at(path("entry-16000"), 8),
                16,
            ))
            .expect("late concrete page should load");
        assert_eq!(late.entries[0].path, path("entry-16001"));
        assert_eq!(
            late.entries[0].metrics.allocated_bytes,
            ByteBounds::exact(8)
        );
        assert_eq!(
            late.entries[0].metrics.reclaimable_bytes,
            ByteBounds::exact(8)
        );
    }

    #[test]
    fn active_input_families_compact_at_a_fixed_fan_in() {
        let storage = TemporaryStorage::with_limit_bytes(512 * 1024);
        let mut store = store(storage);
        for index in 0..MAX_ACTIVE_INPUT_RUNS {
            add_path_run(
                &mut store,
                &[path_observation(
                    &format!("entry-{index:02}"),
                    PathEntryKind::File,
                    1,
                )],
            );
        }
        assert_eq!(store.active_run_counts(), Some((1, 0)));
    }
    #[test]
    fn input_runs_compact_by_size_tier() {
        let storage = TemporaryStorage::with_limit_bytes(INDEXED_PUBLICATION_STORAGE_BYTES);
        let mut store = store(storage);
        let input_runs = MAX_ACTIVE_INPUT_RUNS.saturating_mul(MAX_ACTIVE_INPUT_RUNS);
        for index in 0..input_runs {
            add_path_run(
                &mut store,
                &[path_observation(
                    &format!("entry-{index:03}"),
                    PathEntryKind::File,
                    1,
                )],
            );
        }
        {
            let active = store
                .active
                .as_ref()
                .expect("generation should remain active");
            assert_eq!(active.path_runs.levels[0].len(), 0);
            assert_eq!(active.path_runs.levels[1].len(), 0);
            assert_eq!(active.path_runs.levels[2].len(), 1);
        }
        store.publish().expect("tiered runs should publish");
        let page = store
            .published_mut()
            .expect("published generation should remain available")
            .page(PageRequest::first(RelativePath::root(), input_runs))
            .expect("published page should contain every concrete run entry");
        assert_eq!(page.entries.len(), input_runs);
    }

    #[test]
    fn newer_active_generation_keeps_prior_published_snapshot_and_rejects_stale_runs() {
        let storage = TemporaryStorage::with_limit_bytes(INDEXED_PUBLICATION_STORAGE_BYTES);
        let mut store = store(storage.clone());
        store.publish().expect("empty generation should publish");
        let next = ScanGeneration::from_value(1);
        store
            .begin_generation(next)
            .expect("newer generation should begin");
        assert_eq!(
            store.published_generation(),
            Some(ScanGeneration::initial())
        );
        assert_eq!(store.active_generation(), Some(next));

        let stale = RunWriter::new(
            tempfile::tempfile().expect("temporary stale run should open"),
            storage
                .reservation(0)
                .expect("stale reservation should fit"),
            RunDescriptor::new(ScanGeneration::initial(), 99, RunKind::PathObservation),
            RUN_BLOCK_BYTES,
        )
        .expect("stale writer should initialize")
        .seal()
        .expect("stale writer should seal");
        assert!(matches!(
            store.accept_input_run(stale),
            Err(ScanStoreError::GenerationMismatch)
        ));
    }
    #[test]
    fn overlay_generation_replaces_only_the_focused_subtree() {
        let parent = tempfile::tempdir().expect("session parent should exist");
        let mut store = session_directory_store(parent.path());
        add_path_run(
            &mut store,
            &[
                path_observation("alpha", PathEntryKind::Directory, 0),
                path_observation("alpha/old", PathEntryKind::File, 4),
                path_observation("beta", PathEntryKind::File, 3),
            ],
        );
        add_identity_run(
            &mut store,
            &[
                identity_observation("alpha/old", file_id::FileId::new_inode(1, 1)),
                identity_observation("beta", file_id::FileId::new_inode(2, 2)),
            ],
        );
        store.publish().expect("base generation should publish");

        let next = ScanGeneration::from_value(1);
        let base = overlay_base_of(&store);
        store
            .begin_overlay_generation(&base, next, &path("alpha"), None)
            .expect("focused overlay should start");
        add_path_run(
            &mut store,
            &[
                path_observation("alpha", PathEntryKind::Directory, 0),
                path_observation("alpha/new", PathEntryKind::File, 9),
            ],
        );
        add_identity_run(
            &mut store,
            &[identity_observation(
                "alpha/new",
                file_id::FileId::new_inode(3, 3),
            )],
        );
        assert_eq!(store.publish().expect("overlay should publish"), next);

        let root = store
            .published_mut()
            .expect("overlay should be published")
            .page(PageRequest::first(RelativePath::root(), 8))
            .expect("root page should load");
        assert_eq!(
            root.entries
                .iter()
                .map(|entry| entry.path.clone())
                .collect::<Vec<_>>(),
            vec![path("alpha"), path("beta")]
        );
        let beta = root
            .entries
            .iter()
            .find(|entry| entry.path == path("beta"))
            .expect("retained sibling should remain concrete");
        assert_eq!(beta.metrics.allocated_bytes, ByteBounds::exact(8));
        let alpha = store
            .published_mut()
            .expect("overlay should be published")
            .page(PageRequest::first(path("alpha"), 8))
            .expect("focused page should load");
        assert_eq!(
            alpha
                .entries
                .iter()
                .map(|entry| entry.path.clone())
                .collect::<Vec<_>>(),
            vec![path("alpha/new")]
        );
    }

    #[test]
    fn overlay_generation_keeps_an_unrelated_unreadable_directory_uncertain() {
        let parent = tempfile::tempdir().expect("session parent should exist");
        let mut store = session_directory_store(parent.path());
        add_path_run(
            &mut store,
            &[
                path_observation("alpha", PathEntryKind::Directory, 0),
                path_observation("alpha/old", PathEntryKind::File, 4),
                path_observation("beta", PathEntryKind::Directory, 0),
                path_observation("beta/locked", PathEntryKind::Directory, 0),
            ],
        );
        store.record_unreadable_directory(path("beta/locked"));
        store.publish().expect("base generation should publish");

        let base_beta = store
            .published_mut()
            .expect("base generation should be published")
            .page(PageRequest::first(path("beta"), 8))
            .expect("beta page should load");
        assert_eq!(base_beta.folder_coverage, Coverage::Uncertain);

        let next = ScanGeneration::from_value(1);
        let base = overlay_base_of(&store);
        store
            .begin_overlay_generation(&base, next, &path("alpha"), None)
            .expect("focused overlay should start");
        add_path_run(
            &mut store,
            &[
                path_observation("alpha", PathEntryKind::Directory, 0),
                path_observation("alpha/new", PathEntryKind::File, 9),
            ],
        );
        assert_eq!(store.publish().expect("overlay should publish"), next);

        let overlay_beta = store
            .published_mut()
            .expect("overlay should be published")
            .page(PageRequest::first(path("beta"), 8))
            .expect("beta page should still load after an unrelated overlay");
        assert_eq!(
            overlay_beta.folder_coverage,
            Coverage::Uncertain,
            "an unreadable directory outside the overlay's replaced prefix must stay uncertain"
        );
        assert_eq!(overlay_beta.folder_metrics.allocated_bytes.upper, None);
        let locked = overlay_beta
            .entries
            .iter()
            .find(|entry| entry.path == path("beta/locked"))
            .expect("the unreadable directory itself should still be listed");
        assert_eq!(locked.coverage, Coverage::Uncertain);
    }

    fn folder_observation(
        path_text: &str,
        file_id: file_id::FileId,
        link_count: u64,
        modified_nanos: u128,
    ) -> PathObservation {
        PathObservation::with_snapshot(
            path(path_text),
            PathEntryKind::Directory,
            SummaryMetrics::leaf(0, ByteBounds::exact(0), ByteBounds::exact(0)),
            Coverage::Complete,
            Some(folder_snapshot(file_id, link_count, modified_nanos)),
        )
    }

    fn folder_snapshot(
        file_id: file_id::FileId,
        link_count: u64,
        modified_nanos: u128,
    ) -> EntrySnapshot {
        EntrySnapshot {
            identity: Some(NativeIdentity {
                file_id,
                link_count: Some(link_count),
                reparse_point: false,
            }),
            kind: NodeKind::Directory,
            apparent_bytes: 0,
            allocated_bytes: Some(0),
            modified_nanos: Some(modified_nanos),
        }
    }

    /// A store whose map has the folder `holder` (two links: one more than its own, for the one
    /// folder inside it) with a folder `inner` and a file `leaf` below it, and a file `stay`
    /// beside the folder, published.
    fn store_with_a_folder_inside_a_folder(parent: &Path) -> ScanStore {
        let mut store = session_directory_store(parent);
        add_path_run(
            &mut store,
            &[
                folder_observation("holder", file_id::FileId::new_inode(1, 1), 3, 100),
                folder_observation("holder/inner", file_id::FileId::new_inode(1, 2), 2, 100),
                path_observation("holder/inner/leaf", PathEntryKind::File, 4),
                single_link_path_observation("holder/stay", file_id::FileId::new_inode(1, 3)),
            ],
        );
        store.publish().expect("base generation should publish");
        store
    }

    /// What `holder` holds once the folder inside it is gone: the file `stay`, as the file
    /// system names it.
    fn holder_entries_without_inner() -> FolderDigest {
        let mut entries = FolderDigest::default();
        entries.add(
            std::ffi::OsStr::new("stay"),
            PathEntryKind::File,
            Some(&file_id::FileId::new_inode(1, 3)),
        );
        entries
    }

    /// Another removal from the same folder can already be gone from the file system while this
    /// overlay is built, and the folder is then checked against the map less both: the entry
    /// of the later removal stays in this overlay's map, for its own overlay to drop.
    #[test]
    fn an_overlay_checks_the_folder_against_the_map_less_the_removals_that_follow() {
        let parent = tempfile::tempdir().expect("session parent should exist");
        let mut store = store_with_a_folder_inside_a_folder(parent.path());
        let folder = RefreshedFolder {
            path: path("holder"),
            snapshot: folder_snapshot(file_id::FileId::new_inode(1, 1), 2, 200),
            // `stay` is gone from the file system already: its removal follows.
            entries: FolderDigest::default(),
            removed_later: vec![path("holder/stay")],
        };

        let next = ScanGeneration::from_value(1);
        let base = overlay_base_of(&store);
        store
            .begin_overlay_generation(&base, next, &path("holder/inner"), Some(&folder))
            .expect("the folder holds what the map lists of it, less both removals");
        assert_eq!(store.publish().expect("overlay should publish"), next);
        let page = store
            .published_mut()
            .expect("overlay should be published")
            .page(PageRequest::first(path("holder"), 8))
            .expect("the folder's page should load");
        assert_eq!(
            page.entries
                .iter()
                .map(|entry| entry.path.clone())
                .collect::<Vec<_>>(),
            vec![path("holder/stay")],
            "the later removal's entry is left for its own overlay"
        );
    }

    /// The scan root has no record of its own, so what is checked of it is its entries: the map's
    /// entries of the root less the removal must be what the root holds now.
    #[test]
    fn an_overlay_can_check_the_scan_root_against_the_map() {
        let parent = tempfile::tempdir().expect("session parent should exist");
        let mut store = store_with_a_folder_inside_a_folder(parent.path());
        let snapshot = folder_snapshot(file_id::FileId::new_inode(1, 7), 2, 200);
        let mut intruder = FolderDigest::default();
        intruder.add(
            std::ffi::OsStr::new("intruder"),
            PathEntryKind::File,
            Some(&file_id::FileId::new_inode(1, 9)),
        );
        let base = overlay_base_of(&store);
        let refused = store.begin_overlay_generation(
            &base,
            ScanGeneration::from_value(1),
            &path("holder"),
            Some(&RefreshedFolder {
                path: RelativePath::root(),
                snapshot: snapshot.clone(),
                entries: intruder,
                removed_later: Vec::new(),
            }),
        );
        assert!(matches!(
            refused,
            Err(ScanStoreError::OverlayNeedsRescan(_))
        ));

        let base = overlay_base_of(&store);
        store
            .begin_overlay_generation(
                &base,
                ScanGeneration::from_value(2),
                &path("holder"),
                Some(&RefreshedFolder {
                    path: RelativePath::root(),
                    snapshot,
                    entries: FolderDigest::default(),
                    removed_later: Vec::new(),
                }),
            )
            .expect("the root holds what the map lists of it, less the removal");
        assert_eq!(
            store.publish().expect("overlay should publish"),
            ScanGeneration::from_value(2)
        );
    }

    /// A change to another entry of the folder is still one the map cannot describe, however many
    /// removals follow.
    #[test]
    fn an_overlay_with_removals_to_follow_still_refuses_a_folder_that_changed() {
        let parent = tempfile::tempdir().expect("session parent should exist");
        let mut store = store_with_a_folder_inside_a_folder(parent.path());
        let mut entries = FolderDigest::default();
        entries.add(
            std::ffi::OsStr::new("intruder"),
            PathEntryKind::File,
            Some(&file_id::FileId::new_inode(1, 9)),
        );
        let folder = RefreshedFolder {
            path: path("holder"),
            snapshot: folder_snapshot(file_id::FileId::new_inode(1, 1), 2, 200),
            entries,
            removed_later: vec![path("holder/stay")],
        };

        let base = overlay_base_of(&store);
        let result = store.begin_overlay_generation(
            &base,
            ScanGeneration::from_value(1),
            &path("holder/inner"),
            Some(&folder),
        );

        assert!(matches!(result, Err(ScanStoreError::OverlayNeedsRescan(_))));
    }

    /// Removing an entry changes the folder that held it, and the map a deletion leaves says what
    /// the folder is now: the check that the reader is deleting what they saw compares it.
    #[test]
    fn an_overlay_records_the_folder_that_held_the_removed_entry_as_it_is_now() {
        let parent = tempfile::tempdir().expect("session parent should exist");
        let mut store = store_with_a_folder_inside_a_folder(parent.path());
        let now = folder_snapshot(file_id::FileId::new_inode(1, 1), 2, 200);
        let folder = RefreshedFolder {
            path: path("holder"),
            snapshot: now.clone(),
            entries: holder_entries_without_inner(),
            removed_later: Vec::new(),
        };

        let next = ScanGeneration::from_value(1);
        let base = overlay_base_of(&store);
        store
            .begin_overlay_generation(&base, next, &path("holder/inner"), Some(&folder))
            .expect("the overlay should start");
        assert_eq!(store.publish().expect("overlay should publish"), next);

        let root = store
            .published_mut()
            .expect("overlay should be published")
            .page(PageRequest::first(RelativePath::root(), 8))
            .expect("root page should load");
        let holder = root
            .entries
            .iter()
            .find(|entry| entry.path == path("holder"))
            .expect("the folder should still be listed");
        assert_eq!(holder.snapshot, Some(now));
        let page = store
            .published_mut()
            .expect("overlay should be published")
            .page(PageRequest::first(path("holder"), 8))
            .expect("the folder's page should load");
        assert_eq!(
            page.entries
                .iter()
                .map(|entry| entry.path.clone())
                .collect::<Vec<_>>(),
            vec![path("holder/stay")],
            "only what was removed leaves the map"
        );
    }

    /// A folder that is not the one the map recorded, or not in it at all, is what the map cannot
    /// describe: the overlay says so, and the map the reader has stays.
    #[test]
    fn an_overlay_cannot_describe_a_folder_that_was_replaced_or_that_the_map_lacks() {
        let parent = tempfile::tempdir().expect("session parent should exist");
        let mut store = store_with_a_folder_inside_a_folder(parent.path());
        let replaced = RefreshedFolder {
            path: path("holder"),
            snapshot: folder_snapshot(file_id::FileId::new_inode(1, 99), 2, 200),
            entries: holder_entries_without_inner(),
            removed_later: Vec::new(),
        };
        let elsewhere = RefreshedFolder {
            path: path("elsewhere"),
            snapshot: folder_snapshot(file_id::FileId::new_inode(1, 1), 2, 200),
            entries: holder_entries_without_inner(),
            removed_later: Vec::new(),
        };
        let base = overlay_base_of(&store);

        for (generation, folder) in [(1, replaced), (2, elsewhere)] {
            let error = store
                .begin_overlay_generation(
                    &base,
                    ScanGeneration::from_value(generation),
                    &path("holder/inner"),
                    Some(&folder),
                )
                .expect_err("the overlay cannot be exact");
            assert!(
                matches!(error, ScanStoreError::OverlayNeedsRescan(_)),
                "{error:?}"
            );
            assert_eq!(
                store.published_generation(),
                Some(ScanGeneration::initial()),
                "the map the reader has stays"
            );
        }
    }

    /// The folder's modification time describes whatever happened to its entries, not only the
    /// removal: the overlay records it only for a folder that holds the entries the map lists
    /// for it, less the removed one. Any other folder is what the map cannot describe.
    #[test]
    fn an_overlay_cannot_describe_a_folder_that_holds_other_entries_than_the_map_lists() {
        type Entry = (&'static str, PathEntryKind, Option<file_id::FileId>);
        let parent = tempfile::tempdir().expect("session parent should exist");
        let mut store = store_with_a_folder_inside_a_folder(parent.path());
        let base = overlay_base_of(&store);
        let inode = |number| Some(file_id::FileId::new_inode(1, number));
        let stay: Entry = ("stay", PathEntryKind::File, inode(3));
        let cases: [(&str, Vec<Entry>); 6] = [
            (
                "an entry the map does not list",
                vec![stay, ("new", PathEntryKind::File, inode(4))],
            ),
            ("an entry the map lists is gone", Vec::new()),
            (
                "an entry the map lists has another name",
                vec![("renamed", PathEntryKind::File, inode(3))],
            ),
            (
                "an entry the map lists was replaced by another of its name",
                vec![("stay", PathEntryKind::File, inode(99))],
            ),
            (
                "the removed entry is back",
                vec![stay, ("inner", PathEntryKind::Directory, inode(2))],
            ),
            (
                "an entry the map lists is not the kind it was",
                vec![("stay", PathEntryKind::Link, inode(3))],
            ),
        ];

        for (generation, (what, entries)) in (1..).zip(cases) {
            let mut digest = FolderDigest::default();
            for (name, kind, identity) in &entries {
                digest.add(std::ffi::OsStr::new(name), *kind, identity.as_ref());
            }
            let folder = RefreshedFolder {
                path: path("holder"),
                snapshot: folder_snapshot(file_id::FileId::new_inode(1, 1), 2, 200),
                entries: digest,
                removed_later: Vec::new(),
            };

            let error = store
                .begin_overlay_generation(
                    &base,
                    ScanGeneration::from_value(generation),
                    &path("holder/inner"),
                    Some(&folder),
                )
                .expect_err(what);

            assert!(
                matches!(error, ScanStoreError::OverlayNeedsRescan(_)),
                "{what}: {error:?}"
            );
            assert_eq!(
                store.published_generation(),
                Some(ScanGeneration::initial()),
                "{what}: the map the reader has stays"
            );
        }
    }

    /// Removing one link to a file changes the link count of every other link to it, and the map
    /// does not say where they are: it has the files by path, with the count the scan read.
    #[test]
    fn an_overlay_that_removes_a_hard_link_asks_for_a_rescan() {
        let parent = tempfile::tempdir().expect("session parent should exist");
        let mut store = session_directory_store(parent.path());
        let first = file_id::FileId::new_inode(7, 7);
        add_path_run(
            &mut store,
            &[
                folder_observation("holder", file_id::FileId::new_inode(1, 1), 2, 100),
                path_observation("holder/first", PathEntryKind::File, 3),
                path_observation("holder/second", PathEntryKind::File, 3),
            ],
        );
        add_identity_run(
            &mut store,
            &[
                IdentityObservation {
                    path: path("holder/first"),
                    file_id: first,
                    declared_links: Some(2),
                    allocated_bytes: ByteBounds::exact(8),
                },
                IdentityObservation {
                    path: path("holder/second"),
                    file_id: first,
                    declared_links: Some(2),
                    allocated_bytes: ByteBounds::exact(8),
                },
            ],
        );
        store.publish().expect("base generation should publish");

        let base = overlay_base_of(&store);
        let error = store
            .begin_overlay_generation(
                &base,
                ScanGeneration::from_value(1),
                &path("holder/first"),
                None,
            )
            .expect_err("the overlay cannot say what the other link has now");

        assert!(
            matches!(error, ScanStoreError::OverlayNeedsRescan(_)),
            "{error:?}"
        );
    }

    #[test]
    fn past_the_tracking_cap_every_folder_becomes_uncertain_instead_of_only_the_tracked_ones() {
        let mut store = store(TemporaryStorage::with_limit_bytes(
            INDEXED_PUBLICATION_STORAGE_BYTES,
        ));
        add_path_run(
            &mut store,
            &[
                path_observation("alpha", PathEntryKind::Directory, 0),
                path_observation("alpha/locked-one", PathEntryKind::Directory, 0),
                path_observation("beta", PathEntryKind::Directory, 0),
                path_observation("beta/locked-two", PathEntryKind::Directory, 0),
                path_observation("gamma", PathEntryKind::Directory, 0),
                path_observation("gamma/untouched", PathEntryKind::File, 4),
            ],
        );
        // A cap of 1 is reached by the second distinct unreadable directory,
        // well before any real scan would create thousands of them.
        store.record_unreadable_directory_with_cap(path("alpha/locked-one"), 1);
        store.record_unreadable_directory_with_cap(path("beta/locked-two"), 1);
        store.publish().expect("generation should publish");

        let gamma = store
            .published_mut()
            .expect("generation should be published")
            .page(PageRequest::first(path("gamma"), 8))
            .expect("gamma page should load");
        assert_eq!(
            gamma.folder_coverage,
            Coverage::Uncertain,
            "past the cap, even an untouched folder must be uncertain rather than trusted as complete"
        );
        assert_eq!(gamma.folder_metrics.allocated_bytes.upper, None);
        assert_eq!(
            gamma.root_coverage,
            Coverage::Uncertain,
            "the root is also uncertain once tracking overflows"
        );
        let untouched = gamma
            .entries
            .iter()
            .find(|entry| entry.path == path("gamma/untouched"))
            .expect("the untouched file should still be listed");
        assert_eq!(
            untouched.metrics.apparent_bytes, 4,
            "a leaf file's own, directly observed size is unaffected by the fallback"
        );
    }
    #[test]
    fn shuffled_batches_publish_identical_tree_map_pages() {
        let paths = [
            path_observation("alpha", PathEntryKind::Directory, 0),
            path_observation("alpha/first", PathEntryKind::File, 3),
            path_observation("beta", PathEntryKind::Directory, 0),
            path_observation("beta/second", PathEntryKind::File, 5),
        ];
        let shared = file_id::FileId::new_inode(256, 2);
        let identities = [
            IdentityObservation {
                path: path("alpha/first"),
                file_id: shared,
                declared_links: Some(2),
                allocated_bytes: ByteBounds::exact(8),
            },
            IdentityObservation {
                path: path("beta/second"),
                file_id: shared,
                declared_links: Some(2),
                allocated_bytes: ByteBounds::exact(8),
            },
        ];
        let mut first = store(TemporaryStorage::with_limit_bytes(
            INDEXED_PUBLICATION_STORAGE_BYTES,
        ));
        first
            .append_observation_batch(
                vec![
                    paths[3].clone(),
                    paths[0].clone(),
                    paths[2].clone(),
                    paths[1].clone(),
                ],
                vec![identities[1].clone(), identities[0].clone()],
            )
            .expect("shuffled batch should be accepted");
        first.publish().expect("first generation should publish");

        let mut second = store(TemporaryStorage::with_limit_bytes(
            INDEXED_PUBLICATION_STORAGE_BYTES,
        ));
        second
            .append_observation_batch(
                vec![paths[2].clone(), paths[3].clone()],
                vec![identities[1].clone()],
            )
            .expect("first shuffled chunk should be accepted");
        second
            .append_observation_batch(
                vec![paths[1].clone(), paths[0].clone()],
                vec![identities[0].clone()],
            )
            .expect("second shuffled chunk should be accepted");
        second.publish().expect("second generation should publish");

        let first_root = first
            .published_mut()
            .expect("first generation should be retained")
            .page(PageRequest::first(RelativePath::root(), 8))
            .expect("first root page should load");
        let second_root = second
            .published_mut()
            .expect("second generation should be retained")
            .page(PageRequest::first(RelativePath::root(), 8))
            .expect("second root page should load");
        assert_eq!(first_root, second_root);
        let first_alpha = first
            .published_mut()
            .expect("first generation should be retained")
            .page(PageRequest::first(path("alpha"), 8))
            .expect("first nested page should load");
        let second_alpha = second
            .published_mut()
            .expect("second generation should be retained")
            .page(PageRequest::first(path("alpha"), 8))
            .expect("second nested page should load");
        assert_eq!(first_alpha, second_alpha);
    }
}
