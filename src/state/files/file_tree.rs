use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::Metadata;
use std::path::{Path, PathBuf};

use crate::deletion::{
    DeletionEntryOutcome, DeletionReport, PlannedKind, PlannedSnapshot, ReviewedEntry,
    validate_scan_root_identity,
};
use crate::filter::FilterPattern;
use crate::model::{
    Arena, MemoryBudget, ModelError, Node, NodeId, NodeKind, NodeState, SyntheticKind,
    UnscannedReason,
};
use crate::native_path::NativeIdentity;
use crate::state::tiles::{FileMetadata, files_in_folder};
use file_id::FileId;

struct RescanStage {
    target_id: NodeId,
    arena: Arena,
    filter: Option<FilterPattern>,
    filter_root: Option<PathBuf>,
}

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
}

impl FileTree {
    pub fn new(
        path_in_filesystem: PathBuf,
        show_apparent_size: bool,
        process_memory_mib: usize,
    ) -> Result<Self, ModelError> {
        let budget = MemoryBudget::from_mib(process_memory_mib)?;
        let arena = Arena::new(path_in_filesystem.clone(), budget)?;
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
        })
    }

    pub fn new_with_root_identity(
        path_in_filesystem: PathBuf,
        root_identity: NativeIdentity,
        show_apparent_size: bool,
        process_memory_mib: usize,
    ) -> Result<Self, ModelError> {
        validate_scan_root_identity(&path_in_filesystem, &root_identity)
            .map_err(|error| ModelError::Invariant(error.to_string()))?;
        let mut tree = Self::new(path_in_filesystem, show_apparent_size, process_memory_mib)?;
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
    pub fn deletion_target_for_path(
        &self,
        path: &Path,
    ) -> Result<crate::state::FileToDelete, ModelError> {
        let ids = self
            .arena
            .path_ids(path)
            .ok_or_else(|| ModelError::InvalidPath(path.to_string_lossy().into_owned()))?;
        let node_id = *ids
            .last()
            .ok_or_else(|| ModelError::InvalidPath(path.to_string_lossy().into_owned()))?;
        if ids.iter().any(|id| {
            self.arena
                .node(*id)
                .is_none_or(|node| node.state != NodeState::Complete)
        }) {
            return Err(ModelError::Invariant(
                "deletion requires a fully materialized path".to_string(),
            ));
        }
        let node = self
            .arena
            .node(node_id)
            .ok_or_else(|| ModelError::InvalidPath(path.to_string_lossy().into_owned()))?;
        if node.kind.is_synthetic() {
            return Err(ModelError::Invariant(
                "deletion requires a fully materialized subtree".to_string(),
            ));
        }
        let relative = path
            .strip_prefix(&self.path_in_filesystem)
            .map_err(|_| ModelError::InvalidPath(path.to_string_lossy().into_owned()))?;
        let (file_type, num_descendants) = match node.kind {
            NodeKind::Directory => (
                crate::state::tiles::FileType::Folder,
                Some(node.metrics.descendants),
            ),
            NodeKind::File | NodeKind::Link => (crate::state::tiles::FileType::File, None),
            NodeKind::Root | NodeKind::Synthetic(_) => {
                return Err(ModelError::Invariant(
                    "scan roots and synthetic nodes cannot be deleted".to_string(),
                ));
            }
        };
        Ok(crate::state::FileToDelete {
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
            expected_snapshot: node.snapshot.clone(),
            reviewed_entries: Vec::new(),
        })
    }

    /// Returns a human-readable ineligibility reason if any node in the
    /// subtree rooted at `root` cannot currently be reviewed for deletion,
    /// or `Ok(())` if the subtree is fully eligible.
    ///
    /// Call this before [`Self::reviewed_subtree`] to surface actionable
    /// messages rather than internal invariant errors.
    pub fn subtree_deletion_eligibility(&self, root: NodeId) -> Result<(), &'static str> {
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            let Some(node) = self.arena.node(id) else {
                continue;
            };
            // Shared entries are excluded by the deletion review and worker; skip them.
            if node.kind == NodeKind::Synthetic(SyntheticKind::Shared) {
                continue;
            }
            match node.state {
                NodeState::Scanning => return Err("still scanning"),
                NodeState::Aggregated => return Err("aggregated"),
                NodeState::Uncertain => return Err("uncertain"),
                NodeState::Complete => {}
            }
            if node.kind.is_synthetic() {
                return Err("aggregated");
            }
            stack.extend(node.children.iter().copied());
        }
        Ok(())
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
        if self
            .arena
            .node(id)
            .is_some_and(|node| node.kind.is_directory())
        {
            self.current_path.push(id);
            self.arena.touch(id);
            true
        } else {
            false
        }
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

    pub fn try_apply_deletion_report(&mut self, report: &DeletionReport) -> Result<(), ModelError> {
        let mut affected_link_counts: HashMap<FileId, Option<u64>> = HashMap::new();
        for result in &report.entries {
            if !matches!(
                result.outcome,
                DeletionEntryOutcome::Deleted | DeletionEntryOutcome::Missing
            ) || !matches!(
                result.entry.snapshot.kind,
                PlannedKind::File | PlannedKind::Link
            ) {
                continue;
            }
            let file_id = result.entry.snapshot.identity.file_id;
            let post_delete = matches!(result.outcome, DeletionEntryOutcome::Deleted)
                .then(|| result.entry.snapshot.identity.link_count)
                .flatten()
                .map(|count| count.saturating_sub(1));
            affected_link_counts
                .entry(file_id)
                .and_modify(|current| {
                    *current = match (*current, post_delete) {
                        (Some(_), Some(next)) => Some(next),
                        _ => None,
                    };
                })
                .or_insert(post_delete);
        }
        let removed_paths = report
            .entries
            .iter()
            .filter(|result| {
                matches!(
                    result.outcome,
                    DeletionEntryOutcome::Deleted | DeletionEntryOutcome::Missing
                )
            })
            .map(|result| self.path_in_filesystem.join(&result.entry.relative_path))
            .collect::<Vec<_>>();
        self.arena
            .try_remove_paths_with_link_counts(&removed_paths, &affected_link_counts)?;
        if report.changed_entries() > 0
            || report.failed_entries() > 0
            || report.unattempted_entries() > 0
        {
            let root = self.path_in_filesystem.join(&report.root_relative_path);
            self.arena.mark_path_uncertain(
                &root,
                UnscannedReason::Metadata("deletion result requires a focused rescan".to_string()),
            );
        }
        let freed = if self.show_apparent_size {
            report.deleted_apparent_bytes()
        } else {
            report.deleted_allocated_bytes()
        };
        self.space_freed = self.space_freed.saturating_add(freed);
        Ok(())
    }

    #[allow(clippy::needless_pass_by_value)]
    pub fn add_entry(
        &mut self,
        entry_metadata: &Metadata,
        entry_full_path: &Path,
        identity: NativeIdentity,
    ) -> Result<Option<NodeId>, ModelError> {
        if let Some(stage) = self.rescan.as_mut() {
            let pinned = HashSet::from([stage.arena.root()]);
            return add_entry_to(
                &mut stage.arena,
                stage.filter.as_ref(),
                stage.filter_root.as_deref(),
                &pinned,
                entry_metadata,
                entry_full_path,
                &identity,
            );
        }
        let pinned = self.pinned_nodes();
        add_entry_to(
            &mut self.arena,
            self.filter.as_ref(),
            self.filter_root.as_deref(),
            &pinned,
            entry_metadata,
            entry_full_path,
            &identity,
        )
    }

    #[allow(clippy::needless_pass_by_value)]
    pub fn record_unscanned(
        &mut self,
        path: &Path,
        reason: UnscannedReason,
    ) -> Result<(), ModelError> {
        if let Some(stage) = self.rescan.as_mut() {
            let pinned = HashSet::from([stage.arena.root()]);
            return record_unscanned_to(&mut stage.arena, &pinned, path, &reason);
        }
        let pinned = self.pinned_nodes();
        record_unscanned_to(&mut self.arena, &pinned, path, &reason)
    }

    pub fn complete_directory(
        &mut self,
        path: &Path,
        expected_identity: Option<&NativeIdentity>,
    ) -> Result<(), ModelError> {
        if let Some(stage) = self.rescan.as_mut() {
            stage.arena.complete_directory(path, expected_identity)
        } else {
            self.arena.complete_directory(path, expected_identity)
        }
    }

    pub fn finalize(&mut self) -> Result<(), ModelError> {
        if self.rescan.is_some() {
            return Err(ModelError::Invariant(
                "cannot finalize while a focused rescan is staged".to_string(),
            ));
        }
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

    pub fn begin_rescan(
        &mut self,
        target: PathBuf,
        filter: Option<FilterPattern>,
    ) -> Result<(), ModelError> {
        if self.rescan.is_some() {
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
        let remaining = self
            .arena
            .memory_limit()
            .saturating_sub(self.arena.memory_used());
        let budget = MemoryBudget::from_model_limit(remaining)?;
        self.rescan = Some(RescanStage {
            target_id,
            arena: Arena::new(target, budget)?,
            filter,
            filter_root,
        });
        Ok(())
    }

    pub fn finish_rescan(&mut self) -> Result<(), ModelError> {
        let previous_path = self.get_current_path();
        let stage = self
            .rescan
            .take()
            .ok_or_else(|| ModelError::Invariant("focused rescan is not active".to_string()))?;
        self.arena
            .replace_subtree_from(stage.target_id, stage.arena)?;
        self.filter = stage.filter;
        self.filter_root = stage.filter_root;
        self.restore_navigation(&previous_path);
        self.failed_to_read = self.metadata_failure_count();
        Ok(())
    }

    pub fn cancel_rescan(&mut self) -> Result<(), ModelError> {
        if self.rescan.take().is_none() {
            return Err(ModelError::Invariant(
                "focused rescan is not active".to_string(),
            ));
        }
        Ok(())
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
        self.rescan.as_ref().map_or_else(
            || self.arena.identity_count(),
            |stage| stage.arena.identity_count(),
        )
    }

    pub fn increment_failed_to_read(&mut self) {
        if self.rescan.is_none() {
            self.failed_to_read = self.failed_to_read.saturating_add(1);
        }
    }

    fn pinned_nodes(&self) -> HashSet<NodeId> {
        let mut pinned = self.current_path.iter().copied().collect::<HashSet<_>>();
        if let Some(root) = self.filter_root.as_ref()
            && let Some(ids) = self.arena.path_ids(root)
        {
            pinned.extend(ids);
        }
        pinned
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
    pinned: &HashSet<NodeId>,
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
                if !arena.aggregate_cold_subtree(pinned)? {
                    return Err(error);
                }
            }
            result => return result,
        }
    }
}

fn record_unscanned_to(
    arena: &mut Arena,
    pinned: &HashSet<NodeId>,
    path: &Path,
    reason: &UnscannedReason,
) -> Result<(), ModelError> {
    loop {
        match arena.record_unscanned(path, reason.clone()) {
            Err(error @ ModelError::MemoryExhausted { .. }) => {
                if !arena.aggregate_cold_subtree(pinned)? {
                    return Err(error);
                }
            }
            result => return result,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    #[cfg(any(unix, windows))]
    use std::ffi::OsString;
    use std::fs;

    #[cfg(any(unix, windows))]
    use crate::deletion::{
        DeletionEntryOutcome, DeletionEntryResult, DeletionReport, build_plan, execute_plan,
    };
    #[cfg(any(unix, windows))]
    use crate::model::ByteBounds;
    use crate::model::{NodeKind, SyntheticKind};
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
        tree.add_entry(&metadata, path, identity)
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
    }
    #[test]
    fn focused_rescan_staging_uses_only_remaining_live_model_budget() {
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
            .expect("fixture should consume its model budget");

        let error = tree
            .begin_rescan(target, None)
            .expect_err("staging should not receive a second full model budget");

        assert!(matches!(
            error,
            crate::model::ModelError::MemoryExhausted { .. }
        ));
        assert_eq!(model_state(&tree), before);
        assert!(tree.rescan.is_none());
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
        let entry = plan
            .entries
            .first()
            .cloned()
            .expect("deletion plan should contain the target");
        let report = DeletionReport {
            target_node_id: first_id,
            root_relative_path: std::path::PathBuf::from("first"),
            scan_root: root.path().to_path_buf(),
            entries: vec![DeletionEntryResult {
                entry,
                outcome: DeletionEntryOutcome::Missing,
            }],
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
}
