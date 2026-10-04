//! Observation points on the deletion lifecycle, for the `deletion_lifecycle` fuzz target.
//!
//! This module exists only with the `fuzzing` feature. The library without it, and the release
//! binary, contain neither the module nor the calls to it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use crate::deletion::{DeletionPlan, DeletionReport, PlannedEntry};
use crate::state::deletion_work::DeletionWork;

/// What the planner reviewed for one plan, copied out for the observer.
#[derive(Clone, Debug)]
pub struct ReviewedPlan {
    /// The absolute path of the plan's target.
    pub target: PathBuf,
    /// Every entry the planner reviewed, or `None` when the plan keeps them in a file.
    pub entries: Option<Vec<PlannedEntry>>,
}

impl ReviewedPlan {
    fn of(plan: &DeletionPlan) -> Self {
        Self {
            target: plan.target.full_path(),
            entries: plan.reviewed_entries_for_probe(),
        }
    }
}

/// What the owner loop and the deletion executor tell an observer about each deletion.
///
/// `requested` and `confirmed` run on the owner loop's thread, while it handles the key the
/// interface acted on. `before_execution` and `after_execution` run on the executor's own thread,
/// one plan at a time, and the executor does nothing else until they return. An observer may
/// therefore read, and change, the file system from the executor's two: no other deletion is in
/// flight, and nothing the executor does overlaps it.
pub trait DeletionProbe: Send + Sync {
    /// The interface accepted a request to delete `target`, and the work queue gave the request
    /// work item `work_id` (`None` if the queue holds no item for it, which is a bug). With
    /// `reduced_guardrails` the request is also the consent: no confirming key follows, and the
    /// work item is the one it pays for.
    fn requested(&self, work_id: Option<u64>, target: &Path, reduced_guardrails: bool);

    /// The interface accepted the confirmation of work item `work_id`, which asked to delete
    /// `target`.
    fn confirmed(&self, work_id: u64, target: &Path);

    /// A confirmed plan, work item `work_id`, reached the executor. Its final whole-plan check has
    /// not run: whatever the observer changes now is a change after the planner's review.
    fn before_execution(&self, work_id: u64, plan: &ReviewedPlan);

    /// The executor ended work item `work_id`: with its report, or with `None` when the final
    /// whole-plan check refused the plan and nothing was removed.
    fn after_execution(&self, work_id: u64, report: Option<&DeletionReport>);
}

static PROBE: Mutex<Option<Arc<dyn DeletionProbe>>> = Mutex::new(None);

/// Keeps a probe installed. Dropping it removes the probe.
#[must_use = "the probe is removed as soon as the guard is dropped"]
pub struct ProbeGuard(());

impl Drop for ProbeGuard {
    fn drop(&mut self) {
        *PROBE.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }
}

/// Installs `probe` for every owner loop and executor in the process, replacing any earlier one.
pub fn install_probe(probe: Arc<dyn DeletionProbe>) -> ProbeGuard {
    *PROBE.lock().unwrap_or_else(PoisonError::into_inner) = Some(probe);
    ProbeGuard(())
}

/// The installed probe, cloned out of the lock so that a probe may itself call back into this
/// module without deadlocking.
fn installed() -> Option<Arc<dyn DeletionProbe>> {
    PROBE.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

pub(crate) fn requested(work: &DeletionWork, target: &Path, reduced_guardrails: bool) {
    if let Some(probe) = installed() {
        probe.requested(work.id_for_probe(target), target, reduced_guardrails);
    }
}

pub(crate) fn confirmed(work_id: u64, target: &Path) {
    if let Some(probe) = installed() {
        probe.confirmed(work_id, target);
    }
}

pub(crate) fn before_execution(work_id: u64, plan: &DeletionPlan) {
    if let Some(probe) = installed() {
        probe.before_execution(work_id, &ReviewedPlan::of(plan));
    }
}

pub(crate) fn after_execution(work_id: u64, report: Option<&DeletionReport>) {
    if let Some(probe) = installed() {
        probe.after_execution(work_id, report);
    }
}
