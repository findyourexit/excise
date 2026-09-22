use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::Metadata;
use std::path::{Path, PathBuf};

use crate::deletion::{
    DeletionEntryOutcome, DeletionReport, PlannedKind, PlannedSnapshot, ReviewedEntry, same_object,
    validate_scan_root_identity,
};
use crate::filter::FilterPattern;
use crate::model::{
    Arena, MemoryBudget, ModelError, Node, NodeId, NodeKind, NodeState, SyntheticKind,
    UnscannedReason,
};
use crate::native_path::NativeIdentity;
use crate::state::FileToDelete;
use crate::state::tiles::{FileMetadata, FileType, files_in_folder};
use crate::temporary_storage::TemporaryStorage;
use file_id::FileId;

struct RescanStage {
    target_id: NodeId,
    target: PathBuf,
    arena: Arena,
    filter: Option<FilterPattern>,
    filter_root: Option<PathBuf>,
    reserved_stage_bytes: usize,
}

/// The UI owns this while it exposes the scan field. One cold subtree is
/// compacted per owner-loop turn before the bounded stage can be allocated.
struct RescanPreparation {
    target_id: NodeId,
    target: PathBuf,
    filter: Option<FilterPattern>,
    filter_root: Option<PathBuf>,
    desired_stage_bytes: usize,
    pinned: HashSet<NodeId>,
}

/// The owner either continues bounded compaction, starts the stage, or returns
/// to the live map without treating unavailable optional capacity as a failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RescanPreparationProgress {
    Compacting,
    Ready,
    InsufficientCapacity,
}
const MAX_STALE_SCAN_PREFIXES: usize = 32;
const MIN_FOCUSED_SCAN_MODEL_BYTES: usize = 8 * 1024 * 1024;
const FOCUSED_SCAN_MODEL_NUMERATOR: usize = 1;
const FOCUSED_SCAN_MODEL_DENOMINATOR: usize = 3;

pub struct FileTree {
    pub current_path: Vec<NodeId>,
    pub space_freed: u128,
    pub failed_to_read: u64,
    pub path_in_filesystem: PathBuf,
    arena: Arena,
    root_identity: Option<NativeIdentity>,
    show_apparent_size: bool,
    filter: Option<FilterPattern>,
    filter_root: Option<PathBuf>,
    rescan: Option<RescanStage>,
    rescan_preparation: Option<RescanPreparation>,
    /// Prefixes removed or authoritatively replaced while primary events may still arrive.
    stale_scan_prefixes: Vec<PathBuf>,
}

impl FileTree {
    pub fn new(
        path_in_filesystem: PathBuf,
        show_apparent_size: bool,
        process_memory_mib: usize,
    ) -> Result<Self, ModelError> {
        Self::new_with_temporary_storage(
            path_in_filesystem,
            show_apparent_size,
            process_memory_mib,
            TemporaryStorage::default(),
        )
    }

    pub(crate) fn new_with_temporary_storage(
        path_in_filesystem: PathBuf,
        show_apparent_size: bool,
        process_memory_mib: usize,
        temporary_storage: TemporaryStorage,
    ) -> Result<Self, ModelError> {
        let budget = MemoryBudget::from_mib(process_memory_mib)?;
        let arena = Arena::new_with_temporary_storage(
            path_in_filesystem.clone(),
            budget,
            temporary_storage,
        )?;
        Ok(Self {
            current_path: vec![arena.root()],
            arena,
            root_identity: None,
            path_in_filesystem,
            space_freed: 0,
            failed_to_read: 0,
            show_apparent_size,
            filter: None,
            filter_root: None,
            rescan: None,
            rescan_preparation: None,
            stale_scan_prefixes: Vec::with_capacity(MAX_STALE_SCAN_PREFIXES),
        })
    }

    pub fn new_with_root_identity(
        path_in_filesystem: PathBuf,
        root_identity: NativeIdentity,
        show_apparent_size: bool,
        process_memory_mib: usize,
    ) -> Result<Self, ModelError> {
        Self::new_with_root_identity_and_temporary_storage(
            path_in_filesystem,
            root_identity,
            show_apparent_size,
            process_memory_mib,
            TemporaryStorage::default(),
        )
    }

    pub(crate) fn new_with_root_identity_and_temporary_storage(
        path_in_filesystem: PathBuf,
        root_identity: NativeIdentity,
        show_apparent_size: bool,
        process_memory_mib: usize,
        temporary_storage: TemporaryStorage,
    ) -> Result<Self, ModelError> {
        validate_scan_root_identity(&path_in_filesystem, &root_identity)
            .map_err(|error| ModelError::Invariant(error.to_string()))?;
        let mut tree = Self::new_with_temporary_storage(
            path_in_filesystem,
            show_apparent_size,
            process_memory_mib,
            temporary_storage,
        )?;
        tree.root_identity = Some(root_identity.clone());
        tree.arena.set_root_identity(root_identity);
        Ok(tree)
    }

    #[must_use]
    pub fn current_id(&self) -> NodeId {
        self.current_path
            .last()
            .copied()
            .unwrap_or(self.arena.root())
    }

    #[must_use]
    pub fn current_node(&self) -> &Node {
        self.arena
            .node(self.current_id())
            .or_else(|| self.arena.node(self.arena.root()))
            .expect("arena root must exist")
    }

    #[must_use]
    pub fn total_node(&self) -> &Node {
        self.arena
            .node(self.arena.root())
            .expect("arena root must exist")
    }

    #[must_use]
    pub fn get_current_path(&self) -> PathBuf {
        self.arena
            .path_for(self.current_id())
            .unwrap_or_else(|| self.path_in_filesystem.clone())
    }

    #[must_use]
    pub fn path_for_id(&self, id: NodeId) -> Option<PathBuf> {
        self.arena.path_for(id)
    }

    #[must_use]
    pub(crate) const fn show_apparent_size(&self) -> bool {
        self.show_apparent_size
    }

    #[must_use]
    pub fn node_kind(&self, id: NodeId) -> Option<NodeKind> {
        self.arena.node(id).map(|node| node.kind)
    }

    #[must_use]
    pub fn node_state(&self, id: NodeId) -> Option<crate::model::NodeState> {
        self.arena.node(id).map(|node| node.state)
    }

    #[must_use]
    pub fn node(&self, id: NodeId) -> Option<&Node> {
        self.arena.node(id)
    }

    #[must_use]
    pub fn entry_snapshot(&self, id: NodeId) -> Option<crate::model::EntrySnapshot> {
        self.arena.node(id).map(|node| node.snapshot.clone())
    }

    #[must_use]
    pub fn identity_for_path(&self, path: &Path) -> Option<NativeIdentity> {
        let id = self.arena.path_ids(path)?.last().copied()?;
        self.arena.node(id)?.snapshot.identity.clone()
    }
    /// Returns a concrete, identity-bound deletion target for any retained real entry.
    ///
    /// The background planner re-enumerates directories from the live filesystem,
    /// then revalidates every entry immediately before mutation. A partially scanned
    /// or memory-summarized directory can therefore be deleted safely without
    /// waiting for this display model to retain all of its descendants.
    #[allow(
        clippy::too_many_lines,
        reason = "target construction keeps every target-kind safety guard in one auditable decision boundary"
    )]
    pub fn deletion_target_for_path(&self, path: &Path) -> Result<FileToDelete, ModelError> {
        let node_id = self
            .arena
            .path_ids(path)
            .and_then(|ids| ids.last().copied())
            .ok_or_else(|| ModelError::InvalidPath(path.to_string_lossy().into_owned()))?;
        let backing_path = self
            .arena
            .path_for(node_id)
            .ok_or_else(|| ModelError::Invariant("deletion path disappeared".to_string()))?;
        let node = self
            .arena
            .node(node_id)
            .ok_or_else(|| ModelError::InvalidPath(path.to_string_lossy().into_owned()))?;
        let expected_kind = match node.kind {
            NodeKind::Root => {
                return Err(ModelError::Invariant(
                    "scan roots cannot be deleted".to_string(),
                ));
            }
            NodeKind::Synthetic(SyntheticKind::Aggregate) => NodeKind::Directory,
            NodeKind::Synthetic(_) => {
                return Err(ModelError::Invariant(
                    "virtual summaries cannot be deleted".to_string(),
                ));
            }
            NodeKind::Directory | NodeKind::File | NodeKind::Link => node.kind,
        };
        if node.snapshot.kind != node.kind
            && node.kind != NodeKind::Synthetic(SyntheticKind::Aggregate)
        {
            return Err(ModelError::Invariant(
                "deletion target has an inconsistent model snapshot".to_string(),
            ));
        }
        let mut expected_snapshot = node.snapshot.clone();
        expected_snapshot.kind = expected_kind;
        if expected_snapshot.identity.is_none() {
            return Err(ModelError::Invariant(
                "deletion requires a verified concrete backing path".to_string(),
            ));
        }
        let relative = backing_path
            .strip_prefix(&self.path_in_filesystem)
            .map_err(|_| ModelError::InvalidPath(path.to_string_lossy().into_owned()))?;
        let (file_type, num_descendants) = match expected_kind {
            NodeKind::Directory => (FileType::Folder, Some(node.metrics.descendants)),
            NodeKind::File | NodeKind::Link => (FileType::File, None),
            NodeKind::Root | NodeKind::Synthetic(_) => {
                unreachable!("only concrete non-root targets reach deletion target construction")
            }
        };
        Ok(FileToDelete {
            node_id,
            synthetic: false,
            path_in_filesystem: self.path_in_filesystem.clone(),
            path_to_file: relative.iter().map(OsStr::to_os_string).collect(),
            file_type,
            num_descendants,
            size: if self.show_apparent_size {
                node.metrics.apparent_bytes
            } else {
                node.metrics.allocated_bytes.lower
            },
            expected_snapshot,
            reviewed_entries: Vec::new(),
        })
    }

    pub fn reviewed_subtree(
        &self,
        root: NodeId,
        maximum_bytes: usize,
    ) -> Result<Vec<ReviewedEntry>, ModelError> {
        let mut reviewed = Vec::new();
        let mut stack = vec![root];
        let mut used = 0_usize;
        while let Some(id) = stack.pop() {
            let node = self
                .arena
                .node(id)
                .ok_or_else(|| ModelError::Invariant("reviewed node disappeared".to_string()))?;
            if node.kind == NodeKind::Synthetic(SyntheticKind::Shared) {
                continue;
            }
            if node.state != NodeState::Complete || node.kind.is_synthetic() {
                return Err(ModelError::Invariant(
                    "deletion requires a fully materialized subtree".to_string(),
                ));
            }
            let identity = node.snapshot.identity.clone().ok_or_else(|| {
                ModelError::Invariant("reviewed entry has no stable identity".to_string())
            })?;
            let kind = match node.kind {
                NodeKind::Directory => PlannedKind::Directory,
                NodeKind::File => PlannedKind::File,
                NodeKind::Link => PlannedKind::Link,
                NodeKind::Root | NodeKind::Synthetic(_) => {
                    return Err(ModelError::Invariant(
                        "scan roots and synthetic nodes cannot be reviewed".to_string(),
                    ));
                }
            };
            let path = self
                .arena
                .path_for(id)
                .ok_or_else(|| ModelError::Invariant("reviewed path disappeared".to_string()))?;
            let relative_path = path
                .strip_prefix(&self.path_in_filesystem)
                .map_err(|_| ModelError::InvalidPath(path.to_string_lossy().into_owned()))?
                .to_path_buf();
            let required = std::mem::size_of::<ReviewedEntry>()
                .saturating_add(
                    relative_path
                        .as_os_str()
                        .as_encoded_bytes()
                        .len()
                        .saturating_mul(2),
                )
                .saturating_add(128);
            used = used.saturating_add(required);
            if used > maximum_bytes {
                return Err(ModelError::MemoryExhausted {
                    required: used,
                    limit: maximum_bytes,
                });
            }
            reviewed.push(ReviewedEntry {
                relative_path,
                snapshot: PlannedSnapshot {
                    identity,
                    kind,
                    apparent_bytes: node.snapshot.apparent_bytes,
                    allocated_bytes: node.snapshot.allocated_bytes,
                    modified_nanos: node.snapshot.modified_nanos,
                },
            });
            stack.extend(node.children.iter().copied());
        }
        reviewed.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        Ok(reviewed)
    }

    pub fn enter_folder(&mut self, id: NodeId) -> bool {
        if self.arena.node(id).is_some_and(|node| {
            node.kind.is_directory() || node.kind == NodeKind::Synthetic(SyntheticKind::Aggregate)
        }) {
            self.current_path.push(id);
            self.arena.touch(id);
            true
        } else {
            false
        }
    }

    pub(crate) fn enter_path(&mut self, path: &Path) -> bool {
        let Some(id) = self
            .arena
            .path_ids(path)
            .and_then(|ids| ids.last().copied())
        else {
            return false;
        };
        self.enter_folder(id)
    }

    pub fn nodes(&self) -> impl Iterator<Item = &Node> {
        self.arena.nodes()
    }

    pub fn leave_folder(&mut self) -> bool {
        if self.current_path.len() > 1 {
            self.current_path.pop();
            true
        } else {
            false
        }
    }

    #[allow(dead_code)]
    pub fn apply_deletion_report(&mut self, report: &DeletionReport) {
        let _ = self.try_apply_deletion_report(report);
    }

    #[allow(
        clippy::too_many_lines,
        reason = "streamed report reconciliation deliberately keeps its bounded uncertainty rules together"
    )]
    pub fn try_apply_deletion_report(&mut self, report: &DeletionReport) -> Result<(), ModelError> {
        let previous_path = self.get_current_path();
        let report_target_is_valid = report.scan_root == self.path_in_filesystem
            && report.contains_entry_path(&report.root_relative_path);
        let mut target_uncertain =
            !report_target_is_valid || !report.precise || !report.reporting_complete();
        let mut tree_uncertain = target_uncertain;
        // Reports can spill far beyond model memory. Retain only current model
        // node IDs and their affected identities, never every report path.
        let mut removed_nodes = HashSet::new();
        let mut affected_link_counts: HashMap<FileId, Option<u64>> = HashMap::new();
        let mut removed_target = false;
        if report_target_is_valid {
            for result in &report.entries {
                let Ok(result) = result else {
                    target_uncertain = true;
                    tree_uncertain = true;
                    break;
                };
                if !report.contains_entry_path(&result.entry.relative_path) {
                    target_uncertain = true;
                    tree_uncertain = true;
                    continue;
                }
                let removes_target = result.entry.relative_path == report.root_relative_path
                    && matches!(
                        &result.outcome,
                        DeletionEntryOutcome::Deleted | DeletionEntryOutcome::Missing
                    );
                if !matches!(
                    &result.outcome,
                    DeletionEntryOutcome::Deleted | DeletionEntryOutcome::Missing
                ) {
                    target_uncertain = true;
                    continue;
                }
                removed_target |= removes_target;
                let Some(node_id) = self
                    .arena
                    .node_id_for_relative_path(&result.entry.relative_path)
                else {
                    // A live planner can delete descendants that this bounded display
                    // model never retained. There is no stale model entry to repair.
                    continue;
                };
                let model_matches = self.arena.node(node_id).is_some_and(|node| {
                    node_matches_planned_identity(node, &result.entry.snapshot)
                });
                if node_id == self.arena.root() || !model_matches {
                    target_uncertain = true;
                    tree_uncertain = true;
                    continue;
                }
                if !removed_nodes.contains(&node_id) {
                    if removed_nodes.try_reserve(1).is_err() {
                        target_uncertain = true;
                        tree_uncertain = true;
                        break;
                    }
                    removed_nodes.insert(node_id);
                }
                if !matches!(
                    result.entry.snapshot.kind,
                    PlannedKind::File | PlannedKind::Link
                ) {
                    continue;
                }
                let file_id = result.entry.snapshot.identity.file_id;
                let post_delete = matches!(&result.outcome, DeletionEntryOutcome::Deleted)
                    .then_some(result.entry.snapshot.identity.link_count)
                    .flatten()
                    .map(|count| count.saturating_sub(1));
                if let Some(current) = affected_link_counts.get_mut(&file_id) {
                    *current = match (*current, post_delete) {
                        (Some(_), Some(next)) => Some(next),
                        _ => None,
                    };
                } else if affected_link_counts.try_reserve(1).is_err() {
                    target_uncertain = true;
                    tree_uncertain = true;
                    break;
                } else {
                    affected_link_counts.insert(file_id, post_delete);
                }
            }
        }
        self.arena
            .try_remove_node_ids_with_link_counts(&removed_nodes, &affected_link_counts)?;
        if removed_target {
            self.ignore_stale_scan_prefix(self.path_in_filesystem.join(&report.root_relative_path));
        }
        self.restore_navigation(&previous_path);
        let filter_root_removed = self
            .filter_root
            .as_deref()
            .is_some_and(|root| self.arena.path_ids(root).is_none());
        if filter_root_removed {
            self.filter_root = self.filter.as_ref().map(|_| self.get_current_path());
        }
        if target_uncertain {
            self.mark_deletion_result_uncertain(report, report_target_is_valid, tree_uncertain);
        }
        if report_target_is_valid {
            let freed = if self.show_apparent_size {
                report.deleted_apparent_bytes()
            } else {
                report.deleted_allocated_bytes()
            };
            self.space_freed = self.space_freed.saturating_add(freed);
        }
        Ok(())
    }

    fn mark_deletion_result_uncertain(
        &mut self,
        report: &DeletionReport,
        report_target_is_valid: bool,
        whole_tree: bool,
    ) {
        if report_target_is_valid {
            let target = self.path_in_filesystem.join(&report.root_relative_path);
            self.arena.mark_path_uncertain(
                &target,
                UnscannedReason::Metadata("deletion result requires a focused rescan".to_string()),
            );
        }
        if whole_tree {
            self.arena.mark_path_uncertain(
                &self.path_in_filesystem,
                UnscannedReason::Metadata(
                    "deletion result has unresolved identity accounting".to_string(),
                ),
            );
        }
    }

    fn scan_path_is_stale(&self, path: &Path) -> bool {
        self.stale_scan_prefixes
            .iter()
            .any(|prefix| path == prefix || path.starts_with(prefix))
    }

    #[must_use]
    pub(crate) fn primary_scan_path_is_stale(&self, path: &Path) -> bool {
        self.scan_path_is_stale(path)
    }

    fn ignore_stale_scan_prefix(&mut self, path: PathBuf) {
        if self.scan_path_is_stale(&path) {
            return;
        }
        self.stale_scan_prefixes
            .retain(|prefix| !prefix.starts_with(&path));
        if self.stale_scan_prefixes.len() == MAX_STALE_SCAN_PREFIXES {
            // A bounded foreground model must not retain arbitrary deletion history.
            // Ignoring the remaining primary scan is safer than reviving a removed entry.
            self.stale_scan_prefixes.clear();
            self.stale_scan_prefixes
                .push(self.path_in_filesystem.clone());
        } else {
            self.stale_scan_prefixes.push(path);
        }
    }

    pub fn add_entry(
        &mut self,
        entry_metadata: &Metadata,
        entry_full_path: &Path,
        identity: &NativeIdentity,
    ) -> Result<Option<NodeId>, ModelError> {
        if self.rescan.is_some() {
            self.add_focused_entry(entry_metadata, entry_full_path, identity)
        } else {
            self.add_primary_entry(entry_metadata, entry_full_path, identity)
        }
    }

    pub(crate) fn add_primary_entry(
        &mut self,
        entry_metadata: &Metadata,
        entry_full_path: &Path,
        identity: &NativeIdentity,
    ) -> Result<Option<NodeId>, ModelError> {
        if self.scan_path_is_stale(entry_full_path) {
            return Ok(None);
        }
        let filter = self.filter.as_ref();
        let filter_root = self.filter_root.as_deref();
        let current_path = self.current_path.as_slice();
        add_entry_to(
            &mut self.arena,
            filter,
            filter_root,
            |arena| pinned_nodes_for(arena, current_path, filter_root),
            entry_metadata,
            entry_full_path,
            identity,
        )
    }
    pub(crate) fn add_focused_entry(
        &mut self,
        entry_metadata: &Metadata,
        entry_full_path: &Path,
        identity: &NativeIdentity,
    ) -> Result<Option<NodeId>, ModelError> {
        let stage = self.rescan.as_mut().ok_or_else(|| {
            ModelError::Invariant("focused scan entry arrived without a staging model".to_string())
        })?;
        add_entry_to(
            &mut stage.arena,
            stage.filter.as_ref(),
            stage.filter_root.as_deref(),
            |arena| HashSet::from([arena.root()]),
            entry_metadata,
            entry_full_path,
            identity,
        )
    }
    #[allow(clippy::needless_pass_by_value)]
    pub fn record_unscanned(
        &mut self,
        path: &Path,
        reason: UnscannedReason,
    ) -> Result<(), ModelError> {
        if self.rescan.is_some() {
            self.record_focused_unscanned(path, reason)
        } else {
            self.record_primary_unscanned(path, reason)
        }
    }

    #[allow(clippy::needless_pass_by_value)]
    pub(crate) fn record_primary_unscanned(
        &mut self,
        path: &Path,
        reason: UnscannedReason,
    ) -> Result<(), ModelError> {
        if self.scan_path_is_stale(path) {
            return Ok(());
        }
        let filter_root = self.filter_root.as_deref();
        let current_path = self.current_path.as_slice();
        record_unscanned_to(
            &mut self.arena,
            |arena| pinned_nodes_for(arena, current_path, filter_root),
            path,
            &reason,
        )
    }

    #[allow(clippy::needless_pass_by_value)]
    pub(crate) fn record_focused_unscanned(
        &mut self,
        path: &Path,
        reason: UnscannedReason,
    ) -> Result<(), ModelError> {
        let stage = self.rescan.as_mut().ok_or_else(|| {
            ModelError::Invariant("focused scan result arrived without a staging model".to_string())
        })?;
        record_unscanned_to(
            &mut stage.arena,
            |arena| HashSet::from([arena.root()]),
            path,
            &reason,
        )
    }

    pub fn complete_directory(
        &mut self,
        path: &Path,
        expected_identity: Option<&NativeIdentity>,
    ) -> Result<(), ModelError> {
        if self.rescan.is_some() {
            self.complete_focused_directory(path, expected_identity)
        } else {
            self.complete_primary_directory(path, expected_identity)
        }
    }

    pub(crate) fn complete_primary_directory(
        &mut self,
        path: &Path,
        expected_identity: Option<&NativeIdentity>,
    ) -> Result<(), ModelError> {
        if self.scan_path_is_stale(path) {
            return Ok(());
        }
        self.arena.complete_directory(path, expected_identity)
    }

    pub(crate) fn complete_focused_directory(
        &mut self,
        path: &Path,
        expected_identity: Option<&NativeIdentity>,
    ) -> Result<(), ModelError> {
        self.rescan
            .as_mut()
            .ok_or_else(|| {
                ModelError::Invariant(
                    "focused directory completion arrived without a staging model".to_string(),
                )
            })?
            .arena
            .complete_directory(path, expected_identity)
    }

    pub fn finalize(&mut self) -> Result<(), ModelError> {
        self.stale_scan_prefixes.clear();
        self.arena.finalize()
    }

    #[must_use]
    pub fn files_in_current_folder(&self, offset: usize) -> Vec<FileMetadata> {
        files_in_folder(
            &self.arena,
            self.current_id(),
            offset,
            self.show_apparent_size,
            self.filter.as_ref(),
            self.filter_root.as_deref(),
        )
    }

    pub fn set_filter(&mut self, filter: Option<FilterPattern>) {
        self.filter_root = filter.as_ref().map(|_| self.get_current_path());
        self.filter = filter;
    }

    #[must_use]
    pub fn filter(&self) -> Option<&FilterPattern> {
        self.filter.as_ref()
    }

    /// Starts a synchronous focused rescan stage for noninteractive callers.
    /// The interactive owner uses [`Self::begin_rescan_preparation`] and advances
    /// the compaction incrementally so its loading surface remains responsive.
    pub fn begin_rescan(
        &mut self,
        target: PathBuf,
        filter: Option<FilterPattern>,
    ) -> Result<(), ModelError> {
        self.begin_rescan_preparation(target, filter)?;
        loop {
            match self.advance_rescan_preparation() {
                Ok(RescanPreparationProgress::Ready) => return Ok(()),
                Ok(RescanPreparationProgress::Compacting) => {}
                Ok(RescanPreparationProgress::InsufficientCapacity) => {
                    let preparation = self
                        .rescan_preparation
                        .take()
                        .expect("rescan preparation was checked above");
                    return Err(ModelError::MemoryExhausted {
                        required: self
                            .arena
                            .memory_used()
                            .saturating_add(preparation.desired_stage_bytes),
                        limit: self.arena.memory_limit(),
                    });
                }
                Err(error) => {
                    self.rescan_preparation = None;
                    return Err(error);
                }
            }
        }
    }

    pub(crate) fn begin_rescan_preparation(
        &mut self,
        target: PathBuf,
        filter: Option<FilterPattern>,
    ) -> Result<(), ModelError> {
        if self.rescan.is_some() || self.rescan_preparation.is_some() {
            return Err(ModelError::Invariant(
                "focused rescan is already active".to_string(),
            ));
        }
        let target_id = self
            .arena
            .path_ids(&target)
            .and_then(|ids| ids.last().copied())
            .ok_or_else(|| ModelError::InvalidPath(target.to_string_lossy().into_owned()))?;
        if !self.arena.node(target_id).is_some_and(|node| {
            node.kind.is_directory() || node.kind == NodeKind::Synthetic(SyntheticKind::Aggregate)
        }) {
            return Err(ModelError::InvalidPath(
                target.to_string_lossy().into_owned(),
            ));
        }
        let filter_root = filter
            .as_ref()
            .map(|_| self.filter_root.clone().unwrap_or_else(|| target.clone()));
        let desired_stage_bytes = (self.arena.memory_limit() / FOCUSED_SCAN_MODEL_DENOMINATOR)
            .saturating_mul(FOCUSED_SCAN_MODEL_NUMERATOR)
            .max(MIN_FOCUSED_SCAN_MODEL_BYTES)
            .min(self.arena.memory_limit());
        let mut pinned = self.pinned_nodes();
        pinned.insert(target_id);
        self.rescan_preparation = Some(RescanPreparation {
            target_id,
            target,
            filter,
            filter_root,
            desired_stage_bytes,
            pinned,
        });
        Ok(())
    }

    /// Compacts at most one cold subtree and reports whether focused input may start.
    pub(crate) fn advance_rescan_preparation(
        &mut self,
    ) -> Result<RescanPreparationProgress, ModelError> {
        let preparation = self.rescan_preparation.as_ref().ok_or_else(|| {
            ModelError::Invariant("focused rescan preparation is not active".to_string())
        })?;
        let remaining = self
            .arena
            .memory_limit()
            .saturating_sub(self.arena.memory_used());
        if remaining < preparation.desired_stage_bytes {
            if self.arena.aggregate_cold_subtree(&preparation.pinned)? {
                return Ok(RescanPreparationProgress::Compacting);
            }
            return Ok(RescanPreparationProgress::InsufficientCapacity);
        }

        let preparation = self
            .rescan_preparation
            .take()
            .expect("rescan preparation was checked above");
        let budget = MemoryBudget::from_model_limit(preparation.desired_stage_bytes)?;
        self.arena
            .reserve_staging_capacity(preparation.desired_stage_bytes)?;
        let stage = Arena::new_with_temporary_storage(
            preparation.target.clone(),
            budget,
            self.arena.temporary_storage(),
        );
        match stage {
            Ok(arena) => {
                self.rescan = Some(RescanStage {
                    target_id: preparation.target_id,
                    target: preparation.target,
                    arena,
                    filter: preparation.filter,
                    filter_root: preparation.filter_root,
                    reserved_stage_bytes: preparation.desired_stage_bytes,
                });
                Ok(RescanPreparationProgress::Ready)
            }
            Err(error) => {
                self.arena
                    .release_staging_capacity(preparation.desired_stage_bytes);
                self.rescan_preparation = Some(preparation);
                Err(error)
            }
        }
    }

    pub fn finish_rescan(&mut self) -> Result<(), ModelError> {
        let previous_path = self.get_current_path();
        let stage = self
            .rescan
            .take()
            .ok_or_else(|| ModelError::Invariant("focused rescan is not active".to_string()))?;
        let RescanStage {
            target_id,
            target,
            arena,
            reserved_stage_bytes,
            ..
        } = stage;
        self.arena.release_staging_capacity(reserved_stage_bytes);
        self.arena.replace_subtree_from(target_id, arena)?;
        // Primary events can still arrive after a focused result is committed.
        // The focused traversal is authoritative for this path, so preserve it
        // until the primary scanner sends its terminal event.
        self.ignore_stale_scan_prefix(target);
        self.restore_navigation(&previous_path);
        self.failed_to_read = self.metadata_failure_count();
        Ok(())
    }

    pub fn cancel_rescan(&mut self) -> Result<(), ModelError> {
        if let Some(stage) = self.rescan.take() {
            self.arena
                .release_staging_capacity(stage.reserved_stage_bytes);
            return Ok(());
        }
        if self.rescan_preparation.take().is_some() {
            return Ok(());
        }
        Err(ModelError::Invariant(
            "focused rescan is not active".to_string(),
        ))
    }

    #[must_use]
    pub fn model_stats(&self) -> (usize, usize, bool) {
        (
            self.arena.memory_used(),
            self.arena.memory_limit(),
            self.arena.identity_spill_path().is_some(),
        )
    }

    #[must_use]
    pub fn internal_scan_paths(&self) -> Vec<PathBuf> {
        let mut paths = self.arena.internal_scan_paths();
        if let Some(stage) = self.rescan.as_ref() {
            paths.extend(stage.arena.internal_scan_paths());
        }
        paths
    }

    #[must_use]
    pub fn identity_count(&self) -> usize {
        self.arena.identity_count()
    }

    pub fn increment_failed_to_read(&mut self) {
        if self.rescan.is_none() && self.rescan_preparation.is_none() {
            self.failed_to_read = self.failed_to_read.saturating_add(1);
        }
    }

    fn pinned_nodes(&self) -> HashSet<NodeId> {
        pinned_nodes_for(&self.arena, &self.current_path, self.filter_root.as_deref())
    }

    fn restore_navigation(&mut self, previous_path: &Path) {
        let mut candidate = previous_path.to_path_buf();
        loop {
            if let Some(ids) = self.arena.path_ids(&candidate) {
                self.current_path = ids;
                return;
            }
            if !candidate.pop() {
                break;
            }
        }
        self.current_path = vec![self.arena.root()];
    }

    fn metadata_failure_count(&self) -> u64 {
        u64::try_from(
            self.nodes()
                .filter(|node| {
                    matches!(
                        node.unscanned_reason.as_ref(),
                        Some(UnscannedReason::Metadata(_) | UnscannedReason::Replacement(_))
                    )
                })
                .count(),
        )
        .unwrap_or(u64::MAX)
    }
}

fn add_entry_to(
    arena: &mut Arena,
    filter: Option<&FilterPattern>,
    filter_root: Option<&Path>,
    mut pinned_nodes: impl FnMut(&Arena) -> HashSet<NodeId>,
    entry_metadata: &Metadata,
    entry_full_path: &Path,
    identity: &NativeIdentity,
) -> Result<Option<NodeId>, ModelError> {
    let aggregate = filter_root.is_some_and(|root| {
        entry_full_path.starts_with(root)
            && !entry_metadata.is_dir()
            && filter.is_some_and(|filter| !filter.matches_path(entry_full_path, root))
    });
    loop {
        let result = if aggregate {
            arena.add_entry_aggregated(entry_full_path, entry_metadata, identity.clone())
        } else {
            arena.add_entry(entry_full_path, entry_metadata, identity.clone())
        };
        match result {
            Err(error @ ModelError::MemoryExhausted { .. }) => {
                let pinned = pinned_nodes(arena);
                if !arena.aggregate_cold_subtree(&pinned)? {
                    return Err(error);
                }
            }
            result => return result,
        }
    }
}

fn record_unscanned_to(
    arena: &mut Arena,
    mut pinned_nodes: impl FnMut(&Arena) -> HashSet<NodeId>,
    path: &Path,
    reason: &UnscannedReason,
) -> Result<(), ModelError> {
    loop {
        match arena.record_unscanned(path, reason.clone()) {
            Err(error @ ModelError::MemoryExhausted { .. }) => {
                let pinned = pinned_nodes(arena);
                if !arena.aggregate_cold_subtree(&pinned)? {
                    return Err(error);
                }
            }
            result => return result,
        }
    }
}

fn pinned_nodes_for(
    arena: &Arena,
    current_path: &[NodeId],
    filter_root: Option<&Path>,
) -> HashSet<NodeId> {
    let mut pinned = current_path.iter().copied().collect::<HashSet<_>>();
    if let Some(root) = filter_root
        && let Some(ids) = arena.path_ids(root)
    {
        pinned.extend(ids);
    }
    pinned
}
fn node_matches_planned_identity(node: &Node, planned: &PlannedSnapshot) -> bool {
    let kind_matches = matches!(
        (node.kind, planned.kind),
        (NodeKind::Directory, PlannedKind::Directory)
            | (
                NodeKind::Synthetic(SyntheticKind::Aggregate),
                PlannedKind::Directory
            )
            | (NodeKind::File, PlannedKind::File)
            | (NodeKind::Link, PlannedKind::Link)
    );
    // Deleted hard-link records intentionally clear the volatile link count.
    // The stable object identity still binds the outcome to this model node.
    kind_matches
        && node.snapshot.kind == node.kind
        && node
            .snapshot
            .identity
            .as_ref()
            .is_some_and(|identity| same_object(identity, &planned.identity))
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    #[cfg(any(unix, windows))]
    use std::ffi::OsString;
    use std::fs;
    #[cfg(any(unix, windows))]
    use std::sync::atomic::AtomicBool;

    #[cfg(any(unix, windows))]
    use crate::deletion::{
        DeletionEntryOutcome, DeletionEntryResult, DeletionReport, PlannedEntry, build_plan,
        execute_plan,
    };
    #[cfg(any(unix, windows))]
    use crate::model::ByteBounds;
    use crate::model::{NodeKind, NodeState, SyntheticKind};
    use crate::native_path::identity_for;
    #[cfg(any(unix, windows))]
    use crate::state::FileToDelete;
    #[cfg(any(unix, windows))]
    use crate::state::tiles::FileType;

    use super::*;

    fn add(tree: &mut FileTree, path: &Path) {
        let metadata = fs::symlink_metadata(path).expect("fixture metadata should exist");
        let identity = identity_for(path, &metadata)
            .expect("fixture identity should be readable")
            .expect("fixture should not be a link");
        tree.add_entry(&metadata, path, &identity)
            .expect("fixture should be added");
    }

    fn node_id_at(tree: &FileTree, path: &Path) -> NodeId {
        tree.nodes()
            .find_map(|node| {
                tree.path_for_id(node.id)
                    .is_some_and(|node_path| node_path == path)
                    .then_some(node.id)
            })
            .expect("fixture node should be retained")
    }

    fn model_state(
        tree: &FileTree,
    ) -> Vec<(
        NodeId,
        PathBuf,
        crate::model::NodeKind,
        crate::model::NodeState,
        crate::model::NodeMetrics,
        crate::model::EntrySnapshot,
    )> {
        let mut state = tree
            .nodes()
            .map(|node| {
                (
                    node.id,
                    tree.path_for_id(node.id)
                        .expect("fixture node path should be available"),
                    node.kind,
                    node.state,
                    node.metrics,
                    node.snapshot.clone(),
                )
            })
            .collect::<Vec<_>>();
        state.sort_by_key(|(id, ..)| *id);
        state
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn fitting_entry_does_not_construct_compaction_pins() {
        let root = tempfile::tempdir().expect("scan root should exist");
        let entry = root.path().join("entry");
        fs::write(&entry, b"payload").expect("fixture entry should exist");
        let metadata = fs::symlink_metadata(&entry).expect("fixture metadata should exist");
        let identity = identity_for(&entry, &metadata)
            .expect("fixture identity should be readable")
            .expect("fixture should not be a link");
        let mut tree = FileTree::new(
            root.path().to_path_buf(),
            false,
            crate::model::MIN_PROCESS_MIB,
        )
        .expect("tree should be created");
        let pins_constructed = std::cell::Cell::new(false);

        assert!(
            add_entry_to(
                &mut tree.arena,
                None,
                None,
                |_| {
                    pins_constructed.set(true);
                    HashSet::new()
                },
                &metadata,
                &entry,
                &identity,
            )
            .expect("fitting entry should be retained")
            .is_some()
        );
        assert!(
            !pins_constructed.get(),
            "compaction pins are only needed after model memory is exhausted"
        );
    }

    #[test]
    fn focused_glob_rescan_keeps_matches_and_exact_other() {
        let root = tempfile::tempdir().expect("rescan root should exist");
        let matched = root.path().join("matched.log");
        let omitted = root.path().join("omitted.tmp");
        fs::write(&matched, b"abc").expect("matched fixture should be written");
        fs::write(&omitted, b"defgh").expect("omitted fixture should be written");
        let filter = FilterPattern::new("*.log").expect("filter should compile");
        let mut tree = FileTree::new(
            root.path().to_path_buf(),
            true,
            crate::model::MIN_PROCESS_MIB,
        )
        .expect("file tree should be created");
        tree.begin_rescan(root.path().to_path_buf(), Some(filter))
            .expect("rescan should begin");

        add(&mut tree, &matched);
        add(&mut tree, &omitted);
        tree.finish_rescan().expect("rescan should finalize");

        assert_eq!(tree.total_node().metrics.apparent_bytes, 8);
        assert_eq!(tree.total_node().metrics.descendants, 2);
        let files = tree.files_in_current_folder(0);
        assert!(
            files
                .iter()
                .any(|file| file.name == OsStr::new("matched.log"))
        );
        assert!(files.iter().any(|file| {
            file.synthetic_kind == Some(SyntheticKind::Other)
                && file.apparent_size == 5
                && file.descendants == Some(1)
        }));
        assert!(
            !files
                .iter()
                .any(|file| file.name == OsStr::new("omitted.tmp"))
        );
        let other = files
            .iter()
            .find(|file| file.synthetic_kind == Some(SyntheticKind::Other))
            .expect("filtered entries should have a grouped summary");
        assert!(other.is_interactive());
        let other_path = tree
            .path_for_id(other.node_id)
            .expect("grouped summary should retain a model path");
        assert!(matches!(
            tree.deletion_target_for_path(&other_path),
            Err(crate::model::ModelError::Invariant(_))
        ));
    }

    #[test]
    fn focused_rescan_preserves_a_filter_changed_while_it_runs() {
        let root = tempfile::tempdir().expect("rescan root should exist");
        let log = root.path().join("kept.log");
        let temporary = root.path().join("kept.tmp");
        fs::write(&log, b"log").expect("log fixture should be written");
        fs::write(&temporary, b"tmp").expect("temporary fixture should be written");
        let mut tree = FileTree::new(
            root.path().to_path_buf(),
            true,
            crate::model::MIN_PROCESS_MIB,
        )
        .expect("file tree should be created");
        tree.set_filter(Some(
            FilterPattern::new("*.log").expect("initial filter should compile"),
        ));
        tree.begin_rescan(root.path().to_path_buf(), tree.filter().cloned())
            .expect("rescan should begin");
        tree.set_filter(Some(
            FilterPattern::new("*.tmp").expect("replacement filter should compile"),
        ));
        add(&mut tree, &log);
        add(&mut tree, &temporary);
        tree.finish_rescan().expect("rescan should finish");

        assert_eq!(tree.filter().map(FilterPattern::raw), Some("*.tmp"));
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn focused_scan_staging_does_not_divert_primary_scan_entries() {
        let root = tempfile::tempdir().expect("scan root should exist");
        let target = root.path().join("target");
        let target_child = target.join("child");
        let primary_entry = root.path().join("primary-entry");
        fs::create_dir(&target).expect("target should exist");
        fs::write(&target_child, b"focused").expect("target child should exist");
        fs::write(&primary_entry, b"primary").expect("primary entry should exist");
        let mut tree = FileTree::new(
            root.path().to_path_buf(),
            true,
            crate::model::MIN_PROCESS_MIB,
        )
        .expect("tree should be created");
        add(&mut tree, &target);
        tree.begin_rescan(target.clone(), None)
            .expect("focused staging should start");

        let primary_metadata = fs::symlink_metadata(&primary_entry)
            .expect("primary entry metadata should be readable");
        let primary_identity = identity_for(&primary_entry, &primary_metadata)
            .expect("primary entry should be identifiable")
            .expect("primary entry should not be a link");
        tree.add_primary_entry(&primary_metadata, &primary_entry, &primary_identity)
            .expect("primary entry should remain in the live model");
        let focused_metadata =
            fs::symlink_metadata(&target_child).expect("focused child metadata should be readable");
        let focused_identity = identity_for(&target_child, &focused_metadata)
            .expect("focused child should be identifiable")
            .expect("focused child should not be a link");
        tree.add_focused_entry(&focused_metadata, &target_child, &focused_identity)
            .expect("focused child should enter the staging model");
        tree.complete_focused_directory(&target, None)
            .expect("focused target should complete");
        tree.finish_rescan()
            .expect("focused result should replace only its target subtree");

        assert!(tree.arena.path_ids(&primary_entry).is_some());
        assert!(tree.arena.path_ids(&target_child).is_some());
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn directory_with_unfinished_descendants_can_be_planned_directly() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let parent = root.path().join("parent");
        let child = parent.join("child");
        fs::create_dir(&parent).expect("parent should be created");
        fs::create_dir(&child).expect("child should be created");

        let mut tree = FileTree::new(
            root.path().to_path_buf(),
            true,
            crate::model::MIN_PROCESS_MIB,
        )
        .expect("tree should be created");
        add(&mut tree, &parent);
        add(&mut tree, &child);

        let target = tree
            .deletion_target_for_path(&parent)
            .expect("a visible incomplete directory should be deletable");
        assert_eq!(target.file_type, FileType::Folder);
        assert_eq!(target.expected_snapshot.kind, NodeKind::Directory);
        let plan = build_plan(root.path(), target, false)
            .expect("planner should independently review the live directory");
        assert_eq!(plan.planned_entries(), 2);
        let report = execute_plan(
            root.path(),
            plan,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
        );
        tree.try_apply_deletion_report(&report)
            .expect("incomplete model deletion should reconcile its retained target");
        assert!(!parent.exists());
        assert!(tree.arena.path_ids(&parent).is_none());
        assert_eq!(
            tree.total_node().state,
            NodeState::Scanning,
            "reconciliation must not finalize an active primary scan"
        );
    }
    #[test]
    fn focused_rescan_reports_insufficient_capacity_without_model_error() {
        let root = tempfile::tempdir().expect("rescan root should exist");
        let target = root.path().join("target");
        let old = target.join("old");
        fs::create_dir(&target).expect("target should be created");
        fs::write(&old, b"old").expect("old fixture should be written");

        let mut tree = FileTree::new(
            root.path().to_path_buf(),
            true,
            crate::model::MIN_PROCESS_MIB,
        )
        .expect("file tree should be created");
        add(&mut tree, &target);
        add(&mut tree, &old);
        for path in [&target, root.path()] {
            tree.complete_directory(path, None)
                .expect("fixture directory should complete");
        }
        tree.finalize().expect("fixture tree should finalize");
        let before = model_state(&tree);
        tree.arena
            .consume_remaining_budget_for_test()
            .expect("fixture should fill its model budget");

        tree.begin_rescan_preparation(target, None)
            .expect("focused preparation should begin");
        assert_eq!(
            tree.advance_rescan_preparation()
                .expect("capacity pressure should be a recoverable preparation result"),
            RescanPreparationProgress::InsufficientCapacity
        );
        assert_eq!(model_state(&tree), before);
        assert!(tree.rescan.is_none());
        assert!(tree.rescan_preparation.is_some());
        tree.cancel_rescan()
            .expect("unavailable focused preparation should cancel cleanly");
    }

    #[test]
    fn focused_rescan_reserves_a_fixed_share_of_the_model_budget() {
        let root = tempfile::tempdir().expect("rescan root should exist");
        let target = root.path().join("target");
        fs::create_dir(&target).expect("target should be created");

        let mut tree = FileTree::new(
            root.path().to_path_buf(),
            true,
            crate::model::MIN_PROCESS_MIB,
        )
        .expect("file tree should be created");
        add(&mut tree, &target);
        let before = tree.arena.memory_used();
        let stage_bytes = tree.arena.memory_limit() / 3;

        tree.begin_rescan(target, None)
            .expect("focused stage should initialize");
        assert_eq!(
            tree.rescan
                .as_ref()
                .expect("focused stage should exist")
                .arena
                .memory_limit(),
            stage_bytes,
            "the focused map capacity must not depend on when the user opens it"
        );
        assert_eq!(
            tree.arena.memory_used(),
            before.saturating_add(stage_bytes),
            "the primary model must reserve the focused stage capacity"
        );

        tree.cancel_rescan()
            .expect("focused stage should cancel cleanly");
        assert_eq!(tree.arena.memory_used(), before);
    }

    #[test]
    fn focused_rescan_preparation_yields_after_one_cold_subtree() {
        let root = tempfile::tempdir().expect("rescan root should exist");
        let target = root.path().join("target");
        let target_child = target.join("target-child");
        let cold = root.path().join("cold");
        fs::create_dir(&target).expect("target should be created");
        fs::write(&target_child, b"target").expect("target child should be written");
        fs::create_dir(&cold).expect("cold sibling should be created");
        let cold_children = (0..8)
            .map(|index| {
                let child = cold.join(format!("cold-child-{index}"));
                fs::write(&child, b"cold").expect("cold child should be written");
                child
            })
            .collect::<Vec<_>>();

        let mut tree = FileTree::new(
            root.path().to_path_buf(),
            true,
            crate::model::MIN_PROCESS_MIB,
        )
        .expect("file tree should be created");
        for path in [&target, &target_child, &cold] {
            add(&mut tree, path);
        }
        for child in &cold_children {
            add(&mut tree, child);
        }
        for path in [&target, &cold, root.path()] {
            tree.complete_directory(path, None)
                .expect("fixture directory should complete");
        }
        tree.finalize().expect("fixture tree should finalize");
        let cold_id = node_id_at(&tree, &cold);
        tree.arena
            .consume_remaining_budget_for_test()
            .expect("fixture should fill its model budget");

        tree.begin_rescan_preparation(target, None)
            .expect("visible target should begin staging preparation");
        assert_eq!(tree.node_kind(cold_id), Some(NodeKind::Directory));
        assert_eq!(
            tree.advance_rescan_preparation()
                .expect("one cold subtree should compact cleanly"),
            RescanPreparationProgress::Compacting,
            "preparation must return to the owner loop after one compaction"
        );
        assert_eq!(
            tree.node_kind(cold_id),
            Some(NodeKind::Synthetic(SyntheticKind::Aggregate))
        );
        assert!(tree.rescan.is_none());
        assert!(tree.rescan_preparation.is_some());
        tree.cancel_rescan().expect("fixture stage should clean up");
    }

    #[test]
    fn leaving_deep_navigation_reclaims_a_summarized_branch_at_memory_limit() {
        let root = tempfile::tempdir().expect("model root should exist");
        let mut directories = Vec::new();
        let mut current = root.path().to_path_buf();
        for level in 0..9 {
            current.push(format!("level-{level}"));
            fs::create_dir(&current).expect("deep fixture directory should be created");
            directories.push(current.clone());
        }
        let aggregated = current.join("aggregated");
        let pending = root.path().join("pending");
        fs::write(&aggregated, b"aggregated").expect("aggregate fixture should be written");
        fs::write(&pending, b"pending").expect("pending fixture should be written");

        let mut tree = FileTree::new(
            root.path().to_path_buf(),
            true,
            crate::model::MIN_PROCESS_MIB,
        )
        .expect("file tree should be created");
        for directory in &directories {
            add(&mut tree, directory);
        }
        let metadata = fs::symlink_metadata(&aggregated).expect("aggregate metadata should exist");
        let identity = identity_for(&aggregated, &metadata)
            .expect("aggregate identity should be readable")
            .expect("aggregate fixture should not be a link");
        assert!(
            tree.arena
                .add_entry_aggregated(&aggregated, &metadata, identity)
                .expect("aggregate fixture should be represented")
                .is_none()
        );
        tree.arena
            .record_unscanned(
                &current.join("Other"),
                UnscannedReason::Metadata("fixture metadata unavailable".to_string()),
            )
            .expect("Other should retain untracked metrics");

        for directory in directories.iter().rev() {
            tree.complete_directory(directory, None)
                .expect("fixture directory should complete");
        }
        tree.complete_directory(root.path(), None)
            .expect("fixture root should complete");
        for directory in &directories {
            assert!(tree.enter_folder(node_id_at(&tree, directory)));
        }
        for _ in &directories {
            assert!(tree.leave_folder());
        }
        assert_eq!(
            tree.current_path.len(),
            1,
            "navigation should return to root"
        );

        tree.arena
            .consume_remaining_budget_for_test()
            .expect("fixture should consume its model budget");
        add(&mut tree, &pending);

        let compacted = node_id_at(&tree, &directories[0]);
        assert_eq!(
            tree.node_kind(compacted),
            Some(NodeKind::Synthetic(SyntheticKind::Aggregate))
        );
        let target = tree
            .deletion_target_for_path(&directories[0])
            .expect("aggregate should retain a direct deletion target");
        assert_eq!(target.file_type, FileType::Folder);
        assert_eq!(target.expected_snapshot.kind, NodeKind::Directory);
        assert!(tree.path_for_id(node_id_at(&tree, &pending)).is_some());
    }

    #[test]
    fn root_scan_compacts_unscanned_leaf_at_model_limit() {
        let root = tempfile::tempdir().expect("model root should exist");
        let cold = root.path().join("cold");
        let unreadable = cold.join("unreadable");
        let pending = root.path().join("pending");
        fs::create_dir(&cold).expect("cold directory should be created");
        fs::write(&pending, b"pending").expect("pending file should be created");

        let mut tree = FileTree::new(
            root.path().to_path_buf(),
            true,
            crate::model::MIN_PROCESS_MIB,
        )
        .expect("file tree should be created");
        add(&mut tree, &cold);
        tree.record_unscanned(
            &unreadable,
            UnscannedReason::Metadata("fixture metadata unavailable".to_string()),
        )
        .expect("unreadable leaf should be represented");

        tree.arena
            .consume_remaining_budget_for_test()
            .expect("fixture should consume its model budget");
        add(&mut tree, &pending);
        tree.finalize().expect("model should finalize");
        let (used, limit, _) = tree.model_stats();
        assert!(used <= limit, "compaction must preserve the model limit");

        let compacted = node_id_at(&tree, &cold);
        assert_eq!(
            tree.node_kind(compacted),
            Some(NodeKind::Synthetic(SyntheticKind::Aggregate))
        );
        assert_eq!(
            tree.node(compacted)
                .expect("aggregate should remain")
                .metrics
                .allocated_bytes
                .upper,
            None,
            "the aggregated unreadable leaf must retain its unknown bound"
        );
        assert!(tree.path_for_id(node_id_at(&tree, &pending)).is_some());
    }

    #[test]
    fn root_scan_compacts_existing_unreadable_directory_at_model_limit() {
        let root = tempfile::tempdir().expect("model root should exist");
        let cold = root.path().join("cold");
        let unreadable = cold.join("unreadable");
        let pending = root.path().join("pending");
        fs::create_dir(&cold).expect("cold directory should be created");
        fs::create_dir(&unreadable).expect("unreadable directory should be created");
        fs::write(&pending, b"pending").expect("pending file should be created");

        let mut tree = FileTree::new(
            root.path().to_path_buf(),
            true,
            crate::model::MIN_PROCESS_MIB,
        )
        .expect("file tree should be created");
        add(&mut tree, &cold);
        add(&mut tree, &unreadable);
        tree.record_unscanned(
            &unreadable,
            UnscannedReason::Metadata("fixture metadata unavailable".to_string()),
        )
        .expect("unreadable directory should be represented");

        tree.arena
            .consume_remaining_budget_for_test()
            .expect("fixture should consume its model budget");
        add(&mut tree, &pending);
        tree.finalize().expect("model should finalize");
        let (used, limit, _) = tree.model_stats();
        assert!(used <= limit, "compaction must preserve the model limit");

        let compacted = node_id_at(&tree, &cold);
        assert_eq!(
            tree.node_kind(compacted),
            Some(NodeKind::Synthetic(SyntheticKind::Aggregate))
        );
        assert!(tree.path_for_id(node_id_at(&tree, &pending)).is_some());
    }

    #[test]
    fn cancelled_focused_rescan_discards_staging_without_touching_live_tree() {
        let root = tempfile::tempdir().expect("rescan root should exist");
        let target = root.path().join("target");
        let sibling = root.path().join("sibling");
        let old = target.join("old");
        let replacement = target.join("replacement");
        let retained = sibling.join("retained");
        fs::create_dir(&target).expect("target should be created");
        fs::create_dir(&sibling).expect("sibling should be created");
        fs::write(&old, b"old").expect("old fixture should be written");
        fs::write(&retained, b"retained").expect("retained fixture should be written");

        let mut tree = FileTree::new(
            root.path().to_path_buf(),
            true,
            crate::model::MIN_PROCESS_MIB,
        )
        .expect("file tree should be created");
        for path in [&target, &sibling, &old, &retained] {
            add(&mut tree, path);
        }
        for path in [&target, &sibling, root.path()] {
            tree.complete_directory(path, None)
                .expect("fixture directory should complete");
        }
        tree.finalize().expect("fixture tree should finalize");
        let sibling_id = node_id_at(&tree, &sibling);
        assert!(tree.enter_folder(sibling_id));
        let before = model_state(&tree);

        fs::remove_file(&old).expect("old fixture should be removed");
        fs::write(&replacement, b"replacement").expect("replacement fixture should be written");
        tree.begin_rescan(target.clone(), None)
            .expect("focused rescan should stage");
        add(&mut tree, &replacement);
        tree.complete_directory(&target, None)
            .expect("staged target should complete");
        tree.increment_failed_to_read();
        assert_eq!(tree.failed_to_read, 0);

        assert_eq!(model_state(&tree), before);
        tree.cancel_rescan().expect("staging should discard");
        assert_eq!(model_state(&tree), before);
        assert_eq!(tree.current_id(), sibling_id);
    }

    #[test]
    fn successful_focused_rescan_replaces_only_target_subtree() {
        let root = tempfile::tempdir().expect("rescan root should exist");
        let target = root.path().join("target");
        let sibling = root.path().join("sibling");
        let old = target.join("old");
        let replacement = target.join("replacement");
        let retained = sibling.join("retained");
        fs::create_dir(&target).expect("target should be created");
        fs::create_dir(&sibling).expect("sibling should be created");
        fs::write(&old, b"old").expect("old fixture should be written");
        fs::write(&retained, b"retained").expect("retained fixture should be written");

        let mut tree = FileTree::new(
            root.path().to_path_buf(),
            true,
            crate::model::MIN_PROCESS_MIB,
        )
        .expect("file tree should be created");
        for path in [&target, &sibling, &old, &retained] {
            add(&mut tree, path);
        }
        for path in [&target, &sibling, root.path()] {
            tree.complete_directory(path, None)
                .expect("fixture directory should complete");
        }
        tree.finalize().expect("fixture tree should finalize");
        let target_id = node_id_at(&tree, &target);
        let sibling_id = node_id_at(&tree, &sibling);
        let retained_id = node_id_at(&tree, &retained);
        let old_id = node_id_at(&tree, &old);
        assert!(tree.enter_folder(sibling_id));

        fs::remove_file(&old).expect("old fixture should be removed");
        fs::write(&replacement, b"replacement").expect("replacement fixture should be written");
        tree.begin_rescan(target.clone(), None)
            .expect("focused rescan should stage");
        add(&mut tree, &replacement);
        tree.complete_directory(&target, None)
            .expect("staged target should complete");
        tree.finish_rescan().expect("staged target should merge");

        assert_eq!(tree.current_id(), sibling_id);
        assert_eq!(node_id_at(&tree, &target), target_id);
        assert_eq!(node_id_at(&tree, &sibling), sibling_id);
        assert_eq!(node_id_at(&tree, &retained), retained_id);
        assert_ne!(tree.path_for_id(old_id), Some(old));
        assert_eq!(tree.node_kind(target_id), Some(NodeKind::Directory));
        assert_eq!(tree.total_node().metrics.apparent_bytes, 19);
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn completed_focused_rescan_ignores_late_primary_entries_below_its_target() {
        let root = tempfile::tempdir().expect("rescan root should exist");
        let target = root.path().join("target");
        let focused = target.join("focused");
        let late_primary = target.join("late-primary");
        fs::create_dir(&target).expect("target should be created");
        fs::write(&focused, b"focused").expect("focused fixture should be written");
        fs::write(&late_primary, b"late").expect("late primary fixture should be written");

        let mut tree = FileTree::new(
            root.path().to_path_buf(),
            true,
            crate::model::MIN_PROCESS_MIB,
        )
        .expect("file tree should be created");
        add(&mut tree, &target);
        tree.begin_rescan(target.clone(), None)
            .expect("focused rescan should stage");
        add(&mut tree, &focused);
        tree.complete_directory(&target, None)
            .expect("focused target should complete");
        tree.finish_rescan().expect("focused target should merge");

        let metadata =
            fs::symlink_metadata(&late_primary).expect("late primary metadata should be readable");
        let identity = identity_for(&late_primary, &metadata)
            .expect("late primary entry should be identifiable")
            .expect("late primary entry should not be a link");
        assert!(tree.primary_scan_path_is_stale(&late_primary));
        assert!(
            tree.add_primary_entry(&metadata, &late_primary, &identity)
                .expect("late primary entry should be ignored rather than overwrite focus")
                .is_none()
        );
        assert!(tree.arena.path_ids(&focused).is_some());
        assert!(tree.arena.path_ids(&late_primary).is_none());
    }

    #[test]
    fn path_filter_keeps_matching_ancestors_inside_its_filter_root() {
        let root = tempfile::tempdir().expect("filter root should exist");
        let scoped = root.path().join("scoped");
        let scoped_build = scoped.join("build");
        let scoped_object = scoped_build.join("main.o");
        let outside = root.path().join("outside");
        let outside_build = outside.join("build");
        let outside_object = outside_build.join("main.o");
        for path in [&scoped, &scoped_build, &outside, &outside_build] {
            fs::create_dir(path).expect("fixture directory should be created");
        }
        fs::write(&scoped_object, b"object").expect("scoped object should be written");
        fs::write(&outside_object, b"object").expect("outside object should be written");

        let mut tree = FileTree::new(
            root.path().to_path_buf(),
            true,
            crate::model::MIN_PROCESS_MIB,
        )
        .expect("file tree should be created");
        for path in [
            &scoped,
            &scoped_build,
            &scoped_object,
            &outside,
            &outside_build,
            &outside_object,
        ] {
            add(&mut tree, path);
        }
        for path in [
            &scoped_build,
            &scoped,
            &outside_build,
            &outside,
            root.path(),
        ] {
            tree.complete_directory(path, None)
                .expect("fixture directory should complete");
        }
        tree.finalize().expect("fixture tree should finalize");

        let scoped_id = node_id_at(&tree, &scoped);
        let scoped_build_id = node_id_at(&tree, &scoped_build);
        assert!(tree.enter_folder(scoped_id));
        tree.set_filter(Some(
            FilterPattern::new("build/*.o").expect("filter should compile"),
        ));
        assert!(tree.leave_folder());

        let root_files = tree.files_in_current_folder(0);
        assert!(
            root_files
                .iter()
                .any(|file| file.name == OsStr::new("scoped"))
        );
        assert!(
            !root_files
                .iter()
                .any(|file| file.name == OsStr::new("outside"))
        );
        assert!(tree.enter_folder(scoped_id));
        assert!(
            tree.files_in_current_folder(0)
                .iter()
                .any(|file| file.name == OsStr::new("build"))
        );
        assert!(tree.enter_folder(scoped_build_id));
        assert!(
            tree.files_in_current_folder(0)
                .iter()
                .any(|file| file.name == OsStr::new("main.o"))
        );
    }

    #[test]
    fn focused_rescan_reconciles_hard_links_across_the_target_boundary() {
        let root = tempfile::tempdir().expect("rescan root should exist");
        let target = root.path().join("target");
        let sibling = root.path().join("sibling");
        let inside = target.join("inside");
        let outside = sibling.join("outside");
        fs::create_dir(&target).expect("target should be created");
        fs::create_dir(&sibling).expect("sibling should be created");
        fs::write(&inside, b"payload").expect("inside fixture should be written");
        fs::hard_link(&inside, &outside).expect("hard link should be created");

        let mut tree = FileTree::new(
            root.path().to_path_buf(),
            false,
            crate::model::MIN_PROCESS_MIB,
        )
        .expect("file tree should be created");
        for path in [&target, &sibling, &inside, &outside] {
            add(&mut tree, path);
        }
        for path in [&target, &sibling, root.path()] {
            tree.complete_directory(path, None)
                .expect("fixture directory should complete");
        }
        tree.finalize().expect("fixture tree should finalize");
        let outside_id = node_id_at(&tree, &outside);
        let allocated_before = tree.total_node().metrics.allocated_bytes;

        tree.begin_rescan(target.clone(), None)
            .expect("focused rescan should stage");
        add(&mut tree, &inside);
        tree.complete_directory(&target, None)
            .expect("staged target should complete");
        tree.finish_rescan().expect("staged target should merge");

        assert_eq!(node_id_at(&tree, &outside), outside_id);
        assert_eq!(tree.identity_count(), 1);
        assert_eq!(tree.total_node().metrics.allocated_bytes, allocated_before);
    }
    #[cfg(unix)]
    #[test]
    fn replaced_root_directory_is_rejected_before_model_creation() {
        let parent = tempfile::tempdir().expect("scan parent should exist");
        let scan_root = parent.path().join("scan-root");
        let original = parent.path().join("original-root");
        fs::create_dir(&scan_root).expect("scan root should be created");
        let metadata = fs::symlink_metadata(&scan_root).expect("root metadata should exist");
        let identity = identity_for(&scan_root, &metadata)
            .expect("root identity should be readable")
            .expect("root should not be a symbolic link");
        fs::rename(&scan_root, &original).expect("original root should be displaced");
        fs::create_dir(&scan_root).expect("replacement root should be created");

        assert!(
            FileTree::new_with_root_identity(
                scan_root,
                identity,
                false,
                crate::model::MIN_PROCESS_MIB,
            )
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn replaced_root_symlink_is_rejected_before_model_creation() {
        use std::os::unix::fs::symlink;

        let parent = tempfile::tempdir().expect("scan parent should exist");
        let scan_root = parent.path().join("scan-root");
        let outside = parent.path().join("outside-root");
        fs::create_dir(&scan_root).expect("scan root should be created");
        fs::create_dir(&outside).expect("outside root should be created");
        let metadata = fs::symlink_metadata(&scan_root).expect("root metadata should exist");
        let identity = identity_for(&scan_root, &metadata)
            .expect("root identity should be readable")
            .expect("root should not be a symbolic link");
        fs::remove_dir(&scan_root).expect("original root should be removed");
        symlink(&outside, &scan_root).expect("replacement symlink should be created");

        assert!(
            FileTree::new_with_root_identity(
                scan_root,
                identity,
                false,
                crate::model::MIN_PROCESS_MIB,
            )
            .is_err()
        );
    }

    #[test]
    fn late_directory_completion_after_model_eviction_is_ignored() {
        let root = tempfile::tempdir().expect("scan root should exist");
        let evicted = root.path().join("evicted");
        fs::create_dir(&evicted).expect("fixture directory should be created");
        let metadata = fs::symlink_metadata(&evicted).expect("fixture metadata should exist");
        let identity = identity_for(&evicted, &metadata)
            .expect("fixture identity should be readable")
            .expect("fixture should have an identity");
        let mut tree = FileTree::new(
            root.path().to_path_buf(),
            false,
            crate::model::MIN_PROCESS_MIB,
        )
        .expect("file tree should be created");
        add(&mut tree, &evicted);
        assert!(
            tree.arena
                .try_remove_path(&evicted)
                .expect("fixture eviction should rebuild identities")
        );

        tree.complete_directory(&evicted, Some(&identity))
            .expect("late scanner completion below the scan root should be ignored");
        assert!(tree.arena.path_ids(&evicted).is_none());
    }

    #[test]
    fn directory_completion_outside_the_scan_root_remains_rejected() {
        let root = tempfile::tempdir().expect("scan root should exist");
        let outside = tempfile::tempdir().expect("outside directory should exist");
        let mut tree = FileTree::new(
            root.path().to_path_buf(),
            false,
            crate::model::MIN_PROCESS_MIB,
        )
        .expect("file tree should be created");

        assert!(matches!(
            tree.complete_directory(outside.path(), None),
            Err(crate::model::ModelError::InvalidPath(_))
        ));
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn identity_capacity_uncertainty_does_not_block_permanent_deletion() {
        let root = tempfile::tempdir().expect("scan root should exist");
        let target = root.path().join("target");
        fs::write(&target, b"target").expect("target should be created");
        let mut tree = FileTree::new(
            root.path().to_path_buf(),
            false,
            crate::model::MIN_PROCESS_MIB,
        )
        .expect("file tree should be created");
        add(&mut tree, &target);
        assert!(
            tree.arena
                .mark_path_uncertain(root.path(), UnscannedReason::IdentityStorageCapacity,)
        );

        let target = tree
            .deletion_target_for_path(&target)
            .expect("identity accounting uncertainty must not block live planning");
        let plan = build_plan(root.path(), target, false)
            .expect("planner should bind the visible file to its live identity");
        assert_eq!(plan.planned_entries(), 1);
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn backed_aggregate_deletes_without_materialization_or_stale_reinsertion() {
        let root = tempfile::tempdir().expect("scan root should exist");
        let compacted = root.path().join("compacted");
        let child = compacted.join("child");
        fs::create_dir(&compacted).expect("compacted directory should exist");
        fs::write(&child, b"payload").expect("compacted child should exist");

        let mut tree = FileTree::new(
            root.path().to_path_buf(),
            true,
            crate::model::MIN_PROCESS_MIB,
        )
        .expect("file tree should be created");
        add(&mut tree, &compacted);
        add(&mut tree, &child);
        for path in [&compacted, root.path()] {
            tree.complete_directory(path, None)
                .expect("fixture directory should complete");
        }
        tree.finalize().expect("fixture tree should finalize");
        let compacted_id = node_id_at(&tree, &compacted);
        let root_id = tree.arena.root();
        assert!(
            tree.arena
                .aggregate_cold_subtree(&HashSet::from([root_id]))
                .expect("directory should compact")
        );
        assert_eq!(
            tree.node_kind(compacted_id),
            Some(NodeKind::Synthetic(SyntheticKind::Aggregate))
        );
        let stale_metadata = fs::symlink_metadata(&compacted)
            .expect("compacted directory metadata should remain available");
        let stale_identity = identity_for(&compacted, &stale_metadata)
            .expect("compacted directory should be identifiable")
            .expect("compacted directory should not be a link");

        let target = tree
            .deletion_target_for_path(&compacted)
            .expect("aggregate should be immediately deletion-ready");
        assert_eq!(target.expected_snapshot.kind, NodeKind::Directory);
        let plan = build_plan(root.path(), target, false)
            .expect("planner should independently review aggregate contents");
        let report = execute_plan(
            root.path(),
            plan,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
        );
        tree.try_apply_deletion_report(&report)
            .expect("completed aggregate deletion should reconcile");
        assert!(!compacted.exists());
        assert!(tree.path_for_id(compacted_id).is_none());
        assert!(tree.primary_scan_path_is_stale(&compacted));
        assert!(tree.primary_scan_path_is_stale(&child));

        tree.add_entry(&stale_metadata, &compacted, &stale_identity)
            .expect("late primary scan entry should be safely ignored");
        assert!(tree.path_for_id(compacted_id).is_none());
    }

    #[test]
    fn deletion_eligibility_refuses_virtual_and_unverified_nodes() {
        let root = tempfile::tempdir().expect("scan root should exist");
        let compacted = root.path().join("compacted");
        let child = compacted.join("child");
        fs::create_dir(&compacted).expect("compacted directory should exist");
        fs::write(&child, b"payload").expect("compacted child should exist");

        let mut tree = FileTree::new(
            root.path().to_path_buf(),
            true,
            crate::model::MIN_PROCESS_MIB,
        )
        .expect("file tree should be created");
        add(&mut tree, &compacted);
        add(&mut tree, &child);
        for path in [&compacted, root.path()] {
            tree.complete_directory(path, None)
                .expect("fixture directory should complete");
        }
        tree.finalize().expect("fixture tree should finalize");
        let compacted_id = node_id_at(&tree, &compacted);
        let root_id = tree.arena.root();
        assert!(
            tree.arena
                .aggregate_cold_subtree(&HashSet::from([root_id]))
                .expect("directory should compact")
        );
        tree.arena
            .node_mut(compacted_id)
            .expect("aggregate should remain")
            .snapshot
            .identity = None;
        assert!(matches!(
            tree.deletion_target_for_path(&compacted),
            Err(crate::model::ModelError::Invariant(_))
        ));
        let summary_root = tempfile::tempdir().expect("summary root should exist");
        let omitted = summary_root.path().join("omitted");
        fs::write(&omitted, b"payload").expect("omitted entry should exist");
        let mut summary_tree = FileTree::new(
            summary_root.path().to_path_buf(),
            true,
            crate::model::MIN_PROCESS_MIB,
        )
        .expect("summary tree should be created");
        let metadata = fs::symlink_metadata(&omitted).expect("omitted metadata should exist");
        let identity = identity_for(&omitted, &metadata)
            .expect("omitted identity should be readable")
            .expect("omitted entry should not be a link");
        assert!(
            summary_tree
                .arena
                .add_entry_aggregated(&omitted, &metadata, identity)
                .expect("Other summary should be represented")
                .is_none()
        );
        summary_tree
            .complete_directory(summary_root.path(), None)
            .expect("summary root should complete");
        summary_tree
            .finalize()
            .expect("summary tree should finalize");
        let other = summary_tree
            .nodes()
            .find(|node| node.kind == NodeKind::Synthetic(SyntheticKind::Other))
            .and_then(|node| summary_tree.path_for_id(node.id))
            .expect("Other summary should remain");
        assert!(matches!(
            summary_tree.deletion_target_for_path(&other),
            Err(crate::model::ModelError::Invariant(_))
        ));
        assert!(matches!(
            summary_tree.deletion_target_for_path(summary_root.path()),
            Err(crate::model::ModelError::Invariant(_))
        ));
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn deletion_eligibility_refuses_shared_allocation_summary() {
        let root = tempfile::tempdir().expect("scan root should exist");
        let first = root.path().join("first");
        let second = root.path().join("second");
        fs::write(&first, b"payload").expect("first link should exist");
        fs::hard_link(&first, &second).expect("second hard link should exist");

        let mut tree = FileTree::new(
            root.path().to_path_buf(),
            false,
            crate::model::MIN_PROCESS_MIB,
        )
        .expect("file tree should be created");
        add(&mut tree, &first);
        add(&mut tree, &second);
        tree.complete_directory(root.path(), None)
            .expect("root should complete");
        tree.finalize().expect("tree should finalize");
        let shared = tree
            .nodes()
            .find(|node| node.kind == NodeKind::Synthetic(SyntheticKind::Shared))
            .and_then(|node| tree.path_for_id(node.id))
            .expect("shared summary should remain");

        assert!(matches!(
            tree.deletion_target_for_path(&shared),
            Err(crate::model::ModelError::Invariant(_))
        ));
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn stale_deletion_outcome_does_not_remove_a_replaced_model_entry() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let target = root.path().join("target");
        let displaced = root.path().join("displaced");
        fs::write(&target, b"old").expect("original target should exist");
        let mut tree = FileTree::new(
            root.path().to_path_buf(),
            true,
            crate::model::MIN_PROCESS_MIB,
        )
        .expect("file tree should be created");
        add(&mut tree, &target);
        tree.complete_directory(root.path(), None)
            .expect("root should complete");
        tree.finalize().expect("tree should finalize");
        let target_id = node_id_at(&tree, &target);
        let snapshot = tree
            .entry_snapshot(target_id)
            .expect("target snapshot should exist");
        let planned = PlannedEntry {
            relative_path: PathBuf::from("target"),
            snapshot: crate::deletion::PlannedSnapshot {
                identity: snapshot
                    .identity
                    .clone()
                    .expect("target should retain its identity"),
                kind: PlannedKind::File,
                apparent_bytes: snapshot.apparent_bytes,
                allocated_bytes: snapshot.allocated_bytes,
                modified_nanos: snapshot.modified_nanos,
            },
        };
        fs::rename(&target, &displaced).expect("original target should be displaced");
        fs::write(&target, b"new").expect("replacement target should exist");
        add(&mut tree, &target);
        let report = DeletionReport {
            target_node_id: target_id,
            root_relative_path: PathBuf::from("target"),
            scan_root: root.path().to_path_buf(),
            entries: vec![DeletionEntryResult {
                entry: planned,
                outcome: DeletionEntryOutcome::Deleted,
            }]
            .into(),
            soft_cancelled: false,
            precise: true,
            estimated_bytes: 0,
        };

        tree.try_apply_deletion_report(&report)
            .expect("stale outcome should be reconciled conservatively");

        assert_eq!(tree.path_for_id(target_id), Some(target));
        assert_eq!(tree.node_state(target_id), Some(NodeState::Uncertain));
        assert_eq!(tree.total_node().state, NodeState::Uncertain);
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn deletion_report_never_removes_a_path_outside_its_target() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let target = root.path().join("target");
        let sibling = root.path().join("sibling");
        fs::write(&target, b"target").expect("target should exist");
        fs::write(&sibling, b"sibling").expect("sibling should exist");

        let mut tree = FileTree::new(
            root.path().to_path_buf(),
            true,
            crate::model::MIN_PROCESS_MIB,
        )
        .expect("file tree should be created");
        add(&mut tree, &target);
        add(&mut tree, &sibling);
        tree.complete_directory(root.path(), None)
            .expect("root should complete");
        tree.finalize().expect("tree should finalize");
        let target_id = node_id_at(&tree, &target);
        let sibling_id = node_id_at(&tree, &sibling);
        let sibling_snapshot = tree
            .entry_snapshot(sibling_id)
            .expect("sibling snapshot should exist");
        let report = DeletionReport {
            target_node_id: target_id,
            root_relative_path: PathBuf::from("target"),
            scan_root: root.path().to_path_buf(),
            entries: vec![DeletionEntryResult {
                entry: PlannedEntry {
                    relative_path: PathBuf::from("sibling"),
                    snapshot: crate::deletion::PlannedSnapshot {
                        identity: sibling_snapshot
                            .identity
                            .expect("sibling should retain its identity"),
                        kind: PlannedKind::File,
                        apparent_bytes: sibling_snapshot.apparent_bytes,
                        allocated_bytes: sibling_snapshot.allocated_bytes,
                        modified_nanos: sibling_snapshot.modified_nanos,
                    },
                },
                outcome: DeletionEntryOutcome::Deleted,
            }]
            .into(),
            soft_cancelled: false,
            precise: true,
            estimated_bytes: 0,
        };

        tree.try_apply_deletion_report(&report)
            .expect("invalid report path should be contained conservatively");

        assert_eq!(tree.path_for_id(target_id), Some(target));
        assert_eq!(tree.path_for_id(sibling_id), Some(sibling));
        assert_eq!(tree.node_state(target_id), Some(NodeState::Uncertain));
        assert_eq!(tree.total_node().state, NodeState::Uncertain);
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn partial_hard_link_deletion_rebuilds_identity_metrics() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let first = root.path().join("first");
        let second = root.path().join("second");
        fs::write(&first, b"payload").expect("hard-link source should be written");
        fs::hard_link(&first, &second).expect("hard link should be created");

        let mut tree = FileTree::new(
            root.path().to_path_buf(),
            false,
            crate::model::MIN_PROCESS_MIB,
        )
        .expect("file tree should be created");
        add(&mut tree, &first);
        add(&mut tree, &second);
        tree.complete_directory(root.path(), None)
            .expect("root should complete");
        tree.finalize().expect("tree should finalize");
        let first_id = node_id_at(&tree, &first);
        let snapshot = tree
            .entry_snapshot(first_id)
            .expect("first snapshot should exist");
        let target = FileToDelete {
            node_id: first_id,
            synthetic: false,
            path_in_filesystem: root.path().to_path_buf(),
            path_to_file: vec![OsString::from("first")],
            file_type: FileType::File,
            num_descendants: None,
            size: snapshot.apparent_bytes,
            expected_snapshot: snapshot.clone(),
            reviewed_entries: tree
                .reviewed_subtree(first_id, 1 << 20)
                .expect("first subtree should be reviewable"),
        };
        let plan = build_plan(root.path(), target, false).expect("deletion plan should build");
        let report = execute_plan(
            root.path(),
            plan,
            &std::sync::atomic::AtomicBool::new(false),
            &std::sync::atomic::AtomicBool::new(false),
        );
        assert_eq!(report.deleted_entries(), 1);
        assert_eq!(report.deleted_allocated_bytes(), 0);
        tree.apply_deletion_report(&report);

        let second_id = node_id_at(&tree, &second);
        let second_node = tree.node(second_id).expect("remaining link should exist");
        let allocated = snapshot
            .allocated_bytes
            .map_or(ByteBounds::unknown(), ByteBounds::exact);
        assert_eq!(second_node.metrics.allocated_bytes, allocated);
        assert_eq!(
            second_node
                .snapshot
                .identity
                .as_ref()
                .and_then(|identity| identity.link_count),
            Some(1)
        );
        assert_eq!(second_node.metrics.reclaimable_bytes, allocated);
        assert!(
            tree.nodes()
                .all(|node| { node.kind != NodeKind::Synthetic(SyntheticKind::Shared) })
        );
        assert_eq!(tree.identity_count(), 1);
        assert_eq!(tree.space_freed, 0);
    }
    #[cfg(any(unix, windows))]
    #[test]
    fn missing_hard_link_refreshes_survivor_metadata() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let first = root.path().join("first");
        let second = root.path().join("second");
        fs::write(&first, b"payload").expect("hard-link source should be written");
        fs::hard_link(&first, &second).expect("hard link should be created");

        let mut tree = FileTree::new(
            root.path().to_path_buf(),
            false,
            crate::model::MIN_PROCESS_MIB,
        )
        .expect("file tree should be created");
        add(&mut tree, &first);
        add(&mut tree, &second);
        tree.complete_directory(root.path(), None)
            .expect("root should complete");
        tree.finalize().expect("tree should finalize");
        let first_id = node_id_at(&tree, &first);
        let snapshot = tree
            .entry_snapshot(first_id)
            .expect("first snapshot should exist");
        let target = FileToDelete {
            node_id: first_id,
            synthetic: false,
            path_in_filesystem: root.path().to_path_buf(),
            path_to_file: vec![OsString::from("first")],
            file_type: FileType::File,
            num_descendants: None,
            size: snapshot.apparent_bytes,
            expected_snapshot: snapshot.clone(),
            reviewed_entries: tree
                .reviewed_subtree(first_id, 1 << 20)
                .expect("first subtree should be reviewable"),
        };
        let plan = build_plan(root.path(), target, false).expect("deletion plan should build");
        let entry = PlannedEntry {
            relative_path: std::path::PathBuf::from("first"),
            snapshot: plan.root_snapshot().clone(),
        };
        let report = DeletionReport {
            target_node_id: first_id,
            root_relative_path: std::path::PathBuf::from("first"),
            scan_root: root.path().to_path_buf(),
            entries: vec![DeletionEntryResult {
                entry,
                outcome: DeletionEntryOutcome::Missing,
            }]
            .into(),
            soft_cancelled: false,
            precise: true,
            estimated_bytes: plan.estimated_bytes,
        };
        fs::remove_file(&first).expect("planned path should become missing");
        assert_eq!(report.missing_entries(), 1);
        tree.apply_deletion_report(&report);

        let second_id = node_id_at(&tree, &second);
        let second_node = tree.node(second_id).expect("remaining link should exist");
        let allocated = snapshot
            .allocated_bytes
            .map_or(ByteBounds::unknown(), ByteBounds::exact);
        assert_eq!(second_node.metrics.allocated_bytes, allocated);
        assert_eq!(
            second_node
                .snapshot
                .identity
                .as_ref()
                .and_then(|identity| identity.link_count),
            Some(1)
        );
        assert_eq!(second_node.metrics.reclaimable_bytes, allocated);
        assert_eq!(tree.identity_count(), 1);
        assert_eq!(tree.space_freed, 0);
    }
    #[cfg(any(unix, windows))]
    #[test]
    fn deletion_reconciliation_restores_a_valid_current_folder() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let parent = root.path().join("parent");
        let child = parent.join("child");
        let sibling = root.path().join("sibling.log");
        fs::create_dir(&parent).expect("parent should be created");
        fs::write(&child, b"payload").expect("child should be created");
        fs::write(&sibling, b"survivor").expect("sibling should be created");

        let mut tree = FileTree::new(
            root.path().to_path_buf(),
            false,
            crate::model::MIN_PROCESS_MIB,
        )
        .expect("tree should be created");
        add(&mut tree, &parent);
        add(&mut tree, &child);
        add(&mut tree, &sibling);
        tree.complete_directory(&parent, None)
            .expect("parent should complete");
        tree.complete_directory(root.path(), None)
            .expect("root should complete");
        tree.finalize().expect("tree should finalize");
        let parent_id = node_id_at(&tree, &parent);
        assert!(tree.enter_folder(parent_id));
        tree.set_filter(Some(
            FilterPattern::new("*.log").expect("filter should compile"),
        ));

        let target = tree
            .deletion_target_for_path(&parent)
            .expect("complete directory should be eligible");
        let plan = build_plan(root.path(), target, false).expect("plan should build");
        let report = execute_plan(
            root.path(),
            plan,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
        );
        tree.try_apply_deletion_report(&report)
            .expect("report should reconcile");

        assert_eq!(tree.get_current_path(), root.path());
        assert_eq!(tree.current_id(), tree.arena.root());
        assert!(tree.path_for_id(parent_id).is_none());
        assert!(
            tree.files_in_current_folder(0)
                .iter()
                .any(|file| file.name == OsStr::new("sibling.log"))
        );
    }
}
