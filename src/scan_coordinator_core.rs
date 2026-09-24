use crate::scan_session::ScanSessionId;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};

const MAX_FOCUS_PATHS: usize = 32;
const FOREGROUND_LEASE_BURST: u8 = 4;

/// Increasing identifier for one complete scan result.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ScanGeneration(u64);

impl ScanGeneration {
    #[must_use]
    pub const fn initial() -> Self {
        Self(0)
    }

    #[must_use]
    pub const fn from_value(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn value(self) -> u64 {
        self.0
    }
}

/// A normalized root-relative path used as a deterministic scheduler key.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct RelativePath(Vec<OsString>);

impl RelativePath {
    /// # Errors
    ///
    /// Returns [`RelativePathError::NotRelative`] when `path` contains a root,
    /// prefix, or parent component.
    pub fn from_path(path: &Path) -> Result<Self, RelativePathError> {
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
    pub fn from_components(components: Vec<OsString>) -> Result<Self, RelativePathError> {
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
    pub const fn root() -> Self {
        Self(Vec::new())
    }

    #[must_use]
    pub const fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    #[must_use]
    pub const fn depth(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn is_direct_child_of(&self, parent: &Self) -> bool {
        self.depth() == parent.depth().saturating_add(1) && self.starts_with(parent)
    }

    #[must_use]
    pub fn to_path_buf(&self) -> PathBuf {
        let mut path = PathBuf::new();
        for component in &self.0 {
            path.push(component);
        }
        path
    }

    #[must_use]
    pub fn components(&self) -> &[OsString] {
        &self.0
    }

    #[must_use]
    pub fn starts_with(&self, prefix: &Self) -> bool {
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
pub enum RelativePathError {
    NotRelative,
}

/// A purpose-specific work item. The scheduler never accepts untyped jobs.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum WorkKind {
    EnumerateDirectory,
    ReduceRun,
    PlanDeletion,
    ExecuteDeletion,
    RefreshSubtree,
}

impl WorkKind {
    const fn is_scan_work(self) -> bool {
        matches!(
            self,
            Self::EnumerateDirectory | Self::ReduceRun | Self::RefreshSubtree
        )
    }
}

/// Semantic scheduling class. Smaller discriminants are more urgent.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum WorkPriority {
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
pub struct WorkKey {
    session: ScanSessionId,
    generation: ScanGeneration,
    kind: WorkKind,
    path: RelativePath,
}

impl WorkKey {
    #[must_use]
    pub const fn new(
        session: ScanSessionId,
        generation: ScanGeneration,
        kind: WorkKind,
        path: RelativePath,
    ) -> Self {
        Self {
            session,
            generation,
            kind,
            path,
        }
    }

    #[must_use]
    pub const fn session(&self) -> ScanSessionId {
        self.session
    }

    #[must_use]
    pub const fn generation(&self) -> ScanGeneration {
        self.generation
    }

    #[must_use]
    pub const fn kind(&self) -> WorkKind {
        self.kind
    }

    #[must_use]
    pub const fn path(&self) -> &RelativePath {
        &self.path
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct LeaseId(u64);

impl LeaseId {
    #[must_use]
    pub const fn value(self) -> u64 {
        self.0
    }
}

/// Immutable grant for one worker. Results must echo this value verbatim.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkLease {
    id: LeaseId,
    key: WorkKey,
    priority: WorkPriority,
    focused: bool,
}

impl WorkLease {
    #[must_use]
    pub const fn id(&self) -> LeaseId {
        self.id
    }

    #[must_use]
    pub const fn key(&self) -> &WorkKey {
        &self.key
    }

    #[must_use]
    pub const fn priority(&self) -> WorkPriority {
        self.priority
    }

    #[must_use]
    pub const fn is_focused(&self) -> bool {
        self.focused
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScheduleOutcome {
    Enqueued,
    PriorityRaised,
    AlreadyPending,
    AlreadyLeased,
    StaleSession,
    StaleGeneration,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompletionOutcome {
    Accepted,
    StaleLease,
}

/// Result of returning an exact active lease to the ready queue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequeueOutcome {
    Requeued,
    StaleLease,
}

/// Current or terminal lifecycle state for coordinator-owned work.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkLifecycle {
    Pending,
    Leased,
    Succeeded,
    Failed,
    Cancelled,
    Invalidated,
}

pub enum WorkCompletion {
    Succeeded,
    Failed,
    Cancelled,
    Invalidated,
}

impl WorkCompletion {
    const fn lifecycle(self) -> WorkLifecycle {
        match self {
            Self::Succeeded => WorkLifecycle::Succeeded,
            Self::Failed => WorkLifecycle::Failed,
            Self::Cancelled => WorkLifecycle::Cancelled,
            Self::Invalidated => WorkLifecycle::Invalidated,
        }
    }
}

/// Bounded terminal work accounting for the current generation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TerminalWorkCounts {
    pub(crate) succeeded: u64,
    pub(crate) failed: u64,
    pub(crate) cancelled: u64,
    pub(crate) invalidated: u64,
}

impl TerminalWorkCounts {
    #[must_use]
    pub const fn succeeded(self) -> u64 {
        self.succeeded
    }

    #[must_use]
    pub const fn failed(self) -> u64 {
        self.failed
    }

    #[must_use]
    pub const fn cancelled(self) -> u64 {
        self.cancelled
    }

    #[must_use]
    pub const fn invalidated(self) -> u64 {
        self.invalidated
    }
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WorkCounts {
    pub(crate) safety: usize,
    pub(crate) foreground: usize,
    pub(crate) reducer: usize,
    pub(crate) background: usize,
    pub(crate) prefetch: usize,
}

impl WorkCounts {
    #[must_use]
    pub const fn safety(self) -> usize {
        self.safety
    }

    #[must_use]
    pub const fn foreground(self) -> usize {
        self.foreground
    }

    #[must_use]
    pub const fn reducer(self) -> usize {
        self.reducer
    }

    #[must_use]
    pub const fn background(self) -> usize {
        self.background
    }

    #[must_use]
    pub const fn prefetch(self) -> usize {
        self.prefetch
    }

    #[must_use]
    pub const fn total(self) -> usize {
        self.safety
            .saturating_add(self.foreground)
            .saturating_add(self.reducer)
            .saturating_add(self.background)
            .saturating_add(self.prefetch)
    }
}

/// Combined scheduler state for a fixed-size UI status summary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SchedulerSnapshot {
    pub(crate) session: ScanSessionId,
    pub(crate) generation: ScanGeneration,
    pub(crate) pending: WorkCounts,
    pub(crate) active_leases: usize,
    pub(crate) active_deletion_execution: bool,
    pub(crate) focus_count: usize,
    pub(crate) focus_epoch: u64,
    pub(crate) terminal: TerminalWorkCounts,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Invalidation {
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

impl SchedulerSnapshot {
    #[must_use]
    pub const fn session(self) -> ScanSessionId {
        self.session
    }

    #[must_use]
    pub const fn generation(self) -> ScanGeneration {
        self.generation
    }

    #[must_use]
    pub const fn pending(self) -> WorkCounts {
        self.pending
    }

    #[must_use]
    pub const fn active_leases(self) -> usize {
        self.active_leases
    }

    #[must_use]
    pub const fn active_deletion_execution(self) -> bool {
        self.active_deletion_execution
    }

    #[must_use]
    pub const fn focus_count(self) -> usize {
        self.focus_count
    }

    #[must_use]
    pub const fn focus_epoch(self) -> u64 {
        self.focus_epoch
    }

    #[must_use]
    pub const fn terminal(self) -> TerminalWorkCounts {
        self.terminal
    }
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

    fn first_scan_eligible(&self, priority: WorkPriority) -> Option<QueuedWork> {
        self.by_priority
            .get(&priority)?
            .iter()
            .find(|queued| queued.key.kind.is_scan_work())
            .cloned()
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

    fn first_scan_under(
        &self,
        priority: WorkPriority,
        prefix: &RelativePath,
    ) -> Option<QueuedWork> {
        let paths = self.by_path.get(&priority)?;
        for (path, entries) in paths.range(prefix.clone()..) {
            if !path.starts_with(prefix) {
                break;
            }
            if let Some(queued) = entries.iter().find(|queued| queued.key.kind.is_scan_work()) {
                return Some(queued.clone());
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
pub struct ScanCoordinator {
    session: ScanSessionId,
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
    terminal: TerminalWorkCounts,
}

impl ScanCoordinator {
    #[must_use]
    pub fn new(session: ScanSessionId, generation: ScanGeneration) -> Self {
        Self {
            session,
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
            terminal: TerminalWorkCounts::default(),
        }
    }

    #[must_use]
    pub const fn session(&self) -> ScanSessionId {
        self.session
    }

    #[must_use]
    pub const fn generation(&self) -> ScanGeneration {
        self.generation
    }

    /// Moves to a newer stored scan result while keeping independent deletion
    /// work until its planner or executor reports a final result.
    ///
    /// Scan-store recovery can retire incomplete results before opening a later
    /// one, so the coordinator accepts any strictly newer identifier instead of
    /// requiring consecutive numbers.
    pub fn advance_to(&mut self, generation: ScanGeneration) -> bool {
        if generation <= self.generation {
            return false;
        }
        self.terminate_matching(WorkLifecycle::Invalidated, |key| key.kind.is_scan_work());
        self.generation = generation;
        self.focus_paths.clear();
        self.focus_epoch = self.focus_epoch.wrapping_add(1);
        self.non_background_leases = 0;
        self.terminal = TerminalWorkCounts::default();
        true
    }

    /// Moves to the immediately following scan result identifier.
    ///
    /// Returns `None` only after the generation identifier is exhausted.
    pub fn advance_generation(&mut self) -> Option<ScanGeneration> {
        let next = self.generation.0.checked_add(1).map(ScanGeneration)?;
        let advanced = self.advance_to(next);
        debug_assert!(advanced);
        Some(next)
    }

    /// Adds a pending item, retaining only its most urgent semantic priority.
    pub fn schedule(&mut self, key: WorkKey, priority: WorkPriority) -> ScheduleOutcome {
        if key.session != self.session {
            return ScheduleOutcome::StaleSession;
        }
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
    pub fn focus(&mut self, path: RelativePath) {
        if let Some(index) = self.focus_paths.iter().position(|current| current == &path) {
            self.focus_paths.remove(index);
        }
        if self.focus_paths.len() == MAX_FOCUS_PATHS {
            self.focus_paths.pop_front();
        }
        self.focus_paths.push_back(path);
        self.focus_epoch = self.focus_epoch.wrapping_add(1);
    }

    /// Grants the next deterministic eligible work claim when work is ready.
    pub fn lease_next(&mut self) -> Option<WorkLease> {
        let (queued, focused) = self.next_queued()?;
        self.lease_queued(queued, focused)
    }

    /// Grants the next focused and fair scanner work claim selected by the
    /// central work records. The caller then retrieves its durable payload by path.
    pub fn lease_next_scan(&mut self) -> Option<WorkLease> {
        let (queued, focused) = self.next_scan_queued()?;
        self.lease_queued(queued, focused)
    }

    /// Grants the exact pending key after its physical work queue selected it.
    ///
    /// The scanner journal owns durable task payloads while this coordinator
    /// owns the full lifecycle. Keeping the two admissions separate avoids
    /// exposing the queue to worker threads.
    pub fn lease_exact(&mut self, key: &WorkKey) -> Option<WorkLease> {
        let pending = self.pending.get(key)?;
        if pending.queued.key.kind == WorkKind::ExecuteDeletion
            && self.active_deletion_execution.is_some()
        {
            return None;
        }
        self.lease_queued(pending.queued.clone(), false)
    }

    /// Records successful completion of an exact active lease.
    pub fn complete(&mut self, lease: &WorkLease) -> CompletionOutcome {
        self.finish(lease, WorkCompletion::Succeeded)
    }

    /// Records the explicit terminal outcome reported for an exact active lease.
    pub fn finish(&mut self, lease: &WorkLease, completion: WorkCompletion) -> CompletionOutcome {
        let Some(active) = self.active.get(&lease.id) else {
            return CompletionOutcome::StaleLease;
        };
        if active != lease {
            return CompletionOutcome::StaleLease;
        }
        let Some(active) = self.remove_active(lease.id) else {
            return CompletionOutcome::StaleLease;
        };
        debug_assert_eq!(&active, lease);
        self.record_terminal(completion.lifecycle(), 1);
        CompletionOutcome::Accepted
    }

    /// Returns an exact active lease to the ready queue when its worker did
    /// not accept the command. This is an active-to-pending transition, not a
    /// terminal outcome.
    pub fn requeue(&mut self, lease: &WorkLease) -> RequeueOutcome {
        let Some(active) = self.active.get(&lease.id) else {
            return RequeueOutcome::StaleLease;
        };
        if active != lease {
            return RequeueOutcome::StaleLease;
        }
        let Some(active) = self.remove_active(lease.id) else {
            return RequeueOutcome::StaleLease;
        };
        let queued = QueuedWork {
            sequence: self.next_sequence,
            key: active.key.clone(),
        };
        self.next_sequence = self.next_sequence.saturating_add(1);
        self.queues.insert(active.priority, queued.clone());
        self.pending.insert(
            active.key,
            PendingWork {
                priority: active.priority,
                queued,
            },
        );
        RequeueOutcome::Requeued
    }

    /// Cancels every pending or active item in the current generation.
    pub fn cancel_all(&mut self) {
        self.terminate_all(WorkLifecycle::Cancelled);
    }

    /// Fails every pending or active item in the current generation.
    pub fn fail_all(&mut self) {
        self.terminate_all(WorkLifecycle::Failed);
    }

    /// Invalidates every pending or active item in the current generation.
    pub fn invalidate_all(&mut self) {
        self.terminate_all(WorkLifecycle::Invalidated);
    }

    /// Cancels traversal-worker leases while preserving the enclosing refresh,
    /// reduction, and independent deletion authority lanes.
    pub fn cancel_scan_work(&mut self) {
        self.terminate_matching(WorkLifecycle::Cancelled, |key| {
            key.kind == WorkKind::EnumerateDirectory
        });
    }

    /// Fails traversal-worker leases while preserving the enclosing refresh,
    /// reduction, and independent deletion authority lanes.
    pub fn fail_scan_work(&mut self) {
        self.terminate_matching(WorkLifecycle::Failed, |key| {
            key.kind == WorkKind::EnumerateDirectory
        });
    }

    /// Invalidates traversal-worker leases while preserving the enclosing
    /// refresh, reduction, and independent deletion authority lanes.
    pub fn invalidate_scan_work(&mut self) {
        self.terminate_matching(WorkLifecycle::Invalidated, |key| {
            key.kind == WorkKind::EnumerateDirectory
        });
    }

    /// Invalidates work at `prefix` and its component descendants in `generation`.
    pub fn invalidate_prefix(
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
        self.record_terminal(
            WorkLifecycle::Invalidated,
            u64::try_from(invalidation.pending).unwrap_or(u64::MAX),
        );

        let stale = self
            .active
            .iter()
            .filter_map(|(id, lease)| {
                (lease.key.generation == generation && lease.key.path.starts_with(prefix))
                    .then_some(*id)
            })
            .collect::<Vec<_>>();
        for lease_id in stale {
            if self.remove_active(lease_id).is_some() {
                invalidation.active = invalidation.active.saturating_add(1);
            }
        }
        self.record_terminal(
            WorkLifecycle::Invalidated,
            u64::try_from(invalidation.active).unwrap_or(u64::MAX),
        );
        invalidation
    }

    #[must_use]
    pub fn snapshot(&self) -> SchedulerSnapshot {
        SchedulerSnapshot {
            session: self.session,
            generation: self.generation,
            pending: self.queues.counts(),
            active_leases: self.active.len(),
            active_deletion_execution: self.active_deletion_execution.is_some(),
            focus_count: self.focus_paths.len(),
            focus_epoch: self.focus_epoch,
            terminal: self.terminal,
        }
    }

    fn remove_active(&mut self, lease_id: LeaseId) -> Option<WorkLease> {
        let active = self.active.remove(&lease_id)?;
        self.active_by_key.remove(&active.key);
        if self.active_deletion_execution == Some(active.id) {
            self.active_deletion_execution = None;
        }
        Some(active)
    }

    fn terminate_all(&mut self, terminal: WorkLifecycle) {
        let pending = u64::try_from(self.pending.len()).unwrap_or(u64::MAX);
        let active = u64::try_from(self.active.len()).unwrap_or(u64::MAX);
        self.pending.clear();
        self.queues = ReadyQueues::default();
        self.active.clear();
        self.active_by_key.clear();
        self.active_deletion_execution = None;
        self.record_terminal(terminal, pending.saturating_add(active));
    }

    fn terminate_matching(
        &mut self,
        terminal: WorkLifecycle,
        predicate: impl Fn(&WorkKey) -> bool,
    ) {
        let pending = self
            .pending
            .iter()
            .filter_map(|(key, pending)| predicate(key).then_some((key.clone(), pending.clone())))
            .collect::<Vec<_>>();
        for (key, pending) in &pending {
            self.queues.remove(pending.priority, &pending.queued);
            self.pending.remove(key);
        }
        let active = self
            .active
            .iter()
            .filter_map(|(lease_id, lease)| predicate(lease.key()).then_some(*lease_id))
            .collect::<Vec<_>>();
        for lease_id in &active {
            self.remove_active(*lease_id);
        }
        let count = u64::try_from(pending.len().saturating_add(active.len())).unwrap_or(u64::MAX);
        self.record_terminal(terminal, count);
    }

    fn lease_queued(&mut self, queued: QueuedWork, focused: bool) -> Option<WorkLease> {
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

    fn record_terminal(&mut self, lifecycle: WorkLifecycle, count: u64) {
        match lifecycle {
            WorkLifecycle::Succeeded => {
                self.terminal.succeeded = self.terminal.succeeded.saturating_add(count);
            }
            WorkLifecycle::Failed => {
                self.terminal.failed = self.terminal.failed.saturating_add(count);
            }
            WorkLifecycle::Cancelled => {
                self.terminal.cancelled = self.terminal.cancelled.saturating_add(count);
            }
            WorkLifecycle::Invalidated => {
                self.terminal.invalidated = self.terminal.invalidated.saturating_add(count);
            }
            WorkLifecycle::Pending | WorkLifecycle::Leased => {
                debug_assert!(count == 0, "only terminal states may be recorded");
            }
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

    fn next_scan_queued(&self) -> Option<(QueuedWork, bool)> {
        if self.non_background_leases >= FOREGROUND_LEASE_BURST
            && let Some(queued) = self.queues.first_scan_eligible(WorkPriority::Background)
        {
            return Some((queued, false));
        }
        if let Some(queued) = self.queues.first_scan_eligible(WorkPriority::Foreground) {
            return Some((queued, false));
        }
        for path in self.focus_paths.iter().rev() {
            for priority in [WorkPriority::Background, WorkPriority::Prefetch] {
                if let Some(queued) = self.queues.first_scan_under(priority, path) {
                    return Some((queued, true));
                }
            }
        }
        for priority in [
            WorkPriority::Reducer,
            WorkPriority::Background,
            WorkPriority::Prefetch,
        ] {
            if let Some(queued) = self.queues.first_scan_eligible(priority) {
                return Some((queued, false));
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

    fn session() -> ScanSessionId {
        ScanSessionId::from_bytes([7; 16])
    }

    fn coordinator(generation: ScanGeneration) -> ScanCoordinator {
        ScanCoordinator::new(session(), generation)
    }

    fn key(generation: ScanGeneration, kind: WorkKind, path_text: &str) -> WorkKey {
        WorkKey::new(session(), generation, kind, path(path_text))
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
    fn rejects_work_from_another_scan_session() {
        let generation = ScanGeneration::initial();
        let mut coordinator = coordinator(generation);
        let foreign = WorkKey::new(
            ScanSessionId::from_bytes([8; 16]),
            generation,
            WorkKind::EnumerateDirectory,
            path("target"),
        );

        assert_eq!(
            coordinator.schedule(foreign, WorkPriority::Background),
            ScheduleOutcome::StaleSession
        );
    }

    #[test]
    fn duplicate_pending_work_keeps_the_urgent_priority_without_another_lease() {
        let generation = ScanGeneration::initial();
        let item = key(generation, WorkKind::EnumerateDirectory, "target");
        let mut coordinator = coordinator(generation);

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
        let mut coordinator = coordinator(generation);
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
    fn scan_selection_uses_focus_without_leasing_deletion_work() {
        let generation = ScanGeneration::initial();
        let mut coordinator = coordinator(generation);
        let background = key(generation, WorkKind::EnumerateDirectory, "elsewhere");
        let focused = key(generation, WorkKind::EnumerateDirectory, "target/child");
        let deletion = key(generation, WorkKind::PlanDeletion, "delete");
        coordinator.schedule(background, WorkPriority::Background);
        coordinator.schedule(focused.clone(), WorkPriority::Background);
        coordinator.schedule(deletion.clone(), WorkPriority::Foreground);
        coordinator.focus(path("target"));

        let scan = coordinator
            .lease_next_scan()
            .expect("focused scanner work should lease");
        assert_eq!(scan.key(), &focused);
        assert!(scan.is_focused());
        assert_eq!(coordinator.complete(&scan), CompletionOutcome::Accepted);
        let pending_deletion = coordinator
            .lease_next()
            .expect("deletion work should remain in the shared ledger");
        assert_eq!(pending_deletion.key(), &deletion);
    }

    #[test]
    fn stale_or_forged_completion_cannot_complete_an_active_lease() {
        let generation = ScanGeneration::initial();
        let item = key(generation, WorkKind::EnumerateDirectory, "target");
        let mut coordinator = coordinator(generation);
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
    fn exact_lease_requeues_without_becoming_terminal_work() {
        let generation = ScanGeneration::initial();
        let item = key(generation, WorkKind::PlanDeletion, "target");
        let mut coordinator = coordinator(generation);
        assert_eq!(
            coordinator.schedule(item.clone(), WorkPriority::Foreground),
            ScheduleOutcome::Enqueued
        );
        let lease = coordinator.lease_next().expect("lease should exist");

        assert_eq!(coordinator.requeue(&lease), RequeueOutcome::Requeued);
        let snapshot = coordinator.snapshot();
        assert_eq!(snapshot.active_leases, 0);
        assert_eq!(snapshot.pending.foreground, 1);
        assert_eq!(snapshot.terminal, TerminalWorkCounts::default());
        let retry = coordinator
            .lease_next()
            .expect("requeued lease should exist");
        assert_eq!(retry.key(), &item);
        assert_ne!(retry.id(), lease.id(), "retry must receive a fresh lease");
    }

    #[test]
    fn exact_leases_record_success_failure_and_cancellation_separately() {
        let generation = ScanGeneration::initial();
        let mut coordinator = coordinator(generation);
        for path_text in ["succeeded", "failed", "cancelled"] {
            assert_eq!(
                coordinator.schedule(
                    key(generation, WorkKind::EnumerateDirectory, path_text),
                    WorkPriority::Background,
                ),
                ScheduleOutcome::Enqueued
            );
        }

        let succeeded = coordinator.lease_next().expect("first lease should exist");
        assert_eq!(
            coordinator.finish(&succeeded, WorkCompletion::Succeeded),
            CompletionOutcome::Accepted
        );
        let failed = coordinator.lease_next().expect("second lease should exist");
        assert_eq!(
            coordinator.finish(&failed, WorkCompletion::Failed),
            CompletionOutcome::Accepted
        );
        let cancelled = coordinator.lease_next().expect("third lease should exist");
        assert_eq!(
            coordinator.finish(&cancelled, WorkCompletion::Cancelled),
            CompletionOutcome::Accepted
        );

        assert_eq!(
            coordinator.snapshot().terminal,
            TerminalWorkCounts {
                succeeded: 1,
                failed: 1,
                cancelled: 1,
                invalidated: 0,
            }
        );
    }

    #[test]
    fn cancelling_all_work_marks_pending_and_active_leases_cancelled() {
        let generation = ScanGeneration::initial();
        let mut coordinator = coordinator(generation);
        coordinator.schedule(
            key(generation, WorkKind::EnumerateDirectory, "active"),
            WorkPriority::Background,
        );
        coordinator.schedule(
            key(generation, WorkKind::EnumerateDirectory, "pending"),
            WorkPriority::Background,
        );
        let active = coordinator.lease_next().expect("active lease should exist");

        coordinator.cancel_all();

        assert_eq!(coordinator.snapshot().active_leases, 0);
        assert_eq!(coordinator.snapshot().pending, WorkCounts::default());
        assert_eq!(coordinator.snapshot().terminal.cancelled, 2);
        assert_eq!(
            coordinator.complete(&active),
            CompletionOutcome::StaleLease,
            "a cancelled lease must never be accepted later"
        );
    }

    #[test]
    fn invalidation_is_component_aware_and_stales_active_leases() {
        let generation = ScanGeneration::initial();
        let mut coordinator = coordinator(generation);
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
        assert_eq!(coordinator.snapshot().terminal.invalidated, 1);
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
        let mut coordinator = coordinator(generation);
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
        let mut coordinator = coordinator(generation);
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
    fn advancing_generation_invalidates_scan_work_but_retains_deletion_work() {
        let generation = ScanGeneration::initial();
        let mut coordinator = coordinator(generation);
        let scan = key(generation, WorkKind::EnumerateDirectory, "target");
        let deletion = key(generation, WorkKind::PlanDeletion, "target");
        coordinator.schedule(scan.clone(), WorkPriority::Background);
        coordinator.schedule(deletion.clone(), WorkPriority::Foreground);
        let scan_lease = coordinator
            .lease_exact(&scan)
            .expect("exact scan lease should exist");
        let next = coordinator
            .advance_generation()
            .expect("generation should advance");

        assert_eq!(next.value(), 1);
        assert_eq!(
            coordinator.complete(&scan_lease),
            CompletionOutcome::StaleLease
        );
        assert_eq!(
            coordinator.schedule(scan, WorkPriority::Background),
            ScheduleOutcome::StaleGeneration
        );
        let deletion_lease = coordinator
            .lease_next()
            .expect("independent deletion should survive the scan cutover");
        assert_eq!(deletion_lease.key(), &deletion);
        assert_eq!(
            coordinator.complete(&deletion_lease),
            CompletionOutcome::Accepted
        );
    }

    #[test]
    fn multi_worker_focus_deletion_and_invalidation_trace_is_deterministic() {
        let generation = ScanGeneration::initial();
        let expected = (0..64)
            .map(|_| {
                let mut coordinator = coordinator(generation);
                let background = key(generation, WorkKind::EnumerateDirectory, "elsewhere");
                let focused_first = key(generation, WorkKind::EnumerateDirectory, "target/first");
                let focused_second = key(generation, WorkKind::EnumerateDirectory, "target/second");
                let deletion = key(generation, WorkKind::ExecuteDeletion, "delete-target");
                assert_eq!(
                    coordinator.schedule(background.clone(), WorkPriority::Background),
                    ScheduleOutcome::Enqueued
                );
                assert_eq!(
                    coordinator.schedule(focused_first.clone(), WorkPriority::Background),
                    ScheduleOutcome::Enqueued
                );
                assert_eq!(
                    coordinator.schedule(focused_second, WorkPriority::Background),
                    ScheduleOutcome::Enqueued
                );
                assert_eq!(
                    coordinator.schedule(deletion.clone(), WorkPriority::Prefetch),
                    ScheduleOutcome::Enqueued
                );
                coordinator.focus(path("target"));

                // Two worker slots acquire work before either reports back.
                let deletion_lease = coordinator
                    .lease_next()
                    .expect("safety deletion should lease first");
                let focused_lease = coordinator
                    .lease_next()
                    .expect("focused traversal should lease second");
                let trace = vec![
                    deletion_lease.key().path().to_path_buf(),
                    focused_lease.key().path().to_path_buf(),
                ];
                assert_eq!(deletion_lease.key(), &deletion);
                assert_eq!(focused_lease.key(), &focused_first);
                assert!(focused_lease.is_focused());

                assert_eq!(
                    coordinator.invalidate_prefix(generation, &path("target")),
                    Invalidation {
                        pending: 1,
                        active: 1,
                    }
                );
                assert_eq!(
                    coordinator.complete(&focused_lease),
                    CompletionOutcome::StaleLease
                );
                assert_eq!(
                    coordinator.complete(&deletion_lease),
                    CompletionOutcome::Accepted
                );
                let background_lease = coordinator
                    .lease_next()
                    .expect("unrelated traversal should remain leaseable");
                assert_eq!(background_lease.key(), &background);
                assert_eq!(
                    coordinator.complete(&background_lease),
                    CompletionOutcome::Accepted
                );
                assert!(coordinator.lease_next().is_none());
                trace
            })
            .collect::<Vec<_>>();
        assert!(expected.windows(2).all(|pair| pair[0] == pair[1]));
    }
}
