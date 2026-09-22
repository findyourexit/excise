use std::collections::HashSet;
use std::error::Error as StdError;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use thiserror::Error;

use super::directory_summary::{
    DirectorySummaryRunError, reduce_path_observation_run, visit_directory_summaries,
};
use super::identity_observation::{
    IdentityObservation, IdentityObservationRunError, IdentityReductionError,
    append_identity_observation, compare_identity_observations, reduce_identity_observations,
};
use super::manifest::{ManifestError, RunManifestEntry, ScanManifest};
use super::page::{
    PageIndexError, PageRequest, ProvisionalPage, ScanPage, ScanPageEntry, ScanPageError,
    materialize_child_queries, read_child_query_root_metadata, read_page_entry,
    visit_child_query_entries, visit_child_query_page_entries,
};
use super::path_catalog::{
    PathCatalogEntry, PathCatalogError, build_path_catalog, read_path_catalog_entry,
};
use super::path_observation::{PathObservationRunError, append_path_observation};
use super::path_reducer::{Coverage, PathObservation, SummaryMetrics};
use super::run_file::{RunDescriptor, RunError, RunKind, RunWriter, SealedRun};
use super::run_merge::{RunMergeError, merge_sorted_runs};
use crate::scan_coordinator::ScanGeneration;
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

#[derive(Debug, Error)]
pub(crate) enum ScanStoreError {
    #[error(transparent)]
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
}

/// Logical sealed-run I/O used by the internal benchmark harness.
///
/// The counters measure serialized run bytes rather than operating-system
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

    fn with_child_query_entries<E>(
        &mut self,
        visit: impl FnMut(PathObservation, Option<IdentityObservation>) -> Result<(), E>,
    ) -> Result<(), E>
    where
        E: From<RunError> + From<PageIndexError>,
    {
        visit_child_query_entries(&mut self.child_queries, visit)
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

    #[cfg(test)]
    fn preserve_for_recovery(&mut self) {
        self.child_queries.preserve_path_for_recovery();
        self.path_catalog.preserve_path_for_recovery();
        self.identity_index.preserve_path_for_recovery();
        self.directory_summary_index.preserve_path_for_recovery();
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
            accepted_run_ids: HashSet::new(),
            provisional_page: None,
        }
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
}

impl ScanInputRunFactory {
    #[must_use]
    pub(crate) const fn generation(&self) -> ScanGeneration {
        self.generation
    }

    /// Seals the two bounded raw fact families for one scanner batch.
    ///
    /// # Errors
    ///
    /// Returns an error when a batch is oversized, a run cannot be written, or
    /// the shared run identifier sequence is exhausted.
    pub(crate) fn seal_observation_batch(
        &self,
        mut paths: Vec<PathObservation>,
        mut identities: Vec<IdentityObservation>,
    ) -> Result<Vec<SealedRun>, ScanStoreError> {
        if paths.len() > MAX_OBSERVATIONS_PER_BATCH || identities.len() > MAX_OBSERVATIONS_PER_BATCH
        {
            return Err(ScanStoreError::BatchTooLarge);
        }
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
        if let Some(storage) = session_storage.as_ref() {
            storage.persist_manifest(&active.manifest.encode()?)?;
        }
        Ok(Self {
            temporary_storage,
            session,
            session_storage,
            next_run_id: Arc::new(AtomicU64::new(0)),
            active: Some(active),
            published: None,
            summary_only: None,
            retired_generation: None,
            #[cfg(feature = "internal")]
            io_metrics: ScanStoreIoMetrics::default(),
        })
    }

    #[cfg(feature = "internal")]
    #[must_use]
    pub(crate) const fn io_metrics(&self) -> ScanStoreIoMetrics {
        self.io_metrics
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

    #[allow(
        clippy::too_many_lines,
        reason = "Recovery validates every published run and its root metadata together so an interrupted session cannot expose a partial generation."
    )]
    /// Reopens the last complete immutable generation from a private session.
    pub(crate) fn recover_published(storage: ScanStoreStorage) -> Result<Self, ScanStoreError> {
        let manifest = ScanManifest::decode(&storage.read_manifest()?)?;
        if manifest.state() != ScanGenerationState::Published {
            return Err(ScanStoreError::Recovery(
                "manifest does not contain a published generation".to_string(),
            ));
        }
        let child_entry = manifest
            .runs()
            .iter()
            .copied()
            .find(|entry| entry.descriptor.kind() == RunKind::ChildQuery)
            .ok_or_else(|| {
                ScanStoreError::Recovery(
                    "published manifest does not reference a child-query run".to_string(),
                )
            })?;
        let catalog_entry = manifest
            .runs()
            .iter()
            .copied()
            .find(|entry| entry.descriptor.kind() == RunKind::PathCatalog)
            .ok_or_else(|| {
                ScanStoreError::Recovery(
                    "published manifest does not reference a path-catalog run".to_string(),
                )
            })?;
        let identity_entry = manifest
            .runs()
            .iter()
            .copied()
            .find(|entry| entry.descriptor.kind() == RunKind::IdentityObservation)
            .ok_or_else(|| {
                ScanStoreError::Recovery(
                    "published manifest does not reference an identity index".to_string(),
                )
            })?;
        let directory_entry = manifest
            .runs()
            .iter()
            .copied()
            .find(|entry| entry.descriptor.kind() == RunKind::DirectorySummary)
            .ok_or_else(|| {
                ScanStoreError::Recovery(
                    "published manifest does not reference a directory summary index".to_string(),
                )
            })?;
        if manifest.runs().len() != 4 {
            return Err(ScanStoreError::Recovery(
                "published manifest retains obsolete runs".to_string(),
            ));
        }
        let reservation = storage.quota().reservation(child_entry.bytes)?;
        let (file, path) = storage.open_run_file(child_entry.descriptor.run_id(), true)?;
        let child_queries = SealedRun::open_named(
            file,
            path,
            reservation,
            child_entry.descriptor,
            child_entry.bytes,
        )?;
        let catalog_reservation = storage.quota().reservation(catalog_entry.bytes)?;
        let (catalog_file, catalog_path) =
            storage.open_run_file(catalog_entry.descriptor.run_id(), true)?;
        let path_catalog = SealedRun::open_named(
            catalog_file,
            catalog_path,
            catalog_reservation,
            catalog_entry.descriptor,
            catalog_entry.bytes,
        )?;
        let identity_reservation = storage.quota().reservation(identity_entry.bytes)?;
        let (identity_file, identity_path) =
            storage.open_run_file(identity_entry.descriptor.run_id(), true)?;
        let identity_index = SealedRun::open_named(
            identity_file,
            identity_path,
            identity_reservation,
            identity_entry.descriptor,
            identity_entry.bytes,
        )?;
        let directory_reservation = storage.quota().reservation(directory_entry.bytes)?;
        let (directory_file, directory_path) =
            storage.open_run_file(directory_entry.descriptor.run_id(), true)?;
        let directory_summary_index = SealedRun::open_named(
            directory_file,
            directory_path,
            directory_reservation,
            directory_entry.descriptor,
            directory_entry.bytes,
        )?;
        let (root_page_metrics, root_page_coverage) =
            read_child_query_root_metadata(&child_queries)?;
        let next_run_id = manifest
            .runs()
            .iter()
            .map(|entry| entry.descriptor.run_id())
            .max()
            .and_then(|run_id| run_id.checked_add(1))
            .ok_or(ScanStoreError::RunIdOverflow)?;
        Ok(Self {
            temporary_storage: storage.quota(),
            session: manifest.session(),
            next_run_id: Arc::new(AtomicU64::new(next_run_id)),
            active: None,
            session_storage: Some(storage),
            published: Some(PublishedGeneration {
                manifest,
                child_queries,
                path_catalog,
                identity_index,
                directory_summary_index,
                root_page_metrics,
                root_page_coverage,
                unrecorded_path_count: 0,
            }),
            summary_only: None,
            retired_generation: None,
            #[cfg(feature = "internal")]
            io_metrics: ScanStoreIoMetrics::default(),
        })
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

    #[must_use]
    pub(crate) const fn session(&self) -> ScanSessionId {
        self.session
    }

    #[cfg(test)]
    fn preserve_for_recovery(&mut self) {
        if let Some(published) = self.published.as_mut() {
            published.preserve_for_recovery();
        }
    }
    /// Records one path omitted from canonical page data while preserving every
    /// successfully observed path.
    pub(crate) fn record_unrecorded_path(&mut self) {
        if let Some(active) = self.active.as_mut() {
            active.unrecorded_path_count = active.unrecorded_path_count.saturating_add(1);
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
        self.persist_manifest(&active.manifest)?;
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
        if let Err(error) = self.persist_manifest(&active.manifest) {
            self.active = Some(active);
            return Err(error);
        }
        self.retired_generation = Some(active.manifest.generation());
        Ok(())
    }

    /// Starts a newer generation by rebuilding every fact outside
    /// `replaced_prefix` from the compact published child-query run. Callers
    /// may then publish a deletion overlay or add authoritative replacement facts.
    ///
    /// # Errors
    ///
    /// Returns an error when no published base exists, generation ordering is
    /// invalid, a retained query record is corrupt, or the copy cannot fit the
    /// shared temporary-storage budget. Copy failure leaves the new generation
    /// incomplete and preserves the prior published snapshot.
    pub(crate) fn begin_overlay_generation(
        &mut self,
        generation: ScanGeneration,
        replaced_prefix: &crate::scan_coordinator::RelativePath,
    ) -> Result<(), ScanStoreError> {
        self.start_overlay_generation(generation)?;
        if replaced_prefix.is_root() {
            return Ok(());
        }
        let mut published = self
            .published
            .take()
            .ok_or(ScanStoreError::NoPublishedGeneration)?;
        let copied = self.copy_overlay_facts(&mut published, replaced_prefix);
        self.published = Some(published);
        if let Err(error) = copied {
            self.mark_active_incomplete();
            return Err(error);
        }
        Ok(())
    }

    fn copy_overlay_facts(
        &mut self,
        published: &mut PublishedGeneration,
        replaced_prefix: &crate::scan_coordinator::RelativePath,
    ) -> Result<(), ScanStoreError> {
        let mut paths = Vec::with_capacity(MAX_OBSERVATIONS_PER_BATCH);
        let mut identities = Vec::with_capacity(MAX_OBSERVATIONS_PER_BATCH);
        published.with_child_query_entries(|path, identity| -> Result<(), ScanStoreError> {
            if path.path.starts_with(replaced_prefix) {
                return Ok(());
            }
            if let Some(identity) =
                identity.filter(|identity| identity_requires_reduction(&path, identity))
            {
                identities.push(identity);
            }
            paths.push(path);
            if paths.len() == MAX_OBSERVATIONS_PER_BATCH {
                self.append_overlay_batch(&mut paths, &mut identities)?;
            }
            Ok(())
        })?;
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
        generation: ScanGeneration,
    ) -> Result<(), ScanStoreError> {
        let unrecorded_path_count = self
            .published
            .as_ref()
            .ok_or(ScanStoreError::NoPublishedGeneration)?
            .unrecorded_path_count();
        self.begin_generation(generation)?;
        if let Some(active) = self.active.as_mut() {
            active.unrecorded_path_count = unrecorded_path_count;
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
        if let Err(error) = self.persist_added_run(&mut active, &run) {
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
    /// Physical bytes belong only to the identity observations. Path metrics
    /// retain hierarchy, apparent size, and coverage; publishing derives the
    /// once-per-identity physical totals from the companion run.
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
        let result = (|| {
            let factory = self.input_run_factory()?;
            for run in factory.seal_observation_batch(paths, identities)? {
                self.accept_input_run(run)?;
            }
            Ok(())
        })();
        if result.is_err() {
            self.mark_active_incomplete();
        }
        result
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
        let mut active = self
            .active
            .take()
            .ok_or(ScanStoreError::NoActiveGeneration)?;
        if active.manifest.state() != ScanGenerationState::Scanning {
            self.active = Some(active);
            return Err(ScanStoreError::ClosedGeneration);
        }
        active.manifest.transition(ScanGenerationState::Reducing)?;
        if let Err(error) = self.persist_manifest(&active.manifest) {
            self.active = Some(active);
            return Err(error);
        }
        let generation = active.manifest.generation();
        let (
            mut path_observations,
            mut identity_observations,
            mut allocation_contributions,
            mut directory_summaries,
            path_catalog,
        ) = match self.reduce_active_generation(&mut active) {
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
            )?;
            Ok((child_writer.seal()?, page_metadata))
        })();
        let (child_queries, page_metadata) = match child_result {
            Ok(result) => result,
            Err(error) if is_scan_store_capacity_error(&error) => {
                return self.finish_summary_only_generation(
                    active,
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
        #[cfg(feature = "internal")]
        self.record_publication_io(
            &path_observations,
            &identity_observations,
            &allocation_contributions,
            &directory_summaries,
            &child_queries,
        );
        let unrecorded_path_count = active.unrecorded_path_count;
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
        if let Err(error) = self.persist_manifest(&manifest) {
            let _ = active.manifest.transition(ScanGenerationState::Incomplete);
            let _ = self.persist_manifest(&active.manifest);
            self.active = Some(active);
            return Err(error);
        }
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
        });
        Ok(generation)
    }

    fn finish_incomplete_generation(
        &mut self,
        mut active: ActiveGeneration,
        error: ScanStoreError,
    ) -> Result<ScanGeneration, ScanStoreError> {
        let _ = active.manifest.transition(ScanGenerationState::Incomplete);
        let _ = self.persist_manifest(&active.manifest);
        self.active = Some(active);
        Err(error)
    }

    fn finish_summary_only_generation(
        &mut self,
        mut active: ActiveGeneration,
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
        if let Err(error) = self.persist_manifest(&manifest) {
            let _ = active.manifest.transition(ScanGenerationState::Incomplete);
            let _ = self.persist_manifest(&active.manifest);
            self.active = Some(active);
            return Err(error);
        }
        drop(sources);
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
    ) -> Result<(SealedRun, SealedRun, SealedRun, SealedRun, SealedRun), ScanStoreError> {
        let generation = active.manifest.generation();
        let path_runs = std::mem::take(&mut active.path_runs);
        let path_observations =
            self.merge_tiered_family(active, RunKind::PathObservation, path_runs)?;
        let identity_runs = std::mem::take(&mut active.identity_runs);
        let identity_observations =
            self.merge_tiered_family(active, RunKind::IdentityObservation, identity_runs)?;

        #[cfg(feature = "internal")]
        {
            self.io_metrics.reduction_read_bytes = self
                .io_metrics
                .reduction_read_bytes
                .saturating_add(path_observations.bytes());
        }
        let mut path_reader = path_observations.into_reader()?;
        let mut directory_writer = self.new_writer(generation, RunKind::DirectorySummary)?;
        reduce_path_observation_run(&mut path_reader, &mut directory_writer)?;
        let path_observations = path_reader.into_sealed();
        let directory_summaries = directory_writer.seal()?;
        #[cfg(feature = "internal")]
        {
            self.io_metrics.reduction_written_bytes = self
                .io_metrics
                .reduction_written_bytes
                .saturating_add(directory_summaries.bytes());
        }
        self.persist_added_run(active, &directory_summaries)?;

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
        #[cfg(feature = "internal")]
        {
            self.io_metrics.reduction_written_bytes = self
                .io_metrics
                .reduction_written_bytes
                .saturating_add(path_catalog.bytes());
        }
        self.persist_added_run(active, &path_catalog)?;

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
        #[cfg(feature = "internal")]
        {
            self.io_metrics.reduction_written_bytes = self
                .io_metrics
                .reduction_written_bytes
                .saturating_add(allocation_contributions.bytes());
        }
        self.persist_added_run(active, &allocation_contributions)?;

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
                self.persist_added_run(active, &output)?;
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
                self.persist_replaced_runs(active, &descriptors, &output)?;
                drop(readers);
                Ok(output)
            }
        }
    }

    fn persist_manifest(&self, manifest: &ScanManifest) -> Result<(), ScanStoreError> {
        if let Some(storage) = self.session_storage.as_ref() {
            storage.persist_manifest(&manifest.encode()?)?;
        }
        Ok(())
    }

    fn persist_added_run(
        &self,
        active: &mut ActiveGeneration,
        run: &SealedRun,
    ) -> Result<(), ScanStoreError> {
        let mut manifest = active.manifest.clone();
        manifest.add_run(RunManifestEntry {
            descriptor: run.descriptor(),
            bytes: run.bytes(),
        })?;
        self.persist_manifest(&manifest)?;
        active.manifest = manifest;
        Ok(())
    }

    fn persist_replaced_runs(
        &self,
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
        self.persist_manifest(&manifest)?;
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
fn is_scan_store_capacity_error(error: &ScanStoreError) -> bool {
    let mut source: &(dyn StdError + 'static) = error;
    loop {
        if let Some(io_error) = source.downcast_ref::<io::Error>() {
            return io_error.kind() == io::ErrorKind::StorageFull;
        }
        let Some(next) = source.source() else {
            return false;
        };
        source = next;
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
        let manifest_path = storage.manifest_path();
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
            .reduce_active_generation(&mut active)
            .expect("directory summaries should reduce");
        store
            .finish_summary_only_generation(
                active,
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
        assert_eq!(
            ScanManifest::decode(
                &fs::read(manifest_path).expect("summary-only manifest should exist"),
            )
            .expect("summary-only manifest should decode")
            .state(),
            ScanGenerationState::SummaryOnly
        );
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
            .pop()
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
            .pop()
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
    fn private_session_persists_only_complete_manifest_transitions() {
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
        let initial = fs::read(storage.manifest_path()).expect("initial manifest should exist");
        assert_eq!(
            ScanManifest::decode(&initial)
                .expect("initial manifest should decode")
                .state(),
            ScanGenerationState::Scanning
        );
        store
            .append_observation_batch(
                vec![path_observation("entry", PathEntryKind::File, 1)],
                Vec::new(),
            )
            .expect("canonical observation should append");
        let active = ScanManifest::decode(
            &fs::read(storage.manifest_path()).expect("active manifest should exist"),
        )
        .expect("active manifest should decode");
        assert_eq!(active.state(), ScanGenerationState::Scanning);
        assert_eq!(active.runs().len(), 1);
        assert_eq!(
            active.runs()[0].descriptor.kind(),
            RunKind::PathObservation,
            "an accepted raw run must be durable before compaction"
        );
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
        let published = fs::read(storage.manifest_path()).expect("published manifest should exist");
        assert_eq!(
            ScanManifest::decode(&published)
                .expect("published manifest should decode")
                .state(),
            ScanGenerationState::Published
        );
        assert_eq!(
            ScanManifest::decode(&published)
                .expect("published manifest should decode")
                .runs()
                .len(),
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
    fn private_session_persists_incomplete_capacity_and_cancelled_states_separately() {
        let parent = tempfile::tempdir().expect("session parent should exist");

        let incomplete_storage = ScanStoreStorage::new(
            TemporaryStorage::scan_store_with_limit_bytes(512),
            Some(parent.path()),
        )
        .expect("capacity-limited session should initialize");
        let incomplete_manifest = incomplete_storage.manifest_path();
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
            .expect("incomplete terminal state should persist");
        assert_eq!(
            ScanManifest::decode(
                &fs::read(&incomplete_manifest).expect("incomplete manifest should exist"),
            )
            .expect("incomplete manifest should decode")
            .state(),
            ScanGenerationState::Incomplete
        );

        let cancelled_storage = ScanStoreStorage::new(
            TemporaryStorage::scan_store_with_limit_bytes(INDEXED_PUBLICATION_STORAGE_BYTES),
            Some(parent.path()),
        )
        .expect("cancellable session should initialize");
        let cancelled_manifest = cancelled_storage.manifest_path();
        let mut cancelled =
            ScanStore::new_with_storage(ScanGeneration::initial(), cancelled_storage)
                .expect("cancellable store should initialize");
        cancelled
            .cancel_active()
            .expect("cancelled terminal state should persist");
        assert_eq!(
            ScanManifest::decode(
                &fs::read(&cancelled_manifest).expect("cancelled manifest should exist"),
            )
            .expect("cancelled manifest should decode")
            .state(),
            ScanGenerationState::Cancelled
        );
    }
    #[test]
    fn published_private_session_recovers_its_child_query_after_interruption() {
        let parent = tempfile::tempdir().expect("session parent should exist");
        let quota =
            TemporaryStorage::scan_store_with_limit_bytes(INDEXED_PUBLICATION_STORAGE_BYTES);
        let storage = ScanStoreStorage::new(quota.clone(), Some(parent.path()))
            .expect("private scan session should initialize");
        let mut store = ScanStore::new_with_storage(ScanGeneration::initial(), storage.clone())
            .expect("scan store should initialize");
        store
            .append_observation_batch(
                vec![path_observation("entry", PathEntryKind::File, 4)],
                Vec::new(),
            )
            .expect("canonical observation should append");
        store.publish().expect("generation should publish");
        store.preserve_for_recovery();
        drop(store);
        let root = storage.preserve_for_recovery();
        assert!(
            root.exists(),
            "interrupted private session should remain recoverable"
        );

        let recovered_storage = ScanStoreStorage::reopen(quota, &root)
            .expect("interrupted session layout should reopen");
        let recovered = ScanStore::recover_published(recovered_storage)
            .expect("published manifest and child query should recover");
        let page = recovered
            .published()
            .expect("recovered generation should be published")
            .page(PageRequest::first(RelativePath::root(), 8))
            .expect("recovered root page should load");
        assert_eq!(
            page.entries
                .into_iter()
                .map(|entry| entry.path)
                .collect::<Vec<_>>(),
            vec![path("entry")]
        );
        drop(recovered);
        assert!(
            !root.exists(),
            "recovered session should clean up on normal drop"
        );
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

        let manifest = ScanManifest::decode(
            &fs::read(storage.manifest_path()).expect("active manifest should exist"),
        )
        .expect("active manifest should decode");
        assert_eq!(manifest.runs().len(), 1);
        assert_eq!(
            manifest.runs()[0].descriptor.kind(),
            RunKind::PathObservation
        );
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
        let mut store = store(TemporaryStorage::with_limit_bytes(
            INDEXED_PUBLICATION_STORAGE_BYTES,
        ));
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
        store
            .begin_overlay_generation(next, &path("alpha"))
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
