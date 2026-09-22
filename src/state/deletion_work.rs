use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::deletion::DeletionPlan;

use super::file_to_delete::FileToDelete;

pub(crate) const MAX_DELETION_WORK_ITEMS: usize = 4;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct DeletionWorkId(pub(crate) u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeletionWorkSummary {
    /// Operations that can still be cancelled without interrupting a mutation.
    pub pending_operations: usize,
    /// At most one worker may mutate the file system at a time.
    pub mutating: bool,
    /// The active mutation's reviewed-entry count, when one is running.
    pub planned_entries: Option<u64>,
    /// The active mutation's live completion count, when one is running.
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

pub(crate) enum DeletionWorkCommand {
    Plan {
        work_id: DeletionWorkId,
        target: Box<FileToDelete>,
        reduced_guardrails: bool,
        maximum_bytes: usize,
    },
    Revalidate {
        work_id: DeletionWorkId,
        plan: Box<DeletionPlan>,
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
            Self::Plan { work_id, .. }
            | Self::Revalidate { work_id, .. }
            | Self::Execute { work_id, .. } => *work_id,
        }
    }
}

pub(crate) struct DeletionWork {
    items: VecDeque<DeletionWorkItem>,
    in_flight: Option<DeletionWorkId>,
    next_id: u64,
}

struct DeletionWorkItem {
    id: DeletionWorkId,
    target: PathBuf,
    phase: DeletionWorkPhase,
}

enum DeletionWorkPhase {
    QueuedPlanning {
        target: Box<FileToDelete>,
        reduced_guardrails: bool,
        maximum_bytes: usize,
    },
    Planning,
    AwaitingConfirmation,
    QueuedRevalidation {
        plan: Box<DeletionPlan>,
        progress: Arc<AtomicU64>,
    },
    Revalidating {
        progress: Arc<AtomicU64>,
    },
    QueuedExecution {
        plan: Box<DeletionPlan>,
        progress: Arc<AtomicU64>,
    },
    Executing {
        planned_entries: u64,
        completed: Arc<AtomicU64>,
    },
}

impl Default for DeletionWork {
    fn default() -> Self {
        Self::new()
    }
}

impl DeletionWork {
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            items: VecDeque::with_capacity(MAX_DELETION_WORK_ITEMS),
            in_flight: None,
            next_id: 0,
        }
    }

    pub(crate) fn enqueue_planning(
        &mut self,
        target: FileToDelete,
        reduced_guardrails: bool,
        maximum_bytes: usize,
    ) -> Result<DeletionWorkId, DeletionWorkError> {
        if self.items.len() >= MAX_DELETION_WORK_ITEMS {
            return Err(DeletionWorkError::QueueFull);
        }
        let target_path = target.full_path();
        if self
            .items
            .iter()
            .any(|item| targets_overlap(&item.target, &target_path))
        {
            return Err(DeletionWorkError::OverlappingTarget);
        }
        let id = self.next_work_id();
        self.items.push_back(DeletionWorkItem {
            id,
            target: target_path,
            phase: DeletionWorkPhase::QueuedPlanning {
                target: Box::new(target),
                reduced_guardrails,
                maximum_bytes,
            },
        });
        Ok(id)
    }

    #[must_use]
    pub(crate) fn next_command(&mut self) -> Option<DeletionWorkCommand> {
        if self.in_flight.is_some() {
            return None;
        }
        let item = self.items.front_mut()?;
        let work_id = item.id;
        let command = match &mut item.phase {
            DeletionWorkPhase::QueuedPlanning { .. } => {
                let DeletionWorkPhase::QueuedPlanning {
                    target,
                    reduced_guardrails,
                    maximum_bytes,
                } = std::mem::replace(&mut item.phase, DeletionWorkPhase::Planning)
                else {
                    unreachable!("queued planning phase must remain queued planning")
                };
                DeletionWorkCommand::Plan {
                    work_id,
                    target,
                    reduced_guardrails,
                    maximum_bytes,
                }
            }
            DeletionWorkPhase::QueuedRevalidation { .. } => {
                let DeletionWorkPhase::QueuedRevalidation { plan, progress } =
                    std::mem::replace(&mut item.phase, DeletionWorkPhase::Planning)
                else {
                    unreachable!("queued revalidation phase must remain queued revalidation")
                };
                item.phase = DeletionWorkPhase::Revalidating {
                    progress: Arc::clone(&progress),
                };
                DeletionWorkCommand::Revalidate { work_id, plan }
            }
            DeletionWorkPhase::QueuedExecution { .. } => {
                let DeletionWorkPhase::QueuedExecution { plan, progress } =
                    std::mem::replace(&mut item.phase, DeletionWorkPhase::Planning)
                else {
                    unreachable!("queued execution phase must remain queued execution")
                };
                let planned_entries = plan.planned_entries();
                item.phase = DeletionWorkPhase::Executing {
                    planned_entries,
                    completed: Arc::clone(&progress),
                };
                DeletionWorkCommand::Execute {
                    work_id,
                    plan,
                    progress,
                }
            }
            DeletionWorkPhase::Planning
            | DeletionWorkPhase::AwaitingConfirmation
            | DeletionWorkPhase::Revalidating { .. }
            | DeletionWorkPhase::Executing { .. } => return None,
        };
        self.in_flight = Some(work_id);
        Some(command)
    }

    pub(crate) fn restore_unsubmitted(&mut self, command: DeletionWorkCommand) {
        let work_id = command.work_id();
        if self.in_flight != Some(work_id) {
            return;
        }
        let Some(item) = self.items.iter_mut().find(|item| item.id == work_id) else {
            self.in_flight = None;
            return;
        };
        match (command, &mut item.phase) {
            (
                DeletionWorkCommand::Plan {
                    target,
                    reduced_guardrails,
                    maximum_bytes,
                    ..
                },
                DeletionWorkPhase::Planning,
            ) => {
                item.phase = DeletionWorkPhase::QueuedPlanning {
                    target,
                    reduced_guardrails,
                    maximum_bytes,
                };
            }
            (
                DeletionWorkCommand::Revalidate { plan, .. },
                DeletionWorkPhase::Revalidating { progress },
            ) => {
                let progress = Arc::clone(progress);
                item.phase = DeletionWorkPhase::QueuedRevalidation { plan, progress };
            }
            (
                DeletionWorkCommand::Execute { plan, progress, .. },
                DeletionWorkPhase::Executing { .. },
            ) => {
                item.phase = DeletionWorkPhase::QueuedExecution { plan, progress };
            }
            _ => return,
        }
        self.in_flight = None;
    }

    pub(crate) fn planning_succeeded(&mut self, work_id: DeletionWorkId) -> bool {
        if self.in_flight != Some(work_id) {
            return false;
        }
        let Some(item) = self.items.iter_mut().find(|item| item.id == work_id) else {
            return false;
        };
        if !matches!(item.phase, DeletionWorkPhase::Planning) {
            return false;
        }
        item.phase = DeletionWorkPhase::AwaitingConfirmation;
        self.in_flight = None;
        true
    }

    pub(crate) fn planning_failed(&mut self, work_id: DeletionWorkId) -> bool {
        self.remove_in_flight(work_id, DeletionWorkPhaseKind::Planning)
    }

    pub(crate) fn confirm(
        &mut self,
        work_id: DeletionWorkId,
        plan: Box<DeletionPlan>,
        progress: Arc<AtomicU64>,
    ) -> bool {
        let Some(index) = self.items.iter().position(|item| item.id == work_id) else {
            return false;
        };
        if !matches!(
            self.items[index].phase,
            DeletionWorkPhase::AwaitingConfirmation
        ) {
            return false;
        }
        if self.items[index].target != plan.target.full_path() {
            let _ = self.items.remove(index);
            return false;
        }
        self.items[index].phase = DeletionWorkPhase::QueuedRevalidation { plan, progress };
        true
    }

    pub(crate) fn revalidation_succeeded(
        &mut self,
        work_id: DeletionWorkId,
        plan: Box<DeletionPlan>,
    ) -> bool {
        if self.in_flight != Some(work_id) {
            return false;
        }
        let Some(index) = self.items.iter().position(|item| item.id == work_id) else {
            return false;
        };
        let progress = match &self.items[index].phase {
            DeletionWorkPhase::Revalidating { progress } => Arc::clone(progress),
            _ => return false,
        };
        if self.items[index].target != plan.target.full_path() {
            let _ = self.items.remove(index);
            self.in_flight = None;
            return false;
        }
        self.items[index].phase = DeletionWorkPhase::QueuedExecution { plan, progress };
        self.in_flight = None;
        true
    }

    pub(crate) fn revalidation_failed(&mut self, work_id: DeletionWorkId) -> bool {
        self.remove_in_flight(work_id, DeletionWorkPhaseKind::Revalidating)
    }

    pub(crate) fn execution_finished(&mut self, work_id: DeletionWorkId) -> bool {
        self.remove_in_flight(work_id, DeletionWorkPhaseKind::Executing)
    }

    /// Cancels only an operation still represented by the foreground deletion modal.
    ///
    /// Returns whether a planning worker must receive its cancellation signal.
    pub(crate) fn cancel_modal(&mut self, work_id: DeletionWorkId) -> bool {
        let Some(index) = self.items.iter().position(|item| item.id == work_id) else {
            return false;
        };
        let phase = &self.items[index].phase;
        if !matches!(
            phase,
            DeletionWorkPhase::QueuedPlanning { .. }
                | DeletionWorkPhase::Planning
                | DeletionWorkPhase::AwaitingConfirmation
        ) {
            return false;
        }
        let cancel_planning =
            self.in_flight == Some(work_id) && matches!(phase, DeletionWorkPhase::Planning);
        let _ = self.items.remove(index);
        cancel_planning
    }

    /// Drops work that has not reached a filesystem mutation. An in-flight worker is left
    /// reserved until it reports completion, so no later command can overlap it.
    pub(crate) fn cancel_pending(&mut self) {
        self.items
            .retain(|item| !matches!(item.phase, DeletionWorkPhase::Executing { .. }));
    }

    /// Releases the reservation left by a cancelled planner or revalidator after it reports.
    pub(crate) fn discard_cancelled_event(&mut self, work_id: DeletionWorkId) {
        if self.in_flight == Some(work_id) && !self.items.iter().any(|item| item.id == work_id) {
            self.in_flight = None;
        }
    }

    #[must_use]
    pub(crate) fn summary(&self) -> DeletionWorkSummary {
        let mut summary = DeletionWorkSummary::default();
        for item in &self.items {
            match &item.phase {
                DeletionWorkPhase::Executing {
                    planned_entries,
                    completed,
                } => {
                    summary.mutating = true;
                    summary.planned_entries = Some(*planned_entries);
                    summary.completed_entries = Some(completed.load(Ordering::Acquire));
                }
                DeletionWorkPhase::QueuedPlanning { .. }
                | DeletionWorkPhase::Planning
                | DeletionWorkPhase::AwaitingConfirmation
                | DeletionWorkPhase::QueuedRevalidation { .. }
                | DeletionWorkPhase::Revalidating { .. }
                | DeletionWorkPhase::QueuedExecution { .. } => {
                    summary.pending_operations = summary.pending_operations.saturating_add(1);
                }
            }
        }
        if self
            .in_flight
            .is_some_and(|work_id| !self.items.iter().any(|item| item.id == work_id))
        {
            summary.pending_operations = summary.pending_operations.saturating_add(1);
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

    fn remove_in_flight(&mut self, work_id: DeletionWorkId, phase: DeletionWorkPhaseKind) -> bool {
        if self.in_flight != Some(work_id) {
            return false;
        }
        let Some(index) = self.items.iter().position(|item| item.id == work_id) else {
            return false;
        };
        if !phase.matches(&self.items[index].phase) {
            return false;
        }
        let _ = self.items.remove(index);
        self.in_flight = None;
        true
    }
}

#[derive(Clone, Copy)]
enum DeletionWorkPhaseKind {
    Planning,
    Revalidating,
    Executing,
}

impl DeletionWorkPhaseKind {
    const fn matches(self, phase: &DeletionWorkPhase) -> bool {
        matches!(
            (self, phase),
            (Self::Planning, DeletionWorkPhase::Planning)
                | (Self::Revalidating, DeletionWorkPhase::Revalidating { .. })
                | (Self::Executing, DeletionWorkPhase::Executing { .. })
        )
    }
}

fn targets_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::PathBuf;
    #[cfg(unix)]
    use std::sync::Arc;
    #[cfg(unix)]
    use std::sync::atomic::AtomicU64;

    #[cfg(unix)]
    use std::path::Path;

    use super::*;
    #[cfg(unix)]
    use crate::deletion::{PlannedKind, PlannedSnapshot, ReviewedEntry, build_plan};
    use crate::model::{EntrySnapshot, NodeId, NodeKind};
    #[cfg(unix)]
    use crate::native_path::identity_for;
    use crate::state::tiles::FileType;

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

    #[test]
    fn queue_rejects_overlapping_targets_and_enforces_its_capacity() {
        let mut work = DeletionWork::new();
        work.enqueue_planning(target(&["first"]), false, 1024)
            .expect("first target should fit");
        assert_eq!(
            work.enqueue_planning(target(&["first", "child"]), false, 1024),
            Err(DeletionWorkError::OverlappingTarget)
        );
        work.enqueue_planning(target(&["second"]), false, 1024)
            .expect("second target should fit");
        work.enqueue_planning(target(&["third"]), false, 1024)
            .expect("third target should fit");
        work.enqueue_planning(target(&["fourth"]), false, 1024)
            .expect("fourth target should fit");
        assert_eq!(
            work.enqueue_planning(target(&["fifth"]), false, 1024),
            Err(DeletionWorkError::QueueFull)
        );
    }

    #[cfg(unix)]
    fn file_plan(root: &Path, name: &str) -> DeletionPlan {
        use std::os::unix::fs::MetadataExt as _;
        use std::time::UNIX_EPOCH;

        let path = root.join(name);
        std::fs::write(&path, b"file").expect("test target should be written");
        let metadata = std::fs::symlink_metadata(&path).expect("test target metadata should exist");
        let identity = identity_for(&path, &metadata)
            .expect("test target identity should be readable")
            .expect("test target should not be a link");
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
            node_id: NodeId(2),
            synthetic: false,
            path_in_filesystem: root.to_path_buf(),
            path_to_file: vec![OsString::from(name)],
            file_type: FileType::File,
            num_descendants: None,
            size: snapshot.apparent_bytes,
            expected_snapshot: EntrySnapshot {
                identity: Some(identity),
                kind: NodeKind::File,
                apparent_bytes: snapshot.apparent_bytes,
                allocated_bytes: snapshot.allocated_bytes,
                modified_nanos: snapshot.modified_nanos,
            },
            reviewed_entries: vec![ReviewedEntry {
                relative_path: PathBuf::from(name),
                snapshot,
            }],
        };
        build_plan(root, target, false).expect("test plan should build")
    }

    #[cfg(unix)]
    #[test]
    fn revalidation_is_followed_immediately_by_its_serial_mutation() {
        let root = tempfile::tempdir().expect("test root should exist");
        let plan = Box::new(file_plan(root.path(), "first"));
        let first_target = plan.target.clone();
        let mut work = DeletionWork::new();
        let first = work
            .enqueue_planning(first_target, false, 1024)
            .expect("first target should queue");
        work.enqueue_planning(target(&["second"]), false, 1024)
            .expect("second target should queue");

        assert!(matches!(
            work.next_command(),
            Some(DeletionWorkCommand::Plan { work_id, .. }) if work_id == first
        ));
        assert!(work.planning_succeeded(first));
        let progress = Arc::new(AtomicU64::new(0));
        assert!(work.confirm(first, plan, Arc::clone(&progress)));
        let plan = match work.next_command() {
            Some(DeletionWorkCommand::Revalidate { work_id, plan }) if work_id == first => plan,
            Some(_) | None => panic!("first operation should revalidate before later planning"),
        };
        assert!(work.revalidation_succeeded(first, plan));

        let execution_progress = match work.next_command() {
            Some(DeletionWorkCommand::Execute {
                work_id, progress, ..
            }) if work_id == first => progress,
            Some(_) | None => panic!("revalidated work should execute before later planning"),
        };
        assert!(Arc::ptr_eq(&progress, &execution_progress));
        assert!(work.summary().mutating);
        assert_eq!(work.summary().pending_operations, 1);
    }

    #[test]
    fn cancellation_retains_the_worker_reservation_until_its_event_arrives() {
        let mut work = DeletionWork::new();
        let work_id = work
            .enqueue_planning(target(&["first"]), false, 1024)
            .expect("target should queue");
        let _ = work.next_command();
        assert!(work.cancel_modal(work_id));
        assert_eq!(work.summary().pending_operations, 1);
        assert!(work.next_command().is_none());

        work.discard_cancelled_event(work_id);
        assert!(!work.summary().has_work());
    }
}
