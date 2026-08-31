use std::collections::{HashMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::File;
use std::io;
use std::mem::size_of;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::UNIX_EPOCH;

use cap_primitives::ambient_authority;
use cap_primitives::fs::{self as cap_fs, FollowSymlinks};
use file_id::FileId;
use serde::{Deserialize, Serialize};
#[cfg(not(unix))]
use sysinfo::{DiskRefreshKind, Disks};

use crate::model::NodeId;
use crate::model::NodeKind;
use crate::native_path::{
    NativeIdentity, identity_for, safe_display_os_str, safe_display_path_text, safe_display_text,
};
use crate::state::FileToDelete;

pub const DEFAULT_PLAN_LIMIT_BYTES: usize = 64 * 1024 * 1024;

#[must_use]
pub const fn deletion_supported() -> bool {
    cfg!(any(target_os = "linux", target_vendor = "apple", windows))
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PlannedKind {
    Directory,
    File,
    Link,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlannedSnapshot {
    pub identity: NativeIdentity,
    pub kind: PlannedKind,
    pub apparent_bytes: u128,
    pub allocated_bytes: Option<u128>,
    pub modified_nanos: Option<u128>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewedEntry {
    pub relative_path: PathBuf,
    pub snapshot: PlannedSnapshot,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlannedEntry {
    pub relative_path: PathBuf,
    pub snapshot: PlannedSnapshot,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConfirmationChallenge {
    ConfirmFile,
    TypeName(String),
    TypePhrase(String),
    ReducedGuard,
}

impl ConfirmationChallenge {
    #[must_use]
    pub fn expected_input(&self) -> &str {
        match self {
            Self::ConfirmFile | Self::ReducedGuard => "y",
            Self::TypeName(name) | Self::TypePhrase(name) => name,
        }
    }
}

#[derive(Clone, Debug)]
pub struct DeletionPlan {
    pub target: FileToDelete,
    pub root_relative_path: PathBuf,
    pub scan_root_identity: NativeIdentity,
    pub entries: Vec<PlannedEntry>,
    pub challenge: ConfirmationChallenge,
    pub apparent_bytes: u128,
    pub estimated_bytes: usize,
}

impl DeletionPlan {
    #[must_use]
    pub fn planned_entries(&self) -> u64 {
        u64::try_from(self.entries.len()).unwrap_or(u64::MAX)
    }

    #[must_use]
    pub fn root_snapshot(&self) -> Option<&PlannedSnapshot> {
        self.entries
            .iter()
            .find(|entry| entry.relative_path == self.root_relative_path)
            .map(|entry| &entry.snapshot)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeletionEntryOutcome {
    Deleted,
    Changed(String),
    Missing,
    Failed(String),
    Unattempted,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeletionEntryResult {
    pub entry: PlannedEntry,
    pub outcome: DeletionEntryOutcome,
}

#[derive(Clone, Debug)]
pub struct DeletionReport {
    pub target_node_id: NodeId,
    pub root_relative_path: PathBuf,
    pub scan_root: PathBuf,
    pub entries: Vec<DeletionEntryResult>,
    pub soft_cancelled: bool,
    pub precise: bool,
    pub estimated_bytes: usize,
}

impl DeletionReport {
    #[must_use]
    pub fn deleted_entries(&self) -> u64 {
        self.count(|outcome| matches!(outcome, DeletionEntryOutcome::Deleted))
    }

    #[must_use]
    pub fn changed_entries(&self) -> u64 {
        self.count(|outcome| matches!(outcome, DeletionEntryOutcome::Changed(_)))
    }

    #[must_use]
    pub fn missing_entries(&self) -> u64 {
        self.count(|outcome| matches!(outcome, DeletionEntryOutcome::Missing))
    }

    #[must_use]
    pub fn failed_entries(&self) -> u64 {
        self.count(|outcome| matches!(outcome, DeletionEntryOutcome::Failed(_)))
    }

    #[must_use]
    pub fn unattempted_entries(&self) -> u64 {
        self.count(|outcome| matches!(outcome, DeletionEntryOutcome::Unattempted))
    }

    #[must_use]
    pub fn deleted_apparent_bytes(&self) -> u128 {
        self.entries
            .iter()
            .filter(|entry| matches!(entry.outcome, DeletionEntryOutcome::Deleted))
            .fold(0_u128, |total, entry| {
                total.saturating_add(entry.entry.snapshot.apparent_bytes)
            })
    }
    #[must_use]
    pub fn deleted_allocated_bytes(&self) -> u128 {
        let mut allocations = HashMap::<FileId, (u64, Option<u64>, Option<u128>)>::new();
        for result in &self.entries {
            if !matches!(result.outcome, DeletionEntryOutcome::Deleted) {
                continue;
            }
            let snapshot = &result.entry.snapshot;
            let allocation = allocations.entry(snapshot.identity.file_id).or_insert((
                0,
                snapshot.identity.link_count,
                snapshot.allocated_bytes,
            ));
            allocation.0 = allocation.0.saturating_add(1);
            allocation.1 = match (allocation.1, snapshot.identity.link_count) {
                (Some(left), Some(right)) => Some(left.max(right)),
                _ => None,
            };
            allocation.2 = match (allocation.2, snapshot.allocated_bytes) {
                (Some(left), Some(right)) => Some(left.max(right)),
                (left, None) | (None, left) => left,
            };
        }
        allocations
            .values()
            .filter_map(|(deleted, links, allocated)| {
                links
                    .filter(|links| *links > 0 && *deleted >= *links)
                    .and(*allocated)
            })
            .fold(0_u128, u128::saturating_add)
    }

    fn count(&self, predicate: impl Fn(&DeletionEntryOutcome) -> bool) -> u64 {
        u64::try_from(
            self.entries
                .iter()
                .filter(|entry| predicate(&entry.outcome))
                .count(),
        )
        .unwrap_or(u64::MAX)
    }
}

#[derive(Clone, Debug)]
pub enum DeletionPlanError {
    Synthetic,
    Root,
    InvalidRelativePath,
    Changed,
    Missing(PathBuf),
    Cancelled,
    MemoryLimit {
        limit: usize,
    },
    Io {
        path: String,
        message: String,
        kind: io::ErrorKind,
    },
}
impl fmt::Display for DeletionPlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Synthetic => {
                formatter.write_str("aggregate and synthetic nodes cannot be deleted")
            }
            Self::Root => formatter.write_str("scan roots and filesystem roots cannot be deleted"),
            Self::InvalidRelativePath => {
                formatter.write_str("deletion target is not a safe relative path")
            }
            Self::Changed => {
                formatter.write_str("deletion target changed while its plan was built")
            }
            Self::Missing(path) => write!(
                formatter,
                "planned deletion entry is missing: {}",
                safe_display_path_text(path)
            ),
            Self::Cancelled => formatter.write_str("deletion planning was cancelled"),
            Self::MemoryLimit { limit } => write!(
                formatter,
                "deletion plan exceeds its {limit} byte memory limit"
            ),
            Self::Io { path, message, .. } => write!(
                formatter,
                "deletion planning failed for {}: {}",
                safe_display_text(path),
                safe_display_text(message)
            ),
        }
    }
}

impl std::error::Error for DeletionPlanError {}

impl DeletionPlanError {
    #[must_use]
    pub(crate) const fn is_stale(&self) -> bool {
        matches!(
            self,
            Self::Changed
                | Self::Io {
                    kind: io::ErrorKind::NotFound,
                    ..
                }
        )
    }

    #[must_use]
    pub(crate) const fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }

    #[must_use]
    pub(crate) const fn is_missing(&self) -> bool {
        matches!(self, Self::Missing(_))
    }

    #[must_use]
    pub(crate) fn is_missing_target(&self, plan: &DeletionPlan) -> bool {
        matches!(self, Self::Missing(path) if path == &plan.root_relative_path)
    }
}

/// Builds and revalidates an identity-bound deletion plan.
///
/// # Errors
/// Returns a planning error when the target is ineligible, changed, unreadable, cancelled, or
/// exceeds the bounded plan memory limit.
pub fn build_plan(
    scan_root: &Path,
    target: FileToDelete,
    reduced_guardrails: bool,
) -> Result<DeletionPlan, DeletionPlanError> {
    let scan_root_identity = current_scan_root_identity(scan_root)?;
    build_plan_cancellable_with_root_identity(
        scan_root,
        scan_root_identity,
        target,
        reduced_guardrails,
        &AtomicBool::new(false),
        DEFAULT_PLAN_LIMIT_BYTES,
    )
}

/// Builds an identity-bound deletion plan with explicit cancellation and memory limits.
///
/// # Errors
/// Returns a planning error when the target is ineligible, changed, unreadable, cancelled, or
/// exceeds `maximum_bytes`.
pub fn build_plan_cancellable(
    scan_root: &Path,
    target: FileToDelete,
    reduced_guardrails: bool,
    cancelled: &AtomicBool,
    maximum_bytes: usize,
) -> Result<DeletionPlan, DeletionPlanError> {
    let scan_root_identity = current_scan_root_identity(scan_root)?;
    build_plan_cancellable_with_root_identity(
        scan_root,
        scan_root_identity,
        target,
        reduced_guardrails,
        cancelled,
        maximum_bytes,
    )
}

#[allow(clippy::too_many_lines)]
pub(crate) fn build_plan_cancellable_with_root_identity(
    scan_root: &Path,
    scan_root_identity: NativeIdentity,
    mut target: FileToDelete,
    reduced_guardrails: bool,
    cancelled: &AtomicBool,
    maximum_bytes: usize,
) -> Result<DeletionPlan, DeletionPlanError> {
    if target.synthetic {
        return Err(DeletionPlanError::Synthetic);
    }
    let relative = relative_target(&target)?;
    let full_path = target.full_path();
    if target.expected_snapshot.kind == NodeKind::Directory {
        let mount_root = match is_mount_root(&full_path) {
            Ok(value) => value,
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            Err(error) => return Err(plan_io(&full_path, error)),
        };
        if mount_root {
            let unchanged = target
                .expected_snapshot
                .identity
                .as_ref()
                .is_some_and(|expected| {
                    current_scan_root_identity(&full_path)
                        .is_ok_and(|actual| same_object(expected, &actual))
                });
            return Err(if unchanged {
                DeletionPlanError::Root
            } else {
                DeletionPlanError::Changed
            });
        }
    }
    let root = open_root(scan_root, &scan_root_identity)?;
    let (snapshot, directory_handle) = inspect_relative(&root, &relative)?;
    validate_model_snapshot(&target, &snapshot)?;
    let challenge = challenge_for(&target, &snapshot, reduced_guardrails);
    let mut entries = Vec::new();
    let mut estimated_bytes = 0;

    if let Some(handle) = directory_handle {
        push_planned_entry(
            &mut entries,
            PlannedEntry {
                relative_path: relative.clone(),
                snapshot,
            },
            &mut estimated_bytes,
            maximum_bytes,
        )?;
        let mut pending = VecDeque::from([0_usize]);
        let mut root_handle = Some(handle);
        while let Some(index) = pending.pop_front() {
            if cancelled.load(Ordering::Acquire) {
                return Err(DeletionPlanError::Cancelled);
            }
            let relative_path = entries[index].relative_path.clone();
            let expected = entries[index].snapshot.clone();
            let handle = if index == 0 {
                root_handle
                    .take()
                    .ok_or(DeletionPlanError::InvalidRelativePath)?
            } else {
                let (actual, handle) = match inspect_relative(&root, &relative_path) {
                    Err(DeletionPlanError::Missing(_)) => return Err(DeletionPlanError::Changed),
                    result => result?,
                };
                if actual != expected {
                    return Err(DeletionPlanError::Changed);
                }
                handle.ok_or(DeletionPlanError::Changed)?
            };
            let read_dir =
                cap_fs::read_base_dir(&handle).map_err(|error| plan_io(&relative_path, error))?;
            for child in read_dir {
                if cancelled.load(Ordering::Acquire) {
                    return Err(DeletionPlanError::Cancelled);
                }
                let child = child.map_err(|error| plan_io(&relative_path, error))?;
                let name = child.file_name();
                validate_component(&name)?;
                let child_relative = relative_path.join(&name);
                let (child_snapshot, child_directory) =
                    inspect_child(&handle, &name, &child_relative)?;
                let directory = child_directory.is_some();
                drop(child_directory);
                let child_index = entries.len();
                push_planned_entry(
                    &mut entries,
                    PlannedEntry {
                        relative_path: child_relative,
                        snapshot: child_snapshot,
                    },
                    &mut estimated_bytes,
                    maximum_bytes,
                )?;
                if directory {
                    pending.push_back(child_index);
                }
            }
        }
        entries.sort_by(|left, right| {
            right
                .relative_path
                .components()
                .count()
                .cmp(&left.relative_path.components().count())
                .then_with(|| left.relative_path.cmp(&right.relative_path))
        });
    } else {
        push_planned_entry(
            &mut entries,
            PlannedEntry {
                relative_path: relative.clone(),
                snapshot,
            },
            &mut estimated_bytes,
            maximum_bytes,
        )?;
    }
    target
        .reviewed_entries
        .sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    if entries.len() != target.reviewed_entries.len()
        || entries.iter().any(|entry| {
            target
                .reviewed_entries
                .binary_search_by(|reviewed| reviewed.relative_path.cmp(&entry.relative_path))
                .ok()
                .and_then(|index| target.reviewed_entries.get(index))
                .is_none_or(|reviewed| reviewed.snapshot != entry.snapshot)
        })
    {
        return Err(DeletionPlanError::Changed);
    }
    estimated_bytes = estimated_bytes.saturating_mul(2);

    let apparent_bytes = entries.iter().fold(0_u128, |total, entry| {
        total.saturating_add(entry.snapshot.apparent_bytes)
    });
    let plan = DeletionPlan {
        target,
        root_relative_path: relative,
        scan_root_identity,
        entries,
        challenge,
        apparent_bytes,
        estimated_bytes,
    };
    revalidate_plan_cancellable(scan_root, &plan, cancelled)?;
    Ok(plan)
}

/// Revalidates every planned identity against the live filesystem.
///
/// # Errors
/// Returns an error if any entry changed, disappeared, became unreadable, or escaped the scan root.
pub fn revalidate_plan(scan_root: &Path, plan: &DeletionPlan) -> Result<(), DeletionPlanError> {
    revalidate_plan_cancellable(scan_root, plan, &AtomicBool::new(false))
}

pub(crate) fn revalidate_plan_cancellable(
    scan_root: &Path,
    plan: &DeletionPlan,
    cancelled: &AtomicBool,
) -> Result<(), DeletionPlanError> {
    let root = open_root(scan_root, &plan.scan_root_identity)?;
    for entry in &plan.entries {
        if cancelled.load(Ordering::Acquire) {
            return Err(DeletionPlanError::Cancelled);
        }
        let (actual, _) = match inspect_relative(&root, &entry.relative_path) {
            Err(DeletionPlanError::Missing(path)) if path == plan.root_relative_path => {
                return Err(DeletionPlanError::Missing(path));
            }
            Err(DeletionPlanError::Missing(_)) => return Err(DeletionPlanError::Changed),
            result => result?,
        };
        if actual != entry.snapshot {
            return Err(DeletionPlanError::Changed);
        }
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_vendor = "apple"))]
pub fn execute_plan(
    scan_root: &Path,
    plan: DeletionPlan,
    soft_cancelled: &AtomicBool,
    hard_cancelled: &AtomicBool,
) -> DeletionReport {
    execute_plan_unix(scan_root, plan, soft_cancelled, hard_cancelled)
}

#[cfg(windows)]
pub fn execute_plan(
    scan_root: &Path,
    plan: DeletionPlan,
    soft_cancelled: &AtomicBool,
    hard_cancelled: &AtomicBool,
) -> DeletionReport {
    execute_plan_windows(scan_root, plan, soft_cancelled, hard_cancelled)
}

#[cfg(not(any(target_os = "linux", target_vendor = "apple", windows)))]
pub fn execute_plan(
    scan_root: &Path,
    plan: DeletionPlan,
    _soft_cancelled: &AtomicBool,
    _hard_cancelled: &AtomicBool,
) -> DeletionReport {
    failed_report(
        scan_root,
        plan,
        "permanent deletion is unavailable on this target",
    )
}

#[cfg(any(target_os = "linux", target_vendor = "apple"))]
fn execute_plan_unix(
    scan_root: &Path,
    plan: DeletionPlan,
    soft_cancelled: &AtomicBool,
    hard_cancelled: &AtomicBool,
) -> DeletionReport {
    execute_plan_unix_with_hook(scan_root, plan, soft_cancelled, hard_cancelled, || {})
}

#[cfg(any(target_os = "linux", target_vendor = "apple"))]
fn execute_plan_unix_with_hook<F>(
    scan_root: &Path,
    plan: DeletionPlan,
    soft_cancelled: &AtomicBool,
    hard_cancelled: &AtomicBool,
    after_isolation: F,
) -> DeletionReport
where
    F: FnMut(),
{
    execute_plan_unix_with_hooks(
        scan_root,
        plan,
        soft_cancelled,
        hard_cancelled,
        after_isolation,
        |_| {},
    )
}

#[cfg(any(target_os = "linux", target_vendor = "apple"))]
fn execute_plan_unix_with_hooks<F, G>(
    scan_root: &Path,
    plan: DeletionPlan,
    soft_cancelled: &AtomicBool,
    hard_cancelled: &AtomicBool,
    mut after_isolation: F,
    mut after_inspection: G,
) -> DeletionReport
where
    F: FnMut(),
    G: FnMut(&OsStr),
{
    let root = match open_root(scan_root, &plan.scan_root_identity) {
        Ok(root) => root,
        Err(error) => return failed_report(scan_root, plan, &error.to_string()),
    };
    let estimated_bytes = plan.estimated_bytes;
    let target_node_id = plan.target.node_id;
    let root_relative_path = plan.root_relative_path.clone();
    let planned_link_counts = planned_link_counts(&plan.entries);
    let mut deleted_link_counts = HashMap::new();
    let mut results = Vec::with_capacity(plan.entries.len());
    let mut stopped = false;
    for mut entry in plan.entries {
        if stopped
            || soft_cancelled.load(Ordering::Acquire)
            || hard_cancelled.load(Ordering::Acquire)
        {
            stopped = true;
            results.push(DeletionEntryResult {
                entry,
                outcome: DeletionEntryOutcome::Unattempted,
            });
            continue;
        }
        let outcome = execute_unix_entry(
            &root,
            &mut entry,
            &mut after_isolation,
            &mut after_inspection,
        );
        if matches!(&outcome, DeletionEntryOutcome::Deleted) {
            note_deleted_link(&mut entry, &planned_link_counts, &deleted_link_counts);
            let file_id = entry.snapshot.identity.file_id;
            deleted_link_counts
                .entry(file_id)
                .and_modify(|count| *count = count.saturating_add(1))
                .or_insert(1);
        }
        results.push(DeletionEntryResult { entry, outcome });
    }
    DeletionReport {
        target_node_id,
        root_relative_path,
        scan_root: scan_root.to_path_buf(),
        entries: results,
        soft_cancelled: soft_cancelled.load(Ordering::Acquire),
        precise: !hard_cancelled.load(Ordering::Acquire),
        estimated_bytes,
    }
}

#[cfg(any(target_os = "linux", target_vendor = "apple"))]
#[allow(clippy::too_many_lines)]
fn execute_unix_entry<F, G>(
    root: &File,
    entry: &mut PlannedEntry,
    after_isolation: &mut F,
    after_inspection: &mut G,
) -> DeletionEntryOutcome
where
    F: FnMut(),
    G: FnMut(&OsStr),
{
    let (parent, original_name) = match open_parent(root, &entry.relative_path) {
        Ok(value) => value,
        Err(DeletionPlanError::Io {
            kind: io::ErrorKind::NotFound,
            ..
        }) => {
            return DeletionEntryOutcome::Changed(
                "entry parent or namespace disappeared".to_string(),
            );
        }
        Err(error) => return DeletionEntryOutcome::Failed(error.to_string()),
    };
    let (detached_name, placeholder) = match create_placeholder(&parent) {
        Ok(value) => value,
        Err(error) => return DeletionEntryOutcome::Failed(error.to_string()),
    };
    if let Err(error) = exchange_names(&parent, &original_name, &detached_name) {
        let disappeared = error.kind() == io::ErrorKind::NotFound;
        let cleanup = remove_verified_placeholder(&parent, &detached_name, &placeholder);
        return if disappeared {
            cleanup.map_or_else(
                |cleanup_error| {
                    DeletionEntryOutcome::Failed(format!(
                        "target disappeared; placeholder cleanup failed: {cleanup_error}"
                    ))
                },
                |()| DeletionEntryOutcome::Missing,
            )
        } else {
            DeletionEntryOutcome::Failed(cleanup.map_or_else(
                |cleanup_error| format!("{error}; placeholder cleanup failed: {cleanup_error}"),
                |()| error.to_string(),
            ))
        };
    }
    after_isolation();

    let actual = match inspect_child(&parent, &detached_name, &entry.relative_path) {
        Ok((snapshot, handle)) => {
            drop(handle);
            entry.snapshot.identity.link_count = snapshot.identity.link_count;
            snapshot
        }
        Err(DeletionPlanError::Io {
            kind: io::ErrorKind::NotFound,
            ..
        }) => {
            return match finalize_placeholder(&parent, &original_name, &detached_name, &placeholder)
            {
                Ok(()) => DeletionEntryOutcome::Missing,
                Err(error) => DeletionEntryOutcome::Failed(format!(
                    "isolated entry disappeared; namespace cleanup failed: {error}"
                )),
            };
        }
        Err(error) => {
            let restore = restore_detached(&parent, &original_name, &detached_name, &placeholder);
            return DeletionEntryOutcome::Failed(restore.map_or_else(
                |restore_error| format!("{error}; namespace recovery failed: {restore_error}"),
                |()| error.to_string(),
            ));
        }
    };
    if !matches_for_execution(&entry.snapshot, &actual) {
        return match restore_detached(&parent, &original_name, &detached_name, &placeholder) {
            Ok(()) => DeletionEntryOutcome::Changed(
                "identity, type, size, allocation, or modification changed".to_string(),
            ),
            Err(error) => DeletionEntryOutcome::Failed(format!(
                "entry changed; namespace recovery failed: {error}"
            )),
        };
    }

    let link_hold = if matches!(entry.snapshot.kind, PlannedKind::File | PlannedKind::Link) {
        if actual.identity.link_count.is_some() {
            if let Ok(name) = create_link_hold(&parent, &detached_name) {
                Some(name)
            } else {
                entry.snapshot.identity.link_count = None;
                None
            }
        } else {
            entry.snapshot.identity.link_count = None;
            None
        }
    } else {
        None
    };
    after_inspection(&detached_name);

    let removal = match entry.snapshot.kind {
        PlannedKind::Directory => cap_fs::remove_dir(&parent, Path::new(&detached_name)),
        PlannedKind::File | PlannedKind::Link => {
            cap_fs::remove_file(&parent, Path::new(&detached_name))
        }
    };
    match removal {
        Ok(()) => {
            if let Some(link_hold) = link_hold.as_ref() {
                if !link_hold_matches_count(&parent, link_hold, actual.identity.link_count)
                    .unwrap_or(false)
                {
                    entry.snapshot.identity.link_count = None;
                }
            }
            let finalization =
                finalize_placeholder(&parent, &original_name, &detached_name, &placeholder);
            let hold_cleanup = link_hold
                .as_ref()
                .map(|name| remove_link_hold(&parent, name, &actual.identity));
            match (finalization, hold_cleanup) {
                (Ok(()), Some(Ok(())) | None) => DeletionEntryOutcome::Deleted,
                (Ok(()), Some(Err(error))) => DeletionEntryOutcome::Failed(format!(
                    "target deleted; hard-link check cleanup failed: {error}"
                )),
                (Err(error), Some(Ok(())) | None) => DeletionEntryOutcome::Failed(format!(
                    "target deleted; namespace cleanup failed: {error}"
                )),
                (Err(error), Some(Err(cleanup_error))) => DeletionEntryOutcome::Failed(format!(
                    "target deleted; namespace cleanup failed: {error}; hard-link check cleanup failed: {cleanup_error}"
                )),
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let finalization =
                finalize_placeholder(&parent, &original_name, &detached_name, &placeholder);
            let hold_cleanup = link_hold
                .as_ref()
                .map(|name| remove_link_hold(&parent, name, &actual.identity));
            match (finalization, hold_cleanup) {
                (Ok(()), Some(Ok(())) | None) => DeletionEntryOutcome::Missing,
                (Ok(()), Some(Err(cleanup_error))) => DeletionEntryOutcome::Failed(format!(
                    "target disappeared; hard-link check cleanup failed: {cleanup_error}"
                )),
                (Err(error), Some(Ok(())) | None) => DeletionEntryOutcome::Failed(format!(
                    "target disappeared; namespace cleanup failed: {error}"
                )),
                (Err(error), Some(Err(cleanup_error))) => DeletionEntryOutcome::Failed(format!(
                    "target disappeared; namespace cleanup failed: {error}; hard-link check cleanup failed: {cleanup_error}"
                )),
            }
        }
        Err(error) => {
            let restore = restore_detached(&parent, &original_name, &detached_name, &placeholder);
            let hold_cleanup = link_hold
                .as_ref()
                .map(|name| remove_link_hold(&parent, name, &actual.identity));
            let cleanup_error = hold_cleanup.and_then(Result::err);
            if error.kind() == io::ErrorKind::DirectoryNotEmpty {
                restore.map_or_else(
                    |restore_error| {
                        DeletionEntryOutcome::Failed(format!(
                            "directory changed; namespace recovery failed: {restore_error}"
                        ))
                    },
                    |()| {
                        if let Some(cleanup_error) = cleanup_error {
                            DeletionEntryOutcome::Failed(format!(
                                "directory changed; hard-link check cleanup failed: {cleanup_error}"
                            ))
                        } else {
                            DeletionEntryOutcome::Changed(
                                "directory contains a new or changed entry".to_string(),
                            )
                        }
                    },
                )
            } else {
                DeletionEntryOutcome::Failed(restore.map_or_else(
                    |restore_error| format!("{error}; namespace recovery failed: {restore_error}"),
                    |()| {
                        cleanup_error.map_or_else(
                            || error.to_string(),
                            |cleanup_error| {
                                format!("{error}; hard-link check cleanup failed: {cleanup_error}")
                            },
                        )
                    },
                ))
            }
        }
    }
}

#[cfg(windows)]
fn execute_plan_windows(
    scan_root: &Path,
    plan: DeletionPlan,
    soft_cancelled: &AtomicBool,
    hard_cancelled: &AtomicBool,
) -> DeletionReport {
    let root = match open_root(scan_root, &plan.scan_root_identity) {
        Ok(root) => root,
        Err(error) => return failed_report(scan_root, plan, &error.to_string()),
    };
    let estimated_bytes = plan.estimated_bytes;
    let target_node_id = plan.target.node_id;
    let root_relative_path = plan.root_relative_path.clone();
    let planned_link_counts = planned_link_counts(&plan.entries);
    let mut deleted_link_counts = HashMap::new();
    let mut results = Vec::with_capacity(plan.entries.len());
    let mut stopped = false;
    for mut entry in plan.entries {
        if stopped
            || soft_cancelled.load(Ordering::Acquire)
            || hard_cancelled.load(Ordering::Acquire)
        {
            stopped = true;
            results.push(DeletionEntryResult {
                entry,
                outcome: DeletionEntryOutcome::Unattempted,
            });
            continue;
        }
        let outcome = execute_windows_entry(&root, &mut entry);
        if matches!(&outcome, DeletionEntryOutcome::Deleted) {
            note_deleted_link(&mut entry, &planned_link_counts, &deleted_link_counts);
            let file_id = entry.snapshot.identity.file_id;
            deleted_link_counts
                .entry(file_id)
                .and_modify(|count| *count = count.saturating_add(1))
                .or_insert(1);
        }
        results.push(DeletionEntryResult { entry, outcome });
    }
    DeletionReport {
        target_node_id,
        root_relative_path,
        scan_root: scan_root.to_path_buf(),
        entries: results,
        soft_cancelled: soft_cancelled.load(Ordering::Acquire),
        precise: !hard_cancelled.load(Ordering::Acquire),
        estimated_bytes,
    }
}

#[cfg(windows)]
fn execute_windows_entry(root: &File, entry: &mut PlannedEntry) -> DeletionEntryOutcome {
    use cap_primitives::fs::{_WindowsByHandle as _, OpenOptionsExt as _};

    const DELETE: u32 = 0x0001_0000;
    const FILE_READ_ATTRIBUTES: u32 = 0x0000_0080;
    const SYNCHRONIZE: u32 = 0x0010_0000;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const FILE_SHARE_DELETE: u32 = 0x0000_0004;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

    let (parent, name) = match open_parent(root, &entry.relative_path) {
        Ok(value) => value,
        Err(DeletionPlanError::Io {
            kind: io::ErrorKind::NotFound,
            ..
        }) => return DeletionEntryOutcome::Missing,
        Err(error) => return DeletionEntryOutcome::Failed(error.to_string()),
    };
    let mut options = cap_fs::OpenOptions::new();
    options
        .access_mode(DELETE | FILE_READ_ATTRIBUTES | SYNCHRONIZE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        ._cap_fs_ext_follow(FollowSymlinks::No);
    let handle = match cap_fs::open(&parent, Path::new(&name), &options) {
        Ok(handle) => handle,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return DeletionEntryOutcome::Missing;
        }
        Err(error) => return DeletionEntryOutcome::Failed(error.to_string()),
    };
    let metadata = match cap_fs::Metadata::from_file(&handle) {
        Ok(metadata) => metadata,
        Err(error) => return DeletionEntryOutcome::Failed(error.to_string()),
    };
    let kind = if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        PlannedKind::Link
    } else if metadata.is_dir() {
        PlannedKind::Directory
    } else {
        PlannedKind::File
    };
    let actual = match snapshot_from_open_file(&handle, kind) {
        Ok(snapshot) => snapshot,
        Err(error) => return DeletionEntryOutcome::Failed(error.to_string()),
    };
    if kind == PlannedKind::File {
        // A pathname-independent post-open hard-link count can still change
        // before handle deletion. Report regular-file allocation conservatively.
        entry.snapshot.identity.link_count = None;
    } else {
        entry.snapshot.identity.link_count = actual.identity.link_count;
    }
    if !matches_for_execution(&entry.snapshot, &actual) {
        return DeletionEntryOutcome::Changed(
            "identity, type, size, allocation, or modification changed".to_string(),
        );
    }
    match crate::windows_delete::remove_open_handle(&handle) {
        Ok(()) => DeletionEntryOutcome::Deleted,
        Err(error) if error.raw_os_error() == Some(145) => {
            DeletionEntryOutcome::Changed("directory contains a new or changed entry".to_string())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => DeletionEntryOutcome::Missing,
        Err(error) => DeletionEntryOutcome::Failed(error.to_string()),
    }
}

fn failed_report(scan_root: &Path, plan: DeletionPlan, message: &str) -> DeletionReport {
    DeletionReport {
        target_node_id: plan.target.node_id,
        root_relative_path: plan.root_relative_path,
        scan_root: scan_root.to_path_buf(),
        entries: plan
            .entries
            .into_iter()
            .map(|entry| DeletionEntryResult {
                entry,
                outcome: DeletionEntryOutcome::Failed(message.to_string()),
            })
            .collect(),
        soft_cancelled: false,
        precise: true,
        estimated_bytes: plan.estimated_bytes,
    }
}

#[cfg(any(target_os = "linux", target_vendor = "apple"))]
fn create_placeholder(parent: &File) -> io::Result<(OsString, PlannedSnapshot)> {
    use std::sync::atomic::AtomicU64;

    static NEXT_PLACEHOLDER: AtomicU64 = AtomicU64::new(0);
    for _ in 0..128 {
        let sequence = NEXT_PLACEHOLDER.fetch_add(1, Ordering::Relaxed);
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).map_err(|error| io::Error::other(error.to_string()))?;
        let token = random
            .iter()
            .fold(String::with_capacity(32), |mut token, byte| {
                use std::fmt::Write as _;

                let _ = write!(token, "{byte:02x}");
                token
            });
        let name = OsString::from(format!(
            ".excise-delete-{token}-{:x}-{sequence:x}",
            std::process::id()
        ));
        let mut options = cap_fs::OpenOptions::new();
        options.write(true).create_new(true);
        match cap_fs::open(parent, Path::new(&name), &options) {
            Ok(file) => {
                let metadata = file.metadata()?;
                return snapshot_from_std_metadata(&metadata, PlannedKind::File)
                    .map(|snapshot| (name, snapshot));
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not reserve an isolated deletion name",
    ))
}

#[cfg(any(target_os = "linux", target_vendor = "apple"))]
fn exchange_names(parent: &File, left: &OsStr, right: &OsStr) -> io::Result<()> {
    rustix::fs::renameat_with(
        parent,
        Path::new(left),
        parent,
        Path::new(right),
        rustix::fs::RenameFlags::EXCHANGE,
    )
    .map_err(io::Error::from)
}

#[cfg(any(target_os = "linux", target_vendor = "apple"))]
fn remove_verified_placeholder(
    parent: &File,
    name: &OsStr,
    expected: &PlannedSnapshot,
) -> io::Result<()> {
    let (actual, handle) = inspect_child(parent, name, Path::new(name))
        .map_err(|error| io::Error::other(error.to_string()))?;
    drop(handle);
    if actual != *expected {
        return Err(io::Error::other(
            "isolated deletion placeholder identity changed",
        ));
    }
    cap_fs::remove_file(parent, Path::new(name))
}

#[cfg(any(target_os = "linux", target_vendor = "apple"))]
fn restore_detached(
    parent: &File,
    original: &OsStr,
    detached: &OsStr,
    placeholder: &PlannedSnapshot,
) -> io::Result<()> {
    let (actual, handle) = inspect_child(parent, original, Path::new(original))
        .map_err(|error| io::Error::other(error.to_string()))?;
    drop(handle);
    if actual != *placeholder {
        return Err(io::Error::other(
            "original name no longer contains the deletion placeholder",
        ));
    }
    exchange_names(parent, original, detached)?;
    remove_verified_placeholder(parent, detached, placeholder)
}

#[cfg(any(target_os = "linux", target_vendor = "apple"))]
fn finalize_placeholder(
    parent: &File,
    original: &OsStr,
    detached: &OsStr,
    placeholder: &PlannedSnapshot,
) -> io::Result<()> {
    let (actual, handle) = inspect_child(parent, original, Path::new(original))
        .map_err(|error| io::Error::other(error.to_string()))?;
    drop(handle);
    if actual != *placeholder {
        return Err(io::Error::other(
            "original name no longer contains the deletion placeholder",
        ));
    }
    rustix::fs::renameat_with(
        parent,
        Path::new(original),
        parent,
        Path::new(detached),
        rustix::fs::RenameFlags::NOREPLACE,
    )
    .map_err(io::Error::from)?;
    remove_verified_placeholder(parent, detached, placeholder)
}

#[cfg(any(target_os = "linux", target_vendor = "apple"))]
fn create_link_hold(parent: &File, source: &OsStr) -> io::Result<OsString> {
    use std::sync::atomic::AtomicU64;

    static NEXT_LINK_HOLD: AtomicU64 = AtomicU64::new(0);
    for _ in 0..128 {
        let sequence = NEXT_LINK_HOLD.fetch_add(1, Ordering::Relaxed);
        let name = OsString::from(format!(
            ".excise-link-check-{:x}-{sequence:x}",
            std::process::id()
        ));
        match cap_fs::hard_link(parent, Path::new(source), parent, Path::new(&name)) {
            Ok(()) => return Ok(name),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not reserve a hard-link verification name",
    ))
}

#[cfg(any(target_os = "linux", target_vendor = "apple"))]
fn link_hold_matches_count(parent: &File, hold: &OsStr, expected: Option<u64>) -> io::Result<bool> {
    let metadata = cap_fs::stat(parent, Path::new(hold), FollowSymlinks::No)?;
    let snapshot = snapshot_from_cap_metadata(parent, hold, &metadata, PlannedKind::File)?;
    Ok(snapshot.identity.link_count == expected)
}

#[cfg(any(target_os = "linux", target_vendor = "apple"))]
fn remove_link_hold(parent: &File, hold: &OsStr, expected: &NativeIdentity) -> io::Result<()> {
    let metadata = match cap_fs::stat(parent, Path::new(hold), FollowSymlinks::No) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let actual = snapshot_from_cap_metadata(parent, hold, &metadata, PlannedKind::File)?;
    if !same_object(expected, &actual.identity) {
        return Err(io::Error::other(
            "hard-link verification name no longer contains the target",
        ));
    }
    match cap_fs::remove_file(parent, Path::new(hold)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn inspect_relative(
    root: &File,
    relative: &Path,
) -> Result<(PlannedSnapshot, Option<File>), DeletionPlanError> {
    let (parent, name) = open_parent(root, relative)?;
    match inspect_child(&parent, &name, relative) {
        Err(DeletionPlanError::Io {
            kind: io::ErrorKind::NotFound,
            ..
        }) => Err(DeletionPlanError::Missing(relative.to_path_buf())),
        result => result,
    }
}

fn inspect_child(
    parent: &File,
    name: &OsStr,
    display_path: &Path,
) -> Result<(PlannedSnapshot, Option<File>), DeletionPlanError> {
    let metadata = cap_fs::stat(parent, Path::new(name), FollowSymlinks::No)
        .map_err(|error| plan_io(display_path, error))?;
    if metadata.is_dir() && !metadata.is_symlink() {
        let handle = cap_fs::open_dir_nofollow(parent, Path::new(name))
            .map_err(|error| plan_io(display_path, error))?;
        let snapshot = snapshot_from_open_file(&handle, PlannedKind::Directory)
            .map_err(|error| plan_io(display_path, error))?;
        Ok((snapshot, Some(handle)))
    } else {
        let kind = if metadata.is_symlink() {
            PlannedKind::Link
        } else {
            PlannedKind::File
        };
        let snapshot = snapshot_from_cap_metadata(parent, name, &metadata, kind)
            .map_err(|error| plan_io(display_path, error))?;
        Ok((snapshot, None))
    }
}

pub(crate) fn current_scan_root_identity(path: &Path) -> Result<NativeIdentity, DeletionPlanError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| plan_io(path, error))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(DeletionPlanError::Changed);
    }
    let identity = identity_for(path, &metadata)
        .map_err(|error| plan_io(path, error))?
        .ok_or(DeletionPlanError::Changed)?;
    if identity.reparse_point {
        return Err(DeletionPlanError::Changed);
    }
    Ok(identity)
}

pub(crate) fn validate_scan_root_identity(
    path: &Path,
    expected: &NativeIdentity,
) -> Result<(), DeletionPlanError> {
    if !same_object(expected, &current_scan_root_identity(path)?) {
        return Err(DeletionPlanError::Changed);
    }
    Ok(())
}

fn open_root(path: &Path, expected: &NativeIdentity) -> Result<File, DeletionPlanError> {
    validate_scan_root_identity(path, expected)?;
    let root = cap_fs::open_ambient_dir(path, ambient_authority())
        .map_err(|error| plan_io(path, error))?;
    let handle_snapshot = snapshot_from_open_file(&root, PlannedKind::Directory)
        .map_err(|error| plan_io(path, error))?;
    if !same_object(expected, &handle_snapshot.identity) {
        return Err(DeletionPlanError::Changed);
    }
    validate_scan_root_identity(path, expected)?;
    Ok(root)
}

fn validate_model_snapshot(
    target: &FileToDelete,
    actual: &PlannedSnapshot,
) -> Result<(), DeletionPlanError> {
    let expected_kind = match target.expected_snapshot.kind {
        NodeKind::Directory => PlannedKind::Directory,
        NodeKind::File => PlannedKind::File,
        NodeKind::Link => PlannedKind::Link,
        NodeKind::Root | NodeKind::Synthetic(_) => return Err(DeletionPlanError::Synthetic),
    };
    if expected_kind != actual.kind
        || target.expected_snapshot.apparent_bytes != actual.apparent_bytes
        || target.expected_snapshot.modified_nanos != actual.modified_nanos
        || target
            .expected_snapshot
            .identity
            .as_ref()
            .is_some_and(|identity| identity != &actual.identity)
    {
        return Err(DeletionPlanError::Changed);
    }
    Ok(())
}

fn open_parent(root: &File, relative: &Path) -> Result<(File, OsString), DeletionPlanError> {
    let components = validated_components(relative)?;
    let Some((name, parents)) = components.split_last() else {
        return Err(DeletionPlanError::Root);
    };
    let mut parent = root.try_clone().map_err(|error| plan_io(relative, error))?;
    for component in parents {
        parent = cap_fs::open_dir_nofollow(&parent, Path::new(component))
            .map_err(|error| plan_io(relative, error))?;
    }
    Ok((parent, name.clone()))
}

fn is_mount_root(path: &Path) -> io::Result<bool> {
    let canonical = std::fs::canonicalize(path)?;
    if canonical.parent().is_none_or(|parent| parent == canonical) {
        return Ok(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let path_dev = std::fs::symlink_metadata(&canonical)?.dev();
        let parent = canonical.parent().expect("non-root path has a parent");
        let parent_dev = std::fs::symlink_metadata(parent)?.dev();
        Ok(path_dev != parent_dev)
    }
    #[cfg(not(unix))]
    {
        let disks = Disks::new_with_refreshed_list_specifics(DiskRefreshKind::nothing());
        Ok(disks.list().iter().any(|disk| {
            std::fs::canonicalize(disk.mount_point()).is_ok_and(|mount| mount == canonical)
        }))
    }
}

fn relative_target(target: &FileToDelete) -> Result<PathBuf, DeletionPlanError> {
    let relative = target.path_to_file.iter().collect::<PathBuf>();
    validated_components(&relative)?;
    if relative.as_os_str().is_empty() {
        Err(DeletionPlanError::Root)
    } else {
        Ok(relative)
    }
}

fn validated_components(path: &Path) -> Result<Vec<OsString>, DeletionPlanError> {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(component) => components.push(component.to_os_string()),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(DeletionPlanError::InvalidRelativePath);
            }
        }
    }
    if components.is_empty() {
        return Err(DeletionPlanError::Root);
    }
    Ok(components)
}

fn validate_component(name: &OsStr) -> Result<(), DeletionPlanError> {
    if name.is_empty() || name == OsStr::new(".") || name == OsStr::new("..") {
        Err(DeletionPlanError::InvalidRelativePath)
    } else {
        Ok(())
    }
}

fn challenge_for(
    target: &FileToDelete,
    snapshot: &PlannedSnapshot,
    reduced_guardrails: bool,
) -> ConfirmationChallenge {
    let name = target.path_to_file.last().map_or_else(
        || safe_display_os_str(OsStr::new("")),
        |name| safe_display_os_str(name),
    );
    if name.deceptive {
        return ConfirmationChallenge::TypePhrase(format!(
            "DELETE {}",
            challenge_code(&snapshot.identity.file_id)
        ));
    }
    if reduced_guardrails {
        return ConfirmationChallenge::ReducedGuard;
    }
    if snapshot.kind == PlannedKind::Directory {
        ConfirmationChallenge::TypeName(name.text)
    } else {
        ConfirmationChallenge::ConfirmFile
    }
}

fn challenge_code(file_id: &FileId) -> String {
    const ALPHABET: &[u8; 32] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let bytes = serde_json::to_vec(file_id).unwrap_or_default();
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    (0..4)
        .map(|shift| ALPHABET[((hash >> (shift * 5)) & 31) as usize] as char)
        .collect()
}

fn matches_for_execution(expected: &PlannedSnapshot, actual: &PlannedSnapshot) -> bool {
    same_object(&expected.identity, &actual.identity)
        && expected.kind == actual.kind
        && (expected.kind == PlannedKind::Directory
            || (expected.apparent_bytes == actual.apparent_bytes
                && expected.allocated_bytes == actual.allocated_bytes
                && expected.modified_nanos == actual.modified_nanos))
}

fn same_object(expected: &NativeIdentity, actual: &NativeIdentity) -> bool {
    expected.file_id == actual.file_id && expected.reparse_point == actual.reparse_point
}

fn planned_link_counts(entries: &[PlannedEntry]) -> HashMap<FileId, u64> {
    let mut counts: HashMap<FileId, u64> = HashMap::new();
    for entry in entries {
        if matches!(entry.snapshot.kind, PlannedKind::File | PlannedKind::Link)
            && entry.snapshot.identity.link_count.is_some()
        {
            counts
                .entry(entry.snapshot.identity.file_id)
                .and_modify(|count| *count = count.saturating_add(1))
                .or_insert(1);
        }
    }
    counts
}

fn note_deleted_link(
    entry: &mut PlannedEntry,
    planned: &HashMap<FileId, u64>,
    deleted: &HashMap<FileId, u64>,
) {
    if !matches!(entry.snapshot.kind, PlannedKind::File | PlannedKind::Link) {
        return;
    }
    let Some(actual) = entry.snapshot.identity.link_count else {
        return;
    };
    let Some(planned) = planned.get(&entry.snapshot.identity.file_id).copied() else {
        entry.snapshot.identity.link_count = None;
        return;
    };
    let already_deleted = deleted
        .get(&entry.snapshot.identity.file_id)
        .copied()
        .unwrap_or(0);
    if actual > planned.saturating_sub(already_deleted) {
        entry.snapshot.identity.link_count = None;
    }
}

#[cfg(unix)]
fn modified_nanos(metadata: &std::fs::Metadata) -> Option<u128> {
    metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
}

#[cfg(unix)]
#[allow(clippy::unnecessary_wraps)]
fn snapshot_from_std_metadata(
    metadata: &std::fs::Metadata,
    kind: PlannedKind,
) -> io::Result<PlannedSnapshot> {
    use std::os::unix::fs::MetadataExt as _;

    Ok(PlannedSnapshot {
        identity: NativeIdentity {
            file_id: FileId::new_inode(metadata.dev(), metadata.ino()),
            link_count: Some(metadata.nlink()),
            reparse_point: metadata.file_type().is_symlink(),
        },
        kind,
        apparent_bytes: if kind == PlannedKind::Directory {
            0
        } else {
            u128::from(metadata.len())
        },
        allocated_bytes: matches!(kind, PlannedKind::File | PlannedKind::Link)
            .then(|| u128::from(metadata.blocks()).saturating_mul(512)),
        modified_nanos: modified_nanos(metadata),
    })
}

#[cfg(windows)]
fn snapshot_from_open_file(handle: &File, kind: PlannedKind) -> io::Result<PlannedSnapshot> {
    use cap_primitives::fs::_WindowsByHandle as _;

    let metadata = cap_fs::Metadata::from_file(handle)?;
    let volume = metadata
        .volume_serial_number()
        .ok_or_else(|| io::Error::other("file handle did not expose a volume serial number"))?;
    let index = metadata
        .file_index()
        .ok_or_else(|| io::Error::other("file handle did not expose a file index"))?;
    let modified_nanos = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.into_std().duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos());
    Ok(PlannedSnapshot {
        identity: NativeIdentity {
            file_id: FileId::new_low_res(volume, index),
            link_count: metadata.number_of_links().map(u64::from),
            reparse_point: metadata.file_attributes() & 0x0000_0400 != 0,
        },
        kind,
        apparent_bytes: if kind == PlannedKind::Directory {
            0
        } else {
            u128::from(metadata.len())
        },
        allocated_bytes: (kind != PlannedKind::Directory)
            .then(|| crate::os::physical_size_from_handle(handle))
            .transpose()?
            .map(u128::from),
        modified_nanos,
    })
}

#[cfg(not(windows))]
fn snapshot_from_open_file(handle: &File, kind: PlannedKind) -> io::Result<PlannedSnapshot> {
    snapshot_from_std_metadata(&handle.metadata()?, kind)
}

#[cfg(not(any(unix, windows)))]
fn snapshot_from_std_metadata(
    _metadata: &std::fs::Metadata,
    _kind: PlannedKind,
) -> io::Result<PlannedSnapshot> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "permanent deletion is unavailable on this target",
    ))
}

#[cfg(unix)]
#[allow(clippy::unnecessary_wraps)]
fn snapshot_from_cap_metadata(
    _parent: &File,
    _name: &OsStr,
    metadata: &cap_fs::Metadata,
    kind: PlannedKind,
) -> io::Result<PlannedSnapshot> {
    use cap_primitives::fs::MetadataExt as _;

    let modified_nanos = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.into_std().duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos());
    Ok(PlannedSnapshot {
        identity: NativeIdentity {
            file_id: FileId::new_inode(metadata.dev(), metadata.ino()),
            link_count: Some(metadata.nlink()),
            reparse_point: metadata.is_symlink(),
        },
        kind,
        apparent_bytes: u128::from(metadata.len()),

        allocated_bytes: matches!(kind, PlannedKind::File | PlannedKind::Link)
            .then(|| u128::from(metadata.blocks()).saturating_mul(512)),
        modified_nanos,
    })
}

#[cfg(windows)]
fn snapshot_from_cap_metadata(
    parent: &File,
    name: &OsStr,
    _metadata: &cap_fs::Metadata,
    kind: PlannedKind,
) -> io::Result<PlannedSnapshot> {
    use cap_primitives::fs::OpenOptionsExt as _;

    const FILE_READ_ATTRIBUTES: u32 = 0x0000_0080;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const FILE_SHARE_DELETE: u32 = 0x0000_0004;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    let mut options = cap_fs::OpenOptions::new();
    options
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        ._cap_fs_ext_follow(FollowSymlinks::No);
    let handle = cap_fs::open(parent, Path::new(name), &options)?;
    snapshot_from_open_file(&handle, kind)
}

#[cfg(not(any(unix, windows)))]
fn snapshot_from_cap_metadata(
    _parent: &File,
    _name: &OsStr,
    _metadata: &cap_fs::Metadata,
    _kind: PlannedKind,
) -> io::Result<PlannedSnapshot> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "permanent deletion is unavailable on this target",
    ))
}

#[allow(clippy::needless_pass_by_value)]
fn plan_io(path: &Path, error: io::Error) -> DeletionPlanError {
    DeletionPlanError::Io {
        path: safe_display_path_text(path),
        message: safe_display_text(&error.to_string()),
        kind: error.kind(),
    }
}
fn push_planned_entry(
    entries: &mut Vec<PlannedEntry>,
    entry: PlannedEntry,
    estimated_bytes: &mut usize,
    maximum_bytes: usize,
) -> Result<(), DeletionPlanError> {
    let required = size_of::<PlannedEntry>()
        .saturating_add(
            entry
                .relative_path
                .as_os_str()
                .as_encoded_bytes()
                .len()
                .saturating_mul(2),
        )
        .saturating_add(128);
    let next = estimated_bytes.saturating_add(required);
    if next > maximum_bytes {
        return Err(DeletionPlanError::MemoryLimit {
            limit: maximum_bytes,
        });
    }
    *estimated_bytes = next;
    entries.push(entry);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::sync::atomic::AtomicBool;

    use super::*;
    use crate::state::tiles::FileType;

    fn reviewed_snapshot(path: &Path, metadata: &std::fs::Metadata) -> PlannedSnapshot {
        let identity = crate::native_path::identity_for(path, metadata)
            .expect("fixture identity lookup should succeed")
            .expect("fixture identity should be readable");
        let kind = if metadata.file_type().is_symlink() || identity.reparse_point {
            PlannedKind::Link
        } else if metadata.is_dir() {
            PlannedKind::Directory
        } else {
            PlannedKind::File
        };
        PlannedSnapshot {
            identity,
            kind,
            apparent_bytes: if kind == PlannedKind::Directory {
                0
            } else {
                u128::from(metadata.len())
            },
            allocated_bytes: matches!(kind, PlannedKind::File | PlannedKind::Link)
                .then(|| {
                    crate::os::physical_size(path, metadata)
                        .ok()
                        .map(u128::from)
                })
                .flatten(),
            modified_nanos: metadata
                .modified()
                .ok()
                .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_nanos()),
        }
    }

    fn reviewed_entries(root: &Path, target: &Path) -> Vec<ReviewedEntry> {
        let mut entries = Vec::new();
        let mut stack = vec![target.to_path_buf()];
        while let Some(path) = stack.pop() {
            let metadata =
                std::fs::symlink_metadata(&path).expect("fixture metadata should be readable");
            let snapshot = reviewed_snapshot(&path, &metadata);
            if snapshot.kind == PlannedKind::Directory {
                for child in std::fs::read_dir(&path).expect("fixture directory should be readable")
                {
                    stack.push(child.expect("fixture entry should be readable").path());
                }
            }
            entries.push(ReviewedEntry {
                relative_path: path
                    .strip_prefix(root)
                    .expect("fixture should be below root")
                    .to_path_buf(),
                snapshot,
            });
        }
        entries.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        entries
    }

    fn target(root: &Path, name: OsString, file_type: FileType) -> FileToDelete {
        let path = root.join(&name);
        let reviewed_entries = reviewed_entries(root, &path);
        let snapshot = reviewed_entries
            .iter()
            .find(|entry| entry.relative_path == Path::new(&name))
            .expect("target should be reviewed")
            .snapshot
            .clone();
        let kind = match snapshot.kind {
            PlannedKind::Directory => NodeKind::Directory,
            PlannedKind::File => NodeKind::File,
            PlannedKind::Link => NodeKind::Link,
        };
        FileToDelete {
            node_id: NodeId(1),
            synthetic: false,
            path_in_filesystem: root.to_path_buf(),
            path_to_file: vec![name],
            file_type,
            num_descendants: (kind == NodeKind::Directory).then(|| {
                u64::try_from(reviewed_entries.len().saturating_sub(1)).unwrap_or(u64::MAX)
            }),
            size: 0,
            expected_snapshot: crate::model::EntrySnapshot {
                identity: Some(snapshot.identity.clone()),
                kind,
                apparent_bytes: snapshot.apparent_bytes,
                allocated_bytes: snapshot.allocated_bytes,
                modified_nanos: snapshot.modified_nanos,
            },
            reviewed_entries,
        }
    }

    #[test]
    fn plan_io_display_escapes_hostile_path_and_message() {
        let path = Path::new("plan-\u{1b}[31m-\u{202e}name");
        let error = plan_io(path, io::Error::other("metadata failed\t\u{202e}"));
        let rendered = error.to_string();

        assert!(rendered.contains("[deceptive]"));
        assert!(rendered.contains("\\x1b"));
        assert!(rendered.contains("\\u{202e}"));
        assert!(rendered.contains("\\t"));
        assert!(!rendered.chars().any(char::is_control));
        assert!(!rendered.contains('\u{202e}'));
    }

    #[test]
    fn missing_parent_is_stale_but_missing_target_is_explicit() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let parent = root.path().join("parent");
        std::fs::create_dir(&parent).expect("parent should be created");
        let nested = parent.join("target");
        std::fs::write(&nested, b"payload").expect("nested target should be written");

        let reviewed = target(root.path(), OsString::from("parent/target"), FileType::File);
        std::fs::remove_dir_all(&parent).expect("parent should be removed");
        let parent_error = build_plan(root.path(), reviewed, false)
            .expect_err("missing parent should reject the stale namespace");
        assert!(parent_error.is_stale());
        assert!(!parent_error.is_missing());
        assert!(matches!(
            parent_error,
            DeletionPlanError::Io {
                kind: io::ErrorKind::NotFound,
                ..
            }
        ));

        let path = root.path().join("target");
        std::fs::write(&path, b"payload").expect("target should be written");
        let reviewed = target(root.path(), OsString::from("target"), FileType::File);
        std::fs::remove_file(&path).expect("target should be removed");
        let target_error = build_plan(root.path(), reviewed, false)
            .expect_err("missing target should be reported explicitly");
        assert!(target_error.is_missing());
        assert!(!target_error.is_stale());
        assert!(matches!(
            target_error,
            DeletionPlanError::Missing(path) if path == Path::new("target")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn hard_link_created_after_identity_inspection_is_not_reclaimable() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let path = root.path().join("target");
        let late_link = root.path().join("late-link");
        std::fs::write(&path, b"payload").expect("target should be written");
        let plan = build_plan(
            root.path(),
            target(root.path(), OsString::from("target"), FileType::File),
            false,
        )
        .expect("file plan should build");

        let report = execute_plan_unix_with_hooks(
            root.path(),
            plan,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
            || {},
            |detached| {
                std::fs::hard_link(root.path().join(detached), &late_link)
                    .expect("late hard link should be created");
            },
        );

        assert_eq!(report.deleted_entries(), 1);
        assert_eq!(report.deleted_allocated_bytes(), 0);
        assert!(!path.exists());
        assert!(late_link.exists());
    }

    #[test]
    fn unchanged_file_plan_deletes_exact_identity() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let path = root.path().join("target");
        std::fs::write(&path, b"payload").expect("target should be written");
        let plan = build_plan(
            root.path(),
            target(root.path(), OsString::from("target"), FileType::File),
            false,
        )
        .expect("file plan should build");
        assert_eq!(plan.entries.len(), 1);
        assert_eq!(plan.challenge, ConfirmationChallenge::ConfirmFile);

        let report = execute_plan(
            root.path(),
            plan,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
        );

        assert_eq!(report.deleted_entries(), 1);
        assert!(report.precise);
        assert!(!path.exists());
    }
    #[test]
    fn synthetic_and_scan_root_targets_are_rejected() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let path = root.path().join("target");
        std::fs::write(&path, b"payload").expect("target should be written");

        let mut synthetic = target(root.path(), OsString::from("target"), FileType::File);
        synthetic.synthetic = true;
        assert!(matches!(
            build_plan(root.path(), synthetic, false),
            Err(DeletionPlanError::Synthetic)
        ));

        let mut scan_root = target(root.path(), OsString::from("target"), FileType::File);
        scan_root.path_to_file.clear();
        assert!(matches!(
            build_plan(root.path(), scan_root, false),
            Err(DeletionPlanError::Root)
        ));
        assert!(path.exists());
    }

    #[test]
    fn replaced_file_is_skipped() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let path = root.path().join("target");
        std::fs::write(&path, b"original").expect("target should be written");
        let plan = build_plan(
            root.path(),
            target(root.path(), OsString::from("target"), FileType::File),
            false,
        )
        .expect("file plan should build");
        std::fs::rename(&path, root.path().join("original"))
            .expect("original identity should remain allocated");
        std::fs::write(&path, b"replacement").expect("replacement should be written");

        let report = execute_plan(
            root.path(),
            plan,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
        );

        assert_eq!(report.changed_entries(), 1);
        assert!(path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn replacement_after_isolation_is_never_deleted() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let path = root.path().join("target");
        std::fs::write(&path, b"reviewed").expect("target should be written");
        let plan = build_plan(
            root.path(),
            target(root.path(), OsString::from("target"), FileType::File),
            false,
        )
        .expect("file plan should build");
        let displaced_placeholder = root.path().join("displaced-placeholder");

        let report = execute_plan_unix_with_hook(
            root.path(),
            plan,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
            || {
                std::fs::rename(&path, &displaced_placeholder)
                    .expect("placeholder should be displaced");
                std::fs::write(&path, b"replacement")
                    .expect("replacement should occupy the original name");
            },
        );

        assert_eq!(
            std::fs::read(&path).expect("replacement should survive"),
            b"replacement"
        );
        assert_eq!(report.failed_entries(), 1);
        assert!(report.precise);
    }

    #[test]
    fn new_directory_child_is_never_swept() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let directory = root.path().join("target");
        std::fs::create_dir(&directory).expect("target directory should be created");
        let planned_child = directory.join("planned");
        std::fs::write(&planned_child, b"planned").expect("planned child should be written");
        let plan = build_plan(
            root.path(),
            target(root.path(), OsString::from("target"), FileType::Folder),
            false,
        )
        .expect("directory plan should build");
        let new_child = directory.join("new");
        std::fs::write(&new_child, b"new").expect("new child should be written");

        let report = execute_plan(
            root.path(),
            plan,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
        );

        assert_eq!(report.deleted_entries(), 1);
        assert_eq!(report.changed_entries(), 1);
        assert!(!planned_child.exists());
        assert!(new_child.exists());
        assert!(directory.exists());
    }

    #[test]
    fn soft_cancel_leaves_every_entry_unattempted() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let path = root.path().join("target");
        std::fs::write(&path, b"payload").expect("target should be written");
        let plan = build_plan(
            root.path(),
            target(root.path(), OsString::from("target"), FileType::File),
            false,
        )
        .expect("file plan should build");

        let report = execute_plan(
            root.path(),
            plan,
            &AtomicBool::new(true),
            &AtomicBool::new(false),
        );

        assert!(report.soft_cancelled);
        assert_eq!(report.unattempted_entries(), 1);
        assert!(path.exists());
    }

    #[test]
    fn changed_scan_snapshot_is_rejected_before_consent() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let path = root.path().join("target");
        std::fs::write(&path, b"original").expect("target should be written");
        let reviewed = target(root.path(), OsString::from("target"), FileType::File);
        std::fs::write(&path, b"changed content").expect("target should change");

        assert!(matches!(
            build_plan(root.path(), reviewed, false),
            Err(DeletionPlanError::Changed)
        ));
        assert!(path.exists());
    }

    #[test]
    fn stale_reviewed_subtree_is_rejected_before_consent() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let directory = root.path().join("target");
        std::fs::create_dir(&directory).expect("target directory should be created");
        let reviewed_child = directory.join("reviewed");
        std::fs::write(&reviewed_child, b"reviewed").expect("reviewed child should be written");
        let reviewed = target(root.path(), OsString::from("target"), FileType::Folder);
        let stale_child = directory.join("not-reviewed");
        std::fs::write(&stale_child, b"late").expect("late child should be written");

        assert!(matches!(
            build_plan(root.path(), reviewed, false),
            Err(DeletionPlanError::Changed)
        ));
        assert!(reviewed_child.exists());
        assert!(stale_child.exists());
    }

    #[test]
    fn file_to_directory_replacement_is_skipped() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let path = root.path().join("target");
        std::fs::write(&path, b"original").expect("target should be written");
        let plan = build_plan(
            root.path(),
            target(root.path(), OsString::from("target"), FileType::File),
            false,
        )
        .expect("file plan should build");
        std::fs::rename(&path, root.path().join("original"))
            .expect("original identity should remain allocated");
        std::fs::create_dir(&path).expect("replacement directory should be created");

        let report = execute_plan(
            root.path(),
            plan,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
        );
        assert_eq!(report.changed_entries(), 1);
        assert!(path.is_dir());
    }
    #[test]
    fn directory_to_file_replacement_is_skipped() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let path = root.path().join("target");
        std::fs::create_dir(&path).expect("target directory should be created");
        let plan = build_plan(
            root.path(),
            target(root.path(), OsString::from("target"), FileType::Folder),
            false,
        )
        .expect("directory plan should build");
        std::fs::rename(&path, root.path().join("original"))
            .expect("original directory identity should remain allocated");
        std::fs::write(&path, b"replacement").expect("replacement file should be written");

        let report = execute_plan(
            root.path(),
            plan,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
        );
        assert_eq!(report.changed_entries(), 1);
        assert!(path.is_file());
    }

    #[test]
    fn changed_entry_does_not_block_safe_sibling() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let directory = root.path().join("target");
        std::fs::create_dir(&directory).expect("target directory should be created");
        let changed = directory.join("changed");
        let safe = directory.join("safe");
        std::fs::write(&changed, b"original").expect("changed fixture should be written");
        std::fs::write(&safe, b"safe").expect("safe fixture should be written");
        let plan = build_plan(
            root.path(),
            target(root.path(), OsString::from("target"), FileType::Folder),
            false,
        )
        .expect("directory plan should build");
        std::fs::rename(&changed, directory.join("original"))
            .expect("original identity should remain allocated");
        std::fs::write(&changed, b"replacement").expect("replacement should be written");

        let report = execute_plan(
            root.path(),
            plan,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
        );
        assert!(report.deleted_entries() >= 1);
        assert!(report.changed_entries() >= 1);
        assert!(!safe.exists());
        assert!(changed.exists());
    }

    #[test]
    fn hard_cancel_marks_report_imprecise() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let path = root.path().join("target");
        std::fs::write(&path, b"payload").expect("target should be written");
        let plan = build_plan(
            root.path(),
            target(root.path(), OsString::from("target"), FileType::File),
            false,
        )
        .expect("file plan should build");

        let report = execute_plan(
            root.path(),
            plan,
            &AtomicBool::new(false),
            &AtomicBool::new(true),
        );
        assert!(!report.precise);
        assert_eq!(report.unattempted_entries(), 1);
        assert!(path.exists());
    }

    #[test]
    fn deletion_plan_respects_memory_limit() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let path = root.path().join("target");
        std::fs::write(&path, b"payload").expect("target should be written");
        let target = target(root.path(), OsString::from("target"), FileType::File);

        assert!(matches!(
            build_plan_cancellable(root.path(), target, false, &AtomicBool::new(false), 1,),
            Err(DeletionPlanError::MemoryLimit { limit: 1 })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn deep_plan_does_not_retain_one_handle_per_level() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let target_path = root.path().join("target");
        std::fs::create_dir(&target_path).expect("target directory should be created");
        let mut deepest = target_path;
        for _ in 0..300 {
            deepest.push("d");
            std::fs::create_dir(&deepest).expect("deep directory should be created");
        }
        let plan = build_plan(
            root.path(),
            target(root.path(), OsString::from("target"), FileType::Folder),
            false,
        )
        .expect("deep plan should stay within the process file descriptor limit");

        assert_eq!(plan.planned_entries(), 301);
    }

    #[test]
    fn safe_directory_and_reduced_guardrails_use_distinct_challenges() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let path = root.path().join("target");
        std::fs::create_dir(&path).expect("target directory should be created");
        let reviewed = target(root.path(), OsString::from("target"), FileType::Folder);
        let guarded =
            build_plan(root.path(), reviewed.clone(), false).expect("guarded plan should build");
        let reduced = build_plan(root.path(), reviewed, true).expect("reduced plan should build");

        assert_eq!(
            guarded.challenge,
            ConfirmationChallenge::TypeName("target".to_string())
        );
        assert_eq!(reduced.challenge, ConfirmationChallenge::ReducedGuard);
    }

    #[cfg(unix)]
    #[test]
    fn deleting_a_link_never_deletes_its_target() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("deletion root should exist");
        let outside = tempfile::tempdir().expect("outside root should exist");
        let outside_file = outside.path().join("outside");
        std::fs::write(&outside_file, b"outside").expect("outside target should be written");
        let link = root.path().join("link");
        symlink(&outside_file, &link).expect("link should be created");
        let plan = build_plan(
            root.path(),
            target(root.path(), OsString::from("link"), FileType::File),
            false,
        )
        .expect("link plan should build without following it");
        assert_eq!(
            plan.root_snapshot().map(|snapshot| snapshot.kind),
            Some(PlannedKind::Link)
        );
        let link_allocation = plan
            .root_snapshot()
            .and_then(|snapshot| snapshot.allocated_bytes)
            .expect("link-object allocation should be known");

        let report = execute_plan(
            root.path(),
            plan,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
        );

        assert_eq!(report.deleted_entries(), 1);
        assert_eq!(report.deleted_allocated_bytes(), link_allocation);
        assert!(!link.exists());
        assert!(outside_file.exists());
    }

    #[cfg(unix)]
    #[test]
    fn mount_root_is_rejected_but_a_link_to_it_is_plannable() {
        use std::os::unix::fs::symlink;

        assert!(is_mount_root(Path::new("/")).expect("root mount should be inspectable"));
        let root = tempfile::tempdir().expect("deletion root should exist");
        let link = root.path().join("root-link");
        symlink("/", &link).expect("root link should be created");
        let plan = build_plan(
            root.path(),
            target(root.path(), OsString::from("root-link"), FileType::File),
            false,
        )
        .expect("a link to a mount root should not follow its target");
        assert_eq!(
            plan.root_snapshot().map(|snapshot| snapshot.kind),
            Some(PlannedKind::Link)
        );
    }

    #[cfg(windows)]
    #[test]
    fn deleting_a_junction_never_deletes_its_target() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let outside = tempfile::tempdir().expect("outside root should exist");
        let outside_file = outside.path().join("outside");
        std::fs::write(&outside_file, b"outside").expect("outside target should be written");
        let junction = root.path().join("junction");
        let quote = |path: &Path| format!("'{}'", path.display().to_string().replace('\'', "''"));
        let command = format!(
            "$ErrorActionPreference='Stop'; New-Item -ItemType Junction -Path {} -Target {} | Out-Null",
            quote(&junction),
            quote(outside.path())
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

        let plan = build_plan(
            root.path(),
            target(root.path(), OsString::from("junction"), FileType::Folder),
            false,
        )
        .expect("junction plan should build without following its target");
        assert_eq!(
            plan.root_snapshot().map(|snapshot| snapshot.kind),
            Some(PlannedKind::Link)
        );
        let junction_allocation = plan
            .root_snapshot()
            .and_then(|snapshot| snapshot.allocated_bytes)
            .expect("reparse-object allocation should be known");

        let report = execute_plan(
            root.path(),
            plan,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
        );

        assert_eq!(report.deleted_entries(), 1);
        assert_eq!(report.deleted_allocated_bytes(), junction_allocation);
        assert!(!junction.exists());
        assert!(outside_file.exists());
    }

    #[cfg(windows)]
    #[test]
    fn sharing_violation_is_reported_without_deleting_target() {
        use std::os::windows::fs::OpenOptionsExt as _;

        use windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION;

        const DELETE: u32 = 0x0001_0000;
        const FILE_SHARE_READ: u32 = 0x0000_0001;
        const FILE_SHARE_WRITE: u32 = 0x0000_0002;

        let root = tempfile::tempdir().expect("deletion root should exist");
        let path = root.path().join("target");
        std::fs::write(&path, b"payload").expect("target should be written");
        let plan = build_plan(
            root.path(),
            target(root.path(), OsString::from("target"), FileType::File),
            false,
        )
        .expect("file plan should build");
        let blocker = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .open(&path)
            .expect("sharing blocker should open");
        let denied_delete = std::fs::OpenOptions::new()
            .access_mode(DELETE)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .open(&path)
            .expect_err("delete access should be denied by the sharing blocker");
        assert_eq!(
            denied_delete.raw_os_error(),
            i32::try_from(ERROR_SHARING_VIOLATION).ok()
        );

        let report = execute_plan(
            root.path(),
            plan,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
        );

        assert_eq!(report.failed_entries(), 1);
        assert!(path.exists());
        drop(blocker);
    }

    #[cfg(windows)]
    #[test]
    fn regular_file_deletion_reports_allocation_as_unknown() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let path = root.path().join("target");
        std::fs::write(&path, b"payload").expect("target should be written");
        let plan = build_plan(
            root.path(),
            target(root.path(), OsString::from("target"), FileType::File),
            false,
        )
        .expect("file plan should build");

        let report = execute_plan(
            root.path(),
            plan,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
        );

        assert_eq!(report.deleted_entries(), 1);
        assert_eq!(report.deleted_allocated_bytes(), 0);
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn hostile_directory_name_uses_generated_challenge() {
        use std::os::unix::ffi::OsStringExt as _;

        let root = tempfile::tempdir().expect("deletion root should exist");
        let name = OsString::from_vec(b"bad\x1bname".to_vec());
        std::fs::create_dir(root.path().join(&name)).expect("hostile directory should be created");
        let plan = build_plan(
            root.path(),
            target(root.path(), name, FileType::Folder),
            false,
        )
        .expect("hostile directory plan should build");

        assert!(matches!(
            plan.challenge,
            ConfirmationChallenge::TypePhrase(_)
        ));
    }
    #[cfg(unix)]
    #[test]
    fn hostile_file_name_uses_generated_challenge() {
        use std::os::unix::ffi::OsStringExt as _;

        let root = tempfile::tempdir().expect("deletion root should exist");
        let name = OsString::from_vec(b"bad\x1bfile".to_vec());
        std::fs::write(root.path().join(&name), b"payload")
            .expect("hostile file should be written");
        let plan = build_plan(
            root.path(),
            target(root.path(), name, FileType::File),
            false,
        )
        .expect("hostile file plan should build");

        assert!(matches!(
            plan.challenge,
            ConfirmationChallenge::TypePhrase(ref phrase) if phrase.starts_with("DELETE ")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn replaced_scan_root_is_rejected_before_execution() {
        let parent = tempfile::tempdir().expect("deletion parent should exist");
        let scan_root = parent.path().join("scan-root");
        let original = parent.path().join("original-root");
        std::fs::create_dir(&scan_root).expect("scan root should be created");
        std::fs::write(scan_root.join("target"), b"original").expect("target should be written");
        let plan = build_plan(
            &scan_root,
            target(&scan_root, OsString::from("target"), FileType::File),
            false,
        )
        .expect("file plan should build");

        std::fs::rename(&scan_root, &original).expect("original root should be displaced");
        std::fs::create_dir(&scan_root).expect("replacement root should be created");
        std::fs::write(scan_root.join("target"), b"replacement")
            .expect("replacement target should be written");

        let report = execute_plan(
            &scan_root,
            plan,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
        );

        assert_eq!(report.failed_entries(), 1);
        assert!(scan_root.join("target").exists());
        assert!(original.join("target").exists());
    }

    #[cfg(unix)]
    #[test]
    fn allocated_bytes_are_reported_only_after_last_hard_link_deletion() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let first = root.path().join("first");
        let second = root.path().join("second");
        std::fs::write(&first, b"payload").expect("hard-link source should be written");
        std::fs::hard_link(&first, &second).expect("hard link should be created");

        let first_plan = build_plan(
            root.path(),
            target(root.path(), OsString::from("first"), FileType::File),
            false,
        )
        .expect("first hard-link plan should build");
        let first_allocated = first_plan
            .root_snapshot()
            .and_then(|snapshot| snapshot.allocated_bytes)
            .expect("first allocation should be known");
        let first_report = execute_plan(
            root.path(),
            first_plan,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
        );
        assert_eq!(first_report.deleted_allocated_bytes(), 0);
        assert!(second.exists());

        let second_report = execute_plan(
            root.path(),
            build_plan(
                root.path(),
                target(root.path(), OsString::from("second"), FileType::File),
                false,
            )
            .expect("last hard-link plan should build"),
            &AtomicBool::new(false),
            &AtomicBool::new(false),
        );
        assert_eq!(second_report.deleted_allocated_bytes(), first_allocated);
        assert!(!second.exists());
    }
    #[cfg(unix)]
    #[test]
    fn external_hard_link_created_after_planning_is_not_reported_as_freed() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let outside = tempfile::tempdir().expect("external root should exist");
        let external = outside.path().join("external");
        let first = root.path().join("first");
        std::fs::write(&first, b"payload").expect("hard-link source should be written");
        let plan = build_plan(
            root.path(),
            target(root.path(), OsString::from("first"), FileType::File),
            false,
        )
        .expect("file plan should build");
        std::fs::hard_link(&first, &external).expect("external hard link should be created");

        let report = execute_plan(
            root.path(),
            plan,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
        );

        assert_eq!(report.deleted_entries(), 1);
        assert_eq!(report.deleted_allocated_bytes(), 0);
        assert_eq!(
            external
                .metadata()
                .expect("external link should remain")
                .len(),
            7
        );
    }

    #[cfg(unix)]
    #[test]
    fn planned_hard_links_still_report_allocation_after_both_delete() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let directory = root.path().join("target");
        std::fs::create_dir(&directory).expect("target directory should be created");
        let first = directory.join("first");
        let second = directory.join("second");
        std::fs::write(&first, b"payload").expect("hard-link source should be written");
        std::fs::hard_link(&first, &second).expect("hard link should be created");
        let plan = build_plan(
            root.path(),
            target(root.path(), OsString::from("target"), FileType::Folder),
            false,
        )
        .expect("directory plan should build");
        let allocated = plan
            .entries
            .iter()
            .find(|entry| entry.relative_path == Path::new("target/first"))
            .and_then(|entry| entry.snapshot.allocated_bytes)
            .expect("allocation should be known");

        let report = execute_plan(
            root.path(),
            plan,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
        );

        assert_eq!(report.deleted_entries(), 3);
        assert_eq!(report.deleted_allocated_bytes(), allocated);
        assert!(!directory.exists());
    }
}
