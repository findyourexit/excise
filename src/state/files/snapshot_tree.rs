use std::ffi::OsStr;
use std::fs;
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use crate::model::{
    EntrySnapshot, ModelError, Node, NodeId, NodeKind, NodeMetrics, NodeState, SyntheticKind,
    UnscannedReason,
};

use crate::native_path::identity_for;
use crate::os::physical_size;
use crate::scan_coordinator::RelativePath;
use crate::scan_store::page::{PageEntryKind, ScanPage, ScanPageEntry};
use crate::scan_store::path_reducer::{Coverage, SummaryMetrics};
use crate::state::FileToDelete;
use crate::state::files::tree_view::TreeView;
use crate::state::tiles::files_in_folder::normalize_file_metadata;
use crate::state::tiles::{FileMetadata, FileType};

/// A small materialized view of one immutable `ScanStore` page.
///
/// It owns only the current folder's direct entries and its ancestor chain. The
/// underlying generation remains in the `ScanStore`; switching folders replaces
/// this view rather than growing a second full in-memory tree.
enum SnapshotSource {
    Stored(EntrySnapshot),
    LivePath,
    Metrics,
}

struct SnapshotNode {
    name: Arc<OsStr>,
    kind: NodeKind,
    metrics: SummaryMetrics,
    coverage: Coverage,
    relative: Option<RelativePath>,
    source: SnapshotSource,
}

pub(crate) struct SnapshotTree {
    root_path: PathBuf,
    current_relative: RelativePath,
    current_id: NodeId,
    nodes: Vec<Node>,
    relative_paths: Vec<Option<RelativePath>>,
    model_stats: (usize, usize, bool),
}

impl SnapshotTree {
    /// # Errors
    ///
    /// Returns an error when the bounded page cannot fit the configured model
    /// budget or cannot be assigned node IDs.
    pub(crate) fn from_page(
        root_path: PathBuf,
        page: ScanPage,
        model_stats: (usize, usize, bool),
    ) -> Result<Self, ModelError> {
        ensure_page_fits(&page, model_stats.1)?;
        let ScanPage {
            folder,
            folder_metrics,
            folder_coverage,
            root_metrics,
            root_coverage,
            entries,
            shared_allocation,
            ..
        } = page;
        let root_snapshot = snapshot_for_path(
            &root_path,
            &RelativePath::root(),
            NodeKind::Root,
            root_metrics,
        );
        let mut root = Node::new(
            NodeId(0),
            None,
            Arc::from(OsStr::new("")),
            NodeKind::Root,
            state_for(root_coverage),
            root_snapshot,
        );
        root.metrics = node_metrics(root_metrics);
        root.unscanned_reason = reason_for(root_coverage);
        let mut tree = Self {
            root_path,
            current_relative: folder.clone(),
            current_id: NodeId(0),
            nodes: vec![root],
            relative_paths: vec![Some(RelativePath::root())],
            model_stats,
        };

        let mut parent = NodeId(0);
        for (index, component) in folder.components().iter().enumerate() {
            let relative = RelativePath::from_components(
                folder.components()[..index.saturating_add(1)].to_vec(),
            )
            .map_err(|_| {
                ModelError::Invariant("scan page contained an invalid ancestor".to_string())
            })?;
            let is_current = index.saturating_add(1) == folder.depth();

            let (metrics, coverage) = if is_current {
                (folder_metrics, folder_coverage)
            } else {
                (SummaryMetrics::default(), Coverage::Uncertain)
            };
            parent = tree.push_node(
                parent,
                SnapshotNode {
                    name: Arc::from(component.as_os_str()),
                    kind: NodeKind::Directory,
                    metrics,
                    coverage,
                    relative: Some(relative),
                    source: SnapshotSource::LivePath,
                },
            )?;
        }
        tree.current_id = parent;

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
            tree.push_node(
                tree.current_id,
                SnapshotNode {
                    name: Arc::from(name.as_os_str()),
                    kind: node_kind_for(kind),
                    metrics,
                    coverage,
                    relative: Some(path),
                    source: snapshot.map_or(SnapshotSource::Metrics, SnapshotSource::Stored),
                },
            )?;
        }
        if let Some(shared) = shared_allocation {
            tree.push_node(
                tree.current_id,
                SnapshotNode {
                    name: Arc::from(OsStr::new("Shared allocation")),
                    kind: NodeKind::Synthetic(SyntheticKind::Shared),
                    metrics: shared.metrics,
                    coverage: shared.coverage,
                    relative: None,
                    source: SnapshotSource::Metrics,
                },
            )?;
        }
        Ok(tree)
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
                    NodeKind::Root | NodeKind::Synthetic(_) => {
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

    /// Builds an identity-bound deletion target from the immutable page's
    /// scan-time snapshot. The planner revalidates this exact snapshot again
    /// immediately before every filesystem mutation.
    ///
    /// # Errors
    ///
    /// Returns an error when the selected page entry is virtual or lacks the
    /// stable scan-time identity required for safe deletion.
    pub(crate) fn deletion_target_for_id(
        &self,
        id: NodeId,
        show_apparent_size: bool,
    ) -> Result<FileToDelete, ModelError> {
        let node = self
            .node(id)
            .ok_or_else(|| ModelError::InvalidPath("selected page item disappeared".to_string()))?;
        let (expected_kind, file_type) = match node.kind {
            NodeKind::Directory => (NodeKind::Directory, FileType::Folder),
            NodeKind::File => (NodeKind::File, FileType::File),
            NodeKind::Link => (NodeKind::Link, FileType::File),
            NodeKind::Root | NodeKind::Synthetic(_) => {
                return Err(ModelError::Invariant(
                    "virtual scan summaries cannot be deleted".to_string(),
                ));
            }
        };
        let relative = self.relative_for_id(id).ok_or_else(|| {
            ModelError::Invariant("selected page item has no backing path".to_string())
        })?;
        let expected_snapshot = node.snapshot.clone();
        if expected_snapshot.kind != expected_kind || expected_snapshot.identity.is_none() {
            return Err(ModelError::Invariant(
                "selected item has no verified scan-time identity".to_string(),
            ));
        }
        Ok(FileToDelete {
            node_id: id,
            synthetic: false,
            path_in_filesystem: self.root_path.clone(),
            path_to_file: relative.components().to_vec(),
            file_type,
            num_descendants: (expected_kind == NodeKind::Directory)
                .then_some(node.metrics.descendants),
            size: if show_apparent_size {
                node.metrics.apparent_bytes
            } else {
                node.metrics.allocated_bytes.lower
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
            source,
        } = input;
        let id =
            NodeId(
                u32::try_from(self.nodes.len()).map_err(|_| ModelError::MemoryExhausted {
                    required: usize::MAX,
                    limit: self.model_stats.1,
                })?,
            );
        let snapshot = match source {
            SnapshotSource::Stored(snapshot) => snapshot,
            SnapshotSource::LivePath => relative.as_ref().map_or_else(
                || snapshot_from_metrics(kind, metrics),
                |relative| snapshot_for_path(&self.root_path, relative, kind, metrics),
            ),
            SnapshotSource::Metrics => snapshot_from_metrics(kind, metrics),
        };
        let mut node = Node::new(id, Some(parent), name, kind, state_for(coverage), snapshot);
        node.metrics = node_metrics(metrics);
        node.unscanned_reason = reason_for(coverage);
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

    fn node(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(id.index())
    }

    fn has_filter(&self) -> bool {
        false
    }

    fn model_stats(&self) -> (usize, usize, bool) {
        self.model_stats
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
}

fn ensure_page_fits(page: &ScanPage, limit: usize) -> Result<(), ModelError> {
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
    let required = node_count
        .saturating_mul(size_of::<Node>())
        .saturating_add(names)
        .saturating_add(node_count.saturating_mul(size_of::<Option<RelativePath>>()));
    if required > limit {
        return Err(ModelError::MemoryExhausted { required, limit });
    }
    Ok(())
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
        }
    }

    #[test]
    fn snapshot_keeps_only_page_entries_and_marks_shared_total_noninteractive() {
        let root = tempfile::tempdir().expect("snapshot root should exist");
        let tree = SnapshotTree::from_page(
            root.path().to_path_buf(),
            page(RelativePath::root()),
            (0, 64 * 1024, false),
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
        let tree = SnapshotTree::from_page(root.path().to_path_buf(), page, (0, 64 * 1024, false))
            .expect("snapshot should materialize");
        fs::write(&entry, b"replacement").expect("fixture should change after scan");
        let target = tree
            .deletion_target_for_id(NodeId(1), false)
            .expect("concrete page entry should plan");
        assert_eq!(target.full_path(), entry);
        assert_eq!(target.file_type, FileType::File);
        assert_eq!(target.expected_snapshot.identity, Some(identity));
        assert_eq!(target.expected_snapshot.apparent_bytes, 4);
    }
}
