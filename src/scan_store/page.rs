use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::io;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use redb::{
    Builder as RedbBuilder, Database, Durability, ReadableDatabase, ReadableTable, TableDefinition,
};
use thiserror::Error;

use super::directory_summary::{DirectorySummaryCodecError, decode_directory_summary};
use super::identity_observation::{
    AllocationContribution, AllocationContributionCodecError, AllocationPlacement,
    IdentityObservation, IdentityObservationCodecError, decode_allocation_contribution,
    decode_identity_observation,
};
use super::path_key::{
    PathKeyError, append_path_component_key, decode_path_key, encode_path_key_into,
};
use super::path_observation::{
    PathObservationCodecError, PathObservationRunError, decode_path_observation,
    encode_path_observation_into, visit_path_observations,
};
use super::path_reducer::{
    Coverage, DirectorySummary, PathEntryKind, PathObservation, SummaryMetrics, coverage_code,
    coverage_from_code,
};
use super::run_file::{RunError, RunKind, RunReader, RunWriter, SealedRun};
use super::session::PublishedGeneration;
use super::storage::ScanStoreStorage;
use super::summary_metrics::{
    SummaryMetricsCodecError, decode_summary_metrics, encode_summary_metrics_into,
    summary_metrics_flags,
};
use crate::file_id_codec::{FileIdCodecError, append_file_id, decode_file_id_prefix};
use crate::filter::FilterPattern;
use crate::model::{ByteBounds, EntrySnapshot};
use crate::scan_coordinator::{RelativePath, ScanGeneration};
use crate::temporary_storage::{BoundedFileBackend, TemporaryStorage};
/// Publication batches enough facts to amortize redb copy-on-write pages while
/// remaining bounded independently of scan size.
const PAGE_INDEX_BATCH: usize = 1_024;
/// A page never retains more than this many concrete children while it builds a
/// map view. Callers use `next_after` to request the next deterministic slice.
const MAX_PAGE_ENTRIES: usize = 4_096;
const PAGE_INDEX_CACHE_BYTES: usize = 4 * 1024 * 1024;
const PAGE_INDEX_RECORDS: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("page-index-records");
const PAGE_QUERY_RECORDS: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("page-query-records");
const PAGE_RECORD_NAMESPACE: [u8; 2] = [0, 0];
const PAGE_HEADER_TAG: u8 = 0;
const PAGE_ENTRY_TAG: u8 = 1;
const PAGE_HEADER_VERSION: u8 = 1;
const PAGE_ENTRY_VERSION: u8 = 4;
const QUERY_METRIC_BYTES: usize = size_of::<u128>();
const HEADER_SHARED_PRESENT: u8 = 1;
const ENTRY_DIRECTORY_SUMMARY_SEEN: u8 = 1;
const ENTRY_IDENTITY_PRESENT: u8 = 1 << 1;
const ENTRY_LEAF_ALLOCATION_SEEN: u8 = 1 << 2;
const ENTRY_KNOWN_FLAGS: u8 =
    ENTRY_DIRECTORY_SUMMARY_SEEN | ENTRY_IDENTITY_PRESENT | ENTRY_LEAF_ALLOCATION_SEEN;
const IDENTITY_DECLARED_LINKS_PRESENT: u8 = 1;
const IDENTITY_ALLOCATION_UPPER_PRESENT: u8 = 1 << 1;
const IDENTITY_KNOWN_FLAGS: u8 =
    IDENTITY_DECLARED_LINKS_PRESENT | IDENTITY_ALLOCATION_UPPER_PRESENT;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PageRequest {
    pub(crate) folder: RelativePath,
    pub(crate) after: Option<PageCursor>,
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
    pub(crate) const fn after(folder: RelativePath, after: PageCursor, limit: usize) -> Self {
        Self {
            folder,
            after: Some(after),
            limit,
        }
    }
}

/// Opaque stable continuation token for metric-ordered direct-child pages.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PageCursor {
    pub(crate) path: RelativePath,
    represented_bytes: u128,
}

impl PageCursor {
    #[must_use]
    fn from_entry(entry: &ScanPageEntry) -> Self {
        Self {
            path: entry.path.clone(),
            represented_bytes: entry.metrics.allocated_bytes.lower,
        }
    }

    #[must_use]
    pub(crate) const fn at(path: RelativePath, represented_bytes: u128) -> Self {
        Self {
            path,
            represented_bytes,
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
    pub(crate) next_after: Option<PageCursor>,
    pub(crate) shared_allocation: Option<SharedAllocationSummary>,
    /// Paths omitted from the canonical hierarchy, reported outside node state.
    pub(crate) unrecorded_path_count: u64,
}

/// Bounded live projection of one folder while its scan generation remains open.
///
/// It retains only the strongest observed direct children. The canonical raw
/// runs remain authoritative and are re-read if the user changes folders.
/// This prevents a live map from growing with the scan while preserving exact
/// final publication semantics.
pub(crate) struct ProvisionalPage {
    generation: ScanGeneration,
    folder: RelativePath,
    limit: usize,
    folder_kind: Option<PathEntryKind>,
    folder_metrics: SummaryMetrics,
    root_metrics: SummaryMetrics,
    entries: BTreeMap<RelativePath, ProvisionalEntry>,
}

struct ProvisionalEntry {
    kind: PathEntryKind,
    metrics: SummaryMetrics,
    coverage: Coverage,
    snapshot: Option<EntrySnapshot>,
}

impl ProvisionalPage {
    pub(crate) fn new(
        generation: ScanGeneration,
        request: &PageRequest,
    ) -> Result<Self, ScanPageError> {
        validate_page_request(request)?;
        if request.after.is_some() {
            return Err(ScanPageError::LivePagePagination);
        }
        Ok(Self {
            generation,
            folder: request.folder.clone(),
            limit: request.limit,
            folder_kind: request.folder.is_root().then_some(PathEntryKind::Directory),
            folder_metrics: SummaryMetrics::default(),
            root_metrics: SummaryMetrics::default(),
            entries: BTreeMap::new(),
        })
    }

    #[must_use]
    pub(crate) fn matches(&self, request: &PageRequest) -> bool {
        request.after.is_none() && self.folder == request.folder && self.limit == request.limit
    }

    /// Incorporates one newly admitted canonical input run without retaining its
    /// full hierarchy in memory.
    pub(crate) fn observe_run(
        &mut self,
        run: &mut SealedRun,
    ) -> Result<(), PathObservationRunError> {
        run.with_reader(|reader| {
            visit_path_observations(reader, |observation| {
                self.observe(observation);
                Ok(())
            })
        })
    }

    pub(crate) fn observe(&mut self, observation: PathObservation) {
        add_child_metrics(&mut self.root_metrics, observation.metrics);
        if observation.path == self.folder {
            self.folder_kind = Some(observation.kind);
            merge_metrics(&mut self.folder_metrics, observation.metrics);
            return;
        }
        if !observation.path.starts_with(&self.folder) {
            return;
        }
        add_child_metrics(&mut self.folder_metrics, observation.metrics);
        let child = direct_child_under(&self.folder, &observation.path)
            .expect("a strict descendant must have one direct child");
        let is_direct = child == observation.path;
        if let Some(entry) = self.entries.get_mut(&child) {
            entry.observe(observation, is_direct);
            return;
        }
        let entry = ProvisionalEntry::from_observation(observation, is_direct);
        self.insert(child, entry);
    }

    pub(crate) fn page(&self, unrecorded_path_count: u64) -> Result<ScanPage, ScanPageError> {
        if !self.folder.is_root() {
            match self.folder_kind {
                Some(PathEntryKind::Directory) => {}
                Some(PathEntryKind::File | PathEntryKind::Link) => {
                    return Err(ScanPageError::NotDirectory);
                }
                None => return Err(ScanPageError::MissingFolder),
            }
        }
        let mut entries = self
            .entries
            .iter()
            .map(|(path, entry)| entry.page_entry(path))
            .collect::<Vec<_>>();
        entries.sort_unstable_by(compare_page_entries);
        let mut folder_metrics = self.folder_metrics;
        let mut root_metrics = self.root_metrics;
        mark_metrics_uncertain(&mut folder_metrics);
        mark_metrics_uncertain(&mut root_metrics);
        Ok(ScanPage {
            generation: self.generation,
            folder: self.folder.clone(),
            folder_metrics,
            folder_coverage: Coverage::Uncertain,
            root_metrics,
            root_coverage: Coverage::Uncertain,
            entries,
            next_after: None,
            shared_allocation: None,
            unrecorded_path_count,
        })
    }

    fn insert(&mut self, path: RelativePath, entry: ProvisionalEntry) {
        if self.entries.len() < self.limit {
            self.entries.insert(path, entry);
            return;
        }
        let Some((worst_path, worst)) =
            self.entries
                .iter()
                .max_by(|(left_path, left), (right_path, right)| {
                    compare_provisional_entries(left_path, left, right_path, right)
                })
        else {
            return;
        };
        if compare_provisional_entries(&path, &entry, worst_path, worst) == Ordering::Less {
            let worst_path = worst_path.clone();
            self.entries.remove(&worst_path);
            self.entries.insert(path, entry);
        }
    }
}

impl ProvisionalEntry {
    fn from_observation(observation: PathObservation, is_direct: bool) -> Self {
        let mut entry = Self {
            kind: PathEntryKind::Directory,
            metrics: SummaryMetrics::default(),
            coverage: Coverage::Complete,
            snapshot: None,
        };
        entry.observe(observation, is_direct);
        entry
    }

    fn observe(&mut self, observation: PathObservation, is_direct: bool) {
        if is_direct {
            self.kind = observation.kind;
            merge_metrics(&mut self.metrics, observation.metrics);
            self.coverage = combine_coverage(self.coverage, observation.coverage);
            self.snapshot = observation.snapshot;
        } else {
            add_child_metrics(&mut self.metrics, observation.metrics);
            self.coverage = combine_coverage(self.coverage, observation.coverage);
        }
    }

    fn page_entry(&self, path: &RelativePath) -> ScanPageEntry {
        let mut metrics = self.metrics;
        let coverage = if self.kind == PathEntryKind::Directory || self.snapshot.is_none() {
            mark_metrics_uncertain(&mut metrics);
            Coverage::Uncertain
        } else {
            self.coverage
        };
        ScanPageEntry {
            path: path.clone(),
            kind: page_entry_kind(self.kind),
            metrics,
            coverage,
            snapshot: self.snapshot.clone(),
        }
    }
}

fn combine_coverage(left: Coverage, right: Coverage) -> Coverage {
    if left == Coverage::Uncertain || right == Coverage::Uncertain {
        Coverage::Uncertain
    } else {
        Coverage::Complete
    }
}

fn merge_metrics(target: &mut SummaryMetrics, source: SummaryMetrics) {
    target.apparent_bytes = target.apparent_bytes.saturating_add(source.apparent_bytes);
    target.allocated_bytes.add(source.allocated_bytes);
    target.reclaimable_bytes.add(source.reclaimable_bytes);
    target.descendants = target.descendants.saturating_add(source.descendants);
}

fn add_child_metrics(target: &mut SummaryMetrics, child: SummaryMetrics) {
    merge_metrics(target, child);
    target.descendants = target.descendants.saturating_add(1);
}

fn mark_metrics_uncertain(metrics: &mut SummaryMetrics) {
    metrics.allocated_bytes.upper = None;
    metrics.reclaimable_bytes.upper = None;
}

fn compare_provisional_entries(
    left_path: &RelativePath,
    left: &ProvisionalEntry,
    right_path: &RelativePath,
    right: &ProvisionalEntry,
) -> Ordering {
    right
        .metrics
        .allocated_bytes
        .lower
        .cmp(&left.metrics.allocated_bytes.lower)
        .then_with(|| left_path.cmp(right_path))
}

#[derive(Debug, Error)]
pub(crate) enum ScanPageError {
    #[error(transparent)]
    Run(#[from] RunError),
    #[error(transparent)]
    PathKey(#[from] PathKeyError),
    #[error(transparent)]
    Record(#[from] PageRecordError),
    #[error("tree-map page limit must be between one and {MAX_PAGE_ENTRIES}")]
    InvalidLimit,
    #[error("tree-map page cursor is not a direct child of its folder")]
    InvalidCursor,
    #[error("tree-map page folder was not found in this generation")]
    MissingFolder,
    #[error("tree-map page folder is not a directory")]
    NotDirectory,
    #[error("live scan pages do not support continuation cursors")]
    LivePagePagination,
}

#[derive(Debug, Error)]
pub(crate) enum PageIndexError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Run(#[from] RunError),
    #[error(transparent)]
    PathKey(#[from] PathKeyError),
    #[error(transparent)]
    PathObservation(#[from] PathObservationCodecError),
    #[error(transparent)]
    DirectorySummary(#[from] DirectorySummaryCodecError),
    #[error(transparent)]
    IdentityObservation(#[from] IdentityObservationCodecError),
    #[error(transparent)]
    AllocationContribution(#[from] AllocationContributionCodecError),
    #[error(transparent)]
    Record(#[from] PageRecordError),
    #[error("canonical page index database failed: {0}")]
    Database(String),
    #[error("canonical page index record key was duplicated")]
    DuplicateRecord,
    #[error("canonical page index missed an observed direct child")]
    MissingEntry,
    #[error("canonical page index missed a directory header")]
    MissingHeader,
    #[error("canonical page index directory summary targeted a non-directory")]
    SummaryForNonDirectory,
    #[error("canonical page index identity targeted a directory")]
    IdentityForDirectory,
    #[error("canonical page index allocation targeted an invalid direct child")]
    InvalidAllocationTarget,
    #[error("canonical page query input must use the child-query run kind")]
    WrongInputKind,
    #[error("canonical page query output must use the child-query run kind")]
    WrongOutputKind,
}

#[derive(Debug, Error)]
pub(crate) enum PageRecordError {
    #[error(transparent)]
    FileId(#[from] FileIdCodecError),
    #[error(transparent)]
    PathKey(#[from] PathKeyError),
    #[error(transparent)]
    PathObservation(#[from] PathObservationCodecError),
    #[error(transparent)]
    SummaryMetrics(#[from] SummaryMetricsCodecError),
    #[error("canonical page index record is malformed")]
    Malformed,
}

/// Root values are retained next to the child-query run, avoiding a second
/// random read for every navigation request.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PageIndexMetadata {
    pub(crate) root_metrics: SummaryMetrics,
    pub(crate) root_coverage: Coverage,
}

#[derive(Clone, Copy)]
struct StoredHeader {
    metrics: SummaryMetrics,
    coverage: Coverage,
    shared: Option<SummaryMetrics>,
}

#[derive(Clone, Eq, PartialEq)]
struct StoredIdentity {
    file_id: file_id::FileId,
    declared_links: Option<u64>,
    allocated_bytes: ByteBounds,
}

impl StoredIdentity {
    fn from_observation(observation: &IdentityObservation) -> Self {
        Self {
            file_id: observation.file_id,
            declared_links: observation.declared_links,
            allocated_bytes: observation.allocated_bytes,
        }
    }

    fn from_snapshot(snapshot: &EntrySnapshot) -> Option<Self> {
        snapshot.identity.as_ref().map(|identity| Self {
            file_id: identity.file_id,
            declared_links: identity.link_count,
            allocated_bytes: snapshot
                .allocated_bytes
                .map_or_else(ByteBounds::unknown, ByteBounds::exact),
        })
    }

    fn into_observation(self, path: RelativePath) -> IdentityObservation {
        IdentityObservation {
            path,
            file_id: self.file_id,
            declared_links: self.declared_links,
            allocated_bytes: self.allocated_bytes,
        }
    }
}

struct StoredEntry {
    /// Keeps the scan-time identity snapshot byte-for-byte consistent with its
    /// original observation metrics. Directory page totals live separately.
    observation: PathObservation,
    metrics: SummaryMetrics,
    coverage: Coverage,
    /// Byte length of this entry's parent canonical path key inside its
    /// `ChildQuery` record key. It permits lossless overlay reconstruction
    /// without retaining a second path run.
    parent_key_bytes: u32,
    identity: Option<StoredIdentity>,
    directory_summary_seen: bool,
    leaf_allocation_seen: bool,
}

impl StoredEntry {
    fn from_observation(observation: PathObservation, parent_key_bytes: u32) -> Self {
        let identity = observation
            .snapshot
            .as_ref()
            .and_then(StoredIdentity::from_snapshot);
        let leaf_allocation_seen = observation.kind != PathEntryKind::Directory
            && identity.as_ref().is_some_and(|identity| {
                identity.declared_links == Some(1)
                    && observation.metrics.allocated_bytes == identity.allocated_bytes
                    && observation.metrics.reclaimable_bytes == identity.allocated_bytes
            });
        Self {
            metrics: observation.metrics,
            coverage: observation.coverage,
            observation,
            parent_key_bytes,
            identity,
            directory_summary_seen: false,
            leaf_allocation_seen,
        }
    }

    fn into_page_entry(mut self) -> ScanPageEntry {
        let kind = page_entry_kind(self.observation.kind);
        if kind.is_directory() {
            if !self.directory_summary_seen {
                self.coverage = Coverage::Uncertain;
                self.metrics.allocated_bytes = ByteBounds::unknown();
                self.metrics.reclaimable_bytes = ByteBounds::unknown();
            }
        } else if self.identity.is_none() {
            self.coverage = Coverage::Uncertain;
            self.metrics.allocated_bytes = ByteBounds::unknown();
            self.metrics.reclaimable_bytes = ByteBounds::unknown();
        } else if !self.leaf_allocation_seen {
            // A multi-link identity is accounted by a virtual shared child at
            // its LCA, so individual concrete names correctly hold zero.
            self.metrics.allocated_bytes = ByteBounds::exact(0);
            self.metrics.reclaimable_bytes = ByteBounds::exact(0);
        }
        clear_uncertain_physical_bounds(&mut self.metrics, self.coverage);
        ScanPageEntry {
            path: self.observation.path,
            kind,
            metrics: self.metrics,
            coverage: self.coverage,
            snapshot: self.observation.snapshot,
        }
    }
}

#[derive(Default)]
struct EntryEncodingScratch {
    observation_key: Vec<u8>,
    observation_value: Vec<u8>,
}

/// Builds a query-ready child run while publication owns all derived facts.
/// The temporary database is only a bounded build accumulator; it consumes
/// every raw source run and leaves the sealed, sparse-indexed `ChildQuery` run
/// as the sole retained generation representation.
///
/// # Errors
///
/// Returns an error for corrupt input facts, a bounded-storage failure, or an
/// inconsistent hierarchy. Callers must not publish a partially materialized
/// generation.
pub(crate) fn materialize_child_queries(
    temporary_storage: &TemporaryStorage,
    session_storage: Option<&ScanStoreStorage>,
    path_observations: &mut SealedRun,
    identity_observations: &mut SealedRun,
    directory_summaries: &mut SealedRun,
    allocation_contributions: &mut SealedRun,
    output: &mut RunWriter,
) -> Result<PageIndexMetadata, PageIndexError> {
    if output.descriptor().kind() != RunKind::ChildQuery {
        return Err(PageIndexError::WrongOutputKind);
    }
    let database = create_page_index_database(temporary_storage, session_storage)?;

    path_observations.with_reader(|reader| index_path_observations(&database, reader))?;
    identity_observations.with_reader(|reader| index_identity_observations(&database, reader))?;
    directory_summaries.with_reader(|reader| index_directory_summaries(&database, reader))?;
    allocation_contributions
        .with_reader(|reader| index_allocation_contributions(&database, reader))?;
    build_metric_query_index(&database)?;

    let root = read_header_from_database(&database, &RelativePath::root())?
        .ok_or(PageIndexError::MissingHeader)?;
    write_page_records(&database, output)?;
    Ok(PageIndexMetadata {
        root_metrics: root.metrics,
        root_coverage: root.coverage,
    })
}

impl PublishedGeneration {
    /// Reads one bounded direct-child page from the immutable, publication-time
    /// child-query index. Navigation performs a sparse seek plus at most one
    /// page of record decoding; it never rereads the canonical fact runs.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid request, corrupt child-query record, or
    /// a missing/non-directory folder in this generation.
    pub(crate) fn page(&self, request: PageRequest) -> Result<ScanPage, ScanPageError> {
        validate_page_request(&request)?;
        let Some(header) = read_header_from_run(self.child_query_run(), &request.folder)? else {
            return Err(classify_missing_folder(
                self.child_query_run(),
                &request.folder,
            )?);
        };
        let (entries, next_after) = read_page_entries(self.child_query_run(), &request)?;
        let mut folder_metrics = header.metrics;
        let mut root_metrics = self.root_page_metrics();
        if self.unrecorded_path_count() > 0 {
            root_metrics.allocated_bytes.upper = None;
            root_metrics.reclaimable_bytes.upper = None;
        }
        clear_uncertain_physical_bounds(&mut folder_metrics, header.coverage);
        clear_uncertain_physical_bounds(&mut root_metrics, self.root_page_coverage());
        let shared_allocation = header.shared.map(|metrics| SharedAllocationSummary {
            coverage: if bounds_are_exact(metrics.allocated_bytes)
                && bounds_are_exact(metrics.reclaimable_bytes)
            {
                Coverage::Complete
            } else {
                Coverage::Uncertain
            },
            metrics,
        });
        Ok(ScanPage {
            generation: self.generation(),
            folder: request.folder,
            folder_metrics,
            folder_coverage: header.coverage,
            root_metrics,
            root_coverage: self.root_page_coverage(),
            entries,
            next_after,
            shared_allocation,
            unrecorded_path_count: self.unrecorded_path_count(),
        })
    }

    /// Reads a bounded direct-child page whose entries have a matching path
    /// somewhere below them in the immutable canonical subtree.
    ///
    /// A matching descendant retains each necessary directory ancestor as a
    /// concrete navigation entry. The scan stores at most one page plus one
    /// continuation candidate while walking the sealed query run.
    pub(crate) fn filtered_page(
        &self,
        request: PageRequest,
        root_path: &Path,
        filter: &FilterPattern,
        filter_root: &RelativePath,
    ) -> Result<ScanPage, ScanPageError> {
        validate_page_request(&request)?;
        let Some(header) = read_header_from_run(self.child_query_run(), &request.folder)? else {
            return Err(classify_missing_folder(
                self.child_query_run(),
                &request.folder,
            )?);
        };
        let filter_base = root_path.join(filter_root.to_path_buf());
        let (entries, next_after) = read_filtered_page_entries(
            self.child_query_run(),
            &request,
            root_path,
            &filter_base,
            filter,
            filter_root,
        )?;
        let mut folder_metrics = header.metrics;
        let mut root_metrics = self.root_page_metrics();
        if self.unrecorded_path_count() > 0 {
            root_metrics.allocated_bytes.upper = None;
            root_metrics.reclaimable_bytes.upper = None;
        }
        clear_uncertain_physical_bounds(&mut folder_metrics, header.coverage);
        clear_uncertain_physical_bounds(&mut root_metrics, self.root_page_coverage());
        let shared_allocation = header.shared.map(|metrics| SharedAllocationSummary {
            coverage: if bounds_are_exact(metrics.allocated_bytes)
                && bounds_are_exact(metrics.reclaimable_bytes)
            {
                Coverage::Complete
            } else {
                Coverage::Uncertain
            },
            metrics,
        });
        Ok(ScanPage {
            generation: self.generation(),
            folder: request.folder,
            folder_metrics,
            folder_coverage: header.coverage,
            root_metrics,
            root_coverage: self.root_page_coverage(),
            entries,
            next_after,
            shared_allocation,
            unrecorded_path_count: self.unrecorded_path_count(),
        })
    }
}

fn create_page_index_database(
    temporary_storage: &TemporaryStorage,
    session_storage: Option<&ScanStoreStorage>,
) -> Result<Database, PageIndexError> {
    let file = match session_storage {
        Some(storage) => storage.create_index_file()?.0,
        None => tempfile::tempfile()?,
    };
    let backend = BoundedFileBackend::new(
        file,
        Arc::new(Mutex::new(temporary_storage.reservation(0)?)),
        Arc::new(AtomicBool::new(false)),
    )?;
    let mut builder = RedbBuilder::new();
    builder.set_cache_size(PAGE_INDEX_CACHE_BYTES);
    let database = builder
        .create_with_backend(backend)
        .map_err(database_error)?;
    let transaction = begin_ephemeral_write(&database)?;
    {
        let _records = transaction
            .open_table(PAGE_INDEX_RECORDS)
            .map_err(database_error)?;
        let _query = transaction
            .open_table(PAGE_QUERY_RECORDS)
            .map_err(database_error)?;
    }
    transaction.commit().map_err(database_error)?;
    Ok(database)
}

fn begin_ephemeral_write(database: &Database) -> Result<redb::WriteTransaction, PageIndexError> {
    let mut transaction = database.begin_write().map_err(database_error)?;
    transaction
        .set_durability(Durability::None)
        .map_err(database_error)?;
    Ok(transaction)
}

fn index_path_observations(
    database: &Database,
    reader: &mut RunReader,
) -> Result<(), PageIndexError> {
    let mut key = Vec::new();
    let mut value = Vec::new();
    let mut batch = Vec::with_capacity(PAGE_INDEX_BATCH);
    while reader.next_record_into(&mut key, &mut value)? {
        batch.push(decode_path_observation(&key, &value)?);
        if batch.len() == PAGE_INDEX_BATCH {
            flush_path_batch(database, &mut batch)?;
        }
    }
    flush_path_batch(database, &mut batch)
}

fn flush_path_batch(
    database: &Database,
    batch: &mut Vec<PathObservation>,
) -> Result<(), PageIndexError> {
    if batch.is_empty() {
        return Ok(());
    }
    let transaction = begin_ephemeral_write(database)?;
    {
        let mut table = transaction
            .open_table(PAGE_INDEX_RECORDS)
            .map_err(database_error)?;
        let mut record_key = Vec::new();
        let mut parent_key = Vec::new();
        let mut child_path_key = Vec::new();
        let mut record_value = Vec::new();
        let mut encoding = EntryEncodingScratch::default();
        for observation in batch.drain(..) {
            let parent = relative_parent(&observation.path).ok_or(PageIndexError::MissingEntry)?;
            encode_path_key_into(&parent, &mut parent_key)?;
            let parent_key_bytes = u32::try_from(parent_key.len())
                .map_err(|_| PageIndexError::Record(PageRecordError::Malformed))?;
            encode_path_key_into(&observation.path, &mut child_path_key)?;
            let child_suffix = child_path_key
                .get(parent_key.len()..)
                .expect("a direct child retains its parent path-key prefix");
            page_entry_key_from_encoded(&parent_key, child_suffix, &mut record_key);
            encode_stored_entry_into(
                &StoredEntry::from_observation(observation, parent_key_bytes),
                &mut record_value,
                &mut encoding,
            )?;
            if table
                .insert(record_key.as_slice(), record_value.as_slice())
                .map_err(database_error)?
                .is_some()
            {
                return Err(PageIndexError::DuplicateRecord);
            }
        }
    }
    transaction.commit().map_err(database_error)?;
    Ok(())
}

fn index_directory_summaries(
    database: &Database,
    reader: &mut RunReader,
) -> Result<(), PageIndexError> {
    let mut key = Vec::new();
    let mut value = Vec::new();
    let mut batch = Vec::with_capacity(PAGE_INDEX_BATCH);
    while reader.next_record_into(&mut key, &mut value)? {
        let (_, summary) = decode_directory_summary(&key, &value)?;
        batch.push(summary);
        if batch.len() == PAGE_INDEX_BATCH {
            flush_directory_summary_batch(database, &mut batch)?;
        }
    }
    flush_directory_summary_batch(database, &mut batch)
}

fn flush_directory_summary_batch(
    database: &Database,
    batch: &mut Vec<DirectorySummary>,
) -> Result<(), PageIndexError> {
    if batch.is_empty() {
        return Ok(());
    }
    let transaction = begin_ephemeral_write(database)?;
    {
        let mut table = transaction
            .open_table(PAGE_INDEX_RECORDS)
            .map_err(database_error)?;
        let mut header_key = Vec::new();
        let mut entry_key = Vec::new();
        let mut child_path_key = Vec::new();
        let mut record_value = Vec::new();
        let mut encoding = EntryEncodingScratch::default();
        for summary in batch.drain(..) {
            let path = summary.path;
            // Single-link leaf allocations already live in the path summary;
            // grouped identities are added below from contribution records.
            let metrics = summary.metrics;
            page_header_key_into(&path, &mut header_key)?;
            encode_stored_header_into(
                &StoredHeader {
                    metrics,
                    coverage: summary.coverage,
                    shared: None,
                },
                &mut record_value,
            );
            if table
                .insert(header_key.as_slice(), record_value.as_slice())
                .map_err(database_error)?
                .is_some()
            {
                return Err(PageIndexError::DuplicateRecord);
            }
            let Some(parent) = relative_parent(&path) else {
                continue;
            };
            page_entry_key_into(&parent, &path, &mut entry_key, &mut child_path_key)?;
            let existing = table
                .get(entry_key.as_slice())
                .map_err(database_error)?
                .map(|value| value.value().to_vec())
                .ok_or(PageIndexError::MissingEntry)?;
            let mut entry = decode_stored_entry(&child_path_key, &existing)?;
            if entry.observation.kind != PathEntryKind::Directory {
                return Err(PageIndexError::SummaryForNonDirectory);
            }
            if entry.directory_summary_seen {
                return Err(PageIndexError::DuplicateRecord);
            }
            entry.metrics = metrics;
            entry.coverage = summary.coverage;
            entry.directory_summary_seen = true;
            encode_stored_entry_into(&entry, &mut record_value, &mut encoding)?;
            table
                .insert(entry_key.as_slice(), record_value.as_slice())
                .map_err(database_error)?;
        }
    }
    transaction.commit().map_err(database_error)?;
    Ok(())
}

fn index_identity_observations(
    database: &Database,
    reader: &mut RunReader,
) -> Result<(), PageIndexError> {
    let mut key = Vec::new();
    let mut value = Vec::new();
    let mut batch = Vec::with_capacity(PAGE_INDEX_BATCH);
    while reader.next_record_into(&mut key, &mut value)? {
        batch.push(decode_identity_observation(&key, &value)?);
        if batch.len() == PAGE_INDEX_BATCH {
            flush_identity_batch(database, &mut batch)?;
        }
    }
    flush_identity_batch(database, &mut batch)
}

fn flush_identity_batch(
    database: &Database,
    batch: &mut Vec<IdentityObservation>,
) -> Result<(), PageIndexError> {
    if batch.is_empty() {
        return Ok(());
    }
    let transaction = begin_ephemeral_write(database)?;
    {
        let mut table = transaction
            .open_table(PAGE_INDEX_RECORDS)
            .map_err(database_error)?;
        let mut entry_key = Vec::new();
        let mut child_path_key = Vec::new();
        let mut record_value = Vec::new();
        let mut encoding = EntryEncodingScratch::default();
        for observation in batch.drain(..) {
            let path = observation.path.clone();
            let parent = relative_parent(&path).ok_or(PageIndexError::MissingEntry)?;
            page_entry_key_into(&parent, &path, &mut entry_key, &mut child_path_key)?;
            let existing = table
                .get(entry_key.as_slice())
                .map_err(database_error)?
                .map(|value| value.value().to_vec())
                .ok_or(PageIndexError::MissingEntry)?;
            let mut entry = decode_stored_entry(&child_path_key, &existing)?;
            if entry.observation.kind == PathEntryKind::Directory {
                return Err(PageIndexError::IdentityForDirectory);
            }
            let identity = StoredIdentity::from_observation(&observation);
            if entry
                .identity
                .as_ref()
                .is_some_and(|existing| existing != &identity)
            {
                return Err(PageIndexError::DuplicateRecord);
            }
            entry.identity = Some(identity);
            encode_stored_entry_into(&entry, &mut record_value, &mut encoding)?;
            table
                .insert(entry_key.as_slice(), record_value.as_slice())
                .map_err(database_error)?;
        }
    }
    transaction.commit().map_err(database_error)?;
    Ok(())
}

fn index_allocation_contributions(
    database: &Database,
    reader: &mut RunReader,
) -> Result<(), PageIndexError> {
    let mut key = Vec::new();
    let mut value = Vec::new();
    let mut batch = Vec::with_capacity(PAGE_INDEX_BATCH);
    while reader.next_record_into(&mut key, &mut value)? {
        let (_, contribution) = decode_allocation_contribution(&key, &value)?;
        batch.push(contribution);
        if batch.len() == PAGE_INDEX_BATCH {
            flush_allocation_batch(database, &mut batch)?;
        }
    }
    flush_allocation_batch(database, &mut batch)
}

fn flush_allocation_batch(
    database: &Database,
    batch: &mut Vec<AllocationContribution>,
) -> Result<(), PageIndexError> {
    if batch.is_empty() {
        return Ok(());
    }
    let transaction = begin_ephemeral_write(database)?;
    {
        let mut table = transaction
            .open_table(PAGE_INDEX_RECORDS)
            .map_err(database_error)?;
        let mut scratch = AllocationScratch::default();
        for contribution in batch.drain(..) {
            apply_allocation(&mut table, &contribution, &mut scratch)?;
        }
    }
    transaction.commit().map_err(database_error)?;
    Ok(())
}

#[derive(Default)]
struct AllocationScratch {
    folder_key: Vec<u8>,
    child_path_key: Vec<u8>,
    component_key: Vec<u8>,
    record_key: Vec<u8>,
    record_value: Vec<u8>,
    entry_encoding: EntryEncodingScratch,
}

fn apply_allocation(
    table: &mut redb::Table<'_, &[u8], &[u8]>,
    contribution: &AllocationContribution,
    scratch: &mut AllocationScratch,
) -> Result<(), PageIndexError> {
    scratch.folder_key.clear();
    let depth = contribution.recipient.depth();
    for (index, component) in contribution.recipient.components().iter().enumerate() {
        page_header_key_from_encoded(&scratch.folder_key, &mut scratch.record_key);
        update_header_physical(
            table,
            &scratch.record_key,
            contribution,
            false,
            &mut scratch.record_value,
        )?;

        scratch.component_key.clear();
        append_path_component_key(component, &mut scratch.component_key)?;
        scratch.child_path_key.clear();
        scratch
            .child_path_key
            .extend_from_slice(&scratch.folder_key);
        scratch
            .child_path_key
            .extend_from_slice(&scratch.component_key);
        page_entry_key_from_encoded(
            &scratch.folder_key,
            &scratch.component_key,
            &mut scratch.record_key,
        );
        let is_leaf =
            contribution.placement == AllocationPlacement::Leaf && index.saturating_add(1) == depth;
        update_entry_physical(
            table,
            &scratch.record_key,
            &scratch.child_path_key,
            contribution,
            is_leaf,
            &mut scratch.record_value,
            &mut scratch.entry_encoding,
        )?;
        std::mem::swap(&mut scratch.folder_key, &mut scratch.child_path_key);
    }
    if contribution.placement == AllocationPlacement::Shared {
        page_header_key_from_encoded(&scratch.folder_key, &mut scratch.record_key);
        update_header_physical(
            table,
            &scratch.record_key,
            contribution,
            true,
            &mut scratch.record_value,
        )?;
    }
    Ok(())
}

fn update_header_physical(
    table: &mut redb::Table<'_, &[u8], &[u8]>,
    header_key: &[u8],
    contribution: &AllocationContribution,
    add_shared: bool,
    record_value: &mut Vec<u8>,
) -> Result<(), PageIndexError> {
    let existing = table
        .get(header_key)
        .map_err(database_error)?
        .map(|value| value.value().to_vec())
        .ok_or(PageIndexError::MissingHeader)?;
    let mut header = decode_stored_header(&existing)?;
    add_physical(
        &mut header.metrics,
        contribution.allocated_bytes,
        contribution.reclaimable_bytes,
    );
    if add_shared {
        let shared = header.shared.get_or_insert_with(SummaryMetrics::default);
        add_physical(
            shared,
            contribution.allocated_bytes,
            contribution.reclaimable_bytes,
        );
    }
    encode_stored_header_into(&header, record_value);
    table
        .insert(header_key, record_value.as_slice())
        .map_err(database_error)?;
    Ok(())
}

fn update_entry_physical(
    table: &mut redb::Table<'_, &[u8], &[u8]>,
    entry_key: &[u8],
    child_path_key: &[u8],
    contribution: &AllocationContribution,
    is_leaf: bool,
    record_value: &mut Vec<u8>,
    encoding: &mut EntryEncodingScratch,
) -> Result<(), PageIndexError> {
    let existing = table
        .get(entry_key)
        .map_err(database_error)?
        .map(|value| value.value().to_vec())
        .ok_or(PageIndexError::MissingEntry)?;
    let mut entry = decode_stored_entry(child_path_key, &existing)?;
    if is_leaf {
        if entry.observation.kind == PathEntryKind::Directory {
            return Err(PageIndexError::InvalidAllocationTarget);
        }
        entry.metrics.allocated_bytes = contribution.allocated_bytes;
        entry.metrics.reclaimable_bytes = contribution.reclaimable_bytes;
        entry.leaf_allocation_seen = true;
    } else {
        if entry.observation.kind != PathEntryKind::Directory {
            return Err(PageIndexError::InvalidAllocationTarget);
        }
        add_physical(
            &mut entry.metrics,
            contribution.allocated_bytes,
            contribution.reclaimable_bytes,
        );
    }
    encode_stored_entry_into(&entry, record_value, encoding)?;
    table
        .insert(entry_key, record_value.as_slice())
        .map_err(database_error)?;
    Ok(())
}

fn read_header_from_database(
    database: &Database,
    folder: &RelativePath,
) -> Result<Option<StoredHeader>, PageIndexError> {
    let mut key = Vec::new();
    page_header_key_into(folder, &mut key)?;
    let transaction = database.begin_read().map_err(database_error)?;
    let table = transaction
        .open_table(PAGE_INDEX_RECORDS)
        .map_err(database_error)?;
    table
        .get(key.as_slice())
        .map_err(database_error)?
        .map(|value| decode_stored_header(value.value()))
        .transpose()
        .map_err(Into::into)
}

fn build_metric_query_index(database: &Database) -> Result<(), PageIndexError> {
    let transaction = begin_ephemeral_write(database)?;
    {
        let records = transaction
            .open_table(PAGE_INDEX_RECORDS)
            .map_err(database_error)?;
        let mut query = transaction
            .open_table(PAGE_QUERY_RECORDS)
            .map_err(database_error)?;
        let mut full_path_key = Vec::new();
        let mut query_key = Vec::new();
        for record in records.iter().map_err(database_error)? {
            let (key, value) = record.map_err(database_error)?;
            let key = key.value();
            let value = value.value();
            if value.first() == Some(&PAGE_HEADER_VERSION) {
                query.insert(key, value).map_err(database_error)?;
                continue;
            }
            let parent_key_bytes = stored_entry_parent_key_bytes(value)?;
            child_path_key_from_index_record(key, parent_key_bytes, &mut full_path_key)?;
            let represented_bytes = decode_stored_entry(&full_path_key, value)?
                .into_page_entry()
                .metrics
                .allocated_bytes
                .lower;
            page_entry_query_key_from_encoded(
                full_path_key
                    .get(
                        ..usize::try_from(parent_key_bytes)
                            .map_err(|_| PageRecordError::Malformed)?,
                    )
                    .ok_or(PageRecordError::Malformed)?,
                represented_bytes,
                full_path_key
                    .get(
                        usize::try_from(parent_key_bytes)
                            .map_err(|_| PageRecordError::Malformed)?..,
                    )
                    .ok_or(PageRecordError::Malformed)?,
                &mut query_key,
            );
            if query
                .insert(query_key.as_slice(), value)
                .map_err(database_error)?
                .is_some()
            {
                return Err(PageIndexError::DuplicateRecord);
            }
        }
    }
    transaction.commit().map_err(database_error)
}

fn write_page_records(database: &Database, output: &mut RunWriter) -> Result<(), PageIndexError> {
    let transaction = database.begin_read().map_err(database_error)?;
    let table = transaction
        .open_table(PAGE_QUERY_RECORDS)
        .map_err(database_error)?;
    for record in table.iter().map_err(database_error)? {
        let (key, value) = record.map_err(database_error)?;
        output.append(key.value(), value.value())?;
    }
    Ok(())
}

fn encode_stored_header_into(header: &StoredHeader, output: &mut Vec<u8>) {
    output.clear();
    output.push(PAGE_HEADER_VERSION);
    output.push(coverage_code(header.coverage));
    output.push(summary_metrics_flags(header.metrics));
    output.push(u8::from(header.shared.is_some()));
    encode_summary_metrics_into(header.metrics, output);
    if let Some(shared) = header.shared {
        output.push(summary_metrics_flags(shared));
        encode_summary_metrics_into(shared, output);
    }
}

fn decode_stored_header(mut input: &[u8]) -> Result<StoredHeader, PageRecordError> {
    if take_record_byte(&mut input)? != PAGE_HEADER_VERSION {
        return Err(PageRecordError::Malformed);
    }
    let coverage =
        coverage_from_code(take_record_byte(&mut input)?).ok_or(PageRecordError::Malformed)?;
    let metrics_flags = take_record_byte(&mut input)?;
    let flags = take_record_byte(&mut input)?;
    let metrics = decode_summary_metrics(metrics_flags, &mut input)?;
    if flags & !HEADER_SHARED_PRESENT != 0 {
        return Err(PageRecordError::Malformed);
    }
    let shared = if flags & HEADER_SHARED_PRESENT == 0 {
        None
    } else {
        let shared = decode_summary_metrics(take_record_byte(&mut input)?, &mut input)?;
        if shared.apparent_bytes != 0 || shared.descendants != 0 {
            return Err(PageRecordError::Malformed);
        }
        Some(shared)
    };
    if !input.is_empty() {
        return Err(PageRecordError::Malformed);
    }
    Ok(StoredHeader {
        metrics,
        coverage,
        shared,
    })
}

fn encode_stored_entry_into(
    entry: &StoredEntry,
    output: &mut Vec<u8>,
    scratch: &mut EntryEncodingScratch,
) -> Result<(), PageRecordError> {
    let snapshot_identity = entry
        .observation
        .snapshot
        .as_ref()
        .and_then(StoredIdentity::from_snapshot);
    let identity_to_encode = match (entry.identity.as_ref(), snapshot_identity.as_ref()) {
        (Some(identity), Some(snapshot_identity)) if identity == snapshot_identity => None,
        (identity, _) => identity,
    };
    output.clear();
    output.push(PAGE_ENTRY_VERSION);
    let mut flags = 0_u8;
    if entry.directory_summary_seen {
        flags |= ENTRY_DIRECTORY_SUMMARY_SEEN;
    }
    if identity_to_encode.is_some() {
        flags |= ENTRY_IDENTITY_PRESENT;
    }
    if entry.leaf_allocation_seen {
        flags |= ENTRY_LEAF_ALLOCATION_SEEN;
    }
    output.push(flags);
    output.extend_from_slice(&entry.parent_key_bytes.to_le_bytes());
    output.push(coverage_code(entry.coverage));
    output.push(summary_metrics_flags(entry.metrics));
    encode_summary_metrics_into(entry.metrics, output);
    if let Some(identity) = identity_to_encode {
        encode_stored_identity_into(identity, output);
    }
    encode_path_observation_into(
        &entry.observation,
        &mut scratch.observation_key,
        &mut scratch.observation_value,
    )?;
    output.extend_from_slice(&scratch.observation_value);
    Ok(())
}

fn decode_stored_entry(path_key: &[u8], mut input: &[u8]) -> Result<StoredEntry, PageRecordError> {
    if take_record_byte(&mut input)? != PAGE_ENTRY_VERSION {
        return Err(PageRecordError::Malformed);
    }
    let flags = take_record_byte(&mut input)?;
    if flags & !ENTRY_KNOWN_FLAGS != 0 {
        return Err(PageRecordError::Malformed);
    }
    let parent_key_bytes = u32::from_le_bytes(take_record_array(&mut input)?);
    let coverage =
        coverage_from_code(take_record_byte(&mut input)?).ok_or(PageRecordError::Malformed)?;
    let metrics = decode_summary_metrics(take_record_byte(&mut input)?, &mut input)?;
    let encoded_identity = (flags & ENTRY_IDENTITY_PRESENT != 0)
        .then(|| decode_stored_identity(&mut input))
        .transpose()?;
    let observation = decode_path_observation(path_key, input)?;
    let identity = encoded_identity.or_else(|| {
        observation
            .snapshot
            .as_ref()
            .and_then(StoredIdentity::from_snapshot)
    });
    Ok(StoredEntry {
        observation,
        metrics,
        coverage,
        parent_key_bytes,
        identity,
        directory_summary_seen: flags & ENTRY_DIRECTORY_SUMMARY_SEEN != 0,
        leaf_allocation_seen: flags & ENTRY_LEAF_ALLOCATION_SEEN != 0,
    })
}

fn encode_stored_identity_into(identity: &StoredIdentity, output: &mut Vec<u8>) {
    append_file_id(&identity.file_id, output);
    let mut flags = 0_u8;
    if identity.declared_links.is_some() {
        flags |= IDENTITY_DECLARED_LINKS_PRESENT;
    }
    if identity.allocated_bytes.upper.is_some() {
        flags |= IDENTITY_ALLOCATION_UPPER_PRESENT;
    }
    output.push(flags);
    if let Some(declared_links) = identity.declared_links {
        output.extend_from_slice(&declared_links.to_le_bytes());
    }
    output.extend_from_slice(&identity.allocated_bytes.lower.to_le_bytes());
    if let Some(upper) = identity.allocated_bytes.upper {
        output.extend_from_slice(&upper.to_le_bytes());
    }
}

fn decode_stored_identity(input: &mut &[u8]) -> Result<StoredIdentity, PageRecordError> {
    let (file_id, bytes) = decode_file_id_prefix(input)?;
    *input = input.get(bytes..).ok_or(PageRecordError::Malformed)?;
    let flags = take_record_byte(input)?;
    if flags & !IDENTITY_KNOWN_FLAGS != 0 {
        return Err(PageRecordError::Malformed);
    }
    let declared_links = (flags & IDENTITY_DECLARED_LINKS_PRESENT != 0)
        .then(|| take_record_u64(input))
        .transpose()?;
    if declared_links == Some(0) {
        return Err(PageRecordError::Malformed);
    }
    let lower = take_record_u128(input)?;
    let upper = (flags & IDENTITY_ALLOCATION_UPPER_PRESENT != 0)
        .then(|| take_record_u128(input))
        .transpose()?;
    if upper.is_some_and(|upper| upper < lower) {
        return Err(PageRecordError::Malformed);
    }
    Ok(StoredIdentity {
        file_id,
        declared_links,
        allocated_bytes: ByteBounds { lower, upper },
    })
}

fn take_record_byte(input: &mut &[u8]) -> Result<u8, PageRecordError> {
    let (&byte, rest) = input.split_first().ok_or(PageRecordError::Malformed)?;
    *input = rest;
    Ok(byte)
}

fn take_record_u64(input: &mut &[u8]) -> Result<u64, PageRecordError> {
    Ok(u64::from_le_bytes(take_record_array(input)?))
}

fn take_record_u128(input: &mut &[u8]) -> Result<u128, PageRecordError> {
    Ok(u128::from_le_bytes(take_record_array(input)?))
}

fn take_record_array<const N: usize>(input: &mut &[u8]) -> Result<[u8; N], PageRecordError> {
    let bytes = input.get(..N).ok_or(PageRecordError::Malformed)?;
    let mut output = [0_u8; N];
    output.copy_from_slice(bytes);
    *input = &input[N..];
    Ok(output)
}

/// Visits every canonical fact retained in a published child-query run.
///
/// The query records preserve each original path observation and its optional
/// identity fact. Overlay publication re-sorts these bounded batches instead
/// of retaining duplicate raw runs beside the page index.
///
/// # Errors
///
/// Returns an error for a wrong run family, corrupt record, or visitor error.
pub(crate) fn visit_child_query_entries<E>(
    run: &mut SealedRun,
    mut visit: impl FnMut(PathObservation, Option<IdentityObservation>) -> Result<(), E>,
) -> Result<(), E>
where
    E: From<RunError> + From<PageIndexError>,
{
    run.with_reader(|reader| {
        if reader.descriptor().kind() != RunKind::ChildQuery {
            return Err(E::from(PageIndexError::WrongInputKind));
        }
        let mut key = Vec::new();
        let mut value = Vec::new();
        let mut path_key = Vec::new();
        while reader.next_record_into(&mut key, &mut value)? {
            if value.first() == Some(&PAGE_HEADER_VERSION) {
                continue;
            }
            let parent_key_bytes = stored_entry_parent_key_bytes(&value)
                .map_err(|error| E::from(PageIndexError::from(error)))?;
            child_path_key_from_record(&key, parent_key_bytes, &mut path_key)
                .map_err(|error| E::from(PageIndexError::from(error)))?;
            let entry = decode_stored_entry(&path_key, &value)
                .map_err(|error| E::from(PageIndexError::from(error)))?;
            let StoredEntry {
                observation,
                identity,
                ..
            } = entry;
            let identity =
                identity.map(|identity| identity.into_observation(observation.path.clone()));
            visit(observation, identity)?;
        }
        Ok(())
    })
}

/// Visits every concrete entry with its publication-time page metrics.
///
/// # Errors
///
/// Returns an error for a wrong run family, corrupt record, or visitor error.
pub(crate) fn visit_child_query_page_entries<E>(
    run: &mut SealedRun,
    mut visit: impl FnMut(ScanPageEntry) -> Result<(), E>,
) -> Result<(), E>
where
    E: From<RunError> + From<PageIndexError>,
{
    run.with_reader(|reader| {
        if reader.descriptor().kind() != RunKind::ChildQuery {
            return Err(E::from(PageIndexError::WrongInputKind));
        }
        let mut key = Vec::new();
        let mut value = Vec::new();
        let mut path_key = Vec::new();
        while reader.next_record_into(&mut key, &mut value)? {
            if value.first() == Some(&PAGE_HEADER_VERSION) {
                continue;
            }
            let parent_key_bytes = stored_entry_parent_key_bytes(&value)
                .map_err(|error| E::from(PageIndexError::from(error)))?;
            child_path_key_from_record(&key, parent_key_bytes, &mut path_key)
                .map_err(|error| E::from(PageIndexError::from(error)))?;
            let entry = decode_stored_entry(&path_key, &value)
                .map_err(|error| E::from(PageIndexError::from(error)))?;
            visit(entry.into_page_entry())?;
        }
        Ok(())
    })
}

fn stored_entry_parent_key_bytes(input: &[u8]) -> Result<u32, PageRecordError> {
    let mut input = input;
    if take_record_byte(&mut input)? != PAGE_ENTRY_VERSION {
        return Err(PageRecordError::Malformed);
    }
    let flags = take_record_byte(&mut input)?;
    if flags & !ENTRY_KNOWN_FLAGS != 0 {
        return Err(PageRecordError::Malformed);
    }
    Ok(u32::from_le_bytes(take_record_array(&mut input)?))
}

fn child_path_key_from_index_record(
    record_key: &[u8],
    parent_key_bytes: u32,
    output: &mut Vec<u8>,
) -> Result<(), PageRecordError> {
    child_path_key_from_record_with_metric(record_key, parent_key_bytes, false, output)
}

fn child_path_key_from_record(
    record_key: &[u8],
    parent_key_bytes: u32,
    output: &mut Vec<u8>,
) -> Result<(), PageRecordError> {
    child_path_key_from_record_with_metric(record_key, parent_key_bytes, true, output)
}

fn child_path_key_from_record_with_metric(
    record_key: &[u8],
    parent_key_bytes: u32,
    metric_ordered: bool,
    output: &mut Vec<u8>,
) -> Result<(), PageRecordError> {
    let parent_key_bytes =
        usize::try_from(parent_key_bytes).map_err(|_| PageRecordError::Malformed)?;
    let namespace_end = parent_key_bytes
        .checked_add(PAGE_RECORD_NAMESPACE.len())
        .ok_or(PageRecordError::Malformed)?;
    let tag_end = namespace_end
        .checked_add(1)
        .ok_or(PageRecordError::Malformed)?;
    if record_key.get(parent_key_bytes..namespace_end) != Some(PAGE_RECORD_NAMESPACE.as_slice())
        || record_key.get(namespace_end) != Some(&PAGE_ENTRY_TAG)
    {
        return Err(PageRecordError::Malformed);
    }
    let suffix_start = tag_end
        .checked_add(if metric_ordered {
            QUERY_METRIC_BYTES
        } else {
            0
        })
        .ok_or(PageRecordError::Malformed)?;
    let suffix = record_key
        .get(suffix_start..)
        .filter(|suffix| !suffix.is_empty())
        .ok_or(PageRecordError::Malformed)?;
    output.clear();
    output.extend_from_slice(
        record_key
            .get(..parent_key_bytes)
            .ok_or(PageRecordError::Malformed)?,
    );
    output.extend_from_slice(suffix);
    Ok(())
}

fn read_header_from_run(
    run: &SealedRun,
    folder: &RelativePath,
) -> Result<Option<StoredHeader>, ScanPageError> {
    let mut expected = Vec::new();
    page_header_key_into(folder, &mut expected)?;
    let mut reader = run.range_reader(&expected)?;
    let mut key = Vec::new();
    let mut value = Vec::new();
    while reader.next_record_into(&mut key, &mut value)? {
        if key < expected {
            continue;
        }
        if key != expected {
            return Ok(None);
        }
        return Ok(Some(decode_stored_header(&value)?));
    }
    Ok(None)
}

fn read_exact_entry_from_run(
    run: &SealedRun,
    parent: &RelativePath,
    child: &RelativePath,
) -> Result<Option<StoredEntry>, ScanPageError> {
    let mut parent_key = Vec::new();
    encode_path_key_into(parent, &mut parent_key)?;
    let mut prefix = Vec::new();
    page_entry_prefix_from_encoded(&parent_key, &mut prefix);
    let mut reader = run.range_reader(&prefix)?;
    let mut key = Vec::new();
    let mut value = Vec::new();
    let mut path_key = Vec::new();
    while reader.next_record_into(&mut key, &mut value)? {
        if key < prefix {
            continue;
        }
        if !key.starts_with(&prefix) {
            return Ok(None);
        }
        let parent_key_bytes = stored_entry_parent_key_bytes(&value)?;
        child_path_key_from_record(&key, parent_key_bytes, &mut path_key)?;
        if decode_path_key(&path_key)? != *child {
            continue;
        }
        return Ok(Some(decode_stored_entry(&path_key, &value)?));
    }
    Ok(None)
}

/// Reads one concrete entry from the immutable child-query run.
///
/// The root has no concrete entry and returns `None`.
pub(crate) fn read_page_entry(
    run: &SealedRun,
    path: &RelativePath,
) -> Result<Option<ScanPageEntry>, ScanPageError> {
    let Some(parent) = relative_parent(path) else {
        return Ok(None);
    };
    read_exact_entry_from_run(run, &parent, path)
        .map(|entry| entry.map(StoredEntry::into_page_entry))
}

/// Reads the root summary retained in a sealed child-query run.
pub(crate) fn read_child_query_root_metadata(
    run: &SealedRun,
) -> Result<(SummaryMetrics, Coverage), ScanPageError> {
    let header =
        read_header_from_run(run, &RelativePath::root())?.ok_or(ScanPageError::MissingFolder)?;
    Ok((header.metrics, header.coverage))
}

fn classify_missing_folder(
    run: &SealedRun,
    folder: &RelativePath,
) -> Result<ScanPageError, ScanPageError> {
    let Some(parent) = relative_parent(folder) else {
        return Ok(ScanPageError::MissingFolder);
    };
    let Some(entry) = read_exact_entry_from_run(run, &parent, folder)? else {
        return Ok(ScanPageError::MissingFolder);
    };
    if entry.observation.kind == PathEntryKind::Directory {
        Ok(ScanPageError::MissingFolder)
    } else {
        Ok(ScanPageError::NotDirectory)
    }
}

fn read_page_entries(
    run: &SealedRun,
    request: &PageRequest,
) -> Result<(Vec<ScanPageEntry>, Option<PageCursor>), ScanPageError> {
    let mut folder_key = Vec::new();
    encode_path_key_into(&request.folder, &mut folder_key)?;
    let mut entry_prefix = Vec::new();
    page_entry_prefix_from_encoded(&folder_key, &mut entry_prefix);
    let mut lower_bound = entry_prefix.clone();
    if let Some(after) = request.after.as_ref() {
        let mut after_path_key = Vec::new();
        page_entry_query_key_into(
            &request.folder,
            after,
            &mut lower_bound,
            &mut after_path_key,
        )?;
    }
    let mut reader = run.range_reader(&lower_bound)?;
    let mut key = Vec::new();
    let mut value = Vec::new();
    let mut full_path_key = Vec::new();
    let mut entries = Vec::with_capacity(request.limit);
    while reader.next_record_into(&mut key, &mut value)? {
        if key < lower_bound || (request.after.is_some() && key == lower_bound) {
            continue;
        }
        if !key.starts_with(&entry_prefix) {
            break;
        }
        let parent_key_bytes = stored_entry_parent_key_bytes(&value)?;
        child_path_key_from_record(&key, parent_key_bytes, &mut full_path_key)?;
        let entry = decode_stored_entry(&full_path_key, &value)?.into_page_entry();
        if !entry.path.is_direct_child_of(&request.folder) {
            return Err(PageRecordError::Malformed.into());
        }
        if entries.len() == request.limit {
            let next_after = entries.last().map(PageCursor::from_entry);
            return Ok((entries, next_after));
        }

        entries.push(entry);
    }
    Ok((entries, None))
}
fn read_filtered_page_entries(
    run: &SealedRun,
    request: &PageRequest,
    root_path: &Path,
    filter_base: &Path,
    filter: &FilterPattern,
    filter_root: &RelativePath,
) -> Result<(Vec<ScanPageEntry>, Option<PageCursor>), ScanPageError> {
    let mut folder_key = Vec::new();
    encode_path_key_into(&request.folder, &mut folder_key)?;
    let mut lower_bound = Vec::new();
    page_entry_prefix_from_encoded(&folder_key, &mut lower_bound);
    let mut reader = run.range_reader(&lower_bound)?;
    let mut key = Vec::new();
    let mut value = Vec::new();
    let mut path_key = Vec::new();
    let mut candidates: Vec<ScanPageEntry> = Vec::with_capacity(request.limit.saturating_add(1));
    while reader.next_record_into(&mut key, &mut value)? {
        if key < lower_bound {
            continue;
        }
        if !key.starts_with(&folder_key) {
            break;
        }
        if value.first() == Some(&PAGE_HEADER_VERSION) {
            continue;
        }
        let parent_key_bytes = stored_entry_parent_key_bytes(&value)?;
        child_path_key_from_record(&key, parent_key_bytes, &mut path_key)?;
        let entry = decode_stored_entry(&path_key, &value)?.into_page_entry();
        if !entry.path.starts_with(&request.folder) || !entry.path.starts_with(filter_root) {
            continue;
        }
        let display_path = root_path.join(entry.path.to_path_buf());
        if !filter.matches_path(&display_path, filter_base) {
            continue;
        }
        let child = direct_child_under(&request.folder, &entry.path)?;
        let candidate = if child == entry.path {
            entry
        } else {
            read_exact_entry_from_run(run, &request.folder, &child)?
                .map(StoredEntry::into_page_entry)
                .ok_or(PageRecordError::Malformed)?
        };
        if request
            .after
            .as_ref()
            .is_some_and(|after| !is_after_cursor(&candidate, after))
        {
            continue;
        }
        if candidates
            .iter()
            .any(|existing| existing.path == candidate.path)
        {
            continue;
        }
        candidates.push(candidate);
        candidates.sort_unstable_by(compare_page_entries);
        if candidates.len() > request.limit.saturating_add(1) {
            let _ = candidates.pop();
        }
    }
    let next_after = (candidates.len() > request.limit)
        .then(|| PageCursor::from_entry(&candidates[request.limit.saturating_sub(1)]));
    candidates.truncate(request.limit);
    Ok((candidates, next_after))
}

fn direct_child_under(
    folder: &RelativePath,
    path: &RelativePath,
) -> Result<RelativePath, ScanPageError> {
    if !path.starts_with(folder) || path.depth() <= folder.depth() {
        return Err(PageRecordError::Malformed.into());
    }
    RelativePath::from_components(path.components()[..folder.depth().saturating_add(1)].to_vec())
        .map_err(|_| PageRecordError::Malformed.into())
}

fn compare_page_entries(left: &ScanPageEntry, right: &ScanPageEntry) -> Ordering {
    right
        .metrics
        .allocated_bytes
        .lower
        .cmp(&left.metrics.allocated_bytes.lower)
        .then_with(|| left.path.cmp(&right.path))
}

fn is_after_cursor(entry: &ScanPageEntry, after: &PageCursor) -> bool {
    entry.metrics.allocated_bytes.lower < after.represented_bytes
        || (entry.metrics.allocated_bytes.lower == after.represented_bytes
            && entry.path > after.path)
}

fn validate_page_request(request: &PageRequest) -> Result<(), ScanPageError> {
    if request.limit == 0 || request.limit > MAX_PAGE_ENTRIES {
        return Err(ScanPageError::InvalidLimit);
    }
    if request
        .after
        .as_ref()
        .is_some_and(|after| !after.path.is_direct_child_of(&request.folder))
    {
        return Err(ScanPageError::InvalidCursor);
    }
    Ok(())
}

fn page_header_key_into(folder: &RelativePath, output: &mut Vec<u8>) -> Result<(), PathKeyError> {
    encode_path_key_into(folder, output)?;
    output.extend_from_slice(&PAGE_RECORD_NAMESPACE);
    output.push(PAGE_HEADER_TAG);
    Ok(())
}

fn page_header_key_from_encoded(folder_key: &[u8], output: &mut Vec<u8>) {
    output.clear();
    output.extend_from_slice(folder_key);
    output.extend_from_slice(&PAGE_RECORD_NAMESPACE);
    output.push(PAGE_HEADER_TAG);
}

fn page_entry_prefix_from_encoded(folder_key: &[u8], output: &mut Vec<u8>) {
    output.clear();
    output.extend_from_slice(folder_key);
    output.extend_from_slice(&PAGE_RECORD_NAMESPACE);
    output.push(PAGE_ENTRY_TAG);
}

fn page_entry_key_from_encoded(folder_key: &[u8], component_key: &[u8], output: &mut Vec<u8>) {
    page_entry_prefix_from_encoded(folder_key, output);
    output.extend_from_slice(component_key);
}

fn page_entry_query_key_from_encoded(
    folder_key: &[u8],
    represented_bytes: u128,
    child_suffix: &[u8],
    output: &mut Vec<u8>,
) {
    page_entry_prefix_from_encoded(folder_key, output);
    output.extend_from_slice(&(!represented_bytes).to_be_bytes());
    output.extend_from_slice(child_suffix);
}

fn page_entry_query_key_into(
    folder: &RelativePath,
    cursor: &PageCursor,
    output: &mut Vec<u8>,
    child_path_key: &mut Vec<u8>,
) -> Result<(), PathKeyError> {
    encode_path_key_into(folder, output)?;
    let folder_key_length = output.len();
    encode_path_key_into(&cursor.path, child_path_key)?;
    let child_suffix = child_path_key
        .get(folder_key_length..)
        .expect("a direct child retains its parent path-key prefix");
    let folder_key = output.clone();
    page_entry_query_key_from_encoded(&folder_key, cursor.represented_bytes, child_suffix, output);
    Ok(())
}

fn page_entry_key_into(
    folder: &RelativePath,
    child: &RelativePath,
    output: &mut Vec<u8>,
    child_path_key: &mut Vec<u8>,
) -> Result<(), PathKeyError> {
    encode_path_key_into(folder, output)?;
    let folder_key_length = output.len();
    encode_path_key_into(child, child_path_key)?;
    let child_suffix = child_path_key
        .get(folder_key_length..)
        .expect("a direct child retains its parent path-key prefix");
    output.extend_from_slice(&PAGE_RECORD_NAMESPACE);
    output.push(PAGE_ENTRY_TAG);
    output.extend_from_slice(child_suffix);
    Ok(())
}

fn relative_parent(path: &RelativePath) -> Option<RelativePath> {
    (!path.is_root()).then(|| {
        RelativePath::from_components(path.components()[..path.depth().saturating_sub(1)].to_vec())
            .expect("a prefix of a valid relative path remains valid")
    })
}

fn add_physical(metrics: &mut SummaryMetrics, allocated: ByteBounds, reclaimable: ByteBounds) {
    metrics.allocated_bytes.add(allocated);
    metrics.reclaimable_bytes.add(reclaimable);
}

fn clear_uncertain_physical_bounds(metrics: &mut SummaryMetrics, coverage: Coverage) {
    if coverage == Coverage::Uncertain {
        metrics.allocated_bytes.upper = None;
        metrics.reclaimable_bytes.upper = None;
    }
}

fn bounds_are_exact(bounds: ByteBounds) -> bool {
    bounds.upper == Some(bounds.lower)
}

fn page_entry_kind(kind: PathEntryKind) -> PageEntryKind {
    match kind {
        PathEntryKind::Directory => PageEntryKind::Directory,
        PathEntryKind::File => PageEntryKind::File,
        PathEntryKind::Link => PageEntryKind::Link,
    }
}

fn database_error(error: impl std::fmt::Display) -> PageIndexError {
    PageIndexError::Database(error.to_string())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::model::NodeKind;
    use crate::native_path::NativeIdentity;
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
            TemporaryStorage::with_limit_bytes(2 * 1024 * 1024),
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
    fn filtered_pages_retain_matching_subtree_ancestors() {
        let mut store = published_store();
        let filter = FilterPattern::new("first").expect("filter should compile");
        let root = store
            .published_mut()
            .expect("generation should publish")
            .filtered_page(
                PageRequest::first(RelativePath::root(), 8),
                Path::new("/scan"),
                &filter,
                &RelativePath::root(),
            )
            .expect("filtered root page should load");
        assert_eq!(
            root.entries
                .iter()
                .map(|entry| entry.path.clone())
                .collect::<Vec<_>>(),
            vec![path("alpha")],
            "a descendant match must retain its navigable direct ancestor"
        );

        let alpha = store
            .published_mut()
            .expect("generation should publish")
            .filtered_page(
                PageRequest::first(path("alpha"), 8),
                Path::new("/scan"),
                &filter,
                &RelativePath::root(),
            )
            .expect("filtered child page should load");
        assert_eq!(
            alpha
                .entries
                .iter()
                .map(|entry| entry.path.clone())
                .collect::<Vec<_>>(),
            vec![path("alpha/first")]
        );
    }

    #[test]
    fn filtered_page_continuations_preserve_metric_order() {
        let mut store = published_store();
        let filter = FilterPattern::new("*").expect("glob filter should compile");
        let first = store
            .published_mut()
            .expect("generation should publish")
            .filtered_page(
                PageRequest::first(RelativePath::root(), 1),
                Path::new("/scan"),
                &filter,
                &RelativePath::root(),
            )
            .expect("first filtered page should load");
        assert_eq!(first.entries[0].path, path("alpha"));
        let after = first.next_after.expect("filtered page should continue");
        let second = store
            .published_mut()
            .expect("generation should publish")
            .filtered_page(
                PageRequest::after(RelativePath::root(), after, 1),
                Path::new("/scan"),
                &filter,
                &RelativePath::root(),
            )
            .expect("second filtered page should load");
        assert_eq!(second.entries[0].path, path("beta"));
        assert!(second.next_after.is_none());
    }

    #[test]
    fn indexed_directory_totals_preserve_the_original_deletion_snapshot() {
        let directory_id = file_id::FileId::new_inode(1, 1);
        let file_id = file_id::FileId::new_inode(1, 2);
        let directory_snapshot = EntrySnapshot {
            identity: Some(crate::native_path::NativeIdentity {
                file_id: directory_id,
                link_count: Some(1),
                reparse_point: false,
            }),
            kind: crate::model::NodeKind::Directory,
            apparent_bytes: 0,
            allocated_bytes: None,
            modified_nanos: Some(10),
        };
        let file_snapshot = EntrySnapshot {
            identity: Some(crate::native_path::NativeIdentity {
                file_id,
                link_count: Some(1),
                reparse_point: false,
            }),
            kind: crate::model::NodeKind::File,
            apparent_bytes: 4,
            allocated_bytes: Some(8),
            modified_nanos: Some(11),
        };
        let mut store = ScanStore::new(
            ScanGeneration::initial(),
            TemporaryStorage::with_limit_bytes(2 * 1024 * 1024),
        )
        .expect("store should initialize");
        add_path_run(
            &mut store,
            &[
                PathObservation::with_snapshot(
                    path("folder"),
                    PathEntryKind::Directory,
                    SummaryMetrics::leaf(0, ByteBounds::exact(0), ByteBounds::exact(0)),
                    Coverage::Complete,
                    Some(directory_snapshot.clone()),
                ),
                PathObservation::with_snapshot(
                    path("folder/file"),
                    PathEntryKind::File,
                    SummaryMetrics::leaf(4, ByteBounds::exact(0), ByteBounds::exact(0)),
                    Coverage::Complete,
                    Some(file_snapshot.clone()),
                ),
            ],
        );
        add_identity_run(
            &mut store,
            &[identity_observation("folder/file", file_id, 1)],
        );
        store.publish().expect("generation should publish");

        let root = store
            .published()
            .expect("generation should publish")
            .page(PageRequest::first(RelativePath::root(), 8))
            .expect("indexed root page should load");
        let folder = root
            .entries
            .iter()
            .find(|entry| entry.path == path("folder"))
            .expect("folder should remain a direct root child");
        assert_eq!(
            folder.metrics,
            SummaryMetrics {
                apparent_bytes: 4,
                allocated_bytes: ByteBounds::exact(8),
                reclaimable_bytes: ByteBounds::exact(8),
                descendants: 1,
            }
        );
        assert_eq!(folder.snapshot, Some(directory_snapshot));
        let nested = store
            .published()
            .expect("generation should publish")
            .page(PageRequest::first(path("folder"), 8))
            .expect("indexed nested page should load");
        assert_eq!(nested.entries[0].snapshot, Some(file_snapshot));
    }

    #[test]
    fn indexed_page_lookup_reaches_a_late_child_without_a_full_page_scan() {
        let mut store = ScanStore::new(
            ScanGeneration::initial(),
            TemporaryStorage::with_limit_bytes(8 * 1024 * 1024),
        )
        .expect("store should initialize");
        for first in (0..2_048).step_by(128) {
            let observations = (first..first + 128)
                .map(|index| path_observation(&format!("entry-{index:04}"), PathEntryKind::File, 1))
                .collect::<Vec<_>>();
            add_path_run(&mut store, &observations);
        }
        store.publish().expect("generation should publish");

        let page = store
            .published()
            .expect("generation should publish")
            .page(PageRequest::after(
                RelativePath::root(),
                PageCursor::at(path("entry-1791"), 0),
                8,
            ))
            .expect("late indexed page should load");
        assert_eq!(page.entries.len(), 8);
        assert_eq!(page.entries[0].path, path("entry-1792"));
        assert_eq!(page.entries[7].path, path("entry-1799"));
        assert_eq!(page.next_after, Some(PageCursor::at(path("entry-1799"), 0)));
    }

    #[test]
    fn child_queries_order_by_final_metric_then_native_name() {
        let mut store = ScanStore::new(
            ScanGeneration::initial(),
            TemporaryStorage::with_limit_bytes(8 * 1024 * 1024),
        )
        .expect("store should initialize");
        let observation = |name: &str, bytes: u128, inode: u64| {
            PathObservation::with_snapshot(
                path(name),
                PathEntryKind::File,
                SummaryMetrics::leaf(bytes, ByteBounds::exact(bytes), ByteBounds::exact(bytes)),
                Coverage::Complete,
                Some(EntrySnapshot {
                    identity: Some(NativeIdentity {
                        file_id: file_id::FileId::new_inode(1, inode),
                        link_count: Some(1),
                        reparse_point: false,
                    }),
                    kind: NodeKind::File,
                    apparent_bytes: bytes,
                    allocated_bytes: Some(bytes),
                    modified_nanos: None,
                }),
            )
        };
        add_path_run(
            &mut store,
            &[
                observation("alpha", 8, 1),
                observation("beta", 32, 2),
                observation("delta", 1, 3),
                observation("gamma", 32, 4),
            ],
        );
        store.publish().expect("generation should publish");

        let first = store
            .published()
            .expect("generation should publish")
            .page(PageRequest::first(RelativePath::root(), 2))
            .expect("first metric page should load");
        assert_eq!(
            first
                .entries
                .iter()
                .map(|entry| entry.path.clone())
                .collect::<Vec<_>>(),
            vec![path("beta"), path("gamma")]
        );
        let after = first.next_after.expect("more entries should remain");
        assert_eq!(after, PageCursor::at(path("gamma"), 32));
        let second = store
            .published()
            .expect("generation should publish")
            .page(PageRequest::after(RelativePath::root(), after, 2))
            .expect("second metric page should load");
        assert_eq!(
            second
                .entries
                .iter()
                .map(|entry| entry.path.clone())
                .collect::<Vec<_>>(),
            vec![path("alpha"), path("delta")]
        );
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
            published.page(PageRequest::after(
                path("alpha"),
                PageCursor::at(path("beta"), 0),
                1,
            )),
            Err(ScanPageError::InvalidCursor)
        ));
    }

    #[test]
    fn publication_retains_compact_query_indexes() {
        let store = published_store();
        let kinds = store
            .published()
            .expect("generation should publish")
            .manifest()
            .runs()
            .iter()
            .map(|entry| entry.descriptor.kind())
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            vec![
                RunKind::ChildQuery,
                RunKind::PathCatalog,
                RunKind::IdentityObservation,
                RunKind::DirectorySummary,
            ]
        );
    }
}
