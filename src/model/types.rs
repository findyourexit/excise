use std::ffi::OsStr;
use std::sync::Arc;

use crate::native_path::NativeIdentity;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct NodeId(pub u32);

impl NodeId {
    #[must_use]
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ByteBounds {
    pub lower: u128,
    pub upper: Option<u128>,
}

impl ByteBounds {
    #[must_use]
    pub const fn exact(bytes: u128) -> Self {
        Self {
            lower: bytes,
            upper: Some(bytes),
        }
    }

    #[must_use]
    pub const fn unknown() -> Self {
        Self {
            lower: 0,
            upper: None,
        }
    }

    pub fn add(&mut self, other: Self) {
        self.lower = self.lower.saturating_add(other.lower);
        self.upper = match (self.upper, other.upper) {
            (Some(left), Some(right)) => Some(left.saturating_add(right)),
            _ => None,
        };
    }

    pub fn subtract(&mut self, other: Self) {
        self.lower = self.lower.saturating_sub(other.lower);
        self.upper = match (self.upper, other.upper) {
            (Some(left), Some(right)) => Some(left.saturating_sub(right)),
            _ => None,
        };
    }
}

impl Default for ByteBounds {
    fn default() -> Self {
        Self::exact(0)
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct NodeMetrics {
    pub apparent_bytes: u128,
    pub allocated_bytes: ByteBounds,
    pub reclaimable_bytes: ByteBounds,
    pub descendants: u64,
}

impl NodeMetrics {
    pub fn add(&mut self, other: Self) {
        self.apparent_bytes = self.apparent_bytes.saturating_add(other.apparent_bytes);
        self.allocated_bytes.add(other.allocated_bytes);
        self.reclaimable_bytes.add(other.reclaimable_bytes);
        self.descendants = self.descendants.saturating_add(other.descendants);
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SyntheticKind {
    Other,
    Shared,
    Aggregate,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum NodeKind {
    Root,
    Directory,
    File,
    Link,
    Synthetic(SyntheticKind),
}

impl NodeKind {
    #[must_use]
    pub const fn is_directory(self) -> bool {
        matches!(self, Self::Root | Self::Directory)
    }

    #[must_use]
    pub const fn is_synthetic(self) -> bool {
        matches!(self, Self::Synthetic(_))
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum NodeState {
    Scanning,
    Complete,
    Aggregated,
    Uncertain,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EntrySnapshot {
    pub identity: Option<NativeIdentity>,
    pub kind: NodeKind,
    pub apparent_bytes: u128,
    pub allocated_bytes: Option<u128>,
    pub modified_nanos: Option<u128>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UnscannedReason {
    SymbolicLink,
    FilesystemBoundary,
    Excluded(String),
    Metadata(String),
    Replacement(String),
    /// The scan continues, but its bounded identity store can no longer prove
    /// exact physical-allocation and reclaimability metrics.
    IdentityStorageCapacity,
    MemoryAggregation,
}
#[derive(Clone, Debug)]
pub struct Node {
    pub id: NodeId,
    pub parent: Option<NodeId>,
    pub name: Arc<OsStr>,
    pub kind: NodeKind,
    pub state: NodeState,
    pub children: Vec<NodeId>,
    pub metrics: NodeMetrics,
    pub snapshot: EntrySnapshot,
    pub unscanned_reason: Option<UnscannedReason>,
    pub last_access: u64,
}

impl Node {
    #[must_use]
    pub fn new(
        id: NodeId,
        parent: Option<NodeId>,
        name: Arc<OsStr>,
        kind: NodeKind,
        state: NodeState,
        snapshot: EntrySnapshot,
    ) -> Self {
        Self {
            id,
            parent,
            name,
            kind,
            state,
            children: Vec::new(),
            metrics: NodeMetrics::default(),
            snapshot,
            unscanned_reason: None,
            last_access: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::sync::Arc;

    use super::{EntrySnapshot, Node, NodeId, NodeKind, NodeMetrics, NodeState};

    #[test]
    fn node_literal_does_not_require_arena_accounting() {
        let node = Node {
            id: NodeId(7),
            parent: None,
            name: Arc::from(OsStr::new("fixture")),
            kind: NodeKind::File,
            state: NodeState::Complete,
            children: Vec::new(),
            metrics: NodeMetrics::default(),
            snapshot: EntrySnapshot {
                identity: None,
                kind: NodeKind::File,
                apparent_bytes: 0,
                allocated_bytes: Some(0),
                modified_nanos: None,
            },
            unscanned_reason: None,
            last_access: 0,
        };

        assert_eq!(node.id, NodeId(7));
    }
}
