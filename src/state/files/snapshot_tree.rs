use std::ffi::OsStr;
use std::fs;
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use crate::filter::FilterPattern;
use crate::model::{
    EntrySnapshot, ModelError, Node, NodeId, NodeKind, NodeMetrics, NodeState, SyntheticKind,
    UnscannedReason,
};

use crate::native_path::identity_for;
use crate::os::physical_size;
use crate::scan_coordinator::{RelativePath, ScanGeneration};
use crate::scan_store::page::{
    PageCursor, PageEntryKind, ScanPage, ScanPageEntry, SharedAllocationSummary,
};
use crate::scan_store::path_reducer::{Coverage, SummaryMetrics};
use crate::state::FileToDelete;
use crate::state::files::tree_view::TreeView;
use crate::state::tiles::files_in_folder::normalize_file_metadata;
use crate::state::tiles::{FileMetadata, FileType};

/// A small materialized view of one immutable `ScanStore` page.
///
/// It owns only one folder's direct entries and its ancestor chain. The
/// underlying generation remains in the `ScanStore`; a bounded page cache may
/// retain this view for quick backtracking without building a full memory tree.
pub(crate) struct SnapshotTree {
    root_path: PathBuf,
    generation: ScanGeneration,
    current_relative: RelativePath,
    current_id: NodeId,
    nodes: Vec<Node>,
    relative_paths: Vec<Option<RelativePath>>,
    page_memory_limit: usize,
    scan_store_stats: (u64, u64),
    retained_bytes: usize,
    next_after: Option<PageCursor>,
    unrecorded_path_count: u64,
    filter: Option<SnapshotFilter>,
    provisional: bool,
}

#[derive(Clone)]
struct SnapshotFilter {
    pattern: FilterPattern,
}

struct SnapshotNode {
    name: Arc<OsStr>,
    kind: NodeKind,
    metrics: SummaryMetrics,
    coverage: Coverage,
    relative: Option<RelativePath>,
    scan_snapshot: Option<EntrySnapshot>,
    use_live_snapshot: bool,
}

struct ConcreteDeletionTarget {
    root_path: PathBuf,
    node_id: NodeId,
    relative: RelativePath,
    expected_kind: NodeKind,
    file_type: FileType,
    metrics: SummaryMetrics,
    expected_snapshot: Option<EntrySnapshot>,
}

impl SnapshotTree {
    /// # Errors
    ///
    /// Returns an error when the bounded page cannot fit the configured model
    /// budget or cannot be assigned node IDs.
    pub(crate) fn from_page(
        root_path: PathBuf,
        page: ScanPage,
        page_memory_limit: usize,
        scan_store_stats: (u64, u64),
    ) -> Result<Self, ModelError> {
        Self::from_page_with_source(
            root_path,
            page,
            page_memory_limit,
            scan_store_stats,
            None,
            false,
        )
    }

    /// Builds one incomplete bounded page while its canonical generation is still scanning.
    pub(crate) fn from_provisional_page(
        root_path: PathBuf,
        page: ScanPage,
        page_memory_limit: usize,
        scan_store_stats: (u64, u64),
    ) -> Result<Self, ModelError> {
        Self::from_page_with_source(
            root_path,
            page,
            page_memory_limit,
            scan_store_stats,
            None,
            true,
        )
    }

    /// Builds the bounded root-only view shown before an active scan has emitted
    /// any concrete page entries.
    pub(crate) fn loading(
        root_path: PathBuf,
        generation: ScanGeneration,
        page_memory_limit: usize,
        scan_store_stats: (u64, u64),
    ) -> Result<Self, ModelError> {
        Self::from_page_with_source(
            root_path,
            ScanPage {
                generation,
                folder: RelativePath::root(),
                folder_metrics: SummaryMetrics::default(),
                folder_coverage: Coverage::Uncertain,
                root_metrics: SummaryMetrics::default(),
                root_coverage: Coverage::Uncertain,
                entries: Vec::new(),
                next_after: None,
                shared_allocation: None,
                unrecorded_path_count: 0,
            },
            page_memory_limit,
            scan_store_stats,
            None,
            true,
        )
    }

    /// Builds one bounded page while retaining the active canonical filter.
    pub(crate) fn from_page_with_filter(
        root_path: PathBuf,
        page: ScanPage,
        page_memory_limit: usize,
        scan_store_stats: (u64, u64),
        filter: Option<(FilterPattern, RelativePath)>,
    ) -> Result<Self, ModelError> {
        Self::from_page_with_source(
            root_path,
            page,
            page_memory_limit,
            scan_store_stats,
            filter,
            false,
        )
    }

    fn from_page_with_source(
        root_path: PathBuf,
        page: ScanPage,
        page_memory_limit: usize,
        scan_store_stats: (u64, u64),
        filter: Option<(FilterPattern, RelativePath)>,
        provisional: bool,
    ) -> Result<Self, ModelError> {
        let retained_bytes = page_retained_bytes(&page);
        ensure_page_fits(retained_bytes, page_memory_limit)?;
        let ScanPage {
            generation,
            folder,
            folder_metrics,
            folder_coverage,
            root_metrics,
            root_coverage,
            entries,
            next_after,
            shared_allocation,
            unrecorded_path_count,
        } = page;
        let mut root = Node::new(
            NodeId(0),
            None,
            Arc::from(OsStr::new("")),
            NodeKind::Root,
            if provisional {
                NodeState::Scanning
            } else {
                state_for(root_coverage)
            },
            snapshot_for_path(
                &root_path,
                &RelativePath::root(),
                NodeKind::Root,
                root_metrics,
            ),
        );
        root.metrics = node_metrics(root_metrics);
        root.unscanned_reason = (!provisional).then(|| reason_for(root_coverage)).flatten();
        let mut tree = Self {
            root_path,
            generation,
            current_relative: folder.clone(),
            current_id: NodeId(0),
            nodes: vec![root],
            relative_paths: vec![Some(RelativePath::root())],
            page_memory_limit,
            scan_store_stats,
            retained_bytes,
            next_after,
            unrecorded_path_count,
            filter: filter.map(|(pattern, _)| SnapshotFilter { pattern }),
            provisional,
        };
        let current_id = tree.append_ancestors(&folder, folder_metrics, folder_coverage)?;
        tree.current_id = current_id;
        tree.append_page_entries(entries, shared_allocation)?;
        Ok(tree)
    }

    fn append_ancestors(
        &mut self,
        folder: &RelativePath,
        folder_metrics: SummaryMetrics,
        folder_coverage: Coverage,
    ) -> Result<NodeId, ModelError> {
        let mut parent = NodeId(0);
        for (index, component) in folder.components().iter().enumerate() {
            let relative = RelativePath::from_components(
                folder.components()[..index.saturating_add(1)].to_vec(),
            )
            .map_err(|_| {
                ModelError::Invariant("scan page contained an invalid ancestor".to_string())
            })?;
            let is_current = index.saturating_add(1) == folder.depth();
            parent = self.push_node(
                parent,
                SnapshotNode {
                    name: Arc::from(component.as_os_str()),
                    kind: NodeKind::Directory,
                    metrics: if is_current {
                        folder_metrics
                    } else {
                        SummaryMetrics::default()
                    },
                    coverage: if is_current {
                        folder_coverage
                    } else {
                        Coverage::Uncertain
                    },
                    relative: Some(relative),
                    scan_snapshot: None,
                    use_live_snapshot: true,
                },
            )?;
        }
        Ok(parent)
    }

    fn append_page_entries(
        &mut self,
        entries: Vec<ScanPageEntry>,
        shared_allocation: Option<SharedAllocationSummary>,
    ) -> Result<(), ModelError> {
        for ScanPageEntry {
            path,
            kind,
            metrics,
            coverage,
            snapshot,
        } in entries
        {
            let name = path
                .components()
                .last()
                .ok_or_else(|| ModelError::Invariant("scan page entry had no name".to_string()))?;
            self.push_node(
                self.current_id,
                SnapshotNode {
                    name: Arc::from(name.as_os_str()),
                    kind: node_kind_for(kind),
                    metrics,
                    coverage,
                    relative: Some(path),
                    scan_snapshot: snapshot,
                    use_live_snapshot: false,
                },
            )?;
        }
        if let Some(shared) = shared_allocation {
            self.push_node(
                self.current_id,
                SnapshotNode {
                    name: Arc::from(OsStr::new("Shared allocation")),
                    kind: NodeKind::Synthetic(SyntheticKind::Shared),
                    metrics: shared.metrics,
                    coverage: shared.coverage,
                    relative: None,
                    scan_snapshot: None,
                    use_live_snapshot: false,
                },
            )?;
        }
        Ok(())
    }

    pub(crate) fn next_after(&self) -> Option<&PageCursor> {
        self.next_after.as_ref()
    }

    #[must_use]
    pub(crate) const fn current_id(&self) -> NodeId {
        self.current_id
    }

    #[must_use]
    pub(crate) fn current_relative(&self) -> &RelativePath {
        &self.current_relative
    }

    #[must_use]
    pub(crate) const fn generation(&self) -> ScanGeneration {
        self.generation
    }

    #[must_use]
    pub(crate) const fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }

    #[must_use]
    pub(crate) const fn page_cache_budget_bytes(&self) -> usize {
        self.page_memory_limit / 4
    }
    #[must_use]
    pub(crate) fn filter_raw(&self) -> Option<&str> {
        self.filter.as_ref().map(|filter| filter.pattern.raw())
    }

    #[must_use]
    pub(crate) fn path_for_id(&self, id: NodeId) -> Option<PathBuf> {
        self.relative_paths
            .get(id.index())
            .and_then(Option::as_ref)
            .map(|relative| self.root_path.join(relative.to_path_buf()))
    }

    #[must_use]
    pub(crate) fn relative_for_id(&self, id: NodeId) -> Option<&RelativePath> {
        self.relative_paths.get(id.index()).and_then(Option::as_ref)
    }

    #[must_use]
    pub(crate) fn id_for_relative(&self, relative: &RelativePath) -> Option<NodeId> {
        self.relative_paths
            .iter()
            .position(|candidate| candidate.as_ref() == Some(relative))
            .and_then(|index| u32::try_from(index).ok())
            .map(NodeId)
    }

    #[must_use]
    pub(crate) fn selected_folder(&self, id: NodeId) -> Option<RelativePath> {
        self.nodes
            .get(id.index())
            .filter(|node| node.kind == NodeKind::Directory)
            .and_then(|_| self.relative_for_id(id).cloned())
    }

    #[must_use]
    pub(crate) fn parent_folder(&self) -> Option<RelativePath> {
        (!self.current_relative.is_root()).then(|| {
            RelativePath::from_components(
                self.current_relative.components()
                    [..self.current_relative.depth().saturating_sub(1)]
                    .to_vec(),
            )
            .expect("a prefix of a valid snapshot path remains valid")
        })
    }

    #[must_use]
    pub(crate) fn files_in_current_folder(
        &self,
        offset: usize,
        show_apparent_size: bool,
    ) -> Vec<FileMetadata> {
        let files = self
            .current_node()
            .children
            .iter()
            .filter_map(|id| self.node(*id))
            .map(|node| {
                let size = if show_apparent_size {
                    node.metrics.apparent_bytes
                } else {
                    node.metrics.allocated_bytes.lower
                };
                let (descendants, file_type, synthetic_kind) = match node.kind {
                    NodeKind::Directory => (Some(node.metrics.descendants), FileType::Folder, None),
                    NodeKind::File | NodeKind::Link => (None, FileType::File, None),
                    NodeKind::Synthetic(SyntheticKind::Shared) => {
                        (Some(0), FileType::Synthetic, Some(SyntheticKind::Shared))
                    }
                    NodeKind::Root => {
                        unreachable!(
                            "snapshot pages contain only direct concrete entries or shared totals"
                        )
                    }
                };
                FileMetadata {
                    node_id: node.id,
                    name: node.name.to_os_string(),
                    size,
                    apparent_size: node.metrics.apparent_bytes,
                    descendants,
                    percentage: 0.0,
                    file_type,
                    synthetic_kind,
                    uncertain: node.state == NodeState::Uncertain
                        || (!show_apparent_size && node.metrics.allocated_bytes.upper.is_none()),
                }
            })
            .collect::<Vec<_>>();
        normalize_file_metadata(files, offset)
    }

    /// Builds an identity-bound deletion target from one immutable canonical entry.
    ///
    /// The planner revalidates this scan-time snapshot immediately before every
    /// filesystem mutation.
    ///
    /// # Errors
    ///
    /// Returns an error when the canonical entry lacks the stable scan-time
    /// identity required for safe deletion.
    pub(crate) fn deletion_target_from_entry(
        root_path: PathBuf,
        node_id: NodeId,
        relative: &RelativePath,
        entry: ScanPageEntry,
        show_apparent_size: bool,
    ) -> Result<FileToDelete, ModelError> {
        let (expected_kind, file_type) = match entry.kind {
            PageEntryKind::Directory => (NodeKind::Directory, FileType::Folder),
            PageEntryKind::File => (NodeKind::File, FileType::File),
            PageEntryKind::Link => (NodeKind::Link, FileType::File),
        };
        Self::concrete_deletion_target(
            ConcreteDeletionTarget {
                root_path,
                node_id,
                relative: relative.clone(),
                expected_kind,
                file_type,
                metrics: entry.metrics,
                expected_snapshot: entry.snapshot,
            },
            show_apparent_size,
        )
    }

    /// Builds a deletion target only for a directly observed item on the active
    /// provisional page. Shared summaries and inferred preview nodes are deliberately rejected.
    pub(crate) fn deletion_target_from_preview(
        &self,
        node_id: NodeId,
        show_apparent_size: bool,
    ) -> Result<FileToDelete, ModelError> {
        let node = self.node(node_id).ok_or_else(|| {
            ModelError::Invariant("Selected item is no longer on this scan page".to_string())
        })?;
        if node.parent != Some(self.current_id) {
            return Err(ModelError::Invariant(
                "Only a directly observed item can be deleted while scanning".to_string(),
            ));
        }
        let (expected_kind, file_type) = match node.kind {
            NodeKind::Directory => (NodeKind::Directory, FileType::Folder),
            NodeKind::File => (NodeKind::File, FileType::File),
            NodeKind::Link => (NodeKind::Link, FileType::File),
            NodeKind::Root | NodeKind::Synthetic(_) => {
                return Err(ModelError::Invariant(
                    "Only a directly observed file or folder can be deleted while scanning"
                        .to_string(),
                ));
            }
        };
        let relative = self.relative_for_id(node_id).ok_or_else(|| {
            ModelError::Invariant("Selected item has no concrete scan path".to_string())
        })?;
        Self::concrete_deletion_target(
            ConcreteDeletionTarget {
                root_path: self.root_path.clone(),
                node_id,
                relative: relative.clone(),
                expected_kind,
                file_type,
                metrics: SummaryMetrics {
                    apparent_bytes: node.metrics.apparent_bytes,
                    allocated_bytes: node.metrics.allocated_bytes,
                    reclaimable_bytes: node.metrics.reclaimable_bytes,
                    descendants: node.metrics.descendants,
                },
                expected_snapshot: Some(node.snapshot.clone()),
            },
            show_apparent_size,
        )
    }

    fn concrete_deletion_target(
        target: ConcreteDeletionTarget,
        show_apparent_size: bool,
    ) -> Result<FileToDelete, ModelError> {
        let ConcreteDeletionTarget {
            root_path,
            node_id,
            relative,
            expected_kind,
            file_type,
            metrics,
            expected_snapshot,
        } = target;
        let expected_snapshot = expected_snapshot.ok_or_else(|| {
            ModelError::Invariant(
                "Wait for this item to receive a verified scan preview before deleting".to_string(),
            )
        })?;
        if expected_snapshot.kind != expected_kind || expected_snapshot.identity.is_none() {
            return Err(ModelError::Invariant(
                "Wait for this item to receive a verified scan preview before deleting".to_string(),
            ));
        }
        Ok(FileToDelete {
            node_id,
            synthetic: false,
            path_in_filesystem: root_path,
            path_to_file: relative.components().to_vec(),
            file_type,
            num_descendants: (expected_kind == NodeKind::Directory).then_some(metrics.descendants),
            size: if show_apparent_size {
                metrics.apparent_bytes
            } else {
                metrics.allocated_bytes.lower
            },
            expected_snapshot,
            reviewed_entries: Vec::new(),
        })
    }

    fn push_node(&mut self, parent: NodeId, input: SnapshotNode) -> Result<NodeId, ModelError> {
        let SnapshotNode {
            name,
            kind,
            metrics,
            coverage,
            relative,
            scan_snapshot,
            use_live_snapshot,
        } = input;
        let id =
            NodeId(
                u32::try_from(self.nodes.len()).map_err(|_| ModelError::MemoryExhausted {
                    required: usize::MAX,
                    limit: self.page_memory_limit,
                })?,
            );
        let snapshot = match scan_snapshot {
            Some(snapshot) => snapshot,
            None if use_live_snapshot => relative.as_ref().map_or_else(
                || snapshot_from_metrics(kind, metrics),
                |relative| snapshot_for_path(&self.root_path, relative, kind, metrics),
            ),
            None => snapshot_from_metrics(kind, metrics),
        };
        let scanning_directory = self.provisional && kind.is_directory();
        let mut node = Node::new(
            id,
            Some(parent),
            name,
            kind,
            if scanning_directory {
                NodeState::Scanning
            } else {
                state_for(coverage)
            },
            snapshot,
        );
        node.metrics = node_metrics(metrics);
        node.unscanned_reason = if scanning_directory {
            None
        } else {
            reason_for(coverage)
        };
        let parent_node = self.nodes.get_mut(parent.index()).ok_or_else(|| {
            ModelError::Invariant("snapshot page parent disappeared while building".to_string())
        })?;
        parent_node.children.push(id);
        self.nodes.push(node);
        self.relative_paths.push(relative);
        Ok(id)
    }
}

impl TreeView for SnapshotTree {
    fn current_node(&self) -> &Node {
        self.nodes
            .get(self.current_id.index())
            .expect("snapshot page current node must exist")
    }

    fn total_node(&self) -> &Node {
        self.nodes.first().expect("snapshot page root must exist")
    }

    fn get_current_path(&self) -> PathBuf {
        self.root_path.join(self.current_relative.to_path_buf())
    }
    fn current_relative_path(&self) -> &RelativePath {
        self.current_relative()
    }

    fn node(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(id.index())
    }
    fn scan_root(&self) -> &Path {
        &self.root_path
    }

    fn relative_path_for_id(&self, id: NodeId) -> Option<&RelativePath> {
        self.relative_for_id(id)
    }

    fn has_filter(&self) -> bool {
        self.filter.is_some()
    }

    fn storage_stats(&self) -> Option<(u64, u64)> {
        Some(self.scan_store_stats)
    }

    fn failed_to_read(&self) -> u64 {
        u64::try_from(
            self.nodes
                .iter()
                .filter(|node| node.id != NodeId(0) && node.state == NodeState::Uncertain)
                .count(),
        )
        .unwrap_or(u64::MAX)
    }

    fn unreadable_path_count(&self) -> u64 {
        self.unrecorded_path_count
            .saturating_add(self.failed_to_read())
    }
}

fn ensure_page_fits(required: usize, limit: usize) -> Result<(), ModelError> {
    if required > limit {
        return Err(ModelError::MemoryExhausted { required, limit });
    }
    Ok(())
}

fn page_retained_bytes(page: &ScanPage) -> usize {
    let node_count = page
        .entries
        .len()
        .saturating_add(page.folder.depth())
        .saturating_add(1)
        .saturating_add(usize::from(page.shared_allocation.is_some()));
    let names = page
        .entries
        .iter()
        .map(|entry| {
            entry
                .path
                .components()
                .last()
                .map_or(0, |name| name.as_encoded_bytes().len())
        })
        .sum::<usize>()
        .saturating_add(
            page.folder
                .components()
                .iter()
                .map(|name| name.as_encoded_bytes().len())
                .sum(),
        )
        .saturating_add(usize::from(page.shared_allocation.is_some()) * "Shared allocation".len());
    node_count
        .saturating_mul(size_of::<Node>())
        .saturating_add(names)
        .saturating_add(node_count.saturating_mul(size_of::<Option<RelativePath>>()))
}

fn snapshot_for_path(
    root_path: &Path,
    relative: &RelativePath,
    kind: NodeKind,
    metrics: SummaryMetrics,
) -> EntrySnapshot {
    let path = root_path.join(relative.to_path_buf());
    let fallback = snapshot_from_metrics(kind, metrics);
    let Ok(metadata) = fs::symlink_metadata(&path) else {
        return fallback;
    };
    let Ok(identity) = identity_for(&path, &metadata) else {
        return fallback;
    };
    let Some(identity) = identity else {
        return fallback;
    };
    let actual_kind = if metadata.file_type().is_symlink() || identity.reparse_point {
        NodeKind::Link
    } else if metadata.is_dir() {
        NodeKind::Directory
    } else {
        NodeKind::File
    };
    if !(kind == NodeKind::Root && actual_kind == NodeKind::Directory) && actual_kind != kind {
        return fallback;
    }
    EntrySnapshot {
        identity: Some(identity),
        kind,
        apparent_bytes: if kind.is_directory() {
            0
        } else {
            u128::from(metadata.len())
        },
        allocated_bytes: (!kind.is_directory())
            .then(|| physical_size(&path, &metadata).ok().map(u128::from))
            .flatten(),
        modified_nanos: metadata
            .modified()
            .ok()
            .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos()),
    }
}

fn snapshot_from_metrics(kind: NodeKind, metrics: SummaryMetrics) -> EntrySnapshot {
    EntrySnapshot {
        identity: None,
        kind,
        apparent_bytes: metrics.apparent_bytes,
        allocated_bytes: (!kind.is_directory())
            .then_some(metrics.allocated_bytes.upper)
            .flatten(),
        modified_nanos: None,
    }
}

fn node_kind_for(kind: PageEntryKind) -> NodeKind {
    match kind {
        PageEntryKind::Directory => NodeKind::Directory,
        PageEntryKind::File => NodeKind::File,
        PageEntryKind::Link => NodeKind::Link,
    }
}

fn node_metrics(metrics: SummaryMetrics) -> NodeMetrics {
    NodeMetrics {
        apparent_bytes: metrics.apparent_bytes,
        allocated_bytes: metrics.allocated_bytes,
        reclaimable_bytes: metrics.reclaimable_bytes,
        descendants: metrics.descendants,
    }
}

fn state_for(coverage: Coverage) -> NodeState {
    match coverage {
        Coverage::Complete => NodeState::Complete,
        Coverage::Uncertain => NodeState::Uncertain,
    }
}

fn reason_for(coverage: Coverage) -> Option<UnscannedReason> {
    match coverage {
        Coverage::Complete => None,
        Coverage::Uncertain => Some(UnscannedReason::Metadata(
            "some scan data could not be verified".to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::model::ByteBounds;
    use crate::scan_coordinator::ScanGeneration;
    use crate::scan_store::page::{
        PageEntryKind, ScanPage, ScanPageEntry, SharedAllocationSummary,
    };

    fn path(value: &str) -> RelativePath {
        RelativePath::from_path(Path::new(value)).expect("fixture path should be relative")
    }

    fn page(folder: RelativePath) -> ScanPage {
        ScanPage {
            generation: ScanGeneration::initial(),
            folder,
            folder_metrics: SummaryMetrics {
                apparent_bytes: 4,
                allocated_bytes: ByteBounds::exact(4),
                reclaimable_bytes: ByteBounds::exact(4),
                descendants: 1,
            },
            folder_coverage: Coverage::Complete,
            root_metrics: SummaryMetrics {
                apparent_bytes: 4,
                allocated_bytes: ByteBounds::exact(4),
                reclaimable_bytes: ByteBounds::exact(4),
                descendants: 1,
            },
            root_coverage: Coverage::Complete,
            entries: vec![ScanPageEntry {
                path: path("entry"),
                kind: PageEntryKind::File,
                metrics: SummaryMetrics::leaf(4, ByteBounds::exact(4), ByteBounds::exact(4)),
                coverage: Coverage::Complete,
                snapshot: None,
            }],
            next_after: None,
            shared_allocation: Some(SharedAllocationSummary {
                metrics: SummaryMetrics::leaf(0, ByteBounds::exact(2), ByteBounds::exact(2)),
                coverage: Coverage::Complete,
            }),
            unrecorded_path_count: 0,
        }
    }

    #[test]
    fn snapshot_keeps_only_page_entries_and_marks_shared_total_noninteractive() {
        let root = tempfile::tempdir().expect("snapshot root should exist");
        let tree = SnapshotTree::from_page(
            root.path().to_path_buf(),
            page(RelativePath::root()),
            64 * 1024,
            (0, 4 * 1024 * 1024),
        )
        .expect("snapshot should materialize");
        let files = tree.files_in_current_folder(0, false);
        assert_eq!(files.len(), 2);
        assert!(files.iter().any(|file| file.name == "entry"));
        assert!(
            files
                .iter()
                .any(|file| file.synthetic_kind == Some(SyntheticKind::Shared))
        );
        assert!(
            files
                .iter()
                .find(|file| file.synthetic_kind == Some(SyntheticKind::Shared))
                .is_some_and(|file| !file.is_interactive())
        );
    }

    #[test]
    fn snapshot_deletion_target_uses_immutable_scan_identity() {
        let root = tempfile::tempdir().expect("snapshot root should exist");
        let entry = root.path().join("entry");
        fs::write(&entry, b"data").expect("fixture entry should exist");
        let metadata = fs::symlink_metadata(&entry).expect("fixture metadata should exist");
        let identity = identity_for(&entry, &metadata)
            .expect("fixture identity should resolve")
            .expect("fixture entry should be concrete");
        let mut page = page(RelativePath::root());
        page.entries[0].snapshot = Some(EntrySnapshot {
            identity: Some(identity.clone()),
            kind: NodeKind::File,
            apparent_bytes: u128::from(metadata.len()),
            allocated_bytes: physical_size(&entry, &metadata).ok().map(u128::from),
            modified_nanos: metadata
                .modified()
                .ok()
                .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_nanos()),
        });
        let canonical_entry = page.entries[0].clone();
        let relative = canonical_entry.path.clone();
        let target = SnapshotTree::deletion_target_from_entry(
            root.path().to_path_buf(),
            NodeId(1),
            &relative,
            canonical_entry,
            false,
        )
        .expect("concrete canonical entry should plan");
        assert_eq!(target.full_path(), entry);
        assert_eq!(target.file_type, FileType::File);
        assert_eq!(target.expected_snapshot.identity, Some(identity));
        assert_eq!(target.expected_snapshot.apparent_bytes, 4);
    }
}
