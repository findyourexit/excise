use std::ffi::OsString;

use crate::model::{NodeId, SyntheticKind};

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

impl FileMetadata {
    /// Grouped totals are selectable for explanation. Only a shared allocation
    /// total has no useful item-specific detail to inspect.
    #[must_use]
    pub const fn is_interactive(&self) -> bool {
        !matches!(self.synthetic_kind, Some(SyntheticKind::Shared))
    }
}

fn calculate_percentage(size: u128, total_size: u128, total_files_in_parent: usize) -> f64 {
    if size == 0 && total_size == 0 {
        1.0 / total_files_in_parent.max(1) as f64
    } else {
        size as f64 / total_size as f64
    }
}

/// Sorts one bounded direct-child page and recalculates its visible weights.
pub(crate) fn normalize_file_metadata(
    mut files: Vec<FileMetadata>,
    offset: usize,
) -> Vec<FileMetadata> {
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
