//! Internal Criterion fixtures for canonical scan-store measurements.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::model::{ByteBounds, EntrySnapshot, NodeKind};
use crate::native_path::NativeIdentity;
use crate::scan_coordinator::{RelativePath, ScanGeneration};
use crate::scan_store::identity_observation::{
    IdentityObservation, append_identity_observation, compare_identity_observations,
};
use crate::scan_store::page::{PageCursor, PageRequest};
use crate::scan_store::path_observation::append_path_observation;
use crate::scan_store::path_reducer::{Coverage, PathEntryKind, PathObservation, SummaryMetrics};
use crate::scan_store::run_file::RunKind;
use crate::scan_store::session::{MAX_OBSERVATIONS_PER_BATCH, ScanStore};
use crate::temporary_storage::TemporaryStorage;
const STORAGE_MIB: usize = 256;
const QUERY_PAGE_ENTRIES: usize = 32;

/// A running scanner fixture for Criterion measurements of scheduler behavior.
pub struct FilesystemScanBenchmark {
    scan: crate::runtime::BenchmarkScan,
}

impl FilesystemScanBenchmark {
    /// Scans one fixture to completion with the selected scanner worker count.
    ///
    /// # Panics
    ///
    /// Panics when the scanner reports an error or does not finish in time.
    #[must_use]
    pub fn scan_to_completion(root: &Path, threads: usize) -> usize {
        crate::runtime::BenchmarkScan::scan_to_completion(root, threads)
            .expect("benchmark scanner should complete")
    }

    /// Starts an initial scan so a caller can measure bounded focus delivery.
    ///
    /// # Panics
    ///
    /// Panics when the scanner fixture cannot start.
    #[must_use]
    pub fn scanning(root: &Path, threads: usize) -> Self {
        Self {
            scan: crate::runtime::BenchmarkScan::start(root, threads)
                .expect("benchmark scanner should start"),
        }
    }

    /// Completes an initial scan and retains its scanner service for rebuild cancellation.
    ///
    /// # Panics
    ///
    /// Panics when the initial scanner fixture does not complete.
    #[must_use]
    pub fn ready_for_rebuild(root: &Path, threads: usize) -> Self {
        let benchmark = Self::scanning(root, threads);
        benchmark
            .scan
            .complete_initial_scan()
            .expect("benchmark initial scan should complete");
        benchmark
    }
    /// Completes an initial scan, then starts a fresh generation for focus-delivery timing.
    ///
    /// # Panics
    ///
    /// Panics when the scanner fixture cannot start its controlled focus scan.
    #[must_use]
    pub fn ready_for_focus(root: &Path, threads: usize) -> Self {
        let benchmark = Self::ready_for_rebuild(root, threads);
        benchmark
            .scan
            .start_focus_scan()
            .expect("benchmark focus scan should start");
        benchmark
    }

    /// Returns the time until every requested focus reaches the scanner scheduler.
    ///
    /// # Panics
    ///
    /// Panics when the bounded focus channel cannot process each request while scanning.
    #[must_use]
    pub fn focus_latency(&self, paths: &[PathBuf]) -> Duration {
        self.scan
            .focus_latency(paths)
            .expect("benchmark focus requests should be processed")
    }

    /// Returns the time until a newly requested rebuild acknowledges cancellation.
    ///
    /// # Panics
    ///
    /// Panics when the rebuild cannot start or does not acknowledge cancellation.
    #[must_use]
    pub fn cancel_rebuild_latency(&self) -> Duration {
        self.scan
            .cancel_rebuild_latency()
            .expect("benchmark rebuild cancellation should complete")
    }
}

/// Canonical layouts that exercise distinct storage and query paths.
#[derive(Clone, Copy, Debug)]
pub enum CanonicalWorkload {
    Flat {
        entries: usize,
    },
    Fanout {
        directories: usize,
        leaves_per_directory: usize,
    },
    Deep {
        depth: usize,
        leaves: usize,
    },
    SharedLinks {
        groups: usize,
        links_per_group: usize,
    },
}

impl CanonicalWorkload {
    #[must_use]
    pub const fn entry_count(self) -> usize {
        match self {
            Self::Flat { entries } => entries,
            Self::Fanout {
                directories,
                leaves_per_directory,
            } => directories.saturating_mul(leaves_per_directory.saturating_add(1)),
            Self::Deep { depth, leaves } => depth.saturating_add(leaves),
            Self::SharedLinks {
                groups,
                links_per_group,
            } => groups.saturating_mul(links_per_group),
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Flat { .. } => "flat",
            Self::Fanout { .. } => "fanout",
            Self::Deep { .. } => "deep",
            Self::SharedLinks { .. } => "shared-links",
        }
    }
}

/// Deterministic logical I/O and phase timing for one canonical publication.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CanonicalStoreMetrics {
    pub observations: usize,
    pub input_written_bytes: u64,
    pub merge_read_bytes: u64,
    pub merge_written_bytes: u64,
    pub reduction_read_bytes: u64,
    pub reduction_written_bytes: u64,
    pub publication_read_bytes: u64,
    pub publication_written_bytes: u64,
    pub retained_bytes: u64,
    pub peak_temporary_bytes: u64,
    pub ingestion_elapsed: Duration,
    pub publication_elapsed: Duration,
    pub ingestion_cpu: Option<Duration>,
    pub publication_cpu: Option<Duration>,
}

/// A published canonical generation and one representative bounded page query.
pub struct CanonicalStoreBenchmark {
    store: ScanStore,
    storage: TemporaryStorage,
    query: BenchmarkPageQuery,
    metrics: CanonicalStoreMetrics,
}

#[derive(Clone)]
struct BenchmarkPageQuery {
    folder: RelativePath,
    after: Option<PageCursor>,
}

fn benchmark_metrics(
    store: &ScanStore,
    storage: &TemporaryStorage,
    observations: usize,
    ingestion_elapsed: Duration,
    publication_elapsed: Duration,
    ingestion_cpu: Option<Duration>,
    publication_cpu: Option<Duration>,
) -> CanonicalStoreMetrics {
    let io = store.io_metrics();
    CanonicalStoreMetrics {
        observations,
        input_written_bytes: io.input_written_bytes,
        merge_read_bytes: io.merge_read_bytes,
        merge_written_bytes: io.merge_written_bytes,
        reduction_read_bytes: io.reduction_read_bytes,
        reduction_written_bytes: io.reduction_written_bytes,
        publication_read_bytes: io.publication_read_bytes,
        publication_written_bytes: io.publication_written_bytes,
        retained_bytes: storage.used(),
        peak_temporary_bytes: storage.peak_used(),
        ingestion_elapsed,
        publication_elapsed,
        ingestion_cpu,
        publication_cpu,
    }
}

#[cfg(unix)]
fn process_cpu_time() -> Option<Duration> {
    use nix::sys::resource::{UsageWho, getrusage};
    use nix::sys::time::TimeValLike as _;

    let usage = getrusage(UsageWho::RUSAGE_SELF).ok()?;
    let micros = usage
        .user_time()
        .num_microseconds()
        .checked_add(usage.system_time().num_microseconds())?;
    u64::try_from(micros).ok().map(Duration::from_micros)
}

#[cfg(not(unix))]
const fn process_cpu_time() -> Option<Duration> {
    None
}

fn elapsed_cpu_since(start: Option<Duration>) -> Option<Duration> {
    process_cpu_time()
        .zip(start)
        .map(|(end, start)| end.saturating_sub(start))
}

impl CanonicalStoreBenchmark {
    /// Builds and publishes a deterministic workload through bounded scanner batches.
    ///
    /// # Panics
    ///
    /// Panics when the benchmark's fixed scan-store budget cannot hold its
    /// generated workload or canonical publication fails.
    #[must_use]
    pub fn build(workload: CanonicalWorkload) -> Self {
        Self::build_with_storage_mib(workload, STORAGE_MIB)
    }

    /// Builds and publishes a deterministic workload through bounded scanner batches
    /// with an explicit storage ceiling.
    ///
    /// # Panics
    ///
    /// Panics when the storage ceiling cannot hold the generated workload or
    /// canonical publication fails.
    #[must_use]
    pub fn build_with_storage_mib(workload: CanonicalWorkload, storage_mib: usize) -> Self {
        let storage = TemporaryStorage::scan_store_from_mib(storage_mib)
            .expect("benchmark scan-store capacity should be valid");
        let mut store = ScanStore::new(ScanGeneration::initial(), storage.clone())
            .expect("benchmark store should initialize");
        let (paths, identities, query) = observations_for(workload);
        let observations = paths.len();
        let ingestion_cpu_started = process_cpu_time();
        let ingestion_started = Instant::now();
        let mut paths = paths.into_iter();
        let mut identities = identities.into_iter();
        loop {
            let paths = paths
                .by_ref()
                .take(MAX_OBSERVATIONS_PER_BATCH)
                .collect::<Vec<_>>();
            let identities = identities
                .by_ref()
                .take(MAX_OBSERVATIONS_PER_BATCH)
                .collect::<Vec<_>>();
            if paths.is_empty() && identities.is_empty() {
                break;
            }
            store
                .append_observation_batch(paths, identities)
                .expect("benchmark facts should append");
        }
        let ingestion_elapsed = ingestion_started.elapsed();
        let ingestion_cpu = elapsed_cpu_since(ingestion_cpu_started);
        let publication_cpu_started = process_cpu_time();
        let publication_started = Instant::now();
        store
            .publish()
            .expect("benchmark generation should publish");
        let publication_elapsed = publication_started.elapsed();
        let publication_cpu = elapsed_cpu_since(publication_cpu_started);
        let metrics = benchmark_metrics(
            &store,
            &storage,
            observations,
            ingestion_elapsed,
            publication_elapsed,
            ingestion_cpu,
            publication_cpu,
        );
        Self {
            store,
            storage,
            query,
            metrics,
        }
    }

    /// Builds a canonical workload from premerged raw runs.
    ///
    /// This isolates reduction and query cost at large scale. It preserves the
    /// canonical result of bounded scanner batches while intentionally excluding
    /// their fan-in work, which the bounded workload matrix measures separately.
    ///
    /// # Panics
    ///
    /// Panics when the storage ceiling cannot hold the generated workload or
    /// canonical publication fails.
    #[must_use]
    pub fn build_premerged(workload: CanonicalWorkload, storage_mib: usize) -> Self {
        let storage = TemporaryStorage::scan_store_from_mib(storage_mib)
            .expect("benchmark scan-store capacity should be valid");
        let mut store = ScanStore::new(ScanGeneration::initial(), storage.clone())
            .expect("benchmark store should initialize");
        let (mut paths, mut identities, query) = observations_for(workload);
        let observations = paths.len();
        let ingestion_cpu_started = process_cpu_time();
        let ingestion_started = Instant::now();
        paths.sort_unstable_by(|left, right| left.path.cmp(&right.path));
        if !paths.is_empty() {
            let mut writer = store
                .begin_input_run(RunKind::PathObservation)
                .expect("benchmark path input run should begin");
            let mut key = Vec::new();
            let mut value = Vec::new();
            for observation in &paths {
                append_path_observation(&mut writer, observation, &mut key, &mut value)
                    .expect("benchmark path observation should append");
            }
            store
                .accept_input_run(writer.seal().expect("benchmark path run should seal"))
                .expect("benchmark path run should be accepted");
        }
        identities.sort_unstable_by(compare_identity_observations);
        if !identities.is_empty() {
            let mut writer = store
                .begin_input_run(RunKind::IdentityObservation)
                .expect("benchmark identity input run should begin");
            let mut key = Vec::new();
            let mut value = Vec::new();
            for observation in &identities {
                append_identity_observation(&mut writer, observation, &mut key, &mut value)
                    .expect("benchmark identity observation should append");
            }
            store
                .accept_input_run(writer.seal().expect("benchmark identity run should seal"))
                .expect("benchmark identity run should be accepted");
        }
        let ingestion_elapsed = ingestion_started.elapsed();
        let ingestion_cpu = elapsed_cpu_since(ingestion_cpu_started);
        let publication_cpu_started = process_cpu_time();
        let publication_started = Instant::now();
        store
            .publish()
            .expect("benchmark generation should publish");
        let publication_elapsed = publication_started.elapsed();
        let publication_cpu = elapsed_cpu_since(publication_cpu_started);
        let metrics = benchmark_metrics(
            &store,
            &storage,
            observations,
            ingestion_elapsed,
            publication_elapsed,
            ingestion_cpu,
            publication_cpu,
        );
        Self {
            store,
            storage,
            query,
            metrics,
        }
    }

    #[must_use]
    pub fn retained_bytes(&self) -> u64 {
        self.storage.used()
    }

    /// Returns the high-water mark of scan-store temporary storage during publication.
    #[must_use]
    pub fn peak_bytes(&self) -> u64 {
        self.storage.peak_used()
    }

    /// Returns logical serialized I/O and process CPU time for this publication.
    #[must_use]
    pub const fn metrics(&self) -> CanonicalStoreMetrics {
        self.metrics
    }

    /// Reads a representative bounded page without rebuilding the generation.
    ///
    /// # Panics
    ///
    /// Panics if the benchmark fixture no longer retains its published generation
    /// or the generated representative page cannot be read.
    #[must_use]
    pub fn query_representative_page(&mut self) -> usize {
        let request = self.query.after.as_ref().map_or_else(
            || PageRequest::first(self.query.folder.clone(), QUERY_PAGE_ENTRIES),
            |after| {
                PageRequest::after(self.query.folder.clone(), after.clone(), QUERY_PAGE_ENTRIES)
            },
        );
        self.store
            .published_mut()
            .expect("benchmark generation should remain published")
            .page(request)
            .expect("benchmark page should load")
            .entries
            .len()
    }
}

fn observations_for(
    workload: CanonicalWorkload,
) -> (
    Vec<PathObservation>,
    Vec<IdentityObservation>,
    BenchmarkPageQuery,
) {
    match workload {
        CanonicalWorkload::Flat { entries } => flat_observations(entries),
        CanonicalWorkload::Fanout {
            directories,
            leaves_per_directory,
        } => fanout_observations(directories, leaves_per_directory),
        CanonicalWorkload::Deep { depth, leaves } => deep_observations(depth, leaves),
        CanonicalWorkload::SharedLinks {
            groups,
            links_per_group,
        } => shared_link_observations(groups, links_per_group),
    }
}

fn flat_observations(
    entries: usize,
) -> (
    Vec<PathObservation>,
    Vec<IdentityObservation>,
    BenchmarkPageQuery,
) {
    let mut paths = Vec::with_capacity(entries);
    let mut identities = Vec::new();
    for index in 0..entries {
        let (path, identity) = file_observation(relative_path(index), index, 1);
        paths.push(path);
        identities.extend(identity);
    }
    let after = entries
        .checked_sub(QUERY_PAGE_ENTRIES.saturating_add(1))
        .map(|index| PageCursor::at(relative_path(index), 1));
    (
        paths,
        identities,
        BenchmarkPageQuery {
            folder: RelativePath::root(),
            after,
        },
    )
}

fn fanout_observations(
    directories: usize,
    leaves_per_directory: usize,
) -> (
    Vec<PathObservation>,
    Vec<IdentityObservation>,
    BenchmarkPageQuery,
) {
    let mut paths = Vec::with_capacity(directories.saturating_mul(leaves_per_directory + 1));
    let mut identities = Vec::new();
    let mut file_index = 0_usize;
    for directory_index in 0..directories {
        let directory = branch_path(directory_index);
        paths.push(directory_observation(directory.clone(), directory_index));
        for leaf_index in 0..leaves_per_directory {
            let leaf = directory
                .to_path_buf()
                .join(format!("leaf-{leaf_index:08}"));
            let relative =
                RelativePath::from_path(&leaf).expect("benchmark path should be relative");
            let (path, identity) = file_observation(relative, file_index, 1);
            paths.push(path);
            identities.extend(identity);
            file_index = file_index.saturating_add(1);
        }
    }
    let after = directories
        .checked_sub(QUERY_PAGE_ENTRIES.saturating_add(1))
        .map(|index| {
            PageCursor::at(
                branch_path(index),
                u128::try_from(leaves_per_directory).unwrap_or(u128::MAX),
            )
        });
    (
        paths,
        identities,
        BenchmarkPageQuery {
            folder: RelativePath::root(),
            after,
        },
    )
}

fn deep_observations(
    depth: usize,
    leaves: usize,
) -> (
    Vec<PathObservation>,
    Vec<IdentityObservation>,
    BenchmarkPageQuery,
) {
    let mut paths = Vec::with_capacity(depth.saturating_add(leaves));
    let mut identities = Vec::new();
    let mut directory = PathBuf::new();
    for level in 0..depth {
        directory.push(format!("level-{level:08}"));
        let relative =
            RelativePath::from_path(&directory).expect("benchmark path should be relative");
        paths.push(directory_observation(relative, level));
    }
    let folder = RelativePath::from_path(&directory).expect("benchmark path should be relative");
    for leaf_index in 0..leaves {
        let leaf = directory.join(format!("leaf-{leaf_index:08}"));
        let relative = RelativePath::from_path(&leaf).expect("benchmark path should be relative");
        let (path, identity) = file_observation(relative, leaf_index, 1);
        paths.push(path);
        identities.extend(identity);
    }
    let after = leaves
        .checked_sub(QUERY_PAGE_ENTRIES.saturating_add(1))
        .map(|index| {
            let path = directory.join(format!("leaf-{index:08}"));
            PageCursor::at(
                RelativePath::from_path(&path).expect("benchmark path should be relative"),
                1,
            )
        });
    (paths, identities, BenchmarkPageQuery { folder, after })
}

fn shared_link_observations(
    groups: usize,
    links_per_group: usize,
) -> (
    Vec<PathObservation>,
    Vec<IdentityObservation>,
    BenchmarkPageQuery,
) {
    let mut paths = Vec::with_capacity(groups.saturating_mul(links_per_group));
    let mut identities = Vec::with_capacity(groups.saturating_mul(links_per_group));
    for group in 0..groups {
        for link in 0..links_per_group {
            let path = PathBuf::from(format!("link-{group:08}-{link:04}"));
            let relative =
                RelativePath::from_path(&path).expect("benchmark path should be relative");
            let (observation, identity) = file_observation(relative, group, links_per_group);
            paths.push(observation);
            identities.extend(identity);
        }
    }
    (
        paths,
        identities,
        BenchmarkPageQuery {
            folder: RelativePath::root(),
            after: None,
        },
    )
}

fn directory_observation(path: RelativePath, index: usize) -> PathObservation {
    PathObservation::with_snapshot(
        path,
        PathEntryKind::Directory,
        SummaryMetrics::leaf(0, ByteBounds::exact(0), ByteBounds::exact(0)),
        Coverage::Complete,
        Some(EntrySnapshot {
            identity: Some(NativeIdentity {
                file_id: file_id::FileId::new_inode(2, u64::try_from(index).unwrap_or(u64::MAX)),
                link_count: Some(1),
                reparse_point: false,
            }),
            kind: NodeKind::Directory,
            apparent_bytes: 0,
            allocated_bytes: None,
            modified_nanos: Some(1),
        }),
    )
}

fn file_observation(
    path: RelativePath,
    index: usize,
    link_count: usize,
) -> (PathObservation, Option<IdentityObservation>) {
    let file_id = file_id::FileId::new_inode(1, u64::try_from(index).unwrap_or(u64::MAX));
    let link_count = u64::try_from(link_count).unwrap_or(u64::MAX);
    let single_link = link_count == 1;
    let allocated = single_link.then_some(ByteBounds::exact(1));
    let observation = PathObservation::with_snapshot(
        path.clone(),
        PathEntryKind::File,
        SummaryMetrics::leaf(
            1,
            allocated.unwrap_or_else(|| ByteBounds::exact(0)),
            allocated.unwrap_or_else(|| ByteBounds::exact(0)),
        ),
        Coverage::Complete,
        Some(EntrySnapshot {
            identity: Some(NativeIdentity {
                file_id,
                link_count: Some(link_count),
                reparse_point: false,
            }),
            kind: NodeKind::File,
            apparent_bytes: 1,
            allocated_bytes: Some(1),
            modified_nanos: Some(1),
        }),
    );
    let identity = (!single_link).then_some(IdentityObservation {
        path,
        file_id,
        declared_links: Some(link_count),
        allocated_bytes: ByteBounds::exact(1),
    });
    (observation, identity)
}

fn relative_path(index: usize) -> RelativePath {
    let name = format!("entry-{index:08}");
    RelativePath::from_path(Path::new(&name)).expect("benchmark path should be relative")
}

fn branch_path(index: usize) -> RelativePath {
    let name = format!("branch-{index:08}");
    RelativePath::from_path(Path::new(&name)).expect("benchmark path should be relative")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_publication_reports_input_merge_and_publication_metrics() {
        let entries = MAX_OBSERVATIONS_PER_BATCH.saturating_mul(8);
        let fixture = CanonicalStoreBenchmark::build(CanonicalWorkload::Flat { entries });
        let metrics = fixture.metrics();

        assert_eq!(metrics.observations, entries);
        assert!(metrics.input_written_bytes > 0);
        assert!(metrics.merge_read_bytes > 0);
        assert!(metrics.merge_written_bytes > 0);
        assert!(metrics.reduction_read_bytes > 0);
        assert!(metrics.reduction_written_bytes > 0);
        assert!(metrics.publication_read_bytes > 0);
        assert!(metrics.publication_written_bytes > 0);
        assert!(metrics.retained_bytes > 0);
        assert!(metrics.peak_temporary_bytes >= metrics.retained_bytes);
    }
}
