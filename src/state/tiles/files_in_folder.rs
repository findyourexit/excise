use std::ffi::OsString;

use crate::filter::FilterPattern;
use crate::model::{Arena, NodeId, NodeKind, NodeState, SyntheticKind};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FileType {
    File,
    Folder,
    Synthetic,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FileMetadata {
    pub node_id: NodeId,
    pub name: OsString,
    pub size: u128,
    pub apparent_size: u128,
    pub descendants: Option<u64>,
    pub percentage: f64,
    pub file_type: FileType,
    pub synthetic_kind: Option<SyntheticKind>,
    pub uncertain: bool,
}

fn subtree_matches_filter(
    arena: &Arena,
    root: NodeId,
    filter: &FilterPattern,
    filter_root: &std::path::Path,
) -> bool {
    let mut pending = vec![root];
    while let Some(id) = pending.pop() {
        let Some(node) = arena.node(id) else {
            continue;
        };
        if arena.path_for(id).is_some_and(|path| {
            path.starts_with(filter_root) && filter.matches_path(&path, filter_root)
        }) {
            return true;
        }
        pending.extend(node.children.iter().copied());
    }
    false
}

fn calculate_percentage(size: u128, total_size: u128, total_files_in_parent: usize) -> f64 {
    if size == 0 && total_size == 0 {
        1.0 / total_files_in_parent.max(1) as f64
    } else {
        size as f64 / total_size as f64
    }
}

pub fn files_in_folder(
    arena: &Arena,
    parent: NodeId,
    offset: usize,
    show_apparent_size: bool,
    filter: Option<&FilterPattern>,
    filter_root: Option<&std::path::Path>,
) -> Vec<FileMetadata> {
    let mut files = arena
        .children(parent)
        .iter()
        .filter_map(|id| arena.node(*id))
        .filter(|node| {
            node.kind.is_synthetic()
                || filter.is_none_or(|filter| {
                    filter_root
                        .is_some_and(|root| subtree_matches_filter(arena, node.id, filter, root))
                })
        })
        .map(|node| {
            let size = if show_apparent_size {
                node.metrics.apparent_bytes
            } else {
                node.metrics.allocated_bytes.lower
            };
            let (descendants, file_type, synthetic_kind) = match node.kind {
                NodeKind::Root | NodeKind::Directory => {
                    (Some(node.metrics.descendants), FileType::Folder, None)
                }
                NodeKind::File | NodeKind::Link => (None, FileType::File, None),
                NodeKind::Synthetic(kind) => (
                    Some(node.metrics.descendants),
                    FileType::Synthetic,
                    Some(kind),
                ),
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
    let total_size = files
        .iter()
        .fold(0_u128, |total, file| total.saturating_add(file.size));
    let count = files.len();
    for file in &mut files {
        file.percentage = calculate_percentage(file.size, total_size, count);
    }
    files.sort_by(|left, right| {
        right
            .percentage
            .total_cmp(&left.percentage)
            .then_with(|| left.name.cmp(&right.name))
    });

    if offset > 0 {
        let removed_count = offset.min(files.len());
        let removed_size = files
            .drain(..removed_count)
            .fold(0_u128, |total, file| total.saturating_add(file.size));
        let remaining_total = total_size.saturating_sub(removed_size);
        let remaining_count = files.len();
        for file in &mut files {
            file.percentage = calculate_percentage(file.size, remaining_total, remaining_count);
        }
    }
    files
}
