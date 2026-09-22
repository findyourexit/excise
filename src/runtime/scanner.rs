mod scheduler;
mod task_queue;
mod task_spill;
#[cfg(windows)]
use std::ffi::OsString;
use std::fs::{self, File, Metadata};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use cap_primitives::ambient_authority;
use cap_primitives::fs::{self as cap_fs};
use crossbeam_channel::{
    Receiver, RecvTimeoutError, SendTimeoutError, Sender, TrySendError, bounded,
};
use ignore::gitignore::{Gitignore, GitignoreBuilder};

use scheduler::SchedulerHandle;
use scheduler::{LeasedDirectoryTask, SchedulerCommand};
use task_queue::{DirectoryTask, TASK_QUEUE_PER_WORKER, TaskQueue};

use super::worker::{ScannedEntry, WorkerEvent, send_event};
use crate::model::{ByteBounds, EntrySnapshot, NodeKind, UnscannedReason};
use crate::native_path::{NativeIdentity, identity_for};
use crate::os::physical_size;
use crate::scan_coordinator::{
    RelativePath, ScanGeneration, SessionCoordinator, WorkCompletion, WorkLease,
};
use crate::scan_session::ScanSessionId;
use crate::scan_store::identity_observation::IdentityObservation;
use crate::scan_store::path_reducer::{Coverage, PathEntryKind, PathObservation, SummaryMetrics};
use crate::scan_store::run_file::SealedRun;
use crate::scan_store::session::ScanInputRunFactory;
use crate::temporary_storage::TemporaryStorage;
/// The owner consumes each bounded batch before checking input again. Keep the
/// batch small so scanning never makes keyboard feedback wait indefinitely.
const MAX_FOCUS_REQUESTS: usize = 32;
const BATCH_SIZE: usize = 32;
const SCANNER_COMMAND_RETRY: Duration = Duration::from_millis(25);

#[derive(Clone, Debug)]
pub struct ScannerOptions {
    pub root: PathBuf,
    pub session: ScanSessionId,
    pub root_identity: Option<NativeIdentity>,
    pub generation: ScanGeneration,
    pub coordinator: SessionCoordinator,
    pub threads: usize,
    pub cross_filesystems: bool,
    pub exclusions: Vec<String>,
    pub internal_paths: Vec<PathBuf>,
    pub temporary_storage: TemporaryStorage,
    pub input_runs: Option<ScanInputRunFactory>,
}

struct Exclusions {
    matcher: Gitignore,
    rules: Vec<(String, Gitignore)>,
    internal_paths: Vec<PathBuf>,
}

impl Exclusions {
    fn new(
        root: &Path,
        patterns: Vec<String>,
        internal_paths: Vec<PathBuf>,
    ) -> Result<Self, String> {
        if let Some(path) = internal_paths.iter().find(|path| !path.is_absolute()) {
            return Err(format!(
                "internal scanner path must be absolute: {}",
                path.to_string_lossy()
            ));
        }
        let matcher = crate::config::compile_exclusions(root, &patterns)
            .map_err(|error| error.to_string())?;
        let mut rules = Vec::with_capacity(patterns.len());
        for pattern in patterns {
            let mut rule = GitignoreBuilder::new(root);
            rule.add_line(None, &pattern)
                .map_err(|error| error.to_string())?;
            rules.push((pattern, rule.build().map_err(|error| error.to_string())?));
        }
        Ok(Self {
            matcher,
            rules,
            internal_paths,
        })
    }

    fn is_internal(&self, path: &Path) -> bool {
        self.internal_paths
            .iter()
            .any(|internal_path| path == internal_path || path.starts_with(internal_path))
    }

    fn reason(&self, path: &Path, is_dir: bool) -> Option<String> {
        if self.is_internal(path) {
            return Some("Excise session state".to_string());
        }
        if !self
            .matcher
            .matched_path_or_any_parents(path, is_dir)
            .is_ignore()
        {
            return None;
        }
        self.rules
            .iter()
            .rev()
            .find(|(_, matcher)| {
                matcher
                    .matched_path_or_any_parents(path, is_dir)
                    .is_ignore()
            })
            .map_or_else(
                || Some("configured exclusion".to_string()),
                |(pattern, _)| Some(pattern.clone()),
            )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ScannerRequestError {
    Busy,
    Disconnected,
}

enum ScannerCommand {
    Rebuild {
        options: ScannerOptions,
        cancelled: Arc<AtomicBool>,
    },
}

/// Bounded control for the one persistent scanner worker.
///
/// Initial scans and invalidation-triggered root rebuilds execute serially on
/// the same service. A rebuild has its own cancellation flag so it never
/// interrupts the application's lifetime or an unrelated generation.
#[derive(Clone)]
pub(super) struct ScannerHandle {
    commands: Sender<ScannerCommand>,
    scheduler: Arc<Mutex<Option<SchedulerHandle>>>,
    rebuild_cancellation: Arc<Mutex<Option<Arc<AtomicBool>>>>,
}

impl ScannerHandle {
    pub(super) fn prioritize(&self, path: &Path) {
        let scheduler = self
            .scheduler
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(scheduler) = scheduler {
            scheduler.prioritize(path);
        }
    }

    pub(super) fn request_rebuild(
        &self,
        options: ScannerOptions,
    ) -> Result<(), ScannerRequestError> {
        self.enqueue_rebuild(options, false)
    }

    pub(super) fn request_pre_cancelled_rebuild(
        &self,
        options: ScannerOptions,
    ) -> Result<(), ScannerRequestError> {
        self.enqueue_rebuild(options, true)
    }

    fn enqueue_rebuild(
        &self,
        options: ScannerOptions,
        initially_cancelled: bool,
    ) -> Result<(), ScannerRequestError> {
        let cancelled = Arc::new(AtomicBool::new(initially_cancelled));
        let mut active = self
            .rebuild_cancellation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if active.is_some() {
            return Err(ScannerRequestError::Busy);
        }
        match self.commands.try_send(ScannerCommand::Rebuild {
            options,
            cancelled: Arc::clone(&cancelled),
        }) {
            Ok(()) => {
                *active = Some(cancelled);
                Ok(())
            }
            Err(TrySendError::Full(_)) => Err(ScannerRequestError::Busy),
            Err(TrySendError::Disconnected(_)) => Err(ScannerRequestError::Disconnected),
        }
    }

    pub(super) fn cancel_rebuild(&self) {
        let cancellation = self
            .rebuild_cancellation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(cancellation) = cancellation {
            cancellation.store(true, Ordering::Release);
        }
    }

    pub(super) fn complete_rebuild(&self) {
        *self
            .rebuild_cancellation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    fn clear_rebuild_cancellation(&self, completed: &Arc<AtomicBool>) {
        let mut active = self
            .rebuild_cancellation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if active
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, completed))
        {
            *active = None;
        }
    }
}

pub(super) fn spawn(
    options: ScannerOptions,
    sender: Sender<WorkerEvent>,
    cancelled: Arc<AtomicBool>,
) -> Result<(thread::JoinHandle<()>, ScannerHandle), std::io::Error> {
    let (commands, command_receiver) = bounded(1);
    let scanner = ScannerHandle {
        commands,
        scheduler: Arc::new(Mutex::new(None)),
        rebuild_cancellation: Arc::new(Mutex::new(None)),
    };
    let service = scanner.clone();
    let handle = thread::Builder::new()
        .name("excise-scanner".to_string())
        .spawn(move || {
            run_generation(
                options,
                &sender,
                cancelled.as_ref(),
                cancelled.as_ref(),
                &service.scheduler,
            );
            loop {
                if cancelled.load(Ordering::Acquire) {
                    return;
                }
                let command = match command_receiver.recv_timeout(SCANNER_COMMAND_RETRY) {
                    Ok(command) => command,
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => return,
                };
                let ScannerCommand::Rebuild {
                    options,
                    cancelled: rebuild_cancelled,
                } = command;
                run_generation(
                    options,
                    &sender,
                    rebuild_cancelled.as_ref(),
                    cancelled.as_ref(),
                    &service.scheduler,
                );
                service.clear_rebuild_cancellation(&rebuild_cancelled);
            }
        })?;
    Ok((handle, scanner))
}

fn run_generation(
    options: ScannerOptions,
    sender: &Sender<WorkerEvent>,
    cancelled: &AtomicBool,
    service_cancelled: &AtomicBool,
    active_scheduler: &Mutex<Option<SchedulerHandle>>,
) {
    if cancelled.load(Ordering::Acquire) {
        let _ = send_event(
            sender,
            WorkerEvent::ScanFinished { cancelled: true },
            service_cancelled,
        );
        return;
    }

    let command_capacity = options.threads.saturating_mul(BATCH_SIZE).max(1);
    let (commands, command_receiver) = bounded(command_capacity);
    let (focus, focus_receiver) = bounded(MAX_FOCUS_REQUESTS);
    let scheduler = SchedulerHandle::new(focus);
    *active_scheduler
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(scheduler);
    let root = options.root.clone();
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_with_scheduler(
            options,
            sender,
            cancelled,
            commands,
            command_receiver,
            focus_receiver,
        );
    }))
    .is_err()
    {
        let _ = send_event(
            sender,
            WorkerEvent::ScanFailed {
                path: Some(root),
                message: "scanner worker panicked".to_string(),
            },
            service_cancelled,
        );
        let _ = send_event(
            sender,
            WorkerEvent::ScanFinished { cancelled: true },
            service_cancelled,
        );
    }
}

#[cfg(test)]
fn run(options: ScannerOptions, sender: &Sender<WorkerEvent>, cancelled: &AtomicBool) {
    let command_capacity = options.threads.saturating_mul(BATCH_SIZE).max(1);
    let (commands, command_receiver) = bounded(command_capacity);
    let (focus, focus_receiver) = bounded(MAX_FOCUS_REQUESTS);
    run_with_scheduler(
        options,
        sender,
        cancelled,
        commands,
        command_receiver,
        focus_receiver,
    );
    drop(focus);
}

#[allow(clippy::too_many_lines)]
fn run_with_scheduler(
    options: ScannerOptions,
    sender: &Sender<WorkerEvent>,
    cancelled: &AtomicBool,
    scheduler_commands: Sender<SchedulerCommand>,
    scheduler_receiver: Receiver<SchedulerCommand>,
    focus_receiver: Receiver<PathBuf>,
) {
    let ScannerOptions {
        session,
        generation,
        coordinator,
        root,
        root_identity,
        threads,
        cross_filesystems,
        exclusions: exclusion_patterns,
        internal_paths,
        temporary_storage,
        input_runs,
    } = options;
    if input_runs
        .as_ref()
        .is_some_and(|factory| factory.generation() != generation)
    {
        let _ = send_event(
            sender,
            WorkerEvent::ScanFailed {
                path: Some(root.clone()),
                message: "scanner input-run factory targets a different generation".to_string(),
            },
            cancelled,
        );
        let _ = send_event(
            sender,
            WorkerEvent::ScanFinished { cancelled: false },
            cancelled,
        );
        return;
    }
    if let Err(message) = validate_scan_root(&root, root_identity.as_ref()) {
        let _ = send_event(
            sender,
            WorkerEvent::ScanFailed {
                path: Some(root.clone()),
                message,
            },
            cancelled,
        );
        let _ = send_event(
            sender,
            WorkerEvent::ScanFinished { cancelled: false },
            cancelled,
        );
        return;
    }
    let mut exclusions = match Exclusions::new(&root, exclusion_patterns, internal_paths) {
        Ok(exclusions) => exclusions,
        Err(message) => {
            let _ = send_event(
                sender,
                WorkerEvent::ScanFailed {
                    path: Some(root),
                    message,
                },
                cancelled,
            );
            let _ = send_event(
                sender,
                WorkerEvent::ScanFinished { cancelled: false },
                cancelled,
            );
            return;
        }
    };
    let root_filesystem = if cross_filesystems {
        None
    } else {
        match filesystem_key(&root) {
            Ok(key) => Some(key),
            Err(error) => {
                let _ = send_event(
                    sender,
                    WorkerEvent::ScanFailed {
                        path: Some(root.clone()),
                        message: format!("could not identify root filesystem: {error}"),
                    },
                    cancelled,
                );
                let _ = send_event(
                    sender,
                    WorkerEvent::ScanFinished { cancelled: false },
                    cancelled,
                );
                return;
            }
        }
    };
    let root_directory = match cap_fs::open_ambient_dir(&root, ambient_authority()) {
        Ok(directory) => directory,
        Err(error) => {
            let _ = send_event(
                sender,
                WorkerEvent::ScanFailed {
                    path: Some(root.clone()),
                    message: format!("could not open scan root: {error}"),
                },
                cancelled,
            );
            let _ = send_event(
                sender,
                WorkerEvent::ScanFinished { cancelled: false },
                cancelled,
            );
            return;
        }
    };
    if let Err(error) = validate_root_handle(&root_directory, root_identity.as_ref()) {
        let _ = send_event(
            sender,
            WorkerEvent::ScanFailed {
                path: Some(root.clone()),
                message: format!("could not validate opened scan root: {error}"),
            },
            cancelled,
        );
        let _ = send_event(
            sender,
            WorkerEvent::ScanFinished { cancelled: false },
            cancelled,
        );
        return;
    }
    let (queue, task_spill_path) = match TaskQueue::new(
        root.clone(),
        threads.saturating_mul(TASK_QUEUE_PER_WORKER).max(1),
        &temporary_storage,
    ) {
        Ok(queue) => queue,
        Err(error) => {
            let _ = send_event(
                sender,
                WorkerEvent::ScanFailed {
                    path: Some(root.clone()),
                    message: format!("could not create private scanner task spill: {error}"),
                },
                cancelled,
            );
            let _ = send_event(
                sender,
                WorkerEvent::ScanFinished { cancelled: false },
                cancelled,
            );
            return;
        }
    };
    let queue = Arc::new(queue);
    if let Some(task_spill_path) = task_spill_path {
        if !task_spill_path.is_absolute() {
            let _ = send_event(
                sender,
                WorkerEvent::ScanFailed {
                    path: Some(root.clone()),
                    message: "scanner task spill path must be absolute".to_string(),
                },
                cancelled,
            );
            let _ = send_event(
                sender,
                WorkerEvent::ScanFinished { cancelled: false },
                cancelled,
            );
            return;
        }
        exclusions.internal_paths.push(task_spill_path);
    }
    let root_invalid = AtomicBool::new(false);
    let scan_failed = AtomicBool::new(false);
    thread::scope(|scope| {
        let mut assignment_senders = Vec::with_capacity(threads);
        let mut assignment_receivers = Vec::with_capacity(threads);
        let mut worker_event_senders = Vec::with_capacity(threads);
        let mut worker_event_receivers = Vec::with_capacity(threads);
        for _ in 0..threads {
            let (assignment_sender, assignment_receiver) = bounded(1);
            let (worker_event_sender, worker_event_receiver) = bounded(1);
            assignment_senders.push(assignment_sender);
            assignment_receivers.push(assignment_receiver);
            worker_event_senders.push(worker_event_sender);
            worker_event_receivers.push(worker_event_receiver);
        }

        let scheduler_queue = Arc::clone(&queue);
        let scheduler_root = &root;
        let scheduler_cancelled = cancelled;
        let scheduler_failed = &scan_failed;
        let scheduler_root_invalid = &root_invalid;
        let scheduler_events = sender;
        let scheduler_coordinator = coordinator.clone();
        let scheduler = match thread::Builder::new()
            .name("excise-scan-coordinator".to_string())
            .spawn_scoped(scope, move || {
                scheduler::run(
                    scheduler_queue,
                    scheduler_root,
                    session,
                    generation,
                    scheduler_coordinator,
                    scheduler_receiver,
                    focus_receiver,
                    assignment_senders,
                    worker_event_receivers,
                    scheduler_cancelled,
                    scheduler_failed,
                    scheduler_root_invalid,
                    scheduler_events,
                );
            }) {
            Ok(handle) => handle,
            Err(error) => {
                scan_failed.store(true, Ordering::Release);
                let _ = send_event(
                    sender,
                    WorkerEvent::ScanFailed {
                        path: Some(root.clone()),
                        message: format!("could not spawn scan coordinator: {error}"),
                    },
                    cancelled,
                );
                return;
            }
        };

        for (index, (worker_assignments, worker_events)) in assignment_receivers
            .into_iter()
            .zip(worker_event_senders)
            .enumerate()
        {
            let worker_commands = scheduler_commands.clone();
            let worker_cancelled = cancelled;
            let worker_failed = &scan_failed;
            let worker_root_invalid = &root_invalid;
            let worker_root_directory = &root_directory;
            let worker_exclusions = &exclusions;
            let worker_root_filesystem = root_filesystem.as_ref();
            let worker_root = &root;
            let worker_root_identity = root_identity.as_ref();
            let worker_input_runs = input_runs.as_ref();
            if let Err(error) = thread::Builder::new()
                .name(format!("excise-scan-{index}"))
                .spawn_scoped(scope, move || {
                    scan_worker(
                        index,
                        &worker_assignments,
                        &worker_commands,
                        &worker_events,
                        worker_cancelled,
                        worker_failed,
                        worker_root_invalid,
                        worker_root_directory,
                        worker_root,
                        worker_root_identity,
                        worker_input_runs,
                        worker_exclusions,
                        worker_root_filesystem,
                        cross_filesystems,
                    );
                })
            {
                scan_failed.store(true, Ordering::Release);
                let _ = send_event(
                    sender,
                    WorkerEvent::ScanFailed {
                        path: Some(root.clone()),
                        message: format!("could not spawn scanner worker: {error}"),
                    },
                    cancelled,
                );
            }
        }
        drop(scheduler_commands);
        if scheduler.join().is_err() {
            scan_failed.store(true, Ordering::Release);
            let _ = send_event(
                sender,
                WorkerEvent::ScanFailed {
                    path: Some(root.clone()),
                    message: "scan coordinator thread panicked".to_string(),
                },
                cancelled,
            );
        }
    });
    let completion_delivery = AtomicBool::new(false);
    let _ = send_event(
        sender,
        WorkerEvent::ScanFinished {
            cancelled: cancelled.load(Ordering::Acquire)
                && !scan_failed.load(Ordering::Acquire)
                && !root_invalid.load(Ordering::Acquire),
        },
        &completion_delivery,
    );
}

struct ScanFrame {
    task: DirectoryTask,
    _directory: File,
    entries: cap_fs::ReadDir,
    batch: Vec<ScannedEntry>,
    directories: Vec<DirectoryTask>,
}

struct ScanDirectoryOutcome {
    continue_scanning: bool,
    completed: bool,
}
#[allow(
    clippy::too_many_arguments,
    reason = "The worker receives its bounded queue, cancellation state, root identity, and scan policy explicitly."
)]
fn scan_worker(
    worker: usize,
    assignments: &Receiver<LeasedDirectoryTask>,
    scheduler: &Sender<SchedulerCommand>,
    events: &Sender<WorkerEvent>,
    cancelled: &AtomicBool,
    failed: &AtomicBool,
    root_invalid: &AtomicBool,
    root_directory: &File,
    root: &Path,
    root_identity: Option<&NativeIdentity>,
    input_runs: Option<&ScanInputRunFactory>,
    exclusions: &Exclusions,
    root_filesystem: Option<&FilesystemKey>,
    cross_filesystems: bool,
) {
    loop {
        if cancelled.load(Ordering::Acquire)
            || failed.load(Ordering::Acquire)
            || root_invalid.load(Ordering::Acquire)
        {
            return;
        }
        let assignment = match assignments.recv_timeout(Duration::from_millis(25)) {
            Ok(assignment) => assignment,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        let lease = assignment.lease.clone();
        let outcome = scan_directory(
            assignment.task,
            Some(&lease),
            scheduler,
            events,
            cancelled,
            failed,
            root_invalid,
            root_directory,
            root,
            input_runs,
            root_identity,
            exclusions,
            root_filesystem,
            cross_filesystems,
        );
        let work_completion = if outcome.completed {
            WorkCompletion::Succeeded
        } else if root_invalid.load(Ordering::Acquire) {
            WorkCompletion::Invalidated
        } else if failed.load(Ordering::Acquire) || outcome.continue_scanning {
            WorkCompletion::Failed
        } else {
            WorkCompletion::Cancelled
        };
        if !send_scheduler_command(
            scheduler,
            SchedulerCommand::Complete {
                worker,
                lease: assignment.lease,
                work_completion,
            },
            cancelled,
        ) {
            failed.store(true, Ordering::Release);
            return;
        }
        if !outcome.continue_scanning {
            if !root_invalid.load(Ordering::Acquire)
                && !failed.load(Ordering::Acquire)
                && !cancelled.load(Ordering::Acquire)
            {
                cancelled.store(true, Ordering::Release);
            }
            return;
        }
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn scan_directory(
    task: DirectoryTask,
    lease: Option<&WorkLease>,
    scheduler: &Sender<SchedulerCommand>,
    sender: &Sender<WorkerEvent>,
    cancelled: &AtomicBool,
    failed: &AtomicBool,
    root_invalid: &AtomicBool,
    root_directory: &File,
    root: &Path,
    input_runs: Option<&ScanInputRunFactory>,
    root_identity: Option<&NativeIdentity>,
    exclusions: &Exclusions,
    root_filesystem: Option<&FilesystemKey>,
    cross_filesystems: bool,
) -> ScanDirectoryOutcome {
    if cancelled.load(Ordering::Acquire)
        || failed.load(Ordering::Acquire)
        || root_invalid.load(Ordering::Acquire)
    {
        return ScanDirectoryOutcome {
            continue_scanning: false,
            completed: false,
        };
    }
    if !validate_root_for_traversal(root, root_identity, sender, cancelled, root_invalid) {
        return ScanDirectoryOutcome {
            continue_scanning: false,
            completed: false,
        };
    }
    let mut frame = match open_frame(root_directory, root, task) {
        Ok(frame) => frame,
        Err(error) => {
            let (task, error) = *error;
            report_directory_task_error(task.path, error, lease, sender, cancelled);
            return ScanDirectoryOutcome {
                continue_scanning: true,
                completed: false,
            };
        }
    };
    if let Err(error) = validate_directory_task(root_directory, root, &frame.task) {
        report_directory_task_error(frame.task.path.clone(), error, lease, sender, cancelled);
        return ScanDirectoryOutcome {
            continue_scanning: true,
            completed: false,
        };
    }
    if !validate_root_for_traversal(root, root_identity, sender, cancelled, root_invalid) {
        return ScanDirectoryOutcome {
            continue_scanning: false,
            completed: false,
        };
    }
    loop {
        if cancelled.load(Ordering::Acquire)
            || failed.load(Ordering::Acquire)
            || root_invalid.load(Ordering::Acquire)
        {
            return ScanDirectoryOutcome {
                continue_scanning: false,
                completed: false,
            };
        }
        if frame.batch.len() == BATCH_SIZE {
            if !flush_frame(
                &mut frame, lease, root, input_runs, scheduler, sender, cancelled, failed,
            ) {
                return ScanDirectoryOutcome {
                    continue_scanning: false,
                    completed: false,
                };
            }
            if !validate_root_for_traversal(root, root_identity, sender, cancelled, root_invalid) {
                return ScanDirectoryOutcome {
                    continue_scanning: false,
                    completed: false,
                };
            }
            if let Err(error) = validate_directory_task(root_directory, root, &frame.task) {
                report_directory_task_error(
                    frame.task.path.clone(),
                    error,
                    lease,
                    sender,
                    cancelled,
                );
                return ScanDirectoryOutcome {
                    continue_scanning: true,
                    completed: false,
                };
            }
        }
        match frame.entries.next() {
            Some(Ok(entry)) => {
                process_entry(
                    &mut frame,
                    &entry,
                    root,
                    lease,
                    input_runs,
                    sender,
                    cancelled,
                    failed,
                    exclusions,
                    root_filesystem,
                    cross_filesystems,
                );
            }
            Some(Err(error)) => {
                let _ = send_event(
                    sender,
                    WorkerEvent::ScanFailed {
                        path: Some(frame.task.path.clone()),
                        message: error.to_string(),
                    },
                    cancelled,
                );
            }
            None => break,
        }
    }
    if !validate_root_for_traversal(root, root_identity, sender, cancelled, root_invalid) {
        return ScanDirectoryOutcome {
            continue_scanning: false,
            completed: false,
        };
    }
    if !frame.batch.is_empty()
        && !flush_frame(
            &mut frame, lease, root, input_runs, scheduler, sender, cancelled, failed,
        )
    {
        return ScanDirectoryOutcome {
            continue_scanning: false,
            completed: false,
        };
    }
    if !validate_root_for_traversal(root, root_identity, sender, cancelled, root_invalid) {
        return ScanDirectoryOutcome {
            continue_scanning: false,
            completed: false,
        };
    }
    if let Err(error) = validate_directory_task(root_directory, root, &frame.task) {
        report_directory_task_error(frame.task.path.clone(), error, lease, sender, cancelled);
        return ScanDirectoryOutcome {
            continue_scanning: true,
            completed: false,
        };
    }
    ScanDirectoryOutcome {
        continue_scanning: true,
        completed: true,
    }
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn scan_directory_with_queue(
    task: DirectoryTask,
    queue: &TaskQueue,
    sender: &Sender<WorkerEvent>,
    cancelled: &AtomicBool,
    failed: &AtomicBool,
    root_invalid: &AtomicBool,
    root_directory: &File,
    root: &Path,
    root_identity: Option<&NativeIdentity>,
    exclusions: &Exclusions,
    root_filesystem: Option<&FilesystemKey>,
    cross_filesystems: bool,
) -> bool {
    let (scheduler, commands) = bounded(BATCH_SIZE);
    let outcome = scan_directory(
        task,
        None,
        &scheduler,
        sender,
        cancelled,
        failed,
        root_invalid,
        root_directory,
        root,
        None,
        root_identity,
        exclusions,
        root_filesystem,
        cross_filesystems,
    );
    drop(scheduler);
    for command in commands.try_iter() {
        match command {
            SchedulerCommand::Enqueue(task) => {
                if let Err(error) = queue.schedule(task) {
                    failed.store(true, Ordering::Release);
                    let _ = send_event(
                        sender,
                        WorkerEvent::ScanFailed {
                            path: None,
                            message: format!("could not queue scanner directory: {error}"),
                        },
                        cancelled,
                    );
                    return false;
                }
            }
            SchedulerCommand::Complete { .. } => {
                panic!("direct scan helper cannot receive a task completion")
            }
        }
    }
    outcome.continue_scanning
}

fn validate_root_for_traversal(
    root: &Path,
    root_identity: Option<&NativeIdentity>,
    sender: &Sender<WorkerEvent>,
    cancelled: &AtomicBool,
    root_invalid: &AtomicBool,
) -> bool {
    if root_invalid.load(Ordering::Acquire) {
        return false;
    }
    let Err(message) = validate_scan_root(root, root_identity) else {
        return true;
    };
    let message = if message == "scan root identity changed before scanning" {
        "scan root identity changed during traversal".to_string()
    } else {
        format!("scan root became invalid during traversal: {message}")
    };
    if root_invalid
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        let _ = send_event(
            sender,
            WorkerEvent::ScanFailed {
                path: Some(root.to_path_buf()),
                message,
            },
            cancelled,
        );
    }
    false
}

enum DirectoryTaskError {
    Replaced(String),
    Io(io::Error),
}

fn validate_directory_task(
    root_directory: &File,
    root: &Path,
    task: &DirectoryTask,
) -> Result<(), DirectoryTaskError> {
    let Some(expected) = task.identity.as_ref() else {
        return Ok(());
    };
    let directory = open_scan_directory(root_directory, root, &task.path)
        .map_err(|error| classify_directory_open_error(&task.path, error))?;
    let metadata = cap_fs::Metadata::from_file(&directory).map_err(DirectoryTaskError::Io)?;
    if !metadata.is_dir() || metadata.is_symlink() {
        return Err(DirectoryTaskError::Replaced(
            "scanner directory task was replaced by a symbolic link or non-directory".to_string(),
        ));
    }
    let actual = identity_from_entry_metadata(&metadata)
        .map_err(DirectoryTaskError::Io)?
        .ok_or_else(|| {
            DirectoryTaskError::Replaced(
                "scanner directory task identity is unavailable".to_string(),
            )
        })?;
    if !same_identity(expected, &actual) || actual.reparse_point {
        return Err(DirectoryTaskError::Replaced(
            "scanner directory task identity changed before traversal".to_string(),
        ));
    }
    #[cfg(test)]
    maybe_replace_after_validation(&task.path);
    Ok(())
}

#[cfg(all(test, unix))]
static VALIDATION_REPLACEMENT: std::sync::OnceLock<Mutex<Option<(PathBuf, PathBuf, PathBuf)>>> =
    std::sync::OnceLock::new();
#[cfg(all(test, unix))]
static BATCH_REPLACEMENT: std::sync::OnceLock<Mutex<Option<(PathBuf, PathBuf, PathBuf)>>> =
    std::sync::OnceLock::new();

#[cfg(test)]
fn maybe_replace_after_validation(path: &Path) {
    #[cfg(unix)]
    {
        let replacement = VALIDATION_REPLACEMENT.get_or_init(|| Mutex::new(None));
        let Some((expected_path, displaced_path, target_path)) = replacement
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        else {
            return;
        };
        if expected_path != path {
            let mut pending = replacement
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *pending = Some((expected_path, displaced_path, target_path));
            return;
        }
        fs::rename(path, displaced_path).expect("original directory should be displaced");
        std::os::unix::fs::symlink(target_path, path)
            .expect("replacement symlink should be created");
    }
    #[cfg(not(unix))]
    let _ = path;
}

#[cfg(test)]
fn maybe_replace_after_batch(path: &Path) {
    #[cfg(unix)]
    {
        let replacement = BATCH_REPLACEMENT.get_or_init(|| Mutex::new(None));
        let mut pending = replacement
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some((expected_path, displaced_path, target_path)) = pending.take() else {
            return;
        };
        if expected_path != path {
            *pending = Some((expected_path, displaced_path, target_path));
            return;
        }
        drop(pending);
        fs::rename(path, displaced_path).expect("original directory should be displaced");
        std::os::unix::fs::symlink(target_path, path)
            .expect("replacement symlink should be created");
    }
    #[cfg(not(unix))]
    let _ = path;
}

#[cfg(all(test, unix))]
fn replace_after_next_validation(path: PathBuf, displaced: PathBuf, target: PathBuf) {
    *VALIDATION_REPLACEMENT
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((path, displaced, target));
}

#[cfg(all(test, unix))]
pub(super) fn replace_after_next_batch(path: PathBuf, displaced: PathBuf, target: PathBuf) {
    *BATCH_REPLACEMENT
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((path, displaced, target));
}
fn report_directory_task_error(
    path: PathBuf,
    error: DirectoryTaskError,
    lease: Option<&WorkLease>,
    sender: &Sender<WorkerEvent>,
    cancelled: &AtomicBool,
) {
    match error {
        DirectoryTaskError::Replaced(message) => {
            let _ = send_event(
                sender,
                WorkerEvent::ScanUnscanned {
                    lease: lease.cloned(),
                    path,
                    reason: UnscannedReason::Replacement(message),
                    input_runs: Vec::new(),
                },
                cancelled,
            );
        }
        DirectoryTaskError::Io(error) => {
            let _ = send_event(
                sender,
                WorkerEvent::ScanFailed {
                    path: Some(path),
                    message: error.to_string(),
                },
                cancelled,
            );
        }
    }
}

fn validate_root_handle(
    root_directory: &File,
    expected: Option<&NativeIdentity>,
) -> io::Result<()> {
    let metadata = cap_fs::Metadata::from_file(root_directory)?;
    if !metadata.is_dir() || metadata.is_symlink() {
        return Err(io::Error::other("opened scan root is not a real directory"));
    }
    let Some(expected) = expected else {
        return Ok(());
    };
    let actual = identity_from_entry_metadata(&metadata)?
        .ok_or_else(|| io::Error::other("opened scan root identity is unavailable"))?;
    if !same_identity(expected, &actual) || actual.reparse_point {
        return Err(io::Error::other("opened scan root identity changed"));
    }
    Ok(())
}

#[cfg(windows)]
fn cap_metadata_is_reparse(metadata: &cap_fs::Metadata) -> bool {
    use cap_primitives::fs::_WindowsByHandle as _;

    metadata.file_attributes() & 0x0000_0400 != 0
}

#[cfg(not(windows))]
fn cap_metadata_is_reparse(metadata: &cap_fs::Metadata) -> bool {
    metadata.is_symlink()
}

fn open_scan_directory(root_directory: &File, root: &Path, path: &Path) -> io::Result<File> {
    let relative = task_relative_path(root, path)?;
    let mut directory = root_directory.try_clone()?;
    for component in relative.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        directory = cap_fs::open_dir_nofollow(&directory, Path::new(name))?;
        let metadata = cap_fs::Metadata::from_file(&directory)?;
        if cap_metadata_is_reparse(&metadata) {
            return Err(io::Error::other(
                "scanner path component is a symbolic link or reparse point",
            ));
        }
    }
    Ok(directory)
}

fn task_relative_path(root: &Path, path: &Path) -> io::Result<PathBuf> {
    let relative = path.strip_prefix(root).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "scanner task path is outside the scan root",
        )
    })?;
    if relative.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "scanner task path is not a safe relative path",
        ));
    }
    Ok(relative.to_path_buf())
}

#[cfg(windows)]
fn metadata_is_reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;

    metadata.file_type().is_symlink() || metadata.file_attributes() & 0x0000_0400 != 0
}

#[cfg(not(windows))]
fn metadata_is_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

fn path_has_reparse_component(path: &Path) -> bool {
    let mut current = PathBuf::new();
    path.components().any(|component| {
        current.push(component);
        fs::symlink_metadata(&current).is_ok_and(|metadata| metadata_is_reparse(&metadata))
    })
}

fn classify_directory_open_error(path: &Path, error: io::Error) -> DirectoryTaskError {
    let is_non_directory = fs::symlink_metadata(path).is_ok_and(|metadata| !metadata.is_dir());
    if is_non_directory || path_has_reparse_component(path) {
        DirectoryTaskError::Replaced(
            "scanner directory task was replaced by a symbolic link or non-directory".to_string(),
        )
    } else {
        DirectoryTaskError::Io(error)
    }
}

fn same_identity(left: &NativeIdentity, right: &NativeIdentity) -> bool {
    left.file_id == right.file_id && left.reparse_point == right.reparse_point
}

fn open_frame(
    root_directory: &File,
    root: &Path,
    task: DirectoryTask,
) -> Result<ScanFrame, Box<(DirectoryTask, DirectoryTaskError)>> {
    let directory = match open_scan_directory(root_directory, root, &task.path) {
        Ok(directory) => directory,
        Err(error) => {
            return Err(Box::new((
                task.clone(),
                classify_directory_open_error(&task.path, error),
            )));
        }
    };
    let metadata = match cap_fs::Metadata::from_file(&directory) {
        Ok(metadata) => metadata,
        Err(error) => return Err(Box::new((task, DirectoryTaskError::Io(error)))),
    };
    if !metadata.is_dir() || metadata.is_symlink() {
        return Err(Box::new((
            task,
            DirectoryTaskError::Replaced(
                "scanner directory task was replaced by a symbolic link or non-directory"
                    .to_string(),
            ),
        )));
    }
    let actual = match identity_from_entry_metadata(&metadata) {
        Ok(Some(identity)) => identity,
        Ok(None) => {
            return Err(Box::new((
                task,
                DirectoryTaskError::Replaced(
                    "scanner directory task identity is unavailable".to_string(),
                ),
            )));
        }
        Err(error) => return Err(Box::new((task, DirectoryTaskError::Io(error)))),
    };
    if actual.reparse_point
        || task
            .identity
            .as_ref()
            .is_some_and(|expected| !same_identity(expected, &actual))
    {
        return Err(Box::new((
            task,
            DirectoryTaskError::Replaced(
                "scanner directory task identity changed before traversal".to_string(),
            ),
        )));
    }
    let entries = match cap_fs::read_base_dir(&directory) {
        Ok(entries) => entries,
        Err(error) => return Err(Box::new((task, DirectoryTaskError::Io(error)))),
    };
    Ok(ScanFrame {
        task,
        _directory: directory,
        entries,
        batch: Vec::with_capacity(BATCH_SIZE),
        directories: Vec::with_capacity(BATCH_SIZE),
    })
}

#[allow(clippy::unnecessary_wraps)]
fn identity_from_entry_metadata(metadata: &cap_fs::Metadata) -> io::Result<Option<NativeIdentity>> {
    #[cfg(unix)]
    {
        use cap_primitives::fs::MetadataExt as _;
        Ok(Some(NativeIdentity {
            file_id: file_id::FileId::new_inode(metadata.dev(), metadata.ino()),
            link_count: Some(metadata.nlink()),
            reparse_point: metadata.is_symlink(),
        }))
    }
    #[cfg(windows)]
    {
        use cap_primitives::fs::_WindowsByHandle as _;
        let volume = metadata.volume_serial_number().ok_or_else(|| {
            io::Error::other("directory entry did not expose a volume serial number")
        })?;
        let index = metadata
            .file_index()
            .ok_or_else(|| io::Error::other("directory entry did not expose a file index"))?;
        Ok(Some(NativeIdentity {
            file_id: file_id::FileId::new_low_res(volume, index),
            link_count: metadata.number_of_links().map(u64::from),
            reparse_point: metadata.file_attributes() & 0x0000_0400 != 0,
        }))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = metadata;
        Ok(None)
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "the boundary event retains its complete scan-time identity and transport context"
)]
fn report_unscanned_entry(
    root: &Path,
    lease: Option<&WorkLease>,
    factory: Option<&ScanInputRunFactory>,
    metadata: &Metadata,
    path: PathBuf,
    identity: NativeIdentity,
    reason: UnscannedReason,
    sender: &Sender<WorkerEvent>,
    cancelled: &AtomicBool,
    failed: &AtomicBool,
) {
    let coverage = if matches!(reason, UnscannedReason::SymbolicLink) {
        Coverage::Complete
    } else {
        Coverage::Uncertain
    };
    let entry = ScannedEntry {
        metadata: metadata.clone(),
        path: path.clone(),
        identity,
    };
    let input_runs = match seal_scanned_entries(root, &[entry], coverage, factory) {
        Ok(input_runs) => input_runs,
        Err(message) => {
            failed.store(true, Ordering::Release);
            let _ = send_event(
                sender,
                WorkerEvent::ScanFailed {
                    path: Some(path),
                    message,
                },
                cancelled,
            );
            return;
        }
    };
    let _ = send_event(
        sender,
        WorkerEvent::ScanUnscanned {
            lease: lease.cloned(),
            path,
            reason,
            input_runs,
        },
        cancelled,
    );
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn process_entry(
    frame: &mut ScanFrame,
    entry: &cap_fs::DirEntry,
    root: &Path,
    lease: Option<&WorkLease>,
    input_runs: Option<&ScanInputRunFactory>,
    sender: &Sender<WorkerEvent>,
    cancelled: &AtomicBool,
    failed: &AtomicBool,
    exclusions: &Exclusions,
    root_filesystem: Option<&FilesystemKey>,
    cross_filesystems: bool,
) {
    let name = entry.file_name();
    let path = frame.task.path.join(&name);
    if exclusions.is_internal(&path) {
        return;
    }
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) => {
            let _ = send_event(
                sender,
                WorkerEvent::ScanFailed {
                    path: Some(path),
                    message: error.to_string(),
                },
                cancelled,
            );
            return;
        }
    };
    let identity = match identity_for(&path, &metadata) {
        Ok(Some(identity)) => identity,
        Ok(None) => {
            let _ = send_event(
                sender,
                WorkerEvent::ScanUnscanned {
                    lease: lease.cloned(),
                    path,
                    reason: UnscannedReason::Metadata(
                        "scan entry identity is unavailable".to_string(),
                    ),
                    input_runs: Vec::new(),
                },
                cancelled,
            );
            return;
        }
        Err(error) => {
            let message = format!(
                "{}: {error}",
                path.parent()
                    .unwrap_or_else(|| Path::new("."))
                    .to_string_lossy()
            );
            let _ = send_event(
                sender,
                WorkerEvent::ScanFailed {
                    path: Some(path),
                    message,
                },
                cancelled,
            );
            return;
        }
    };
    let is_dir = metadata.is_dir();
    if let Some(pattern) = exclusions.reason(&path, is_dir) {
        report_unscanned_entry(
            root,
            lease,
            input_runs,
            &metadata,
            path,
            identity,
            UnscannedReason::Excluded(pattern),
            sender,
            cancelled,
            failed,
        );
        return;
    }
    if metadata.file_type().is_symlink() || identity.reparse_point {
        report_unscanned_entry(
            root,
            lease,
            input_runs,
            &metadata,
            path,
            identity,
            UnscannedReason::SymbolicLink,
            sender,
            cancelled,
            failed,
        );
        return;
    }
    if is_dir && !cross_filesystems {
        let reason = match root_filesystem {
            Some(root_filesystem) => filesystem_boundary_reason(&path, &metadata, root_filesystem),
            None => Some(UnscannedReason::Metadata(
                "root filesystem identity is unavailable".to_string(),
            )),
        };
        if let Some(reason) = reason {
            report_unscanned_entry(
                root, lease, input_runs, &metadata, path, identity, reason, sender, cancelled,
                failed,
            );
            return;
        }
    }
    let directory = is_dir.then(|| DirectoryTask {
        path: path.clone(),
        identity: Some(identity.clone()),
    });
    frame.batch.push(ScannedEntry {
        metadata,
        path,
        identity,
    });
    if let Some(directory) = directory {
        frame.directories.push(directory);
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "batch sealing keeps every worker-to-actor cancellation boundary explicit"
)]
fn flush_frame(
    frame: &mut ScanFrame,
    lease: Option<&WorkLease>,
    root: &Path,
    input_runs: Option<&ScanInputRunFactory>,
    scheduler: &Sender<SchedulerCommand>,
    sender: &Sender<WorkerEvent>,
    cancelled: &AtomicBool,
    failed: &AtomicBool,
) -> bool {
    let entries = std::mem::replace(&mut frame.batch, Vec::with_capacity(BATCH_SIZE));
    let input_runs = match seal_scanned_entries(root, &entries, Coverage::Complete, input_runs) {
        Ok(input_runs) => input_runs,
        Err(message) => {
            failed.store(true, Ordering::Release);
            frame.directories.clear();
            let _ = send_event(
                sender,
                WorkerEvent::ScanFailed {
                    path: Some(frame.task.path.clone()),
                    message,
                },
                cancelled,
            );
            return false;
        }
    };
    if !send_event(
        sender,
        WorkerEvent::ScanBatch {
            lease: lease.cloned(),
            entries,
            input_runs,
        },
        cancelled,
    ) {
        frame.directories.clear();
        return false;
    }
    // Let replacement-race fixtures mutate only after the batch is observable.
    #[cfg(test)]
    maybe_replace_after_batch(&frame.task.path);
    for task in std::mem::take(&mut frame.directories) {
        if !send_scheduler_command(scheduler, SchedulerCommand::Enqueue(task), cancelled) {
            failed.store(true, Ordering::Release);
            let _ = send_event(
                sender,
                WorkerEvent::ScanFailed {
                    path: None,
                    message: "scanner coordinator disconnected".to_string(),
                },
                cancelled,
            );
            return false;
        }
    }
    true
}

fn seal_scanned_entries(
    root: &Path,
    entries: &[ScannedEntry],
    coverage: Coverage,
    factory: Option<&ScanInputRunFactory>,
) -> Result<Vec<SealedRun>, String> {
    let Some(factory) = factory else {
        return Ok(Vec::new());
    };
    let mut paths = Vec::with_capacity(entries.len());
    let mut identities = Vec::with_capacity(entries.len());
    for entry in entries {
        let relative = entry
            .path
            .strip_prefix(root)
            .map_err(|_| "scanner entry escaped its configured root".to_string())?;
        let relative = RelativePath::from_path(relative)
            .map_err(|_| "scanner entry has an invalid root-relative path".to_string())?;
        let kind = if entry.metadata.file_type().is_symlink() || entry.identity.reparse_point {
            PathEntryKind::Link
        } else if entry.metadata.is_dir() {
            PathEntryKind::Directory
        } else {
            PathEntryKind::File
        };
        let node_kind = match kind {
            PathEntryKind::Directory => NodeKind::Directory,
            PathEntryKind::File => NodeKind::File,
            PathEntryKind::Link => NodeKind::Link,
        };
        let allocated_bytes = (kind != PathEntryKind::Directory)
            .then(|| {
                physical_size(&entry.path, &entry.metadata)
                    .ok()
                    .map(u128::from)
            })
            .flatten();
        let direct_allocation =
            kind != PathEntryKind::Directory && entry.identity.link_count == Some(1);
        let physical_bounds = allocated_bytes.map_or_else(ByteBounds::unknown, ByteBounds::exact);
        let (allocated_bounds, reclaimable_bounds) = if direct_allocation {
            (physical_bounds, physical_bounds)
        } else {
            (ByteBounds::exact(0), ByteBounds::exact(0))
        };
        let snapshot = EntrySnapshot {
            identity: Some(entry.identity.clone()),
            kind: node_kind,
            apparent_bytes: if kind == PathEntryKind::Directory {
                0
            } else {
                u128::from(entry.metadata.len())
            },
            allocated_bytes,
            modified_nanos: entry
                .metadata
                .modified()
                .ok()
                .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|duration| duration.as_nanos()),
        };
        paths.push(PathObservation::with_snapshot(
            relative.clone(),
            kind,
            SummaryMetrics::leaf(
                snapshot.apparent_bytes,
                allocated_bounds,
                reclaimable_bounds,
            ),
            coverage,
            Some(snapshot),
        ));
        if kind != PathEntryKind::Directory && entry.identity.link_count != Some(1) {
            identities.push(IdentityObservation {
                path: relative,
                file_id: entry.identity.file_id,
                declared_links: entry.identity.link_count,
                allocated_bytes: allocated_bytes
                    .map_or_else(ByteBounds::unknown, ByteBounds::exact),
            });
        }
    }
    factory
        .seal_observation_batch(paths, identities)
        .map_err(|error| error.to_string())
}

fn send_scheduler_command(
    sender: &Sender<SchedulerCommand>,
    mut command: SchedulerCommand,
    cancelled: &AtomicBool,
) -> bool {
    loop {
        match sender.send_timeout(command, Duration::from_millis(25)) {
            Ok(()) => return true,
            Err(SendTimeoutError::Timeout(returned)) => {
                if cancelled.load(Ordering::Acquire) {
                    return false;
                }
                command = returned;
            }
            Err(SendTimeoutError::Disconnected(_)) => return false,
        }
    }
}
fn validate_scan_root(path: &Path, expected: Option<&NativeIdentity>) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        format!(
            "could not inspect scan root {}: {error}",
            path.to_string_lossy()
        )
    })?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("scan root was replaced by a symbolic link or non-directory".to_string());
    }
    let actual = identity_for(path, &metadata)
        .map_err(|error| format!("could not identify scan root: {error}"))?
        .ok_or_else(|| "scan root identity is unavailable".to_string())?;
    if actual.reparse_point {
        return Err("scan root was replaced by a reparse point".to_string());
    }
    if expected.is_some_and(|expected| {
        actual.file_id != expected.file_id || actual.reparse_point != expected.reparse_point
    }) {
        return Err("scan root identity changed before scanning".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scanner_reports_temporary_storage_exhaustion_without_committing_root() {
        use crossbeam_channel::unbounded;

        let root = tempfile::tempdir().expect("scan root should exist");
        for index in 0..=TASK_QUEUE_PER_WORKER.saturating_add(1) {
            fs::create_dir(root.path().join(format!("directory-{index}")))
                .expect("directory fixture should be created");
        }
        let (sender, events) = unbounded();
        let cancelled = AtomicBool::new(false);
        run(
            ScannerOptions {
                session: ScanSessionId::from_bytes([4; 16]),
                generation: ScanGeneration::initial(),
                coordinator: SessionCoordinator::start(
                    ScanSessionId::from_bytes([4; 16]),
                    ScanGeneration::initial(),
                )
                .expect("scanner coordinator should start"),
                root: root.path().to_path_buf(),
                root_identity: None,
                threads: 1,
                cross_filesystems: false,
                exclusions: Vec::new(),
                internal_paths: Vec::new(),
                temporary_storage: TemporaryStorage::scan_store_with_limit_bytes(0),
                input_runs: None,
            },
            &sender,
            &cancelled,
        );

        let mut saw_capacity_error = false;
        let mut finished = false;
        for event in events.try_iter() {
            match event {
                WorkerEvent::ScanFailed { message, .. } => {
                    saw_capacity_error |= message.contains("scan store capacity exhausted")
                        && message.contains("--scan-store-mib");
                }
                WorkerEvent::ScanFinished { cancelled } => finished |= !cancelled,
                WorkerEvent::ScanBatch { .. }
                | WorkerEvent::ScanUnscanned { .. }
                | WorkerEvent::DeletionPlanned { .. }
                | WorkerEvent::DeletionExecutionRejected { .. }
                | WorkerEvent::DeletionFinished { .. } => {}
            }
        }
        assert!(
            saw_capacity_error,
            "temporary-storage exhaustion should be actionable"
        );
        assert!(
            finished,
            "scanner should report a terminal event after failure"
        );
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the test validates every scanner event and canonical admission boundary"
    )]
    #[test]
    fn scanner_seals_worker_facts_for_lease_validated_store_admission() {
        use crossbeam_channel::unbounded;

        let root = tempfile::tempdir().expect("scan root should exist");
        let entry = root.path().join("entry");
        fs::write(&entry, b"payload").expect("scan fixture should exist");
        let excluded = root.path().join("excluded");
        fs::create_dir(&excluded).expect("excluded fixture should exist");
        let storage = TemporaryStorage::scan_store_with_limit_bytes(8 * 1024 * 1024);
        let mut store =
            crate::scan_store::session::ScanStore::new(ScanGeneration::initial(), storage.clone())
                .expect("scan store should initialize");
        let input_runs = store
            .input_run_factory()
            .expect("initial generation should accept input");
        let (sender, events) = unbounded();
        let cancelled = AtomicBool::new(false);
        run(
            ScannerOptions {
                session: store.session(),
                generation: input_runs.generation(),
                coordinator: SessionCoordinator::start(store.session(), input_runs.generation())
                    .expect("scanner coordinator should start"),
                root: root.path().to_path_buf(),
                root_identity: None,
                threads: 1,
                cross_filesystems: false,
                exclusions: vec!["excluded/".to_string()],
                internal_paths: Vec::new(),
                temporary_storage: storage,
                input_runs: Some(input_runs),
            },
            &sender,
            &cancelled,
        );

        let mut admitted_runs: usize = 0;
        let mut finished = false;
        for event in events.try_iter() {
            match event {
                WorkerEvent::ScanBatch {
                    lease: Some(lease),
                    input_runs,
                    ..
                } => {
                    admitted_runs = admitted_runs.saturating_add(input_runs.len());
                    for run in input_runs {
                        assert!(
                            store
                                .accept_leased_input_run(&lease, run)
                                .expect("lease-validated scanner run should be admitted")
                        );
                    }
                }
                WorkerEvent::ScanBatch { lease: None, .. } => {
                    panic!("scanner sealed an unleased batch")
                }
                WorkerEvent::ScanFinished { cancelled } => finished |= !cancelled,
                WorkerEvent::ScanFailed { message, .. } => panic!("scanner failed: {message}"),
                WorkerEvent::ScanUnscanned {
                    lease: Some(lease),
                    input_runs,
                    ..
                } => {
                    assert!(
                        !input_runs.is_empty(),
                        "known unscanned coverage must arrive in a sealed run"
                    );
                    admitted_runs = admitted_runs.saturating_add(input_runs.len());
                    for run in input_runs {
                        assert!(
                            store
                                .accept_leased_input_run(&lease, run)
                                .expect("lease-validated coverage run should be admitted")
                        );
                    }
                }
                WorkerEvent::ScanUnscanned { lease: None, .. } => {
                    panic!("scanner sealed an unleased coverage result")
                }
                WorkerEvent::DeletionPlanned { .. }
                | WorkerEvent::DeletionExecutionRejected { .. }
                | WorkerEvent::DeletionFinished { .. } => {}
            }
        }

        assert!(finished, "scanner should report normal completion");
        assert!(
            admitted_runs > 0,
            "scanner should seal at least one input run"
        );
        store
            .publish()
            .expect("admitted scanner facts should publish");
        let page = store
            .published()
            .expect("published generation should exist")
            .page(crate::scan_store::page::PageRequest::first(
                RelativePath::root(),
                8,
            ))
            .expect("published root page should load");
        assert_eq!(page.entries.len(), 2);
        assert_eq!(
            page.entries[0].path,
            RelativePath::from_path(Path::new("entry")).expect("fixture path should be relative")
        );
        assert!(
            page.entries.iter().any(|entry| entry.path
                == RelativePath::from_path(Path::new("excluded"))
                    .expect("fixture path should be relative")
                && entry.coverage == Coverage::Uncertain),
            "excluded directory must remain a canonical uncertain boundary"
        );
    }

    #[derive(Debug, Eq, PartialEq)]
    struct WorkerTrace {
        observed_paths: std::collections::BTreeSet<RelativePath>,
        admitted_runs: usize,
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the trace records each worker event and its canonical admission boundary"
    )]
    fn trace_scan_with_workers(root: &Path, threads: usize) -> WorkerTrace {
        use crossbeam_channel::unbounded;

        let storage = TemporaryStorage::scan_store_with_limit_bytes(8 * 1024 * 1024);
        let mut store =
            crate::scan_store::session::ScanStore::new(ScanGeneration::initial(), storage.clone())
                .expect("scan store should initialize");
        let input_runs = store
            .input_run_factory()
            .expect("initial generation should accept input");
        let (sender, events) = unbounded();
        let cancelled = AtomicBool::new(false);
        run(
            ScannerOptions {
                session: store.session(),
                generation: input_runs.generation(),
                coordinator: SessionCoordinator::start(store.session(), input_runs.generation())
                    .expect("scanner coordinator should start"),
                root: root.to_path_buf(),
                root_identity: None,
                threads,
                cross_filesystems: false,
                exclusions: Vec::new(),
                internal_paths: Vec::new(),
                temporary_storage: storage,
                input_runs: Some(input_runs),
            },
            &sender,
            &cancelled,
        );

        let mut observed_paths = std::collections::BTreeSet::new();
        let mut admitted_runs: usize = 0;
        let mut finished = false;
        for event in events.try_iter() {
            match event {
                WorkerEvent::ScanBatch {
                    lease: Some(lease),
                    entries,
                    input_runs,
                } => {
                    for entry in entries {
                        let relative = entry
                            .path
                            .strip_prefix(root)
                            .ok()
                            .and_then(|path| RelativePath::from_path(path).ok())
                            .expect("scanner entries should remain rooted");
                        assert!(
                            observed_paths.insert(relative),
                            "scanner must emit each path exactly once"
                        );
                    }
                    admitted_runs = admitted_runs.saturating_add(input_runs.len());
                    for run in input_runs {
                        assert!(
                            store
                                .accept_leased_input_run(&lease, run)
                                .expect("lease-validated run should be admitted")
                        );
                    }
                }
                WorkerEvent::ScanBatch { lease: None, .. } => {
                    panic!("scanner emitted an unleased batch")
                }
                WorkerEvent::ScanFinished { cancelled } => finished |= !cancelled,
                WorkerEvent::ScanFailed { message, .. } => panic!("scanner failed: {message}"),
                WorkerEvent::ScanUnscanned { path, reason, .. } => {
                    panic!("fixture path was unexpectedly unscanned: {path:?}: {reason:?}")
                }
                WorkerEvent::DeletionPlanned { .. }
                | WorkerEvent::DeletionExecutionRejected { .. }
                | WorkerEvent::DeletionFinished { .. } => {}
            }
        }
        assert!(finished, "scanner should reach its normal terminal event");
        assert!(
            admitted_runs > 0,
            "scanner should seal canonical input runs"
        );
        store
            .publish()
            .expect("worker trace facts should publish canonically");
        for path in &observed_paths {
            assert!(
                store
                    .published()
                    .expect("published generation should remain available")
                    .page_entry(path)
                    .expect("canonical exact lookup should succeed")
                    .is_some(),
                "every worker result must recover through the published generation"
            );
        }
        WorkerTrace {
            observed_paths,
            admitted_runs,
        }
    }

    #[test]
    fn worker_count_recovery_trace_matrix_preserves_canonical_paths() {
        let root = tempfile::tempdir().expect("scan root should exist");
        let mut expected_paths = std::collections::BTreeSet::new();
        for branch in ["alpha", "beta", "gamma", "delta"] {
            let directory = root.path().join(branch);
            let nested = directory.join("nested");
            fs::create_dir(&directory).expect("branch should exist");
            fs::create_dir(&nested).expect("nested branch should exist");
            fs::write(directory.join("top"), b"top").expect("top file should exist");
            fs::write(nested.join("leaf"), b"leaf").expect("leaf file should exist");
            let branch_relative = RelativePath::from_path(Path::new(branch))
                .expect("fixture branch should be relative");
            let nested_relative = RelativePath::from_path(&PathBuf::from(branch).join("nested"))
                .expect("fixture nested branch should be relative");
            for relative in [
                branch_relative,
                nested_relative,
                RelativePath::from_path(&PathBuf::from(branch).join("top"))
                    .expect("fixture file should be relative"),
                RelativePath::from_path(&PathBuf::from(branch).join("nested/leaf"))
                    .expect("fixture leaf should be relative"),
            ] {
                expected_paths.insert(relative);
            }
        }

        for workers in [1, 2, 4, 8] {
            let trace = trace_scan_with_workers(root.path(), workers);
            assert_eq!(
                trace.observed_paths, expected_paths,
                "worker count {workers} must retain every observed path"
            );
        }
    }

    #[cfg(any(unix, windows))]
    fn replace_directory_with_link(path: &Path, displaced: &Path, target: &Path) {
        fs::rename(path, displaced).expect("original directory should be displaced");
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, path).expect("replacement symlink should be created");
        #[cfg(windows)]
        {
            let quote =
                |value: &Path| format!("'{}'", value.display().to_string().replace('\'', "''"));
            let command = format!(
                "$ErrorActionPreference='Stop'; New-Item -ItemType Junction -Path {} -Target {} | Out-Null",
                quote(path),
                quote(target)
            );
            let output = std::process::Command::new("pwsh")
                .args([
                    "-NoLogo",
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    &command,
                ])
                .output()
                .expect("junction command should start");
            assert!(
                output.status.success(),
                "junction command failed with {}: stdout={:?} stderr={:?}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn scanner_marks_replaced_descendant_link_uncertain() {
        use crossbeam_channel::bounded;
        use std::sync::atomic::AtomicBool;
        use std::time::Duration;

        let root = tempfile::tempdir().expect("scan root should exist");
        let outside = tempfile::tempdir().expect("outside root should exist");
        fs::write(outside.path().join("secret"), b"outside")
            .expect("outside fixture should be written");
        let descendant = root.path().join("descendant");
        let displaced = root.path().join("displaced-descendant");
        fs::create_dir(&descendant).expect("descendant should be created");
        fs::write(descendant.join("original"), b"inside")
            .expect("descendant fixture should be written");
        let metadata = fs::symlink_metadata(&descendant).expect("descendant metadata should exist");
        let identity = identity_for(&descendant, &metadata)
            .expect("descendant identity should be readable")
            .expect("descendant identity should be available");
        replace_directory_with_link(&descendant, &displaced, outside.path());

        let (queue, _) = TaskQueue::new(root.path().to_path_buf(), 1, &TemporaryStorage::default())
            .expect("scanner task queue should be available");
        let (sender, events) = bounded(4);
        let cancelled = AtomicBool::new(false);
        let root_invalid = AtomicBool::new(false);
        let failed = AtomicBool::new(false);
        let root_directory = cap_fs::open_ambient_dir(root.path(), ambient_authority())
            .expect("scan root handle should open");
        let exclusions = Exclusions::new(root.path(), Vec::new(), Vec::new())
            .expect("scanner exclusions should compile");
        assert!(scan_directory_with_queue(
            DirectoryTask {
                path: descendant.clone(),
                identity: Some(identity),
            },
            &queue,
            &sender,
            &cancelled,
            &failed,
            &root_invalid,
            &root_directory,
            root.path(),
            None,
            &exclusions,
            None,
            true,
        ));

        match events
            .recv_timeout(Duration::from_secs(5))
            .expect("replaced descendant should be reported")
        {
            WorkerEvent::ScanUnscanned { path, reason, .. } => {
                assert_eq!(path, descendant);
                assert!(matches!(reason, UnscannedReason::Replacement(_)));
            }
            _ => panic!("replaced descendant should not be traversed"),
        }
        assert!(
            events.try_recv().is_err(),
            "replacement must not be traversed"
        );
    }
    #[cfg(unix)]
    #[test]
    fn scanner_does_not_follow_replacement_after_identity_validation() {
        use crossbeam_channel::bounded;
        use std::sync::atomic::AtomicBool;
        use std::time::Duration;

        let root = tempfile::tempdir().expect("scan root should exist");
        let outside = tempfile::tempdir().expect("outside root should exist");
        let outside_secret = outside.path().join("secret");
        fs::write(&outside_secret, b"outside").expect("outside fixture should be written");
        let descendant = root.path().join("descendant");
        let displaced = root.path().join("displaced-descendant");
        fs::create_dir(&descendant).expect("descendant should be created");
        fs::write(descendant.join("original"), b"inside")
            .expect("descendant fixture should be written");
        let metadata = fs::symlink_metadata(&descendant).expect("descendant metadata should exist");
        let identity = identity_for(&descendant, &metadata)
            .expect("descendant identity should be readable")
            .expect("descendant identity should be available");
        replace_after_next_validation(descendant.clone(), displaced, outside.path().to_path_buf());

        let (queue, _) = TaskQueue::new(root.path().to_path_buf(), 1, &TemporaryStorage::default())
            .expect("scanner task queue should be available");
        let (sender, events) = bounded(8);
        let cancelled = AtomicBool::new(false);
        let root_invalid = AtomicBool::new(false);
        let failed = AtomicBool::new(false);
        let root_directory = cap_fs::open_ambient_dir(root.path(), ambient_authority())
            .expect("scan root handle should open");
        let exclusions = Exclusions::new(root.path(), Vec::new(), Vec::new())
            .expect("scanner exclusions should compile");
        assert!(scan_directory_with_queue(
            DirectoryTask {
                path: descendant.clone(),
                identity: Some(identity),
            },
            &queue,
            &sender,
            &cancelled,
            &failed,
            &root_invalid,
            &root_directory,
            root.path(),
            None,
            &exclusions,
            None,
            true,
        ));

        let mut saw_replacement = false;
        while let Ok(event) = events.recv_timeout(Duration::from_secs(1)) {
            match event {
                WorkerEvent::ScanUnscanned { path, reason, .. } => {
                    assert_eq!(path, descendant);
                    assert!(matches!(reason, UnscannedReason::Replacement(_)));
                    saw_replacement = true;
                    break;
                }
                WorkerEvent::ScanBatch { entries, .. } => {
                    assert!(
                        !entries
                            .iter()
                            .any(|entry| entry.path == descendant.join("secret")),
                        "scanner must not enumerate the replacement target"
                    );
                }
                WorkerEvent::ScanFailed { .. }
                | WorkerEvent::ScanFinished { .. }
                | WorkerEvent::DeletionPlanned { .. }
                | WorkerEvent::DeletionExecutionRejected { .. }
                | WorkerEvent::DeletionFinished { .. } => {}
            }
        }
        assert!(
            saw_replacement,
            "replacement should be reported as unscanned"
        );
        assert!(
            outside_secret.exists(),
            "outside target should remain untouched"
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn scanner_rejects_replaced_ancestor_before_traversal() {
        use crossbeam_channel::bounded;
        use std::sync::atomic::AtomicBool;
        use std::time::Duration;

        let root = tempfile::tempdir().expect("scan root should exist");
        let outside = tempfile::tempdir().expect("outside root should exist");
        let ancestor = root.path().join("ancestor");
        let descendant = ancestor.join("descendant");
        fs::create_dir_all(&descendant).expect("descendant should be created");
        fs::write(descendant.join("original"), b"inside")
            .expect("descendant fixture should be written");
        let metadata = fs::symlink_metadata(&descendant).expect("descendant metadata should exist");
        let identity = identity_for(&descendant, &metadata)
            .expect("descendant identity should be readable")
            .expect("descendant identity should be available");

        let moved = outside.path().join("moved-ancestor");
        replace_directory_with_link(&ancestor, &moved, &moved);
        let outside_only = moved.join("descendant").join("outside-only");
        let escaped_path = descendant.join("outside-only");
        fs::write(&outside_only, b"outside").expect("outside fixture should be written");

        let (queue, _) = TaskQueue::new(root.path().to_path_buf(), 1, &TemporaryStorage::default())
            .expect("scanner task queue should be available");
        let (sender, events) = bounded(8);
        let cancelled = AtomicBool::new(false);
        let root_invalid = AtomicBool::new(false);
        let failed = AtomicBool::new(false);
        let root_directory = cap_fs::open_ambient_dir(root.path(), ambient_authority())
            .expect("scan root handle should open");
        let exclusions = Exclusions::new(root.path(), Vec::new(), Vec::new())
            .expect("scanner exclusions should compile");
        assert!(scan_directory_with_queue(
            DirectoryTask {
                path: descendant.clone(),
                identity: Some(identity),
            },
            &queue,
            &sender,
            &cancelled,
            &failed,
            &root_invalid,
            &root_directory,
            root.path(),
            None,
            &exclusions,
            None,
            true,
        ));

        let mut saw_replacement = false;
        while let Ok(event) = events.recv_timeout(Duration::from_secs(1)) {
            match event {
                WorkerEvent::ScanUnscanned { path, reason, .. } => {
                    assert_eq!(path, descendant);
                    assert!(matches!(reason, UnscannedReason::Replacement(_)));
                    saw_replacement = true;
                    break;
                }
                WorkerEvent::ScanBatch { entries, .. } => {
                    assert!(
                        !entries.iter().any(|entry| entry.path == escaped_path),
                        "scanner must not enumerate the moved ancestor"
                    );
                }
                WorkerEvent::ScanFailed { .. }
                | WorkerEvent::ScanFinished { .. }
                | WorkerEvent::DeletionPlanned { .. }
                | WorkerEvent::DeletionExecutionRejected { .. }
                | WorkerEvent::DeletionFinished { .. } => {}
            }
        }
        assert!(
            saw_replacement,
            "replaced ancestor should be reported as unscanned"
        );
        assert!(
            outside_only.exists(),
            "outside target should remain untouched"
        );
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum FilesystemKey {
    #[cfg(unix)]
    Unix(u64),
    #[cfg(windows)]
    Windows(OsString),
    #[cfg(not(any(unix, windows)))]
    Other,
}

#[cfg(unix)]
fn filesystem_key(path: &Path) -> std::io::Result<FilesystemKey> {
    let metadata = fs::metadata(path)?;
    Ok(filesystem_key_with_metadata(path, &metadata))
}

#[cfg(windows)]
fn filesystem_key(path: &Path) -> std::io::Result<FilesystemKey> {
    let metadata = fs::metadata(path)?;
    filesystem_key_with_metadata(path, &metadata)
}

#[cfg(not(any(unix, windows)))]
fn filesystem_key(path: &Path) -> std::io::Result<FilesystemKey> {
    let metadata = fs::metadata(path)?;
    Ok(filesystem_key_with_metadata(path, &metadata))
}

#[cfg(unix)]
fn filesystem_key_with_metadata(_path: &Path, metadata: &fs::Metadata) -> FilesystemKey {
    use std::os::unix::fs::MetadataExt as _;

    FilesystemKey::Unix(metadata.dev())
}

#[cfg(windows)]
fn filesystem_key_with_metadata(
    path: &Path,
    _metadata: &fs::Metadata,
) -> std::io::Result<FilesystemKey> {
    use std::path::Component;

    let prefix = path
        .components()
        .find_map(|component| match component {
            Component::Prefix(prefix) => Some(prefix.as_os_str().to_os_string()),
            _ => None,
        })
        .ok_or_else(|| std::io::Error::other("path has no Windows volume prefix"))?;
    Ok(FilesystemKey::Windows(prefix))
}

#[cfg(not(any(unix, windows)))]
fn filesystem_key_with_metadata(_path: &Path, _metadata: &fs::Metadata) -> FilesystemKey {
    FilesystemKey::Other
}

#[cfg(unix)]
fn filesystem_boundary_reason(
    path: &Path,
    metadata: &fs::Metadata,
    root_filesystem: &FilesystemKey,
) -> Option<UnscannedReason> {
    let current = filesystem_key_with_metadata(path, metadata);
    if &current == root_filesystem {
        None
    } else {
        Some(UnscannedReason::FilesystemBoundary)
    }
}

#[cfg(windows)]
fn filesystem_boundary_reason(
    path: &Path,
    metadata: &fs::Metadata,
    root_filesystem: &FilesystemKey,
) -> Option<UnscannedReason> {
    match filesystem_key_with_metadata(path, metadata) {
        Ok(current) if &current != root_filesystem => Some(UnscannedReason::FilesystemBoundary),
        Ok(_) => None,
        Err(error) => Some(UnscannedReason::Metadata(format!(
            "could not identify filesystem: {error}"
        ))),
    }
}

#[cfg(not(any(unix, windows)))]
fn filesystem_boundary_reason(
    path: &Path,
    metadata: &fs::Metadata,
    root_filesystem: &FilesystemKey,
) -> Option<UnscannedReason> {
    let current = filesystem_key_with_metadata(path, metadata);
    if &current == root_filesystem {
        None
    } else {
        Some(UnscannedReason::FilesystemBoundary)
    }
}
