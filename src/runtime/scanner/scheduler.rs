use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossbeam_channel::{Receiver, Select, Sender, TrySendError};

use super::super::worker::{WorkerEvent, send_event};
use super::task_queue::{DirectoryTask, TaskQueue};
use crate::scan_coordinator::{
    CompletionOutcome, RelativePath, ScanGeneration, ScheduleOutcome, SessionCoordinator,
    WorkCompletion, WorkKey, WorkKind, WorkLease, WorkPriority,
};
use crate::scan_session::ScanSessionId;

const ACTOR_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Bounded UI-to-actor focus control.
///
/// The session coordinator exposes combined status directly. This handle only
/// carries best-effort focus requests into the scanner's private task journal.
#[derive(Clone)]
pub(crate) struct SchedulerHandle {
    focus: Sender<PathBuf>,
}

impl SchedulerHandle {
    pub(crate) const fn new(focus: Sender<PathBuf>) -> Self {
        Self { focus }
    }

    pub(crate) fn prioritize(&self, path: &Path) {
        let _ = self.focus.try_send(path.to_path_buf());
    }
}

/// Worker-to-actor ownership transfer. Scanner workers never mutate the
/// directory journal directly.
pub(super) enum SchedulerCommand {
    Enqueue(DirectoryTask),
    Complete {
        worker: usize,
        lease: WorkLease,
        work_completion: WorkCompletion,
    },
}

/// One directory task paired with the exact lease the worker must echo back.
pub(super) struct LeasedDirectoryTask {
    pub(super) task: DirectoryTask,
    pub(super) lease: WorkLease,
}

/// Owns bounded directory scheduling for one scanner generation.
///
/// The actor is the only caller that touches the persistent task journal. The
/// session coordinator owns every work key and lease. Scanner workers receive
/// an exact lease, return results through fixed-size channels, and never
/// mutate either record directly.
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::needless_pass_by_value,
    reason = "the actor owns its channels and every cancellation boundary explicitly"
)]
pub(super) fn run(
    queue: Arc<TaskQueue>,
    root: &Path,
    session: ScanSessionId,
    generation: ScanGeneration,
    coordinator: SessionCoordinator,
    commands: Receiver<SchedulerCommand>,
    focus: Receiver<PathBuf>,
    assignments: Vec<Sender<LeasedDirectoryTask>>,
    worker_events: Vec<Receiver<WorkerEvent>>,
    cancelled: &AtomicBool,
    failed: &AtomicBool,
    root_invalid: &AtomicBool,
    events: &Sender<WorkerEvent>,
) {
    if assignments.len() != worker_events.len() {
        fail(
            &coordinator,
            events,
            cancelled,
            failed,
            "scanner coordinator worker channels are inconsistent".to_string(),
        );
        return;
    }
    let root_key = match task_key(session, generation, root, root) {
        Ok(key) => key,
        Err(error) => {
            fail(&coordinator, events, cancelled, failed, error);
            return;
        }
    };
    if !matches!(
        coordinator.register(root_key, WorkPriority::Background),
        Ok(ScheduleOutcome::Enqueued)
    ) {
        fail(
            &coordinator,
            events,
            cancelled,
            failed,
            "scanner coordinator could not admit its root task".to_string(),
        );
        return;
    }

    let mut active = vec![None; assignments.len()];
    let mut root_invalidated = false;

    loop {
        if root_invalid.load(Ordering::Acquire) {
            if !root_invalidated {
                coordinator.invalidate_scan_work();
                root_invalidated = true;
            }
            // The invalidating worker publishes its failure after setting this flag.
            // Keep its receiver alive until that worker completes its active lease.
            if active.iter().all(Option::is_none) {
                return;
            }
        }
        if cancelled.load(Ordering::Acquire) {
            coordinator.cancel_scan_work();
            return;
        }
        if failed.load(Ordering::Acquire) {
            coordinator.fail_scan_work();
            return;
        }

        if !root_invalidated {
            if let Err(error) = dispatch_ready(
                &queue,
                root,
                &coordinator,
                &assignments,
                &mut active,
                cancelled,
                failed,
                root_invalid,
            ) {
                fail(&coordinator, events, cancelled, failed, error);
                return;
            }

            if queue.is_idle() && active.iter().all(Option::is_none) {
                return;
            }
        }

        let mut select = Select::new();
        let command_index = select.recv(&commands);
        let focus_index = select.recv(&focus);
        let worker_start = focus_index.saturating_add(1);
        for receiver in &worker_events {
            select.recv(receiver);
        }
        let Ok(operation) = select.select_timeout(ACTOR_POLL_INTERVAL) else {
            continue;
        };
        let index = operation.index();
        if index == command_index {
            match operation.recv(&commands) {
                Ok(SchedulerCommand::Enqueue(task)) => {
                    let admitted =
                        match register_task(&coordinator, session, generation, root, &task) {
                            Ok(admitted) => admitted,
                            Err(error) => {
                                fail(&coordinator, events, cancelled, failed, error);
                                return;
                            }
                        };
                    if admitted && let Err(error) = queue.schedule(task) {
                        fail(
                            &coordinator,
                            events,
                            cancelled,
                            failed,
                            format!("could not queue scanner directory: {error}"),
                        );
                        return;
                    }
                }
                Ok(SchedulerCommand::Complete {
                    worker,
                    lease,
                    work_completion,
                }) => {
                    let Some(worker_event_receiver) = worker_events.get(worker) else {
                        fail(
                            &coordinator,
                            events,
                            cancelled,
                            failed,
                            "scanner worker completed with an invalid worker identifier"
                                .to_string(),
                        );
                        return;
                    };
                    if let Err(error) = forward_pending_events(
                        worker_event_receiver,
                        worker,
                        &active,
                        events,
                        cancelled,
                    ) {
                        fail(&coordinator, events, cancelled, failed, error);
                        return;
                    }
                    if let Err(error) = complete_lease(
                        &queue,
                        &coordinator,
                        &mut active,
                        worker,
                        &lease,
                        work_completion,
                    ) {
                        fail(&coordinator, events, cancelled, failed, error);
                        return;
                    }
                }
                Err(_) => {
                    if active.iter().any(Option::is_some) {
                        fail(
                            &coordinator,
                            events,
                            cancelled,
                            failed,
                            "scanner workers disconnected before completing their leases"
                                .to_string(),
                        );
                    }
                    return;
                }
            }
            continue;
        }

        if index == focus_index {
            if let Ok(path) = operation.recv(&focus)
                && let Ok(relative) = path.strip_prefix(root)
                && let Ok(relative) = RelativePath::from_path(relative)
            {
                coordinator.focus(relative);
            }
            continue;
        }

        let worker = index.saturating_sub(worker_start);
        let Ok(event) = operation.recv(&worker_events[worker]) else {
            if active.get(worker).is_some_and(Option::is_some) {
                fail(
                    &coordinator,
                    events,
                    cancelled,
                    failed,
                    "scanner worker disconnected before completing its lease".to_string(),
                );
            }
            return;
        };
        if let Err(error) = forward_worker_event(worker, &active, events, event, cancelled) {
            fail(&coordinator, events, cancelled, failed, error);
            return;
        }
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "Dispatch keeps its bounded journal, selected leases, and cancellation gates in one actor step."
)]
fn dispatch_ready(
    queue: &TaskQueue,
    root: &Path,
    coordinator: &SessionCoordinator,
    assignments: &[Sender<LeasedDirectoryTask>],
    active: &mut [Option<WorkLease>],
    cancelled: &AtomicBool,
    failed: &AtomicBool,
    root_invalid: &AtomicBool,
) -> Result<(), String> {
    for (worker, assignment_sender) in assignments.iter().enumerate() {
        if active[worker].is_some() {
            continue;
        }
        let Some(assignment) =
            lease_task(queue, root, coordinator, cancelled, failed, root_invalid)?
        else {
            return Ok(());
        };
        let lease = assignment.lease.clone();
        match assignment_sender.try_send(assignment) {
            Ok(()) => active[worker] = Some(lease),
            Err(TrySendError::Full(_)) => {
                return Err(
                    "scanner coordinator attempted to assign an already-busy worker".to_string(),
                );
            }
            Err(TrySendError::Disconnected(_)) => {
                return Err("scanner worker assignment channel disconnected".to_string());
            }
        }
    }
    Ok(())
}

fn complete_lease(
    queue: &TaskQueue,
    coordinator: &SessionCoordinator,
    active: &mut [Option<WorkLease>],
    worker: usize,
    lease: &WorkLease,
    work_completion: WorkCompletion,
) -> Result<(), String> {
    let Some(expected) = active.get(worker).and_then(Option::as_ref) else {
        return Err("scanner worker completed without an active lease".to_string());
    };
    if expected != lease {
        return Err("scanner worker returned a stale or forged task lease".to_string());
    }
    if coordinator
        .finish(lease.clone(), work_completion)
        .map_err(|error| error.to_string())?
        != CompletionOutcome::Accepted
    {
        return Err("scanner coordinator rejected an active task completion".to_string());
    }
    active[worker] = None;
    queue.complete();
    Ok(())
}

fn forward_pending_events(
    receiver: &Receiver<WorkerEvent>,
    worker: usize,
    active: &[Option<WorkLease>],
    events: &Sender<WorkerEvent>,
    cancelled: &AtomicBool,
) -> Result<(), String> {
    for event in receiver.try_iter() {
        forward_worker_event(worker, active, events, event, cancelled)?;
    }
    Ok(())
}

fn forward_worker_event(
    worker: usize,
    active: &[Option<WorkLease>],
    events: &Sender<WorkerEvent>,
    event: WorkerEvent,
    cancelled: &AtomicBool,
) -> Result<(), String> {
    let Some(active_lease) = active.get(worker).and_then(Option::as_ref) else {
        return Err("scanner worker emitted a result without an active lease".to_string());
    };
    if let WorkerEvent::ScanBatch { lease, .. } | WorkerEvent::ScanUnscanned { lease, .. } = &event
    {
        let Some(lease) = lease.as_ref() else {
            return Err("scanner worker emitted an unleased sealed result".to_string());
        };
        if lease != active_lease {
            return Err("scanner worker emitted a sealed result for a stale lease".to_string());
        }
    }
    if !send_event(events, event, cancelled) && !cancelled.load(Ordering::Acquire) {
        return Err("owner loop stopped receiving scanner results".to_string());
    }
    Ok(())
}

fn task_key(
    session: ScanSessionId,
    generation: ScanGeneration,
    root: &Path,
    path: &Path,
) -> Result<WorkKey, String> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| "scanner task escaped its configured root".to_string())?;
    let relative = RelativePath::from_path(relative)
        .map_err(|_| "scanner task has an invalid root-relative path".to_string())?;
    Ok(WorkKey::new(
        session,
        generation,
        WorkKind::EnumerateDirectory,
        relative,
    ))
}

fn register_task(
    coordinator: &SessionCoordinator,
    session: ScanSessionId,
    generation: ScanGeneration,
    root: &Path,
    task: &DirectoryTask,
) -> Result<bool, String> {
    let key = task_key(session, generation, root, &task.path)?;
    match coordinator
        .register(key, WorkPriority::Background)
        .map_err(|error| error.to_string())?
    {
        ScheduleOutcome::Enqueued => Ok(true),
        ScheduleOutcome::PriorityRaised
        | ScheduleOutcome::AlreadyPending
        | ScheduleOutcome::AlreadyLeased => Ok(false),
        ScheduleOutcome::StaleSession | ScheduleOutcome::StaleGeneration => {
            Err("scanner task does not belong to the active session generation".to_string())
        }
    }
}

fn lease_task(
    queue: &TaskQueue,
    root: &Path,
    coordinator: &SessionCoordinator,
    cancelled: &AtomicBool,
    failed: &AtomicBool,
    root_invalid: &AtomicBool,
) -> Result<Option<LeasedDirectoryTask>, String> {
    let Some(lease) = coordinator
        .lease_next_scan()
        .map_err(|error| error.to_string())?
    else {
        return Ok(None);
    };
    let path = root.join(lease.key().path().to_path_buf());
    let task = queue
        .take_path(&path, cancelled, failed, root_invalid)
        .map_err(|error| format!("could not read scanner task journal: {error}"))?;
    let Some(task) = task else {
        let _ = coordinator.finish(lease, WorkCompletion::Failed);
        return Err("session coordinator selected a missing scanner task".to_string());
    };
    Ok(Some(LeasedDirectoryTask { task, lease }))
}

fn fail(
    coordinator: &SessionCoordinator,
    events: &Sender<WorkerEvent>,
    cancelled: &AtomicBool,
    failed: &AtomicBool,
    message: String,
) {
    coordinator.fail_scan_work();
    failed.store(true, Ordering::Release);
    let _ = send_event(
        events,
        WorkerEvent::ScanFailed {
            path: None,
            message,
        },
        cancelled,
    );
}
