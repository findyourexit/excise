use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use crossbeam_channel::{
    Receiver, RecvTimeoutError, SendTimeoutError, Sender, TryRecvError, TrySendError, bounded,
};

use super::scanner::{self, EntryMetadata, ScannerHandle, ScannerOptions, ScannerRequestError};
use crate::deletion::{
    DeletionPlan, DeletionPlanError, DeletionReport,
    build_plan_cancellable_with_root_identity_and_temporary_storage,
    build_plan_cancellable_with_temporary_storage, execute_plan_counted,
    revalidate_plan_cancellable,
};
use crate::error::AppError;
#[cfg(test)]
use crate::native_path::DECEPTIVE_DISPLAY_MARKER;
#[cfg(all(test, unix))]
use crate::native_path::safe_display_path_text;
use crate::native_path::{NativeIdentity, safe_display_text};
use crate::report::write_deletion_history_json;
#[cfg(test)]
use crate::scan_coordinator::ScanGeneration;
use crate::scan_coordinator::{
    CompletionOutcome, RelativePath, SchedulerSnapshot, SessionCoordinator, WorkCompletion,
    WorkKind, WorkLease, WorkPriority,
};
use crate::scan_session::ScanSessionId;
use crate::scan_store::session::SealedBatch;
use crate::signals::{Joined, ShutdownWait};
use crate::state::deletion_work::{
    DeletionExecutionProgress, DeletionWorkCommand, DeletionWorkId, MAX_DELETION_WORK_ITEMS,
};
use crate::temporary_storage::TemporaryStorage;

const CHANNEL_RETRY: Duration = Duration::from_millis(25);

fn clear_active_deletion_progress(
    active: &Mutex<Option<Arc<DeletionExecutionProgress>>>,
    progress: &Arc<DeletionExecutionProgress>,
) {
    let mut active = active
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if active
        .as_ref()
        .is_some_and(|current| Arc::ptr_eq(current, progress))
    {
        *active = None;
    }
}

pub struct ScannedEntry {
    pub metadata: EntryMetadata,
    pub path: PathBuf,
    pub identity: NativeIdentity,
}

pub(super) enum WorkerEvent {
    ScanBatch {
        lease: Option<WorkLease>,
        entries: Vec<ScannedEntry>,
        input_runs: SealedBatch,
    },
    ScanUnscanned {
        lease: Option<WorkLease>,
        path: PathBuf,
        reason: crate::model::UnscannedReason,
        input_runs: SealedBatch,
    },
    ScanFailed {
        path: Option<PathBuf>,
        message: String,
    },
    ScanFinished {
        cancelled: bool,
    },
    DeletionPlanned {
        work_id: DeletionWorkId,
        result: Result<Box<DeletionPlan>, DeletionPlanError>,
    },
    DeletionExecutionRejected {
        work_id: DeletionWorkId,
        error: DeletionPlanError,
    },
    DeletionFinished {
        work_id: DeletionWorkId,
        report: DeletionReport,
    },
}

enum PlannerCommand {
    Plan {
        work_id: DeletionWorkId,
        target: Box<crate::state::FileToDelete>,
        reduced_guardrails: bool,
        maximum_bytes: usize,
    },
}

enum ExecutorCommand {
    Execute {
        work_id: DeletionWorkId,
        plan: Box<DeletionPlan>,
        progress: Arc<DeletionExecutionProgress>,
    },
    /// Holds the thread inside a job that does not return, as a deletion entry stuck in a hung
    /// file system would: it tells `entered`, then does nothing until `release` is sent to or
    /// dropped.
    #[cfg(test)]
    Hold {
        entered: Sender<()>,
        release: Receiver<()>,
    },
}

pub(crate) enum DeletionWorkSubmissionError {
    Busy(Box<DeletionWorkCommand>),
    Disconnected,
}

/// What a deletion-history export reports when it ends.
#[derive(Debug)]
pub(crate) struct HistoryExportOutcome {
    /// How many of the history's reports it covered: the first that many, which the owner drops
    /// once they are written, keeping any added since.
    pub(crate) exported: usize,
    /// The file written, or why none was.
    pub(crate) result: Result<PathBuf, String>,
}

/// Why an export could not start.
#[derive(Debug)]
pub(crate) enum HistoryExportError {
    /// An export is already running: there is room for one.
    Busy,
    /// The operating system could not start the export's thread.
    Spawn(io::Error),
}

/// One deletion-history export, running on a thread of its own.
struct HistoryExport {
    /// Holds the export's one outcome.
    outcome: Receiver<HistoryExportOutcome>,
    join: thread::JoinHandle<()>,
}

/// The planner can build one non-mutating identity plan while the executor owns
/// the only revalidate-and-mutate lane. All command and event queues are bounded.
pub struct WorkerPool {
    events: Receiver<WorkerEvent>,
    planner_commands: Sender<PlannerCommand>,
    executor_commands: Sender<ExecutorCommand>,
    cancelled: Arc<AtomicBool>,
    deletion_plan_cancelled: Arc<AtomicBool>,
    deletion_soft_cancelled: Arc<AtomicBool>,
    active_deletion_progress: Arc<Mutex<Option<Arc<DeletionExecutionProgress>>>>,
    scanner: ScannerHandle,
    scan_session: ScanSessionId,
    coordinator: SessionCoordinator,
    generation_rebuild_lease: Mutex<Option<WorkLease>>,
    history_export: Mutex<Option<HistoryExport>>,
    scanner_handle: thread::JoinHandle<()>,
    planner_handle: thread::JoinHandle<()>,
    executor_handle: thread::JoinHandle<()>,
}

impl WorkerPool {
    #[cfg(test)]
    #[allow(
        clippy::too_many_lines,
        reason = "each worker has an explicit startup and cleanup path to preserve bounded ownership"
    )]
    pub fn start(scanner_options: ScannerOptions, event_capacity: usize) -> Result<Self, AppError> {
        Self::start_with_deletion_storage(
            scanner_options,
            TemporaryStorage::default(),
            event_capacity,
        )
    }

    #[allow(
        clippy::too_many_lines,
        reason = "each worker startup and failure path retains explicit bounded ownership"
    )]
    pub(crate) fn start_with_deletion_storage(
        scanner_options: ScannerOptions,
        deletion_storage: TemporaryStorage,
        event_capacity: usize,
    ) -> Result<Self, AppError> {
        let (event_sender, events) = bounded(event_capacity);
        let (planner_commands, planner_receiver) = bounded(MAX_DELETION_WORK_ITEMS);
        let (executor_commands, executor_receiver) = bounded(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        let deletion_plan_cancelled = Arc::new(AtomicBool::new(false));
        let deletion_soft_cancelled = Arc::new(AtomicBool::new(false));
        let active_deletion_progress = Arc::new(Mutex::new(None));
        let scan_root = scanner_options.root.clone();
        let scan_root_identity = scanner_options.root_identity.clone();
        // A file the deletion keeps open in the user's tree is one the scanner must leave out: the
        // two are one session's, so they share the registry of such files.
        let temporary_storage =
            deletion_storage.sharing_private_files_with(&scanner_options.temporary_storage);
        let scan_session = scanner_options.session;
        let coordinator = scanner_options.coordinator.clone();

        let (scanner_handle, scanner) = scanner::spawn(
            scanner_options,
            event_sender.clone(),
            Arc::clone(&cancelled),
        )
        .map_err(|error| AppError::io("could not spawn scanner worker", error))?;
        let planner = match thread::Builder::new()
            .name("excise-deletion-planner".to_string())
            .spawn({
                let sender = event_sender.clone();
                let cancelled = Arc::clone(&cancelled);
                let plan_cancelled = Arc::clone(&deletion_plan_cancelled);
                let root = scan_root.clone();
                let root_identity = scan_root_identity.clone();
                let temporary_storage = temporary_storage.clone();
                move || {
                    planning_worker(
                        &root,
                        root_identity.as_ref(),
                        &temporary_storage,
                        &planner_receiver,
                        &sender,
                        &plan_cancelled,
                        &cancelled,
                    );
                }
            }) {
            Ok(handle) => handle,
            Err(error) => {
                cancelled.store(true, Ordering::Release);
                drop(events);
                let _ = scanner_handle.join();
                return Err(AppError::io("could not spawn deletion planner", error));
            }
        };
        let executor = match thread::Builder::new()
            .name("excise-deletion-executor".to_string())
            .spawn({
                let sender = event_sender;
                let cancelled = Arc::clone(&cancelled);
                let soft_cancelled = Arc::clone(&deletion_soft_cancelled);
                let active_progress = Arc::clone(&active_deletion_progress);
                let root = scan_root;
                move || {
                    execution_worker(
                        &root,
                        &executor_receiver,
                        &sender,
                        &soft_cancelled,
                        &active_progress,
                        &cancelled,
                    );
                }
            }) {
            Ok(handle) => handle,
            Err(error) => {
                cancelled.store(true, Ordering::Release);
                drop(events);
                let _ = scanner_handle.join();
                let _ = planner.join();
                return Err(AppError::io("could not spawn deletion executor", error));
            }
        };

        Ok(Self {
            events,
            planner_commands,
            executor_commands,
            cancelled,
            deletion_plan_cancelled,
            deletion_soft_cancelled,
            active_deletion_progress,
            scanner,
            scan_session,
            coordinator,
            generation_rebuild_lease: Mutex::new(None),
            history_export: Mutex::new(None),
            scanner_handle,
            planner_handle: planner,
            executor_handle: executor,
        })
    }

    #[must_use]
    pub const fn events(&self) -> &Receiver<WorkerEvent> {
        &self.events
    }

    /// Prioritizes a visible directory through the bounded scheduler control channel.
    pub fn prioritize_scan(&self, path: &Path) {
        self.scanner.prioritize(path);
    }

    /// Returns the latest combined session scheduler state without consuming a worker event.
    #[must_use]
    pub(crate) fn scheduler_snapshot(&self) -> Option<SchedulerSnapshot> {
        self.coordinator.snapshot().ok()
    }

    pub(crate) fn submit_deletion_work(
        &self,
        command: DeletionWorkCommand,
    ) -> Result<(), DeletionWorkSubmissionError> {
        match command {
            DeletionWorkCommand::Plan {
                work_id,
                target,
                reduced_guardrails,
                maximum_bytes,
            } => {
                self.deletion_plan_cancelled.store(false, Ordering::Release);
                match self.planner_commands.try_send(PlannerCommand::Plan {
                    work_id,
                    target,
                    reduced_guardrails,
                    maximum_bytes,
                }) {
                    Ok(()) => Ok(()),
                    Err(TrySendError::Full(PlannerCommand::Plan {
                        work_id,
                        target,
                        reduced_guardrails,
                        maximum_bytes,
                    })) => Err(DeletionWorkSubmissionError::Busy(Box::new(
                        DeletionWorkCommand::Plan {
                            work_id,
                            target,
                            reduced_guardrails,
                            maximum_bytes,
                        },
                    ))),
                    Err(TrySendError::Disconnected(_)) => {
                        Err(DeletionWorkSubmissionError::Disconnected)
                    }
                }
            }
            DeletionWorkCommand::Execute {
                work_id,
                plan,
                progress,
            } => {
                let active_progress = Arc::clone(&progress);
                *self
                    .active_deletion_progress
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    Some(Arc::clone(&active_progress));
                self.deletion_soft_cancelled.store(false, Ordering::Release);
                match self.executor_commands.try_send(ExecutorCommand::Execute {
                    work_id,
                    plan,
                    progress,
                }) {
                    Ok(()) => Ok(()),
                    Err(TrySendError::Full(ExecutorCommand::Execute {
                        work_id,
                        plan,
                        progress,
                    })) => {
                        clear_active_deletion_progress(
                            &self.active_deletion_progress,
                            &active_progress,
                        );
                        Err(DeletionWorkSubmissionError::Busy(Box::new(
                            DeletionWorkCommand::Execute {
                                work_id,
                                plan,
                                progress,
                            },
                        )))
                    }
                    #[cfg(test)]
                    Err(TrySendError::Full(ExecutorCommand::Hold { .. })) => {
                        unreachable!("only an execution is submitted here")
                    }
                    Err(TrySendError::Disconnected(_)) => {
                        clear_active_deletion_progress(
                            &self.active_deletion_progress,
                            &active_progress,
                        );
                        Err(DeletionWorkSubmissionError::Disconnected)
                    }
                }
            }
        }
    }

    pub(crate) fn acquire_reducer(&self) -> Result<WorkLease, AppError> {
        self.coordinator
            .acquire(
                WorkKind::ReduceRun,
                RelativePath::root(),
                WorkPriority::Reducer,
            )
            .map_err(|error| AppError::Worker(error.to_string()))?
            .ok_or_else(|| AppError::Invariant("scan reduction work was not admitted".to_string()))
    }

    pub(crate) fn finish_coordinated_work(
        &self,
        lease: WorkLease,
        completion: WorkCompletion,
    ) -> Result<(), AppError> {
        if self
            .coordinator
            .finish(lease, completion)
            .map_err(|error| AppError::Worker(error.to_string()))?
            != CompletionOutcome::Accepted
        {
            return Err(AppError::Invariant(
                "session coordinator rejected completed work".to_string(),
            ));
        }
        Ok(())
    }

    /// Stops at a claim or entry boundary. The executor is always joined by shutdown.
    pub fn safely_stop_deletion(&self) {
        self.deletion_soft_cancelled.store(true, Ordering::Release);
        let progress = self
            .active_deletion_progress
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(Arc::clone);
        if let Some(progress) = progress {
            let _ = progress.request_stop();
        }
    }

    #[cfg(test)]
    pub(crate) fn deletion_soft_cancelled_for_test(&self) -> bool {
        self.deletion_soft_cancelled.load(Ordering::Acquire)
    }

    pub fn cancel_deletion_plan(&self) {
        self.deletion_plan_cancelled.store(true, Ordering::Release);
    }

    pub fn request_generation_rebuild(&self, options: ScannerOptions) -> Result<(), AppError> {
        self.request_generation_rebuild_inner(options, false)
    }

    #[cfg(feature = "internal")]
    pub(crate) fn request_pre_cancelled_generation_rebuild(
        &self,
        options: ScannerOptions,
    ) -> Result<(), AppError> {
        self.request_generation_rebuild_inner(options, true)
    }

    fn request_generation_rebuild_inner(
        &self,
        mut options: ScannerOptions,
        pre_cancelled: bool,
    ) -> Result<(), AppError> {
        options.session = self.scan_session;
        let coordinator = options.coordinator.clone();
        let lease = coordinator
            .acquire(
                WorkKind::RefreshSubtree,
                RelativePath::root(),
                WorkPriority::Foreground,
            )
            .map_err(|error| AppError::Worker(error.to_string()))?
            .ok_or_else(|| {
                AppError::Invariant("generation refresh was not admitted".to_string())
            })?;
        let request = if pre_cancelled {
            self.scanner.request_pre_cancelled_rebuild(options)
        } else {
            self.scanner.request_rebuild(options)
        };
        match request {
            Ok(()) => {
                *self
                    .generation_rebuild_lease
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(lease);
                Ok(())
            }
            Err(error) => {
                let _ = coordinator.requeue(lease);
                Err(match error {
                    ScannerRequestError::Busy => {
                        AppError::Invariant("scan rebuild queue is full".to_string())
                    }
                    ScannerRequestError::Disconnected => {
                        AppError::Worker("scanner worker disconnected".to_string())
                    }
                })
            }
        }
    }

    pub fn cancel_generation_rebuild(&self) {
        self.scanner.cancel_rebuild();
    }

    pub fn finish_generation_rebuild(&self, completion: WorkCompletion) -> Result<(), AppError> {
        self.scanner.complete_rebuild();
        let lease = self
            .generation_rebuild_lease
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .ok_or_else(|| {
                AppError::Invariant("generation rebuild had no work lease".to_string())
            })?;
        self.finish_coordinated_work(lease, completion)
    }

    /// Exports `reports` into `directory` on a thread of its own (`excise-history-export`) and
    /// reports the outcome to [`Self::poll_history_export`]. Serializing a long history takes
    /// longer than the interface may go without a frame, so it never runs on the owner loop. One
    /// export runs at a time: its outcome queue holds one result, and the thread exits once it
    /// has sent it.
    pub(crate) fn start_history_export(
        &self,
        reports: Vec<Arc<DeletionReport>>,
        directory: PathBuf,
    ) -> Result<(), HistoryExportError> {
        let exported = reports.len();
        self.start_history_export_with(exported, move |cancelled| {
            export_deletion_history(&reports, &directory, cancelled)
        })
    }

    /// [`Self::start_history_export`] with the writing supplied: `write` runs on the export's
    /// thread, given the flag that shutdown raises, and its result is the export's outcome for
    /// `exported` reports. The seam that lets a test hold an export inside its write.
    pub(crate) fn start_history_export_with(
        &self,
        exported: usize,
        write: impl FnOnce(&AtomicBool) -> Result<PathBuf, String> + Send + 'static,
    ) -> Result<(), HistoryExportError> {
        let mut slot = self
            .history_export
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot.is_some() {
            return Err(HistoryExportError::Busy);
        }
        let (sender, outcome) = bounded(1);
        let cancelled = Arc::clone(&self.cancelled);
        let join = thread::Builder::new()
            .name("excise-history-export".to_string())
            .spawn(move || {
                let result = write(&cancelled);
                // The queue's only send, so it never waits; a pool that is gone ignores it.
                let _ = sender.send(HistoryExportOutcome { exported, result });
            })
            .map_err(HistoryExportError::Spawn)?;
        *slot = Some(HistoryExport { outcome, join });
        Ok(())
    }

    /// The outcome of the running export once it ended, which also frees the pool to start the
    /// next one.
    pub(crate) fn poll_history_export(&self) -> Option<HistoryExportOutcome> {
        let mut slot = self
            .history_export
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let outcome = match slot.as_ref()?.outcome.try_recv() {
            Ok(outcome) => outcome,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => HistoryExportOutcome {
                exported: 0,
                result: Err("the deletion history export stopped unexpectedly".to_string()),
            },
        };
        if let Some(export) = slot.take() {
            let _ = export.join.join();
        }
        Some(outcome)
    }

    /// Whether an export is running, or has ended and not yet been collected.
    pub(crate) fn history_export_running(&self) -> bool {
        self.history_export
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
    }

    /// Holds the deletion executor inside a job that does not return, as an entry stuck in a hung
    /// file system would, and returns once it is inside. It goes on when `release` is sent to or
    /// dropped.
    #[cfg(test)]
    pub(crate) fn hold_executor_for_test(&self, release: Receiver<()>) {
        let (entered, inside) = bounded(1);
        self.executor_commands
            .try_send(ExecutorCommand::Hold { entered, release })
            .expect("the executor's lane should be free");
        inside
            .recv_timeout(Duration::from_secs(10))
            .expect("the executor should enter the hold");
    }

    /// Stops every worker thread and waits for each as long as it takes.
    pub fn shutdown(self) -> Result<(), AppError> {
        self.shutdown_with(&mut ShutdownWait::patient())
    }

    /// Stops every worker thread and waits for each, unless `wait` gives up on it: a forced stop
    /// leaves the scanner, the planner and an export that have not ended after its grace to the
    /// exit of the process. The deletion executor is the exception: it is waited for however long
    /// it takes, because a process that ended inside one of its entries could leave a target that
    /// is neither intact nor gone.
    pub(crate) fn shutdown_with(self, wait: &mut ShutdownWait) -> Result<(), AppError> {
        let Self {
            events,
            planner_commands,
            executor_commands,
            cancelled,
            deletion_plan_cancelled,
            deletion_soft_cancelled,
            active_deletion_progress: _,
            scanner,
            scan_session: _,
            coordinator,
            generation_rebuild_lease: _,
            history_export,
            scanner_handle,
            planner_handle,
            executor_handle,
        } = self;
        cancelled.store(true, Ordering::Release);
        deletion_plan_cancelled.store(true, Ordering::Release);
        deletion_soft_cancelled.store(true, Ordering::Release);
        scanner.cancel_rebuild();
        drop(events);
        drop(planner_commands);
        drop(executor_commands);
        drop(scanner);
        drop(coordinator);
        // Every thread is joined, whichever one panicked: a thread left running would go on
        // writing after the terminal and the session's storage are cleaned up. The first failure
        // is what the caller gets, once the last thread has stopped. A thread the wait gave up on
        // is not one that failed: nothing is known of it, and the process is on its way out.
        let mut failure = None;
        for (name, handle) in [
            ("scanner", scanner_handle),
            ("deletion planner", planner_handle),
        ] {
            if matches!(wait.join(handle), Joined::Ended(Err(_))) {
                failure.get_or_insert_with(|| AppError::Worker(format!("{name} thread panicked")));
            }
        }
        // The executor is waited for however long it takes, whatever the stop requests. Each entry
        // is moved aside before it is removed, so a process that ended inside one could leave a
        // target that is neither intact nor gone, and a deletion stuck in a call that never returns
        // therefore still holds a forced exit. The grace of a forced stop does not run meanwhile.
        if wait.join_patiently(executor_handle).is_err() {
            failure.get_or_insert_with(|| {
                AppError::Worker("deletion executor thread panicked".to_string())
            });
        }
        // The cancellation above ends an export in progress at its next write.
        let export = history_export
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(export) = export {
            drop(export.outcome);
            if matches!(wait.join(export.join), Joined::Ended(Err(_))) {
                failure.get_or_insert_with(|| {
                    AppError::Worker("deletion history export thread panicked".to_string())
                });
            }
        }
        failure.map_or(Ok(()), Err)
    }
}

/// Writes the deletion history to the next free export file in `directory` and returns its path.
/// Only a cancellation (the pool shutting down) removes the file it was writing, so that no half
/// of one is left for a reader to mistake for a whole export; a write that failed reports its
/// error with the file in place, as every export always has.
fn export_deletion_history(
    reports: &[Arc<DeletionReport>],
    directory: &Path,
    cancelled: &AtomicBool,
) -> Result<PathBuf, String> {
    let path = super::next_export_path_in(directory, "deletion-history")?;
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|error| error.to_string())?;
    super::export_report(ExportWriter { file, cancelled }, |writer| {
        write_deletion_history_json(reports, writer)
    })
    .inspect_err(|_| {
        if cancelled.load(Ordering::Acquire) {
            let _ = std::fs::remove_file(&path);
        }
    })?;
    Ok(path)
}

/// The export's file, which refuses every write once the pool shuts down, so a long export ends
/// within one buffer's write instead of finishing a file nobody will read.
struct ExportWriter<'a> {
    file: std::fs::File,
    cancelled: &'a AtomicBool,
}

impl Write for ExportWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err(io::Error::other("the export was cancelled"));
        }
        self.file.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "planner inputs stay explicit at the identity and capacity boundary"
)]
fn planning_worker(
    scan_root: &std::path::Path,
    scan_root_identity: Option<&NativeIdentity>,
    temporary_storage: &TemporaryStorage,
    commands: &Receiver<PlannerCommand>,
    sender: &Sender<WorkerEvent>,
    plan_cancelled: &AtomicBool,
    cancelled: &AtomicBool,
) {
    loop {
        if cancelled.load(Ordering::Acquire) {
            return;
        }
        let command = match commands.recv_timeout(CHANNEL_RETRY) {
            Ok(command) => command,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        let PlannerCommand::Plan {
            work_id,
            target,
            reduced_guardrails,
            maximum_bytes,
        } = command;
        let result = if let Some(identity) = scan_root_identity {
            build_plan_cancellable_with_root_identity_and_temporary_storage(
                scan_root,
                identity.clone(),
                *target,
                reduced_guardrails,
                plan_cancelled,
                maximum_bytes,
                temporary_storage,
            )
        } else {
            build_plan_cancellable_with_temporary_storage(
                scan_root,
                *target,
                reduced_guardrails,
                plan_cancelled,
                maximum_bytes,
                temporary_storage,
            )
        }
        .map(Box::new);
        if !send_event(
            sender,
            WorkerEvent::DeletionPlanned { work_id, result },
            cancelled,
        ) {
            return;
        }
    }
}

fn execution_worker(
    scan_root: &std::path::Path,
    commands: &Receiver<ExecutorCommand>,
    sender: &Sender<WorkerEvent>,
    soft_cancelled: &AtomicBool,
    active_progress: &Mutex<Option<Arc<DeletionExecutionProgress>>>,
    cancelled: &AtomicBool,
) {
    loop {
        if cancelled.load(Ordering::Acquire) {
            return;
        }
        let command = match commands.recv_timeout(CHANNEL_RETRY) {
            Ok(command) => command,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        let (work_id, plan, progress) = match command {
            ExecutorCommand::Execute {
                work_id,
                plan,
                progress,
            } => (work_id, plan, progress),
            #[cfg(test)]
            ExecutorCommand::Hold { entered, release } => {
                let _ = entered.send(());
                // Returns once the test sends, or drops its end.
                let _ = release.recv();
                continue;
            }
        };
        let event = match revalidate_plan_cancellable(scan_root, &plan, soft_cancelled) {
            Ok(()) => WorkerEvent::DeletionFinished {
                work_id,
                report: execute_plan_counted(
                    scan_root,
                    *plan,
                    soft_cancelled,
                    cancelled,
                    progress.completed(),
                    || !cancelled.load(Ordering::Acquire) && progress.try_claim_mutation(),
                    || !cancelled.load(Ordering::Acquire) && progress.try_begin_mutation(),
                ),
            },
            Err(error) => WorkerEvent::DeletionExecutionRejected { work_id, error },
        };
        clear_active_deletion_progress(active_progress, &progress);
        if !send_event(sender, event, cancelled) {
            return;
        }
    }
}

#[cfg(all(test, unix))]
fn safe_worker_path(path: &Path) -> String {
    safe_display_path_text(path)
}

fn safe_worker_text(value: &str) -> String {
    safe_display_text(value)
}

#[cfg(all(test, unix))]
fn format_deletion_error(
    error: DeletionPlanError,
    scan_root: &Path,
    target_relative: &Path,
) -> String {
    match error {
        DeletionPlanError::Io {
            path,
            message,
            kind: _,
        } => {
            let target_lossy = target_relative.to_string_lossy();
            let root_lossy = scan_root.to_string_lossy();
            let path = if path == target_lossy {
                safe_worker_path(target_relative)
            } else if path == root_lossy {
                safe_worker_path(scan_root)
            } else {
                safe_worker_text(&path)
            };
            format!(
                "deletion planning failed for {path}: {}",
                safe_worker_text(&message)
            )
        }
        error => safe_worker_text(&error.to_string()),
    }
}

fn sanitize_worker_event(event: WorkerEvent) -> WorkerEvent {
    match event {
        WorkerEvent::ScanFailed { path, message } => WorkerEvent::ScanFailed {
            path,
            message: safe_worker_text(&message),
        },
        WorkerEvent::ScanUnscanned {
            lease,
            path,
            reason,
            input_runs,
        } => WorkerEvent::ScanUnscanned {
            lease,
            path,
            reason: sanitize_unscanned_reason(reason),
            input_runs,
        },
        event => event,
    }
}

fn sanitize_unscanned_reason(
    reason: crate::model::UnscannedReason,
) -> crate::model::UnscannedReason {
    match reason {
        crate::model::UnscannedReason::Excluded(pattern) => {
            crate::model::UnscannedReason::Excluded(safe_worker_text(&pattern))
        }
        crate::model::UnscannedReason::Metadata(message) => {
            crate::model::UnscannedReason::Metadata(safe_worker_text(&message))
        }
        crate::model::UnscannedReason::Replacement(message) => {
            crate::model::UnscannedReason::Replacement(safe_worker_text(&message))
        }
        reason => reason,
    }
}

pub(super) fn send_event(
    sender: &Sender<WorkerEvent>,
    mut event: WorkerEvent,
    cancelled: &AtomicBool,
) -> bool {
    event = sanitize_worker_event(event);
    loop {
        match sender.send_timeout(event, CHANNEL_RETRY) {
            Ok(()) => return true,
            Err(SendTimeoutError::Timeout(returned)) => {
                if cancelled.load(Ordering::Acquire) {
                    return false;
                }
                event = returned;
            }
            Err(SendTimeoutError::Disconnected(_)) => return false,
        }
    }
}
#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::time::Duration;

    use crate::model::UnscannedReason;
    use crate::scan_coordinator::SessionCoordinator;
    #[cfg(unix)]
    use crate::state::FileToDelete;

    use super::*;

    fn options(root: &std::path::Path, threads: usize) -> ScannerOptions {
        let session = ScanSessionId::from_bytes([3; 16]);
        ScannerOptions {
            root: root.to_path_buf(),
            session,
            generation: ScanGeneration::initial(),
            coordinator: SessionCoordinator::start(session, ScanGeneration::initial())
                .expect("scanner coordinator should start"),
            root_identity: None,
            threads,
            cross_filesystems: false,
            exclusions: Vec::new(),
            internal_paths: Vec::new(),
            temporary_storage: crate::temporary_storage::TemporaryStorage::default(),
            input_runs: None,
        }
    }

    #[cfg(unix)]
    fn single_file_plan(root: &std::path::Path) -> (std::path::PathBuf, DeletionPlan) {
        use std::os::unix::fs::MetadataExt as _;
        use std::time::UNIX_EPOCH;

        use crate::deletion::{PlannedKind, PlannedSnapshot, ReviewedEntry, build_plan};
        use crate::native_path::identity_for;
        use crate::state::tiles::FileType;

        let path = root.join("target");
        std::fs::write(&path, b"payload").expect("target should be written");
        let metadata = std::fs::symlink_metadata(&path).expect("target metadata should exist");
        let identity = identity_for(&path, &metadata)
            .expect("target identity should be readable")
            .expect("target identity should be available");
        let snapshot = PlannedSnapshot {
            identity: identity.clone(),
            kind: PlannedKind::File,
            apparent_bytes: u128::from(metadata.len()),
            allocated_bytes: Some(u128::from(metadata.blocks()).saturating_mul(512)),
            modified_nanos: metadata
                .modified()
                .ok()
                .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_nanos()),
        };
        let target = FileToDelete {
            node_id: crate::model::NodeId(1),
            synthetic: false,
            path_in_filesystem: root.to_path_buf(),
            path_to_file: vec![std::ffi::OsString::from("target")],
            file_type: FileType::File,
            num_descendants: None,
            size: snapshot.apparent_bytes,
            expected_snapshot: crate::model::EntrySnapshot {
                identity: Some(identity),
                kind: crate::model::NodeKind::File,
                apparent_bytes: snapshot.apparent_bytes,
                allocated_bytes: snapshot.allocated_bytes,
                modified_nanos: snapshot.modified_nanos,
            },
            reviewed_entries: vec![ReviewedEntry {
                relative_path: std::path::PathBuf::from("target"),
                snapshot: snapshot.clone(),
            }],
        };
        let plan = build_plan(root, target, false).expect("deletion plan should build");
        (path, plan)
    }

    #[cfg(unix)]
    #[test]
    fn executor_revalidates_before_first_mutation() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let (path, plan) = single_file_plan(root.path());
        let workers = WorkerPool::start(options(root.path(), 1), 16).expect("workers should start");
        std::fs::write(&path, b"replacement-after-confirmation")
            .expect("target should be replaced before execution");
        let progress = Arc::new(DeletionExecutionProgress::new());
        assert!(
            workers
                .submit_deletion_work(DeletionWorkCommand::Execute {
                    work_id: DeletionWorkId::for_test(1),
                    plan: Box::new(plan),
                    progress: Arc::clone(&progress),
                })
                .is_ok(),
            "execution should be queued"
        );

        let error = loop {
            match workers
                .events()
                .recv_timeout(Duration::from_secs(5))
                .expect("worker should report execution revalidation")
            {
                WorkerEvent::DeletionExecutionRejected { error, .. } => break error,
                WorkerEvent::DeletionFinished { .. } => {
                    panic!("changed target must not be removed")
                }
                _ => {}
            }
        };
        assert!(error.is_stale());
        assert!(
            !progress.has_started_mutation(),
            "a rejected final safety check must not mark the target as mutating"
        );
        assert!(path.exists(), "stale target must not be mutated");
        workers.shutdown().expect("workers should stop");
    }

    #[cfg(unix)]
    #[test]
    fn executor_runs_a_confirmed_plan_serially() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let (path, plan) = single_file_plan(root.path());
        let workers = WorkerPool::start(options(root.path(), 1), 16).expect("workers should start");
        let progress = Arc::new(DeletionExecutionProgress::new());
        assert!(
            workers
                .submit_deletion_work(DeletionWorkCommand::Execute {
                    work_id: DeletionWorkId(2),
                    plan: Box::new(plan),
                    progress: Arc::clone(&progress),
                })
                .is_ok(),
            "execution should be queued after resuming"
        );

        let report = loop {
            match workers
                .events()
                .recv_timeout(Duration::from_secs(5))
                .expect("worker should report deletion")
            {
                WorkerEvent::DeletionFinished { report, .. } => break report,
                WorkerEvent::ScanBatch { .. }
                | WorkerEvent::ScanUnscanned { .. }
                | WorkerEvent::ScanFailed { .. }
                | WorkerEvent::ScanFinished { .. }
                | WorkerEvent::DeletionPlanned { .. }
                | WorkerEvent::DeletionExecutionRejected { .. } => {}
            }
        };
        assert!(
            progress.has_started_mutation(),
            "a successfully revalidated command must mark mutation before reporting progress"
        );
        assert!(!report.soft_cancelled);
        assert_eq!(report.deleted_entries(), 1);
        assert!(!path.exists());
        workers.shutdown().expect("workers should stop");
    }

    #[test]
    fn scheduler_status_is_coalesced_without_consuming_scan_events() {
        let root = tempfile::tempdir().expect("scan root should exist");
        let workers = WorkerPool::start(options(root.path(), 1), 1).expect("workers should start");

        let initial = (0..100)
            .find_map(|_| {
                let snapshot = workers.scheduler_snapshot();
                if snapshot.is_none() {
                    std::thread::sleep(Duration::from_millis(5));
                }
                snapshot
            })
            .expect("scheduler should publish an initial status snapshot");
        assert_eq!(initial.session(), ScanSessionId::from_bytes([3; 16]));
        assert_eq!(initial.generation(), ScanGeneration::initial());

        for _ in 0..128 {
            let _ = workers.scheduler_snapshot();
        }

        loop {
            match workers
                .events()
                .recv_timeout(Duration::from_secs(5))
                .expect("scan should complete after status reads")
            {
                WorkerEvent::ScanFinished { cancelled: false } => break,
                WorkerEvent::ScanFinished { cancelled: true } => panic!("scan was cancelled"),
                WorkerEvent::ScanFailed { message, .. } => panic!("scan failed: {message}"),
                WorkerEvent::ScanBatch { .. }
                | WorkerEvent::ScanUnscanned { .. }
                | WorkerEvent::DeletionPlanned { .. }
                | WorkerEvent::DeletionExecutionRejected { .. }
                | WorkerEvent::DeletionFinished { .. } => {}
            }
        }

        let final_status = workers
            .scheduler_snapshot()
            .expect("completed scan should retain its last scheduler status");
        assert_eq!(final_status.pending().total(), 0);
        assert_eq!(final_status.active_leases(), 0);
        assert!(final_status.terminal().succeeded() >= 1);
        workers.shutdown().expect("workers should stop");
    }

    #[test]
    fn bounded_scanner_delivers_every_file() {
        let root = tempfile::tempdir().expect("scan root should exist");
        for index in 0..100 {
            std::fs::write(root.path().join(format!("file-{index}")), b"x")
                .expect("fixture file should be written");
        }

        let workers = WorkerPool::start(options(root.path(), 2), 1).expect("workers should start");
        let mut names = HashSet::new();
        loop {
            match workers
                .events()
                .recv_timeout(Duration::from_secs(5))
                .expect("scanner should produce completion")
            {
                WorkerEvent::ScanBatch { entries, .. } => {
                    for entry in entries {
                        if let Some(name) = entry.path.file_name().and_then(|name| name.to_str())
                            && name.starts_with("file-")
                        {
                            names.insert(name.to_string());
                        }
                    }
                }
                WorkerEvent::ScanFinished { cancelled: false } => break,
                WorkerEvent::ScanFinished { cancelled: true } => panic!("scan was cancelled"),
                WorkerEvent::ScanFailed { message, .. } => panic!("scan failed: {message}"),
                WorkerEvent::ScanUnscanned { .. }
                | WorkerEvent::DeletionPlanned { .. }
                | WorkerEvent::DeletionExecutionRejected { .. }
                | WorkerEvent::DeletionFinished { .. } => {}
            }
        }
        assert_eq!(names.len(), 100);
        workers.shutdown().expect("workers should stop");
    }

    #[test]
    fn generation_rebuild_reuses_persistent_scanner() {
        let root = tempfile::tempdir().expect("scan root should exist");
        let file = root.path().join("file");
        std::fs::write(&file, b"x").expect("fixture should be written");
        let scanner_options = options(root.path(), 1);
        let workers = WorkerPool::start(scanner_options.clone(), 1).expect("workers should start");

        for pass in 0..2 {
            if pass == 1 {
                workers
                    .request_generation_rebuild(scanner_options.clone())
                    .expect("scan rebuild should start");
            }
            let mut saw_file = false;
            loop {
                match workers
                    .events()
                    .recv_timeout(Duration::from_secs(5))
                    .expect("scan pass should complete")
                {
                    WorkerEvent::ScanBatch { entries, .. } => {
                        saw_file |= entries.iter().any(|entry| entry.path == file);
                    }
                    WorkerEvent::ScanFinished { cancelled: false } => break,
                    WorkerEvent::ScanFinished { cancelled: true } => {
                        panic!("scan pass was cancelled")
                    }
                    WorkerEvent::ScanFailed { message, .. } => panic!("scan failed: {message}"),
                    WorkerEvent::ScanUnscanned { .. }
                    | WorkerEvent::DeletionPlanned { .. }
                    | WorkerEvent::DeletionExecutionRejected { .. }
                    | WorkerEvent::DeletionFinished { .. } => {}
                }
            }
            assert!(saw_file);
            if pass == 1 {
                workers
                    .finish_generation_rebuild(WorkCompletion::Succeeded)
                    .expect("completed rebuild lease should finish");
            }
        }
        workers.shutdown().expect("workers should stop");
    }

    #[test]
    fn cancelled_rebuild_leaves_scanner_ready_for_next_generation() {
        let root = tempfile::tempdir().expect("scan root should exist");
        let file = root.path().join("file");
        std::fs::write(&file, b"x").expect("fixture file should be written");
        let scanner_options = options(root.path(), 1);
        let workers = WorkerPool::start(scanner_options.clone(), 1).expect("workers should start");

        loop {
            match workers
                .events()
                .recv_timeout(Duration::from_secs(5))
                .expect("initial scan should complete")
            {
                WorkerEvent::ScanFinished { cancelled: false } => break,
                WorkerEvent::ScanFinished { cancelled: true } => {
                    panic!("initial scan was cancelled")
                }
                WorkerEvent::ScanFailed { message, .. } => panic!("initial scan failed: {message}"),
                WorkerEvent::ScanBatch { .. }
                | WorkerEvent::ScanUnscanned { .. }
                | WorkerEvent::DeletionPlanned { .. }
                | WorkerEvent::DeletionExecutionRejected { .. }
                | WorkerEvent::DeletionFinished { .. } => {}
            }
        }

        workers
            .request_generation_rebuild(scanner_options.clone())
            .expect("scan rebuild should start");
        workers.cancel_generation_rebuild();
        loop {
            match workers
                .events()
                .recv_timeout(Duration::from_secs(5))
                .expect("cancelled rebuild should complete")
            {
                WorkerEvent::ScanFinished { cancelled: true } => break,
                WorkerEvent::ScanFinished { cancelled: false } => {
                    panic!("rebuild should observe its cancellation")
                }
                WorkerEvent::ScanFailed { message, .. } => panic!("rebuild failed: {message}"),
                WorkerEvent::ScanBatch { .. }
                | WorkerEvent::ScanUnscanned { .. }
                | WorkerEvent::DeletionPlanned { .. }
                | WorkerEvent::DeletionExecutionRejected { .. }
                | WorkerEvent::DeletionFinished { .. } => {}
            }
        }
        workers
            .finish_generation_rebuild(WorkCompletion::Cancelled)
            .expect("cancelled rebuild lease should finish");

        workers
            .request_generation_rebuild(scanner_options)
            .expect("next scan rebuild should start");
        let mut saw_file = false;
        loop {
            match workers
                .events()
                .recv_timeout(Duration::from_secs(5))
                .expect("next rebuild should complete")
            {
                WorkerEvent::ScanBatch { entries, .. } => {
                    saw_file |= entries.iter().any(|entry| entry.path == file);
                }
                WorkerEvent::ScanFinished { cancelled: false } => break,
                WorkerEvent::ScanFinished { cancelled: true } => {
                    panic!("next rebuild was unexpectedly cancelled")
                }
                WorkerEvent::ScanFailed { message, .. } => panic!("next rebuild failed: {message}"),
                WorkerEvent::ScanUnscanned { .. }
                | WorkerEvent::DeletionPlanned { .. }
                | WorkerEvent::DeletionExecutionRejected { .. }
                | WorkerEvent::DeletionFinished { .. } => {}
            }
        }
        assert!(saw_file);
        workers
            .finish_generation_rebuild(WorkCompletion::Succeeded)
            .expect("completed rebuild lease should finish");
        workers.shutdown().expect("workers should stop");
    }

    #[test]
    fn deep_wide_scan_completes_with_bounded_queue() {
        let root = tempfile::tempdir().expect("scan root should exist");
        let mut deepest = root.path().to_path_buf();
        for depth in 0..300 {
            for sibling in 0..9 {
                std::fs::create_dir(deepest.join(format!("s{depth}-{sibling}")))
                    .expect("sibling directory should be created");
            }
            deepest.push("d");
            std::fs::create_dir(&deepest).expect("deep directory should be created");
        }
        let marker = deepest.join("marker");
        std::fs::write(&marker, b"x").expect("deep marker should be written");
        let workers = WorkerPool::start(options(root.path(), 1), 1).expect("workers should start");
        // Hosted volumes can spend more than a minute opening this deep fixture.
        // This regression checks eventual bounded completion, not I/O throughput.
        let event_timeout = Duration::from_secs(120);
        let mut found = false;
        loop {
            match workers
                .events()
                .recv_timeout(event_timeout)
                .expect("deep scan should complete")
            {
                WorkerEvent::ScanBatch { entries, .. } => {
                    found |= entries.iter().any(|entry| entry.path == marker);
                }
                WorkerEvent::ScanFinished { cancelled: false } => break,
                WorkerEvent::ScanFinished { cancelled: true } => panic!("scan was cancelled"),
                WorkerEvent::ScanFailed { message, .. } => panic!("scan failed: {message}"),
                WorkerEvent::ScanUnscanned { .. }
                | WorkerEvent::DeletionPlanned { .. }
                | WorkerEvent::DeletionExecutionRejected { .. }
                | WorkerEvent::DeletionFinished { .. } => {}
            }
        }
        assert!(found);
        workers.shutdown().expect("workers should stop");
    }

    #[test]
    fn bounded_directory_queue_delivers_wide_tree() {
        let root = tempfile::tempdir().expect("scan root should exist");
        for index in 0..64 {
            let directory = root.path().join(format!("directory-{index}"));
            std::fs::create_dir(&directory).expect("fixture directory should be created");
            std::fs::write(directory.join("file"), b"x").expect("fixture file should be written");
        }

        let workers = WorkerPool::start(options(root.path(), 1), 1).expect("workers should start");
        let mut files = HashSet::new();
        loop {
            match workers
                .events()
                .recv_timeout(Duration::from_secs(5))
                .expect("wide scan should complete")
            {
                WorkerEvent::ScanBatch { entries, .. } => {
                    for entry in entries {
                        if entry.path.file_name().is_some_and(|name| name == "file") {
                            files.insert(entry.path);
                        }
                    }
                }
                WorkerEvent::ScanFinished { cancelled: false } => break,
                WorkerEvent::ScanFinished { cancelled: true } => panic!("scan was cancelled"),
                WorkerEvent::ScanFailed { message, .. } => panic!("scan failed: {message}"),
                WorkerEvent::ScanUnscanned { .. }
                | WorkerEvent::DeletionPlanned { .. }
                | WorkerEvent::DeletionExecutionRejected { .. }
                | WorkerEvent::DeletionFinished { .. } => {}
            }
        }
        assert_eq!(files.len(), 64);
        workers.shutdown().expect("workers should stop");
    }

    #[test]
    fn shutdown_unblocks_a_backpressured_scanner() {
        let root = tempfile::tempdir().expect("scan root should exist");
        for index in 0..100 {
            std::fs::write(root.path().join(format!("file-{index}")), b"x")
                .expect("fixture file should be written");
        }
        let workers = WorkerPool::start(options(root.path(), 2), 1).expect("workers should start");
        workers.shutdown().expect("shutdown should unblock senders");
    }

    #[test]
    fn scanner_prunes_configured_exclusions() {
        let root = tempfile::tempdir().expect("scan root should exist");
        let ignored = root.path().join("ignored");
        std::fs::create_dir(&ignored).expect("ignored directory should be created");
        let secret = ignored.join("secret");
        std::fs::write(&secret, b"x").expect("ignored file should be written");
        let mut scanner_options = options(root.path(), 1);
        scanner_options.exclusions = vec!["ignored/".to_string()];

        let workers = WorkerPool::start(scanner_options, 16).expect("workers should start");
        let mut excluded_directory = false;
        let mut traversed_secret = false;
        loop {
            match workers
                .events()
                .recv_timeout(Duration::from_secs(5))
                .expect("excluded scan should complete")
            {
                WorkerEvent::ScanUnscanned { path, reason, .. } => {
                    excluded_directory |= path == ignored
                        && reason == UnscannedReason::Excluded("ignored/".to_string());
                }
                WorkerEvent::ScanBatch { entries, .. } => {
                    traversed_secret |= entries.iter().any(|entry| entry.path == secret);
                }
                WorkerEvent::ScanFinished { cancelled: false } => break,
                WorkerEvent::ScanFinished { cancelled: true } => panic!("scan was cancelled"),
                WorkerEvent::ScanFailed { message, .. } => panic!("scan failed: {message}"),
                WorkerEvent::DeletionPlanned { .. }
                | WorkerEvent::DeletionExecutionRejected { .. }
                | WorkerEvent::DeletionFinished { .. } => {}
            }
        }
        assert!(excluded_directory);
        assert!(!traversed_secret);
        workers.shutdown().expect("workers should stop");
    }

    #[test]
    fn scanner_skips_only_explicit_internal_paths() {
        let root = tempfile::tempdir().expect("scan root should exist");
        let user_session = root.path().join(".excise-session-user-data");
        let active_spill = root.path().join(".excise-session-active");
        std::fs::create_dir(&user_session).expect("user-owned session-like directory should exist");
        std::fs::create_dir(&active_spill).expect("active spill directory should exist");
        let user_file = user_session.join("user-file");
        let spill_file = active_spill.join("spill-file");
        std::fs::write(&user_file, b"user data").expect("user fixture should be written");
        std::fs::write(&spill_file, b"spill data").expect("spill fixture should be written");

        let mut scanner_options = options(root.path(), 1);
        scanner_options.internal_paths = vec![active_spill.clone()];
        let workers = WorkerPool::start(scanner_options, 16).expect("workers should start");
        let mut saw_spill_directory = false;
        let mut saw_user_directory = false;
        let mut saw_user_file = false;
        let mut saw_spill_file = false;
        loop {
            match workers
                .events()
                .recv_timeout(Duration::from_secs(5))
                .expect("scanner should complete")
            {
                WorkerEvent::ScanBatch { entries, .. } => {
                    saw_user_directory |= entries.iter().any(|entry| entry.path == user_session);
                    saw_user_file |= entries.iter().any(|entry| entry.path == user_file);
                    saw_spill_file |= entries.iter().any(|entry| entry.path == spill_file);
                    saw_spill_directory |= entries.iter().any(|entry| entry.path == active_spill);
                }
                WorkerEvent::ScanFinished { cancelled: false } => break,
                WorkerEvent::ScanFinished { cancelled: true } => panic!("scan was cancelled"),
                WorkerEvent::ScanFailed { message, .. } => panic!("scan failed: {message}"),
                WorkerEvent::ScanUnscanned { .. }
                | WorkerEvent::DeletionPlanned { .. }
                | WorkerEvent::DeletionExecutionRejected { .. }
                | WorkerEvent::DeletionFinished { .. } => {}
            }
        }

        assert!(!saw_spill_directory);
        assert!(saw_user_directory);
        assert!(saw_user_file);
        assert!(!saw_spill_file);
        workers.shutdown().expect("workers should stop");
    }

    /// A session keeps files open in the tree it scans, and they come and go while the scan runs:
    /// a deletion's spill on Windows lives in the folder that held its target for as long as the
    /// report stays in the history. The scan leaves out the paths the session registers, by exact
    /// path and not by the shape of a name: a user's own file may look just like it.
    #[test]
    fn scanner_skips_a_registered_private_file_and_no_other_file_of_its_shape() {
        let root = tempfile::tempdir().expect("scan root should exist");
        let ours = root.path().join(".excise-deletion-spill-0001");
        let users = root.path().join(".excise-deletion-spill-0002");
        std::fs::write(&ours, b"ours").expect("the session's file should be written");
        std::fs::write(&users, b"a user's file").expect("the user's file should be written");

        let scanner_options = options(root.path(), 1);
        let registration = scanner_options
            .temporary_storage
            .private_files()
            .register(ours.clone());
        let workers = WorkerPool::start(scanner_options, 16).expect("workers should start");
        let mut saw_ours = false;
        let mut saw_users = false;
        loop {
            match workers
                .events()
                .recv_timeout(Duration::from_secs(5))
                .expect("scanner should complete")
            {
                WorkerEvent::ScanBatch { entries, .. } => {
                    saw_ours |= entries.iter().any(|entry| entry.path == ours);
                    saw_users |= entries.iter().any(|entry| entry.path == users);
                }
                WorkerEvent::ScanFinished { cancelled: false } => break,
                WorkerEvent::ScanFinished { cancelled: true } => panic!("scan was cancelled"),
                WorkerEvent::ScanFailed { message, .. } => panic!("scan failed: {message}"),
                WorkerEvent::ScanUnscanned { path, .. } => {
                    assert_ne!(path, ours, "the session's own file is not even reported");
                }
                WorkerEvent::DeletionPlanned { .. }
                | WorkerEvent::DeletionExecutionRejected { .. }
                | WorkerEvent::DeletionFinished { .. } => {}
            }
        }

        assert!(!saw_ours);
        assert!(saw_users);
        drop(registration);
        workers.shutdown().expect("workers should stop");
    }

    #[cfg(unix)]
    #[test]
    fn scanner_never_traverses_descendant_symlinks() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("scan root should exist");
        let outside = tempfile::tempdir().expect("outside root should exist");
        std::fs::write(outside.path().join("secret"), b"x")
            .expect("outside file should be written");
        let link = root.path().join("linked");
        symlink(outside.path(), &link).expect("directory link should be created");

        let workers = WorkerPool::start(options(root.path(), 1), 16).expect("workers should start");
        let mut skipped_link = false;
        let mut traversed_secret = false;
        loop {
            match workers
                .events()
                .recv_timeout(Duration::from_secs(5))
                .expect("scanner should produce completion")
            {
                WorkerEvent::ScanUnscanned { path, .. } => skipped_link |= path == link,
                WorkerEvent::ScanBatch { entries, .. } => {
                    traversed_secret |= entries
                        .iter()
                        .any(|entry| entry.path.file_name().is_some_and(|name| name == "secret"));
                }
                WorkerEvent::ScanFinished { cancelled: false } => break,
                WorkerEvent::ScanFinished { cancelled: true } => panic!("scan was cancelled"),
                WorkerEvent::ScanFailed { message, .. } => panic!("scan failed: {message}"),
                WorkerEvent::DeletionPlanned { .. }
                | WorkerEvent::DeletionExecutionRejected { .. }
                | WorkerEvent::DeletionFinished { .. } => {}
            }
        }
        assert!(skipped_link);
        assert!(!traversed_secret);
        workers.shutdown().expect("workers should stop");
    }

    #[cfg(unix)]
    #[test]
    fn scanner_rejects_replaced_root_before_traversal() {
        use crate::native_path::identity_for;
        use std::os::unix::fs::symlink;

        let parent = tempfile::tempdir().expect("scan parent should exist");
        let scan_root = parent.path().join("scan-root");
        let original = parent.path().join("original-root");
        let outside = parent.path().join("outside-root");
        std::fs::create_dir(&scan_root).expect("scan root should be created");
        std::fs::write(scan_root.join("original"), b"original")
            .expect("original fixture should be written");
        let metadata = std::fs::symlink_metadata(&scan_root).expect("root metadata should exist");
        let identity = identity_for(&scan_root, &metadata)
            .expect("root identity should be readable")
            .expect("root should not be a symbolic link");
        std::fs::rename(&scan_root, &original).expect("original root should be displaced");
        std::fs::create_dir(&outside).expect("replacement root should be created");
        std::fs::write(outside.join("replacement"), b"replacement")
            .expect("replacement fixture should be written");
        symlink(&outside, &scan_root).expect("replacement symlink should be created");

        let mut scanner_options = options(&scan_root, 1);
        scanner_options.root_identity = Some(identity);
        let workers = WorkerPool::start(scanner_options, 16).expect("workers should start");
        let mut failed = false;
        loop {
            match workers
                .events()
                .recv_timeout(Duration::from_secs(5))
                .expect("scanner should produce completion")
            {
                WorkerEvent::ScanFailed { message, .. } => {
                    failed = message.contains("replaced") || message.contains("changed");
                }
                WorkerEvent::ScanFinished { cancelled: false } => break,
                WorkerEvent::ScanFinished { cancelled: true } => {
                    panic!("replaced root should be rejected, not cancelled")
                }
                WorkerEvent::ScanBatch { .. }
                | WorkerEvent::ScanUnscanned { .. }
                | WorkerEvent::DeletionPlanned { .. }
                | WorkerEvent::DeletionExecutionRejected { .. }
                | WorkerEvent::DeletionFinished { .. } => {}
            }
        }
        assert!(failed);
        workers.shutdown().expect("workers should stop");
    }

    #[cfg(unix)]
    #[test]
    fn scanner_stops_before_following_a_replaced_root() {
        use crate::native_path::identity_for;

        let parent = tempfile::tempdir().expect("scan parent should exist");
        let scan_root = parent.path().join("scan-root");
        let original = parent.path().join("original-root");
        let outside = parent.path().join("outside-root");
        std::fs::create_dir(&scan_root).expect("scan root should be created");
        std::fs::create_dir(&outside).expect("replacement root should be created");
        let child = scan_root.join("child");
        std::fs::create_dir(&child).expect("child directory should be created");
        std::fs::write(child.join("original"), b"original")
            .expect("child fixture should be written");
        for index in 0..256 {
            std::fs::write(scan_root.join(format!("original-{index}")), b"original")
                .expect("original fixture should be written");
        }
        std::fs::write(outside.join("replacement"), b"replacement")
            .expect("replacement fixture should be written");
        let metadata = std::fs::symlink_metadata(&scan_root).expect("root metadata should exist");
        let identity = identity_for(&scan_root, &metadata)
            .expect("root identity should be readable")
            .expect("root should not be a symbolic link");
        scanner::replace_after_next_batch(scan_root.clone(), original, outside.clone());

        let mut scanner_options = options(&scan_root, 1);
        scanner_options.root_identity = Some(identity);
        let workers = WorkerPool::start(scanner_options, 1).expect("workers should start");
        let mut saw_batch = false;
        let mut saw_root_change = false;
        let mut saw_replacement = false;
        loop {
            match workers
                .events()
                .recv_timeout(Duration::from_secs(5))
                .expect("scanner should produce completion")
            {
                WorkerEvent::ScanBatch { entries, .. } => {
                    saw_batch = true;
                    saw_replacement |= entries
                        .iter()
                        .any(|entry| entry.path == outside.join("replacement"));
                }
                WorkerEvent::ScanFailed { message, .. } => {
                    saw_root_change |= message.contains("during traversal");
                }
                WorkerEvent::ScanUnscanned {
                    path,
                    reason: UnscannedReason::Replacement(_),
                    ..
                } if path == scan_root => {
                    saw_root_change = true;
                }
                WorkerEvent::ScanFinished { cancelled: false } => break,
                WorkerEvent::ScanFinished { cancelled: true } => {
                    panic!("root replacement should be uncertain, not cancellation")
                }
                WorkerEvent::ScanUnscanned { .. }
                | WorkerEvent::DeletionPlanned { .. }
                | WorkerEvent::DeletionExecutionRejected { .. }
                | WorkerEvent::DeletionFinished { .. } => {}
            }
        }
        assert!(
            saw_batch,
            "test must emit a batch before replacing the root"
        );
        assert!(
            saw_root_change,
            "root replacement should emit an explicit failure"
        );
        assert!(
            !saw_replacement,
            "scanner must not follow the replacement root"
        );
        workers.shutdown().expect("workers should stop");
    }

    #[test]
    fn worker_error_messages_escape_controls_and_bidi_overrides() {
        let (sender, events) = bounded(1);
        let cancelled = AtomicBool::new(false);
        assert!(send_event(
            &sender,
            WorkerEvent::ScanFailed {
                path: Some(PathBuf::from("/scan/hostile")),
                message: "permission denied\n\u{202e}name\u{1b}[31m".to_string(),
            },
            &cancelled,
        ));
        let WorkerEvent::ScanFailed { message, .. } = events
            .recv()
            .expect("sanitized worker error should be delivered")
        else {
            panic!("expected a scan failure event");
        };
        assert!(message.starts_with(DECEPTIVE_DISPLAY_MARKER));
        assert!(message.contains("\\n"));
        assert!(message.contains("\\u{202e}"));
        assert!(message.contains("\\x1b"));
        assert!(!message.chars().any(char::is_control));
        assert!(!message.contains('\u{202e}'));
    }

    #[cfg(unix)]
    #[test]
    fn deletion_errors_preserve_invalid_target_bytes() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt as _;

        let relative = PathBuf::from(OsString::from_vec(b"bad\xffname".to_vec()));
        let error = DeletionPlanError::Io {
            path: relative.to_string_lossy().into_owned(),
            message: "permission denied".to_string(),
            kind: std::io::ErrorKind::PermissionDenied,
        };
        let rendered = format_deletion_error(error, Path::new("/scan"), &relative);
        assert!(rendered.contains(DECEPTIVE_DISPLAY_MARKER));
        assert!(rendered.contains("bad\\xffname"));
        assert!(!rendered.chars().any(char::is_control));
    }

    #[test]
    fn worker_unscanned_error_reasons_escape_controls() {
        let (sender, events) = bounded(1);
        let cancelled = AtomicBool::new(false);
        assert!(send_event(
            &sender,
            WorkerEvent::ScanUnscanned {
                lease: None,
                path: PathBuf::from("/scan/hostile"),
                reason: crate::model::UnscannedReason::Metadata(
                    "metadata failed\t\u{202e}name".to_string(),
                ),
                input_runs: SealedBatch::empty(),
            },
            &cancelled,
        ));
        let WorkerEvent::ScanUnscanned { reason, .. } = events
            .recv()
            .expect("sanitized unscanned event should be delivered")
        else {
            panic!("expected an unscanned event");
        };
        let crate::model::UnscannedReason::Metadata(message) = reason else {
            panic!("expected metadata reason");
        };
        assert!(message.starts_with(DECEPTIVE_DISPLAY_MARKER));
        assert!(message.contains("\\t"));
        assert!(message.contains("\\u{202e}"));
        assert!(!message.chars().any(char::is_control));
        assert!(!message.contains('\u{202e}'));
    }
    #[test]
    fn deletion_plan_errors_remain_safe_when_rendered() {
        let (sender, events) = bounded(1);
        let cancelled = AtomicBool::new(false);
        assert!(send_event(
            &sender,
            WorkerEvent::DeletionPlanned {
                work_id: DeletionWorkId(1),
                result: Err(DeletionPlanError::Io {
                    path: "bad\u{202e}name".to_string(),
                    message: "permission denied\n\u{202e}name\u{1b}[31m".to_string(),
                    kind: std::io::ErrorKind::PermissionDenied,
                }),
            },
            &cancelled,
        ));
        let WorkerEvent::DeletionPlanned { result, .. } =
            events.recv().expect("deletion error should be delivered")
        else {
            panic!("expected a deletion plan event");
        };
        let message = result
            .expect_err("injected deletion error should remain an error")
            .to_string();
        assert!(message.contains(DECEPTIVE_DISPLAY_MARKER));
        assert!(message.contains("\\n"));
        assert!(message.contains("\\u{202e}"));
        assert!(message.contains("\\x1b"));
        assert!(!message.chars().any(char::is_control));
        assert!(!message.contains('\u{202e}'));
    }

    fn history_report(target: &str) -> Arc<DeletionReport> {
        Arc::new(DeletionReport {
            target_node_id: crate::model::NodeId(1),
            root_relative_path: PathBuf::from(target),
            scan_root: PathBuf::from("root"),
            entries: Vec::new().into(),
            soft_cancelled: false,
            precise: true,
            estimated_bytes: 0,
        })
    }

    /// Waits, bounded, for the running export's outcome.
    fn collect_export(workers: &WorkerPool) -> HistoryExportOutcome {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(outcome) = workers.poll_history_export() {
                return outcome;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the export should end"
            );
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn an_export_writes_what_the_report_writer_streams_and_never_overwrites_a_file() {
        let directory = tempfile::tempdir().expect("export directory should exist");
        let reports = vec![history_report("one"), history_report("two")];
        let mut expected = Vec::new();
        write_deletion_history_json(&reports, &mut expected).expect("history should serialize");
        let never = AtomicBool::new(false);

        let first = export_deletion_history(&reports, directory.path(), &never)
            .expect("the first export should be written");
        let second = export_deletion_history(&reports, directory.path(), &never)
            .expect("the second export should be written");

        assert_eq!(first, directory.path().join("excise-deletion-history.json"));
        assert_eq!(
            second,
            directory.path().join("excise-deletion-history-1.json")
        );
        assert_eq!(
            std::fs::read(&first).expect("the first export should be readable"),
            expected
        );
        assert_eq!(
            std::fs::read(&second).expect("the second export should be readable"),
            expected
        );
    }

    #[test]
    fn a_cancelled_export_stops_writing_and_leaves_no_half_file_behind() {
        let directory = tempfile::tempdir().expect("export directory should exist");
        let cancelled = AtomicBool::new(true);

        let result =
            export_deletion_history(&[history_report("one")], directory.path(), &cancelled);

        assert!(result.is_err(), "a cancelled export writes no file");
        assert_eq!(
            std::fs::read_dir(directory.path())
                .expect("export directory should be readable")
                .count(),
            0,
            "no partial file is left for a reader to mistake for a whole export"
        );
    }

    #[test]
    fn one_export_runs_at_a_time_until_its_outcome_is_collected() {
        let root = tempfile::tempdir().expect("scan root should exist");
        let exports = tempfile::tempdir().expect("export directory should exist");
        let workers = WorkerPool::start(options(root.path(), 1), 1).expect("workers should start");
        let reports = vec![history_report("one")];
        let start = |workers: &WorkerPool| {
            workers.start_history_export(reports.clone(), exports.path().to_path_buf())
        };

        start(&workers).expect("the first export should start");
        assert!(workers.history_export_running());
        assert!(
            matches!(start(&workers), Err(HistoryExportError::Busy)),
            "a second export is refused while the first one's outcome is uncollected"
        );

        let outcome = collect_export(&workers);
        assert_eq!(outcome.exported, 1);
        assert_eq!(
            outcome.result.expect("the export should be written"),
            exports.path().join("excise-deletion-history.json")
        );
        assert!(!workers.history_export_running());

        start(&workers).expect("collecting the outcome frees the pool for the next export");
        let next = collect_export(&workers);
        assert_eq!(
            next.result.expect("the next export should be written"),
            exports.path().join("excise-deletion-history-1.json")
        );
        workers.shutdown().expect("workers should stop");
    }

    #[test]
    fn shutdown_joins_an_export_that_nobody_collected() {
        let root = tempfile::tempdir().expect("scan root should exist");
        let exports = tempfile::tempdir().expect("export directory should exist");
        let workers = WorkerPool::start(options(root.path(), 1), 1).expect("workers should start");
        workers
            .start_history_export(vec![history_report("one")], exports.path().to_path_buf())
            .expect("the export should start");

        workers
            .shutdown()
            .expect("shutdown should join the export thread");
    }
}
