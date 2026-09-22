use std::path::{Path, PathBuf};

use crate::model::{Node, NodeId};

/// Read-only tree information required by the map, summary, and inspector.
///
/// The root-only loading tree and a bounded immutable `ScanStore` page intentionally
/// expose the same presentation contract without sharing scan ownership.
pub(crate) trait TreeView {
    fn current_node(&self) -> &Node;
    fn total_node(&self) -> &Node;
    fn get_current_path(&self) -> PathBuf;
    fn current_relative_path(&self) -> &crate::scan_coordinator::RelativePath;
    fn node(&self, id: NodeId) -> Option<&Node>;
    fn scan_root(&self) -> &Path;
    fn relative_path_for_id(&self, id: NodeId) -> Option<&crate::scan_coordinator::RelativePath>;

    fn has_filter(&self) -> bool;
    fn storage_stats(&self) -> Option<(u64, u64)>;
    fn failed_to_read(&self) -> u64;
    fn unreadable_path_count(&self) -> u64 {
        self.failed_to_read()
    }
}
