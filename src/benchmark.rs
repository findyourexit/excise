//! Internal Criterion fixtures for repeatable scan-store measurements.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::TestBackend;

use crate::error::AppError;
use crate::input::{InputEvent, InputSource};
use crate::model::{ByteBounds, EntrySnapshot, NodeKind};
use crate::native_path::{NativeIdentity, identity_for};
use crate::outcome::{OperationOutcome, RunSummary};
use crate::runtime::{OwnerLoopProbe, RuntimeSettings, SystemClock};
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

pub use crate::runtime::{
    HISTOGRAM_BUCKETS, OwnerLoopReport, OwnerPhase, PhaseHistogram, WorkerEventKind,
};

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

/// Terminal size of the in-memory backend that the owner-loop probe draws into.
const OWNER_LOOP_COLUMNS: u16 = 120;
const OWNER_LOOP_ROWS: u16 = 40;
/// The scanner event buffer a default `excise` run uses.
const OWNER_LOOP_EVENT_CAPACITY: usize = 256;
/// One input poll sleeps at most this long, so completion and the cap are noticed promptly.
const OWNER_LOOP_MAX_POLL: Duration = Duration::from_millis(50);
/// A quit that did not end the loop is offered again after this long.
const OWNER_LOOP_QUIT_RETRY: Duration = Duration::from_millis(250);
/// The longest wall-clock cap a run honors. It keeps every deadline representable.
const OWNER_LOOP_MAX_CAP: Duration = Duration::from_hours(24);

/// Settings for one owner-loop scan. Every field is explicit so two runs differ
/// only where the caller says they do.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OwnerLoopScanOptions {
    /// Scanner worker threads.
    pub scan_threads: usize,
    /// Mirrors `--reduced-motion`: the map snaps to each new layout instead of tweening.
    pub reduced_motion: bool,
    /// Mirrors the command-line tool, which always animates loading; only
    /// `reduced_motion` and the theme quiet it.
    pub animate_loading: bool,
    /// The longest the run may take. A scan unfinished by then is cancelled and
    /// the run reports itself incomplete instead of hanging.
    pub wall_clock_cap: Duration,
}

/// The outcome of one probed owner-loop scan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OwnerLoopScanRun {
    /// Wall time of the whole owner-loop run: startup, scan, quit, and worker shutdown.
    pub wall: Duration,
    /// Phase timings and counters the owner loop recorded about itself.
    pub report: OwnerLoopReport,
    /// The loop's own result. A run capped before its scan finished is `Cancelled`.
    pub outcome: OperationOutcome<RunSummary>,
}

impl OwnerLoopScanRun {
    /// Returns whether the scan finished before the wall-clock cap.
    #[must_use]
    pub const fn complete(&self) -> bool {
        self.report.time_to_scan_complete().is_some()
    }

    /// Returns scanned entries handled per second: over the time to completion
    /// for a finished scan, over the whole capped run otherwise.
    #[must_use]
    pub fn entries_per_second(&self) -> u64 {
        let window = self
            .report
            .time_to_scan_complete()
            .unwrap_or(self.wall)
            .as_nanos();
        let per_second = u128::from(self.report.scan_entries_handled())
            .saturating_mul(1_000_000_000)
            .checked_div(window)
            .unwrap_or(0);
        u64::try_from(per_second).unwrap_or(u64::MAX)
    }
}

/// Runs the production owner loop against a directory tree and measures it.
pub struct OwnerLoopScanBenchmark;

impl OwnerLoopScanBenchmark {
    /// Runs the real owner loop until its scan completes, then quits.
    ///
    /// The run calls `runtime::run` with an in-memory ratatui backend, the system
    /// clock, and an input source that sends nothing until the loop has handled
    /// the scan's completion, then Ctrl-C and `y`. If the scan has not finished by
    /// `wall_clock_cap`, the same quit cancels it, so the call always returns;
    /// check [`OwnerLoopScanRun::complete`]. Scan-store files live in a private
    /// temporary directory that is removed before this returns.
    ///
    /// # Errors
    ///
    /// Returns an error when `root` cannot be inspected or the owner loop rejects
    /// it as a scan root (a symbolic link, for one), or when the loop itself fails.
    pub fn run(root: &Path, options: OwnerLoopScanOptions) -> Result<OwnerLoopScanRun, AppError> {
        let metadata = std::fs::symlink_metadata(root).map_err(|error| {
            AppError::io("could not inspect the owner-loop benchmark root", error)
        })?;
        let root_identity = identity_for(root, &metadata)
            .map_err(|error| {
                AppError::io("could not identify the owner-loop benchmark root", error)
            })?
            .ok_or_else(|| {
                AppError::Config("the owner-loop benchmark root must not be a link".to_string())
            })?;
        let scan_store_dir = tempfile::Builder::new()
            .prefix("excise-owner-loop-")
            .tempdir()
            .map_err(|error| {
                AppError::io(
                    "could not create the owner-loop scan-store directory",
                    error,
                )
            })?;
        let settings = RuntimeSettings {
            root: root.to_path_buf(),
            root_identity,
            scan_threads: options.scan_threads,
            event_capacity: OWNER_LOOP_EVENT_CAPACITY,
            cross_filesystems: false,
            exclusions: Vec::new(),
            memory_mib: crate::model::DEFAULT_PROCESS_MIB,
            temporary_storage_mib: crate::temporary_storage::DEFAULT_TEMPORARY_STORAGE_MIB,
            scan_store_mib: Some(STORAGE_MIB),
            scan_store_reserve_mib: None,
            scan_store_dir: Some(scan_store_dir.path().to_path_buf()),
            apparent_size: false,
            disable_delete_confirmation: false,
            reduced_motion: options.reduced_motion,
            monochrome: false,
            animate_loading: options.animate_loading,
            theme: crate::theme::ThemeId::default(),
            ascii: false,
            mouse: false,
            keymap: crate::config::KeyPreset::default(),
            custom_keys: None,
            config_path: None,
            monochrome_locked: false,
        };
        let probe = OwnerLoopProbe::new();
        let input =
            QuitAfterScan::new(probe.clone(), options.wall_clock_cap, OWNER_LOOP_QUIT_RETRY);
        let started = Instant::now();
        let outcome = crate::runtime::run(
            TestBackend::new(OWNER_LOOP_COLUMNS, OWNER_LOOP_ROWS),
            Box::new(input),
            settings,
            Box::new(SystemClock::new()),
        )?;
        let wall = started.elapsed();
        Ok(OwnerLoopScanRun {
            wall,
            report: probe.report(),
            outcome,
        })
    }
}

/// The key of the quit sequence that `read` delivers next.
#[derive(Clone, Copy)]
enum QuitKey {
    Interrupt,
    Confirm,
}

/// Drives the owner loop like a user who waits out the scan: it sends nothing
/// until the loop has handled the scan's completion or the wall-clock cap
/// passes, then Ctrl-C and `y`. A quit that leaves the loop running is offered
/// again after `retry`, so a modal that swallowed it cannot stall the run.
struct QuitAfterScan {
    probe: OwnerLoopProbe,
    deadline: Instant,
    retry: Duration,
    next_key: QuitKey,
    resume_at: Option<Instant>,
}

impl QuitAfterScan {
    fn new(probe: OwnerLoopProbe, wall_clock_cap: Duration, retry: Duration) -> Self {
        Self {
            probe,
            deadline: Instant::now() + wall_clock_cap.min(OWNER_LOOP_MAX_CAP),
            retry,
            next_key: QuitKey::Interrupt,
            resume_at: None,
        }
    }

    fn quit_due(&self, now: Instant) -> bool {
        (self.probe.scan_complete() || now >= self.deadline)
            && self.resume_at.is_none_or(|resume| now >= resume)
    }
}

impl InputSource for QuitAfterScan {
    fn poll(&mut self, timeout: Duration) -> Result<bool, AppError> {
        if self.quit_due(Instant::now()) {
            return Ok(true);
        }
        if timeout.is_zero() {
            return Ok(false);
        }
        // Wait the way a terminal poll would, but wake for the cap and for completion.
        let until_deadline = self.deadline.saturating_duration_since(Instant::now());
        let mut nap = timeout.min(OWNER_LOOP_MAX_POLL);
        if !until_deadline.is_zero() {
            nap = nap.min(until_deadline);
        }
        std::thread::sleep(nap);
        Ok(self.quit_due(Instant::now()))
    }

    fn read(&mut self) -> Result<InputEvent, AppError> {
        let key = match self.next_key {
            QuitKey::Interrupt => {
                self.next_key = QuitKey::Confirm;
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
            }
            QuitKey::Confirm => {
                self.next_key = QuitKey::Interrupt;
                self.resume_at = Some(Instant::now() + self.retry);
                KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)
            }
        };
        Ok(InputEvent::Terminal(Event::Key(key)))
    }

    fn owner_loop_probe(&self) -> Option<&OwnerLoopProbe> {
        Some(&self.probe)
    }
}

/// Repeatable layouts that exercise distinct storage and query paths.
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

/// Deterministic logical I/O and phase timing for one published scan.
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

/// A published scan and one representative bounded page query.
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
    /// generated workload or publishing fails.
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
    /// publishing fails.
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

    /// Builds a repeatable workload from already merged raw runs.
    ///
    /// This isolates reduction and query cost at large scale. It preserves the
    /// final result of bounded scanner batches while leaving out the work of
    /// combining input runs, which the bounded workload matrix measures separately.
    ///
    /// # Panics
    ///
    /// Panics when the storage ceiling cannot hold the generated workload or
    /// publishing fails.
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
        let batches = 8;
        let entries = MAX_OBSERVATIONS_PER_BATCH.saturating_mul(batches);
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

    fn tree(directories: usize, files_per_directory: usize) -> tempfile::TempDir {
        let root = tempfile::tempdir().expect("owner-loop fixture root should exist");
        for directory in 0..directories {
            let path = root.path().join(format!("dir-{directory}"));
            std::fs::create_dir(&path).expect("fixture directory should be created");
            for file in 0..files_per_directory {
                std::fs::write(path.join(format!("file-{file}")), b"x")
                    .expect("fixture file should be written");
            }
        }
        root
    }

    fn options(reduced_motion: bool, wall_clock_cap: Duration) -> OwnerLoopScanOptions {
        OwnerLoopScanOptions {
            scan_threads: 2,
            reduced_motion,
            animate_loading: true,
            wall_clock_cap,
        }
    }

    #[test]
    fn a_probed_run_records_admission_publication_and_frames() {
        let (directories, files_per_directory) = (3, 6);
        let root = tree(directories, files_per_directory);
        let entries = u64::try_from(directories * (files_per_directory + 1))
            .expect("fixture size fits in u64");
        let run = OwnerLoopScanBenchmark::run(root.path(), options(true, Duration::from_secs(120)))
            .expect("the owner loop should run");
        let report = &run.report;

        assert!(run.complete());
        let OperationOutcome::Exact(summary) = &run.outcome else {
            panic!("a finished scan should be exact: {:?}", run.outcome);
        };
        assert_eq!(report.scan_entries_handled(), entries);
        assert_eq!(
            summary.scanned_entries, entries,
            "the probe agrees with the loop's own summary"
        );
        assert!(run.entries_per_second() > 0);
        assert!(
            report
                .time_to_scan_complete()
                .is_some_and(|elapsed| elapsed <= run.wall)
        );

        assert!(report.frames_rendered() >= 1);
        assert_eq!(report.phase(OwnerPhase::Publication).count(), 1);
        assert_eq!(
            report.worker_event(WorkerEventKind::ScanFinished).count(),
            1
        );
        assert_eq!(
            report.phase(OwnerPhase::Input).count(),
            2,
            "the quit is Ctrl-C then y and nothing before"
        );
        let admissions = report.phase(OwnerPhase::Admission).count();
        assert!(admissions >= 1);
        assert!(report.scan_runs_admitted() >= admissions);
        assert!(report.worker_event(WorkerEventKind::ScanBatch).count() >= admissions);
    }

    #[test]
    fn an_expired_cap_cancels_the_scan_and_reports_it_incomplete() {
        let root = tree(3, 6);
        let run = OwnerLoopScanBenchmark::run(root.path(), options(true, Duration::ZERO))
            .expect("a capped run should still return");

        assert!(!run.complete());
        assert!(
            matches!(
                run.outcome,
                OperationOutcome::Cancelled { precise: true, .. }
            ),
            "the capped scan is cancelled: {:?}",
            run.outcome
        );
        assert_eq!(run.report.time_to_scan_complete(), None);
        assert_eq!(run.report.phase(OwnerPhase::Publication).count(), 0);
        assert_eq!(run.report.phase(OwnerPhase::Input).count(), 2);
    }

    #[test]
    fn a_root_that_cannot_be_inspected_is_an_error_not_a_hang() {
        let parent = tree(1, 1);
        let error = OwnerLoopScanBenchmark::run(
            &parent.path().join("missing"),
            options(true, Duration::from_secs(120)),
        )
        .expect_err("a missing root cannot be scanned");
        assert!(matches!(error, AppError::Io { .. }), "{error}");
    }

    fn quit_key(input: &mut QuitAfterScan) -> KeyEvent {
        match input.read().expect("the quit source should always read") {
            InputEvent::Terminal(Event::Key(key)) => key,
            _ => panic!("the quit source sends only key events"),
        }
    }

    #[test]
    fn the_quit_source_waits_for_completion_then_offers_the_quit_until_it_lands() {
        let probe = OwnerLoopProbe::new();
        let mut input = QuitAfterScan::new(
            probe.clone(),
            Duration::from_secs(3_600),
            Duration::from_millis(1),
        );
        assert!(!input.poll(Duration::ZERO).expect("poll should succeed"));
        assert!(
            !input
                .poll(Duration::from_millis(1))
                .expect("poll should succeed"),
            "a scan in progress is not an input event"
        );

        probe.mark_scan_complete_for_test();
        assert!(input.poll(Duration::ZERO).expect("poll should succeed"));
        let interrupt = quit_key(&mut input);
        assert_eq!(
            (interrupt.code, interrupt.modifiers),
            (KeyCode::Char('c'), KeyModifiers::CONTROL)
        );
        assert!(
            input.poll(Duration::ZERO).expect("poll should succeed"),
            "the confirmation follows at once"
        );
        let confirm = quit_key(&mut input);
        assert_eq!(
            (confirm.code, confirm.modifiers),
            (KeyCode::Char('y'), KeyModifiers::NONE)
        );
        assert!(
            !input.poll(Duration::ZERO).expect("poll should succeed"),
            "a delivered quit is not repeated at once"
        );

        std::thread::sleep(Duration::from_millis(3));
        assert!(
            input.poll(Duration::ZERO).expect("poll should succeed"),
            "a quit that left the loop running is offered again"
        );
        assert_eq!(quit_key(&mut input).code, KeyCode::Char('c'));
    }

    #[test]
    fn the_quit_source_quits_at_the_cap_without_a_completed_scan() {
        let probe = OwnerLoopProbe::new();
        let mut input = QuitAfterScan::new(probe.clone(), Duration::ZERO, OWNER_LOOP_QUIT_RETRY);
        assert!(!probe.scan_complete());
        assert!(
            input.poll(Duration::ZERO).expect("poll should succeed"),
            "a cap that has passed ends the run"
        );
        assert_eq!(quit_key(&mut input).code, KeyCode::Char('c'));
    }
}
