use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use super::FileToDelete;
use crate::deletion::DeletionPlan;
use crate::native_path::safe_display_path_text;
use crate::scan_coordinator::{
    CompletionOutcome, RelativePath, RequeueOutcome, ScanGeneration, SessionCoordinator,
    WorkCompletion, WorkKind, WorkLease, WorkPriority,
};
use crate::scan_session::ScanSessionId;

/// Interactive deletion retains only visible, independently cancellable work.
pub(crate) const MAX_DELETION_WORK_ITEMS: usize = 4;
/// Time a newly confirmed target takes to fill with its checker preparation pattern.
pub(crate) const DELETION_CHECKER_COVER_DURATION: Duration = Duration::from_millis(600);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct DeletionWorkId(pub(crate) u64);

impl DeletionWorkId {
    #[cfg(test)]
    pub(crate) const fn for_test(value: u64) -> Self {
        Self(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeletionWorkSummary {
    /// Operations that have not entered the serial filesystem mutation lane.
    pub pending_operations: usize,
    /// At most one executor may mutate the filesystem at a time.
    pub mutating: bool,
    /// Reviewed entries in the active mutation, if any.
    pub planned_entries: Option<u64>,
    /// Entries completed by the active mutation, if any.
    pub completed_entries: Option<u64>,
}

impl DeletionWorkSummary {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            pending_operations: 0,
            mutating: false,
            planned_entries: None,
            completed_entries: None,
        }
    }

    #[must_use]
    pub const fn has_work(self) -> bool {
        self.pending_operations > 0 || self.mutating
    }
}

impl Default for DeletionWorkSummary {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DeletionWorkError {
    QueueFull,
    OverlappingTarget,
}

impl DeletionWorkError {
    #[must_use]
    pub(crate) const fn message(self) -> &'static str {
        match self {
            Self::QueueFull => {
                "Deletion work queue is full; wait for an active check to finish or cancel it"
            }
            Self::OverlappingTarget => {
                "Deletion work already covers this entry or one of its parent folders"
            }
        }
    }
}

/// Commands are separated by capability: planning never gains filesystem mutation authority.
pub(crate) enum DeletionWorkCommand {
    Plan {
        work_id: DeletionWorkId,
        target: Box<FileToDelete>,
        reduced_guardrails: bool,
        maximum_bytes: usize,
    },
    Execute {
        work_id: DeletionWorkId,
        plan: Box<DeletionPlan>,
        progress: Arc<AtomicU64>,
    },
}

impl DeletionWorkCommand {
    #[must_use]
    pub(crate) const fn work_id(&self) -> DeletionWorkId {
        match self {
            Self::Plan { work_id, .. } | Self::Execute { work_id, .. } => *work_id,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WorkRailStatus {
    Planning,
    AwaitingConfirmation,
    Queued,
    Executing,
}

#[derive(Clone, Copy)]
pub(crate) struct WorkRailItem<'a> {
    pub path: &'a str,
    pub status: WorkRailStatus,
    pub planned_entries: Option<u64>,
    pub completed: Option<&'a AtomicU64>,
    pub confirmed_at: Option<Duration>,
}

struct DeletionWorkItem {
    id: DeletionWorkId,
    path: PathBuf,
    relative_path: RelativePath,
    label: Box<str>,
    /// A lightweight stale-refresh seed; planner input itself is moved to the worker.
    target: Option<FileToDelete>,
    plan: Option<Box<DeletionPlan>>,
    stage: DeletionWorkStage,
    confirmed_at: Option<Duration>,
}

enum DeletionWorkStage {
    AwaitingConfirmation {
        target: Box<FileToDelete>,
        reduced_guardrails: bool,
        maximum_bytes: usize,
    },
    Confirming {
        reduced_guardrails: bool,
        maximum_bytes: usize,
    },
    QueuedPlanning {
        target: Box<FileToDelete>,
        reduced_guardrails: bool,
        maximum_bytes: usize,
    },
    Planning,
    QueuedExecution,
    Executing {
        planned_entries: u64,
        progress: Arc<AtomicU64>,
    },
    CancellingPlanning,
}

/// Owner-loop deletion state. The session coordinator owns every active plan
/// and mutation lease, while this bounded rail retains only user-facing work.
pub(crate) struct DeletionWork {
    items: VecDeque<DeletionWorkItem>,
    coordinator: SessionCoordinator,
    leases: Vec<(DeletionWorkId, WorkLease)>,
    next_id: u64,
}

impl Default for DeletionWork {
    fn default() -> Self {
        Self::new()
    }
}

impl DeletionWork {
    #[must_use]
    pub(crate) fn new() -> Self {
        let coordinator = SessionCoordinator::start(
            ScanSessionId::from_bytes([0; 16]),
            ScanGeneration::initial(),
        )
        .expect("test deletion coordinator should start");
        Self::new_for_coordinator(coordinator)
    }

    #[must_use]
    pub(crate) fn new_for_coordinator(coordinator: SessionCoordinator) -> Self {
        Self {
            items: VecDeque::with_capacity(MAX_DELETION_WORK_ITEMS),
            coordinator,
            leases: Vec::with_capacity(MAX_DELETION_WORK_ITEMS),
            next_id: 0,
        }
    }

    /// Retains an immediate user confirmation before granting the planner work.
    pub(crate) fn enqueue_confirmation(
        &mut self,
        target: FileToDelete,
        reduced_guardrails: bool,
        maximum_bytes: usize,
        now: Duration,
    ) -> Result<DeletionWorkId, DeletionWorkError> {
        let path = target.full_path();
        let display_target = target.display_copy();
        let stage = if reduced_guardrails {
            DeletionWorkStage::QueuedPlanning {
                target: Box::new(target),
                reduced_guardrails,
                maximum_bytes,
            }
        } else {
            DeletionWorkStage::AwaitingConfirmation {
                target: Box::new(target),
                reduced_guardrails,
                maximum_bytes,
            }
        };
        self.enqueue(
            path,
            Some(display_target),
            stage,
            reduced_guardrails.then_some(now),
        )
    }

    fn enqueue(
        &mut self,
        path: PathBuf,
        target: Option<FileToDelete>,
        stage: DeletionWorkStage,
        confirmed_at: Option<Duration>,
    ) -> Result<DeletionWorkId, DeletionWorkError> {
        if self.items.len() >= MAX_DELETION_WORK_ITEMS {
            return Err(DeletionWorkError::QueueFull);
        }
        if self
            .items
            .iter()
            .any(|item| targets_overlap(&item.path, &path))
        {
            return Err(DeletionWorkError::OverlappingTarget);
        }
        let relative_path = target.as_ref().map_or_else(RelativePath::root, |target| {
            RelativePath::from_components(target.path_to_file.clone())
                .expect("deletion targets must contain normalized relative components")
        });
        let id = self.next_work_id();
        self.items.push_back(DeletionWorkItem {
            id,
            label: safe_display_path_text(&path).into_boxed_str(),
            path,
            relative_path,
            target,
            plan: None,
            stage,
            confirmed_at,
        });
        Ok(id)
    }

    /// Starts at most one identity-planning request without blocking the executor lane.
    #[must_use]
    pub(crate) fn next_planning_command(&mut self) -> Option<DeletionWorkCommand> {
        if self.items.iter().any(|item| {
            matches!(
                item.stage,
                DeletionWorkStage::Planning | DeletionWorkStage::CancellingPlanning
            )
        }) {
            return None;
        }
        let index = self
            .items
            .iter()
            .position(|item| matches!(item.stage, DeletionWorkStage::QueuedPlanning { .. }))?;
        let work_id = self.items[index].id;
        let lease = self.acquire_lease(
            WorkKind::PlanDeletion,
            self.items[index].relative_path.clone(),
            WorkPriority::Foreground,
        )?;
        let DeletionWorkStage::QueuedPlanning {
            target,
            reduced_guardrails,
            maximum_bytes,
        } = std::mem::replace(&mut self.items[index].stage, DeletionWorkStage::Planning)
        else {
            unreachable!("queued planning stage must remain queued planning")
        };
        self.items[index].target = Some(target.display_copy());
        self.remember_lease(work_id, lease);
        Some(DeletionWorkCommand::Plan {
            work_id,
            target,
            reduced_guardrails,
            maximum_bytes,
        })
    }

    /// Starts at most one revalidate-and-execute request. The worker keeps those operations
    /// adjacent, so a successful revalidation cannot race a later command queue turn.
    #[must_use]
    pub(crate) fn next_execution_command(&mut self) -> Option<DeletionWorkCommand> {
        if self
            .items
            .iter()
            .any(|item| matches!(item.stage, DeletionWorkStage::Executing { .. }))
        {
            return None;
        }
        let index = self
            .items
            .iter()
            .position(|item| matches!(item.stage, DeletionWorkStage::QueuedExecution))?;
        let work_id = self.items[index].id;
        let lease = self.acquire_lease(
            WorkKind::ExecuteDeletion,
            self.items[index].relative_path.clone(),
            WorkPriority::Safety,
        )?;
        self.remember_lease(work_id, lease);
        let Some(plan) = self.items[index].plan.take() else {
            let _ = self.finish_lease(work_id, WorkCompletion::Failed);
            return None;
        };
        if self.items[index].path != plan.target.full_path() {
            let _ = self.finish_lease(work_id, WorkCompletion::Invalidated);
            let _ = self.items.remove(index);
            return None;
        }
        let progress = Arc::new(AtomicU64::new(0));
        let planned_entries = plan.planned_entries();
        self.items[index].stage = DeletionWorkStage::Executing {
            planned_entries,
            progress: Arc::clone(&progress),
        };
        Some(DeletionWorkCommand::Execute {
            work_id,
            plan,
            progress,
        })
    }

    /// Restores a command only when its worker lane did not accept it.
    pub(crate) fn restore_unsubmitted(&mut self, command: DeletionWorkCommand) {
        let work_id = command.work_id();
        if !self.items.iter().any(|item| item.id == work_id) {
            let _ = self.finish_lease(work_id, WorkCompletion::Cancelled);
            return;
        }
        let restoreable = self
            .items
            .iter()
            .find(|item| item.id == work_id)
            .is_some_and(|item| {
                matches!(
                    (&command, &item.stage),
                    (
                        DeletionWorkCommand::Plan { .. },
                        DeletionWorkStage::Planning
                    ) | (
                        DeletionWorkCommand::Execute { .. },
                        DeletionWorkStage::Executing { .. }
                    )
                )
            });
        if !restoreable {
            let _ = self.finish_lease(work_id, WorkCompletion::Cancelled);
            return;
        }
        if !self.requeue_lease(work_id) {
            return;
        }
        let item = self
            .items
            .iter_mut()
            .find(|item| item.id == work_id)
            .expect("retained deletion work should remain addressable");
        match (command, &mut item.stage) {
            (
                DeletionWorkCommand::Plan {
                    target,
                    reduced_guardrails,
                    maximum_bytes,
                    ..
                },
                DeletionWorkStage::Planning,
            ) => {
                item.stage = DeletionWorkStage::QueuedPlanning {
                    target,
                    reduced_guardrails,
                    maximum_bytes,
                };
            }
            (
                DeletionWorkCommand::Execute { plan, progress, .. },
                DeletionWorkStage::Executing { .. },
            ) => {
                item.plan = Some(plan);
                item.stage = DeletionWorkStage::QueuedExecution;
                let _ = progress;
            }
            _ => {}
        }
    }

    pub(crate) fn planning_succeeded(
        &mut self,
        work_id: DeletionWorkId,
        plan: Box<DeletionPlan>,
    ) -> bool {
        let Some(index) = self.items.iter().position(|item| item.id == work_id) else {
            return false;
        };
        if matches!(
            self.items[index].stage,
            DeletionWorkStage::CancellingPlanning
        ) {
            let _ = self.finish_lease(work_id, WorkCompletion::Cancelled);
            let _ = self.items.remove(index);
            return false;
        }
        if !matches!(self.items[index].stage, DeletionWorkStage::Planning)
            || self.items[index].path != plan.target.full_path()
        {
            let _ = self.finish_lease(work_id, WorkCompletion::Failed);
            let _ = self.items.remove(index);
            return false;
        }
        if !self.finish_lease(work_id, WorkCompletion::Succeeded) {
            let _ = self.items.remove(index);
            return false;
        }
        self.items[index].plan = Some(plan);
        self.items[index].stage = DeletionWorkStage::QueuedExecution;
        true
    }

    pub(crate) fn planning_cancelled(&mut self, work_id: DeletionWorkId) -> bool {
        self.finish_planning(work_id, WorkCompletion::Cancelled)
    }

    pub(crate) fn planning_failed(&mut self, work_id: DeletionWorkId) -> bool {
        self.finish_planning(work_id, WorkCompletion::Failed)
    }

    /// A changed or missing target is never retried from prior consent.
    pub(crate) fn planning_stale(&mut self, work_id: DeletionWorkId) -> bool {
        self.finish_planning(work_id, WorkCompletion::Invalidated)
    }

    #[must_use]
    pub(crate) fn take_next_confirmation(&mut self) -> Option<(DeletionWorkId, Box<FileToDelete>)> {
        let item = self
            .items
            .iter_mut()
            .find(|item| matches!(item.stage, DeletionWorkStage::AwaitingConfirmation { .. }))?;
        let DeletionWorkStage::AwaitingConfirmation {
            target,
            reduced_guardrails,
            maximum_bytes,
        } = std::mem::replace(
            &mut item.stage,
            DeletionWorkStage::Confirming {
                reduced_guardrails: false,
                maximum_bytes: 0,
            },
        )
        else {
            unreachable!("awaiting confirmation stage must remain awaiting confirmation")
        };
        item.stage = DeletionWorkStage::Confirming {
            reduced_guardrails,
            maximum_bytes,
        };
        Some((item.id, target))
    }

    pub(crate) fn return_confirmation(
        &mut self,
        work_id: DeletionWorkId,
        target: Box<FileToDelete>,
    ) -> bool {
        let Some(item) = self.items.iter_mut().find(|item| item.id == work_id) else {
            return false;
        };
        let DeletionWorkStage::Confirming {
            reduced_guardrails,
            maximum_bytes,
        } = &item.stage
        else {
            return false;
        };
        if item.path != target.full_path() {
            return false;
        }
        item.confirmed_at = None;
        item.stage = DeletionWorkStage::AwaitingConfirmation {
            target,
            reduced_guardrails: *reduced_guardrails,
            maximum_bytes: *maximum_bytes,
        };
        true
    }

    pub(crate) fn queue_confirmation(
        &mut self,
        work_id: DeletionWorkId,
        target: Box<FileToDelete>,
        now: Duration,
    ) -> bool {
        let Some(item) = self.items.iter_mut().find(|item| item.id == work_id) else {
            return false;
        };
        let DeletionWorkStage::Confirming {
            reduced_guardrails,
            maximum_bytes,
        } = &item.stage
        else {
            return false;
        };
        if item.path != target.full_path() {
            return false;
        }
        item.confirmed_at = Some(now);
        item.stage = DeletionWorkStage::QueuedPlanning {
            target,
            reduced_guardrails: *reduced_guardrails,
            maximum_bytes: *maximum_bytes,
        };
        true
    }

    /// Cancels an immediate foreground confirmation or signals an in-flight planner.
    pub(crate) fn cancel_modal(&mut self, work_id: DeletionWorkId) -> bool {
        let Some(index) = self.items.iter().position(|item| item.id == work_id) else {
            return false;
        };
        if matches!(self.items[index].stage, DeletionWorkStage::Planning) {
            self.items[index].stage = DeletionWorkStage::CancellingPlanning;
            return self.has_lease(work_id);
        }
        if matches!(
            self.items[index].stage,
            DeletionWorkStage::QueuedPlanning { .. }
                | DeletionWorkStage::AwaitingConfirmation { .. }
                | DeletionWorkStage::Confirming { .. }
                | DeletionWorkStage::QueuedExecution
        ) {
            let _ = self.items.remove(index);
        }
        false
    }

    /// Cancels every non-mutating operation. An in-flight planner remains
    /// leased until its worker acknowledges the cancellation.
    pub(crate) fn cancel_pending(&mut self) -> bool {
        let planning = self
            .items
            .iter()
            .filter(|item| matches!(item.stage, DeletionWorkStage::Planning))
            .map(|item| item.id)
            .collect::<Vec<_>>();
        for work_id in &planning {
            let item = self
                .items
                .iter_mut()
                .find(|item| item.id == *work_id)
                .expect("retained planning work should remain addressable");
            item.stage = DeletionWorkStage::CancellingPlanning;
        }
        let planner_cancelled = planning.iter().any(|work_id| self.has_lease(*work_id));
        self.items.retain(|item| {
            matches!(
                item.stage,
                DeletionWorkStage::Executing { .. } | DeletionWorkStage::CancellingPlanning
            )
        });
        planner_cancelled
    }

    pub(crate) fn execution_failed(&mut self, work_id: DeletionWorkId) -> bool {
        self.finish_execution(work_id, WorkCompletion::Failed)
    }

    /// A changed or missing target is never retried from the earlier consent.
    pub(crate) fn execution_stale(&mut self, work_id: DeletionWorkId) -> bool {
        self.finish_execution(work_id, WorkCompletion::Invalidated)
    }

    pub(crate) fn execution_finished(
        &mut self,
        work_id: DeletionWorkId,
        completion: WorkCompletion,
    ) -> bool {
        self.finish_execution(work_id, completion)
    }

    pub(crate) fn discard(&mut self, work_id: DeletionWorkId) -> bool {
        let _ = self.finish_lease(work_id, WorkCompletion::Cancelled);
        self.remove_if(work_id, |_| true)
    }

    /// Resolves a late cancellation after its item was already removed.
    pub(crate) fn discard_cancelled_event(&mut self, work_id: DeletionWorkId) {
        if !self.items.iter().any(|item| item.id == work_id) {
            let _ = self.finish_lease(work_id, WorkCompletion::Cancelled);
        }
    }

    #[must_use]
    pub(crate) fn has_work(&self) -> bool {
        self.summary().has_work()
    }

    #[must_use]
    pub(crate) fn has_background_activity(&self) -> bool {
        self.items.iter().any(|item| {
            matches!(
                item.stage,
                DeletionWorkStage::QueuedPlanning { .. }
                    | DeletionWorkStage::Planning
                    | DeletionWorkStage::QueuedExecution
                    | DeletionWorkStage::Executing { .. }
                    | DeletionWorkStage::CancellingPlanning
            )
        })
    }

    #[must_use]
    pub(crate) fn has_active_mutation(&self) -> bool {
        self.coordinator
            .snapshot()
            .is_ok_and(crate::scan_coordinator::SchedulerSnapshot::active_deletion_execution)
            && self
                .items
                .iter()
                .any(|item| matches!(item.stage, DeletionWorkStage::Executing { .. }))
    }

    /// Whether a newly confirmed target still needs checker-pattern frames.
    #[must_use]
    pub(crate) fn has_checker_animation(&self, now: Duration) -> bool {
        self.items.iter().any(|item| {
            matches!(
                item.stage,
                DeletionWorkStage::QueuedPlanning { .. }
                    | DeletionWorkStage::Planning
                    | DeletionWorkStage::QueuedExecution
                    | DeletionWorkStage::CancellingPlanning
            ) && item.confirmed_at.is_some_and(|confirmed_at| {
                now.saturating_sub(confirmed_at) < DELETION_CHECKER_COVER_DURATION
            })
        })
    }

    #[must_use]
    pub(crate) fn pending_count(&self) -> usize {
        self.summary().pending_operations
    }

    #[must_use]
    pub(crate) fn active_progress(&self) -> Option<(u64, Arc<AtomicU64>)> {
        self.items.iter().find_map(|item| {
            let DeletionWorkStage::Executing {
                planned_entries,
                progress,
            } = &item.stage
            else {
                return None;
            };
            Some((*planned_entries, Arc::clone(progress)))
        })
    }

    /// Returns presentation data only when the scan root and relative target path match.
    ///
    /// Snapshot page node IDs are page-local, so they cannot identify work after navigation.
    #[must_use]
    pub(crate) fn rail_item_for_relative_path(
        &self,
        scan_root: &Path,
        relative_path: &RelativePath,
    ) -> Option<WorkRailItem<'_>> {
        self.items
            .iter()
            .find(|item| {
                item.target
                    .as_ref()
                    .is_some_and(|target| target.path_in_filesystem.as_path() == scan_root)
                    && item.relative_path == *relative_path
            })
            .map(work_rail_item)
    }

    #[must_use]
    pub(crate) fn len(&self) -> usize {
        self.items.len()
    }

    #[must_use]
    pub(crate) fn rail_item(&self, index: usize) -> Option<WorkRailItem<'_>> {
        self.items.get(index).map(work_rail_item)
    }

    #[must_use]
    pub(crate) fn foreground_rail_item(&self) -> Option<WorkRailItem<'_>> {
        let active = self
            .items
            .iter()
            .position(|item| matches!(item.stage, DeletionWorkStage::Executing { .. }));
        self.rail_item(active.unwrap_or(0))
    }

    #[must_use]
    pub(crate) fn summary(&self) -> DeletionWorkSummary {
        let mut summary = DeletionWorkSummary::default();
        for item in &self.items {
            match &item.stage {
                DeletionWorkStage::Executing {
                    planned_entries,
                    progress,
                } => {
                    summary.mutating = true;
                    summary.planned_entries = Some(*planned_entries);
                    summary.completed_entries = Some(progress.load(Ordering::Acquire));
                }
                _ => {
                    summary.pending_operations = summary.pending_operations.saturating_add(1);
                }
            }
        }
        summary
    }

    fn next_work_id(&mut self) -> DeletionWorkId {
        self.next_id = self.next_id.wrapping_add(1);
        if self.next_id == 0 {
            self.next_id = 1;
        }
        DeletionWorkId(self.next_id)
    }

    fn acquire_lease(
        &mut self,
        kind: WorkKind,
        relative_path: RelativePath,
        priority: WorkPriority,
    ) -> Option<WorkLease> {
        self.coordinator
            .acquire(kind, relative_path, priority)
            .ok()
            .flatten()
    }

    fn remember_lease(&mut self, work_id: DeletionWorkId, lease: WorkLease) {
        if let Some((_, current)) = self.leases.iter_mut().find(|(id, _)| *id == work_id) {
            *current = lease;
        } else {
            debug_assert!(self.leases.len() < MAX_DELETION_WORK_ITEMS);
            self.leases.push((work_id, lease));
        }
    }

    fn has_lease(&self, work_id: DeletionWorkId) -> bool {
        self.leases.iter().any(|(id, _)| *id == work_id)
    }

    fn take_lease(&mut self, work_id: DeletionWorkId) -> Option<WorkLease> {
        let index = self.leases.iter().position(|(id, _)| *id == work_id)?;
        Some(self.leases.swap_remove(index).1)
    }

    fn finish_lease(&mut self, work_id: DeletionWorkId, completion: WorkCompletion) -> bool {
        let Some(lease) = self.take_lease(work_id) else {
            return false;
        };
        self.coordinator
            .finish(lease, completion)
            .is_ok_and(|outcome| outcome == CompletionOutcome::Accepted)
    }

    fn requeue_lease(&mut self, work_id: DeletionWorkId) -> bool {
        let Some(lease) = self.take_lease(work_id) else {
            return false;
        };
        self.coordinator
            .requeue(lease)
            .is_ok_and(|outcome| outcome == RequeueOutcome::Requeued)
    }

    fn finish_planning(&mut self, work_id: DeletionWorkId, completion: WorkCompletion) -> bool {
        let Some(index) = self.items.iter().position(|item| item.id == work_id) else {
            return false;
        };
        if !matches!(
            self.items[index].stage,
            DeletionWorkStage::Planning | DeletionWorkStage::CancellingPlanning
        ) {
            return false;
        }
        let accepted = self.finish_lease(work_id, completion);
        if accepted {
            let _ = self.items.remove(index);
        }
        accepted
    }

    fn finish_execution(&mut self, work_id: DeletionWorkId, completion: WorkCompletion) -> bool {
        let Some(index) = self.items.iter().position(|item| item.id == work_id) else {
            return false;
        };
        if !matches!(self.items[index].stage, DeletionWorkStage::Executing { .. }) {
            return false;
        }
        let accepted = self.finish_lease(work_id, completion);
        if accepted {
            let _ = self.items.remove(index);
        }
        accepted
    }

    fn remove_if(
        &mut self,
        work_id: DeletionWorkId,
        predicate: impl FnOnce(&DeletionWorkStage) -> bool,
    ) -> bool {
        let Some(index) = self.items.iter().position(|item| item.id == work_id) else {
            return false;
        };
        if !predicate(&self.items[index].stage) {
            return false;
        }
        let _ = self.items.remove(index);
        true
    }
}

fn work_rail_item(item: &DeletionWorkItem) -> WorkRailItem<'_> {
    let (status, planned_entries, completed) = match &item.stage {
        DeletionWorkStage::Executing {
            planned_entries,
            progress,
        } => (
            WorkRailStatus::Executing,
            Some(*planned_entries),
            Some(progress.as_ref()),
        ),
        stage => (work_rail_status(stage), None, None),
    };
    WorkRailItem {
        path: &item.label,
        status,
        planned_entries,
        completed,
        confirmed_at: item.confirmed_at,
    }
}

fn targets_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}
fn work_rail_status(stage: &DeletionWorkStage) -> WorkRailStatus {
    match stage {
        DeletionWorkStage::QueuedPlanning { .. }
        | DeletionWorkStage::Planning
        | DeletionWorkStage::CancellingPlanning => WorkRailStatus::Planning,
        DeletionWorkStage::AwaitingConfirmation { .. } | DeletionWorkStage::Confirming { .. } => {
            WorkRailStatus::AwaitingConfirmation
        }
        DeletionWorkStage::QueuedExecution => WorkRailStatus::Queued,
        DeletionWorkStage::Executing { .. } => WorkRailStatus::Executing,
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;
    use std::time::Duration;

    use crate::model::{EntrySnapshot, NodeId, NodeKind};
    use crate::state::tiles::FileType;

    use super::*;

    fn target(path: &[&str]) -> FileToDelete {
        FileToDelete {
            node_id: NodeId(1),
            synthetic: false,
            path_in_filesystem: PathBuf::from("/scan-root"),
            path_to_file: path.iter().map(OsString::from).collect(),
            file_type: FileType::File,
            num_descendants: None,
            size: 0,
            expected_snapshot: EntrySnapshot {
                identity: None,
                kind: NodeKind::File,
                apparent_bytes: 0,
                allocated_bytes: None,
                modified_nanos: None,
            },
            reviewed_entries: Vec::new(),
        }
    }
    fn relative(path: &[&str]) -> RelativePath {
        RelativePath::from_components(path.iter().map(OsString::from).collect())
            .expect("test path should be relative")
    }

    fn coordinator_snapshot(work: &DeletionWork) -> crate::scan_coordinator::SchedulerSnapshot {
        work.coordinator
            .snapshot()
            .expect("session coordinator should remain available")
    }

    #[test]
    fn queue_rejects_overlapping_targets_and_enforces_its_capacity() {
        let mut work = DeletionWork::new();
        work.enqueue_confirmation(target(&["first"]), false, 1024, Duration::ZERO)
            .expect("first target should fit");
        assert_eq!(
            work.enqueue_confirmation(target(&["first", "child"]), false, 1024, Duration::ZERO),
            Err(DeletionWorkError::OverlappingTarget)
        );
        work.enqueue_confirmation(target(&["second"]), false, 1024, Duration::ZERO)
            .expect("second target should fit");
        work.enqueue_confirmation(target(&["third"]), false, 1024, Duration::ZERO)
            .expect("third target should fit");
        work.enqueue_confirmation(target(&["fourth"]), false, 1024, Duration::ZERO)
            .expect("fourth target should fit");
        assert_eq!(
            work.enqueue_confirmation(target(&["fifth"]), false, 1024, Duration::ZERO),
            Err(DeletionWorkError::QueueFull)
        );
    }

    #[test]
    fn cancellation_retains_the_planner_reservation_until_its_event_arrives() {
        let mut work = DeletionWork::new();
        let work_id = work
            .enqueue_confirmation(target(&["first"]), true, 1024, Duration::ZERO)
            .expect("target should queue");
        let command = work
            .next_planning_command()
            .expect("planner should receive the first command");
        assert!(matches!(command, DeletionWorkCommand::Plan { .. }));
        assert!(work.cancel_modal(work_id));
        assert_eq!(work.summary().pending_operations, 1);
        assert!(work.next_planning_command().is_none());

        assert!(work.planning_cancelled(work_id));
        assert!(!work.summary().has_work());
    }

    #[test]
    fn planner_work_is_requeued_and_resolved_through_the_coordinator_ledger() {
        let mut work = DeletionWork::new();
        let work_id = work
            .enqueue_confirmation(target(&["first"]), true, 1024, Duration::ZERO)
            .expect("target should queue");
        let command = work
            .next_planning_command()
            .expect("planner should receive a leased command");
        assert!(matches!(command, DeletionWorkCommand::Plan { .. }));
        let lease = work
            .leases
            .iter()
            .find(|(id, _)| *id == work_id)
            .map(|(_, lease)| lease)
            .expect("planning work should retain its coordinator lease");
        assert_eq!(lease.key().kind(), WorkKind::PlanDeletion);
        assert_eq!(coordinator_snapshot(&work).active_leases(), 1);

        work.restore_unsubmitted(command);
        assert_eq!(coordinator_snapshot(&work).active_leases(), 0);
        assert_eq!(coordinator_snapshot(&work).pending().foreground(), 1);

        let _ = work
            .next_planning_command()
            .expect("requeued planning work should lease again");
        assert!(work.planning_failed(work_id));
        let snapshot = coordinator_snapshot(&work);
        assert_eq!(snapshot.active_leases(), 0);
        assert_eq!(snapshot.terminal().failed(), 1);
    }

    #[test]
    fn planner_lane_can_start_a_later_nonoverlapping_request_while_execution_runs() {
        let mut work = DeletionWork::new();
        let first = work
            .enqueue_confirmation(target(&["first"]), true, 1024, Duration::ZERO)
            .expect("first target should queue");
        let second = work
            .enqueue_confirmation(target(&["second"]), true, 1024, Duration::ZERO)
            .expect("second target should queue");
        let first_command = work
            .next_planning_command()
            .expect("first planner command should start");
        let DeletionWorkCommand::Plan { target, .. } = first_command else {
            panic!("first operation should plan")
        };
        let item = work
            .items
            .iter_mut()
            .find(|item| item.id == first)
            .expect("first item should remain retained");
        item.stage = DeletionWorkStage::Executing {
            planned_entries: 1,
            progress: Arc::new(AtomicU64::new(0)),
        };

        assert_eq!(target.full_path(), PathBuf::from("/scan-root/first"));
        let later = work
            .next_planning_command()
            .expect("later planning must not wait for the executor lane");
        assert!(matches!(later, DeletionWorkCommand::Plan { work_id, .. } if work_id == second));
    }

    #[test]
    fn confirmation_precedes_planning_and_targets_are_status_addressable() {
        let mut work = DeletionWork::new();
        let target = target(&["first"]);
        let target_path = relative(&["first"]);
        let work_id = work
            .enqueue_confirmation(target, false, 1024, Duration::ZERO)
            .expect("confirmation should queue");
        assert!(work.next_planning_command().is_none());
        assert_eq!(
            work.rail_item_for_relative_path(Path::new("/scan-root"), &target_path)
                .map(|item| item.status),
            Some(WorkRailStatus::AwaitingConfirmation)
        );

        let (shown_id, target) = work
            .take_next_confirmation()
            .expect("target should surface before planning");
        assert_eq!(shown_id, work_id);
        assert!(work.queue_confirmation(work_id, target, Duration::ZERO));
        assert_eq!(
            work.rail_item_for_relative_path(Path::new("/scan-root"), &target_path)
                .map(|item| item.status),
            Some(WorkRailStatus::Planning)
        );
        assert!(matches!(
            work.next_planning_command(),
            Some(DeletionWorkCommand::Plan { work_id: id, .. }) if id == work_id
        ));
    }

    #[test]
    fn page_local_node_id_does_not_mark_an_unrelated_path_as_deletion_work() {
        let mut work = DeletionWork::new();
        let mut target = target(&["Development", "kotlin"]);
        target.node_id = NodeId(2);
        work.enqueue_confirmation(target, true, 1024, Duration::ZERO)
            .expect("target should queue");

        assert!(
            work.rail_item_for_relative_path(
                Path::new("/scan-root"),
                &relative(&[".cache", "unrelated"]),
            )
            .is_none()
        );
        assert_eq!(
            work.rail_item_for_relative_path(
                Path::new("/scan-root"),
                &relative(&["Development", "kotlin"]),
            )
            .map(|item| item.status),
            Some(WorkRailStatus::Planning)
        );
    }

    #[test]
    fn path_rail_item_exposes_execution_progress() {
        let mut work = DeletionWork::new();
        let target = target(&["target"]);
        let target_path = relative(&["target"]);
        let work_id = work
            .enqueue_confirmation(target, true, 1024, Duration::ZERO)
            .expect("work should queue");
        let _ = work
            .next_planning_command()
            .expect("planner command should start");
        let progress = Arc::new(AtomicU64::new(3));
        let item = work
            .items
            .iter_mut()
            .find(|item| item.id == work_id)
            .expect("work should remain retained");
        item.stage = DeletionWorkStage::Executing {
            planned_entries: 8,
            progress: Arc::clone(&progress),
        };

        let rail = work
            .rail_item_for_relative_path(Path::new("/scan-root"), &target_path)
            .expect("target path should retain its execution state");
        assert_eq!(rail.status, WorkRailStatus::Executing);
        assert_eq!(rail.planned_entries, Some(8));
        assert_eq!(
            rail.completed
                .map(|completed| completed.load(std::sync::atomic::Ordering::Acquire)),
            Some(3)
        );
    }
}
