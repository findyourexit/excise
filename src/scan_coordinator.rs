use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};

const MAX_FOCUS_PATHS: usize = 32;
const FOREGROUND_LEASE_BURST: u8 = 4;

/// Monotonic identifier for one coherent scan result generation.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct ScanGeneration(u64);

impl ScanGeneration {
    #[must_use]
    pub(crate) const fn initial() -> Self {
        Self(0)
    }

    #[must_use]
    pub(crate) const fn from_value(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub(crate) const fn value(self) -> u64 {
        self.0
    }
}

/// A normalized root-relative path used as a deterministic scheduler key.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct RelativePath(Vec<OsString>);

impl RelativePath {
    /// # Errors
    ///
    /// Returns [`RelativePathError::NotRelative`] when `path` contains a root,
    /// prefix, or parent component.
    pub(crate) fn from_path(path: &Path) -> Result<Self, RelativePathError> {
        let mut components = Vec::new();
        for component in path.components() {
            match component {
                Component::CurDir => {}
                Component::Normal(name) => components.push(name.to_os_string()),
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    return Err(RelativePathError::NotRelative);
                }
            }
        }
        Self::from_components(components)
    }

    /// # Errors
    ///
    /// Returns [`RelativePathError::NotRelative`] when an input is not exactly
    /// one native path component.
    pub(crate) fn from_components(components: Vec<OsString>) -> Result<Self, RelativePathError> {
        if components.iter().any(|component| {
            let mut parsed = Path::new(component).components();
            !matches!(
                (parsed.next(), parsed.next()),
                (Some(Component::Normal(name)), None) if name == component.as_os_str()
            )
        }) {
            return Err(RelativePathError::NotRelative);
        }
        Ok(Self(components))
    }

    #[must_use]
    pub(crate) const fn root() -> Self {
        Self(Vec::new())
    }

    #[must_use]
    pub(crate) const fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    #[must_use]
    pub(crate) const fn depth(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub(crate) fn is_direct_child_of(&self, parent: &Self) -> bool {
        self.depth() == parent.depth().saturating_add(1) && self.starts_with(parent)
    }

    #[must_use]
    pub(crate) fn to_path_buf(&self) -> PathBuf {
        let mut path = PathBuf::new();
        for component in &self.0 {
            path.push(component);
        }
        path
    }

    #[must_use]
    pub(crate) fn components(&self) -> &[OsString] {
        &self.0
    }

    #[must_use]
    pub(crate) fn starts_with(&self, prefix: &Self) -> bool {
        self.0.starts_with(&prefix.0)
    }
}

impl Ord for RelativePath {
    fn cmp(&self, other: &Self) -> Ordering {
        let mut left = self.0.iter();
        let mut right = other.0.iter();
        loop {
            match (left.next(), right.next()) {
                (Some(left), Some(right)) => {
                    let ordering = compare_native_components(left, right);
                    if ordering != Ordering::Equal {
                        return ordering;
                    }
                }
                (None, Some(_)) => return Ordering::Less,
                (Some(_), None) => return Ordering::Greater,
                (None, None) => return Ordering::Equal,
            }
        }
    }
}

impl PartialOrd for RelativePath {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(unix)]
fn compare_native_components(left: &OsStr, right: &OsStr) -> Ordering {
    use std::os::unix::ffi::OsStrExt as _;

    left.as_bytes().cmp(right.as_bytes())
}

#[cfg(windows)]
fn compare_native_components(left: &OsStr, right: &OsStr) -> Ordering {
    use std::os::windows::ffi::OsStrExt as _;

    left.encode_wide().cmp(right.encode_wide())
}

#[cfg(not(any(unix, windows)))]
fn compare_native_components(left: &OsStr, right: &OsStr) -> Ordering {
    left.cmp(right)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RelativePathError {
    NotRelative,
}

/// A purpose-specific work item. The scheduler never accepts untyped jobs.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) enum WorkKind {
    EnumerateDirectory,
    ReduceRun,
    PlanDeletion,
    ExecuteDeletion,
    RefreshSubtree,
}

/// Semantic scheduling class. Smaller discriminants are more urgent.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) enum WorkPriority {
    Safety,
    Foreground,
    Reducer,
    Background,
    Prefetch,
}

impl WorkPriority {
    const ALL: [Self; 5] = [
        Self::Safety,
        Self::Foreground,
        Self::Reducer,
        Self::Background,
        Self::Prefetch,
    ];
}

/// Stable deduplication key for one unit of session work.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct WorkKey {
    generation: ScanGeneration,
    kind: WorkKind,
    path: RelativePath,
}

impl WorkKey {
    #[must_use]
    pub(crate) const fn new(
        generation: ScanGeneration,
        kind: WorkKind,
        path: RelativePath,
    ) -> Self {
        Self {
            generation,
            kind,
            path,
        }
    }

    #[must_use]
    pub(crate) const fn generation(&self) -> ScanGeneration {
        self.generation
    }

    #[must_use]
    pub(crate) const fn kind(&self) -> WorkKind {
        self.kind
    }

    #[must_use]
    pub(crate) const fn path(&self) -> &RelativePath {
        &self.path
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct LeaseId(u64);

impl LeaseId {
    #[must_use]
    pub(crate) const fn value(self) -> u64 {
        self.0
    }
}

/// Immutable grant for one worker. Results must echo this value verbatim.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WorkLease {
    id: LeaseId,
    key: WorkKey,
    priority: WorkPriority,
    focused: bool,
}

impl WorkLease {
    #[must_use]
    pub(crate) const fn id(&self) -> LeaseId {
        self.id
    }

    #[must_use]
    pub(crate) const fn key(&self) -> &WorkKey {
        &self.key
    }

    #[must_use]
    pub(crate) const fn priority(&self) -> WorkPriority {
        self.priority
    }

    #[must_use]
    pub(crate) const fn is_focused(&self) -> bool {
        self.focused
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ScheduleOutcome {
    Enqueued,
    PriorityRaised,
    AlreadyPending,
    AlreadyLeased,
    StaleGeneration,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CompletionOutcome {
    Accepted,
    StaleLease,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct WorkCounts {
    pub(crate) safety: usize,
    pub(crate) foreground: usize,
    pub(crate) reducer: usize,
    pub(crate) background: usize,
    pub(crate) prefetch: usize,
}

/// Coalesced scheduler state suitable for a bounded UI status snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SchedulerSnapshot {
    pub(crate) generation: ScanGeneration,
    pub(crate) pending: WorkCounts,
    pub(crate) active_leases: usize,
    pub(crate) active_deletion_execution: bool,
    pub(crate) focus_count: usize,
    pub(crate) focus_epoch: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct Invalidation {
    pub(crate) pending: usize,
    pub(crate) active: usize,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct QueuedWork {
    sequence: u64,
    key: WorkKey,
}

#[derive(Clone, Debug)]
struct PendingWork {
    priority: WorkPriority,
    queued: QueuedWork,
}

#[derive(Default)]
struct ReadyQueues {
    by_priority: BTreeMap<WorkPriority, BTreeSet<QueuedWork>>,
    by_path: BTreeMap<WorkPriority, BTreeMap<RelativePath, BTreeSet<QueuedWork>>>,
}

impl ReadyQueues {
    fn insert(&mut self, priority: WorkPriority, queued: QueuedWork) {
        self.by_priority
            .entry(priority)
            .or_default()
            .insert(queued.clone());
        self.by_path
            .entry(priority)
            .or_default()
            .entry(queued.key.path.clone())
            .or_default()
            .insert(queued);
    }

    fn remove(&mut self, priority: WorkPriority, queued: &QueuedWork) {
        if let Some(queue) = self.by_priority.get_mut(&priority) {
            queue.remove(queued);
            if queue.is_empty() {
                self.by_priority.remove(&priority);
            }
        }
        let remove_path = self
            .by_path
            .get_mut(&priority)
            .and_then(|paths| paths.get_mut(&queued.key.path))
            .is_some_and(|entries| {
                entries.remove(queued);
                entries.is_empty()
            });
        if remove_path && let Some(paths) = self.by_path.get_mut(&priority) {
            paths.remove(&queued.key.path);
            if paths.is_empty() {
                self.by_path.remove(&priority);
            }
        }
    }

    fn first_eligible(&self, priority: WorkPriority, execution_active: bool) -> Option<QueuedWork> {
        self.by_priority.get(&priority)?.iter().find_map(|queued| {
            (queued.key.kind != WorkKind::ExecuteDeletion || !execution_active)
                .then(|| queued.clone())
        })
    }

    fn first_under(
        &self,
        priority: WorkPriority,
        prefix: &RelativePath,
        execution_active: bool,
    ) -> Option<QueuedWork> {
        let paths = self.by_path.get(&priority)?;
        for (path, entries) in paths.range(prefix.clone()..) {
            if !path.starts_with(prefix) {
                break;
            }
            if let Some(queued) = entries.iter().find_map(|queued| {
                (queued.key.kind != WorkKind::ExecuteDeletion || !execution_active)
                    .then(|| queued.clone())
            }) {
                return Some(queued);
            }
        }
        None
    }

    fn pending_under(
        &self,
        generation: ScanGeneration,
        prefix: &RelativePath,
    ) -> Vec<(WorkPriority, QueuedWork)> {
        let mut pending = Vec::new();
        for priority in WorkPriority::ALL {
            let Some(paths) = self.by_path.get(&priority) else {
                continue;
            };
            for (path, entries) in paths.range(prefix.clone()..) {
                if !path.starts_with(prefix) {
                    break;
                }
                for queued in entries {
                    if queued.key.generation == generation {
                        pending.push((priority, queued.clone()));
                    }
                }
            }
        }
        pending
    }

    fn counts(&self) -> WorkCounts {
        WorkCounts {
            safety: self
                .by_priority
                .get(&WorkPriority::Safety)
                .map_or(0, BTreeSet::len),
            foreground: self
                .by_priority
                .get(&WorkPriority::Foreground)
                .map_or(0, BTreeSet::len),
            reducer: self
                .by_priority
                .get(&WorkPriority::Reducer)
                .map_or(0, BTreeSet::len),
            background: self
                .by_priority
                .get(&WorkPriority::Background)
                .map_or(0, BTreeSet::len),
            prefetch: self
                .by_priority
                .get(&WorkPriority::Prefetch)
                .map_or(0, BTreeSet::len),
        }
    }
}

/// Deterministic, single-owner work state machine for one scan session.
///
/// The future runtime actor owns this value. It is intentionally free of I/O,
/// locks, threads, and channels so queue semantics remain testable in isolation.
pub(crate) struct ScanCoordinator {
    generation: ScanGeneration,
    next_sequence: u64,
    next_lease: u64,
    pending: HashMap<WorkKey, PendingWork>,
    queues: ReadyQueues,
    active: BTreeMap<LeaseId, WorkLease>,
    active_by_key: HashMap<WorkKey, LeaseId>,
    active_deletion_execution: Option<LeaseId>,
    focus_paths: VecDeque<RelativePath>,
    focus_epoch: u64,
    non_background_leases: u8,
}

impl ScanCoordinator {
    #[must_use]
    pub(crate) fn new(generation: ScanGeneration) -> Self {
        Self {
            generation,
            next_sequence: 0,
            next_lease: 0,
            pending: HashMap::new(),
            queues: ReadyQueues::default(),
            active: BTreeMap::new(),
            active_by_key: HashMap::new(),
            active_deletion_execution: None,
            focus_paths: VecDeque::with_capacity(MAX_FOCUS_PATHS),
            focus_epoch: 0,
            non_background_leases: 0,
        }
    }

    #[must_use]
    pub(crate) const fn generation(&self) -> ScanGeneration {
        self.generation
    }

    /// Advances to an isolated generation and invalidates all prior work.
    ///
    /// Returns `None` only after the generation identifier is exhausted.
    pub(crate) fn advance_generation(&mut self) -> Option<ScanGeneration> {
        let next = self.generation.0.checked_add(1)?;
        self.generation = ScanGeneration(next);
        self.pending.clear();
        self.queues = ReadyQueues::default();
        self.active.clear();
        self.active_by_key.clear();
        self.active_deletion_execution = None;
        self.focus_paths.clear();
        self.focus_epoch = self.focus_epoch.wrapping_add(1);
        self.non_background_leases = 0;
        Some(self.generation)
    }

    /// Adds a pending item, retaining only its most urgent semantic priority.
    pub(crate) fn schedule(&mut self, key: WorkKey, priority: WorkPriority) -> ScheduleOutcome {
        if key.generation != self.generation {
            return ScheduleOutcome::StaleGeneration;
        }
        if self.active_by_key.contains_key(&key) {
            return ScheduleOutcome::AlreadyLeased;
        }
        let priority = Self::normalized_priority(key.kind, priority);
        if let Some(existing) = self.pending.get(&key) {
            if priority >= existing.priority {
                return ScheduleOutcome::AlreadyPending;
            }
            let queued = existing.queued.clone();
            let prior_priority = existing.priority;
            self.queues.remove(prior_priority, &queued);
            self.queues.insert(priority, queued.clone());
            self.pending.insert(key, PendingWork { priority, queued });
            return ScheduleOutcome::PriorityRaised;
        }

        let queued = QueuedWork {
            sequence: self.next_sequence,
            key: key.clone(),
        };
        self.next_sequence = self.next_sequence.saturating_add(1);
        self.queues.insert(priority, queued.clone());
        self.pending.insert(key, PendingWork { priority, queued });
        ScheduleOutcome::Enqueued
    }

    /// Marks `path` as a current user focus without re-keying queued descendants.
    pub(crate) fn focus(&mut self, path: RelativePath) {
        if let Some(index) = self.focus_paths.iter().position(|current| current == &path) {
            self.focus_paths.remove(index);
        }
        if self.focus_paths.len() == MAX_FOCUS_PATHS {
            self.focus_paths.pop_front();
        }
        self.focus_paths.push_back(path);
        self.focus_epoch = self.focus_epoch.wrapping_add(1);
    }

    /// Grants the next deterministic eligible lease, if any work is ready.
    pub(crate) fn lease_next(&mut self) -> Option<WorkLease> {
        let (queued, focused) = self.next_queued()?;
        let pending = self.pending.remove(&queued.key)?;
        self.queues.remove(pending.priority, &queued);
        let lease = WorkLease {
            id: LeaseId(self.next_lease),
            key: queued.key,
            priority: pending.priority,
            focused,
        };
        self.next_lease = self.next_lease.saturating_add(1);
        if lease.key.kind == WorkKind::ExecuteDeletion {
            self.active_deletion_execution = Some(lease.id);
        }
        self.active_by_key.insert(lease.key.clone(), lease.id);
        self.active.insert(lease.id, lease.clone());
        if pending.priority == WorkPriority::Background && !focused {
            self.non_background_leases = 0;
        } else if pending.priority != WorkPriority::Safety {
            self.non_background_leases = self.non_background_leases.saturating_add(1);
        }
        Some(lease)
    }

    /// Accepts only the active lease with the exact key that was originally granted.
    pub(crate) fn complete(&mut self, lease: &WorkLease) -> CompletionOutcome {
        let Some(active) = self.active.get(&lease.id) else {
            return CompletionOutcome::StaleLease;
        };
        if active != lease {
            return CompletionOutcome::StaleLease;
        }
        let active = self
            .active
            .remove(&lease.id)
            .expect("active lease was checked above");
        self.active_by_key.remove(&active.key);
        if self.active_deletion_execution == Some(active.id) {
            self.active_deletion_execution = None;
        }
        CompletionOutcome::Accepted
    }

    /// Invalidates work at `prefix` and its component descendants in `generation`.
    pub(crate) fn invalidate_prefix(
        &mut self,
        generation: ScanGeneration,
        prefix: &RelativePath,
    ) -> Invalidation {
        let mut invalidation = Invalidation::default();
        for (priority, queued) in self.queues.pending_under(generation, prefix) {
            self.queues.remove(priority, &queued);
            if self.pending.remove(&queued.key).is_some() {
                invalidation.pending = invalidation.pending.saturating_add(1);
            }
        }

        let stale = self
            .active
            .iter()
            .filter_map(|(id, lease)| {
                (lease.key.generation == generation && lease.key.path.starts_with(prefix))
                    .then_some(*id)
            })
            .collect::<Vec<_>>();
        for lease_id in stale {
            if let Some(lease) = self.active.remove(&lease_id) {
                self.active_by_key.remove(&lease.key);
                if self.active_deletion_execution == Some(lease.id) {
                    self.active_deletion_execution = None;
                }
                invalidation.active = invalidation.active.saturating_add(1);
            }
        }
        invalidation
    }

    #[must_use]
    pub(crate) fn snapshot(&self) -> SchedulerSnapshot {
        SchedulerSnapshot {
            generation: self.generation,
            pending: self.queues.counts(),
            active_leases: self.active.len(),
            active_deletion_execution: self.active_deletion_execution.is_some(),
            focus_count: self.focus_paths.len(),
            focus_epoch: self.focus_epoch,
        }
    }

    fn normalized_priority(kind: WorkKind, priority: WorkPriority) -> WorkPriority {
        if kind == WorkKind::ExecuteDeletion {
            WorkPriority::Safety
        } else {
            priority
        }
    }

    fn next_queued(&self) -> Option<(QueuedWork, bool)> {
        let execution_active = self.active_deletion_execution.is_some();
        if let Some(queued) = self
            .queues
            .first_eligible(WorkPriority::Safety, execution_active)
        {
            return Some((queued, false));
        }
        if self.non_background_leases >= FOREGROUND_LEASE_BURST
            && let Some(queued) = self
                .queues
                .first_eligible(WorkPriority::Background, execution_active)
        {
            return Some((queued, false));
        }
        if let Some(queued) = self
            .queues
            .first_eligible(WorkPriority::Foreground, execution_active)
        {
            return Some((queued, false));
        }
        if let Some(queued) = self.next_focused_queued(execution_active) {
            return Some((queued, true));
        }
        for priority in [
            WorkPriority::Reducer,
            WorkPriority::Background,
            WorkPriority::Prefetch,
        ] {
            if let Some(queued) = self.queues.first_eligible(priority, execution_active) {
                return Some((queued, false));
            }
        }
        None
    }

    fn next_focused_queued(&self, execution_active: bool) -> Option<QueuedWork> {
        for path in self.focus_paths.iter().rev() {
            for priority in [WorkPriority::Background, WorkPriority::Prefetch] {
                if let Some(queued) = self.queues.first_under(priority, path, execution_active) {
                    return Some(queued);
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(path: &str) -> RelativePath {
        RelativePath::from_path(Path::new(path)).expect("fixture path should be relative")
    }

    fn key(generation: ScanGeneration, kind: WorkKind, path_text: &str) -> WorkKey {
        WorkKey::new(generation, kind, path(path_text))
    }

    #[test]
    fn rejects_nonrelative_path_components() {
        for invalid in [Path::new("../outside"), Path::new("/absolute")] {
            assert_eq!(
                RelativePath::from_path(invalid),
                Err(RelativePathError::NotRelative)
            );
        }
        assert_eq!(path("a/./b").to_path_buf(), PathBuf::from("a/b"));
    }

    #[test]
    fn duplicate_pending_work_keeps_the_urgent_priority_without_another_lease() {
        let generation = ScanGeneration::initial();
        let item = key(generation, WorkKind::EnumerateDirectory, "target");
        let mut coordinator = ScanCoordinator::new(generation);

        assert_eq!(
            coordinator.schedule(item.clone(), WorkPriority::Background),
            ScheduleOutcome::Enqueued
        );
        assert_eq!(
            coordinator.schedule(item.clone(), WorkPriority::Foreground),
            ScheduleOutcome::PriorityRaised
        );
        assert_eq!(
            coordinator.schedule(item.clone(), WorkPriority::Prefetch),
            ScheduleOutcome::AlreadyPending
        );
        let snapshot = coordinator.snapshot();
        assert_eq!(
            snapshot.pending,
            WorkCounts {
                foreground: 1,
                ..WorkCounts::default()
            }
        );

        let lease = coordinator.lease_next().expect("one lease should exist");
        assert_eq!(lease.key(), &item);
        assert_eq!(lease.priority(), WorkPriority::Foreground);
        assert_eq!(
            coordinator.schedule(item, WorkPriority::Foreground),
            ScheduleOutcome::AlreadyLeased
        );
    }

    #[test]
    fn focus_promotes_existing_descendant_without_duplicate_queue_entries() {
        let generation = ScanGeneration::initial();
        let mut coordinator = ScanCoordinator::new(generation);
        let background = key(generation, WorkKind::EnumerateDirectory, "elsewhere");
        let focused = key(generation, WorkKind::EnumerateDirectory, "target/child");
        coordinator.schedule(background.clone(), WorkPriority::Background);
        coordinator.schedule(focused.clone(), WorkPriority::Background);
        coordinator.focus(path("target"));

        let lease = coordinator.lease_next().expect("focused work should lease");
        assert_eq!(lease.key(), &focused);
        assert!(lease.is_focused());
        assert_eq!(coordinator.snapshot().pending.background, 1);
        assert_eq!(coordinator.complete(&lease), CompletionOutcome::Accepted);
        assert_eq!(
            coordinator
                .lease_next()
                .expect("background work should remain")
                .key(),
            &background
        );
    }

    #[test]
    fn stale_or_forged_completion_cannot_complete_an_active_lease() {
        let generation = ScanGeneration::initial();
        let item = key(generation, WorkKind::EnumerateDirectory, "target");
        let mut coordinator = ScanCoordinator::new(generation);
        coordinator.schedule(item, WorkPriority::Foreground);
        let lease = coordinator.lease_next().expect("lease should exist");
        let forged = WorkLease {
            id: lease.id(),
            key: key(generation, WorkKind::EnumerateDirectory, "other"),
            priority: lease.priority(),
            focused: false,
        };

        assert_eq!(coordinator.complete(&forged), CompletionOutcome::StaleLease);
        assert_eq!(coordinator.snapshot().active_leases, 1);
        assert_eq!(coordinator.complete(&lease), CompletionOutcome::Accepted);
        assert_eq!(coordinator.complete(&lease), CompletionOutcome::StaleLease);
    }

    #[test]
    fn invalidation_is_component_aware_and_stales_active_leases() {
        let generation = ScanGeneration::initial();
        let mut coordinator = ScanCoordinator::new(generation);
        let target = key(generation, WorkKind::EnumerateDirectory, "target/file");
        let sibling = key(generation, WorkKind::EnumerateDirectory, "targeted/file");
        coordinator.schedule(target.clone(), WorkPriority::Foreground);
        coordinator.schedule(sibling.clone(), WorkPriority::Background);
        let lease = coordinator.lease_next().expect("target lease should exist");

        assert_eq!(
            coordinator.invalidate_prefix(generation, &path("target")),
            Invalidation {
                pending: 0,
                active: 1
            }
        );
        assert_eq!(coordinator.complete(&lease), CompletionOutcome::StaleLease);
        assert_eq!(coordinator.snapshot().active_leases, 0);
        assert_eq!(
            coordinator
                .lease_next()
                .expect("component sibling should remain")
                .key(),
            &sibling
        );
    }

    #[test]
    fn foreground_work_cannot_starve_background_traversal() {
        let generation = ScanGeneration::initial();
        let mut coordinator = ScanCoordinator::new(generation);
        let background = key(generation, WorkKind::EnumerateDirectory, "background");
        coordinator.schedule(background.clone(), WorkPriority::Background);
        for index in 0..=FOREGROUND_LEASE_BURST {
            coordinator.schedule(
                key(
                    generation,
                    WorkKind::RefreshSubtree,
                    &format!("foreground-{index}"),
                ),
                WorkPriority::Foreground,
            );
        }

        for _ in 0..FOREGROUND_LEASE_BURST {
            let lease = coordinator
                .lease_next()
                .expect("foreground lease should exist");
            assert_eq!(lease.priority(), WorkPriority::Foreground);
            assert_eq!(coordinator.complete(&lease), CompletionOutcome::Accepted);
        }
        let background_lease = coordinator
            .lease_next()
            .expect("background lease should not starve");
        assert_eq!(background_lease.key(), &background);
    }

    #[test]
    fn deletion_execution_has_one_live_lease() {
        let generation = ScanGeneration::initial();
        let mut coordinator = ScanCoordinator::new(generation);
        coordinator.schedule(
            key(generation, WorkKind::ExecuteDeletion, "first"),
            WorkPriority::Prefetch,
        );
        coordinator.schedule(
            key(generation, WorkKind::ExecuteDeletion, "second"),
            WorkPriority::Safety,
        );
        let first = coordinator
            .lease_next()
            .expect("first execution should lease");
        assert_eq!(first.priority(), WorkPriority::Safety);
        assert!(coordinator.snapshot().active_deletion_execution);
        assert!(coordinator.lease_next().is_none());
        assert_eq!(coordinator.complete(&first), CompletionOutcome::Accepted);
        assert!(coordinator.lease_next().is_some());
    }

    #[test]
    fn advancing_generation_invalidates_all_prior_work() {
        let generation = ScanGeneration::initial();
        let mut coordinator = ScanCoordinator::new(generation);
        let item = key(generation, WorkKind::EnumerateDirectory, "target");
        coordinator.schedule(item.clone(), WorkPriority::Background);
        let lease = coordinator.lease_next().expect("lease should exist");
        let next = coordinator
            .advance_generation()
            .expect("generation should advance");

        assert_eq!(next.value(), 1);
        assert_eq!(coordinator.complete(&lease), CompletionOutcome::StaleLease);
        assert_eq!(
            coordinator.schedule(item, WorkPriority::Background),
            ScheduleOutcome::StaleGeneration
        );
    }
}
