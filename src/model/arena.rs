use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs::{self, Metadata};
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use file_id::FileId;

use super::identity_store::{IdentityRecord, IdentityStore};
use super::{
    ByteBounds, EntrySnapshot, MemoryBudget, ModelError, Node, NodeId, NodeKind, NodeMetrics,
    NodeState, SyntheticKind, UnscannedReason,
};
use crate::native_path::{NativeIdentity, identity_for};
use crate::os::physical_size;

const NODE_SLOT_BYTES: usize = size_of::<Option<Box<Node>>>();
const NODE_OVERHEAD: usize = NODE_SLOT_BYTES + size_of::<Node>() + 96;
const DUPLICATE_ID_OVERHEAD: usize = size_of::<FileId>() + 64;
const DEFAULT_MAX_CHILDREN: usize = 4_096;

pub struct Arena {
    nodes: Vec<Option<Box<Node>>>,
    free_nodes: Vec<NodeId>,
    lookup: HashMap<(NodeId, Arc<OsStr>), NodeId>,
    root: NodeId,
    root_path: PathBuf,
    budget: MemoryBudget,
    identities: IdentityStore,
    duplicate_identities: HashSet<FileId>,
    access_tick: u64,
    max_children_per_directory: usize,
}

#[allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    reason = "Arena operations share ModelError as their uniform boundary for filesystem paths, bounded storage, and identity persistence; repeating that contract on each method obscures the model API."
)]
impl Arena {
    pub fn new(root_path: PathBuf, mut budget: MemoryBudget) -> Result<Self, ModelError> {
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
            free_nodes: Vec::new(),
            lookup: HashMap::new(),
            root,
            root_path,
            budget,
            identities: IdentityStore::new(identity_budget)?,
            duplicate_identities: HashSet::new(),
            access_tick: 0,
            max_children_per_directory: DEFAULT_MAX_CHILDREN,
        };
        arena.reserve_node(&root_name)?;
        arena.nodes.push(Some(Box::new(Node::new(
            root,
            None,
            root_name,
            NodeKind::Root,
            NodeState::Scanning,
            root_snapshot,
        ))));
        Ok(arena)
    }

    #[must_use]
    pub const fn root(&self) -> NodeId {
        self.root
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
    pub fn internal_scan_paths(&self) -> Vec<PathBuf> {
        self.identities.internal_scan_paths()
    }

    #[must_use]
    pub fn node(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(id.index()).and_then(Option::as_deref)
    }

    pub fn node_mut(&mut self, id: NodeId) -> Option<&mut Node> {
        self.nodes
            .get_mut(id.index())
            .and_then(Option::as_deref_mut)
    }

    pub fn nodes(&self) -> impl Iterator<Item = &Node> {
        self.nodes.iter().filter_map(Option::as_deref)
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

        let kind = if metadata.is_dir() {
            NodeKind::Directory
        } else if metadata.file_type().is_symlink() {
            NodeKind::Link
        } else {
            NodeKind::File
        };
        let apparent = if metadata.is_dir() {
            0
        } else {
            u128::from(metadata.len())
        };
        let allocated = if metadata.is_dir() {
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
            allocated_bytes: if kind == NodeKind::File && !cfg!(windows) {
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
                Some(parent),
                Some(other),
            )?;
            self.accumulate_other(parent, other, metrics);
            return Ok(None);
        }
        if let Some(victim) = replacement {
            let other = self.ensure_other(parent)?;
            self.aggregate_child_into_other(victim, other)?;
        }
        if self.reserve_child(&name).is_err() {
            let other = self.ensure_other(parent)?;
            let metrics = self.observe_leaf_metrics(
                kind,
                apparent,
                allocated,
                &identity,
                Some(parent),
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
            } else {
                let unknown = matches!(reason, UnscannedReason::Metadata(_));
                if let Some(node) = self.node_mut(existing) {
                    node.state = unscanned_state(&reason);
                    node.unscanned_reason = Some(reason);
                    if scoped_zero {
                        node.metrics = NodeMetrics::default();
                    } else if unknown {
                        node.metrics.allocated_bytes.upper = None;
                        node.metrics.reclaimable_bytes.upper = None;
                    }
                }
                if scoped_zero {
                    self.rebuild_metrics();
                }
                return Ok(());
            }
        }
        let metadata = fs::symlink_metadata(path).ok();
        let kind = if metadata.as_ref().is_some_and(Metadata::is_dir) {
            NodeKind::Directory
        } else if metadata
            .as_ref()
            .is_some_and(|metadata| metadata.file_type().is_symlink())
        {
            NodeKind::Link
        } else {
            NodeKind::File
        };
        let apparent = metadata
            .as_ref()
            .map_or(0, |metadata| u128::from(metadata.len()));
        let allocated = metadata
            .as_ref()
            .map_or_else(ByteBounds::unknown, |metadata| {
                if metadata.is_dir() {
                    ByteBounds::exact(0)
                } else {
                    physical_size(path, metadata)
                        .map(u128::from)
                        .map_or_else(|_| ByteBounds::unknown(), ByteBounds::exact)
                }
            });

        let metrics = if scoped_zero {
            NodeMetrics::default()
        } else {
            leaf_metrics(apparent, allocated, None)
        };
        let at_child_limit = self.retained_child_count(parent) >= self.max_children_per_directory;
        let replacement = if !aggregate_at_parent && at_child_limit {
            self.smallest_retained_child(parent)
                .filter(|victim| self.candidate_outranks(name.as_ref(), metrics, *victim))
        } else {
            None
        };
        if aggregate_at_parent || (at_child_limit && replacement.is_none()) {
            let other = self.ensure_other(parent)?;
            self.accumulate_other(parent, other, metrics);
            return Ok(());
        }
        if let Some(victim) = replacement {
            let other = self.ensure_other(parent)?;
            self.aggregate_child_into_other(victim, other)?;
        }
        if self.reserve_child(&name).is_err() {
            let other = self.ensure_other(parent)?;
            self.accumulate_other(parent, other, metrics);
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
                identity: metadata
                    .as_ref()
                    .and_then(|metadata| identity_for(path, metadata).ok().flatten()),
                kind,
                apparent_bytes: apparent,
                allocated_bytes: if kind == NodeKind::File && !cfg!(windows) {
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
        self.lookup.insert((parent, name), id);
        self.push_child(parent, id)?;
        self.propagate_add(parent, metrics);
        self.propagate_descendant(parent, 1);
        Ok(())
    }

    pub fn complete_directory(&mut self, path: &Path) -> Result<(), ModelError> {
        if let Some(id) = self.find_path(path) {
            if let Some(node) = self.node_mut(id)
                && node.state == NodeState::Scanning
            {
                node.state = NodeState::Complete;
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
                    .copied()
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
            .filter_map(Option::as_deref)
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
        let children = self.children(candidate).to_vec();
        let mut metrics = self
            .node(candidate)
            .map_or(NodeMetrics::default(), |node| node.metrics);
        metrics.descendants = metrics.descendants.saturating_add(1);
        let mut removed = Vec::new();
        for child in children {
            self.collect_subtree_ids(child, &mut removed);
        }
        self.identities
            .remap_removed_nodes(&mut removed, candidate)?;
        self.remove_reusable_nodes(removed);
        if let Some(node) = self.node_mut(candidate) {
            node.children.clear();
            node.kind = NodeKind::Synthetic(SyntheticKind::Aggregate);
            node.state = NodeState::Aggregated;
            node.snapshot.kind = node.kind;
            node.metrics = metrics;
            node.unscanned_reason = Some(UnscannedReason::MemoryAggregation);
        }
        self.rebuild_metrics();
        Ok(true)
    }

    pub fn remove_subtree(&mut self, root: NodeId) {
        let mut removed = Vec::new();
        self.collect_subtree_ids(root, &mut removed);
        self.remove_nodes(removed);
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

    fn remove_nodes(&mut self, removed: Vec<NodeId>) {
        self.remove_nodes_with_reuse(removed, false);
    }

    fn remove_reusable_nodes(&mut self, removed: Vec<NodeId>) {
        self.remove_nodes_with_reuse(removed, true);
    }

    fn remove_nodes_with_reuse(&mut self, removed: Vec<NodeId>, reuse_ids: bool) {
        for id in removed.into_iter().rev() {
            if let Some(node) = self.nodes.get_mut(id.index()).and_then(Option::take) {
                if let Some(parent) = node.parent {
                    self.lookup.remove(&(parent, node.name.clone()));
                    if let Some(parent_node) = self.node_mut(parent) {
                        parent_node.children.retain(|child| *child != id);
                        parent_node.children.shrink_to_fit();
                    }
                }
                let releasable = estimate_node(&node.name)
                    .saturating_sub(NODE_SLOT_BYTES)
                    .saturating_add(node.parent.map_or(0, |_| size_of::<NodeId>()));
                if reuse_ids && id != self.root {
                    self.budget
                        .release(releasable.saturating_sub(size_of::<NodeId>()));
                    self.free_nodes.push(id);
                } else {
                    self.budget.release(releasable);
                }
            }
        }
        self.lookup.shrink_to_fit();
    }

    pub fn remove_path(&mut self, path: &Path) -> bool {
        let Some(id) = self.find_path(path) else {
            return false;
        };
        if id == self.root {
            return false;
        }
        self.remove_subtree(id);
        true
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
        let target_kind = self.node(target).map(|node| node.kind).ok_or_else(|| {
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
            self.plan_staged_nodes(&mut staging, &removed, &shared, released)?;
        remap_staged_nodes(&mut staging, target)?;
        let mut identities = self.rebuild_focused_identities(target, &removed)?;
        merge_staged_identities(&mut identities, &mut staging, target)?;
        identities.visit_records(|_, _| Ok(()))?;
        let identity_scratch = IdentityStore::new(self.identities.memory_limit())?;
        let children = staged_children
            .into_iter()
            .map(|child| staged_live_id(&staging.nodes, staging.root, child, target))
            .collect::<Result<Vec<_>, _>>()?;

        self.remove_reusable_nodes(removed);
        self.remove_reusable_nodes(shared);
        let mut consumed_reused_slots = 0_usize;
        for index in 1..staging.nodes.len() {
            let Some(node) = staging.nodes[index].take() else {
                continue;
            };
            let node = *node;
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
            self.insert_planned_node(node.id, node);
        }
        debug_assert_eq!(consumed_reused_slots, reused_slots);
        let replacement_kind = if target == self.root {
            NodeKind::Root
        } else {
            NodeKind::Directory
        };
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

        self.budget = planned_budget;
        self.identities = identities;
        self.prepare_identity_metrics();
        self.rebuild_identity_metrics(identity_scratch);
        Ok(())
    }

    fn shared_nodes_outside(&self, target: NodeId, removed: &[NodeId]) -> Vec<NodeId> {
        self.nodes
            .iter()
            .filter_map(Option::as_deref)
            .filter(|node| {
                node.kind == NodeKind::Synthetic(SyntheticKind::Shared)
                    && node.id != target
                    && removed.binary_search(&node.id).is_err()
            })
            .map(|node| node.id)
            .collect()
    }

    fn plan_staged_nodes(
        &self,
        staging: &mut Arena,
        removed: &[NodeId],
        shared: &[NodeId],
        released: usize,
    ) -> Result<(MemoryBudget, usize), ModelError> {
        let mut budget = self.budget.clone();
        budget.release(released);
        let mut shared_ids = shared.iter().copied();
        let mut removed_ids = removed.iter().copied();
        let mut free_ids = self.free_nodes.iter().rev().copied();
        let mut next_append = self.nodes.len();
        let mut reused_slots = 0_usize;
        for node in staging
            .nodes
            .iter_mut()
            .skip(1)
            .filter_map(Option::as_deref_mut)
        {
            let reusable = shared_ids
                .next()
                .or_else(|| removed_ids.next())
                .or_else(|| free_ids.next());
            let bytes = estimate_node(&node.name).saturating_add(size_of::<NodeId>());
            let bytes = if reusable.is_some() {
                bytes
                    .saturating_sub(NODE_SLOT_BYTES)
                    .saturating_sub(size_of::<NodeId>())
            } else {
                bytes
            };
            budget.reserve(bytes)?;
            node.id = if let Some(id) = reusable {
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
        }
        Ok((budget, reused_slots))
    }

    fn insert_planned_node(&mut self, id: NodeId, node: Node) {
        if id.index() < self.nodes.len() {
            debug_assert!(self.nodes[id.index()].is_none());
            self.nodes[id.index()] = Some(Box::new(node));
        } else {
            debug_assert_eq!(id.index(), self.nodes.len());
            self.nodes.push(Some(Box::new(node)));
        }
    }

    fn rebuild_focused_identities(
        &mut self,
        target: NodeId,
        removed: &[NodeId],
    ) -> Result<IdentityStore, ModelError> {
        let identity_limit = self.identities.memory_limit().min(self.budget.headroom());
        let mut rebuilt = IdentityStore::new(identity_limit)?;
        self.identities.visit_records(|file_id, record| {
            if let Some(record) = remove_replaced_participants(record, target, removed) {
                merge_identity_record(&mut rebuilt, &file_id, record)?;
            }
            Ok(())
        })?;
        Ok(rebuilt)
    }

    fn prepare_identity_metrics(&mut self) {
        for node in self.nodes.iter_mut().filter_map(Option::as_deref_mut) {
            match node.kind {
                NodeKind::File if node.state == NodeState::Complete => {
                    let links = node
                        .snapshot
                        .identity
                        .as_ref()
                        .and_then(|identity| identity.link_count);
                    node.metrics =
                        leaf_metrics(node.snapshot.apparent_bytes, ByteBounds::exact(0), links);
                }
                NodeKind::Synthetic(SyntheticKind::Other | SyntheticKind::Aggregate) => {
                    node.metrics.allocated_bytes = ByteBounds::exact(0);
                    node.metrics.reclaimable_bytes = ByteBounds::exact(0);
                }
                NodeKind::File
                | NodeKind::Root
                | NodeKind::Directory
                | NodeKind::Link
                | NodeKind::Synthetic(SyntheticKind::Shared) => {}
            }
        }
    }
    fn rebuild_identity_metrics(&mut self, replacement: IdentityStore) {
        let mut identities = std::mem::replace(&mut self.identities, replacement);
        let duplicate_bytes = self
            .duplicate_identities
            .len()
            .saturating_mul(DUPLICATE_ID_OVERHEAD);
        self.budget.release(duplicate_bytes);
        self.duplicate_identities.clear();
        identities
            .visit_records(|file_id, record| {
                self.restore_identity_allocation(&record);
                if record.observed_links > 1 {
                    self.track_duplicate(file_id);
                }
                Ok(())
            })
            .expect("preflighted identity records should remain readable");
        self.identities = identities;
        self.rebuild_metrics();
        self.finalize()
            .expect("preflighted identity finalization should remain valid");
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
        let id = self.allocate_child_id(&name)?;
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
        let (is_new, _) = self.identities.observe(
            &identity.file_id,
            identity.link_count,
            allocated,
            node,
            allocation_node,
        )?;
        Ok(if is_new {
            leaf_metrics(apparent, allocated, identity.link_count)
        } else {
            self.track_duplicate(identity.file_id);
            leaf_metrics(apparent, ByteBounds::exact(0), identity.link_count)
        })
    }

    fn retained_child_count(&self, parent: NodeId) -> usize {
        self.children(parent)
            .iter()
            .filter(|id| {
                self.node(**id)
                    .is_some_and(|node| !node.kind.is_synthetic())
            })
            .count()
    }

    fn smallest_retained_child(&self, parent: NodeId) -> Option<NodeId> {
        self.children(parent)
            .iter()
            .copied()
            .filter(|id| self.node(*id).is_some_and(|node| !node.kind.is_synthetic()))
            .min_by(|left, right| self.retention_order(*left, *right))
    }

    fn retention_order(&self, left: NodeId, right: NodeId) -> std::cmp::Ordering {
        let Some(left) = self.node(left) else {
            return std::cmp::Ordering::Less;
        };
        let Some(right) = self.node(right) else {
            return std::cmp::Ordering::Greater;
        };
        retention_rank(left.metrics)
            .cmp(&retention_rank(right.metrics))
            .then_with(|| right.name.cmp(&left.name))
            .then_with(|| right.id.cmp(&left.id))
    }

    fn candidate_outranks(&self, name: &OsStr, metrics: NodeMetrics, victim: NodeId) -> bool {
        let Some(victim) = self.node(victim) else {
            return true;
        };
        match retention_rank(metrics).cmp(&retention_rank(victim.metrics)) {
            std::cmp::Ordering::Greater => true,
            std::cmp::Ordering::Less => false,
            std::cmp::Ordering::Equal => name.cmp(victim.name.as_ref()).is_lt(),
        }
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
        let mut removed = Vec::new();
        self.collect_subtree_ids(child, &mut removed);
        removed.sort_unstable();
        if let Some(identity) = leaf_identity {
            self.identities
                .remap_nodes_for_identity(&identity.file_id, &removed, other)?;
        } else {
            self.identities.remap_removed_nodes(&mut removed, other)?;
        }
        self.add_to_other(other, metrics);
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
        let id = self.allocate_child_id(&name)?;
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
        self.children(parent)
            .iter()
            .copied()
            .find(|id| {
                self.node(*id)
                    .is_some_and(|node| node.kind == NodeKind::Synthetic(SyntheticKind::Other))
            })
            .map_or_else(
                || {
                    self.add_synthetic(
                        parent,
                        "Other",
                        SyntheticKind::Other,
                        NodeMetrics::default(),
                    )
                },
                Ok,
            )
    }

    fn add_to_other(&mut self, other: NodeId, metrics: NodeMetrics) {
        if let Some(node) = self.node_mut(other) {
            node.metrics.add(metrics);
            node.metrics.descendants = node.metrics.descendants.saturating_add(1);
        }
    }

    fn accumulate_other(&mut self, parent: NodeId, other: NodeId, metrics: NodeMetrics) {
        self.add_to_other(other, metrics);
        self.propagate_add(parent, metrics);
        self.propagate_descendant(parent, 1);
    }

    fn rebuild_metrics(&mut self) {
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
                        Some(UnscannedReason::Metadata(_))
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
        let name = self
            .node(child)
            .map(|node| node.name.clone())
            .ok_or_else(|| ModelError::Invariant("child node missing".to_string()))?;
        let children = self.children(parent);
        let position = children
            .iter()
            .position(|existing| {
                self.node(*existing)
                    .is_some_and(|node| node.name.cmp(&name).is_gt())
            })
            .unwrap_or(children.len());
        self.node_mut(parent)
            .ok_or_else(|| ModelError::Invariant("parent node missing".to_string()))?
            .children
            .insert(position, child);
        Ok(())
    }

    fn propagate_add(&mut self, mut id: NodeId, metrics: NodeMetrics) {
        loop {
            let Some(node) = self.node_mut(id) else {
                break;
            };
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
        loop {
            let Some(node) = self.node_mut(id) else {
                break;
            };
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
        if self.budget.reserve(DUPLICATE_ID_OVERHEAD).is_ok() {
            self.duplicate_identities.insert(file_id);
        }
    }

    fn reserve_node(&mut self, name: &OsStr) -> Result<(), ModelError> {
        self.budget.reserve(estimate_node(name))
    }

    fn reserve_child(&mut self, name: &OsStr) -> Result<(), ModelError> {
        let node_bytes = estimate_node(name);
        let node_bytes = if self.free_nodes.is_empty() {
            node_bytes
        } else {
            node_bytes.saturating_sub(NODE_SLOT_BYTES)
        };
        self.budget
            .reserve(node_bytes.saturating_add(size_of::<NodeId>()))
    }

    fn allocate_child_id(&mut self, name: &OsStr) -> Result<NodeId, ModelError> {
        self.reserve_child(name)?;
        self.next_id()
    }

    fn insert_node(&mut self, id: NodeId, node: Node) -> Result<(), ModelError> {
        if id.index() < self.nodes.len() {
            let slot = self.nodes.get_mut(id.index()).ok_or_else(|| {
                ModelError::Invariant("reusable node slot disappeared".to_string())
            })?;
            if slot.is_some() {
                return Err(ModelError::Invariant(
                    "reusable node slot was occupied".to_string(),
                ));
            }
            *slot = Some(Box::new(node));
            return Ok(());
        }
        if id.index() != self.nodes.len() {
            return Err(ModelError::Invariant(
                "new node ID did not match the next arena slot".to_string(),
            ));
        }
        self.nodes.push(Some(Box::new(node)));
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
    let released = estimate_node(&node.name)
        .saturating_sub(NODE_SLOT_BYTES)
        .saturating_add(node.parent.map_or(0, |_| size_of::<NodeId>()));
    if node.parent.is_some() {
        released.saturating_sub(size_of::<NodeId>())
    } else {
        released
    }
}

fn staged_live_id(
    nodes: &[Option<Box<Node>>],
    stage_root: NodeId,
    id: NodeId,
    target: NodeId,
) -> Result<NodeId, ModelError> {
    if id == stage_root {
        return Ok(target);
    }
    nodes
        .get(id.index())
        .and_then(Option::as_deref)
        .map(|node| node.id)
        .ok_or_else(|| ModelError::Invariant("staged node mapping disappeared".to_string()))
}

fn remap_staged_nodes(staging: &mut Arena, target: NodeId) -> Result<(), ModelError> {
    let stage_root = staging.root;
    for index in 1..staging.nodes.len() {
        let (parent, children) = {
            let node = staging.nodes[index].as_deref().ok_or_else(|| {
                ModelError::Invariant("staged node mapping disappeared".to_string())
            })?;
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
            (Some(parent), children)
        };
        let node = staging.nodes[index]
            .as_deref_mut()
            .ok_or_else(|| ModelError::Invariant("staged node mapping disappeared".to_string()))?;
        node.parent = parent;
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
        .filter(|id| is_replaced_node(**id, target, removed))
        .count();
    record
        .nodes
        .retain(|id| !is_replaced_node(*id, target, removed));
    record.observed_links = record
        .observed_links
        .saturating_sub(u64::try_from(removed_links).unwrap_or(u64::MAX));
    if record.nodes.is_empty() {
        return None;
    }
    if record
        .allocation_node
        .is_some_and(|id| is_replaced_node(id, target, removed))
    {
        record.allocation_node = record.nodes.first().copied();
    }
    Some(record)
}

fn remap_staged_record(
    record: &mut IdentityRecord,
    nodes: &[Option<Box<Node>>],
    stage_root: NodeId,
    target: NodeId,
) -> Result<(), ModelError> {
    for id in &mut record.nodes {
        *id = staged_live_id(nodes, stage_root, *id, target)?;
    }
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
        existing.declared_links = match (existing.declared_links, record.declared_links) {
            (Some(left), Some(right)) => Some(left.max(right)),
            (left @ Some(_), None) | (None, left @ Some(_)) => left,
            (None, None) => None,
        };
        existing.allocated_bytes =
            conservative_bounds(existing.allocated_bytes, record.allocated_bytes);
        if existing.allocation_node.is_none() {
            existing.allocation_node = record.allocation_node;
        }
        existing.nodes.extend(record.nodes);
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
        | UnscannedReason::Metadata(_) => NodeState::Uncertain,
    }
}

fn has_zero_scoped_metrics(reason: &UnscannedReason) -> bool {
    matches!(
        reason,
        UnscannedReason::FilesystemBoundary | UnscannedReason::Excluded(_)
    )
}

fn retention_rank(metrics: NodeMetrics) -> (bool, u128) {
    (
        metrics.allocated_bytes.upper.is_none(),
        metrics.allocated_bytes.lower,
    )
}

fn estimate_node(name: &OsStr) -> usize {
    NODE_OVERHEAD.saturating_add(name.as_encoded_bytes().len().saturating_mul(2))
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
            .complete_directory(&cold.join("nested"))
            .expect("completion below a compacted subtree should be harmless");
        assert_eq!(arena.children(pinned_id).len(), 1);
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
            .complete_directory(&directory)
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
            .complete_directory(&directory)
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
            live.complete_directory(path)
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
            .complete_directory(&target)
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
}
