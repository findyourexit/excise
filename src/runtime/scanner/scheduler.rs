use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossbeam_channel::{Receiver, Select, Sender, TrySendError};

use super::super::worker::{WorkerEvent, send_event};
use super::task_queue::{DirectoryTask, TaskQueue};
use crate::scan_coordinator::{
    CompletionOutcome, RelativePath, ScanCoordinator, ScanGeneration, ScheduleOutcome,
    SchedulerSnapshot, WorkCompletion, WorkKey, WorkKind, WorkLease, WorkPriority,
};
use crate::scan_session::ScanSessionId;

const ACTOR_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Bounded UI-to-actor control and status handle.
///
/// Status is one coalesced snapshot rather than an event queue, so scanner
/// progress cannot consume unbounded owner-loop capacity.
#[derive(Clone)]
pub(crate) struct SchedulerHandle {
    focus: Sender<PathBuf>,
    snapshot: Arc<Mutex<Option<SchedulerSnapshot>>>,
}

impl SchedulerHandle {
    pub(crate) const fn new(
        focus: Sender<PathBuf>,
        snapshot: Arc<Mutex<Option<SchedulerSnapshot>>>,
    ) -> Self {
        Self { focus, snapshot }
    }

    pub(crate) fn prioritize(&self, path: &Path) {
        let _ = self.focus.try_send(path.to_path_buf());
    }

    #[must_use]
    pub(crate) fn snapshot(&self) -> Option<SchedulerSnapshot> {
        *self
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
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
/// The actor is the only caller that touches the persistent task journal and
/// the only process that accepts worker output. A worker receives one lease,
/// emits all results through its bounded actor channel, then completes that
/// lease. Results from a disconnected or no-longer-active worker are rejected
/// before they reach the owner loop.
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
    commands: Receiver<SchedulerCommand>,
    focus: Receiver<PathBuf>,
    assignments: Vec<Sender<LeasedDirectoryTask>>,
    worker_events: Vec<Receiver<WorkerEvent>>,
    cancelled: &AtomicBool,
    failed: &AtomicBool,
    root_invalid: &AtomicBool,
    events: &Sender<WorkerEvent>,
    snapshot: &Mutex<Option<SchedulerSnapshot>>,
) {
    let mut coordinator = ScanCoordinator::new(session, generation);
    publish_snapshot(snapshot, &coordinator);
    if assignments.len() != worker_events.len() {
        fail(
            &mut coordinator,
            snapshot,
            events,
            cancelled,
            failed,
            "scanner coordinator worker channels are inconsistent".to_string(),
        );
        return;
    }

    let mut active = vec![None; assignments.len()];

    loop {
        if root_invalid.load(Ordering::Acquire) {
            coordinator.invalidate_all();
            publish_snapshot(snapshot, &coordinator);
            if let Err(error) = forward_active_events(&worker_events, &active, events, cancelled) {
                fail(&mut coordinator, snapshot, events, cancelled, failed, error);
            }
            return;
        }
        if cancelled.load(Ordering::Acquire) {
            coordinator.cancel_all();
            publish_snapshot(snapshot, &coordinator);
            return;
        }
        if failed.load(Ordering::Acquire) {
            coordinator.fail_all();
            publish_snapshot(snapshot, &coordinator);
            return;
        }

        if let Err(error) = dispatch_ready(
            &queue,
            root,
            &mut coordinator,
            &assignments,
            &mut active,
            cancelled,
            failed,
            root_invalid,
        ) {
            fail(&mut coordinator, snapshot, events, cancelled, failed, error);
            return;
        }
        publish_snapshot(snapshot, &coordinator);

        if queue.is_idle() && active.iter().all(Option::is_none) {
            return;
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
                    if let Err(error) = queue.schedule(task) {
                        fail(
                            &mut coordinator,
                            snapshot,
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
                            &mut coordinator,
                            snapshot,
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
                        fail(&mut coordinator, snapshot, events, cancelled, failed, error);
                        return;
                    }
                    if let Err(error) = complete_lease(
                        &queue,
                        &mut coordinator,
                        &mut active,
                        worker,
                        &lease,
                        work_completion,
                    ) {
                        fail(&mut coordinator, snapshot, events, cancelled, failed, error);
                        return;
                    }
                    publish_snapshot(snapshot, &coordinator);
                }
                Err(_) => {
                    if active.iter().any(Option::is_some) {
                        fail(
                            &mut coordinator,
                            snapshot,
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
            if let Ok(path) = operation.recv(&focus) {
                queue.prioritize(path.clone());
                if let Ok(relative) = path.strip_prefix(root)
                    && let Ok(relative) = RelativePath::from_path(relative)
                {
                    coordinator.focus(relative);
                }
            }
            publish_snapshot(snapshot, &coordinator);
            continue;
        }

        let worker = index.saturating_sub(worker_start);
        let Ok(event) = operation.recv(&worker_events[worker]) else {
            if active.get(worker).is_some_and(Option::is_some) {
                fail(
                    &mut coordinator,
                    snapshot,
                    events,
                    cancelled,
                    failed,
                    "scanner worker disconnected before completing its lease".to_string(),
                );
            }
            return;
        };
        if let Err(error) = forward_worker_event(worker, &active, events, event, cancelled) {
            fail(&mut coordinator, snapshot, events, cancelled, failed, error);
            return;
        }
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "Dispatch keeps its bounded queue, active lease table, and cancellation gates in one atomic actor step."
)]
fn dispatch_ready(
    queue: &TaskQueue,
    root: &Path,
    coordinator: &mut ScanCoordinator,
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
        let Some(task) = queue
            .try_take(cancelled, failed, root_invalid)
            .map_err(|error| format!("could not read scanner task journal: {error}"))?
        else {
            return Ok(());
        };
        let assignment = lease_task(coordinator, root, task)?;
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
    coordinator: &mut ScanCoordinator,
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
    if coordinator.finish(lease, work_completion) != CompletionOutcome::Accepted {
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

fn forward_active_events(
    worker_events: &[Receiver<WorkerEvent>],
    active: &[Option<WorkLease>],
    events: &Sender<WorkerEvent>,
    cancelled: &AtomicBool,
) -> Result<(), String> {
    for (worker, receiver) in worker_events.iter().enumerate() {
        if active.get(worker).is_some_and(Option::is_some) {
            forward_pending_events(receiver, worker, active, events, cancelled)?;
        }
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

fn lease_task(
    coordinator: &mut ScanCoordinator,
    root: &Path,
    task: DirectoryTask,
) -> Result<LeasedDirectoryTask, String> {
    let relative = task
        .path
        .strip_prefix(root)
        .map_err(|_| "scanner task escaped its configured root".to_string())?;

    let relative = RelativePath::from_path(relative)
        .map_err(|_| "scanner task has an invalid root-relative path".to_string())?;
    let key = WorkKey::new(
        coordinator.session(),
        coordinator.generation(),
        WorkKind::EnumerateDirectory,
        relative,
    );
    if coordinator.schedule(key, WorkPriority::Background) != ScheduleOutcome::Enqueued {
        return Err("scanner task was not admitted by the coordinator".to_string());
    }
    let lease = coordinator
        .lease_next()
        .ok_or_else(|| "scanner coordinator failed to issue an admitted lease".to_string())?;
    Ok(LeasedDirectoryTask { task, lease })
}

fn publish_snapshot(snapshot: &Mutex<Option<SchedulerSnapshot>>, coordinator: &ScanCoordinator) {
    *snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(coordinator.snapshot());
}
fn fail(
    coordinator: &mut ScanCoordinator,
    snapshot: &Mutex<Option<SchedulerSnapshot>>,
    events: &Sender<WorkerEvent>,
    cancelled: &AtomicBool,
    failed: &AtomicBool,
    message: String,
) {
    coordinator.fail_all();
    publish_snapshot(snapshot, coordinator);
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
