use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs::{self, Metadata};
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use file_id::FileId;

use super::identity_store::{IdentityRecord, IdentityStore, merge_declared_links};
use super::{
    ByteBounds, EntrySnapshot, MemoryBudget, ModelError, Node, NodeId, NodeKind, NodeMetrics,
    NodeState, SyntheticKind, UnscannedReason,
};
use crate::native_path::{NativeIdentity, identity_for};
use crate::os::physical_size;
use crate::temporary_storage::TemporaryStorage;

const NODE_SLOT_BYTES: usize = size_of::<Option<Node>>();
const RETAINED_CHILD_SLOT_BYTES: usize = size_of::<u32>();
const SPARE_CHILD_SLOT_BYTES: usize = size_of::<u32>();
const NODE_OVERHEAD: usize =
    NODE_SLOT_BYTES + RETAINED_CHILD_SLOT_BYTES + SPARE_CHILD_SLOT_BYTES + 96;
const DUPLICATE_ID_OVERHEAD: usize = size_of::<FileId>() + 64;
const DEFAULT_MAX_CHILDREN: usize = 4_096;
/// Eviction candidates kept per directory that has reached the child cap.
///
/// One sweep of a full directory fills the stash and the evictions that follow
/// drain it, so a directory holding a million entries pays a sweep every sixty
/// four evictions instead of one per entry. The fixed candidate buffer stays
/// small beside the megabyte of nodes such a directory already holds.
const EVICTION_STASH: usize = 64;
type RetentionRank = (bool, u128);
type StashedCandidate = (NodeId, Option<RetentionRank>);

/// A retention-order snapshot that survives later metric growth.
///
/// Names and IDs do not change while a node is retained, so keeping them with
/// the rank lets a cached frontier distinguish equal-rank entries exactly as
/// [`Arena::retention_order`] does.
#[derive(Clone, Debug, Eq, PartialEq)]
struct RetentionKey {
    rank: RetentionRank,
    name: Arc<OsStr>,
    id: NodeId,
}

impl RetentionKey {
    fn compare_candidate(
        &self,
        rank: RetentionRank,
        name: &OsStr,
        id: NodeId,
    ) -> std::cmp::Ordering {
        compare_retention(rank, name, id, self.rank, self.name.as_ref(), self.id)
    }
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct UntrackedMetrics {
    allocated_bytes: ByteBounds,
    reclaimable_bytes: ByteBounds,
}

impl UntrackedMetrics {
    fn is_zero(self) -> bool {
        self == Self::default()
    }

    fn add(&mut self, other: Self) {
        self.allocated_bytes.add(other.allocated_bytes);
        self.reclaimable_bytes.add(other.reclaimable_bytes);
    }
}

const UNTRACKED_METRICS_OVERHEAD: usize = size_of::<NodeId>() + size_of::<UntrackedMetrics>() + 64;

pub struct Arena {
    nodes: Vec<Option<Node>>,
    // Keeps capped-child and retained-capacity accounting O(1) without
    // changing `Node`'s public layout.
    retained_child_counts: Vec<u32>,
    // Budgeted child-vector slots that survived a child removal.
    spare_child_slots: Vec<u32>,
    free_nodes: Vec<NodeId>,
    lookup: HashMap<(NodeId, Arc<OsStr>), NodeId>,
    root: NodeId,
    root_path: PathBuf,
    budget: MemoryBudget,
    identities: IdentityStore,
    temporary_storage: TemporaryStorage,
    duplicate_identities: HashSet<FileId>,
    untracked_metrics: HashMap<NodeId, UntrackedMetrics>,
    access_tick: u64,
    max_children_per_directory: usize,
    /// Per-directory eviction candidates, largest first, so the next victim
    /// pops off the end. Only directories at the cap carry one.
    eviction_stash: HashMap<NodeId, EvictionStash>,
    #[cfg(test)]
    eviction_stash_sweeps: usize,
}

/// The smallest children of one directory, captured by a single sweep.
struct EvictionStash {
    /// Candidates ordered by [`Arena::retention_order`], largest first.
    /// A missing rank marks a freshly retained child that must be seated by
    /// [`Arena::retention_order`] before it can become a victim.
    candidates: Vec<StashedCandidate>,
    /// The largest full retention key the sweep captured. Every child left out
    /// ranked above it, so a stashed entry that grows past it may now sit behind
    /// one of them and the sweep has to run again.
    ceiling: RetentionKey,
}

const EVICTION_STASH_OVERHEAD: usize = size_of::<NodeId>() + size_of::<EvictionStash>() + 64;
const EVICTION_STASH_ALLOCATION: usize = EVICTION_STASH_OVERHEAD
    .saturating_add(EVICTION_STASH.saturating_mul(size_of::<StashedCandidate>()));

#[allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    reason = "Arena operations share ModelError as their uniform boundary for filesystem paths, bounded storage, and identity persistence. Repeating that contract on each method obscures the model API."
)]
impl Arena {
    pub fn new(root_path: PathBuf, budget: MemoryBudget) -> Result<Self, ModelError> {
        Self::new_with_temporary_storage(root_path, budget, TemporaryStorage::default())
    }

    pub(crate) fn new_with_temporary_storage(
        root_path: PathBuf,
        mut budget: MemoryBudget,
        temporary_storage: TemporaryStorage,
    ) -> Result<Self, ModelError> {
        let identity_budget = budget.model_limit() / 4;
        budget.reserve(identity_budget)?;
        let root_name: Arc<OsStr> = root_path
            .file_name()
            .unwrap_or_else(|| root_path.as_os_str())
            .into();
        let root = NodeId(0);
        let root_snapshot = EntrySnapshot {
            identity: None,
            kind: NodeKind::Root,
            apparent_bytes: 0,
            allocated_bytes: None,
            modified_nanos: None,
        };
        let mut arena = Self {
            nodes: Vec::new(),
            retained_child_counts: Vec::new(),
            spare_child_slots: Vec::new(),
            free_nodes: Vec::new(),
            lookup: HashMap::new(),
            root,
            root_path,
            budget,
            identities: IdentityStore::new_with_temporary_storage(
                identity_budget,
                &temporary_storage,
            )?,
            duplicate_identities: HashSet::new(),
            temporary_storage,
            untracked_metrics: HashMap::new(),
            access_tick: 0,
            max_children_per_directory: DEFAULT_MAX_CHILDREN,
            eviction_stash: HashMap::new(),
            #[cfg(test)]
            eviction_stash_sweeps: 0,
        };
        arena.reserve_node(&root_name)?;
        arena.nodes.push(Some(Node::new(
            root,
            None,
            root_name,
            NodeKind::Root,
            NodeState::Scanning,
            root_snapshot,
        )));
        arena.retained_child_counts.push(0);
        arena.spare_child_slots.push(0);
        Ok(arena)
    }

    #[must_use]
    pub const fn root(&self) -> NodeId {
        self.root
    }
    pub(crate) fn set_root_identity(&mut self, identity: NativeIdentity) {
        if let Some(root) = self.node_mut(self.root) {
            root.snapshot.identity = Some(identity);
        }
    }

    #[must_use]
    pub const fn memory_used(&self) -> usize {
        self.budget.used()
    }

    #[must_use]
    pub const fn memory_limit(&self) -> usize {
        self.budget.model_limit()
    }

    #[cfg(test)]
    pub(crate) fn consume_remaining_budget_for_test(&mut self) -> Result<(), ModelError> {
        self.budget
            .reserve(self.budget.model_limit().saturating_sub(self.budget.used()))
    }

    #[must_use]
    pub fn identity_count(&self) -> usize {
        self.identities.len()
    }

    #[must_use]
    pub fn identity_spill_path(&self) -> Option<&Path> {
        self.identities.spill_path()
    }

    #[must_use]
    pub(crate) fn temporary_storage(&self) -> TemporaryStorage {
        self.temporary_storage.clone()
    }

    #[must_use]
    pub fn internal_scan_paths(&self) -> Vec<PathBuf> {
        self.identities.internal_scan_paths()
    }

    #[must_use]
    pub fn node(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(id.index()).and_then(Option::as_ref)
    }

    pub fn node_mut(&mut self, id: NodeId) -> Option<&mut Node> {
        self.nodes.get_mut(id.index()).and_then(Option::as_mut)
    }

    pub fn nodes(&self) -> impl Iterator<Item = &Node> {
        self.nodes.iter().filter_map(Option::as_ref)
    }

    pub fn add_entry(
        &mut self,
        path: &Path,
        metadata: &Metadata,
        identity: NativeIdentity,
    ) -> Result<Option<NodeId>, ModelError> {
        self.add_entry_mode(path, metadata, identity, false)
    }

    pub fn add_entry_aggregated(
        &mut self,
        path: &Path,
        metadata: &Metadata,
        identity: NativeIdentity,
    ) -> Result<Option<NodeId>, ModelError> {
        self.add_entry_mode(path, metadata, identity, true)
    }

    #[allow(
        clippy::too_many_lines,
        reason = "Entry insertion must preserve one transactional sequence across hierarchy retention, identity observation, and bounded aggregation."
    )]
    fn add_entry_mode(
        &mut self,
        path: &Path,
        metadata: &Metadata,
        identity: NativeIdentity,
        force_aggregate: bool,
    ) -> Result<Option<NodeId>, ModelError> {
        let relative = path.strip_prefix(&self.root_path).map_err(|_| {
            ModelError::InvalidPath(format!("{} is outside scan root", path.to_string_lossy()))
        })?;
        if relative.as_os_str().is_empty() {
            if let Some(root) = self.node_mut(self.root) {
                root.snapshot.identity = Some(identity);
            }
            return Ok(Some(self.root));
        }

        let components = relative.iter().map(OsStr::to_os_string).collect::<Vec<_>>();
        let mut parent = self.root;
        let mut aggregate_at_parent = false;
        for component in &components[..components.len() - 1] {
            if let Some(existing) = self.find_child(parent, component) {
                if self
                    .node(existing)
                    .is_some_and(|node| node.kind.is_directory())
                {
                    parent = existing;
                    continue;
                }
                aggregate_at_parent = true;
                break;
            }
            if self.retained_child_count(parent) >= self.max_children_per_directory {
                aggregate_at_parent = true;
                break;
            }
            parent = self.ensure_directory(parent, component)?;
        }
        let name = components
            .last()
            .ok_or_else(|| ModelError::InvalidPath("entry had no filename".to_string()))?;

        let kind = if metadata.file_type().is_symlink() || identity.reparse_point {
            NodeKind::Link
        } else if metadata.is_dir() {
            NodeKind::Directory
        } else {
            NodeKind::File
        };
        let apparent = if kind == NodeKind::Directory {
            0
        } else {
            u128::from(metadata.len())
        };
        let allocated = if kind == NodeKind::Directory {
            ByteBounds::exact(0)
        } else {
            physical_size(path, metadata)
                .map(u128::from)
                .map_or_else(|_| ByteBounds::unknown(), ByteBounds::exact)
        };
        let modified_nanos = metadata
            .modified()
            .ok()
            .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos());
        let snapshot = EntrySnapshot {
            identity: Some(identity.clone()),
            kind,
            apparent_bytes: apparent,
            allocated_bytes: if matches!(kind, NodeKind::File | NodeKind::Link) {
                allocated.upper
            } else {
                None
            },
            modified_nanos,
        };
        if let Some(existing) = self.find_child(parent, name.as_os_str()) {
            if self
                .node(existing)
                .is_some_and(|node| node.kind.is_synthetic())
            {
                aggregate_at_parent = true;
            } else {
                if let Some(node) = self.node_mut(existing) {
                    node.kind = kind;
                    node.snapshot = snapshot.clone();
                }
                return Ok(Some(existing));
            }
        }
        let name: Arc<OsStr> = Arc::from(name.as_os_str());
        let at_child_limit = self.retained_child_count(parent) >= self.max_children_per_directory;
        let replacement = if !force_aggregate && !aggregate_at_parent && at_child_limit {
            let metrics = self.preview_leaf_metrics(kind, apparent, allocated, &identity)?;
            self.smallest_retained_child(parent)
                .filter(|victim| self.candidate_outranks(&name, metrics, *victim))
        } else {
            None
        };
        if force_aggregate || aggregate_at_parent || (at_child_limit && replacement.is_none()) {
            let other = self.ensure_other(parent)?;
            let metrics = self.observe_leaf_metrics(
                kind,
                apparent,
                allocated,
                &identity,
                Some(other),
                Some(other),
            )?;
            self.accumulate_other(parent, other, metrics);
            return Ok(None);
        }
        if let Some(victim) = replacement {
            let other = self.ensure_other(parent)?;
            self.aggregate_child_into_other(victim, other)?;
        }
        if self.reserve_child(parent, &name).is_err() {
            let other = self.ensure_other(parent)?;
            let metrics = self.observe_leaf_metrics(
                kind,
                apparent,
                allocated,
                &identity,
                Some(other),
                Some(other),
            )?;
            self.accumulate_other(parent, other, metrics);
            return Ok(None);
        }

        let id = self.next_id()?;
        let state = if kind.is_directory() {
            NodeState::Scanning
        } else {
            NodeState::Complete
        };
        let mut node = Node::new(id, Some(parent), name.clone(), kind, state, snapshot);
        node.metrics =
            self.observe_leaf_metrics(kind, apparent, allocated, &identity, Some(id), Some(id))?;
        self.insert_node(id, node)?;
        self.lookup.insert((parent, name), id);
        self.push_child(parent, id)?;
        self.propagate_add(
            parent,
            self.node(id)
                .map_or(NodeMetrics::default(), |node| node.metrics),
        );
        self.propagate_descendant(parent, 1);
        Ok(Some(id))
    }

    #[allow(
        clippy::too_many_lines,
        reason = "Unscanned entries require one coherent path from visibility state through scoped accounting and bounded retention."
    )]
    pub fn record_unscanned(
        &mut self,
        path: &Path,
        reason: UnscannedReason,
    ) -> Result<(), ModelError> {
        let scoped_zero = has_zero_scoped_metrics(&reason);
        let relative = path.strip_prefix(&self.root_path).map_err(|_| {
            ModelError::InvalidPath(format!("{} is outside scan root", path.to_string_lossy()))
        })?;
        let components = relative.iter().map(OsStr::to_os_string).collect::<Vec<_>>();
        if components.is_empty() {
            if let Some(root) = self.node_mut(self.root) {
                root.state = NodeState::Uncertain;
                root.unscanned_reason = Some(reason);
                if scoped_zero {
                    root.metrics = NodeMetrics::default();
                } else {
                    root.metrics.allocated_bytes.upper = None;
                    root.metrics.reclaimable_bytes.upper = None;
                }
            }
            if scoped_zero {
                self.rebuild_metrics();
            }
            return Ok(());
        }
        let mut parent = self.root;
        let mut aggregate_at_parent = false;
        for component in &components[..components.len() - 1] {
            if let Some(existing) = self.find_child(parent, component) {
                if self
                    .node(existing)
                    .is_some_and(|node| node.kind.is_directory())
                {
                    parent = existing;
                    continue;
                }
                aggregate_at_parent = true;
                break;
            }
            if self.retained_child_count(parent) >= self.max_children_per_directory {
                aggregate_at_parent = true;
                break;
            }
            parent = self.ensure_directory(parent, component)?;
        }
        let name: Arc<OsStr> = Arc::from(
            components
                .last()
                .ok_or_else(|| ModelError::InvalidPath("unscanned path had no name".to_string()))?
                .as_os_str(),
        );
        if let Some(existing) = self.find_child(parent, &name) {
            if self
                .node(existing)
                .is_some_and(|node| node.kind.is_synthetic())
            {
                aggregate_at_parent = true;
            } else if matches!(reason, UnscannedReason::Replacement(_)) {
                let mut removed = Vec::new();
                self.collect_subtree_ids(existing, &mut removed);
                self.remove_nodes_with_accounting(removed)?;
            } else {
                let unknown = matches!(reason, UnscannedReason::Metadata(_));
                let state = unscanned_state(&reason);
                let mut metrics =
                    self.node(existing)
                        .map(|node| node.metrics)
                        .ok_or_else(|| {
                            ModelError::Invariant("unscanned node disappeared".to_string())
                        })?;
                if scoped_zero {
                    metrics = NodeMetrics::default();
                } else if unknown {
                    metrics.allocated_bytes.upper = None;
                    metrics.reclaimable_bytes.upper = None;
                }
                self.reserve_untracked_compaction_slot(existing, metrics)?;
                let node = self.node_mut(existing).ok_or_else(|| {
                    ModelError::Invariant("unscanned node disappeared".to_string())
                })?;
                node.state = state;
                node.unscanned_reason = Some(reason);
                node.metrics = metrics;
                if scoped_zero {
                    self.rebuild_metrics();
                }
                return Ok(());
            }
        }
        let metadata = fs::symlink_metadata(path).ok();
        let metadata_identity = metadata
            .as_ref()
            .and_then(|metadata| identity_for(path, metadata).ok().flatten());
        let is_link = metadata
            .as_ref()
            .is_some_and(|metadata| metadata.file_type().is_symlink())
            || metadata_identity
                .as_ref()
                .is_some_and(|identity| identity.reparse_point)
            || matches!(reason, UnscannedReason::SymbolicLink);
        let kind = if is_link {
            NodeKind::Link
        } else if metadata.as_ref().is_some_and(Metadata::is_dir) {
            NodeKind::Directory
        } else {
            NodeKind::File
        };
        let apparent = metadata
            .as_ref()
            .filter(|_| kind != NodeKind::Directory)
            .map_or(0, |metadata| u128::from(metadata.len()));
        let allocated = metadata
            .as_ref()
            .map_or_else(ByteBounds::unknown, |metadata| {
                if kind == NodeKind::Directory {
                    ByteBounds::exact(0)
                } else {
                    physical_size(path, metadata)
                        .map(u128::from)
                        .map_or_else(|_| ByteBounds::unknown(), ByteBounds::exact)
                }
            });

        let declared_links = metadata_identity
            .as_ref()
            .filter(|identity| identity.reparse_point)
            .and_then(|identity| identity.link_count);
        let metrics = unscanned_metrics(apparent, allocated, kind, &reason, declared_links);
        let at_child_limit = self.retained_child_count(parent) >= self.max_children_per_directory;
        let replacement = if !aggregate_at_parent && at_child_limit {
            self.smallest_retained_child(parent)
                .filter(|victim| self.candidate_outranks(name.as_ref(), metrics, *victim))
        } else {
            None
        };
        if aggregate_at_parent || (at_child_limit && replacement.is_none()) {
            let other = self.ensure_other(parent)?;
            self.accumulate_untracked_other(parent, other, metrics)?;
            return Ok(());
        }
        if let Some(victim) = replacement {
            let other = self.ensure_other(parent)?;
            self.aggregate_child_into_other(victim, other)?;
        }
        let reserved_untracked = self.reserve_untracked_compaction_slot(parent, metrics)?;
        if self.reserve_child(parent, &name).is_err() {
            if reserved_untracked {
                self.remove_untracked_metrics(parent);
            }
            let other = self.ensure_other(parent)?;
            self.accumulate_untracked_other(parent, other, metrics)?;
            return Ok(());
        }
        let id = self.next_id()?;
        let mut node = Node::new(
            id,
            Some(parent),
            name.clone(),
            kind,
            unscanned_state(&reason),
            EntrySnapshot {
                identity: metadata_identity,
                kind,
                apparent_bytes: apparent,
                allocated_bytes: if matches!(kind, NodeKind::File | NodeKind::Link) {
                    allocated.upper
                } else {
                    None
                },
                modified_nanos: metadata
                    .as_ref()
                    .and_then(|metadata| metadata.modified().ok())
                    .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
                    .map(|duration| duration.as_nanos()),
            },
        );
        node.metrics = metrics;
        node.unscanned_reason = Some(reason);
        self.insert_node(id, node)?;
        if reserved_untracked {
            let reservation = self
                .untracked_metrics
                .remove(&parent)
                .expect("untracked reservation should remain available");
            let previous = self.untracked_metrics.insert(id, reservation);
            debug_assert!(previous.is_none());
        }
        self.lookup.insert((parent, name), id);
        self.push_child(parent, id)?;
        self.propagate_add(parent, metrics);
        self.propagate_descendant(parent, 1);
        Ok(())
    }

    pub fn complete_directory(
        &mut self,
        path: &Path,
        expected_identity: Option<&NativeIdentity>,
    ) -> Result<(), ModelError> {
        if let Some(id) = self.find_path(path) {
            if let Some(expected_identity) = expected_identity {
                // The scanner validates directory identity immediately before emitting
                // completion, so the arena does not need to re-stat for an identity
                // check. Modified_nanos may be absent on staging arena roots (which are
                // created by Arena::new with no timestamp), so a single stat is issued
                // when the field is empty to ensure deletion validation can compare
                // timestamps after the staged subtree is merged back into the live arena.
                if let Some(node) = self.node_mut(id)
                    && node.state == NodeState::Scanning
                {
                    if node.snapshot.modified_nanos.is_none() {
                        node.snapshot.modified_nanos = fs::symlink_metadata(path)
                            .ok()
                            .and_then(|m| m.modified().ok())
                            .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
                            .map(|d| d.as_nanos());
                    }
                    node.state = NodeState::Complete;
                    node.snapshot.identity = Some(expected_identity.clone());
                }
                return Ok(());
            }
            let metadata = fs::symlink_metadata(path).ok();
            let identity = metadata
                .as_ref()
                .and_then(|metadata| identity_for(path, metadata).ok().flatten());
            let modified_nanos = metadata
                .as_ref()
                .and_then(|metadata| metadata.modified().ok())
                .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_nanos());
            if let Some(node) = self.node_mut(id)
                && node.state == NodeState::Scanning
            {
                node.state = NodeState::Complete;
                node.snapshot.modified_nanos = modified_nanos;
                if let Some(identity) = identity {
                    node.snapshot.identity = Some(identity);
                }
            }
            return Ok(());
        }
        if self.path_is_aggregated(path) {
            return Ok(());
        }
        Err(ModelError::InvalidPath(path.to_string_lossy().into_owned()))
    }
    pub fn finalize(&mut self) -> Result<(), ModelError> {
        let duplicate_ids = std::mem::take(&mut self.duplicate_identities);
        let duplicate_memory = duplicate_ids.len().saturating_mul(DUPLICATE_ID_OVERHEAD);
        let finalization_result = (|| -> Result<(), ModelError> {
            for file_id in duplicate_ids {
                let Some(record) = self.identities.get(&file_id)? else {
                    continue;
                };
                if record.observed_links < 2 {
                    continue;
                }
                let mut participants = record
                    .nodes
                    .iter()
                    .map(|(id, _)| *id)
                    .filter(|id| self.node(*id).is_some())
                    .collect::<Vec<_>>();
                participants.sort_unstable();
                participants.dedup();
                if participants.is_empty() {
                    continue;
                }
                let Some(allocation_node) = record.allocation_node else {
                    continue;
                };
                if self.node(allocation_node).is_none() {
                    // Its metrics were already retained by a structural aggregate.
                    continue;
                }
                let lca = self
                    .lowest_common_ancestor(&participants)
                    .unwrap_or(self.root);
                let shared_parent = self.shared_parent(lca);
                match self.add_shared_node(shared_parent, &record) {
                    Ok(()) => {}
                    Err(ModelError::MemoryExhausted { .. }) => continue,
                    Err(error) => return Err(error),
                }
                if let Some(node) = self.node_mut(allocation_node) {
                    node.metrics
                        .allocated_bytes
                        .subtract(record.allocated_bytes);
                    node.metrics.reclaimable_bytes.subtract(
                        leaf_metrics(0, record.allocated_bytes, record.declared_links)
                            .reclaimable_bytes,
                    );
                }
            }
            Ok(())
        })();
        self.budget.release(duplicate_memory);
        finalization_result?;
        self.rebuild_metrics();
        if let Some(root) = self.node_mut(self.root)
            && root.state == NodeState::Scanning
        {
            root.state = NodeState::Complete;
        }
        Ok(())
    }
    #[must_use]
    pub fn path_ids(&self, path: &Path) -> Option<Vec<NodeId>> {
        let relative = path.strip_prefix(&self.root_path).ok()?;
        let mut ids = vec![self.root];
        let mut current = self.root;
        for component in relative {
            current = self.find_child(current, component)?;
            ids.push(current);
        }
        Some(ids)
    }

    #[must_use]
    pub fn children(&self, parent: NodeId) -> &[NodeId] {
        self.node(parent)
            .map_or(&[], |node| node.children.as_slice())
    }

    #[must_use]
    pub fn find_child(&self, parent: NodeId, name: &OsStr) -> Option<NodeId> {
        self.lookup
            .get(&(parent, Arc::<OsStr>::from(name)))
            .copied()
    }

    #[must_use]
    pub fn path_for(&self, id: NodeId) -> Option<PathBuf> {
        let mut names = Vec::new();
        let mut current = id;
        while current != self.root {
            let node = self.node(current)?;
            names.push(node.name.clone());
            current = node.parent?;
        }
        let mut path = self.root_path.clone();
        for name in names.into_iter().rev() {
            path.push(name.as_ref());
        }
        Some(path)
    }

    pub fn touch(&mut self, id: NodeId) {
        self.access_tick = self.access_tick.saturating_add(1);
        let tick = self.access_tick;
        if let Some(node) = self.node_mut(id) {
            node.last_access = tick;
        }
    }

    pub fn aggregate_cold_subtree(&mut self, pinned: &HashSet<NodeId>) -> Result<bool, ModelError> {
        let candidate = self
            .nodes
            .iter()
            .filter_map(Option::as_ref)
            .filter(|node| {
                node.kind == NodeKind::Directory
                    && node.state != NodeState::Aggregated
                    && !node.children.is_empty()
                    && !pinned.contains(&node.id)
            })
            .min_by_key(|node| node.last_access)
            .map(|node| node.id);
        let Some(candidate) = candidate else {
            return Ok(false);
        };
        let candidate_parent = self.node(candidate).and_then(|node| node.parent);
        let mut metrics = self
            .node(candidate)
            .map_or(NodeMetrics::default(), |node| node.metrics);
        metrics.descendants = metrics.descendants.saturating_add(1);
        let untracked = self.untracked_metrics_for_subtree(candidate);
        let mut removed = Vec::new();
        for &child in self.children(candidate) {
            self.collect_subtree_ids(child, &mut removed);
        }
        // The model may already be at its hard limit precisely when it needs to
        // collapse a subtree. Re-key one summary that will be removed rather than
        // reserving a second slot before the old tree can release its storage.
        let (reserved, recycled_untracked) =
            if untracked.is_zero() || self.untracked_metrics.contains_key(&candidate) {
                (false, None)
            } else if let Some(source) = removed
                .iter()
                .copied()
                .find(|id| self.untracked_metrics.contains_key(id))
            {
                let source_metrics = self
                    .untracked_metrics
                    .remove(&source)
                    .expect("untracked source must exist");
                let previous = self
                    .untracked_metrics
                    .insert(candidate, UntrackedMetrics::default());
                debug_assert!(previous.is_none());
                (false, Some((source, source_metrics)))
            } else {
                (self.reserve_untracked_slot(candidate, untracked)?, None)
            };
        if let Err(error) = self.identities.remap_removed_nodes(&mut removed, candidate) {
            if let Some((source, source_metrics)) = recycled_untracked {
                let recycled = self
                    .untracked_metrics
                    .remove(&candidate)
                    .expect("recycled untracked slot must exist");
                debug_assert!(recycled.is_zero());
                let previous = self.untracked_metrics.insert(source, source_metrics);
                debug_assert!(previous.is_none());
            } else if reserved {
                self.remove_untracked_metrics(candidate);
            }
            return Err(error);
        }
        self.remove_reusable_nodes(removed);
        self.release_spare_child_slots(candidate);
        if let Some(node) = self.node_mut(candidate) {
            // Aggregates never regain children, so discard the old directory buffer.
            node.children = Vec::new();
            node.kind = NodeKind::Synthetic(SyntheticKind::Aggregate);
            node.state = NodeState::Aggregated;
            node.snapshot.kind = node.kind;
            node.metrics = metrics;
            node.unscanned_reason = Some(UnscannedReason::MemoryAggregation);
        }
        self.set_retained_child_count(candidate, 0);
        if let Some(parent) = candidate_parent {
            self.decrement_retained_child_count(parent);
        }
        self.insert_untracked_metrics(candidate, untracked);
        // Optional eviction caches must yield their budget before the scanner
        // retries the cap-limited insertion that triggered this compaction.
        self.clear_eviction_stashes();
        // `metrics` preserves the candidate's former subtree total and folds its
        // former concrete child count into the synthetic summary. Its ancestors
        // therefore remain exact without collecting and sorting every live node.
        Ok(true)
    }

    pub fn remove_subtree(&mut self, root: NodeId) {
        let _ = self.try_remove_subtree(root);
    }

    pub fn try_remove_subtree(&mut self, root: NodeId) -> Result<(), ModelError> {
        let mut removed = Vec::new();
        self.collect_subtree_ids(root, &mut removed);
        self.remove_nodes_with_accounting(removed).map(|_| ())
    }

    fn collect_subtree_ids(&self, root: NodeId, removed: &mut Vec<NodeId>) {
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            if let Some(node) = self.node(id) {
                stack.extend(node.children.iter().copied());
                removed.push(id);
            }
        }
    }

    pub fn remove_paths(&mut self, paths: &[PathBuf]) -> usize {
        self.try_remove_paths(paths).unwrap_or(0)
    }

    pub fn try_remove_paths(&mut self, paths: &[PathBuf]) -> Result<usize, ModelError> {
        self.try_remove_paths_with_link_counts(paths, &HashMap::new())
    }

    pub(crate) fn try_remove_paths_with_link_counts(
        &mut self,
        paths: &[PathBuf],
        link_counts: &HashMap<FileId, Option<u64>>,
    ) -> Result<usize, ModelError> {
        let mut removed = Vec::new();
        for path in paths {
            if let Some(id) = self.find_path(path)
                && id != self.root
            {
                self.collect_subtree_ids(id, &mut removed);
            }
        }
        self.remove_nodes_with_accounting_and_link_counts(removed, link_counts)
    }

    fn remove_nodes_with_accounting(&mut self, removed: Vec<NodeId>) -> Result<usize, ModelError> {
        self.remove_nodes_with_accounting_and_link_counts(removed, &HashMap::new())
    }

    fn remove_nodes_with_accounting_and_link_counts(
        &mut self,
        mut removed: Vec<NodeId>,
        link_counts: &HashMap<FileId, Option<u64>>,
    ) -> Result<usize, ModelError> {
        removed.sort_unstable();
        removed.dedup();
        if removed.is_empty() {
            return Ok(0);
        }
        let shared = self
            .nodes
            .iter()
            .filter_map(Option::as_ref)
            .filter(|node| {
                node.kind == NodeKind::Synthetic(SyntheticKind::Shared)
                    && removed.binary_search(&node.id).is_err()
            })
            .map(|node| node.id)
            .collect::<Vec<_>>();
        let identities = self.rebuild_identities_without(&removed, link_counts)?;
        let identity_scratch = IdentityStore::new_with_temporary_storage(
            self.identities.memory_limit(),
            &self.temporary_storage,
        )?;
        let mut removal_order = removed.clone();
        removal_order.sort_by_key(|id| self.depth(*id));
        self.remove_nodes(removal_order);
        self.remove_nodes(shared);
        self.identities = identities;
        self.refresh_surviving_link_counts(link_counts)?;
        self.prepare_identity_metrics();
        self.rebuild_identity_metrics(identity_scratch)?;
        Ok(removed.len())
    }

    fn remove_nodes(&mut self, removed: Vec<NodeId>) {
        self.remove_nodes_with_reuse(removed, false);
    }

    fn remove_reusable_nodes(&mut self, removed: Vec<NodeId>) {
        self.remove_nodes_with_reuse(removed, true);
    }

    fn remove_nodes_with_reuse(&mut self, removed: Vec<NodeId>, reuse_ids: bool) {
        let removed_ids = removed.iter().copied().collect::<HashSet<_>>();
        for id in removed.into_iter().rev() {
            self.remove_eviction_stash(id);
            self.remove_untracked_metrics(id);
            if let Some(node) = self.nodes.get_mut(id.index()).and_then(Option::take) {
                let parent_is_removed = node
                    .parent
                    .is_some_and(|parent| removed_ids.contains(&parent));
                self.set_retained_child_count(id, 0);
                self.release_spare_child_slots(id);
                if let Some(parent) = node.parent {
                    self.lookup.remove(&(parent, node.name.clone()));
                    self.detach_child_with_capacity(
                        parent,
                        id,
                        &node.name,
                        !node.kind.is_synthetic(),
                        !parent_is_removed,
                    );
                }
                // Node and sidecar backing slots remain reserved. A surviving
                // parent retains its child slot as a reusable credit. Only a
                // parent removed in this batch drops that child storage.
                let node_bytes = estimate_node(&node.name)
                    .saturating_sub(NODE_SLOT_BYTES)
                    .saturating_sub(RETAINED_CHILD_SLOT_BYTES)
                    .saturating_sub(SPARE_CHILD_SLOT_BYTES);
                let released_child_capacity = if parent_is_removed {
                    size_of::<NodeId>()
                } else {
                    0
                };
                self.budget
                    .release(node_bytes.saturating_add(released_child_capacity));
                if reuse_ids && id != self.root && self.budget.reserve(size_of::<NodeId>()).is_ok()
                {
                    // This is distinct from a surviving parent’s retained
                    // child-vector slot: the ID is now stored in `free_nodes`.
                    self.free_nodes.push(id);
                }
            }
        }
        self.lookup.shrink_to_fit();
    }

    /// Unhooks one child from its parent.
    ///
    /// The name order children are held in locates the seat directly. The list
    /// keeps its capacity: a directory at the cap trades every entry it retains
    /// for one it gives up, so releasing the slot only to claim it again is a
    /// copy of the whole list per entry.
    #[cfg(test)]
    fn detach_child(&mut self, parent: NodeId, child: NodeId, name: &Arc<OsStr>, retained: bool) {
        self.detach_child_with_capacity(parent, child, name, retained, true);
    }

    fn detach_child_with_capacity(
        &mut self,
        parent: NodeId,
        child: NodeId,
        name: &Arc<OsStr>,
        retained: bool,
        retain_capacity: bool,
    ) {
        let seat = self.children(parent).partition_point(|existing| {
            self.node(*existing)
                .is_some_and(|node| node.name.cmp(name).is_lt())
        });
        let detached = if let Some(parent_node) = self.node_mut(parent) {
            let child_count = parent_node.children.len();
            if parent_node.children.get(seat) == Some(&child) {
                parent_node.children.remove(seat);
            } else {
                parent_node.children.retain(|existing| *existing != child);
            }
            parent_node.children.len() != child_count
        } else {
            false
        };
        if retained && detached {
            self.decrement_retained_child_count(parent);
        }
        if retain_capacity && detached {
            self.increment_spare_child_slot(parent);
        }
        let empty_stash = if let Some(stash) = self.eviction_stash.get_mut(&parent) {
            let ceiling_evicted = stash.ceiling.id == child;
            stash
                .candidates
                .retain(|(candidate, _)| *candidate != child);
            ceiling_evicted || stash.candidates.is_empty()
        } else {
            false
        };
        if empty_stash {
            self.remove_eviction_stash(parent);
        }
    }

    pub fn remove_path(&mut self, path: &Path) -> bool {
        self.try_remove_path(path).unwrap_or(false)
    }

    pub fn try_remove_path(&mut self, path: &Path) -> Result<bool, ModelError> {
        let Some(id) = self.find_path(path) else {
            return Ok(false);
        };
        if id == self.root {
            return Ok(false);
        }
        let mut removed = Vec::new();
        self.collect_subtree_ids(id, &mut removed);
        self.remove_nodes_with_accounting(removed).map(|_| true)
    }

    pub fn mark_path_uncertain(&mut self, path: &Path, reason: UnscannedReason) -> bool {
        let Some(id) = self.find_path(path) else {
            return false;
        };
        if let Some(node) = self.node_mut(id) {
            node.state = NodeState::Uncertain;
            node.unscanned_reason = Some(reason);
            node.metrics.allocated_bytes.upper = None;
            node.metrics.reclaimable_bytes.upper = None;
        }
        self.rebuild_metrics();
        true
    }

    pub fn rebuild(&mut self) {
        self.rebuild_metrics();
    }

    #[allow(
        clippy::too_many_lines,
        reason = "Focused replacement is deliberately a single preflight-and-commit transaction so failures cannot partially mutate the live arena."
    )]
    pub fn replace_subtree_from(
        &mut self,
        target: NodeId,
        mut staging: Arena,
    ) -> Result<(), ModelError> {
        let target_path = self.path_for(target).ok_or_else(|| {
            ModelError::Invariant("focused rescan target disappeared".to_string())
        })?;
        if staging.root_path != target_path {
            return Err(ModelError::InvalidPath(
                staging.root_path.to_string_lossy().into_owned(),
            ));
        }
        let (target_kind, target_parent) = self
            .node(target)
            .map(|node| (node.kind, node.parent))
            .ok_or_else(|| {
            ModelError::Invariant("focused rescan target disappeared".to_string())
        })?;
        if target != self.root
            && target_kind != NodeKind::Directory
            && target_kind != NodeKind::Synthetic(SyntheticKind::Aggregate)
        {
            return Err(ModelError::Invariant(
                "focused rescan target is not a directory".to_string(),
            ));
        }
        let staged_root = staging
            .node(staging.root)
            .ok_or_else(|| ModelError::Invariant("focused rescan root disappeared".to_string()))?;
        let staged_state = staged_root.state;
        let staged_reason = staged_root.unscanned_reason.clone();
        let staged_children = staged_root.children.clone();
        let staged_modified_nanos = staged_root.snapshot.modified_nanos;
        let staged_identity = staged_root.snapshot.identity.clone();
        let staged_retained_children = staging
            .retained_child_counts
            .get(staging.root.index())
            .copied()
            .ok_or_else(|| {
                ModelError::Invariant(
                    "focused rescan retained count sidecar disappeared".to_string(),
                )
            })?;

        let mut removed = Vec::new();
        for child in self.children(target).to_vec() {
            self.collect_subtree_ids(child, &mut removed);
        }
        removed.sort_unstable();
        let shared = self.shared_nodes_outside(target, &removed);
        let released = removed
            .iter()
            .chain(shared.iter())
            .filter_map(|id| self.node(*id))
            .fold(0_usize, |total, node| {
                total.saturating_add(released_node_bytes(node))
            });
        let (planned_budget, reused_slots) =
            self.plan_staged_nodes(&mut staging, target, &removed, &shared, released)?;
        remap_staged_nodes(&mut staging, target)?;
        let mut identities = self.rebuild_focused_identities(target, &removed)?;
        merge_staged_identities(&mut identities, &mut staging, target)?;
        identities.visit_records(|_, _| Ok(()))?;
        let identity_scratch = IdentityStore::new_with_temporary_storage(
            self.identities.memory_limit(),
            &self.temporary_storage,
        )?;
        let children = staged_children
            .into_iter()
            .map(|child| staged_live_id(&staging.nodes, staging.root, child, target))
            .collect::<Result<Vec<_>, _>>()?;

        self.remove_reusable_nodes(removed);
        self.remove_reusable_nodes(shared);
        self.remove_untracked_metrics(target);
        let mut consumed_reused_slots = 0_usize;
        for index in 1..staging.nodes.len() {
            let Some(node) = staging.nodes[index].take() else {
                continue;
            };
            let retained_children = staging
                .retained_child_counts
                .get(index)
                .copied()
                .ok_or_else(|| {
                    ModelError::Invariant(
                        "focused rescan staged retained count sidecar disappeared".to_string(),
                    )
                })?;
            let untracked = staging.untracked_metrics.remove(&node.id);
            if let Some(untracked) = untracked {
                debug_assert!(!self.untracked_metrics.contains_key(&node.id));
                self.untracked_metrics.insert(node.id, untracked);
            }
            if consumed_reused_slots < reused_slots {
                let reused = self
                    .free_nodes
                    .pop()
                    .expect("preflighted reusable node slot should remain available");
                debug_assert_eq!(reused, node.id);
                consumed_reused_slots = consumed_reused_slots.saturating_add(1);
            }
            let parent = node
                .parent
                .expect("preflighted focused rescan child should have a parent");
            self.lookup.insert((parent, node.name.clone()), node.id);
            self.insert_planned_node(node.id, node, retained_children)?;
        }
        debug_assert_eq!(consumed_reused_slots, reused_slots);
        let target_was_aggregate = target_kind == NodeKind::Synthetic(SyntheticKind::Aggregate);
        let replacement_kind = if target == self.root {
            NodeKind::Root
        } else {
            NodeKind::Directory
        };
        if target_was_aggregate && let Some(parent) = target_parent {
            self.increment_retained_child_count(parent);
        }
        let target_node = self
            .node_mut(target)
            .expect("preflighted focused rescan target should remain available");
        target_node.kind = replacement_kind;
        target_node.state = if staged_state == NodeState::Scanning {
            NodeState::Complete
        } else {
            staged_state
        };
        target_node.children = children;
        target_node.metrics = NodeMetrics::default();
        target_node.snapshot.kind = replacement_kind;
        target_node.unscanned_reason = staged_reason;
        target_node.snapshot.modified_nanos = staged_modified_nanos;
        if let Some(identity) = staged_identity {
            target_node.snapshot.identity = Some(identity);
        }
        self.set_retained_child_count(target, staged_retained_children);

        self.clear_eviction_stashes();
        self.budget = planned_budget;
        self.clear_spare_child_slots(target);
        self.identities = identities;
        self.prepare_identity_metrics();
        self.rebuild_identity_metrics(identity_scratch)?;
        Ok(())
    }

    fn shared_nodes_outside(&self, target: NodeId, removed: &[NodeId]) -> Vec<NodeId> {
        self.nodes
            .iter()
            .filter_map(Option::as_ref)
            .filter(|node| {
                node.kind == NodeKind::Synthetic(SyntheticKind::Shared)
                    && node.id != target
                    && removed.binary_search(&node.id).is_err()
            })
            .map(|node| node.id)
            .collect()
    }

    #[allow(
        clippy::too_many_lines,
        reason = "focused graft preflight keeps all budget transitions transactional"
    )]
    fn plan_staged_nodes(
        &self,
        staging: &mut Arena,
        target: NodeId,
        removed: &[NodeId],
        shared: &[NodeId],
        released: usize,
    ) -> Result<(MemoryBudget, usize), ModelError> {
        let mut budget = self.budget.clone();
        budget.release(released);
        let released_untracked = removed
            .iter()
            .chain(shared.iter())
            .filter(|id| self.untracked_metrics.contains_key(id))
            .count()
            .saturating_add(usize::from(self.untracked_metrics.contains_key(&target)))
            .saturating_mul(UNTRACKED_METRICS_OVERHEAD);
        budget.release(released_untracked);
        let released_stashes = self
            .eviction_stash
            .len()
            .saturating_mul(EVICTION_STASH_ALLOCATION);
        budget.release(released_stashes);
        let staged_untracked = staging
            .untracked_metrics
            .len()
            .saturating_mul(UNTRACKED_METRICS_OVERHEAD);
        budget.reserve(staged_untracked)?;
        let staged_node_count = staging
            .nodes
            .iter()
            .skip(1)
            .filter_map(Option::as_ref)
            .count();
        let reusable_ids = removed
            .iter()
            .chain(shared.iter())
            .copied()
            .collect::<HashSet<_>>();
        let released_internal_edges = removed
            .iter()
            .chain(shared.iter())
            .filter(|id| {
                self.node(**id)
                    .and_then(|node| node.parent)
                    .is_some_and(|parent| reusable_ids.contains(&parent))
            })
            .count();
        let released_spare_child_slots = removed
            .iter()
            .chain(shared.iter())
            .map(|id| self.spare_child_slot_count(*id))
            .fold(0_usize, usize::saturating_add);
        let released_target_child_slots = self
            .children(target)
            .len()
            .saturating_add(self.spare_child_slot_count(target));
        let removed_reusable_slots = shared.len().saturating_add(removed.len());
        let consumed_free_ids = staged_node_count
            .saturating_sub(removed_reusable_slots)
            .min(self.free_nodes.len());
        let remaining_removed_ids = removed_reusable_slots.saturating_sub(staged_node_count);
        budget.release(released_spare_child_slots.saturating_mul(size_of::<NodeId>()));
        budget.release(released_target_child_slots.saturating_mul(size_of::<NodeId>()));
        budget.release(released_internal_edges.saturating_mul(size_of::<NodeId>()));
        budget.release(consumed_free_ids.saturating_mul(size_of::<NodeId>()));
        budget.reserve(remaining_removed_ids.saturating_mul(size_of::<NodeId>()))?;
        budget.reserve(staged_node_count.saturating_mul(size_of::<NodeId>()))?;
        let mut shared_ids = shared.iter().copied();
        let mut removed_ids = removed.iter().copied();
        let mut free_ids = self.free_nodes.iter().rev().copied();
        let mut next_append = self.nodes.len();
        let mut reused_slots = 0_usize;
        let mut id_remaps = Vec::with_capacity(staging.nodes.len());
        for node in staging.nodes.iter_mut().skip(1).filter_map(Option::as_mut) {
            let reusable = shared_ids
                .next()
                .or_else(|| removed_ids.next())
                .or_else(|| free_ids.next());
            let bytes = estimate_node(&node.name);
            let bytes = if reusable.is_some() {
                bytes
                    .saturating_sub(NODE_SLOT_BYTES)
                    .saturating_sub(RETAINED_CHILD_SLOT_BYTES)
                    .saturating_sub(SPARE_CHILD_SLOT_BYTES)
            } else {
                bytes
            };
            budget.reserve(bytes)?;
            let old_id = node.id;
            let new_id = if let Some(id) = reusable {
                reused_slots = reused_slots.saturating_add(1);
                id
            } else {
                let id = u32::try_from(next_append).map(NodeId).map_err(|_| {
                    ModelError::MemoryExhausted {
                        required: usize::MAX,
                        limit: budget.model_limit(),
                    }
                })?;
                next_append = next_append
                    .checked_add(1)
                    .ok_or(ModelError::MemoryExhausted {
                        required: usize::MAX,
                        limit: budget.model_limit(),
                    })?;
                id
            };
            node.id = new_id;
            id_remaps.push((old_id, new_id));
        }
        let mut remapped_untracked = HashMap::with_capacity(staging.untracked_metrics.len());
        for (old_id, new_id) in id_remaps {
            if let Some(metrics) = staging.untracked_metrics.remove(&old_id) {
                remapped_untracked.insert(new_id, metrics);
            }
        }
        staging.untracked_metrics.extend(remapped_untracked);
        Ok((budget, reused_slots))
    }

    fn insert_planned_node(
        &mut self,
        id: NodeId,
        node: Node,
        retained_children: u32,
    ) -> Result<(), ModelError> {
        if id.index() < self.nodes.len() {
            if self.retained_child_counts.get(id.index()).is_none()
                || self.spare_child_slots.get(id.index()).is_none()
            {
                return Err(ModelError::Invariant(
                    "reusable child sidecar disappeared".to_string(),
                ));
            }
            let slot = self.nodes.get_mut(id.index()).ok_or_else(|| {
                ModelError::Invariant("reusable node slot disappeared".to_string())
            })?;
            if slot.is_some() {
                return Err(ModelError::Invariant(
                    "reusable node slot was occupied".to_string(),
                ));
            }
            *slot = Some(node);
            self.retained_child_counts[id.index()] = retained_children;
            self.spare_child_slots[id.index()] = 0;
        } else {
            if id.index() != self.nodes.len() {
                return Err(ModelError::Invariant(
                    "planned node ID did not match the next arena slot".to_string(),
                ));
            }
            if self.retained_child_counts.len() != self.nodes.len()
                || self.spare_child_slots.len() != self.nodes.len()
            {
                return Err(ModelError::Invariant(
                    "child sidecars did not match arena slots".to_string(),
                ));
            }
            self.nodes.push(Some(node));
            self.retained_child_counts.push(retained_children);
            self.spare_child_slots.push(0);
        }
        Ok(())
    }

    fn rebuild_focused_identities(
        &mut self,
        target: NodeId,
        removed: &[NodeId],
    ) -> Result<IdentityStore, ModelError> {
        let identity_limit = self.identities.memory_limit().min(self.budget.headroom());
        let mut rebuilt =
            IdentityStore::new_with_temporary_storage(identity_limit, &self.temporary_storage)?;
        self.identities.visit_records(|file_id, record| {
            if let Some(record) = remove_replaced_participants(record, target, removed) {
                merge_identity_record(&mut rebuilt, &file_id, record)?;
            }
            Ok(())
        })?;
        Ok(rebuilt)
    }
    fn rebuild_identities_without(
        &mut self,
        removed: &[NodeId],
        link_counts: &HashMap<FileId, Option<u64>>,
    ) -> Result<IdentityStore, ModelError> {
        let identity_limit = self.identities.memory_limit().min(self.budget.headroom());
        let mut rebuilt =
            IdentityStore::new_with_temporary_storage(identity_limit, &self.temporary_storage)?;
        self.identities.visit_records(|file_id, record| {
            if let Some(mut record) = remove_deleted_participants(record, removed) {
                if let Some(link_count) = link_counts.get(&file_id) {
                    record.declared_links = *link_count;
                }
                merge_identity_record(&mut rebuilt, &file_id, record)?;
            }
            Ok(())
        })?;
        Ok(rebuilt)
    }

    fn refresh_surviving_link_counts(
        &mut self,
        link_counts: &HashMap<FileId, Option<u64>>,
    ) -> Result<(), ModelError> {
        if link_counts.is_empty() {
            return Ok(());
        }
        let candidates = self
            .nodes
            .iter()
            .filter_map(Option::as_ref)
            .filter_map(|node| {
                node.snapshot
                    .identity
                    .as_ref()
                    .map(|identity| (node.id, identity.file_id))
            })
            .filter(|(_, file_id)| link_counts.contains_key(file_id))
            .collect::<Vec<_>>();
        let mut refreshed = HashMap::new();
        for (id, file_id) in candidates {
            let link_count = if let Some(link_count) = refreshed.get(&file_id) {
                *link_count
            } else {
                let link_count = self
                    .path_for(id)
                    .and_then(|path| {
                        fs::symlink_metadata(&path)
                            .ok()
                            .map(|metadata| (path, metadata))
                    })
                    .and_then(|(path, metadata)| identity_for(&path, &metadata).ok().flatten())
                    .filter(|identity| identity.file_id == file_id)
                    .and_then(|identity| identity.link_count);
                refreshed.insert(file_id, link_count);
                link_count
            };
            if let Some(node) = self.node_mut(id)
                && let Some(identity) = node.snapshot.identity.as_mut()
            {
                identity.link_count = link_count;
            }
        }
        for (file_id, link_count) in refreshed {
            self.identities
                .refresh_declared_links(&file_id, link_count)?;
        }
        Ok(())
    }

    fn prepare_identity_metrics(&mut self) {
        // Metrics are about to be rewritten from scratch, so any recorded
        // eviction order no longer describes the tree.
        self.clear_eviction_stashes();
        for node in self.nodes.iter_mut().filter_map(Option::as_mut) {
            match node.kind {
                NodeKind::File
                    if node.state == NodeState::Complete && node.unscanned_reason.is_none() =>
                {
                    let links = node
                        .snapshot
                        .identity
                        .as_ref()
                        .and_then(|identity| identity.link_count);
                    node.metrics =
                        leaf_metrics(node.snapshot.apparent_bytes, ByteBounds::exact(0), links);
                }
                NodeKind::Link
                    if node.state == NodeState::Complete && node.unscanned_reason.is_none() =>
                {
                    let links = node
                        .snapshot
                        .identity
                        .as_ref()
                        .and_then(|identity| identity.link_count);
                    node.metrics =
                        leaf_metrics(node.snapshot.apparent_bytes, ByteBounds::exact(0), links);
                }
                NodeKind::Synthetic(SyntheticKind::Other | SyntheticKind::Aggregate) => {
                    let previous = node.metrics;
                    let untracked = self
                        .untracked_metrics
                        .get(&node.id)
                        .copied()
                        .unwrap_or_default();
                    node.metrics.allocated_bytes =
                        preserve_unknown_upper(untracked.allocated_bytes, previous.allocated_bytes);
                    node.metrics.reclaimable_bytes = preserve_unknown_upper(
                        untracked.reclaimable_bytes,
                        previous.reclaimable_bytes,
                    );
                }
                NodeKind::File
                | NodeKind::Root
                | NodeKind::Directory
                | NodeKind::Link
                | NodeKind::Synthetic(SyntheticKind::Shared) => {}
            }
        }
    }

    fn rebuild_identity_metrics(&mut self, replacement: IdentityStore) -> Result<(), ModelError> {
        let mut identities = std::mem::replace(&mut self.identities, replacement);
        let duplicate_bytes = self
            .duplicate_identities
            .len()
            .saturating_mul(DUPLICATE_ID_OVERHEAD);
        self.budget.release(duplicate_bytes);
        self.duplicate_identities.clear();
        let result = identities.visit_records(|file_id, record| {
            self.restore_identity_allocation(&record);
            if record.observed_links > 1 {
                self.track_duplicate(file_id);
            }
            Ok(())
        });
        self.identities = identities;
        result?;
        self.rebuild_metrics();
        self.finalize()?;
        Ok(())
    }

    fn restore_identity_allocation(&mut self, record: &IdentityRecord) {
        let Some(id) = record.allocation_node else {
            return;
        };
        let reclaimable =
            leaf_metrics(0, record.allocated_bytes, record.declared_links).reclaimable_bytes;
        if let Some(node) = self.node_mut(id) {
            node.metrics.allocated_bytes.add(record.allocated_bytes);
            node.metrics.reclaimable_bytes.add(reclaimable);
        }
    }

    fn ensure_directory(&mut self, parent: NodeId, name: &OsString) -> Result<NodeId, ModelError> {
        if let Some(id) = self.find_child(parent, name) {
            return Ok(id);
        }

        let name: Arc<OsStr> = Arc::from(name.as_os_str());
        let id = self.allocate_child_id(parent, &name)?;
        let node = Node::new(
            id,
            Some(parent),
            name.clone(),
            NodeKind::Directory,
            NodeState::Scanning,
            EntrySnapshot {
                identity: None,
                kind: NodeKind::Directory,
                apparent_bytes: 0,
                allocated_bytes: None,
                modified_nanos: None,
            },
        );
        self.insert_node(id, node)?;
        self.lookup.insert((parent, name), id);
        self.push_child(parent, id)?;
        self.propagate_descendant(parent, 1);
        Ok(id)
    }

    fn preview_leaf_metrics(
        &self,
        kind: NodeKind,
        apparent: u128,
        allocated: ByteBounds,
        identity: &NativeIdentity,
    ) -> Result<NodeMetrics, ModelError> {
        if kind.is_directory() {
            return Ok(NodeMetrics::default());
        }
        let duplicate =
            identity.link_count != Some(1) && self.identities.get(&identity.file_id)?.is_some();
        Ok(if duplicate {
            leaf_metrics(apparent, ByteBounds::exact(0), identity.link_count)
        } else {
            leaf_metrics(apparent, allocated, identity.link_count)
        })
    }

    fn observe_leaf_metrics(
        &mut self,
        kind: NodeKind,
        apparent: u128,
        allocated: ByteBounds,
        identity: &NativeIdentity,
        node: Option<NodeId>,
        allocation_node: Option<NodeId>,
    ) -> Result<NodeMetrics, ModelError> {
        if kind.is_directory() {
            return Ok(NodeMetrics::default());
        }
        let (is_new, record) = self.identities.observe(
            &identity.file_id,
            identity.link_count,
            allocated,
            node,
            allocation_node,
        )?;
        if !is_new && record.declared_links.is_none() {
            self.mark_identity_reclaimable_unknown(&record);
        }
        Ok(if is_new {
            leaf_metrics(apparent, allocated, identity.link_count)
        } else {
            self.track_duplicate(identity.file_id);
            leaf_metrics(apparent, ByteBounds::exact(0), identity.link_count)
        })
    }
    fn mark_identity_reclaimable_unknown(&mut self, record: &IdentityRecord) {
        let Some(allocation_node) = record.allocation_node else {
            return;
        };
        let changed = self.node_mut(allocation_node).is_some_and(|node| {
            let next = if node.kind.is_synthetic() {
                ByteBounds::unknown()
            } else {
                ByteBounds {
                    lower: 0,
                    upper: record.allocated_bytes.upper,
                }
            };
            if node.metrics.reclaimable_bytes == next {
                false
            } else {
                node.metrics.reclaimable_bytes = next;
                true
            }
        });
        if changed {
            self.rebuild_metrics();
        }
    }

    fn retained_child_count(&self, parent: NodeId) -> usize {
        self.retained_child_counts
            .get(parent.index())
            .copied()
            .map_or(0, |value| {
                usize::try_from(value).expect("retained child count fits usize")
            })
    }

    fn set_retained_child_count(&mut self, parent: NodeId, count: u32) {
        let retained_count = self
            .retained_child_counts
            .get_mut(parent.index())
            .expect("retained child count sidecar missing");
        *retained_count = count;
    }

    fn increment_retained_child_count(&mut self, parent: NodeId) {
        let count = self
            .retained_child_counts
            .get_mut(parent.index())
            .expect("retained child count sidecar missing");
        *count = count.saturating_add(1);
    }

    fn decrement_retained_child_count(&mut self, parent: NodeId) {
        let count = self
            .retained_child_counts
            .get_mut(parent.index())
            .expect("retained child count sidecar missing");
        *count = count.saturating_sub(1);
    }

    fn spare_child_slot_count(&self, parent: NodeId) -> usize {
        self.spare_child_slots
            .get(parent.index())
            .copied()
            .map_or(0, |value| {
                usize::try_from(value).expect("spare child slot count fits usize")
            })
    }

    fn increment_spare_child_slot(&mut self, parent: NodeId) {
        let slots = self
            .spare_child_slots
            .get_mut(parent.index())
            .expect("spare child slot sidecar missing");
        *slots = slots.saturating_add(1);
    }

    fn consume_spare_child_slot(&mut self, parent: NodeId) {
        let slots = self
            .spare_child_slots
            .get_mut(parent.index())
            .expect("spare child slot sidecar missing");
        debug_assert!(*slots > 0, "reserved child slot should remain available");
        *slots = slots.saturating_sub(1);
    }

    fn clear_spare_child_slots(&mut self, parent: NodeId) {
        let slots = self
            .spare_child_slots
            .get_mut(parent.index())
            .expect("spare child slot sidecar missing");
        *slots = 0;
    }

    fn release_spare_child_slots(&mut self, parent: NodeId) {
        let released = self
            .spare_child_slot_count(parent)
            .saturating_mul(size_of::<NodeId>());
        self.clear_spare_child_slots(parent);
        self.budget.release(released);
    }

    /// The child the cap gives up next: the smallest retained entry, in the
    /// order [`Arena::retention_order`] defines.
    ///
    /// Answered from the stash whenever the stash still describes the directory,
    /// so a wide directory does not re-read every child for every entry the
    /// scanner delivers.
    fn smallest_retained_child(&mut self, parent: NodeId) -> Option<NodeId> {
        if let Some(candidate) = self.stashed_victim(parent) {
            return Some(candidate);
        }
        self.refill_eviction_stash(parent)
    }

    /// The stash entry that still describes its node, re-sorting every entry
    /// whose live rank changed since the sweep.
    ///
    /// Entries that grow beyond the cached frontier are no longer useful, but
    /// every remaining candidate still precedes every child the sweep skipped.
    /// Returns [`None`] only when none of those valid candidates remain.
    fn stashed_victim(&mut self, parent: NodeId) -> Option<NodeId> {
        let victim = {
            let nodes = &self.nodes;
            let stash = self.eviction_stash.get_mut(&parent)?;
            let (candidates, ceiling) = (&mut stash.candidates, &stash.ceiling);
            let mut reseat = false;
            let mut index = 0;
            while index < candidates.len() {
                let candidate = candidates[index].0;
                let Some(node) = nodes
                    .get(candidate.index())
                    .and_then(Option::as_ref)
                    .filter(|node| node.parent == Some(parent) && !node.kind.is_synthetic())
                else {
                    candidates.swap_remove(index);
                    reseat = true;
                    continue;
                };
                let current = retention_rank(node.metrics);
                if candidates[index].1 != Some(current) {
                    if ceiling
                        .compare_candidate(current, node.name.as_ref(), node.id)
                        .is_gt()
                    {
                        // A rank tie can still cross the frontier by name or
                        // ID, so rank alone is not enough here.
                        candidates.swap_remove(index);
                        reseat = true;
                        continue;
                    }
                    candidates[index].1 = Some(current);
                    reseat = true;
                }
                index += 1;
            }
            if reseat {
                // Every change is normalized together, keeping the cache
                // sorted before any victim lookup uses its binary order.
                candidates.sort_unstable_by(|(left, _), (right, _)| {
                    retention_order_for_nodes(nodes, *right, *left)
                });
            }
            candidates.last().map(|(candidate, _)| *candidate)
        };
        if victim.is_none() {
            self.remove_eviction_stash(parent);
        }
        victim
    }

    /// Reads the directory once and keeps its smallest children.
    fn refill_eviction_stash(&mut self, parent: NodeId) -> Option<NodeId> {
        #[cfg(test)]
        {
            self.eviction_stash_sweeps = self.eviction_stash_sweeps.saturating_add(1);
        }
        let mut candidates = self
            .children(parent)
            .iter()
            .copied()
            .filter(|id| self.node(*id).is_some_and(|node| !node.kind.is_synthetic()))
            .collect::<Vec<_>>();
        let keep = EVICTION_STASH.min(candidates.len());
        if keep == 0 {
            self.remove_eviction_stash(parent);
            return None;
        }
        // Only the smallest handful are eviction candidates, and only their
        // order among themselves matters: selecting keeps the sweep linear.
        candidates
            .select_nth_unstable_by(keep - 1, |left, right| self.retention_order(*left, *right));
        candidates.truncate(keep);
        candidates.sort_unstable_by(|left, right| self.retention_order(*right, *left));
        let victim = candidates.last().copied();
        let ceiling = candidates.first().and_then(|id| self.retention_key(*id))?;
        self.remove_eviction_stash(parent);
        if self.budget.reserve(EVICTION_STASH_ALLOCATION).is_err() {
            return victim;
        }
        let mut stashed = Vec::with_capacity(EVICTION_STASH);
        stashed.extend(candidates.into_iter().map(|id| {
            let rank = self
                .node(id)
                .map_or((true, 0), |node| retention_rank(node.metrics));
            (id, Some(rank))
        }));
        self.eviction_stash.insert(
            parent,
            EvictionStash {
                candidates: stashed,
                ceiling,
            },
        );
        victim
    }

    /// Records a freshly retained child for the next cache refresh.
    ///
    /// A newly retained child was not part of the sweep, so the next victim
    /// lookup sorts it with the full retention order before selecting a child.
    fn stash_retained_child(&mut self, parent: NodeId, child: NodeId) {
        let full = self
            .eviction_stash
            .get(&parent)
            .is_some_and(|stash| stash.candidates.len() >= EVICTION_STASH);
        if full {
            self.remove_eviction_stash(parent);
            return;
        }
        if let Some(stash) = self.eviction_stash.get_mut(&parent) {
            stash.candidates.push((child, None));
        }
    }

    fn remove_eviction_stash(&mut self, parent: NodeId) {
        if self.eviction_stash.remove(&parent).is_some() {
            self.budget.release(EVICTION_STASH_ALLOCATION);
        }
    }

    /// Drops every stash. Called wherever metrics are rebuilt rather than
    /// accumulated, since a stashed rank only stays meaningful while entries
    /// grow.
    fn clear_eviction_stashes(&mut self) {
        let count = self.eviction_stash.len();
        self.eviction_stash = HashMap::new();
        self.budget
            .release(count.saturating_mul(EVICTION_STASH_ALLOCATION));
    }

    fn retention_key(&self, id: NodeId) -> Option<RetentionKey> {
        self.node(id)
            .filter(|node| !node.kind.is_synthetic())
            .map(|node| RetentionKey {
                rank: retention_rank(node.metrics),
                name: node.name.clone(),
                id: node.id,
            })
    }

    fn retention_order(&self, left: NodeId, right: NodeId) -> std::cmp::Ordering {
        retention_order_for_nodes(&self.nodes, left, right)
    }

    fn candidate_outranks(&self, name: &OsStr, metrics: NodeMetrics, victim: NodeId) -> bool {
        let Some(victim) = self.node(victim) else {
            return true;
        };
        // A parent cannot hold two children with the same name, so an incoming
        // entry needs the rank and name portions of `retention_order` only.
        retention_rank(metrics)
            .cmp(&retention_rank(victim.metrics))
            .then_with(|| victim.name.as_ref().cmp(name))
            .is_gt()
    }

    fn aggregate_child_into_other(
        &mut self,
        child: NodeId,
        other: NodeId,
    ) -> Result<(), ModelError> {
        let (metrics, leaf_identity) = self
            .node(child)
            .map(|node| {
                let identity = if node.children.is_empty() {
                    node.snapshot.identity.clone()
                } else {
                    None
                };
                (node.metrics, identity)
            })
            .ok_or_else(|| ModelError::Invariant("retained child disappeared".to_string()))?;
        let untracked = self.untracked_metrics_for_subtree(child);
        let reserved = self.reserve_untracked_slot(other, untracked)?;
        let mut removed = Vec::new();
        self.collect_subtree_ids(child, &mut removed);
        removed.sort_unstable();
        let remap_result = if let Some(identity) = leaf_identity {
            self.identities
                .remap_nodes_for_identity(&identity.file_id, &removed, other)
        } else {
            self.identities.remap_removed_nodes(&mut removed, other)
        };
        if let Err(error) = remap_result {
            if reserved {
                self.remove_untracked_metrics(other);
            }
            return Err(error);
        }
        self.add_to_other(other, metrics);
        self.insert_untracked_metrics(other, untracked);
        self.remove_reusable_nodes(removed);
        Ok(())
    }

    fn shared_parent(&self, lca: NodeId) -> NodeId {
        self.node(lca).map_or(self.root, |node| {
            if node.kind.is_directory() {
                lca
            } else {
                node.parent.unwrap_or(self.root)
            }
        })
    }

    fn add_shared_node(
        &mut self,
        parent: NodeId,
        record: &IdentityRecord,
    ) -> Result<(), ModelError> {
        let reclaimable = if record
            .declared_links
            .is_some_and(|links| record.observed_links >= links)
        {
            record.allocated_bytes
        } else {
            ByteBounds {
                lower: 0,
                upper: record.allocated_bytes.upper,
            }
        };
        self.add_synthetic(
            parent,
            "Shared",
            SyntheticKind::Shared,
            NodeMetrics {
                apparent_bytes: 0,
                allocated_bytes: record.allocated_bytes,
                reclaimable_bytes: reclaimable,
                descendants: 0,
            },
        )?;
        Ok(())
    }

    fn add_synthetic(
        &mut self,
        parent: NodeId,
        name: &str,
        synthetic: SyntheticKind,
        metrics: NodeMetrics,
    ) -> Result<NodeId, ModelError> {
        let unique_name = if self.find_child(parent, OsStr::new(name)).is_some() {
            format!("{name} ({})", self.nodes.len())
        } else {
            name.to_string()
        };
        let name: Arc<OsStr> = Arc::from(OsStr::new(&unique_name));
        let id = self.allocate_child_id(parent, &name)?;
        let mut node = Node::new(
            id,
            Some(parent),
            name.clone(),
            NodeKind::Synthetic(synthetic),
            NodeState::Aggregated,
            EntrySnapshot {
                identity: None,
                kind: NodeKind::Synthetic(synthetic),
                apparent_bytes: metrics.apparent_bytes,
                allocated_bytes: None,
                modified_nanos: None,
            },
        );
        node.metrics = metrics;
        node.unscanned_reason = Some(UnscannedReason::MemoryAggregation);
        self.insert_node(id, node)?;
        self.lookup.insert((parent, name), id);
        self.push_child(parent, id)?;
        Ok(id)
    }

    fn ensure_other(&mut self, parent: NodeId) -> Result<NodeId, ModelError> {
        // The overflow entry is named "Other" unless a real entry already held
        // that name, so the name index answers for it directly and only the
        // renamed case pays a walk of the directory.
        let by_name = self
            .find_child(parent, OsStr::new("Other"))
            .filter(|id| self.is_other_node(*id));
        if let Some(existing) = by_name {
            return Ok(existing);
        }
        let existing = self
            .children(parent)
            .iter()
            .copied()
            .find(|id| self.is_other_node(*id));
        if let Some(existing) = existing {
            return Ok(existing);
        }
        // Eviction stashes are optional. When the mandatory overflow node needs
        // global budget, release enough cached frontiers regardless of parent.
        let reservation = self.other_child_reservation(parent);
        while self.budget.used().saturating_add(reservation) > self.budget.model_limit() {
            let Some(stash_parent) = self.eviction_stash.keys().next().copied() else {
                break;
            };
            self.remove_eviction_stash(stash_parent);
        }
        self.add_synthetic(
            parent,
            "Other",
            SyntheticKind::Other,
            NodeMetrics::default(),
        )
    }

    fn is_other_node(&self, id: NodeId) -> bool {
        self.node(id)
            .is_some_and(|node| node.kind == NodeKind::Synthetic(SyntheticKind::Other))
    }

    fn other_child_reservation(&self, parent: NodeId) -> usize {
        const OTHER_NAME: &str = "Other";
        let name_bytes = if self.find_child(parent, OsStr::new(OTHER_NAME)).is_some() {
            OTHER_NAME
                .len()
                .saturating_add(3)
                .saturating_add(decimal_digits(self.nodes.len()))
        } else {
            OTHER_NAME.len()
        };
        self.child_reservation_for_name_bytes(parent, name_bytes)
    }

    fn add_to_other(&mut self, other: NodeId, metrics: NodeMetrics) {
        if let Some(node) = self.node_mut(other) {
            node.metrics.add(metrics);
            node.metrics.descendants = node.metrics.descendants.saturating_add(1);
        }
    }
    // A concrete unscanned node keeps its metrics until compaction. Reserving
    // this slot before the hard limit lets compaction re-key it without a new
    // allocation.
    fn reserve_untracked_compaction_slot(
        &mut self,
        id: NodeId,
        metrics: NodeMetrics,
    ) -> Result<bool, ModelError> {
        if id == self.root {
            return Ok(false);
        }
        self.reserve_untracked_slot(
            id,
            UntrackedMetrics {
                allocated_bytes: metrics.allocated_bytes,
                reclaimable_bytes: metrics.reclaimable_bytes,
            },
        )
    }

    fn reserve_untracked_slot(
        &mut self,
        id: NodeId,
        metrics: UntrackedMetrics,
    ) -> Result<bool, ModelError> {
        if metrics.is_zero() || self.untracked_metrics.contains_key(&id) {
            return Ok(false);
        }
        self.reserve_after_releasing_eviction_stashes(UNTRACKED_METRICS_OVERHEAD)?;
        self.untracked_metrics
            .insert(id, UntrackedMetrics::default());
        Ok(true)
    }

    fn insert_untracked_metrics(&mut self, id: NodeId, metrics: UntrackedMetrics) {
        if metrics.is_zero() {
            return;
        }
        debug_assert!(self.untracked_metrics.contains_key(&id));
        self.untracked_metrics
            .entry(id)
            .and_modify(|existing| existing.add(metrics))
            .or_insert(metrics);
    }

    fn add_untracked_to_node(
        &mut self,
        id: NodeId,
        allocated: ByteBounds,
        reclaimable: ByteBounds,
    ) -> Result<(), ModelError> {
        let metrics = UntrackedMetrics {
            allocated_bytes: allocated,
            reclaimable_bytes: reclaimable,
        };
        if metrics.is_zero() {
            return Ok(());
        }
        self.reserve_untracked_slot(id, metrics)?;
        self.insert_untracked_metrics(id, metrics);
        Ok(())
    }

    fn remove_untracked_metrics(&mut self, id: NodeId) {
        if self.untracked_metrics.remove(&id).is_some() {
            self.budget.release(UNTRACKED_METRICS_OVERHEAD);
        }
    }

    fn untracked_metrics_for_subtree(&self, root: NodeId) -> UntrackedMetrics {
        let mut total = UntrackedMetrics::default();
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            let Some(node) = self.node(id) else {
                continue;
            };
            match node.kind {
                NodeKind::Synthetic(SyntheticKind::Other | SyntheticKind::Aggregate) => {
                    if let Some(metrics) = self.untracked_metrics.get(&id) {
                        total.add(*metrics);
                    }
                }
                NodeKind::Synthetic(SyntheticKind::Shared) => {}
                _ if node.children.is_empty() && node.unscanned_reason.is_some() => {
                    total.allocated_bytes.add(node.metrics.allocated_bytes);
                    total.reclaimable_bytes.add(node.metrics.reclaimable_bytes);
                }
                _ => {
                    if node.state == NodeState::Uncertain
                        && node.unscanned_reason.as_ref().is_some_and(|reason| {
                            matches!(
                                reason,
                                UnscannedReason::Metadata(_) | UnscannedReason::Replacement(_)
                            )
                        })
                    {
                        total.allocated_bytes.add(ByteBounds::unknown());
                        total.reclaimable_bytes.add(ByteBounds::unknown());
                    }
                    stack.extend(node.children.iter().copied());
                }
            }
        }
        total
    }

    fn accumulate_other(&mut self, parent: NodeId, other: NodeId, metrics: NodeMetrics) {
        self.add_to_other(other, metrics);
        self.propagate_add(parent, metrics);
        self.propagate_descendant(parent, 1);
    }

    fn accumulate_untracked_other(
        &mut self,
        parent: NodeId,
        other: NodeId,
        metrics: NodeMetrics,
    ) -> Result<(), ModelError> {
        self.add_untracked_to_node(other, metrics.allocated_bytes, metrics.reclaimable_bytes)?;
        self.add_to_other(other, metrics);
        self.propagate_add(parent, metrics);
        self.propagate_descendant(parent, 1);
        Ok(())
    }

    fn rebuild_metrics(&mut self) {
        self.clear_eviction_stashes();
        let mut order = (0..self.nodes.len())
            .filter_map(|index| self.nodes[index].as_ref().map(|node| node.id))
            .collect::<Vec<_>>();
        order.sort_by_key(|id| std::cmp::Reverse(self.depth(*id)));
        for id in &order {
            if let Some(node) = self.node_mut(*id)
                && node.kind.is_directory()
            {
                node.metrics = if node.state == NodeState::Uncertain
                    && matches!(
                        node.unscanned_reason.as_ref(),
                        Some(UnscannedReason::Metadata(_) | UnscannedReason::Replacement(_))
                    ) {
                    NodeMetrics {
                        allocated_bytes: ByteBounds::unknown(),
                        reclaimable_bytes: ByteBounds::unknown(),
                        ..NodeMetrics::default()
                    }
                } else {
                    NodeMetrics::default()
                };
            }
        }
        for id in order {
            let Some(node) = self.node(id) else {
                continue;
            };
            let metrics = node.metrics;
            let parent = node.parent;
            let synthetic = node.kind.is_synthetic();
            if let Some(parent) = parent
                && let Some(parent_node) = self.node_mut(parent)
            {
                if parent_node
                    .unscanned_reason
                    .as_ref()
                    .is_some_and(has_zero_scoped_metrics)
                {
                    parent_node.metrics.descendants = parent_node
                        .metrics
                        .descendants
                        .saturating_add(metrics.descendants);
                } else {
                    parent_node.metrics.add(metrics);
                }
                if !synthetic {
                    parent_node.metrics.descendants =
                        parent_node.metrics.descendants.saturating_add(1);
                }
            }
        }
    }

    fn lowest_common_ancestor(&self, nodes: &[NodeId]) -> Option<NodeId> {
        let first = *nodes.first()?;
        let mut ancestors = self.ancestor_chain(first);
        for node in &nodes[1..] {
            let other = self
                .ancestor_chain(*node)
                .into_iter()
                .collect::<HashSet<_>>();
            ancestors.retain(|ancestor| other.contains(ancestor));
        }
        ancestors.into_iter().next()
    }

    fn ancestor_chain(&self, mut id: NodeId) -> Vec<NodeId> {
        let mut chain = Vec::new();
        loop {
            chain.push(id);
            let Some(parent) = self.node(id).and_then(|node| node.parent) else {
                break;
            };
            id = parent;
        }
        chain
    }

    fn find_path(&self, path: &Path) -> Option<NodeId> {
        let relative = path.strip_prefix(&self.root_path).ok()?;
        let mut current = self.root;
        for component in relative {
            current = self.find_child(current, component)?;
        }
        Some(current)
    }

    fn path_is_aggregated(&self, path: &Path) -> bool {
        let Ok(relative) = path.strip_prefix(&self.root_path) else {
            return false;
        };
        let mut current = self.root;
        for component in relative {
            if self
                .node(current)
                .is_some_and(|node| node.kind.is_synthetic())
            {
                return true;
            }
            let Some(next) = self.find_child(current, component) else {
                return self.children(current).iter().any(|id| {
                    self.node(*id)
                        .is_some_and(|node| node.kind == NodeKind::Synthetic(SyntheticKind::Other))
                });
            };
            current = next;
        }
        false
    }

    fn push_child(&mut self, parent: NodeId, child: NodeId) -> Result<(), ModelError> {
        let (name, retained) = self
            .node(child)
            .map(|node| (node.name.clone(), !node.kind.is_synthetic()))
            .ok_or_else(|| ModelError::Invariant("child node missing".to_string()))?;
        let reuse_spare_slot = self.spare_child_slot_count(parent) > 0;
        // Children are kept in native name order, so the seat is a search rather
        // than a walk: a directory at the cap would otherwise re-read four
        // thousand names to place every entry the scanner delivers.
        let position = self.children(parent).partition_point(|existing| {
            self.node(*existing)
                .is_some_and(|node| node.name.cmp(&name).is_le())
        });
        {
            let parent_node = self
                .node_mut(parent)
                .ok_or_else(|| ModelError::Invariant("parent node missing".to_string()))?;
            parent_node.children.insert(position, child);
        }
        if reuse_spare_slot {
            self.consume_spare_child_slot(parent);
        }
        if retained {
            self.increment_retained_child_count(parent);
            self.stash_retained_child(parent, child);
        }
        Ok(())
    }

    fn propagate_add(&mut self, mut id: NodeId, metrics: NodeMetrics) {
        while let Some(node) = self.node_mut(id) {
            if node
                .unscanned_reason
                .as_ref()
                .is_some_and(has_zero_scoped_metrics)
            {
                break;
            }
            node.metrics.add(metrics);
            let Some(parent) = node.parent else {
                break;
            };
            id = parent;
        }
    }

    fn propagate_descendant(&mut self, mut id: NodeId, count: u64) {
        while let Some(node) = self.node_mut(id) {
            node.metrics.descendants = node.metrics.descendants.saturating_add(count);
            let Some(parent) = node.parent else {
                break;
            };
            id = parent;
        }
    }

    fn track_duplicate(&mut self, file_id: FileId) {
        if self.duplicate_identities.contains(&file_id) {
            return;
        }
        if self
            .reserve_after_releasing_eviction_stashes(DUPLICATE_ID_OVERHEAD)
            .is_ok()
        {
            self.duplicate_identities.insert(file_id);
        }
    }

    fn reserve_node(&mut self, name: &OsStr) -> Result<(), ModelError> {
        self.budget.reserve(estimate_node(name))
    }

    fn child_reservation_for_name_bytes(&self, parent: NodeId, name_bytes: usize) -> usize {
        let node_bytes = estimate_node_bytes(name_bytes);
        let node_bytes = if self.free_nodes.is_empty() {
            node_bytes
        } else {
            node_bytes
                .saturating_sub(NODE_SLOT_BYTES)
                .saturating_sub(RETAINED_CHILD_SLOT_BYTES)
                .saturating_sub(SPARE_CHILD_SLOT_BYTES)
        };
        let child_slot = if self.spare_child_slot_count(parent) == 0 {
            size_of::<NodeId>()
        } else {
            0
        };
        node_bytes.saturating_add(child_slot)
    }

    fn child_reservation(&self, parent: NodeId, name: &OsStr) -> usize {
        self.child_reservation_for_name_bytes(parent, name.as_encoded_bytes().len())
    }

    fn reserve_child(&mut self, parent: NodeId, name: &OsStr) -> Result<(), ModelError> {
        self.reserve_after_releasing_eviction_stashes(self.child_reservation(parent, name))
    }

    fn reserve_after_releasing_eviction_stashes(
        &mut self,
        reservation: usize,
    ) -> Result<(), ModelError> {
        match self.budget.reserve(reservation) {
            Ok(()) => Ok(()),
            Err(error) if self.eviction_stash.is_empty() => Err(error),
            Err(_) => {
                self.clear_eviction_stashes();
                self.budget.reserve(reservation)
            }
        }
    }

    fn allocate_child_id(&mut self, parent: NodeId, name: &OsStr) -> Result<NodeId, ModelError> {
        self.reserve_child(parent, name)?;
        self.next_id()
    }

    fn insert_node(&mut self, id: NodeId, node: Node) -> Result<(), ModelError> {
        if id.index() < self.nodes.len() {
            if self.retained_child_counts.get(id.index()).is_none()
                || self.spare_child_slots.get(id.index()).is_none()
            {
                return Err(ModelError::Invariant(
                    "reusable child sidecar disappeared".to_string(),
                ));
            }
            let slot = self.nodes.get_mut(id.index()).ok_or_else(|| {
                ModelError::Invariant("reusable node slot disappeared".to_string())
            })?;
            if slot.is_some() {
                return Err(ModelError::Invariant(
                    "reusable node slot was occupied".to_string(),
                ));
            }
            *slot = Some(node);
            self.retained_child_counts[id.index()] = 0;
            self.spare_child_slots[id.index()] = 0;
            return Ok(());
        }
        if id.index() != self.nodes.len() {
            return Err(ModelError::Invariant(
                "new node ID did not match the next arena slot".to_string(),
            ));
        }
        if self.retained_child_counts.len() != self.nodes.len()
            || self.spare_child_slots.len() != self.nodes.len()
        {
            return Err(ModelError::Invariant(
                "child sidecars did not match arena slots".to_string(),
            ));
        }
        self.nodes.push(Some(node));
        self.retained_child_counts.push(0);
        self.spare_child_slots.push(0);
        Ok(())
    }

    fn next_id(&mut self) -> Result<NodeId, ModelError> {
        if let Some(id) = self.free_nodes.pop() {
            self.budget.release(size_of::<NodeId>());
            return Ok(id);
        }
        u32::try_from(self.nodes.len())
            .map(NodeId)
            .map_err(|_| ModelError::MemoryExhausted {
                required: usize::MAX,
                limit: self.budget.model_limit(),
            })
    }

    fn depth(&self, mut id: NodeId) -> usize {
        let mut depth = 0;
        while let Some(parent) = self.node(id).and_then(|node| node.parent) {
            depth += 1;
            id = parent;
        }
        depth
    }
}

fn released_node_bytes(node: &Node) -> usize {
    estimate_node(&node.name)
        .saturating_sub(NODE_SLOT_BYTES)
        .saturating_sub(RETAINED_CHILD_SLOT_BYTES)
        .saturating_sub(SPARE_CHILD_SLOT_BYTES)
}

fn staged_live_id(
    nodes: &[Option<Node>],
    stage_root: NodeId,
    id: NodeId,
    target: NodeId,
) -> Result<NodeId, ModelError> {
    if id == stage_root {
        return Ok(target);
    }
    nodes
        .get(id.index())
        .and_then(Option::as_ref)
        .map(|node| node.id)
        .ok_or_else(|| ModelError::Invariant("staged node mapping disappeared".to_string()))
}

fn remap_staged_nodes(staging: &mut Arena, target: NodeId) -> Result<(), ModelError> {
    let stage_root = staging.root;
    for index in 1..staging.nodes.len() {
        let Some(node) = staging.nodes[index].as_ref() else {
            continue;
        };
        let (parent, children) = {
            let parent = node
                .parent
                .map(|id| staged_live_id(&staging.nodes, stage_root, id, target))
                .transpose()?
                .ok_or_else(|| ModelError::Invariant("staged child has no parent".to_string()))?;
            let children = node
                .children
                .iter()
                .copied()
                .map(|id| staged_live_id(&staging.nodes, stage_root, id, target))
                .collect::<Result<Vec<_>, _>>()?;
            (parent, children)
        };
        let node = staging.nodes[index]
            .as_mut()
            .ok_or_else(|| ModelError::Invariant("staged node mapping disappeared".to_string()))?;
        node.parent = Some(parent);
        node.children = children;
    }
    Ok(())
}

fn merge_staged_identities(
    store: &mut IdentityStore,
    staging: &mut Arena,
    target: NodeId,
) -> Result<(), ModelError> {
    let stage_root = staging.root;
    let (nodes, identities) = (&staging.nodes, &mut staging.identities);
    identities.visit_records(|file_id, mut record| {
        remap_staged_record(&mut record, nodes, stage_root, target)?;
        merge_identity_record(store, &file_id, record)
    })
}

fn is_replaced_node(id: NodeId, target: NodeId, removed: &[NodeId]) -> bool {
    id == target || removed.binary_search(&id).is_ok()
}

fn remove_replaced_participants(
    mut record: IdentityRecord,
    target: NodeId,
    removed: &[NodeId],
) -> Option<IdentityRecord> {
    let removed_links = record
        .nodes
        .iter()
        .filter(|(id, _)| is_replaced_node(*id, target, removed))
        .fold(0_u64, |total, (_, links)| total.saturating_add(*links));
    record
        .nodes
        .retain(|(id, _)| !is_replaced_node(*id, target, removed));
    record.observed_links = record.observed_links.saturating_sub(removed_links);
    if record.nodes.is_empty() {
        return None;
    }
    if record
        .allocation_node
        .is_some_and(|id| is_replaced_node(id, target, removed))
    {
        record.allocation_node = record.nodes.first().map(|(id, _)| *id);
    }
    Some(record)
}
fn remove_deleted_participants(
    mut record: IdentityRecord,
    removed: &[NodeId],
) -> Option<IdentityRecord> {
    let removed_links = record
        .nodes
        .iter()
        .filter(|(id, _)| removed.binary_search(id).is_ok())
        .fold(0_u64, |total, (_, links)| total.saturating_add(*links));
    record
        .nodes
        .retain(|(id, _)| removed.binary_search(id).is_err());
    record.observed_links = record.observed_links.saturating_sub(removed_links);
    if record.nodes.is_empty() {
        return None;
    }
    if record
        .allocation_node
        .is_some_and(|id| removed.binary_search(&id).is_ok())
    {
        record.allocation_node = record.nodes.first().map(|(id, _)| *id);
    }
    Some(record)
}

fn remap_staged_record(
    record: &mut IdentityRecord,
    nodes: &[Option<Node>],
    stage_root: NodeId,
    target: NodeId,
) -> Result<(), ModelError> {
    for (id, _) in &mut record.nodes {
        *id = staged_live_id(nodes, stage_root, *id, target)?;
    }
    record.coalesce_nodes();
    if let Some(id) = record.allocation_node {
        record.allocation_node = Some(staged_live_id(nodes, stage_root, id, target)?);
    }
    Ok(())
}

fn merge_identity_record(
    store: &mut IdentityStore,
    file_id: &FileId,
    record: IdentityRecord,
) -> Result<(), ModelError> {
    if let Some(mut existing) = store.get(file_id)? {
        existing.observed_links = existing
            .observed_links
            .saturating_add(record.observed_links);
        existing.declared_links =
            merge_declared_links(existing.declared_links, record.declared_links);
        existing.allocated_bytes =
            conservative_bounds(existing.allocated_bytes, record.allocated_bytes);
        if existing.allocation_node.is_none() {
            existing.allocation_node = record.allocation_node;
        }
        existing.nodes.extend(record.nodes);
        existing.coalesce_nodes();
        store.upsert_record(file_id, &existing)?;
    } else {
        store.upsert_record(file_id, &record)?;
    }
    Ok(())
}

fn conservative_bounds(left: ByteBounds, right: ByteBounds) -> ByteBounds {
    ByteBounds {
        lower: left.lower.min(right.lower),
        upper: match (left.upper, right.upper) {
            (Some(left), Some(right)) => Some(left.max(right)),
            _ => None,
        },
    }
}

fn unscanned_state(reason: &UnscannedReason) -> NodeState {
    match reason {
        UnscannedReason::SymbolicLink => NodeState::Complete,
        UnscannedReason::MemoryAggregation => NodeState::Aggregated,
        UnscannedReason::FilesystemBoundary
        | UnscannedReason::Excluded(_)
        | UnscannedReason::Metadata(_)
        | UnscannedReason::Replacement(_) => NodeState::Uncertain,
    }
}

fn has_zero_scoped_metrics(reason: &UnscannedReason) -> bool {
    matches!(
        reason,
        UnscannedReason::FilesystemBoundary | UnscannedReason::Excluded(_)
    )
}

fn unscanned_metrics(
    apparent: u128,
    allocated: ByteBounds,
    kind: NodeKind,
    reason: &UnscannedReason,
    declared_links: Option<u64>,
) -> NodeMetrics {
    if has_zero_scoped_metrics(reason) {
        return NodeMetrics::default();
    }
    if matches!(reason, UnscannedReason::Replacement(_)) {
        return NodeMetrics {
            apparent_bytes: apparent,
            allocated_bytes: ByteBounds::unknown(),
            reclaimable_bytes: ByteBounds::unknown(),
            descendants: 0,
        };
    }
    let mut metrics = leaf_metrics(apparent, allocated, declared_links);
    if kind.is_directory() && matches!(reason, UnscannedReason::Metadata(_)) {
        metrics.allocated_bytes.upper = None;
        metrics.reclaimable_bytes.upper = None;
    }
    metrics
}

fn preserve_unknown_upper(rebuilt: ByteBounds, previous: ByteBounds) -> ByteBounds {
    ByteBounds {
        lower: rebuilt.lower,
        upper: if rebuilt.upper.is_none() || previous.upper.is_none() {
            None
        } else {
            rebuilt.upper
        },
    }
}

fn compare_retention(
    left_rank: RetentionRank,
    left_name: &OsStr,
    left_id: NodeId,
    right_rank: RetentionRank,
    right_name: &OsStr,
    right_id: NodeId,
) -> std::cmp::Ordering {
    left_rank
        .cmp(&right_rank)
        .then_with(|| right_name.cmp(left_name))
        .then_with(|| right_id.cmp(&left_id))
}

fn retention_order_for_nodes(
    nodes: &[Option<Node>],
    left: NodeId,
    right: NodeId,
) -> std::cmp::Ordering {
    let Some(left) = nodes.get(left.index()).and_then(Option::as_ref) else {
        return std::cmp::Ordering::Less;
    };
    let Some(right) = nodes.get(right.index()).and_then(Option::as_ref) else {
        return std::cmp::Ordering::Greater;
    };
    compare_retention(
        retention_rank(left.metrics),
        left.name.as_ref(),
        left.id,
        retention_rank(right.metrics),
        right.name.as_ref(),
        right.id,
    )
}

fn retention_rank(metrics: NodeMetrics) -> (bool, u128) {
    (
        metrics.allocated_bytes.upper.is_none(),
        metrics.allocated_bytes.lower,
    )
}

fn estimate_node(name: &OsStr) -> usize {
    estimate_node_bytes(name.as_encoded_bytes().len())
}

fn estimate_node_bytes(name_bytes: usize) -> usize {
    NODE_OVERHEAD.saturating_add(name_bytes.saturating_mul(2))
}

fn decimal_digits(mut value: usize) -> usize {
    let mut digits: usize = 1;
    while value >= 10 {
        value /= 10;
        digits = digits.saturating_add(1);
    }
    digits
}

fn leaf_metrics(apparent: u128, allocated: ByteBounds, declared_links: Option<u64>) -> NodeMetrics {
    let reclaimable = if declared_links.is_some_and(|links| links <= 1) {
        allocated
    } else {
        ByteBounds {
            lower: 0,
            upper: allocated.upper,
        }
    };
    NodeMetrics {
        apparent_bytes: apparent,
        allocated_bytes: allocated,
        reclaimable_bytes: reclaimable,
        descendants: 0,
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::fs;

    use super::*;
    use crate::model::MIN_PROCESS_MIB;
    use crate::native_path::identity_for;

    fn test_arena(root: &Path) -> Arena {
        Arena::new(
            root.to_path_buf(),
            MemoryBudget::from_mib(MIN_PROCESS_MIB)
                .expect("minimum model budget should be available"),
        )
        .expect("arena should be created")
    }

    fn add_path(arena: &mut Arena, path: &Path) -> Option<NodeId> {
        let metadata = fs::symlink_metadata(path).expect("fixture metadata should be readable");
        let identity = identity_for(path, &metadata)
            .expect("fixture identity should be readable")
            .expect("fixture should not be a symbolic link");
        arena
            .add_entry(path, &metadata, identity)
            .expect("fixture should be added")
    }

    fn actual_retained_children(arena: &Arena, parent: NodeId) -> usize {
        arena
            .children(parent)
            .iter()
            .filter(|child| {
                arena
                    .node(**child)
                    .is_some_and(|node| !node.kind.is_synthetic())
            })
            .count()
    }

    fn metric_snapshot(arena: &Arena) -> Vec<(NodeId, NodeMetrics)> {
        let mut metrics = arena
            .nodes()
            .map(|node| (node.id, node.metrics))
            .collect::<Vec<_>>();
        metrics.sort_by_key(|(id, _)| *id);
        metrics
    }

    #[test]
    fn finalization_and_deletion_rebuild_exact_metrics() {
        let root = tempfile::tempdir().expect("model root should exist");
        let directory = root.path().join("directory");
        fs::create_dir(&directory).expect("fixture directory should be created");
        let file = directory.join("file");
        fs::write(&file, b"payload").expect("fixture file should be written");

        let mut arena = test_arena(root.path());
        let directory_id = add_path(&mut arena, &directory).expect("directory should be retained");
        add_path(&mut arena, &file).expect("file should be retained");
        arena.finalize().expect("model should finalize");

        let root_node = arena.node(arena.root()).expect("root should exist");
        assert_eq!(root_node.metrics.apparent_bytes, 7);
        assert_eq!(root_node.metrics.descendants, 2);
        let directory_node = arena.node(directory_id).expect("directory should exist");
        assert_eq!(directory_node.metrics.apparent_bytes, 7);
        assert_eq!(directory_node.metrics.descendants, 1);

        arena.remove_subtree(directory_id);
        arena.rebuild();
        let root_node = arena.node(arena.root()).expect("root should remain");
        assert_eq!(root_node.metrics, NodeMetrics::default());
    }

    #[test]
    fn placeholder_directory_is_counted_once() {
        let root = tempfile::tempdir().expect("model root should exist");
        let directory = root.path().join("directory");
        fs::create_dir(&directory).expect("fixture directory should be created");
        let file = directory.join("file");
        fs::write(&file, b"x").expect("fixture file should be written");

        let mut arena = test_arena(root.path());
        add_path(&mut arena, &file).expect("file should be retained");
        add_path(&mut arena, &directory).expect("directory metadata should update its placeholder");
        arena.finalize().expect("model should finalize");

        assert_eq!(
            arena
                .node(arena.root())
                .expect("root should exist")
                .metrics
                .descendants,
            2
        );
    }

    #[test]
    fn cold_compaction_preserves_metrics_and_pinned_paths() {
        let root = tempfile::tempdir().expect("model root should exist");
        let cold = root.path().join("cold");
        let pinned = root.path().join("pinned");
        fs::create_dir(&cold).expect("cold directory should be created");
        fs::create_dir(&pinned).expect("pinned directory should be created");
        let cold_first = cold.join("first");
        let cold_second = cold.join("second");
        let pinned_file = pinned.join("file");
        fs::write(&cold_first, b"a").expect("cold fixture should be written");
        fs::write(&cold_second, b"bc").expect("cold fixture should be written");
        fs::write(&pinned_file, b"def").expect("pinned fixture should be written");

        let mut arena = test_arena(root.path());
        let cold_id = add_path(&mut arena, &cold).expect("cold directory should be retained");
        let pinned_id = add_path(&mut arena, &pinned).expect("pinned directory should be retained");
        add_path(&mut arena, &cold_first).expect("cold file should be retained");
        add_path(&mut arena, &cold_second).expect("cold file should be retained");
        add_path(&mut arena, &pinned_file).expect("pinned file should be retained");
        arena.finalize().expect("model should finalize");
        let before_metrics = arena.node(arena.root()).expect("root should exist").metrics;
        let before_memory = arena.memory_used();
        let pinned_nodes = HashSet::from([arena.root(), pinned_id]);

        assert!(
            arena
                .aggregate_cold_subtree(&pinned_nodes)
                .expect("cold subtree should compact")
        );

        assert_eq!(
            arena.node(arena.root()).expect("root should exist").metrics,
            before_metrics
        );
        assert!(arena.memory_used() < before_memory);
        let cold_node = arena.node(cold_id).expect("cold aggregate should remain");
        assert_eq!(
            cold_node.kind,
            NodeKind::Synthetic(SyntheticKind::Aggregate)
        );
        assert_eq!(cold_node.state, NodeState::Aggregated);
        assert!(cold_node.children.is_empty());
        arena
            .complete_directory(&cold.join("nested"), None)
            .expect("completion below a compacted subtree should be harmless");
        assert_eq!(arena.children(pinned_id).len(), 1);
    }

    #[test]
    fn repeated_cold_compaction_keeps_metrics_exact_at_the_model_limit() {
        let root = tempfile::tempdir().expect("model root should exist");
        let cold_exact = root.path().join("cold-exact");
        let cold_unknown = root.path().join("cold-unknown");
        let pinned = root.path().join("pinned");
        let exact_leaf = cold_exact.join("exact");
        let unknown_leaf = cold_unknown.join("unknown");
        let pinned_leaf = pinned.join("pinned");
        for directory in [&cold_exact, &cold_unknown, &pinned] {
            fs::create_dir(directory).expect("fixture directory should be created");
        }
        fs::write(&exact_leaf, b"exact").expect("exact fixture should be written");
        fs::write(&unknown_leaf, b"unknown").expect("unknown fixture should be written");
        fs::write(&pinned_leaf, b"pinned").expect("pinned fixture should be written");

        let mut arena = test_arena(root.path());
        let root_id = arena.root();
        let cold_exact_id = add_path(&mut arena, &cold_exact).expect("cold exact should retain");
        add_path(&mut arena, &exact_leaf).expect("exact leaf should retain");
        let cold_unknown_id =
            add_path(&mut arena, &cold_unknown).expect("cold unknown should retain");
        add_path(&mut arena, &unknown_leaf).expect("unknown leaf should retain");
        arena
            .record_unscanned(
                &unknown_leaf,
                UnscannedReason::Metadata("fixture metadata unavailable".to_string()),
            )
            .expect("unknown leaf should retain its bound");
        let pinned_id = add_path(&mut arena, &pinned).expect("pinned directory should retain");
        add_path(&mut arena, &pinned_leaf).expect("pinned leaf should retain");
        arena.finalize().expect("fixture model should finalize");

        let expected_root_metrics = arena.node(root_id).expect("root should exist").metrics;
        assert_eq!(expected_root_metrics.apparent_bytes, 18);
        assert_eq!(expected_root_metrics.allocated_bytes.upper, None);
        assert_eq!(expected_root_metrics.reclaimable_bytes.upper, None);
        let pinned_path = arena
            .path_ids(&pinned_leaf)
            .expect("pinned path should be retained");
        let pinned_nodes = HashSet::from([root_id, pinned_id]);

        for candidate in [cold_exact_id, cold_unknown_id] {
            arena
                .consume_remaining_budget_for_test()
                .expect("fixture should reach the model limit");
            assert!(
                arena
                    .aggregate_cold_subtree(&pinned_nodes)
                    .expect("cold subtree should compact at the model limit")
            );
            let node = arena.node(candidate).expect("aggregate should remain");
            assert_eq!(node.kind, NodeKind::Synthetic(SyntheticKind::Aggregate));
            assert_eq!(
                arena.node(root_id).expect("root should remain").metrics,
                expected_root_metrics,
                "compaction must preserve exact and unknown ancestor accounting"
            );
            assert_eq!(
                arena.path_ids(&pinned_leaf),
                Some(pinned_path.clone()),
                "pinned navigation must remain intact"
            );
            assert!(
                arena.memory_used() <= arena.memory_limit(),
                "compaction must not exceed the hard model budget"
            );
            if candidate == cold_unknown_id {
                assert_eq!(node.metrics.allocated_bytes.upper, None);
                assert!(arena.untracked_metrics.contains_key(&candidate));
            }

            let incremental_metrics = metric_snapshot(&arena);
            arena.rebuild();
            assert_eq!(
                metric_snapshot(&arena),
                incremental_metrics,
                "compaction metrics must already match a full rebuild"
            );
        }

        assert!(
            !arena
                .aggregate_cold_subtree(&pinned_nodes)
                .expect("only the deterministic cold candidates should compact")
        );
    }

    #[test]
    fn cold_compaction_drops_discarded_child_buffer() {
        let root = tempfile::tempdir().expect("model root should exist");
        let cold = root.path().join("cold");
        let first = cold.join("first");
        let second = cold.join("second");
        fs::create_dir(&cold).expect("cold directory should be created");
        fs::write(&first, b"a").expect("first child should be written");
        fs::write(&second, b"b").expect("second child should be written");

        let mut arena = test_arena(root.path());
        let cold_id = add_path(&mut arena, &cold).expect("cold directory should be retained");
        add_path(&mut arena, &first).expect("first child should be retained");
        add_path(&mut arena, &second).expect("second child should be retained");
        assert!(
            arena
                .node(cold_id)
                .expect("cold directory should exist")
                .children
                .capacity()
                >= 2,
            "fixture should allocate a child buffer"
        );

        assert!(
            arena
                .aggregate_cold_subtree(&HashSet::from([arena.root()]))
                .expect("cold subtree should compact")
        );

        assert_eq!(
            arena
                .node(cold_id)
                .expect("cold aggregate should remain")
                .children
                .capacity(),
            0,
            "an Aggregate must not retain discarded child-vector capacity"
        );
    }

    #[test]
    fn cold_compaction_decrements_parent_retained_children() {
        let root = tempfile::tempdir().expect("model root should exist");
        let cold = root.path().join("cold");
        let cold_child = cold.join("child");
        let survivor = root.path().join("survivor");
        fs::create_dir(&cold).expect("cold directory should be created");
        fs::write(&cold_child, b"child").expect("cold child should be written");
        fs::write(&survivor, b"survivor").expect("surviving file should be written");

        let mut arena = test_arena(root.path());
        let cold_id = add_path(&mut arena, &cold).expect("cold directory should be retained");
        add_path(&mut arena, &cold_child).expect("cold child should be retained");
        add_path(&mut arena, &survivor).expect("survivor should be retained");
        arena.finalize().expect("model should finalize");
        assert_eq!(arena.retained_child_count(arena.root()), 2);

        assert!(
            arena
                .aggregate_cold_subtree(&HashSet::from([arena.root()]))
                .expect("cold subtree should compact")
        );

        assert_eq!(
            arena.retained_child_count(arena.root()),
            actual_retained_children(&arena, arena.root())
        );
        assert_eq!(arena.retained_child_count(arena.root()), 1);
        assert_eq!(
            arena
                .node(cold_id)
                .expect("cold aggregate should remain")
                .kind,
            NodeKind::Synthetic(SyntheticKind::Aggregate)
        );
    }

    #[test]
    fn permanent_removal_keeps_surviving_child_capacity_and_sidecar_reserved() {
        let root = tempfile::tempdir().expect("model root should exist");
        let directory = root.path().join("directory");
        let child = directory.join("child");
        fs::create_dir(&directory).expect("fixture directory should be created");
        fs::create_dir(&child).expect("fixture child should be created");

        let mut arena = test_arena(root.path());
        let directory_id = add_path(&mut arena, &directory).expect("directory should be retained");
        let child_id = add_path(&mut arena, &child).expect("child should be retained");
        assert_eq!(arena.retained_child_count(directory_id), 1);
        let sidecar_len = arena.retained_child_counts.len();
        let spare_sidecar_len = arena.spare_child_slots.len();
        let removed = HashSet::from([directory_id, child_id]);
        let expected_reclaimed = [directory_id, child_id]
            .into_iter()
            .map(|id| {
                let node = arena.node(id).expect("removed node should exist");
                let released_child_capacity =
                    if node.parent.is_some_and(|parent| removed.contains(&parent)) {
                        size_of::<NodeId>()
                    } else {
                        0
                    };
                estimate_node(&node.name)
                    .saturating_sub(NODE_SLOT_BYTES)
                    .saturating_sub(RETAINED_CHILD_SLOT_BYTES)
                    .saturating_sub(SPARE_CHILD_SLOT_BYTES)
                    .saturating_add(released_child_capacity)
            })
            .sum::<usize>();
        let before_removal = arena.memory_used();

        arena.remove_nodes(vec![directory_id, child_id]);

        assert_eq!(arena.retained_child_counts.len(), sidecar_len);
        assert_eq!(arena.spare_child_slots.len(), spare_sidecar_len);
        assert_eq!(arena.retained_child_counts[directory_id.index()], 0);
        assert_eq!(arena.retained_child_count(arena.root()), 0);
        assert_eq!(
            arena.memory_used(),
            before_removal.saturating_sub(expected_reclaimed),
            "surviving child-buffer and sidecar slots must remain budgeted after permanent removal"
        );
    }

    #[test]
    fn permanent_removal_keeps_surviving_parent_child_buffer_budgeted() {
        let root = tempfile::tempdir().expect("model root should exist");
        let child = root.path().join("child");
        fs::write(&child, b"child").expect("fixture child should be written");

        let mut arena = test_arena(root.path());
        let child_id = add_path(&mut arena, &child).expect("child should be retained");
        let root_id = arena.root();
        let child_capacity = arena
            .node(root_id)
            .expect("root should exist")
            .children
            .capacity();
        assert!(child_capacity > 0, "fixture should allocate a child buffer");
        let node = arena.node(child_id).expect("child should exist");
        let expected_reclaimed = estimate_node(&node.name)
            .saturating_sub(NODE_SLOT_BYTES)
            .saturating_sub(RETAINED_CHILD_SLOT_BYTES)
            .saturating_sub(SPARE_CHILD_SLOT_BYTES);
        let before_removal = arena.memory_used();

        arena.remove_nodes(vec![child_id]);

        assert_eq!(
            arena
                .node(root_id)
                .expect("root should exist")
                .children
                .capacity(),
            child_capacity,
            "Vec::remove must retain the surviving parent buffer"
        );
        assert_eq!(
            arena.memory_used(),
            before_removal.saturating_sub(expected_reclaimed),
            "the retained parent buffer must stay charged"
        );
    }

    #[test]
    fn reusable_removal_fallback_keeps_free_id_and_parent_capacity_charged() {
        let root = tempfile::tempdir().expect("model root should exist");
        let removed = root.path().join("removed");
        let incoming = root.path().join("incoming-directory-with-a-longer-name");
        fs::create_dir(&removed).expect("removed directory should be created");
        fs::create_dir(&incoming).expect("incoming directory should be created");

        let mut arena = test_arena(root.path());
        let root_id = arena.root();
        let removed_id = add_path(&mut arena, &removed).expect("removed node should be retained");
        arena
            .add_synthetic(
                root_id,
                "Other",
                SyntheticKind::Other,
                NodeMetrics::default(),
            )
            .expect("overflow node should be retained");
        let removed_node = arena.node(removed_id).expect("removed node should exist");
        let released_node_bytes = estimate_node(&removed_node.name)
            .saturating_sub(NODE_SLOT_BYTES)
            .saturating_sub(RETAINED_CHILD_SLOT_BYTES)
            .saturating_sub(SPARE_CHILD_SLOT_BYTES);
        let before_removal = arena.memory_used();

        arena.remove_reusable_nodes(vec![removed_id]);

        assert_eq!(
            arena.memory_used(),
            before_removal
                .saturating_sub(released_node_bytes)
                .saturating_add(size_of::<NodeId>()),
            "the free-ID entry must be charged separately from the retained parent slot"
        );
        assert_eq!(arena.free_nodes, vec![removed_id]);
        assert_eq!(arena.spare_child_slot_count(root_id), 1);

        let reservation = arena.child_reservation(
            root_id,
            incoming
                .file_name()
                .expect("incoming path should have a filename"),
        );
        let remaining = reservation.saturating_sub(1);
        let filler = arena
            .memory_limit()
            .saturating_sub(arena.memory_used())
            .checked_sub(remaining)
            .expect("fixture budget should leave one byte too little for the incoming node");
        arena
            .budget
            .reserve(filler)
            .expect("fixture should consume the remaining budget");
        let before_fallback = arena.memory_used();

        assert!(
            add_path(&mut arena, &incoming).is_none(),
            "a failed reservation should aggregate into the existing Other node"
        );
        assert_eq!(arena.memory_used(), before_fallback);
        assert_eq!(arena.free_nodes, vec![removed_id]);
        assert_eq!(arena.spare_child_slot_count(root_id), 1);
    }

    #[test]
    fn permanent_delete_reinsert_consumes_parent_spare_capacity_once() {
        let root = tempfile::tempdir().expect("model root should exist");
        let first = root.path().join("first");
        let second = root.path().join("second");
        let third = root.path().join("third");
        for path in [&first, &second, &third] {
            fs::create_dir(path).expect("fixture directory should be created");
        }

        let mut arena = test_arena(root.path());
        let root_id = arena.root();
        let first_id = add_path(&mut arena, &first).expect("first node should be retained");
        arena.remove_nodes(vec![first_id]);
        assert_eq!(arena.spare_child_slot_count(root_id), 1);

        let second_reservation = arena.child_reservation(
            root_id,
            second
                .file_name()
                .expect("second path should have a filename"),
        );
        assert_eq!(
            second_reservation,
            estimate_node(
                second
                    .file_name()
                    .expect("second path should have a filename")
            ),
            "the permanent deletion already paid for the parent child slot"
        );
        let before_second = arena.memory_used();
        let second_id = add_path(&mut arena, &second).expect("second node should be retained");
        assert_ne!(
            second_id, first_id,
            "permanent deletion must not reuse the node ID"
        );
        assert_eq!(
            arena.memory_used(),
            before_second.saturating_add(second_reservation)
        );
        assert_eq!(arena.spare_child_slot_count(root_id), 0);

        arena.remove_nodes(vec![second_id]);
        assert_eq!(arena.spare_child_slot_count(root_id), 1);
        let third_reservation = arena.child_reservation(
            root_id,
            third
                .file_name()
                .expect("third path should have a filename"),
        );
        assert_eq!(
            third_reservation,
            estimate_node(
                third
                    .file_name()
                    .expect("third path should have a filename")
            ),
            "each reinsert cycle must reuse, not recharge, the retained child capacity"
        );
        let before_third = arena.memory_used();
        add_path(&mut arena, &third).expect("third node should be retained");
        assert_eq!(
            arena.memory_used(),
            before_third.saturating_add(third_reservation)
        );
        assert_eq!(arena.spare_child_slot_count(root_id), 0);
    }

    #[test]
    fn reused_slots_reset_retained_child_sidecar() {
        let root = tempfile::tempdir().expect("model root should exist");
        let old = root.path().join("old");
        let old_child = old.join("child");
        let replacement = root.path().join("replacement");
        let replacement_child = replacement.join("child");
        for path in [&old, &old_child, &replacement, &replacement_child] {
            fs::create_dir_all(path).expect("fixture directory should be created");
        }

        let mut arena = test_arena(root.path());
        let old_id = add_path(&mut arena, &old).expect("old directory should be retained");
        let old_child_id = add_path(&mut arena, &old_child).expect("old child should be retained");
        assert_eq!(arena.retained_child_count(old_id), 1);

        arena.remove_reusable_nodes(vec![old_id, old_child_id]);

        assert_eq!(arena.retained_child_counts[old_id.index()], 0);
        assert_eq!(arena.spare_child_slots[old_id.index()], 0);
        assert_eq!(
            arena.retained_child_count(arena.root()),
            actual_retained_children(&arena, arena.root())
        );
        let replacement_id =
            add_path(&mut arena, &replacement).expect("replacement directory should be retained");
        assert_eq!(replacement_id, old_id);
        assert_eq!(arena.retained_child_count(replacement_id), 0);
        add_path(&mut arena, &replacement_child).expect("replacement child should be retained");
        assert_eq!(
            arena.retained_child_count(replacement_id),
            actual_retained_children(&arena, replacement_id)
        );
    }

    #[test]
    fn staged_node_remap_tolerates_unreferenced_holes() {
        let root = tempfile::tempdir().expect("model root should exist");
        let removed = root.path().join("removed");
        let retained = root.path().join("retained");
        fs::write(&removed, b"removed").expect("removed fixture should be written");
        fs::write(&retained, b"retained").expect("retained fixture should be written");

        let mut staging = test_arena(root.path());
        let removed_id = add_path(&mut staging, &removed).expect("removed node should be added");
        let retained_id = add_path(&mut staging, &retained).expect("retained node should be added");
        staging.remove_nodes_with_reuse(vec![removed_id], true);

        assert!(staging.node(removed_id).is_none());
        assert_eq!(
            staging.retained_child_count(staging.root()),
            actual_retained_children(&staging, staging.root())
        );
        assert!(
            remap_staged_nodes(&mut staging, NodeId(99)).is_ok(),
            "unreferenced staging holes should be ignored"
        );
        assert_eq!(
            staging
                .node(retained_id)
                .expect("retained node should remain")
                .parent,
            Some(NodeId(99))
        );
    }

    #[test]
    fn focused_graft_restores_retained_children_after_aggregate() {
        let root = tempfile::tempdir().expect("model root should exist");
        let target = root.path().join("target");
        let old = target.join("old");
        let first = target.join("first");
        let second = target.join("second");
        fs::create_dir(&target).expect("target directory should be created");
        fs::write(&old, b"old").expect("old child should be written");

        let mut live = test_arena(root.path());
        let target_id = add_path(&mut live, &target).expect("target should be retained");
        add_path(&mut live, &old).expect("old child should be retained");
        for path in [&target, root.path()] {
            live.complete_directory(path, None)
                .expect("live directory should complete");
        }
        live.finalize().expect("live model should finalize");
        assert!(
            live.aggregate_cold_subtree(&HashSet::from([live.root()]))
                .expect("target should compact")
        );
        assert_eq!(live.retained_child_count(live.root()), 0);

        fs::write(&first, b"first").expect("first replacement should be written");
        fs::write(&second, b"second").expect("second replacement should be written");
        let mut staging = test_arena(&target);
        add_path(&mut staging, &first).expect("first replacement should be retained");
        add_path(&mut staging, &second).expect("second replacement should be retained");
        staging
            .complete_directory(&target, None)
            .expect("staging root should complete");

        live.replace_subtree_from(target_id, staging)
            .expect("focused graft should succeed");

        let target_node = live.node(target_id).expect("target should remain");
        assert_eq!(target_node.kind, NodeKind::Directory);
        assert_eq!(
            live.retained_child_count(target_id),
            actual_retained_children(&live, target_id)
        );
        assert_eq!(live.retained_child_count(target_id), 2);
        assert_eq!(
            live.retained_child_count(live.root()),
            actual_retained_children(&live, live.root())
        );
        assert_eq!(live.retained_child_count(live.root()), 1);
    }
    #[test]
    fn compaction_rehomes_hard_link_allocation_before_finalization() {
        let root = tempfile::tempdir().expect("model root should exist");
        let cold = root.path().join("cold");
        fs::create_dir(&cold).expect("cold directory should be created");
        let first = cold.join("first");
        let survivor = root.path().join("survivor");
        fs::write(&first, b"payload").expect("fixture should be written");
        fs::hard_link(&first, &survivor).expect("hard link should be created");

        let mut arena = test_arena(root.path());
        let cold_id = add_path(&mut arena, &cold).expect("cold directory should be retained");
        let first_id = add_path(&mut arena, &first).expect("first link should be retained");
        let survivor_id = add_path(&mut arena, &survivor).expect("survivor should be retained");
        assert!(
            arena
                .aggregate_cold_subtree(&HashSet::from([arena.root()]))
                .expect("cold subtree should compact")
        );
        assert!(arena.node(first_id).is_none());

        arena
            .finalize()
            .expect("hard links should finalize after compaction");

        let shared = arena
            .children(arena.root())
            .iter()
            .filter_map(|id| arena.node(*id))
            .find(|node| node.kind == NodeKind::Synthetic(SyntheticKind::Shared))
            .expect("shared allocation should remain visible at the common ancestor");
        assert_eq!(shared.parent, Some(arena.root()));
        assert_eq!(
            arena
                .node(cold_id)
                .expect("cold aggregate should remain")
                .metrics
                .allocated_bytes,
            ByteBounds::exact(0)
        );
        assert_eq!(
            arena
                .node(survivor_id)
                .expect("survivor should remain")
                .metrics
                .allocated_bytes,
            ByteBounds::exact(0)
        );
        assert_eq!(
            arena
                .node(arena.root())
                .expect("root should remain")
                .metrics
                .allocated_bytes,
            shared.metrics.allocated_bytes
        );
    }
    #[cfg(unix)]
    #[test]
    fn spilled_fanout_hard_links_coalesce_remaps_and_delete_exactly() {
        const COLD_LINKS: u64 = 128;
        let root = tempfile::tempdir().expect("model root should exist");
        let cold = root.path().join("cold");
        fs::create_dir(&cold).expect("cold directory should be created");
        let first = cold.join("link-000");
        fs::write(&first, b"payload").expect("fixture should be written");
        let mut cold_links = vec![first.clone()];
        for index in 1..COLD_LINKS {
            let path = cold.join(format!("link-{index:03}"));
            fs::hard_link(&first, &path).expect("hard link should be created");
            cold_links.push(path);
        }
        let survivor = root.path().join("survivor");
        fs::hard_link(&first, &survivor).expect("hard link should be created");
        let metadata = fs::symlink_metadata(&first).expect("fixture metadata should be readable");
        let file_id = identity_for(&first, &metadata)
            .expect("fixture identity should be readable")
            .expect("fixture should not be a symbolic link")
            .file_id;

        let mut arena = test_arena(root.path());
        arena.identities = IdentityStore::new(1).expect("identity store should initialize");
        let cold_id = add_path(&mut arena, &cold).expect("cold directory should be retained");
        for path in &cold_links {
            add_path(&mut arena, path).expect("cold hard link should be retained");
        }
        let survivor_id = add_path(&mut arena, &survivor).expect("survivor should be retained");
        assert!(arena.identities.is_spilled());
        assert!(
            arena
                .aggregate_cold_subtree(&HashSet::from([arena.root()]))
                .expect("cold subtree should compact")
        );

        let record = arena
            .identities
            .get(&file_id)
            .expect("identity lookup should succeed")
            .expect("identity should remain");
        assert_eq!(record.observed_links, COLD_LINKS + 1);
        assert_eq!(record.nodes, vec![(cold_id, COLD_LINKS), (survivor_id, 1)]);
        assert_eq!(record.allocation_node, Some(cold_id));

        arena
            .finalize()
            .expect("hard links should finalize after compaction");
        let shared = arena
            .children(arena.root())
            .iter()
            .filter_map(|id| arena.node(*id))
            .find(|node| node.kind == NodeKind::Synthetic(SyntheticKind::Shared))
            .expect("shared allocation should remain visible at the common ancestor");
        let shared_allocation = shared.metrics.allocated_bytes;
        assert_eq!(shared.parent, Some(arena.root()));
        assert_eq!(
            arena
                .node(cold_id)
                .expect("cold aggregate should remain")
                .metrics
                .allocated_bytes,
            ByteBounds::exact(0)
        );
        assert_eq!(
            arena
                .node(survivor_id)
                .expect("survivor should remain")
                .metrics
                .allocated_bytes,
            ByteBounds::exact(0)
        );

        assert!(arena.remove_path(&cold));
        let record = arena
            .identities
            .get(&file_id)
            .expect("identity lookup should succeed")
            .expect("surviving identity should remain");
        assert_eq!(record.observed_links, 1);
        assert_eq!(record.nodes, vec![(survivor_id, 1)]);
        assert_eq!(record.allocation_node, Some(survivor_id));
        assert_eq!(
            arena
                .node(survivor_id)
                .expect("survivor should remain")
                .metrics
                .allocated_bytes,
            shared_allocation
        );
        assert_eq!(
            arena
                .node(arena.root())
                .expect("root should remain")
                .metrics
                .allocated_bytes,
            shared_allocation
        );
        assert!(arena.children(arena.root()).iter().all(|id| {
            arena
                .node(*id)
                .is_none_or(|node| node.kind != NodeKind::Synthetic(SyntheticKind::Shared))
        }));
    }

    #[test]
    fn compaction_releases_budget_for_a_failed_insertion_retry() {
        let root = tempfile::tempdir().expect("model root should exist");
        let cold = root.path().join("cold");
        fs::create_dir(&cold).expect("cold directory should be created");
        let first = cold.join("first");
        let second = cold.join("second");
        let pending = root.path().join("pending");
        fs::write(&first, b"a").expect("fixture should be written");
        fs::write(&second, b"b").expect("fixture should be written");
        fs::write(&pending, b"c").expect("fixture should be written");

        let mut arena = test_arena(root.path());
        add_path(&mut arena, &cold).expect("cold directory should be retained");
        add_path(&mut arena, &first).expect("cold file should be retained");
        add_path(&mut arena, &second).expect("cold file should be retained");
        let remaining = arena.memory_limit().saturating_sub(arena.memory_used());
        arena
            .budget
            .reserve(remaining)
            .expect("test should consume the remaining model budget");
        let metadata = fs::symlink_metadata(&pending).expect("pending metadata should exist");
        let identity = identity_for(&pending, &metadata)
            .expect("pending identity should be readable")
            .expect("pending fixture should not be a link");

        assert!(matches!(
            arena.add_entry(&pending, &metadata, identity.clone()),
            Err(ModelError::MemoryExhausted { .. })
        ));
        assert!(
            arena
                .aggregate_cold_subtree(&HashSet::from([arena.root()]))
                .expect("cold subtree should compact")
        );
        assert!(
            arena
                .add_entry(&pending, &metadata, identity)
                .expect("insertion should retry after compaction")
                .is_some()
        );
    }
    #[test]
    fn child_cap_retains_late_larger_entries_and_stable_node_ids() {
        let root = tempfile::tempdir().expect("model root should exist");
        let first = root.path().join("first");
        let middle = root.path().join("middle");
        let late = root.path().join("late");
        fs::write(&first, b"a").expect("small fixture should be written");
        fs::write(&middle, vec![b'm'; 8 * 1024]).expect("middle fixture should be written");
        fs::write(&late, vec![b'l'; 64 * 1024]).expect("late fixture should be written");

        let mut arena = test_arena(root.path());
        arena.max_children_per_directory = 2;
        let first_id = add_path(&mut arena, &first).expect("small entry should be retained");
        let middle_id = add_path(&mut arena, &middle).expect("middle entry should be retained");
        let late_id =
            add_path(&mut arena, &late).expect("late larger entry should replace the smallest");

        assert!(
            arena
                .find_child(arena.root(), OsStr::new("first"))
                .is_none()
        );
        assert_eq!(late_id, first_id);
        assert_eq!(
            arena.find_child(arena.root(), OsStr::new("middle")),
            Some(middle_id)
        );
        assert_eq!(
            arena.find_child(arena.root(), OsStr::new("late")),
            Some(late_id)
        );
        let other = arena
            .children(arena.root())
            .iter()
            .filter_map(|id| arena.node(*id))
            .find(|node| node.kind == NodeKind::Synthetic(SyntheticKind::Other))
            .expect("displaced entry should be represented by Other");
        assert_eq!(other.metrics.apparent_bytes, 1);
        assert_eq!(other.metrics.descendants, 1);
        let root_metrics = arena.node(arena.root()).expect("root should exist").metrics;
        assert_eq!(root_metrics.apparent_bytes, 1 + 8 * 1024 + 64 * 1024);
        assert_eq!(root_metrics.descendants, 3);
    }
    #[test]
    fn child_cap_breaks_equal_size_ties_by_native_name() {
        let root = tempfile::tempdir().expect("model root should exist");
        let zeta = root.path().join("zeta");
        let middle = root.path().join("middle");
        let alpha = root.path().join("alpha");
        for path in [&zeta, &middle, &alpha] {
            fs::write(path, b"x").expect("equal-size fixture should be written");
        }

        let mut arena = test_arena(root.path());
        arena.max_children_per_directory = 2;
        let zeta_id = add_path(&mut arena, &zeta).expect("zeta should be retained");
        let middle_id = add_path(&mut arena, &middle).expect("middle should be retained");
        let alpha_id = add_path(&mut arena, &alpha).expect("alpha should replace zeta");

        assert!(arena.find_child(arena.root(), OsStr::new("zeta")).is_none());
        assert_eq!(alpha_id, zeta_id);
        assert_eq!(
            arena.find_child(arena.root(), OsStr::new("middle")),
            Some(middle_id)
        );
        assert_eq!(
            arena.find_child(arena.root(), OsStr::new("alpha")),
            Some(alpha_id)
        );
    }

    #[test]
    fn child_cap_reseats_fresh_equal_rank_directory_by_retention_order() {
        let root = tempfile::tempdir().expect("model root should exist");
        let zeta = root.path().join("zeta");
        let beta = root.path().join("beta");
        let alpha = root.path().join("alpha");
        let larger = root.path().join("a-larger");
        for path in [&zeta, &beta, &alpha] {
            fs::create_dir(path).expect("empty directory should be created");
        }
        fs::write(&larger, vec![b'x'; 8 * 1024]).expect("larger file should be written");

        let mut arena = test_arena(root.path());
        arena.max_children_per_directory = 2;
        let zeta_id = add_path(&mut arena, &zeta).expect("zeta should be retained");
        let beta_id = add_path(&mut arena, &beta).expect("beta should be retained");
        let alpha_id = add_path(&mut arena, &alpha).expect("alpha should replace zeta");
        assert_eq!(alpha_id, zeta_id);
        assert_eq!(
            arena.retention_order(beta_id, alpha_id),
            std::cmp::Ordering::Less
        );

        add_path(&mut arena, &larger).expect("larger file should replace beta");

        assert!(arena.find_child(arena.root(), OsStr::new("zeta")).is_none());
        assert!(arena.find_child(arena.root(), OsStr::new("beta")).is_none());
        assert_eq!(
            arena.find_child(arena.root(), OsStr::new("alpha")),
            Some(alpha_id)
        );
        assert!(
            arena
                .find_child(arena.root(), OsStr::new("a-larger"))
                .is_some()
        );
    }

    #[test]
    fn eviction_stash_reorders_all_candidates_after_metric_growth() {
        let root = tempfile::tempdir().expect("model root should exist");
        let a = root.path().join("a");
        let b = root.path().join("b");
        let c = root.path().join("c");
        let d = root.path().join("d");
        for path in [&a, &b, &c, &d] {
            fs::create_dir(path).expect("empty directory should be created");
        }

        let mut arena = test_arena(root.path());
        let root_id = arena.root();
        let a_id = add_path(&mut arena, &a).expect("a should be retained");
        let b_id = add_path(&mut arena, &b).expect("b should be retained");
        let c_id = add_path(&mut arena, &c).expect("c should be retained");
        let d_id = add_path(&mut arena, &d).expect("d should be retained");
        for (id, rank) in [(a_id, 10), (b_id, 7), (c_id, 5), (d_id, 1)] {
            arena
                .node_mut(id)
                .expect("retained directory should exist")
                .metrics
                .allocated_bytes = ByteBounds::exact(rank);
        }
        assert_eq!(arena.refill_eviction_stash(root_id), Some(d_id));

        // The stale order is a, b, c, d while the live order is a, c, d, b.
        // Re-seating only d with `partition_point` would return d, not b.
        arena
            .node_mut(c_id)
            .expect("c should remain")
            .metrics
            .allocated_bytes = ByteBounds::exact(9);
        arena
            .node_mut(d_id)
            .expect("d should remain")
            .metrics
            .allocated_bytes = ByteBounds::exact(8);

        assert_eq!(arena.smallest_retained_child(root_id), Some(b_id));
    }

    #[test]
    fn eviction_stash_rebuilds_at_equal_rank_frontier_ties() {
        let root = tempfile::tempdir().expect("model root should exist");
        let mut arena = test_arena(root.path());
        let root_id = arena.root();
        let mut entries = Vec::with_capacity(EVICTION_STASH.saturating_add(1));
        for index in 0..=EVICTION_STASH {
            let path = root.path().join(format!("entry-{index:03}"));
            fs::create_dir(&path).expect("empty directory should be created");
            let id = add_path(&mut arena, &path).expect("entry should be retained");
            arena
                .node_mut(id)
                .expect("retained entry should exist")
                .metrics
                .allocated_bytes = ByteBounds::exact(u128::from(index != 0));
            entries.push(id);
        }
        let first = entries[0];
        let omitted = entries[1];
        assert_eq!(arena.refill_eviction_stash(root_id), Some(first));

        // `entry-000` started below the cached frontier. Once it reaches the
        // same rank, its name puts it above the cached `entry-002` frontier.
        // the omitted `entry-001` is now the true smallest retained child.
        arena
            .node_mut(first)
            .expect("first entry should exist")
            .metrics
            .allocated_bytes = ByteBounds::exact(1);
        for id in entries.iter().copied().skip(2) {
            let name = arena
                .node(id)
                .expect("cached entry should exist")
                .name
                .clone();
            arena.detach_child(root_id, id, &name, true);
        }

        assert_eq!(arena.smallest_retained_child(root_id), Some(omitted));
    }

    #[test]
    fn retention_key_uses_id_for_equal_rank_and_name_ties() {
        let ceiling = RetentionKey {
            rank: (false, 1),
            name: std::sync::Arc::from(OsStr::new("same")),
            id: NodeId(2),
        };

        assert_eq!(
            ceiling.compare_candidate((false, 1), OsStr::new("same"), NodeId(1)),
            std::cmp::Ordering::Greater,
            "the lower ID must win the final retention-order tie"
        );
    }

    #[test]
    fn eviction_stash_yields_budget_to_flat_directory_overflow() {
        let root = tempfile::tempdir().expect("model root should exist");
        let retained = root.path().join("a");
        let overflow = root.path().join("z");
        fs::create_dir(&retained).expect("retained directory should be created");
        fs::create_dir(&overflow).expect("overflow directory should be created");

        let mut arena = test_arena(root.path());
        let root_id = arena.root();
        arena.max_children_per_directory = 1;
        add_path(&mut arena, &retained).expect("first directory should be retained");

        let other_reservation = arena.child_reservation(root_id, OsStr::new("Other"));
        let remaining_for_cache_and_other = EVICTION_STASH_ALLOCATION
            .saturating_add(other_reservation)
            .saturating_sub(1);
        let available = arena.memory_limit().saturating_sub(arena.memory_used());
        let filler = available
            .checked_sub(remaining_for_cache_and_other)
            .expect("fixture budget should fit the cache and almost fit Other");
        arena
            .budget
            .reserve(filler)
            .expect("fixture should consume the extra model budget");
        assert_eq!(
            arena.memory_limit().saturating_sub(arena.memory_used()),
            remaining_for_cache_and_other
        );

        assert!(
            add_path(&mut arena, &overflow).is_none(),
            "an equal-rank late entry should aggregate into Other"
        );
        let other = arena
            .find_child(root_id, OsStr::new("Other"))
            .expect("representable overflow should create Other");
        assert!(arena.is_other_node(other));
        assert!(
            !arena.eviction_stash.contains_key(&root_id),
            "the optional cache must release its reservation for Other"
        );
        assert_eq!(
            arena.memory_limit().saturating_sub(arena.memory_used()),
            EVICTION_STASH_ALLOCATION.saturating_sub(1),
            "Other should consume its reservation without retaining the cache charge"
        );
    }

    #[test]
    fn eviction_stashes_from_other_parents_yield_budget_to_overflow() {
        let root = tempfile::tempdir().expect("model root should exist");
        let cached_parent = root.path().join("cached-parent");
        let cached_child = cached_parent.join("cached-child");
        let overflow = root.path().join("overflow");
        fs::create_dir(&cached_parent).expect("cache parent should be created");
        fs::create_dir(&cached_child).expect("cache child should be created");
        fs::create_dir(&overflow).expect("overflow should be created");

        let mut arena = test_arena(root.path());
        let root_id = arena.root();
        arena.max_children_per_directory = 1;
        let cached_parent_id =
            add_path(&mut arena, &cached_parent).expect("cache parent should be retained");
        let cached_child_id =
            add_path(&mut arena, &cached_child).expect("cache child should be retained");
        assert_eq!(
            arena.refill_eviction_stash(cached_parent_id),
            Some(cached_child_id)
        );
        assert!(arena.eviction_stash.contains_key(&cached_parent_id));
        assert!(
            !arena.eviction_stash.contains_key(&root_id),
            "only the unrelated parent should hold a stash"
        );

        let other_reservation = arena.other_child_reservation(root_id);
        let remaining_for_other = other_reservation.saturating_sub(1);
        assert!(
            remaining_for_other < EVICTION_STASH_ALLOCATION,
            "the root must be unable to allocate a second stash"
        );
        let available = arena.memory_limit().saturating_sub(arena.memory_used());
        let filler = available
            .checked_sub(remaining_for_other)
            .expect("fixture budget should leave Other one byte short");
        arena
            .budget
            .reserve(filler)
            .expect("fixture should consume the extra model budget");

        assert!(
            add_path(&mut arena, &overflow).is_none(),
            "a globally cached frontier must not prevent mandatory Other"
        );
        let other = arena
            .find_child(root_id, OsStr::new("Other"))
            .expect("representable overflow should create Other");
        assert!(arena.is_other_node(other));
        assert!(
            !arena.eviction_stash.contains_key(&cached_parent_id),
            "Other must release an unrelated optional stash"
        );
        assert_eq!(arena.retained_child_count(root_id), 1);
        assert_eq!(
            arena.memory_limit().saturating_sub(arena.memory_used()),
            EVICTION_STASH_ALLOCATION.saturating_sub(1),
            "Other should consume its reservation after freeing the remote stash"
        );
    }

    #[test]
    fn child_reservation_retries_after_releasing_global_stashes() {
        let root = tempfile::tempdir().expect("model root should exist");
        let cached_parent = root.path().join("cached-parent");
        let cached_child = cached_parent.join("cached-child");
        let incoming = root.path().join("incoming");
        fs::create_dir(&cached_parent).expect("cache parent should be created");
        fs::create_dir(&cached_child).expect("cache child should be created");
        fs::create_dir(&incoming).expect("incoming directory should be created");

        let mut arena = test_arena(root.path());
        let cached_parent_id =
            add_path(&mut arena, &cached_parent).expect("cache parent should be retained");
        let cached_child_id =
            add_path(&mut arena, &cached_child).expect("cache child should be retained");
        assert_eq!(
            arena.refill_eviction_stash(cached_parent_id),
            Some(cached_child_id)
        );
        assert!(arena.eviction_stash.contains_key(&cached_parent_id));

        let reservation = arena.child_reservation(
            arena.root(),
            incoming
                .file_name()
                .expect("incoming path should have a filename"),
        );
        assert!(
            EVICTION_STASH_ALLOCATION >= reservation,
            "one optional stash should make the retried reservation affordable"
        );
        let headroom = reservation.saturating_sub(1);
        let filler = arena
            .memory_limit()
            .saturating_sub(arena.memory_used())
            .checked_sub(headroom)
            .expect("fixture budget should leave one byte too little for the child");
        arena
            .budget
            .reserve(filler)
            .expect("fixture should consume the extra model budget");

        assert!(
            add_path(&mut arena, &incoming).is_some(),
            "a retained child should retry instead of becoming avoidable overflow"
        );
        assert!(
            !arena.eviction_stash.contains_key(&cached_parent_id),
            "the retry must release optional stashes globally"
        );
    }

    #[test]
    fn untracked_overflow_reservation_retries_after_releasing_global_stashes() {
        let root = tempfile::tempdir().expect("model root should exist");
        let cached_parent = root.path().join("cached-parent");
        let cached_child = cached_parent.join("cached-child");
        let aggregated = root.path().join("aggregated");
        fs::create_dir(&cached_parent).expect("cache parent should be created");
        fs::create_dir(&cached_child).expect("cache child should be created");
        fs::write(&aggregated, b"payload").expect("aggregate fixture should be written");

        let mut arena = test_arena(root.path());
        let root_id = arena.root();
        let cached_parent_id =
            add_path(&mut arena, &cached_parent).expect("cache parent should be retained");
        let cached_child_id =
            add_path(&mut arena, &cached_child).expect("cache child should be retained");
        assert_eq!(
            arena.refill_eviction_stash(cached_parent_id),
            Some(cached_child_id)
        );
        let metadata = fs::symlink_metadata(&aggregated).expect("aggregate metadata should exist");
        let identity = identity_for(&aggregated, &metadata)
            .expect("aggregate identity should be readable")
            .expect("aggregate fixture should not be a link");
        assert!(
            arena
                .add_entry_aggregated(&aggregated, &metadata, identity)
                .expect("aggregate fixture should be represented")
                .is_none()
        );
        let other = arena
            .find_child(root_id, OsStr::new("Other"))
            .expect("overflow state should exist");
        assert!(arena.is_other_node(other));
        assert!(!arena.untracked_metrics.contains_key(&other));

        let headroom = UNTRACKED_METRICS_OVERHEAD.saturating_sub(1);
        let filler = arena
            .memory_limit()
            .saturating_sub(arena.memory_used())
            .checked_sub(headroom)
            .expect("fixture budget should leave one byte too little for untracked state");
        arena
            .budget
            .reserve(filler)
            .expect("fixture should consume the extra model budget");

        arena
            .record_unscanned(
                &root.path().join("Other"),
                UnscannedReason::Metadata("fixture metadata unavailable".to_string()),
            )
            .expect("existing overflow state should retry its untracked reservation");

        assert!(arena.untracked_metrics.contains_key(&other));
        assert!(
            !arena.eviction_stash.contains_key(&cached_parent_id),
            "the untracked retry must release optional stashes globally"
        );
    }

    #[test]
    fn concrete_compaction_reservation_releases_global_eviction_stashes() {
        let root = tempfile::tempdir().expect("model root should exist");
        let cold = root.path().join("cold");
        let aggregated = cold.join("aggregated");
        let cached_parent = root.path().join("cached-parent");
        let cached_child = cached_parent.join("cached-child");
        fs::create_dir(&cold).expect("cold directory should be created");
        fs::write(&aggregated, b"aggregated").expect("aggregate fixture should be written");
        fs::create_dir(&cached_parent).expect("cache parent should be created");
        fs::write(&cached_child, b"cached").expect("cache child should be written");

        let mut arena = test_arena(root.path());
        let root_id = arena.root();
        let cold_id = add_path(&mut arena, &cold).expect("cold directory should be retained");
        let metadata = fs::symlink_metadata(&aggregated).expect("aggregate metadata should exist");
        let identity = identity_for(&aggregated, &metadata)
            .expect("aggregate identity should be readable")
            .expect("aggregate fixture should not be a link");
        assert!(
            arena
                .add_entry_aggregated(&aggregated, &metadata, identity)
                .expect("aggregate fixture should be represented")
                .is_none()
        );
        let other = arena
            .find_child(cold_id, OsStr::new("Other"))
            .expect("cold directory should contain Other");
        arena
            .record_unscanned(
                &cold.join("Other"),
                UnscannedReason::Metadata("fixture metadata unavailable".to_string()),
            )
            .expect("Other should retain untracked metrics");
        assert!(arena.untracked_metrics.contains_key(&other));

        let cached_parent_id =
            add_path(&mut arena, &cached_parent).expect("cache parent should be retained");
        let cached_child_id =
            add_path(&mut arena, &cached_child).expect("cache child should be retained");
        assert_eq!(
            arena.refill_eviction_stash(cached_parent_id),
            Some(cached_child_id)
        );
        assert!(arena.eviction_stash.contains_key(&cached_parent_id));

        let headroom = UNTRACKED_METRICS_OVERHEAD.saturating_sub(1);
        let filler = arena
            .memory_limit()
            .saturating_sub(arena.memory_used())
            .checked_sub(headroom)
            .expect("fixture budget should leave one byte too little for compaction state");
        arena
            .budget
            .reserve(filler)
            .expect("fixture should consume the extra model budget");

        assert!(
            arena
                .aggregate_cold_subtree(&HashSet::from([root_id, cached_parent_id]))
                .expect("concrete compaction should release optional stashes")
        );
        assert_eq!(
            arena
                .node(cold_id)
                .expect("cold aggregate should remain")
                .kind,
            NodeKind::Synthetic(SyntheticKind::Aggregate)
        );
        assert!(arena.untracked_metrics.contains_key(&cold_id));
        assert!(
            !arena.eviction_stash.contains_key(&cached_parent_id),
            "the concrete reservation must release unrelated optional stashes"
        );
    }

    #[test]
    fn compaction_reuses_untracked_slot_at_the_model_limit() {
        let root = tempfile::tempdir().expect("model root should exist");
        let cold = root.path().join("cold");
        let aggregated = cold.join("aggregated");
        fs::create_dir(&cold).expect("cold directory should be created");
        fs::write(&aggregated, b"aggregated").expect("aggregate fixture should be written");

        let mut arena = test_arena(root.path());
        let root_id = arena.root();
        let cold_id = add_path(&mut arena, &cold).expect("cold directory should be retained");
        let metadata = fs::symlink_metadata(&aggregated).expect("aggregate metadata should exist");
        let identity = identity_for(&aggregated, &metadata)
            .expect("aggregate identity should be readable")
            .expect("aggregate fixture should not be a link");
        assert!(
            arena
                .add_entry_aggregated(&aggregated, &metadata, identity)
                .expect("aggregate fixture should be represented")
                .is_none()
        );
        let other = arena
            .find_child(cold_id, OsStr::new("Other"))
            .expect("cold directory should contain Other");
        arena
            .record_unscanned(
                &cold.join("Other"),
                UnscannedReason::Metadata("fixture metadata unavailable".to_string()),
            )
            .expect("Other should retain untracked metrics");
        assert!(arena.untracked_metrics.contains_key(&other));

        let remaining = arena.memory_limit().saturating_sub(arena.memory_used());
        arena
            .budget
            .reserve(remaining)
            .expect("test should consume the remaining model budget");

        assert!(
            arena
                .aggregate_cold_subtree(&HashSet::from([root_id]))
                .expect("compaction should reuse a removed untracked slot")
        );
        assert_eq!(
            arena
                .node(cold_id)
                .expect("cold aggregate should remain")
                .kind,
            NodeKind::Synthetic(SyntheticKind::Aggregate)
        );
        assert!(arena.untracked_metrics.contains_key(&cold_id));
        assert!(!arena.untracked_metrics.contains_key(&other));
    }

    #[test]
    fn ascending_replacements_drain_a_single_eviction_stash() {
        let root = tempfile::tempdir().expect("model root should exist");
        let mut arena = test_arena(root.path());
        arena.max_children_per_directory = EVICTION_STASH;
        for index in 0..EVICTION_STASH {
            let directory = root.path().join(format!("z-stashed-{index:03}"));
            fs::create_dir(&directory).expect("stashed directory should be created");
            add_path(&mut arena, &directory).expect("stashed directory should be retained");
        }

        for rank in 1_u128..=4 {
            let incoming = root.path().join(format!("a-incoming-{rank:02}"));
            fs::write(&incoming, vec![b'x'; 8 * 1024])
                .expect("ascending replacement should be written");
            let id =
                add_path(&mut arena, &incoming).expect("ascending replacement should be retained");
            arena
                .node_mut(id)
                .expect("ascending replacement should remain")
                .metrics
                .allocated_bytes = ByteBounds::exact(rank);
        }

        assert_eq!(
            arena.eviction_stash_sweeps, 1,
            "ascending candidates should drain the original stash instead of sweeping per insertion"
        );
    }

    #[test]
    fn eviction_stash_charges_and_releases_its_candidate_buffer() {
        let root = tempfile::tempdir().expect("model root should exist");
        let child = root.path().join("child");
        fs::write(&child, b"child").expect("child should be written");

        let mut arena = test_arena(root.path());
        let root_id = arena.root();
        let child_id = add_path(&mut arena, &child).expect("child should be retained");
        let before_stash = arena.memory_used();

        assert_eq!(arena.refill_eviction_stash(root_id), Some(child_id));
        assert_eq!(
            arena.memory_used(),
            before_stash.saturating_add(EVICTION_STASH_ALLOCATION)
        );

        let name = arena
            .node(child_id)
            .expect("child should remain")
            .name
            .clone();
        arena.detach_child(root_id, child_id, &name, true);

        assert!(!arena.eviction_stash.contains_key(&root_id));
        assert_eq!(arena.memory_used(), before_stash);
    }

    #[test]
    fn removing_a_stashed_directory_releases_its_eviction_cache() {
        let root = tempfile::tempdir().expect("model root should exist");
        let directory = root.path().join("directory");
        let child = directory.join("child");
        fs::create_dir(&directory).expect("directory should be created");
        fs::create_dir(&child).expect("child directory should be created");

        let mut arena = test_arena(root.path());
        arena.max_children_per_directory = 1;
        let directory_id = add_path(&mut arena, &directory).expect("directory should be retained");
        let child_id = add_path(&mut arena, &child).expect("child should be retained");
        assert_eq!(arena.refill_eviction_stash(directory_id), Some(child_id));

        let other = arena
            .ensure_other(arena.root())
            .expect("root overflow node should be retained");
        arena
            .aggregate_child_into_other(directory_id, other)
            .expect("directory aggregation should succeed");

        assert!(!arena.eviction_stash.contains_key(&directory_id));
    }

    #[test]
    fn evicting_a_stash_ceiling_releases_the_cached_name() {
        let root = tempfile::tempdir().expect("model root should exist");
        let first = root.path().join("first");
        let second = root.path().join("second");
        fs::write(&first, b"first").expect("first child should be written");
        fs::write(&second, b"second").expect("second child should be written");

        let mut arena = test_arena(root.path());
        let root_id = arena.root();
        add_path(&mut arena, &first).expect("first child should be retained");
        add_path(&mut arena, &second).expect("second child should be retained");
        let before_stash = arena.memory_used();
        arena
            .refill_eviction_stash(root_id)
            .expect("two children should produce a stash candidate");
        assert_eq!(
            arena.memory_used(),
            before_stash.saturating_add(EVICTION_STASH_ALLOCATION)
        );

        let ceiling = arena
            .eviction_stash
            .get(&root_id)
            .expect("stash should be present")
            .ceiling
            .id;
        arena.remove_reusable_nodes(vec![ceiling]);

        assert!(!arena.eviction_stash.contains_key(&root_id));
        assert!(arena.memory_used() < before_stash);
    }

    #[test]
    fn child_cap_does_not_recreate_aggregated_directories() {
        let root = tempfile::tempdir().expect("model root should exist");
        let directory = root.path().join("directory");
        fs::create_dir(&directory).expect("fixture directory should be created");
        let file = directory.join("file");
        fs::write(&file, b"payload").expect("fixture file should be written");

        let mut arena = test_arena(root.path());
        arena.max_children_per_directory = 0;
        assert!(add_path(&mut arena, &directory).is_none());
        assert!(add_path(&mut arena, &file).is_none());

        arena
            .complete_directory(&directory, None)
            .expect("completion of an aggregated directory should be harmless");
        let root_children = arena.children(arena.root());
        assert_eq!(root_children.len(), 1);
        let other = arena
            .node(root_children[0])
            .expect("root aggregate should remain");
        assert_eq!(other.kind, NodeKind::Synthetic(SyntheticKind::Other));
        assert_eq!(other.metrics.apparent_bytes, 7);
        assert_eq!(other.metrics.descendants, 2);
    }

    #[test]
    fn conflicting_hard_link_counts_never_claim_reclaimable_bytes() {
        let root = tempfile::tempdir().expect("model root should exist");
        let first = root.path().join("first");
        let second = root.path().join("second");
        fs::write(&first, b"payload").expect("fixture should be written");
        fs::hard_link(&first, &second).expect("hard link should be created");
        let first_metadata = fs::symlink_metadata(&first).expect("first metadata should exist");
        let identity = identity_for(&first, &first_metadata)
            .expect("fixture identity lookup should succeed")
            .expect("fixture identity should exist");
        let first_identity = NativeIdentity {
            link_count: Some(1),
            ..identity.clone()
        };
        let second_identity = NativeIdentity {
            link_count: Some(2),
            ..identity
        };

        let mut arena = test_arena(root.path());
        let first_id = arena
            .add_entry(&first, &first_metadata, first_identity)
            .expect("first link should be retained")
            .expect("first link should have a node");
        let second_metadata = fs::symlink_metadata(&second).expect("second metadata should exist");
        arena
            .add_entry(&second, &second_metadata, second_identity)
            .expect("second link should be retained")
            .expect("second link should have a node");
        arena.finalize().expect("conflicting links should finalize");

        assert_eq!(
            arena
                .node(first_id)
                .expect("first link node should remain")
                .metrics
                .reclaimable_bytes,
            ByteBounds::exact(0)
        );
    }
    #[test]
    fn hard_links_move_allocated_bytes_to_shared() {
        let root = tempfile::tempdir().expect("model root should exist");
        let first = root.path().join("first");
        let second = root.path().join("second");
        fs::write(&first, b"payload").expect("fixture should be written");
        fs::hard_link(&first, &second).expect("hard link should be created");

        let mut arena = test_arena(root.path());
        let first_id = add_path(&mut arena, &first).expect("first link should be retained");
        let second_id = add_path(&mut arena, &second).expect("second link should be retained");
        arena.finalize().expect("model should finalize");

        assert_eq!(arena.identity_count(), 1);
        for id in [first_id, second_id] {
            let metrics = arena.node(id).expect("link node should exist").metrics;
            assert_eq!(metrics.allocated_bytes, ByteBounds::exact(0));
            assert_eq!(metrics.reclaimable_bytes, ByteBounds::exact(0));
        }
        let shared = arena
            .children(arena.root())
            .iter()
            .filter_map(|id| arena.node(*id))
            .find(|node| node.kind == NodeKind::Synthetic(SyntheticKind::Shared))
            .expect("shared allocation should be represented once");
        assert_eq!(
            shared.metrics.allocated_bytes,
            shared.metrics.reclaimable_bytes
        );
        assert_eq!(
            arena
                .node(arena.root())
                .expect("root should exist")
                .metrics
                .allocated_bytes,
            shared.metrics.allocated_bytes
        );
        assert_eq!(
            arena
                .node(arena.root())
                .expect("root should exist")
                .metrics
                .descendants,
            2
        );
    }

    #[test]
    fn hard_link_finalization_remains_exact_when_shared_node_cannot_fit() {
        let root = tempfile::tempdir().expect("model root should exist");
        let first = root.path().join("first");
        let second = root.path().join("second");
        fs::write(&first, b"payload").expect("fixture should be written");
        fs::hard_link(&first, &second).expect("hard link should be created");

        let mut arena = test_arena(root.path());
        add_path(&mut arena, &first).expect("first link should be retained");
        add_path(&mut arena, &second).expect("second link should be retained");
        let allocated_before = arena
            .node(arena.root())
            .expect("root should exist")
            .metrics
            .allocated_bytes;
        let remaining = arena.memory_limit().saturating_sub(arena.memory_used());
        arena
            .budget
            .reserve(remaining)
            .expect("test should consume the remaining model budget");

        arena
            .finalize()
            .expect("hard links should finalize conservatively");

        assert_eq!(
            arena
                .node(arena.root())
                .expect("root should exist")
                .metrics
                .allocated_bytes,
            allocated_before
        );
        assert!(arena.children(arena.root()).iter().all(|id| {
            arena
                .node(*id)
                .is_none_or(|node| node.kind != NodeKind::Synthetic(SyntheticKind::Shared))
        }));
    }
    #[test]
    fn aggregated_hard_links_still_count_allocation_once() {
        let root = tempfile::tempdir().expect("model root should exist");
        let first = root.path().join("first");
        let second = root.path().join("second");
        fs::write(&first, b"payload").expect("fixture should be written");
        fs::hard_link(&first, &second).expect("hard link should be created");

        let mut arena = test_arena(root.path());
        arena.max_children_per_directory = 0;
        assert!(add_path(&mut arena, &first).is_none());
        assert!(add_path(&mut arena, &second).is_none());
        arena.finalize().expect("model should finalize");

        let other = arena
            .children(arena.root())
            .iter()
            .filter_map(|id| arena.node(*id))
            .find(|node| node.kind == NodeKind::Synthetic(SyntheticKind::Other))
            .expect("aggregated entries should remain represented");
        let shared = arena
            .children(arena.root())
            .iter()
            .filter_map(|id| arena.node(*id))
            .find(|node| node.kind == NodeKind::Synthetic(SyntheticKind::Shared))
            .expect("aggregated shared allocation should be represented once");
        assert_eq!(other.metrics.allocated_bytes, ByteBounds::exact(0));
        assert_eq!(other.metrics.reclaimable_bytes, ByteBounds::exact(0));
        assert_eq!(
            arena
                .node(arena.root())
                .expect("root should exist")
                .metrics
                .allocated_bytes,
            shared.metrics.allocated_bytes
        );
        assert_eq!(
            arena
                .node(arena.root())
                .expect("root should exist")
                .metrics
                .descendants,
            2
        );
    }

    #[cfg(unix)]
    #[test]
    fn deleting_materialized_hard_link_rehomes_allocation_to_other() {
        let root = tempfile::tempdir().expect("model root should exist");
        let first = root.path().join("first");
        let second = root.path().join("second");
        fs::write(&first, b"payload").expect("fixture should be written");
        fs::hard_link(&first, &second).expect("hard link should be created");

        let mut arena = test_arena(root.path());
        arena.max_children_per_directory = 1;
        let first_id = add_path(&mut arena, &first).expect("first link should be retained");
        assert!(add_path(&mut arena, &second).is_none());
        arena.finalize().expect("hard links should finalize");
        let shared_allocation = arena
            .children(arena.root())
            .iter()
            .filter_map(|id| arena.node(*id))
            .find(|node| node.kind == NodeKind::Synthetic(SyntheticKind::Shared))
            .expect("shared allocation should be represented")
            .metrics
            .allocated_bytes;

        assert!(arena.remove_path(&first));

        let other = arena
            .children(arena.root())
            .iter()
            .filter_map(|id| arena.node(*id))
            .find(|node| node.kind == NodeKind::Synthetic(SyntheticKind::Other))
            .expect("remaining hard link should stay in Other");
        assert_eq!(other.metrics.allocated_bytes, shared_allocation);
        assert_eq!(other.metrics.reclaimable_bytes.lower, 0);
        assert!(arena.children(arena.root()).iter().all(|id| {
            arena
                .node(*id)
                .is_none_or(|node| node.kind != NodeKind::Synthetic(SyntheticKind::Shared))
        }));
        assert_eq!(
            arena
                .node(arena.root())
                .expect("root should exist")
                .metrics
                .allocated_bytes,
            shared_allocation
        );
        assert!(arena.node(first_id).is_none());
    }

    #[test]
    fn unreadable_entry_preserves_unknown_upper_bounds() {
        let root = tempfile::tempdir().expect("model root should exist");
        let mut arena = test_arena(root.path());
        let path = root.path().join("unreadable");
        arena
            .record_unscanned(&path, UnscannedReason::Metadata("denied".to_string()))
            .expect("unreadable entry should be represented");

        let id = arena
            .find_child(arena.root(), OsStr::new("unreadable"))
            .expect("unreadable node should exist");
        let node = arena.node(id).expect("unreadable node should remain");
        assert_eq!(node.state, NodeState::Uncertain);
        assert_eq!(node.metrics.allocated_bytes, ByteBounds::unknown());
        assert_eq!(node.metrics.reclaimable_bytes, ByteBounds::unknown());
    }

    #[test]
    fn unreadable_root_preserves_reason_and_unknown_bounds_after_rebuild() {
        let root = tempfile::tempdir().expect("model root should exist");
        let mut arena = test_arena(root.path());
        let reason = UnscannedReason::Metadata("root metadata denied".to_string());

        arena
            .record_unscanned(root.path(), reason.clone())
            .expect("root uncertainty should be recorded");
        arena.rebuild();

        let root_node = arena.node(arena.root()).expect("root should remain");
        assert_eq!(root_node.state, NodeState::Uncertain);
        assert_eq!(root_node.unscanned_reason.as_ref(), Some(&reason));
        assert_eq!(root_node.metrics.allocated_bytes.upper, None);
        assert_eq!(root_node.metrics.reclaimable_bytes.upper, None);
    }
    #[cfg(unix)]
    #[test]
    fn replacement_uncertainty_discards_old_occupant_bounds() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("replacement root should exist");
        let target = root.path().join("target");
        let old_child = target.join("old-child");
        let replacement_target = root.path().join("replacement-target");
        let ordinary = root.path().join("ordinary");
        fs::create_dir(&target).expect("target directory should be created");
        fs::write(&old_child, b"old occupant").expect("old child should be written");
        fs::create_dir(&replacement_target).expect("replacement target should be created");
        fs::write(&ordinary, b"ordinary").expect("ordinary file should be written");

        let mut arena = test_arena(root.path());
        let target_id = add_path(&mut arena, &target).expect("target should be retained");
        let old_child_id = add_path(&mut arena, &old_child).expect("old child should be retained");
        let ordinary_id = add_path(&mut arena, &ordinary).expect("ordinary should be retained");
        arena
            .complete_directory(&target, None)
            .expect("target directory should complete");
        arena
            .complete_directory(root.path(), None)
            .expect("root directory should complete");
        arena.finalize().expect("model should finalize");
        let ordinary_lower = arena
            .node(ordinary_id)
            .expect("ordinary should exist")
            .metrics
            .allocated_bytes
            .lower;

        fs::rename(&target, root.path().join("displaced")).expect("old target should be displaced");
        symlink(&replacement_target, &target).expect("replacement symlink should be created");
        arena
            .record_unscanned(
                &target,
                UnscannedReason::Replacement("directory identity changed".to_string()),
            )
            .expect("replacement should be represented");
        arena.rebuild();

        assert!(arena.node(old_child_id).is_none());
        assert!(arena.node(target_id).is_none());
        let replacement_id = arena
            .find_child(arena.root(), OsStr::new("target"))
            .expect("replacement should remain visible");
        let replacement = arena
            .node(replacement_id)
            .expect("replacement node should exist");
        assert_eq!(replacement.kind, NodeKind::Link);
        assert_eq!(replacement.state, NodeState::Uncertain);
        assert_eq!(replacement.metrics.allocated_bytes, ByteBounds::unknown());
        assert_eq!(replacement.metrics.reclaimable_bytes, ByteBounds::unknown());

        arena
            .record_unscanned(&ordinary, UnscannedReason::Metadata("denied".to_string()))
            .expect("ordinary unreadable entry should be represented");
        let ordinary = arena.node(ordinary_id).expect("ordinary should remain");
        assert_eq!(ordinary.metrics.allocated_bytes.lower, ordinary_lower);
        assert_eq!(ordinary.metrics.allocated_bytes.upper, None);
    }
    #[cfg(unix)]
    #[test]
    fn symbolic_link_tracks_object_allocation_and_reclaimability() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("link root should exist");
        let target = root.path().join("target");
        let link = root.path().join("link");
        fs::write(&target, b"target").expect("link target should be written");
        symlink(&target, &link).expect("symbolic link should be created");

        let mut arena = test_arena(root.path());
        arena
            .record_unscanned(&link, UnscannedReason::SymbolicLink)
            .expect("symbolic link should be represented");
        let id = arena
            .find_child(arena.root(), OsStr::new("link"))
            .expect("symbolic link should remain visible");
        let node = arena.node(id).expect("symbolic link node should exist");
        assert!(node.snapshot.allocated_bytes.is_some());
        assert_eq!(node.metrics.reclaimable_bytes, node.metrics.allocated_bytes);
    }
    #[test]
    fn unreadable_directory_without_prior_node_stays_unknown_after_rebuild() {
        let root = tempfile::tempdir().expect("model root should exist");
        let directory = root.path().join("unreadable-directory");
        fs::create_dir(&directory).expect("directory fixture should be created");
        let mut arena = test_arena(root.path());
        arena
            .record_unscanned(&directory, UnscannedReason::Metadata("denied".to_string()))
            .expect("unreadable directory should be represented");
        let id = arena
            .find_child(arena.root(), OsStr::new("unreadable-directory"))
            .expect("unreadable directory should exist");
        assert_eq!(
            arena
                .node(id)
                .expect("unreadable directory should remain")
                .metrics
                .allocated_bytes
                .upper,
            None
        );
        arena.rebuild();
        let node = arena
            .node(id)
            .expect("unreadable directory should survive rebuild");
        assert_eq!(node.metrics.allocated_bytes.upper, None);
        assert_eq!(node.metrics.reclaimable_bytes.upper, None);
    }
    #[test]
    fn excluded_and_boundary_entries_are_visible_without_scoped_bytes() {
        let root = tempfile::tempdir().expect("model root should exist");
        let excluded = root.path().join("excluded");
        let boundary = root.path().join("boundary");
        fs::write(&excluded, b"payload").expect("excluded fixture should be written");
        fs::write(&boundary, b"payload").expect("boundary fixture should be written");

        let mut arena = test_arena(root.path());
        for (path, reason) in [
            (
                &excluded,
                UnscannedReason::Excluded("fixture rule".to_string()),
            ),
            (&boundary, UnscannedReason::FilesystemBoundary),
        ] {
            arena
                .record_unscanned(path, reason.clone())
                .expect("unscanned entry should be represented");
            let name = path.file_name().expect("fixture should have a name");
            let id = arena
                .find_child(arena.root(), name)
                .expect("unscanned entry should remain visible");
            let node = arena.node(id).expect("unscanned node should remain");
            assert_eq!(node.state, NodeState::Uncertain);
            assert_eq!(node.unscanned_reason.as_ref(), Some(&reason));
            assert_eq!(node.metrics, NodeMetrics::default());
        }
        arena.finalize().expect("model should finalize");

        let root_metrics = arena.node(arena.root()).expect("root should exist").metrics;
        assert_eq!(root_metrics.apparent_bytes, 0);
        assert_eq!(root_metrics.allocated_bytes, ByteBounds::exact(0));
        assert_eq!(root_metrics.reclaimable_bytes, ByteBounds::exact(0));
    }
    #[test]
    fn excluded_hard_link_does_not_duplicate_scoped_allocation() {
        let root = tempfile::tempdir().expect("model root should exist");
        let included = root.path().join("included");
        let excluded = root.path().join("excluded");
        fs::write(&included, b"payload").expect("included fixture should be written");
        fs::hard_link(&included, &excluded).expect("excluded hard link should be created");

        let mut arena = test_arena(root.path());
        let included_id =
            add_path(&mut arena, &included).expect("included link should be retained");
        arena
            .record_unscanned(
                &excluded,
                UnscannedReason::Excluded("fixture rule".to_string()),
            )
            .expect("excluded link should be represented");
        arena.finalize().expect("model should finalize");

        let included_metrics = arena
            .node(included_id)
            .expect("included link should remain")
            .metrics;
        assert_eq!(
            arena
                .node(arena.root())
                .expect("root should remain")
                .metrics
                .allocated_bytes,
            included_metrics.allocated_bytes
        );
        let excluded_id = arena
            .find_child(arena.root(), OsStr::new("excluded"))
            .expect("excluded link should remain visible");
        assert_eq!(
            arena
                .node(excluded_id)
                .expect("excluded link should remain")
                .metrics,
            NodeMetrics::default()
        );
    }

    #[test]
    fn failed_directory_stays_uncertain_after_completion() {
        let root = tempfile::tempdir().expect("model root should exist");
        let directory = root.path().join("directory");
        fs::create_dir(&directory).expect("fixture directory should be created");
        let mut arena = test_arena(root.path());
        let id = add_path(&mut arena, &directory).expect("directory should be retained");

        arena
            .record_unscanned(&directory, UnscannedReason::Metadata("denied".to_string()))
            .expect("directory failure should be recorded");
        arena
            .complete_directory(&directory, None)
            .expect("directory completion should be accepted");
        arena.finalize().expect("model should finalize");

        let node = arena.node(id).expect("directory should remain");
        assert_eq!(node.state, NodeState::Uncertain);
        assert_eq!(node.metrics.allocated_bytes.upper, None);
        assert_eq!(
            arena
                .node(arena.root())
                .expect("root should exist")
                .metrics
                .allocated_bytes
                .upper,
            None
        );
    }

    #[test]
    fn outside_hard_link_keeps_reclaimable_lower_bound_zero() {
        let root = tempfile::tempdir().expect("model root should exist");
        let outside = tempfile::tempdir().expect("outside root should exist");
        let file = root.path().join("file");
        fs::write(&file, b"payload").expect("fixture should be written");
        fs::hard_link(&file, outside.path().join("outside-link"))
            .expect("outside hard link should be created");
        let mut arena = test_arena(root.path());
        let id = add_path(&mut arena, &file).expect("file should be retained");
        arena.finalize().expect("model should finalize");

        let metrics = arena.node(id).expect("file should remain").metrics;
        assert_eq!(metrics.reclaimable_bytes.lower, 0);
        assert_eq!(
            metrics.reclaimable_bytes.upper,
            metrics.allocated_bytes.upper
        );
        assert!(arena.children(arena.root()).iter().all(|id| {
            arena
                .node(*id)
                .is_none_or(|node| node.kind != NodeKind::Synthetic(SyntheticKind::Shared))
        }));
    }
    #[test]
    fn unknown_link_count_never_claims_reclaimable_lower_bound() {
        let root = tempfile::tempdir().expect("model root should exist");
        let file = root.path().join("file");
        fs::write(&file, b"payload").expect("fixture should be written");
        let metadata = fs::symlink_metadata(&file).expect("fixture metadata should be readable");
        let mut identity = identity_for(&file, &metadata)
            .expect("fixture identity should be readable")
            .expect("fixture should not be a symbolic link");
        identity.link_count = None;

        let mut arena = test_arena(root.path());
        let id = arena
            .add_entry(&file, &metadata, identity)
            .expect("fixture should be added")
            .expect("fixture should be retained");
        arena.finalize().expect("model should finalize");

        let metrics = arena.node(id).expect("file should remain").metrics;
        assert_eq!(metrics.reclaimable_bytes.lower, 0);
        assert_eq!(
            metrics.reclaimable_bytes.upper,
            metrics.allocated_bytes.upper
        );
    }

    #[test]
    fn shared_node_uses_observed_lowest_common_ancestor() {
        let root = tempfile::tempdir().expect("model root should exist");
        let directory = root.path().join("directory");
        fs::create_dir(&directory).expect("directory should be created");
        let first = directory.join("first");
        let second = directory.join("second");
        fs::write(&first, b"payload").expect("fixture should be written");
        fs::hard_link(&first, &second).expect("hard link should be created");
        let mut arena = test_arena(root.path());
        let directory_id = add_path(&mut arena, &directory).expect("directory should be retained");
        add_path(&mut arena, &first).expect("first link should be retained");
        add_path(&mut arena, &second).expect("second link should be retained");
        arena.finalize().expect("model should finalize");

        let shared = arena
            .children(directory_id)
            .iter()
            .filter_map(|id| arena.node(*id))
            .find(|node| node.kind == NodeKind::Synthetic(SyntheticKind::Shared))
            .expect("shared allocation should be placed inside common directory");
        assert_eq!(shared.parent, Some(directory_id));
        assert_eq!(
            arena
                .node(arena.root())
                .expect("root should exist")
                .metrics
                .allocated_bytes,
            shared.metrics.allocated_bytes
        );
    }

    #[test]
    fn apparent_and_allocated_metrics_remain_separate() {
        let root = tempfile::tempdir().expect("model root should exist");
        let file = root.path().join("sparse");
        let handle = fs::File::create(&file).expect("sparse fixture should be created");
        handle
            .set_len(1_048_576)
            .expect("sparse fixture should be sized");
        let mut arena = test_arena(root.path());
        let id = add_path(&mut arena, &file).expect("sparse file should be retained");
        arena.finalize().expect("model should finalize");

        let metrics = arena.node(id).expect("sparse file should remain").metrics;
        assert_eq!(metrics.apparent_bytes, 1_048_576);
        assert!(metrics.allocated_bytes.upper.is_some());
    }

    #[test]
    fn hundred_thousand_entry_flat_model_stays_bounded() {
        let root = tempfile::tempdir().expect("model root should exist");
        let fixture = root.path().join("fixture");
        fs::write(&fixture, b"x").expect("fixture should be written");
        let metadata = fs::metadata(&fixture).expect("fixture metadata should exist");
        let mut arena = Arena::new(
            root.path().to_path_buf(),
            MemoryBudget::from_mib(crate::model::DEFAULT_PROCESS_MIB)
                .expect("default model budget should be available"),
        )
        .expect("arena should be created");

        for index in 0..100_000_u64 {
            let path = root.path().join(format!("entry-{index}"));
            arena
                .add_entry(
                    &path,
                    &metadata,
                    NativeIdentity {
                        file_id: FileId::new_inode(1, index + 1),
                        link_count: Some(1),
                        reparse_point: false,
                    },
                )
                .expect("flat entry should be represented or aggregated");
        }
        arena.finalize().expect("flat model should finalize");

        assert_eq!(
            arena
                .node(arena.root())
                .expect("root should exist")
                .metrics
                .descendants,
            100_000
        );
        assert!(arena.children(arena.root()).len() <= DEFAULT_MAX_CHILDREN + 1);
        assert!(arena.nodes.len() <= DEFAULT_MAX_CHILDREN + 2);
        assert!(arena.memory_used() <= arena.memory_limit());
    }

    #[test]
    fn deep_model_operations_are_iterative() {
        let root = tempfile::tempdir().expect("model root should exist");
        let metadata = fs::metadata(root.path()).expect("root metadata should exist");
        let mut arena = test_arena(root.path());
        let mut path = root.path().to_path_buf();

        for depth in 0..2_048_u64 {
            path.push(format!("d{depth}"));
            arena
                .add_entry(
                    &path,
                    &metadata,
                    NativeIdentity {
                        file_id: FileId::new_inode(2, depth + 1),
                        link_count: Some(1),
                        reparse_point: false,
                    },
                )
                .expect("deep directory should be represented");
        }
        arena.finalize().expect("deep model should finalize");

        let deepest = arena.find_path(&path).expect("deepest node should exist");
        assert_eq!(arena.path_for(deepest), Some(path));
        arena.remove_subtree(arena.children(arena.root())[0]);
        arena.rebuild();
        assert_eq!(arena.children(arena.root()).len(), 0);
    }
    #[test]
    fn focused_graft_memory_failure_preserves_live_paths_ids_and_metrics() {
        let root = tempfile::tempdir().expect("model root should exist");
        let target = root.path().join("target");
        let old = target.join("old");
        let first = target.join("first-replacement");
        let second = target.join("second-replacement");
        fs::create_dir(&target).expect("target should be created");
        fs::write(&old, b"old").expect("old fixture should be written");

        let budget =
            MemoryBudget::from_model_limit(16 * 1024).expect("fixture budget should be valid");
        let mut live =
            Arena::new(root.path().to_path_buf(), budget).expect("live arena should be created");
        let target_id = add_path(&mut live, &target).expect("target should be retained");
        let old_id = add_path(&mut live, &old).expect("old entry should be retained");
        for path in [&target, root.path()] {
            live.complete_directory(path, None)
                .expect("fixture directory should complete");
        }
        live.finalize().expect("live arena should finalize");
        let before = live
            .nodes()
            .map(|node| {
                (
                    node.id,
                    live.path_for(node.id)
                        .expect("live node path should be available"),
                    node.kind,
                    node.state,
                    node.metrics,
                    node.snapshot.clone(),
                )
            })
            .collect::<Vec<_>>();
        let remaining = live.memory_limit().saturating_sub(live.memory_used());
        live.budget
            .reserve(remaining)
            .expect("fixture should consume the remaining live budget");
        let memory_before = live.memory_used();

        fs::remove_file(&old).expect("old fixture should be removed");
        fs::write(&first, b"first").expect("first replacement should be written");
        fs::write(&second, b"second").expect("second replacement should be written");
        let stage_budget =
            MemoryBudget::from_model_limit(16 * 1024).expect("staging budget should be valid");
        let mut stage =
            Arena::new(target.clone(), stage_budget).expect("staging arena should be created");
        add_path(&mut stage, &first).expect("first replacement should be retained");
        add_path(&mut stage, &second).expect("second replacement should be retained");
        stage
            .complete_directory(&target, None)
            .expect("staging target should complete");

        let error = live
            .replace_subtree_from(target_id, stage)
            .expect_err("graft should reject an over-budget replacement before mutation");

        assert!(matches!(error, ModelError::MemoryExhausted { .. }));
        let after = live
            .nodes()
            .map(|node| {
                (
                    node.id,
                    live.path_for(node.id)
                        .expect("live node path should be available"),
                    node.kind,
                    node.state,
                    node.metrics,
                    node.snapshot.clone(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(after, before);
        assert_eq!(live.path_for(old_id), Some(old));
        assert_eq!(live.memory_used(), memory_before);
    }
    #[test]
    fn deletion_rebuild_preserves_unknown_other_bounds() {
        let root = tempfile::tempdir().expect("model root should exist");
        let unknown = root.path().join("unknown");
        let removable = root.path().join("removable");
        fs::write(&removable, b"remove").expect("removable fixture should be written");
        let mut arena = test_arena(root.path());
        arena.max_children_per_directory = 0;
        arena
            .record_unscanned(&unknown, UnscannedReason::Metadata("denied".to_string()))
            .expect("unknown Other entry should be represented");
        arena.max_children_per_directory = DEFAULT_MAX_CHILDREN;
        let removable_id = add_path(&mut arena, &removable).expect("removable should be retained");
        arena
            .complete_directory(root.path(), None)
            .expect("root should complete");
        arena.finalize().expect("model should finalize");
        let other_id = arena
            .children(arena.root())
            .iter()
            .copied()
            .find(|id| {
                arena
                    .node(*id)
                    .is_some_and(|node| node.kind == NodeKind::Synthetic(SyntheticKind::Other))
            })
            .expect("Other entry should remain");
        assert_eq!(
            arena
                .node(other_id)
                .expect("Other entry should exist")
                .metrics
                .allocated_bytes,
            ByteBounds::unknown()
        );
        assert!(
            arena
                .try_remove_path(&removable)
                .expect("deletion rebuild should succeed")
        );
        assert!(arena.node(removable_id).is_none());
        let other = arena.node(other_id).expect("Other entry should survive");
        assert_eq!(other.metrics.allocated_bytes.upper, None);
        assert_eq!(other.metrics.reclaimable_bytes.upper, None);
    }

    #[cfg(unix)]
    #[test]
    fn deletion_rebuild_preserves_symbolic_link_metrics_in_other() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("link root should exist");
        let target = root.path().join("target");
        let link = root.path().join("link");
        let materialized = root.path().join("materialized");
        let removable = root.path().join("removable");
        fs::write(&target, b"target").expect("link target should be written");
        symlink(&target, &link).expect("symbolic link should be created");
        fs::write(&materialized, b"materialized").expect("materialized fixture should be written");
        fs::write(&removable, b"remove").expect("removable fixture should be written");

        let mut arena = test_arena(root.path());
        arena.max_children_per_directory = 0;
        arena
            .record_unscanned(&link, UnscannedReason::SymbolicLink)
            .expect("scanner link event should be represented");
        assert_eq!(arena.identity_count(), 0);
        arena.max_children_per_directory = DEFAULT_MAX_CHILDREN;
        let materialized_metadata =
            fs::symlink_metadata(&materialized).expect("materialized metadata should exist");
        let materialized_identity = identity_for(&materialized, &materialized_metadata)
            .expect("materialized identity should be readable")
            .expect("materialized file should have an identity");
        assert!(
            arena
                .add_entry_aggregated(&materialized, &materialized_metadata, materialized_identity)
                .expect("materialized entry should be aggregated")
                .is_none()
        );
        let removable_id = add_path(&mut arena, &removable).expect("removable should be retained");
        arena
            .complete_directory(root.path(), None)
            .expect("root should complete");
        arena.finalize().expect("model should finalize");

        let other_id = arena
            .children(arena.root())
            .iter()
            .copied()
            .find(|id| {
                arena
                    .node(*id)
                    .is_some_and(|node| node.kind == NodeKind::Synthetic(SyntheticKind::Other))
            })
            .expect("aggregated link should remain in Other");
        let before = arena.node(other_id).expect("Other should exist").metrics;
        assert!(before.allocated_bytes.upper.is_some());
        assert!(before.reclaimable_bytes.lower > 0);

        assert!(
            arena
                .try_remove_path(&removable)
                .expect("deletion rebuild should succeed")
        );
        assert!(arena.node(removable_id).is_none());
        let after = arena.node(other_id).expect("Other should survive").metrics;
        assert_eq!(after.allocated_bytes, before.allocated_bytes);
        assert_eq!(after.reclaimable_bytes, before.reclaimable_bytes);
        let root_metrics = arena.node(arena.root()).expect("root should exist").metrics;
        assert_eq!(root_metrics.allocated_bytes, before.allocated_bytes);
        assert_eq!(root_metrics.reclaimable_bytes, before.reclaimable_bytes);
    }

    #[cfg(unix)]
    #[test]
    fn deletion_rebuild_preserves_symbolic_link_metrics_in_aggregate() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("aggregate link root should exist");
        let aggregate_path = root.path().join("aggregate");
        let target = root.path().join("target");
        let link = aggregate_path.join("link");
        let removable = root.path().join("removable");
        fs::create_dir(&aggregate_path).expect("aggregate directory should be created");
        fs::write(&target, b"target").expect("link target should be written");
        symlink(&target, &link).expect("symbolic link should be created");
        fs::write(&removable, b"remove").expect("removable fixture should be written");

        let mut arena = test_arena(root.path());
        arena
            .record_unscanned(&link, UnscannedReason::SymbolicLink)
            .expect("scanner link event should be represented");
        assert_eq!(arena.identity_count(), 0);
        let removable_id = add_path(&mut arena, &removable).expect("removable should be retained");
        arena
            .complete_directory(&aggregate_path, None)
            .expect("aggregate directory should complete");
        arena
            .complete_directory(root.path(), None)
            .expect("root should complete");
        arena.finalize().expect("model should finalize");
        assert!(
            arena
                .aggregate_cold_subtree(&HashSet::from([arena.root()]))
                .expect("aggregate conversion should succeed")
        );

        let aggregate_id = arena
            .children(arena.root())
            .iter()
            .copied()
            .find(|id| {
                arena
                    .node(*id)
                    .is_some_and(|node| node.kind == NodeKind::Synthetic(SyntheticKind::Aggregate))
            })
            .expect("aggregated link should remain in Aggregate");
        let before = arena
            .node(aggregate_id)
            .expect("Aggregate should exist")
            .metrics;
        assert!(before.allocated_bytes.upper.is_some());
        assert_eq!(before.reclaimable_bytes.lower, 0);

        assert!(
            arena
                .try_remove_path(&removable)
                .expect("deletion rebuild should succeed")
        );
        assert!(arena.node(removable_id).is_none());
        let after = arena
            .node(aggregate_id)
            .expect("Aggregate should survive")
            .metrics;
        assert_eq!(after.allocated_bytes, before.allocated_bytes);
        assert_eq!(after.reclaimable_bytes, before.reclaimable_bytes);
        let root_metrics = arena.node(arena.root()).expect("root should exist").metrics;
        assert_eq!(root_metrics.allocated_bytes, before.allocated_bytes);
        assert_eq!(root_metrics.reclaimable_bytes, before.reclaimable_bytes);
    }
    #[test]
    fn aggregate_rebuild_preserves_unknown_non_leaf_bounds() {
        let root = tempfile::tempdir().expect("aggregate root should exist");
        let aggregate_path = root.path().join("aggregate");
        let unknown_directory = aggregate_path.join("unknown-directory");
        let known = unknown_directory.join("known");
        let removable = root.path().join("removable");
        fs::create_dir(&aggregate_path).expect("aggregate directory should be created");
        fs::create_dir(&unknown_directory).expect("unknown directory should be created");
        fs::write(&known, b"known").expect("known fixture should be written");
        fs::write(&removable, b"remove").expect("removable fixture should be written");

        let mut arena = test_arena(root.path());
        add_path(&mut arena, &aggregate_path).expect("aggregate path should be retained");
        add_path(&mut arena, &unknown_directory).expect("unknown directory should be retained");
        add_path(&mut arena, &known).expect("known child should be retained");
        arena
            .record_unscanned(
                &unknown_directory,
                UnscannedReason::Metadata("denied".to_string()),
            )
            .expect("unknown directory should be marked uncertain");
        let removable_id = add_path(&mut arena, &removable).expect("removable should be retained");
        for path in [&unknown_directory, &aggregate_path, root.path()] {
            arena
                .complete_directory(path, None)
                .expect("fixture directory should complete");
        }
        arena.finalize().expect("model should finalize");
        assert!(
            arena
                .aggregate_cold_subtree(&HashSet::from([arena.root()]))
                .expect("aggregate conversion should succeed")
        );
        let aggregate_id = arena
            .children(arena.root())
            .iter()
            .copied()
            .find(|id| {
                arena
                    .node(*id)
                    .is_some_and(|node| node.kind == NodeKind::Synthetic(SyntheticKind::Aggregate))
            })
            .expect("Aggregate node should remain");
        assert_eq!(
            arena
                .node(aggregate_id)
                .expect("Aggregate node should exist")
                .metrics
                .allocated_bytes
                .upper,
            None
        );

        assert!(arena.remove_path(&removable));
        assert!(arena.node(removable_id).is_none());
        let aggregate = arena
            .node(aggregate_id)
            .expect("Aggregate node should survive rebuild");
        assert_eq!(aggregate.metrics.allocated_bytes.upper, None);
        assert_eq!(aggregate.metrics.reclaimable_bytes.upper, None);
    }

    #[test]
    fn other_rebuild_preserves_unknown_non_leaf_bounds() {
        let root = tempfile::tempdir().expect("Other root should exist");
        let unknown_directory = root.path().join("unknown-directory");
        let known = unknown_directory.join("known");
        let removable = root.path().join("removable");
        fs::create_dir(&unknown_directory).expect("unknown directory should be created");
        fs::write(&known, b"known").expect("known fixture should be written");
        fs::write(&removable, b"remove").expect("removable fixture should be written");

        let mut arena = test_arena(root.path());
        arena.max_children_per_directory = 0;
        arena
            .record_unscanned(
                &unknown_directory,
                UnscannedReason::Metadata("denied".to_string()),
            )
            .expect("unknown directory should be retained in Other");
        let other_id = arena
            .children(arena.root())
            .iter()
            .copied()
            .find(|id| {
                arena
                    .node(*id)
                    .is_some_and(|node| node.kind == NodeKind::Synthetic(SyntheticKind::Other))
            })
            .expect("Other node should exist");
        assert_eq!(
            arena
                .node(other_id)
                .expect("Other node should remain")
                .metrics
                .allocated_bytes
                .upper,
            None
        );
        arena.max_children_per_directory = DEFAULT_MAX_CHILDREN;
        let removable_id = add_path(&mut arena, &removable).expect("removable should be retained");
        arena
            .complete_directory(root.path(), None)
            .expect("root should complete");
        arena.finalize().expect("model should finalize");

        assert!(arena.remove_path(&removable));
        assert!(arena.node(removable_id).is_none());
        let other = arena
            .node(other_id)
            .expect("Other node should survive rebuild");
        assert_eq!(other.metrics.allocated_bytes.upper, None);
        assert_eq!(other.metrics.reclaimable_bytes.upper, None);
    }

    #[cfg(windows)]
    #[test]
    fn deletion_rebuild_preserves_unscanned_reparse_metrics() {
        let root = tempfile::tempdir().expect("reparse root should exist");
        let reparse = root.path().join("reparse");
        let removable = root.path().join("removable");
        fs::write(&reparse, b"reparse fixture").expect("reparse fixture should be written");
        fs::write(&removable, b"remove").expect("removable fixture should be written");

        let mut arena = test_arena(root.path());
        arena.max_children_per_directory = 0;
        arena
            .record_unscanned(&reparse, UnscannedReason::SymbolicLink)
            .expect("scanner reparse event should be represented");
        assert_eq!(arena.identity_count(), 0);
        arena.max_children_per_directory = DEFAULT_MAX_CHILDREN;
        let removable_id = add_path(&mut arena, &removable).expect("removable should be retained");
        arena
            .complete_directory(root.path(), None)
            .expect("root should complete");
        arena.finalize().expect("model should finalize");

        let other_id = arena
            .children(arena.root())
            .iter()
            .copied()
            .find(|id| {
                arena
                    .node(*id)
                    .is_some_and(|node| node.kind == NodeKind::Synthetic(SyntheticKind::Other))
            })
            .expect("aggregated reparse should remain in Other");
        let before = arena.node(other_id).expect("Other should exist").metrics;
        assert!(before.allocated_bytes.upper.is_some());
        assert_eq!(before.reclaimable_bytes.lower, 0);

        assert!(
            arena
                .try_remove_path(&removable)
                .expect("deletion rebuild should succeed")
        );
        assert!(arena.node(removable_id).is_none());
        let after = arena.node(other_id).expect("Other should survive").metrics;
        assert_eq!(after.allocated_bytes, before.allocated_bytes);
        assert_eq!(after.reclaimable_bytes, before.reclaimable_bytes);
    }

    #[test]
    fn deletion_rebuild_preserves_unknown_aggregate_bounds() {
        let root = tempfile::tempdir().expect("aggregate root should exist");
        let aggregate_path = root.path().join("aggregate");
        let aggregate_unknown = aggregate_path.join("unknown");
        let removable = root.path().join("removable");
        fs::create_dir(&aggregate_path).expect("aggregate directory should be created");
        fs::write(&removable, b"remove").expect("removable fixture should be written");
        let mut arena = test_arena(root.path());
        arena
            .record_unscanned(
                &aggregate_unknown,
                UnscannedReason::Metadata("denied".to_string()),
            )
            .expect("unknown aggregate entry should be represented");
        let removable_id = add_path(&mut arena, &removable).expect("removable should be retained");
        arena
            .complete_directory(&aggregate_path, None)
            .expect("aggregate directory should complete");
        arena
            .complete_directory(root.path(), None)
            .expect("aggregate root should complete");
        arena.finalize().expect("aggregate model should finalize");
        assert!(
            arena
                .aggregate_cold_subtree(&HashSet::from([arena.root()]))
                .expect("aggregate conversion should succeed")
        );
        let aggregate_id = arena
            .children(arena.root())
            .iter()
            .copied()
            .find(|id| {
                arena
                    .node(*id)
                    .is_some_and(|node| node.kind == NodeKind::Synthetic(SyntheticKind::Aggregate))
            })
            .expect("Aggregate entry should remain");
        assert_eq!(
            arena
                .node(aggregate_id)
                .expect("Aggregate entry should exist")
                .metrics
                .allocated_bytes
                .upper,
            None
        );
        assert!(
            arena
                .try_remove_path(&removable)
                .expect("aggregate deletion rebuild should succeed")
        );
        assert!(arena.node(removable_id).is_none());
        let aggregate = arena
            .node(aggregate_id)
            .expect("Aggregate entry should survive");
        assert_eq!(aggregate.metrics.allocated_bytes.upper, None);
        assert_eq!(aggregate.metrics.reclaimable_bytes.upper, None);
    }

    #[cfg(unix)]
    #[test]
    fn deletion_rebuild_does_not_double_count_surviving_reparse_objects() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("model root should exist");
        let target = root.path().join("target");
        let link = root.path().join("link");
        let removable = root.path().join("removable");
        fs::write(&target, b"target").expect("link target should be written");
        symlink(&target, &link).expect("symlink should be created");
        fs::write(&removable, b"remove").expect("removable fixture should be written");
        let mut arena = test_arena(root.path());
        let link_metadata = fs::symlink_metadata(&link).expect("link metadata should exist");
        let link_identity = identity_for(&link, &link_metadata)
            .expect("link identity should be readable")
            .expect("link identity should be available");
        let link_id = arena
            .add_entry(&link, &link_metadata, link_identity)
            .expect("link should be added")
            .expect("link should be retained");
        let removable_id = add_path(&mut arena, &removable).expect("removable should be added");
        arena
            .complete_directory(root.path(), None)
            .expect("root should complete");
        arena.finalize().expect("model should finalize");
        let before = arena
            .node(link_id)
            .expect("link should exist")
            .metrics
            .allocated_bytes;

        assert!(
            arena
                .try_remove_path(&removable)
                .expect("deletion rebuild should succeed")
        );
        assert!(arena.node(removable_id).is_none());
        assert_eq!(
            arena
                .node(link_id)
                .expect("surviving link should exist")
                .metrics
                .allocated_bytes,
            before
        );
    }
    #[test]
    fn corrupt_identity_spill_returns_error_without_panicking() {
        let root = tempfile::tempdir().expect("model root should exist");
        let path = root.path().join("target");
        fs::write(&path, b"payload").expect("target fixture should be written");
        let mut arena = test_arena(root.path());
        let target_id = add_path(&mut arena, &path).expect("target should be retained");
        arena
            .complete_directory(root.path(), None)
            .expect("root should complete");
        arena.finalize().expect("model should finalize");

        let mut corrupt = IdentityStore::new(1).expect("spill store should initialize");
        let file_id = FileId::new_inode(17, 1);
        corrupt
            .observe(
                &file_id,
                Some(1),
                ByteBounds::exact(4096),
                Some(NodeId(1)),
                Some(NodeId(1)),
            )
            .expect("identity should spill");
        #[cfg(windows)]
        corrupt
            .corrupt_spill_record_for_test(&file_id)
            .expect("spill record should be corruptible through the database");
        #[cfg(not(windows))]
        {
            let spill_path = corrupt
                .spill_path()
                .expect("spilled store should expose its path")
                .to_path_buf();
            std::fs::write(spill_path.join("identities.redb"), b"corrupt")
                .expect("spill database should be corruptible");
        }
        arena.identities = corrupt;

        let error = arena
            .try_remove_path(&path)
            .expect_err("corrupt identity spill should be reported");
        assert!(error.to_string().contains("identity"));
        assert!(arena.node(target_id).is_some());
    }
}
