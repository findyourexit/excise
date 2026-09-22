use std::collections::HashMap;

use thiserror::Error;

use super::directory_summary::{DirectorySummaryCodecError, decode_directory_summary};
use super::identity_observation::{
    AllocationContributionCodecError, AllocationPlacement, IdentityObservationCodecError,
    decode_allocation_contribution, decode_identity_observation,
};
use super::path_observation::{PathObservationCodecError, decode_path_observation};
use super::path_reducer::{Coverage, PathEntryKind, SummaryMetrics};
use super::run_file::RunError;
use super::session::PublishedGeneration;
use crate::model::{ByteBounds, EntrySnapshot};
use crate::scan_coordinator::{RelativePath, ScanGeneration};

/// A page never retains more than this many concrete children while it builds a
/// map view. Callers use `next_after` to request the next deterministic slice.
const MAX_PAGE_ENTRIES: usize = 4_096;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PageRequest {
    pub(crate) folder: RelativePath,
    pub(crate) after: Option<RelativePath>,
    pub(crate) limit: usize,
}

impl PageRequest {
    #[must_use]
    pub(crate) const fn first(folder: RelativePath, limit: usize) -> Self {
        Self {
            folder,
            after: None,
            limit,
        }
    }

    #[must_use]
    pub(crate) const fn after(folder: RelativePath, after: RelativePath, limit: usize) -> Self {
        Self {
            folder,
            after: Some(after),
            limit,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PageEntryKind {
    Directory,
    File,
    Link,
}

impl PageEntryKind {
    #[must_use]
    pub(crate) const fn is_directory(self) -> bool {
        matches!(self, Self::Directory)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ScanPageEntry {
    pub(crate) path: RelativePath,
    pub(crate) kind: PageEntryKind,
    pub(crate) metrics: SummaryMetrics,
    pub(crate) coverage: Coverage,
    pub(crate) snapshot: Option<EntrySnapshot>,
}

/// The virtual noninteractive physical allocation held at this folder because
/// one or more hard-link groups span its direct children.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SharedAllocationSummary {
    pub(crate) metrics: SummaryMetrics,
    pub(crate) coverage: Coverage,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ScanPage {
    pub(crate) generation: ScanGeneration,
    pub(crate) folder: RelativePath,
    pub(crate) folder_metrics: SummaryMetrics,
    pub(crate) folder_coverage: Coverage,
    pub(crate) root_metrics: SummaryMetrics,
    pub(crate) root_coverage: Coverage,
    pub(crate) entries: Vec<ScanPageEntry>,
    pub(crate) next_after: Option<RelativePath>,
    pub(crate) shared_allocation: Option<SharedAllocationSummary>,
}

#[derive(Debug, Error)]
pub(crate) enum ScanPageError {
    #[error(transparent)]
    Run(#[from] RunError),
    #[error(transparent)]
    PathObservation(#[from] PathObservationCodecError),
    #[error(transparent)]
    DirectorySummary(#[from] DirectorySummaryCodecError),
    #[error(transparent)]
    IdentityObservation(#[from] IdentityObservationCodecError),
    #[error(transparent)]
    AllocationContribution(#[from] AllocationContributionCodecError),
    #[error("tree-map page limit must be between one and {MAX_PAGE_ENTRIES}")]
    InvalidLimit,
    #[error("tree-map page cursor is not a direct child of its folder")]
    InvalidCursor,
    #[error("tree-map page folder was not found in this generation")]
    MissingFolder,
    #[error("tree-map page folder is not a directory")]
    NotDirectory,
}

impl PublishedGeneration {
    /// Materializes one bounded direct-child page from an immutable scan result.
    ///
    /// The service streams each retained run; it never loads a generation or a
    /// directory's full child set into memory. A UI may page through `next_after`
    /// instead of replacing concrete entries with an undeletable aggregate.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid request, corrupt run, or a missing/non-
    /// directory folder in this generation.
    pub(crate) fn page(&mut self, request: PageRequest) -> Result<ScanPage, ScanPageError> {
        validate_page_request(&request)?;
        if let Some(page) = self.cached_page(&request) {
            return Ok(page);
        }
        let cache_key = request.clone();
        let mut entries = self.collect_page_entries(&request)?;
        let mut metrics = self.collect_directory_metrics(&request, &mut entries)?;
        entries.prepare_physical_accounting();
        self.mark_page_identities(&mut entries)?;
        let shared_allocation =
            self.apply_page_allocations(&request, &mut entries, &mut metrics)?;
        metrics.clear_uncertain_physical_bounds();
        entries.finish_physical_accounting();
        let (entries, next_after) = entries.into_parts();
        let page = ScanPage {
            generation: self.generation(),
            folder: request.folder,
            folder_metrics: metrics.folder,
            folder_coverage: metrics.folder_coverage,
            root_metrics: metrics.root,
            root_coverage: metrics.root_coverage,
            entries,
            next_after,
            shared_allocation,
        };
        self.cache_page(cache_key, page.clone());
        Ok(page)
    }

    fn collect_page_entries(
        &mut self,
        request: &PageRequest,
    ) -> Result<PageEntries, ScanPageError> {
        let mut entries = PageEntries::with_capacity(request.limit);
        let mut folder_found = request.folder.is_root();
        let mut folder_is_directory = request.folder.is_root();
        self.with_path_observations(|reader| {
            let mut key = Vec::new();
            let mut value = Vec::new();
            while reader.next_record_into(&mut key, &mut value)? {
                let observation = decode_path_observation(&key, &value)?;
                if observation.path == request.folder {
                    folder_found = true;
                    folder_is_directory = observation.kind == PathEntryKind::Directory;
                    continue;
                }
                if !observation.path.is_direct_child_of(&request.folder)
                    || request
                        .after
                        .as_ref()
                        .is_some_and(|after| observation.path <= *after)
                {
                    continue;
                }
                if entries.states.len() == request.limit {
                    entries.next_after =
                        entries.states.last().map(|state| state.entry.path.clone());
                    break;
                }
                entries
                    .states
                    .push(PageEntryState::from_observation(observation));
            }
            Ok::<(), ScanPageError>(())
        })?;
        if !folder_found {
            return Err(ScanPageError::MissingFolder);
        }
        if !folder_is_directory {
            return Err(ScanPageError::NotDirectory);
        }
        entries.index_direct_children();
        Ok(entries)
    }

    fn collect_directory_metrics(
        &mut self,
        request: &PageRequest,
        entries: &mut PageEntries,
    ) -> Result<PageMetrics, ScanPageError> {
        let mut folder_summary = None;
        let mut root_summary = None;
        self.with_directory_summaries(|reader| {
            let mut key = Vec::new();
            let mut value = Vec::new();
            while reader.next_record_into(&mut key, &mut value)? {
                let (_, summary) = decode_directory_summary(&key, &value)?;
                if summary.path == request.folder {
                    folder_summary = Some((summary.metrics, summary.coverage));
                }
                if summary.path.is_root() {
                    root_summary = Some((summary.metrics, summary.coverage));
                }
                let Some(index) = entries.direct_index.get(&summary.path).copied() else {
                    continue;
                };
                let state = &mut entries.states[index];
                if !state.entry.kind.is_directory() {
                    continue;
                }
                state.entry.metrics = summary.metrics;
                state.entry.metrics.allocated_bytes = ByteBounds::exact(0);
                state.entry.metrics.reclaimable_bytes = ByteBounds::exact(0);
                state.entry.coverage = summary.coverage;
                state.directory_summary_seen = true;
            }
            Ok::<(), ScanPageError>(())
        })?;
        PageMetrics::from_summaries(folder_summary, root_summary)
    }

    fn mark_page_identities(&mut self, entries: &mut PageEntries) -> Result<(), ScanPageError> {
        self.with_identity_observations(|reader| {
            let mut key = Vec::new();
            let mut value = Vec::new();
            while reader.next_record_into(&mut key, &mut value)? {
                let observation = decode_identity_observation(&key, &value)?;
                let Some(index) = entries.direct_index.get(&observation.path).copied() else {
                    continue;
                };
                let state = &mut entries.states[index];
                if !state.entry.kind.is_directory() {
                    state.identity_seen = true;
                }
            }
            Ok::<(), ScanPageError>(())
        })
    }

    fn apply_page_allocations(
        &mut self,
        request: &PageRequest,
        entries: &mut PageEntries,
        metrics: &mut PageMetrics,
    ) -> Result<Option<SharedAllocationSummary>, ScanPageError> {
        let mut shared = SharedAllocationAccounting::default();
        self.with_allocation_contributions(|reader| {
            let mut key = Vec::new();
            let mut value = Vec::new();
            while reader.next_record_into(&mut key, &mut value)? {
                let (_, contribution) = decode_allocation_contribution(&key, &value)?;
                if contribution.recipient.starts_with(&request.folder) {
                    add_physical(
                        &mut metrics.folder,
                        contribution.allocated_bytes,
                        contribution.reclaimable_bytes,
                    );
                }
                add_physical(
                    &mut metrics.root,
                    contribution.allocated_bytes,
                    contribution.reclaimable_bytes,
                );
                if contribution.placement == AllocationPlacement::Shared
                    && contribution.recipient == request.folder
                {
                    shared.add(contribution.allocated_bytes, contribution.reclaimable_bytes);
                    continue;
                }
                let Some(child) = direct_child_under(&request.folder, &contribution.recipient)
                else {
                    continue;
                };
                let Some(index) = entries.direct_index.get(&child).copied() else {
                    continue;
                };
                let state = &mut entries.states[index];
                match contribution.placement {
                    AllocationPlacement::Leaf if contribution.recipient == state.entry.path => {
                        state.entry.metrics.allocated_bytes = contribution.allocated_bytes;
                        state.entry.metrics.reclaimable_bytes = contribution.reclaimable_bytes;
                        state.leaf_allocation_seen = true;
                    }
                    AllocationPlacement::Leaf | AllocationPlacement::Shared
                        if state.entry.kind.is_directory() =>
                    {
                        add_physical(
                            &mut state.entry.metrics,
                            contribution.allocated_bytes,
                            contribution.reclaimable_bytes,
                        );
                    }
                    AllocationPlacement::Leaf | AllocationPlacement::Shared => {}
                }
            }
            Ok::<(), ScanPageError>(())
        })?;
        Ok(shared.into_summary())
    }
}

struct PageEntries {
    states: Vec<PageEntryState>,
    direct_index: HashMap<RelativePath, usize>,
    next_after: Option<RelativePath>,
}

impl PageEntries {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            states: Vec::with_capacity(capacity),
            direct_index: HashMap::with_capacity(capacity),
            next_after: None,
        }
    }

    fn index_direct_children(&mut self) {
        for (index, state) in self.states.iter().enumerate() {
            self.direct_index.insert(state.entry.path.clone(), index);
        }
    }

    fn prepare_physical_accounting(&mut self) {
        for state in &mut self.states {
            state.prepare_physical_accounting();
        }
    }

    fn finish_physical_accounting(&mut self) {
        for state in &mut self.states {
            state.finish_physical_accounting();
        }
    }

    fn into_parts(self) -> (Vec<ScanPageEntry>, Option<RelativePath>) {
        (
            self.states.into_iter().map(|state| state.entry).collect(),
            self.next_after,
        )
    }
}

struct PageMetrics {
    folder: SummaryMetrics,
    folder_coverage: Coverage,
    root: SummaryMetrics,
    root_coverage: Coverage,
}

impl PageMetrics {
    fn from_summaries(
        folder_summary: Option<(SummaryMetrics, Coverage)>,
        root_summary: Option<(SummaryMetrics, Coverage)>,
    ) -> Result<Self, ScanPageError> {
        let (mut folder, folder_coverage) = folder_summary.ok_or(ScanPageError::MissingFolder)?;
        let (mut root, root_coverage) = root_summary.ok_or(ScanPageError::MissingFolder)?;
        folder.allocated_bytes = ByteBounds::exact(0);
        folder.reclaimable_bytes = ByteBounds::exact(0);
        root.allocated_bytes = ByteBounds::exact(0);
        root.reclaimable_bytes = ByteBounds::exact(0);
        Ok(Self {
            folder,
            folder_coverage,
            root,
            root_coverage,
        })
    }

    fn clear_uncertain_physical_bounds(&mut self) {
        if self.folder_coverage == Coverage::Uncertain {
            self.folder.allocated_bytes.upper = None;
            self.folder.reclaimable_bytes.upper = None;
        }
        if self.root_coverage == Coverage::Uncertain {
            self.root.allocated_bytes.upper = None;
            self.root.reclaimable_bytes.upper = None;
        }
    }
}

#[derive(Default)]
struct SharedAllocationAccounting {
    metrics: SummaryMetrics,
    unknown: bool,
    seen: bool,
}

impl SharedAllocationAccounting {
    fn add(&mut self, allocated: ByteBounds, reclaimable: ByteBounds) {
        add_physical(&mut self.metrics, allocated, reclaimable);
        self.unknown |= !bounds_are_exact(allocated) || !bounds_are_exact(reclaimable);
        self.seen = true;
    }

    fn into_summary(self) -> Option<SharedAllocationSummary> {
        self.seen.then_some(SharedAllocationSummary {
            metrics: self.metrics,
            coverage: if self.unknown {
                Coverage::Uncertain
            } else {
                Coverage::Complete
            },
        })
    }
}

fn validate_page_request(request: &PageRequest) -> Result<(), ScanPageError> {
    if request.limit == 0 || request.limit > MAX_PAGE_ENTRIES {
        return Err(ScanPageError::InvalidLimit);
    }
    if request
        .after
        .as_ref()
        .is_some_and(|after| !after.is_direct_child_of(&request.folder))
    {
        return Err(ScanPageError::InvalidCursor);
    }
    Ok(())
}

struct PageEntryState {
    entry: ScanPageEntry,
    directory_summary_seen: bool,
    identity_seen: bool,
    leaf_allocation_seen: bool,
}

impl PageEntryState {
    fn from_observation(observation: super::path_reducer::PathObservation) -> Self {
        Self {
            entry: ScanPageEntry {
                path: observation.path,
                kind: match observation.kind {
                    PathEntryKind::Directory => PageEntryKind::Directory,
                    PathEntryKind::File => PageEntryKind::File,
                    PathEntryKind::Link => PageEntryKind::Link,
                },
                metrics: observation.metrics,
                coverage: observation.coverage,
                snapshot: observation.snapshot,
            },
            directory_summary_seen: false,
            identity_seen: false,
            leaf_allocation_seen: false,
        }
    }

    fn prepare_physical_accounting(&mut self) {
        if self.entry.kind.is_directory() {
            if !self.directory_summary_seen {
                self.entry.coverage = Coverage::Uncertain;
                self.entry.metrics.allocated_bytes = ByteBounds::unknown();
                self.entry.metrics.reclaimable_bytes = ByteBounds::unknown();
            }
            return;
        }
        self.entry.metrics.allocated_bytes = ByteBounds::unknown();
        self.entry.metrics.reclaimable_bytes = ByteBounds::unknown();
    }

    fn finish_physical_accounting(&mut self) {
        if !self.entry.kind.is_directory() && self.identity_seen && !self.leaf_allocation_seen {
            // A multi-link identity is accounted by a virtual shared child at
            // its LCA, so its individual concrete names correctly hold zero.
            self.entry.metrics.allocated_bytes = ByteBounds::exact(0);
            self.entry.metrics.reclaimable_bytes = ByteBounds::exact(0);
        }
        if !self.entry.kind.is_directory() && !self.identity_seen {
            self.entry.coverage = Coverage::Uncertain;
        }
        if self.entry.coverage == Coverage::Uncertain {
            self.entry.metrics.allocated_bytes.upper = None;
            self.entry.metrics.reclaimable_bytes.upper = None;
        }
    }
}

fn add_physical(metrics: &mut SummaryMetrics, allocated: ByteBounds, reclaimable: ByteBounds) {
    metrics.allocated_bytes.add(allocated);
    metrics.reclaimable_bytes.add(reclaimable);
}

fn bounds_are_exact(bounds: ByteBounds) -> bool {
    bounds.upper == Some(bounds.lower)
}

fn direct_child_under(folder: &RelativePath, path: &RelativePath) -> Option<RelativePath> {
    if !path.starts_with(folder) || path.depth() <= folder.depth() {
        return None;
    }
    RelativePath::from_components(path.components()[..folder.depth().saturating_add(1)].to_vec())
        .ok()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::scan_store::identity_observation::{
        IdentityObservation, append_identity_observation,
    };
    use crate::scan_store::path_observation::append_path_observation;
    use crate::scan_store::run_file::RunKind;
    use crate::scan_store::session::ScanStore;
    use crate::temporary_storage::TemporaryStorage;

    fn path(value: &str) -> RelativePath {
        RelativePath::from_path(Path::new(value)).expect("fixture path should be relative")
    }

    fn path_observation(
        path_text: &str,
        kind: PathEntryKind,
        apparent: u128,
    ) -> super::super::path_reducer::PathObservation {
        super::super::path_reducer::PathObservation::new(
            path(path_text),
            kind,
            SummaryMetrics::leaf(apparent, ByteBounds::exact(0), ByteBounds::exact(0)),
            Coverage::Complete,
        )
    }

    fn identity_observation(
        path_text: &str,
        file_id: file_id::FileId,
        links: u64,
    ) -> IdentityObservation {
        IdentityObservation {
            path: path(path_text),
            file_id,
            declared_links: Some(links),
            allocated_bytes: ByteBounds::exact(8),
        }
    }

    fn add_path_run(
        store: &mut ScanStore,
        observations: &[super::super::path_reducer::PathObservation],
    ) {
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

    fn published_store() -> ScanStore {
        let mut store = ScanStore::new(
            ScanGeneration::initial(),
            TemporaryStorage::with_limit_bytes(128 * 1024),
        )
        .expect("store should initialize");
        add_path_run(
            &mut store,
            &[
                path_observation("alpha", PathEntryKind::Directory, 0),
                path_observation("alpha/first", PathEntryKind::File, 4),
                path_observation("alpha/second", PathEntryKind::File, 4),
                path_observation("beta", PathEntryKind::File, 3),
            ],
        );
        let shared_id = file_id::FileId::new_inode(1, 2);
        add_identity_run(
            &mut store,
            &[
                identity_observation("alpha/first", shared_id, 2),
                identity_observation("alpha/second", shared_id, 2),
                identity_observation("beta", file_id::FileId::new_inode(2, 3), 1),
            ],
        );
        store.publish().expect("generation should publish");
        store
    }

    #[test]
    fn page_serves_concrete_children_and_shared_allocation_without_an_aggregate() {
        let mut store = published_store();
        let page = store
            .published_mut()
            .expect("generation should publish")
            .page(PageRequest::first(path("alpha"), 8))
            .expect("page should load");
        assert_eq!(page.entries.len(), 2);
        assert!(
            page.entries
                .iter()
                .all(|entry| entry.metrics.allocated_bytes == ByteBounds::exact(0))
        );
        assert_eq!(
            page.shared_allocation,
            Some(SharedAllocationSummary {
                metrics: SummaryMetrics {
                    apparent_bytes: 0,
                    allocated_bytes: ByteBounds::exact(8),
                    reclaimable_bytes: ByteBounds::exact(8),
                    descendants: 0,
                },
                coverage: Coverage::Complete,
            })
        );

        let root = store
            .published_mut()
            .expect("generation should publish")
            .page(PageRequest::first(RelativePath::root(), 8))
            .expect("root page should load");
        let alpha = root
            .entries
            .iter()
            .find(|entry| entry.path == path("alpha"))
            .expect("alpha directory should be listed");
        assert_eq!(alpha.metrics.apparent_bytes, 8);
        assert_eq!(alpha.metrics.allocated_bytes, ByteBounds::exact(8));
    }

    #[test]
    fn page_cursor_keeps_direct_entries_concrete_and_bounded() {
        let mut store = published_store();
        let first = store
            .published_mut()
            .expect("generation should publish")
            .page(PageRequest::first(RelativePath::root(), 1))
            .expect("first page should load");
        assert_eq!(first.entries.len(), 1);
        assert_eq!(first.entries[0].path, path("alpha"));
        let after = first
            .next_after
            .expect("another concrete child should remain");
        let second = store
            .published_mut()
            .expect("generation should publish")
            .page(PageRequest::after(RelativePath::root(), after, 1))
            .expect("second page should load");
        assert_eq!(second.entries.len(), 1);
        assert_eq!(second.entries[0].path, path("beta"));
        assert!(second.next_after.is_none());
    }

    #[test]
    fn page_rejects_non_directory_and_invalid_cursors() {
        let mut store = published_store();
        let published = store.published_mut().expect("generation should publish");
        assert!(matches!(
            published.page(PageRequest::first(path("beta"), 1)),
            Err(ScanPageError::NotDirectory)
        ));
        assert!(matches!(
            published.page(PageRequest::after(path("alpha"), path("beta"), 1)),
            Err(ScanPageError::InvalidCursor)
        ));
    }
}
