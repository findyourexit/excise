//! The store thread: the one thread that writes an interactive session's scan-store runs.
//!
//! Admitting a scanner batch, merging runs, and publishing a finished scan are blocking file I/O
//! whose cost grows with the scan: one merge, or the publication of the whole scan, can take
//! longer than the interface may go without drawing a frame. Run on the owner loop, they stalled
//! it, and every key and frame waited behind them. A session's [`ScanStore`] therefore lives on
//! this thread (`excise-scan-store`), and the owner loop holds a [`StoreHandle`].
//!
//! # Ownership
//!
//! * **The owner decides, the store thread applies.** Every [`StoreCommand`] is a decision the
//!   owner loop made (admit this batch, publish, begin that generation), applied in the order it
//!   was sent. The store thread decides no application state; it changes the store, and tells
//!   the owner what happened.
//! * **A published generation is a value that changes hands.** The store thread builds it and
//!   sends it ([`StoreEvent::Published`]); the owner swaps it in ([`StoreHandle::install`]) and
//!   reads its pages synchronously from then on, as it always has. The store thread keeps nothing
//!   of it. A deletion's overlay is built from the generation the owner has installed, which the
//!   owner describes in the command ([`OverlayBase`]) and keeps until the overlay's generation
//!   arrives, so those files outlive every read of them, and a generation the owner dropped
//!   unseen is never the one a later overlay derives from.
//! * **What the thread reads of the file system is one folder, for an overlay.** Removing an
//!   entry changes the folder that held it, and the map must say what that folder is now: the
//!   command carries the scan's root, and the thread reads that one folder through the handles of
//!   the folders above it, and lists it, one stat for each entry, keeping a digest of the
//!   listing and nothing of the listing itself ([`FolderDigest`]). The overlay records the
//!   folder's new metadata only when the folder holds the entries the map lists for it, because
//!   that metadata also describes whatever else happened to the folder since the scan. It writes
//!   nothing there.
//! * **Every queue is bounded by constants, not by the tree** ([`STORE_COMMAND_CAPACITY`],
//!   [`STORE_RESULT_CAPACITY`]). The owner stops taking scanner events while the store thread
//!   is behind ([`StoreHandle::has_room_for_batch`]), which backs the scanner up through its own
//!   bounded channel and the in-flight batch cap.
//! * **Stopping is prompt and ends before the session's storage does.** [`StoreHandle::shutdown`]
//!   stops the session's quota, which makes the store thread's next block of I/O fail, drops the
//!   queues, and joins the thread; the session directory is removed only afterwards. The quota
//!   cannot interrupt a call that is already inside a hung file system, so the owner's wait for
//!   the thread ([`StoreHandle::shutdown_with`]) is one a second stop request ends: past a short
//!   grace the thread is left to the exit of the process, with the session directory it still
//!   holds for the next start's sweep.
//!
//! Until [`StoreHandle::start_thread`], commands run on the calling thread as they are sent and
//! their results queue locally, so a handle behaves the same in tests and before the loop starts.

use std::collections::{BTreeSet, VecDeque};
use std::ffi::OsStr;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use crossbeam_channel::{Receiver, SendTimeoutError, Sender, TryRecvError, TrySendError, bounded};

use super::folder_digest::FolderDigest;
use super::page::{PageRequest, ScanPage};
use super::path_reducer::PathEntryKind;
use super::session::{
    MAX_INFLIGHT_SCAN_BATCHES, MAX_TRACKED_UNREADABLE_DIRECTORIES, OverlayBase, Publication,
    PublishedGeneration, RefreshedFolder, ScanInputRunFactory, ScanStore, ScanStoreError,
    SealedBatch, SummaryOnlyGeneration, scan_store_capacity_message,
};
use crate::deletion::{PlannedKind, current_folder_listing};
use crate::native_path::NativeIdentity;
use crate::scan_coordinator::{RelativePath, ScanGeneration, WorkLease};
use crate::scan_session::ScanSessionId;
use crate::signals::{Joined, ShutdownWait, WaitableThread, spawn_waitable};
use crate::temporary_storage::TemporaryStorage;

/// The name of the thread that owns a session's scan store.
pub(crate) const STORE_THREAD_NAME: &str = "excise-scan-store";

/// Commands one admitted batch adds to the queue: the notes the owner flushed ahead of it, and
/// the batch itself.
const COMMANDS_PER_BATCH: usize = 2;

/// Room the queue keeps for control commands, which the batch gate never uses. Each control
/// command follows an owner decision (answer a provisional-page request, publish, begin a
/// generation, discard, cancel), and the owner issues none of them twice without the decision
/// changing: a handful can be outstanding at once, so eight leaves slack.
const CONTROL_RESERVE: usize = 8;

/// Capacity of the owner-to-store-thread queue. The in-flight cap
/// ([`MAX_INFLIGHT_SCAN_BATCHES`]) bounds the batches queued, each preceded by at most one
/// notes command, and [`CONTROL_RESERVE`] covers everything else: the queue's size follows
/// those constants and nothing the scan finds.
pub(crate) const STORE_COMMAND_CAPACITY: usize =
    COMMANDS_PER_BATCH * MAX_INFLIGHT_SCAN_BATCHES + CONTROL_RESERVE;

/// Capacity of the store-thread-to-owner queue. A result answers a command, or reports a
/// generation's first admission failure: the owner has at most one provisional-page request and
/// one publication outstanding, and the store thread reports an admission failure once per
/// generation, so a few results are ever in flight. Eight leaves room for the failure notices
/// of the begin, discard, and cancel commands too. When the owner has not drained them the
/// store thread waits, noticing a stop, instead of dropping one.
pub(crate) const STORE_RESULT_CAPACITY: usize = 8;

/// How often a store thread that cannot hand over a result checks whether the session stopped.
const RESULT_SEND_POLL: Duration = Duration::from_millis(25);

/// What the owner collected about the scan since its last command, which the store thread
/// folds into the active generation. Coalesced here so that a tree full of unreadable
/// directories neither grows the queue nor sends a command per failure.
#[derive(Debug, Default)]
struct StoreNotes {
    unrecorded_paths: u64,
    unreadable_directories: BTreeSet<RelativePath>,
}

impl StoreNotes {
    fn is_empty(&self) -> bool {
        self.unrecorded_paths == 0 && self.unreadable_directories.is_empty()
    }
}

/// A decision of the owner loop, applied by the store thread in the order it was sent.
enum StoreCommand {
    Notes(StoreNotes),
    /// Admits one scanner batch. Dropping the batch, however that happens, returns its credit.
    Admit {
        lease: WorkLease,
        batch: SealedBatch,
    },
    ProvisionalPage(PageRequest),
    Publish,
    BeginGeneration(ScanGeneration),
    /// A deletion's overlay: the next generation, copied from the one the owner has installed
    /// without the removed entries, and published.
    Overlay(Box<OverlayRequest>),
    DiscardActive,
    CancelActive,
    /// Holds the thread inside a call, as a hung file system would: it tells `entered`, then
    /// does nothing until `release` is sent to or dropped.
    #[cfg(test)]
    Hold {
        entered: Sender<()>,
        release: Receiver<()>,
    },
}

/// Where a scan's folders are: the root as the scan read it, so that an overlay can read a folder
/// below it as the file system has it now.
#[derive(Clone, Debug)]
pub(crate) struct ScanRoot {
    pub(crate) path: PathBuf,
    pub(crate) identity: NativeIdentity,
}

/// A deletion's overlay, as the owner asks for it.
struct OverlayRequest {
    generation: ScanGeneration,
    replaced_prefix: RelativePath,
    /// The generation the owner has installed: the one the overlay is the successor of.
    base: OverlayBase,
    root: ScanRoot,
}

/// What the store thread reports back, in the order it happened.
enum StoreResult {
    AdmissionFailed(String),
    ProvisionalPage {
        folder: RelativePath,
        page: Result<Box<ScanPage>, ScanStoreError>,
    },
    Published(Result<Publication, ScanStoreError>),
    Overlaid(Result<Publication, ScanStoreError>),
    Failure(String),
}

/// What [`StoreHandle::poll`] reports to the owner loop.
pub(crate) enum StoreEvent {
    /// A batch could not be admitted: the generation is unusable, and later batches are dropped
    /// until the owner begins another. The message is the one the interface shows.
    AdmissionFailed(String),
    /// The live view of a folder, as of the moment the request was applied.
    ProvisionalPage {
        folder: RelativePath,
        page: Result<Box<ScanPage>, ScanStoreError>,
    },
    /// The primary scan's or a rebuild's publication ended. The owner [`installs`] a
    /// successful one, or drops it.
    ///
    /// [`installs`]: StoreHandle::install
    Published(Result<Publication, ScanStoreError>),
    /// A deletion overlay's publication ended.
    Overlaid(Result<Publication, ScanStoreError>),
    /// The store thread could not apply a command whose failure nothing else reports, or stopped
    /// unexpectedly.
    Failure(String),
}

/// The store thread's state: the store, whether this generation already failed to admit, the
/// stage counter of the publication under way, and what a deletion's overlay needs to read the
/// folder that held the removed entry: the session's stop signal and the paths that are the
/// session's own.
struct StoreEngine {
    store: ScanStore,
    admission_failed: bool,
    progress: Arc<AtomicU8>,
    quota: TemporaryStorage,
    internal_paths: Vec<PathBuf>,
}

impl StoreEngine {
    fn new(store: ScanStore, progress: Arc<AtomicU8>) -> Self {
        Self {
            quota: store.quota(),
            internal_paths: store.internal_paths(),
            store,
            admission_failed: false,
            progress,
        }
    }

    fn apply(&mut self, command: StoreCommand) -> Option<StoreResult> {
        match command {
            StoreCommand::Notes(notes) => {
                self.store.record_unrecorded_paths(notes.unrecorded_paths);
                for path in notes.unreadable_directories {
                    self.store.record_unreadable_directory(path);
                }
                None
            }
            StoreCommand::Admit { lease, batch } => self.admit(&lease, batch),
            StoreCommand::ProvisionalPage(request) => Some(StoreResult::ProvisionalPage {
                page: self.store.provisional_page(&request).map(Box::new),
                folder: request.folder,
            }),
            StoreCommand::Publish => Some(StoreResult::Published(self.publish())),
            StoreCommand::BeginGeneration(generation) => {
                self.admission_failed = false;
                self.store
                    .begin_generation(generation)
                    .err()
                    .map(|error| StoreResult::Failure(error.to_string()))
            }
            StoreCommand::Overlay(request) => {
                self.admission_failed = false;
                Some(StoreResult::Overlaid(self.overlay(*request)))
            }
            StoreCommand::DiscardActive => self
                .store
                .discard_active()
                .err()
                .map(|error| StoreResult::Failure(error.to_string())),
            StoreCommand::CancelActive => self
                .store
                .cancel_active()
                .err()
                .map(|error| StoreResult::Failure(error.to_string())),
            #[cfg(test)]
            StoreCommand::Hold { entered, release } => {
                let _ = entered.send(());
                // Returns once the test sends, or drops its end.
                let _ = release.recv();
                None
            }
        }
    }

    fn admit(&mut self, lease: &WorkLease, batch: SealedBatch) -> Option<StoreResult> {
        if self.admission_failed {
            // Dropping the batch removes its runs and returns its credit.
            return None;
        }
        for run in batch {
            if let Err(error) = self.store.accept_leased_input_run(lease, run) {
                self.admission_failed = true;
                return Some(StoreResult::AdmissionFailed(scan_store_capacity_message(
                    &error,
                )));
            }
        }
        None
    }

    fn publish(&mut self) -> Result<Publication, ScanStoreError> {
        if self.admission_failed {
            return Err(ScanStoreError::ClosedGeneration);
        }
        self.publish_reporting()?;
        self.store
            .take_publication()
            .ok_or(ScanStoreError::NoPublishedGeneration)
    }

    /// Builds and publishes the generation that follows a deletion: the one the owner has
    /// installed, without the removed entries, and with the folder that held them as it is now
    /// when that folder holds the entries the map lists for it.
    fn overlay(&mut self, request: OverlayRequest) -> Result<Publication, ScanStoreError> {
        let OverlayRequest {
            generation,
            replaced_prefix,
            base,
            root,
        } = request;
        let folder = refreshed_folder(&root, &replaced_prefix, &self.quota, &self.internal_paths)?;
        self.store.begin_overlay_generation(
            &base,
            generation,
            &replaced_prefix,
            folder.as_ref(),
        )?;
        self.publish_reporting()?;
        self.store
            .take_publication()
            .ok_or(ScanStoreError::NoPublishedGeneration)
    }

    /// Publishes the active generation, counting the stages it completes into the progress the
    /// owner loop reads.
    fn publish_reporting(&mut self) -> Result<(), ScanStoreError> {
        let Self {
            store, progress, ..
        } = self;
        progress.store(0, Ordering::Relaxed);
        store
            .publish_reporting(&mut |stage| progress.store(stage, Ordering::Relaxed))
            .map(drop)
    }
}

/// The running store thread, and the ends of its two queues the owner holds.
struct StoreThread {
    commands: Sender<StoreCommand>,
    results: Receiver<StoreResult>,
    join: WaitableThread<()>,
}

impl StoreThread {
    fn spawn(engine: StoreEngine, quota: TemporaryStorage) -> io::Result<Self> {
        let (commands, command_receiver) = bounded(STORE_COMMAND_CAPACITY);
        let (result_sender, results) = bounded(STORE_RESULT_CAPACITY);
        let join = spawn_waitable(STORE_THREAD_NAME, move || {
            run(engine, &command_receiver, &result_sender, &quota);
        })?;
        Ok(Self {
            commands,
            results,
            join,
        })
    }

    /// Ends the thread: dropping both queues unblocks it, whether it waits for a command or
    /// for the owner to take a result, and the session's stop makes a merge or publication in
    /// progress fail at its next block of I/O. A call that never returns, in a file system that
    /// hung, is beyond all of that: `wait` is what lets a forced stop leave the thread behind.
    fn join(self, wait: &mut ShutdownWait) -> Result<(), String> {
        let Self {
            commands,
            results,
            join,
        } = self;
        drop(commands);
        drop(results);
        match wait.join(join) {
            Joined::Ended(Err(_)) => Err("the scan store thread panicked".to_string()),
            Joined::Ended(Ok(())) | Joined::Abandoned => Ok(()),
        }
    }
}

/// The folder that held `removed`, as the file system has it now: the one folder whose own
/// metadata the removal changed, for the overlay to record in place of what the scan saw, and
/// the entries it holds, which the overlay checks against the map's before it records the
/// metadata. `None` when the scan's root held it, whose snapshot nothing is checked against.
///
/// The entries are summed as they are read, never kept ([`FolderDigest`]), so a folder of
/// millions of entries costs a pass over it and no memory. The scanner leaves Excise's own files
/// out of the map (the session's storage, and on Windows the spill file a deletion keeps in the
/// folder that held its target, [`crate::private_files::PrivateFiles`]), so this does too, and
/// does not even read them: a file held open with no sharing cannot be. A stopped session ends
/// the pass: shutdown waits for this thread, and a listing is as long as the folder is.
fn refreshed_folder(
    root: &ScanRoot,
    removed: &RelativePath,
    quota: &TemporaryStorage,
    internal_paths: &[PathBuf],
) -> Result<Option<RefreshedFolder>, ScanStoreError> {
    if removed.depth() < 2 {
        return Ok(None);
    }
    let path = RelativePath::from_components(removed.components()[..removed.depth() - 1].to_vec())
        .map_err(|error| ScanStoreError::OverlayNeedsRescan(format!("{error:?}")))?;
    let folder = path.to_path_buf();
    let location = root.path.join(&folder);
    let internal: Vec<&OsStr> = internal_paths
        .iter()
        .filter(|internal| internal.parent() == Some(location.as_path()))
        .filter_map(|internal| internal.file_name())
        .collect();
    let private_files = quota.private_files();
    let mut entries = FolderDigest::default();
    let snapshot = current_folder_listing(
        &root.path,
        &root.identity,
        &folder,
        |name| internal.contains(&name) || private_files.contains_in(&location, name),
        |name, kind, identity| {
            if quota.ensure_running().is_err() {
                return false;
            }
            entries.add(name, entry_kind(kind), Some(&identity.file_id));
            true
        },
    )
    .map_err(|error| {
        ScanStoreError::OverlayNeedsRescan(format!(
            "the folder that held the removed entry cannot be read as it is now: {error}"
        ))
    })?;
    Ok(Some(RefreshedFolder {
        path,
        snapshot,
        entries,
    }))
}

/// The kind the map records for an entry the planner reads as `kind`.
const fn entry_kind(kind: PlannedKind) -> PathEntryKind {
    match kind {
        PlannedKind::Directory => PathEntryKind::Directory,
        PlannedKind::File => PathEntryKind::File,
        PlannedKind::Link => PathEntryKind::Link,
    }
}

fn run(
    mut engine: StoreEngine,
    commands: &Receiver<StoreCommand>,
    results: &Sender<StoreResult>,
    quota: &TemporaryStorage,
) {
    while let Ok(command) = commands.recv() {
        if quota.ensure_running().is_err() {
            return;
        }
        if let Some(result) = engine.apply(command)
            && !send_result(results, result, quota)
        {
            return;
        }
    }
}

/// Hands one result to the owner, waiting while its queue is full. Returns whether the thread
/// should go on: false once the owner is gone or the session stopped.
fn send_result(
    results: &Sender<StoreResult>,
    mut result: StoreResult,
    quota: &TemporaryStorage,
) -> bool {
    loop {
        match results.send_timeout(result, RESULT_SEND_POLL) {
            Ok(()) => return true,
            Err(SendTimeoutError::Timeout(returned)) => {
                if quota.ensure_running().is_err() {
                    return false;
                }
                result = returned;
            }
            Err(SendTimeoutError::Disconnected(_)) => return false,
        }
    }
}

enum Backend {
    /// Commands run on the calling thread as they are sent; their results wait in a local queue.
    Inline {
        engine: Box<StoreEngine>,
        results: VecDeque<StoreResult>,
    },
    Threaded(StoreThread),
    Stopped,
}

/// The store was not reachable: its thread stopped, or a control command found its queue full.
#[derive(Debug)]
pub(crate) struct StoreUnavailable(&'static str);

impl std::fmt::Display for StoreUnavailable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for StoreUnavailable {}

/// What an unfinished request for a folder's live view still owes the owner.
#[derive(Debug, Default)]
struct ProvisionalRequests {
    in_flight: bool,
    /// Asked for while another request was in flight: sent once that one is answered.
    wanted: Option<(RelativePath, usize)>,
}

/// The owner loop's side of a session's scan store.
///
/// It answers what the owner may ask synchronously: the session, the published generation and
/// its pages, the quota's totals. It turns everything the store thread does into commands and
/// events: the owner sends commands as it makes decisions and applies the events
/// [`Self::poll`] returns, so it is never blocked by the store thread, and the store thread
/// never touches application state.
pub(crate) struct StoreHandle {
    session: ScanSessionId,
    quota: TemporaryStorage,
    internal_paths: Vec<PathBuf>,
    factory: ScanInputRunFactory,
    published: Option<PublishedGeneration>,
    summary_only: Option<SummaryOnlyGeneration>,
    /// The generation taking scanner input, as this handle's commands left it: the owner decides
    /// every change, so it knows without asking.
    accepting: Option<ScanGeneration>,
    latest_generation: Option<ScanGeneration>,
    notes: StoreNotes,
    publications_in_flight: usize,
    /// How many stages of the publication in flight have completed: written by the store thread,
    /// read by the owner loop. A counter rather than a queue of notices, so a slow owner can
    /// never make the store thread wait or grow anything.
    progress: Arc<AtomicU8>,
    provisional: ProvisionalRequests,
    unavailable_reported: bool,
    backend: Backend,
}

impl StoreHandle {
    /// Takes over `store`, which runs on the calling thread until [`Self::start_thread`].
    ///
    /// # Errors
    ///
    /// Returns an error when the store has no generation accepting scanner input.
    pub(crate) fn new(store: ScanStore) -> Result<Self, ScanStoreError> {
        let factory = store.input_run_factory()?;
        let active = store.active_generation();
        let progress = Arc::new(AtomicU8::new(0));
        Ok(Self {
            session: store.session(),
            quota: store.quota(),
            internal_paths: store.internal_paths(),
            factory,
            published: None,
            summary_only: None,
            accepting: active,
            latest_generation: active,
            notes: StoreNotes::default(),
            publications_in_flight: 0,
            progress: Arc::clone(&progress),
            provisional: ProvisionalRequests::default(),
            unavailable_reported: false,
            backend: Backend::Inline {
                engine: Box::new(StoreEngine::new(store, progress)),
                results: VecDeque::new(),
            },
        })
    }

    /// Moves the store onto its own thread. Calling it again, or after [`Self::shutdown`], does
    /// nothing.
    ///
    /// # Errors
    ///
    /// Returns an error when the operating system cannot start the thread; the store is lost
    /// with it, and the session cannot go on.
    pub(crate) fn start_thread(&mut self) -> io::Result<()> {
        let Backend::Inline { engine, results } =
            std::mem::replace(&mut self.backend, Backend::Stopped)
        else {
            return Ok(());
        };
        debug_assert!(results.is_empty(), "no result can precede the loop");
        self.backend = Backend::Threaded(StoreThread::spawn(*engine, self.quota.clone())?);
        Ok(())
    }

    /// Stops the thread, if any, and waits for it as long as it takes. The session's storage may
    /// be removed once this returns: nothing writes it afterwards.
    ///
    /// # Errors
    ///
    /// Returns an error when the store thread panicked.
    pub(crate) fn shutdown(&mut self) -> Result<(), String> {
        self.shutdown_with(&mut ShutdownWait::patient())
    }

    /// As [`Self::shutdown`], but `wait` can give up on a thread that does not end. The thread
    /// then keeps its hold on the session's storage, so nothing removes the directory until the
    /// process is gone, and the next start's sweep removes it.
    ///
    /// # Errors
    ///
    /// Returns an error when the store thread panicked.
    pub(crate) fn shutdown_with(&mut self, wait: &mut ShutdownWait) -> Result<(), String> {
        self.quota.stop();
        match std::mem::replace(&mut self.backend, Backend::Stopped) {
            Backend::Threaded(thread) => thread.join(wait),
            Backend::Inline { .. } | Backend::Stopped => Ok(()),
        }
    }

    #[must_use]
    pub(crate) const fn session(&self) -> ScanSessionId {
        self.session
    }

    #[must_use]
    pub(crate) fn storage_stats(&self) -> (u64, u64) {
        (self.quota.used(), self.quota.limit())
    }

    /// The session's storage, as the owner loop's tests share it with what else is the session's.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn quota(&self) -> TemporaryStorage {
        self.quota.clone()
    }

    #[must_use]
    pub(crate) fn internal_paths(&self) -> Vec<PathBuf> {
        self.internal_paths.clone()
    }

    /// A factory for the generation accepting scanner input, for the scanner to seal batches.
    ///
    /// # Errors
    ///
    /// Returns an error when no generation is accepting input.
    pub(crate) fn input_run_factory(&self) -> Result<ScanInputRunFactory, ScanStoreError> {
        self.accepting
            .map(|generation| self.factory.for_generation(generation))
            .ok_or(ScanStoreError::NoActiveGeneration)
    }

    #[must_use]
    pub(crate) const fn active_generation(&self) -> Option<ScanGeneration> {
        self.accepting
    }

    #[must_use]
    pub(crate) fn published_generation(&self) -> Option<ScanGeneration> {
        self.published.as_ref().map(PublishedGeneration::generation)
    }

    #[must_use]
    pub(crate) const fn published(&self) -> Option<&PublishedGeneration> {
        self.published.as_ref()
    }

    #[must_use]
    pub(crate) const fn published_mut(&mut self) -> Option<&mut PublishedGeneration> {
        self.published.as_mut()
    }

    #[must_use]
    pub(crate) const fn summary_only(&self) -> Option<&SummaryOnlyGeneration> {
        self.summary_only.as_ref()
    }

    #[must_use]
    pub(crate) const fn is_summary_only(&self) -> bool {
        self.summary_only.is_some()
    }

    /// The next generation number: one past every generation this session has begun or
    /// published.
    ///
    /// # Errors
    ///
    /// Returns an error when generation numbers are exhausted.
    pub(crate) fn next_generation(&self) -> Result<ScanGeneration, ScanStoreError> {
        self.latest_generation
            .map_or(Ok(ScanGeneration::initial()), |generation| {
                generation
                    .value()
                    .checked_add(1)
                    .map(ScanGeneration::from_value)
                    .ok_or(ScanStoreError::GenerationOverflow)
            })
    }

    /// Whether the queue has room for another scanner batch. The owner takes the next scanner
    /// event only when it does, so a store thread that falls behind backs the scanner up
    /// through the bounded event channel and the in-flight cap instead of growing a queue.
    #[must_use]
    pub(crate) fn has_room_for_batch(&self) -> bool {
        match &self.backend {
            Backend::Threaded(thread) => {
                thread.commands.len() <= COMMANDS_PER_BATCH * (MAX_INFLIGHT_SCAN_BATCHES - 1)
            }
            Backend::Inline { .. } | Backend::Stopped => true,
        }
    }

    /// Hands a sealed batch to the store thread for admission. Whatever happens to it from here,
    /// including this call failing, its credit returns when the batch is dropped.
    ///
    /// # Errors
    ///
    /// Returns an error when the store thread is gone or its queue is full.
    pub(crate) fn admit(
        &mut self,
        lease: &WorkLease,
        batch: SealedBatch,
    ) -> Result<(), StoreUnavailable> {
        self.send(StoreCommand::Admit {
            lease: lease.clone(),
            batch,
        })
    }

    /// Notes one path the scan could not represent in the active generation.
    pub(crate) fn record_unrecorded_path(&mut self) {
        self.notes.unrecorded_paths = self.notes.unrecorded_paths.saturating_add(1);
    }

    /// Notes one directory the scan could not list, in the active generation. Past the
    /// tracking cap the store thread reports every folder uncertain, so no more need be kept.
    pub(crate) fn record_unreadable_directory(&mut self, path: RelativePath) {
        if self.notes.unreadable_directories.len() <= MAX_TRACKED_UNREADABLE_DIRECTORIES {
            self.notes.unreadable_directories.insert(path);
        }
    }

    /// Asks for the live view of `folder`. At most one request is outstanding: one made while
    /// another is in flight replaces any other waiting, and goes out once that one is answered.
    ///
    /// # Errors
    ///
    /// Returns an error when the store thread is gone or its queue is full.
    pub(crate) fn request_provisional_page(
        &mut self,
        folder: &RelativePath,
        limit: usize,
    ) -> Result<(), StoreUnavailable> {
        if self.provisional.in_flight {
            self.provisional.wanted = Some((folder.clone(), limit));
            return Ok(());
        }
        self.provisional.in_flight = true;
        self.send(StoreCommand::ProvisionalPage(PageRequest::first(
            folder.clone(),
            limit,
        )))
        .inspect_err(|_| self.provisional.in_flight = false)
    }

    /// Starts publishing the active generation. The generation stops accepting input.
    ///
    /// # Errors
    ///
    /// Returns an error when the store thread is gone or its queue is full.
    pub(crate) fn begin_publication(&mut self) -> Result<(), StoreUnavailable> {
        self.send(StoreCommand::Publish)?;
        self.accepting = None;
        self.publications_in_flight += 1;
        Ok(())
    }

    /// Whether a publication has been requested and not yet reported.
    #[must_use]
    pub(crate) const fn publication_in_flight(&self) -> bool {
        self.publications_in_flight > 0
    }

    /// How far the publication in flight has got, as stages completed and stages in all, or
    /// `None` when none is in flight. The first stage count a publication shows may still be the
    /// previous publication's until the store thread begins it: the owner reads it for a status
    /// line, never for a decision.
    #[must_use]
    pub(crate) fn publication_progress(&self) -> Option<(u8, u8)> {
        self.publication_in_flight().then(|| {
            (
                self.progress.load(Ordering::Relaxed),
                ScanStore::PUBLISH_STAGES,
            )
        })
    }

    /// Starts a strictly newer generation, abandoning unsealed work; the published generation
    /// stays readable until its replacement arrives.
    ///
    /// # Errors
    ///
    /// Returns an error when the store thread is gone or its queue is full.
    pub(crate) fn begin_generation(
        &mut self,
        generation: ScanGeneration,
    ) -> Result<(), StoreUnavailable> {
        self.send(StoreCommand::BeginGeneration(generation))?;
        self.accepting = Some(generation);
        self.note_generation(generation);
        Ok(())
    }

    /// Starts building the next generation from the one the owner has installed, without
    /// `replaced_prefix`, and publishing it: a deletion's overlay. The installed generation is
    /// the base because it is the one the reader sees, and the one the files of which stay until
    /// the overlay's arrives; `root` is where to read the folder that held the removed entry.
    ///
    /// # Errors
    ///
    /// Returns an error when no map is installed, the store thread is gone, or its queue is full.
    pub(crate) fn begin_overlay(
        &mut self,
        generation: ScanGeneration,
        replaced_prefix: &RelativePath,
        root: ScanRoot,
    ) -> Result<(), StoreUnavailable> {
        let base = self
            .published
            .as_ref()
            .and_then(PublishedGeneration::overlay_base)
            .ok_or(StoreUnavailable(
                "no installed map to build the overlay from",
            ))?;
        self.send(StoreCommand::Overlay(Box::new(OverlayRequest {
            generation,
            replaced_prefix: replaced_prefix.clone(),
            base,
            root,
        })))?;
        self.accepting = None;
        self.publications_in_flight += 1;
        self.note_generation(generation);
        Ok(())
    }

    /// Marks the active generation incomplete: it cannot be published, and the published
    /// generation stays.
    ///
    /// # Errors
    ///
    /// Returns an error when the store thread is gone or its queue is full.
    pub(crate) fn discard_active(&mut self) -> Result<(), StoreUnavailable> {
        self.accepting = None;
        self.send(StoreCommand::DiscardActive)
    }

    /// Marks the active generation cancelled by the reader.
    ///
    /// # Errors
    ///
    /// Returns an error when the store thread is gone or its queue is full.
    pub(crate) fn cancel_active(&mut self) -> Result<(), StoreUnavailable> {
        self.accepting = None;
        self.send(StoreCommand::CancelActive)
    }

    /// Swaps in a generation the store thread published, replacing (and releasing) the one the
    /// owner read until now.
    pub(crate) fn install(&mut self, publication: Publication) {
        match publication {
            Publication::Published(published) => {
                self.summary_only = None;
                self.published = Some(*published);
            }
            Publication::SummaryOnly(summary) => self.summary_only = Some(*summary),
        }
    }

    /// The next thing the store thread reported, if it has reported anything since the last call.
    pub(crate) fn poll(&mut self) -> Option<StoreEvent> {
        let result = self.next_result()?;
        Some(match result {
            StoreResult::AdmissionFailed(message) => StoreEvent::AdmissionFailed(message),
            StoreResult::ProvisionalPage { folder, page } => {
                self.provisional.in_flight = false;
                if let Some((wanted, limit)) = self.provisional.wanted.take() {
                    // A failure to send is reported by the next command that needs the store.
                    let _ = self.request_provisional_page(&wanted, limit);
                }
                StoreEvent::ProvisionalPage { folder, page }
            }
            StoreResult::Published(result) => {
                self.publications_in_flight = self.publications_in_flight.saturating_sub(1);
                self.note_publication(&result);
                StoreEvent::Published(result)
            }
            StoreResult::Overlaid(result) => {
                self.publications_in_flight = self.publications_in_flight.saturating_sub(1);
                self.note_publication(&result);
                StoreEvent::Overlaid(result)
            }
            StoreResult::Failure(message) => StoreEvent::Failure(message),
        })
    }

    fn note_publication(&mut self, result: &Result<Publication, ScanStoreError>) {
        if let Ok(publication) = result {
            self.note_generation(match publication {
                Publication::Published(published) => published.generation(),
                Publication::SummaryOnly(summary) => summary.generation(),
            });
        }
    }

    fn note_generation(&mut self, generation: ScanGeneration) {
        self.latest_generation = self.latest_generation.max(Some(generation));
    }

    fn next_result(&mut self) -> Option<StoreResult> {
        match &mut self.backend {
            Backend::Inline { results, .. } => results.pop_front(),
            Backend::Threaded(thread) => match thread.results.try_recv() {
                Ok(result) => Some(result),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => {
                    (!std::mem::replace(&mut self.unavailable_reported, true))
                        .then(|| StoreResult::Failure("the scan store thread stopped".to_string()))
                }
            },
            Backend::Stopped => None,
        }
    }

    /// Sends one command, preceded by the notes collected since the last one, so the store
    /// thread sees them before whatever decision they bear on.
    fn send(&mut self, command: StoreCommand) -> Result<(), StoreUnavailable> {
        if !self.notes.is_empty() {
            let notes = std::mem::take(&mut self.notes);
            self.send_one(StoreCommand::Notes(notes))?;
        }
        self.send_one(command)
    }

    fn send_one(&mut self, command: StoreCommand) -> Result<(), StoreUnavailable> {
        match &mut self.backend {
            Backend::Inline { engine, results } => {
                results.extend(engine.apply(command));
                Ok(())
            }
            Backend::Threaded(thread) => {
                thread
                    .commands
                    .try_send(command)
                    .map_err(|error| match error {
                        TrySendError::Full(_) => StoreUnavailable("the scan store queue is full"),
                        TrySendError::Disconnected(_) => {
                            StoreUnavailable("the scan store thread stopped")
                        }
                    })
            }
            Backend::Stopped => Err(StoreUnavailable("the scan store was shut down")),
        }
    }

    /// Runs `visit` on the store of an inline handle, for tests that build a generation by hand.
    #[cfg(test)]
    pub(crate) fn with_inline_store<R>(&mut self, visit: impl FnOnce(&mut ScanStore) -> R) -> R {
        match &mut self.backend {
            Backend::Inline { engine, .. } => visit(&mut engine.store),
            Backend::Threaded(_) | Backend::Stopped => {
                panic!("only a handle that has not started its thread exposes its store")
            }
        }
    }

    /// Holds the store thread inside a call, as a hung file system would, and returns once it is
    /// in it. The thread goes on when the returned guard is dropped.
    #[cfg(test)]
    pub(crate) fn hold_for_test(&mut self) -> HeldStoreThread {
        let (entered, entered_receiver) = bounded(1);
        let (release, release_receiver) = bounded(0);
        self.send_one(StoreCommand::Hold {
            entered,
            release: release_receiver,
        })
        .expect("the store thread should take the hold");
        entered_receiver
            .recv_timeout(Duration::from_secs(10))
            .expect("the store thread should enter the hold");
        HeldStoreThread {
            release: Some(release),
        }
    }
}

/// A store thread held inside a call by [`StoreHandle::hold_for_test`]: dropping this lets it go.
#[cfg(test)]
pub(crate) struct HeldStoreThread {
    release: Option<Sender<()>>,
}

#[cfg(test)]
impl Drop for HeldStoreThread {
    fn drop(&mut self) {
        drop(self.release.take());
    }
}

impl Drop for StoreHandle {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::thread;
    use std::time::Instant;

    use super::*;
    use crate::model::ByteBounds;
    use crate::scan_coordinator::{ScanCoordinator, WorkKey, WorkKind, WorkPriority};
    use crate::scan_store::page::PageRequest;
    use crate::scan_store::path_reducer::{
        Coverage, PathEntryKind, PathObservation, SummaryMetrics, UnreadableDirectories,
    };
    use crate::scan_store::storage::ScanStoreStorage;

    const STORAGE_BYTES: u64 = 8 * 1024 * 1024;

    fn path(value: &str) -> RelativePath {
        RelativePath::from_path(Path::new(value)).expect("fixture path should be relative")
    }

    fn handle() -> StoreHandle {
        let store = ScanStore::new(
            ScanGeneration::initial(),
            TemporaryStorage::with_limit_bytes(STORAGE_BYTES),
        )
        .expect("store should initialize");
        StoreHandle::new(store).expect("handle should take the store")
    }

    /// A batch of one file, sealed the way the scanner seals one.
    fn batch(handle: &StoreHandle, name: &str) -> SealedBatch {
        handle
            .input_run_factory()
            .expect("the generation should accept input")
            .seal_observation_batch(
                vec![PathObservation::new(
                    path(name),
                    PathEntryKind::File,
                    SummaryMetrics::leaf(1, ByteBounds::exact(0), ByteBounds::exact(0)),
                    Coverage::Complete,
                )],
                Vec::new(),
            )
            .expect("the batch should seal")
    }

    /// A lease to enumerate the root in `generation` of the handle's session.
    fn lease(handle: &StoreHandle, generation: ScanGeneration) -> WorkLease {
        let session = handle.session();
        let mut coordinator = ScanCoordinator::new(session, generation);
        coordinator.schedule(
            WorkKey::new(
                session,
                generation,
                WorkKind::EnumerateDirectory,
                RelativePath::root(),
            ),
            WorkPriority::Background,
        );
        coordinator
            .lease_next()
            .expect("scheduled work should be leased")
    }

    fn active_lease(handle: &StoreHandle) -> WorkLease {
        let generation = handle
            .active_generation()
            .expect("a generation should be accepting input");
        lease(handle, generation)
    }

    /// Waits, bounded, until `condition` holds.
    fn wait_until(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !condition() {
            assert!(
                Instant::now() < deadline,
                "the store thread should get there"
            );
            thread::sleep(Duration::from_millis(1));
        }
    }

    /// Polls, bounded, until `wanted` takes an event.
    fn wait_for_event<T>(
        handle: &mut StoreHandle,
        mut wanted: impl FnMut(StoreEvent) -> Option<T>,
    ) -> T {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(event) = handle.poll() {
                if let Some(found) = wanted(event) {
                    return found;
                }
                continue;
            }
            assert!(Instant::now() < deadline, "the store thread should answer");
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn queued_commands(handle: &StoreHandle) -> usize {
        match &handle.backend {
            Backend::Threaded(thread) => thread.commands.len(),
            Backend::Inline { .. } | Backend::Stopped => 0,
        }
    }

    fn queued_results(handle: &StoreHandle) -> usize {
        match &handle.backend {
            Backend::Threaded(thread) => thread.results.len(),
            Backend::Inline { results, .. } => results.len(),
            Backend::Stopped => 0,
        }
    }

    #[test]
    fn batches_are_admitted_in_order_before_the_publication_that_follows_them() {
        let mut handle = handle();
        handle.start_thread().expect("the thread should start");
        let factory = handle.input_run_factory().expect("factory");
        let lease = active_lease(&handle);
        for name in ["a", "b"] {
            let sealed = batch(&handle, name);
            handle.admit(&lease, sealed).expect("the queue has room");
        }
        handle.begin_publication().expect("the queue has room");

        let publication = wait_for_event(&mut handle, |event| match event {
            StoreEvent::Published(result) => Some(result),
            StoreEvent::AdmissionFailed(message) => panic!("admission failed: {message}"),
            other => panic!("unexpected event {:?}", std::mem::discriminant(&other)),
        })
        .expect("the scan should publish");
        handle.install(publication);

        let page = handle
            .published_mut()
            .expect("the generation should be installed")
            .page(PageRequest::first(RelativePath::root(), 16))
            .expect("the root page should load");
        assert_eq!(
            page.entries.len(),
            2,
            "both batches were admitted before the publication ran"
        );
        assert_eq!(
            factory.in_flight_batches(),
            0,
            "each batch returned its credit once the thread was done with it"
        );
        assert!(!handle.publication_in_flight());
        handle.shutdown().expect("the thread should stop");
    }

    #[test]
    fn a_batch_the_store_cannot_take_returns_its_credit_with_the_failed_send() {
        let mut handle = handle();
        handle.start_thread().expect("the thread should start");
        let factory = handle.input_run_factory().expect("factory");
        let lease = active_lease(&handle);
        let sealed = batch(&handle, "a");
        assert_eq!(factory.in_flight_batches(), 1, "sealing took the credit");
        handle.shutdown().expect("the thread should stop");

        assert!(
            handle.admit(&lease, sealed).is_err(),
            "a stopped store takes nothing"
        );

        assert_eq!(
            factory.in_flight_batches(),
            0,
            "the batch that never reached the store gave its credit back"
        );
    }

    #[test]
    fn a_refused_batch_is_reported_once_and_the_scan_is_never_published_without_it() {
        let mut handle = handle();
        handle.start_thread().expect("the thread should start");
        let factory = handle.input_run_factory().expect("factory");
        let generation = handle
            .active_generation()
            .expect("a generation should be accepting input");
        // A lease for another generation: the store refuses every run admitted under it.
        let stale = lease(&handle, ScanGeneration::from_value(generation.value() + 1));
        for name in ["a", "b", "c"] {
            let sealed = batch(&handle, name);
            handle.admit(&stale, sealed).expect("the queue has room");
        }

        let message = wait_for_event(&mut handle, |event| match event {
            StoreEvent::AdmissionFailed(message) => Some(message),
            _ => None,
        });
        assert!(!message.is_empty(), "the reader is told why");
        wait_until(|| factory.in_flight_batches() == 0);
        assert!(
            handle.poll().is_none(),
            "the generation's failure is reported once, however many batches follow it"
        );

        handle.begin_publication().expect("the queue has room");
        let result = wait_for_event(&mut handle, |event| match event {
            StoreEvent::Published(result) => Some(result),
            _ => None,
        });
        assert!(
            result.is_err(),
            "a generation that lost a batch is never published as the whole scan"
        );
        handle.shutdown().expect("the thread should stop");
    }

    #[test]
    fn a_stalled_store_thread_backs_the_owner_up_instead_of_growing_a_queue() {
        let mut handle = handle();
        handle.start_thread().expect("the thread should start");
        let lease = active_lease(&handle);
        // Nobody collects results, so the thread ends up waiting to hand over the ninth.
        for _ in 0..=STORE_RESULT_CAPACITY {
            handle.begin_publication().expect("the queue has room");
        }
        wait_until(|| {
            queued_results(&handle) == STORE_RESULT_CAPACITY && queued_commands(&handle) == 0
        });
        assert!(handle.has_room_for_batch());

        let mut queued = 0;
        while handle.has_room_for_batch() {
            handle
                .admit(&lease, SealedBatch::empty())
                .expect("the gate leaves room for the batch it lets through");
            queued += 1;
            assert!(
                queued <= STORE_COMMAND_CAPACITY,
                "the gate closes before the queue fills"
            );
        }
        assert_eq!(queued_commands(&handle), queued);
        assert!(
            STORE_COMMAND_CAPACITY - queued >= CONTROL_RESERVE,
            "a closed gate leaves the control commands their reserve"
        );

        while handle.begin_publication().is_ok() {
            assert!(queued_commands(&handle) <= STORE_COMMAND_CAPACITY);
        }
        assert_eq!(
            queued_commands(&handle),
            STORE_COMMAND_CAPACITY,
            "the queue holds exactly its capacity, and the owner is told instead of waiting"
        );

        wait_until(|| {
            while handle.poll().is_some() {}
            queued_commands(&handle) == 0 && handle.has_room_for_batch()
        });
        handle.shutdown().expect("the thread should stop");
    }

    #[test]
    fn a_second_live_view_request_waits_for_the_first_and_a_third_replaces_it() {
        let mut handle = handle();
        for folder in ["a", "b", "c"] {
            handle
                .request_provisional_page(&path(folder), 8)
                .expect("the request is accepted");
        }

        let answered = std::iter::from_fn(|| handle.poll())
            .map(|event| match event {
                StoreEvent::ProvisionalPage { folder, .. } => folder,
                _ => panic!("only live views were asked for"),
            })
            .collect::<Vec<_>>();

        assert_eq!(
            answered,
            vec![path("a"), path("c")],
            "one request is outstanding at a time, and the one that waited was replaced"
        );
    }

    #[test]
    fn what_the_owner_noted_reaches_the_store_before_the_publication_it_precedes() {
        let mut handle = handle();
        handle.record_unrecorded_path();
        handle.record_unrecorded_path();
        handle.record_unreadable_directory(path("locked"));
        handle.begin_publication().expect("the command is accepted");

        let Some(StoreEvent::Published(Ok(publication))) = handle.poll() else {
            panic!("the scan should publish");
        };
        handle.install(publication);

        let published = handle
            .published()
            .expect("the generation should be installed");
        assert_eq!(published.unrecorded_path_count(), 2);
        assert!(matches!(
            published.unreadable_directories(),
            UnreadableDirectories::Tracked(directories) if directories.contains(&path("locked"))
        ));
    }

    #[test]
    fn stopping_the_store_thread_leaves_no_session_directory_behind() {
        for explicit in [true, false] {
            let parent = tempfile::tempdir().expect("session parent should exist");
            let storage = ScanStoreStorage::new(
                TemporaryStorage::scan_store_with_limit_bytes(STORAGE_BYTES),
                Some(parent.path()),
            )
            .expect("private scan session should initialize");
            let store = ScanStore::new_with_storage(ScanGeneration::initial(), storage)
                .expect("store should initialize");
            let mut handle = StoreHandle::new(store).expect("handle should take the store");
            handle.start_thread().expect("the thread should start");
            let lease = active_lease(&handle);
            for name in ["a", "b", "c"] {
                let sealed = batch(&handle, name);
                handle.admit(&lease, sealed).expect("the queue has room");
            }
            handle.begin_publication().expect("the queue has room");
            assert_eq!(
                std::fs::read_dir(parent.path())
                    .expect("parent should be readable")
                    .count(),
                1,
                "the session directory exists while the thread runs"
            );

            // Stop at once, with the publication possibly under way.
            if explicit {
                handle.shutdown().expect("the thread should stop");
                assert!(
                    handle.begin_publication().is_err(),
                    "a stopped store takes no more commands"
                );
            }
            drop(handle);

            assert_eq!(
                std::fs::read_dir(parent.path())
                    .expect("parent should be readable")
                    .count(),
                0,
                "the thread has stopped using the session, so nothing of it is left"
            );
        }
    }

    #[test]
    fn a_thread_that_stops_unexpectedly_is_reported_once_and_refuses_more() {
        let mut handle = handle();
        handle.start_thread().expect("the thread should start");
        let lease = active_lease(&handle);
        // The session's stop ends the thread at its next command, as a fatal error would.
        handle.quota.stop();
        handle
            .admit(&lease, SealedBatch::empty())
            .expect("the queue has room");

        let message = wait_for_event(&mut handle, |event| match event {
            StoreEvent::Failure(message) => Some(message),
            _ => None,
        });
        assert_eq!(message, "the scan store thread stopped");
        assert!(handle.poll().is_none(), "the stop is reported once");
        assert!(
            handle.begin_publication().is_err(),
            "a thread that is gone takes no more commands"
        );
    }

    /// What the overlay compares with the map is what the scan would have recorded for the
    /// folder: the same digest as the scan's own records of its entries, and without the
    /// session's storage, which the scanner leaves out of the map.
    #[cfg(unix)]
    #[test]
    fn the_live_listing_of_a_folder_is_what_the_scan_records_of_it_without_the_sessions_storage() {
        use std::os::unix::fs::MetadataExt as _;

        let root = tempfile::tempdir().expect("root should exist");
        let folder = root.path().join("holder");
        std::fs::create_dir(&folder).expect("folder should exist");
        std::fs::write(folder.join("file"), b"payload").expect("file should exist");
        std::fs::create_dir(folder.join("sub")).expect("subfolder should exist");
        std::fs::create_dir(folder.join("session")).expect("session storage should exist");
        let scan_root = ScanRoot {
            path: root.path().to_path_buf(),
            identity: crate::deletion::current_scan_root_identity(root.path())
                .expect("the root should have an identity"),
        };
        let quota = TemporaryStorage::with_limit_bytes(1024);
        let recorded = |kind_of: &[(&str, PathEntryKind)]| {
            let mut digest = FolderDigest::default();
            for (name, kind) in kind_of {
                let metadata = std::fs::symlink_metadata(folder.join(name))
                    .expect("entry should have metadata");
                digest.add(
                    OsStr::new(name),
                    *kind,
                    Some(&file_id::FileId::new_inode(metadata.dev(), metadata.ino())),
                );
            }
            digest
        };

        let refreshed = refreshed_folder(
            &scan_root,
            &path("holder/removed"),
            &quota,
            &[folder.join("session")],
        )
        .expect("the folder should be read")
        .expect("a folder below the root is refreshed");
        assert_eq!(
            refreshed.entries,
            recorded(&[
                ("file", PathEntryKind::File),
                ("sub", PathEntryKind::Directory)
            ])
        );

        let without_exclusion = refreshed_folder(&scan_root, &path("holder/removed"), &quota, &[])
            .expect("the folder should be read")
            .expect("a folder below the root is refreshed");
        assert_eq!(
            without_exclusion.entries,
            recorded(&[
                ("file", PathEntryKind::File),
                ("session", PathEntryKind::Directory),
                ("sub", PathEntryKind::Directory)
            ])
        );
        assert!(
            refreshed_folder(&scan_root, &path("removed"), &quota, &[])
                .expect("nothing is read")
                .is_none(),
            "the root's own snapshot is checked against nothing"
        );
    }

    /// Shutdown waits for this thread, and a listing is as long as the folder is: a session that
    /// was stopped ends the pass.
    #[test]
    fn a_stopped_session_ends_the_listing_of_a_folder() {
        let root = tempfile::tempdir().expect("root should exist");
        std::fs::create_dir(root.path().join("holder")).expect("folder should exist");
        std::fs::write(root.path().join("holder/file"), b"payload").expect("file should exist");
        let scan_root = ScanRoot {
            path: root.path().to_path_buf(),
            identity: crate::deletion::current_scan_root_identity(root.path())
                .expect("the root should have an identity"),
        };
        let quota = TemporaryStorage::with_limit_bytes(1024);
        quota.stop();

        let result = refreshed_folder(&scan_root, &path("holder/removed"), &quota, &[]);

        assert!(
            matches!(result, Err(ScanStoreError::OverlayNeedsRescan(_))),
            "{:?}",
            result.map(|folder| folder.map(|folder| folder.path))
        );
    }

    /// A file the session keeps open in the folder, which a deletion's spill is on Windows, is
    /// Excise's own: the scan leaves it out of the map, so the listing the overlay is checked
    /// against leaves it out too, by its exact path. A file of the same shape that the session did
    /// not register is the user's, and counts.
    #[test]
    fn a_registered_file_is_left_out_of_the_listing_and_one_of_its_shape_is_not() {
        let root = tempfile::tempdir().expect("root should exist");
        let folder = root.path().join("holder");
        std::fs::create_dir(&folder).expect("folder should exist");
        std::fs::write(folder.join("keep"), b"payload").expect("kept file should exist");
        let ours = folder.join(".excise-deletion-spill-0001");
        let users = folder.join(".excise-deletion-spill-0002");
        std::fs::write(&ours, b"ours").expect("the session's file should exist");
        std::fs::write(&users, b"a user's file").expect("the user's file should exist");
        let scan_root = ScanRoot {
            path: root.path().to_path_buf(),
            identity: crate::deletion::current_scan_root_identity(root.path())
                .expect("the root should have an identity"),
        };
        let quota = TemporaryStorage::with_limit_bytes(1024);
        let listed = |quota: &TemporaryStorage| {
            refreshed_folder(&scan_root, &path("holder/removed"), quota, &[])
                .expect("the folder should be read")
                .expect("a folder below the root is refreshed")
                .entries
        };

        let registration = quota.private_files().register(ours.clone());
        let with_the_registration = listed(&quota);
        drop(registration);
        std::fs::remove_file(&ours).expect("the session's file should be removed");
        let without_the_file = listed(&quota);
        std::fs::remove_file(&users).expect("the user's file should be removed");
        let without_the_users = listed(&quota);

        assert_eq!(
            with_the_registration, without_the_file,
            "the registered file was left out of the listing"
        );
        assert_ne!(
            without_the_file, without_the_users,
            "a file of the same shape that nobody registered is the user's, and is listed"
        );
    }

    /// The spill file is held open with no sharing: nothing else can read it, and the listing must
    /// not try. It is skipped by its exact path before it is read.
    #[cfg(windows)]
    #[test]
    fn a_spill_file_held_open_with_no_sharing_does_not_stop_the_listing_of_its_folder() {
        let root = tempfile::tempdir().expect("root should exist");
        let folder = root.path().join("holder");
        std::fs::create_dir(&folder).expect("folder should exist");
        std::fs::write(folder.join("keep"), b"payload").expect("kept file should exist");
        let scan_root = ScanRoot {
            path: root.path().to_path_buf(),
            identity: crate::deletion::current_scan_root_identity(root.path())
                .expect("the root should have an identity"),
        };
        let quota = TemporaryStorage::with_limit_bytes(1024);
        let before = refreshed_folder(&scan_root, &path("holder/removed"), &quota, &[])
            .expect("the folder should be read")
            .expect("a folder below the root is refreshed");
        let spill =
            crate::os::windows::create_private_temporary_file(&folder, quota.private_files())
                .expect("the spill file should be created");

        let during = refreshed_folder(&scan_root, &path("holder/removed"), &quota, &[])
            .expect("a file held open with no sharing should not stop the listing")
            .expect("a folder below the root is refreshed");

        assert_eq!(
            during.entries, before.entries,
            "the spill file is Excise's own, and is not an entry of the folder"
        );
        drop(spill);
    }
}
