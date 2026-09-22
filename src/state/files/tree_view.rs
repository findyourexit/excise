use std::path::PathBuf;

use crate::model::{Node, NodeId};

use super::FileTree;

/// Read-only tree information required by the map, summary, and inspector.
///
/// The live mutable arena and a bounded immutable `ScanStore` page intentionally
/// expose the same presentation contract without sharing mutation behavior.
pub(crate) trait TreeView {
    fn current_node(&self) -> &Node;
    fn total_node(&self) -> &Node;
    fn get_current_path(&self) -> PathBuf;
    fn node(&self, id: NodeId) -> Option<&Node>;
    fn has_filter(&self) -> bool;
    fn model_stats(&self) -> (usize, usize, bool);
    fn failed_to_read(&self) -> u64;
}

impl TreeView for FileTree {
    fn current_node(&self) -> &Node {
        self.current_node()
    }

    fn total_node(&self) -> &Node {
        self.total_node()
    }

    fn get_current_path(&self) -> PathBuf {
        self.get_current_path()
    }

    fn node(&self, id: NodeId) -> Option<&Node> {
        self.node(id)
    }

    fn has_filter(&self) -> bool {
        self.filter().is_some()
    }

    fn model_stats(&self) -> (usize, usize, bool) {
        self.model_stats()
    }

    fn failed_to_read(&self) -> u64 {
        self.failed_to_read
    }
}
