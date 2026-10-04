use std::collections::VecDeque;
#[cfg(test)]
use std::fs::Metadata;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(test)]
use std::time::UNIX_EPOCH;
use std::time::{Duration, Instant};

use ratatui::backend::Backend;

use crate::animation::AnimationScheduler;
use crate::config::{CustomKeyBindings, KeyPreset};
use crate::deletion::{
    ConfirmationChallenge, DeletionPlan, DeletionReport, confirmation_challenge_for_target,
    current_scan_root_identity, deletion_supported, validate_scan_root_identity,
};
use crate::error::AppError;
use crate::filter::FilterPattern;
#[cfg(test)]
use crate::model::{ByteBounds, EntrySnapshot, NodeKind};
use crate::model::{MemoryBudget, ModelError, NodeId, SyntheticKind};
use crate::native_path::NativeIdentity;
#[cfg(test)]
use crate::os::physical_size;
use crate::outcome::RunSummary;
use crate::report::{ReportError, canonical_scan_report_state, write_canonical_scan_report_json};
use crate::scan_coordinator::{
    RelativePath, ScanGeneration, SchedulerSnapshot, SessionCoordinator, WorkCompletion, WorkLease,
};
#[cfg(test)]
use crate::scan_store::identity_observation::IdentityObservation;
use crate::scan_store::page::{PageCursor, PageRequest, ScanPage};
#[cfg(test)]
use crate::scan_store::path_reducer::{Coverage, PathEntryKind, PathObservation, SummaryMetrics};
use crate::scan_store::session::{
    Publication, ScanInputRunFactory, ScanStore, ScanStoreError, SealedBatch,
};
use crate::scan_store::storage::ScanStoreStorage;
use crate::scan_store::store_thread::{
    STORE_RESULT_CAPACITY, ScanRoot, StoreEvent, StoreHandle, StoreUnavailable,
};
use crate::signals::ShutdownWait;
use crate::state::deletion_work::{
    DeletionExecutionProgress, DeletionWork, DeletionWorkCommand, DeletionWorkId,
};
use crate::state::files::snapshot_page_cache::SnapshotPageCache;
use crate::state::files::snapshot_tree::SnapshotTree;
use crate::state::files::tree_view::TreeView;
use crate::state::tiles::{Board, HALF_ROWS_PER_CELL, Pivot, TileGeometry};
use crate::state::{FileToDelete, PublicationProgress, UiEffects};
use crate::temporary_storage::TemporaryStorage;
use crate::theme::{Theme, ThemeId};
use crate::ui::Display;
use crate::ui::palette::ColorCycle;

const MIB: usize = 1024 * 1024;
const MINIMUM_PLAN_BYTES: usize = 4 * 1024;
const MAX_RETAINED_DELETION_REPORTS: usize = 32;
const SNAPSHOT_PAGE_ENTRIES: usize = 512;
const MAX_SNAPSHOT_PAGE_HISTORY: usize = 32;

/// How long a finished scan's map may take to publish before the header shows how far it has got.
/// Most publications end well within it, and for those a progress line would only flash by.
const PUBLICATION_PROGRESS_DELAY: Duration = Duration::from_millis(100);

#[cfg(test)]
thread_local! {
    /// Set on a test's thread while it compares the frames a scan draws: the scan store then stays
    /// on the owner loop's thread, so each publication ends before the loop's next pass and no
    /// frame depends on how long a store thread took.
    pub(crate) static SCAN_STORE_ON_OWNER_THREAD: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

/// What a publication the store thread owes is for, which decides what the owner does with the
/// generation when it arrives. The store thread answers in the order it was asked, so each result
/// belongs to the oldest one outstanding.
#[derive(Debug)]
enum PendingPublication {
    /// The primary scan's map.
    Primary,
    /// A rebuild's replacement map.
    Rebuild,
    /// A deletion's overlay: the map without `prefix`.
    Overlay { prefix: RelativePath },
}

/// A publication whose generation the owner has swapped in, or given up on, for the owner loop to
/// finish its own part of: the coordinator's reduction lease, the completion animation, the next
/// rebuild. Every publication the app starts ends in exactly one of these.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum FinishedPublication {
    Primary,
    Rebuild {
        /// Why the replacement map could not be published, if it could not.
        failure: Option<String>,
    },
    /// The reader cancelled the rebuild before its replacement map was swapped in: a map the
    /// store thread built meanwhile was dropped, and the stale map stays.
    RebuildCancelled,
}

/// A drill the reader asked for while the page it opens could only come from the store thread: the
/// live view of a folder belongs to the scan the store thread is writing, and a finished scan's
/// map is not published until the store thread says so. The map moves when that page arrives.
#[derive(Debug)]
struct PendingNavigation {
    folder: RelativePath,
    kind: NavigationKind,
}

#[derive(Debug)]
enum NavigationKind {
    /// Opening the selected folder.
    Enter { pivot: Option<TileGeometry> },
    /// Leaving a folder for its parent; the map selects the folder it left.
    Up { leaving_relative: RelativePath },
}

/// What a background job reports to the reader when it ends. The reader did not ask at this
/// moment, so [`App::announce`] shows it only once they are not in the middle of a decision.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum Announcement {
    Notice(String),
    Error(String),
}

pub enum UiMode {
    Loading,
    Normal,
    Rebuilding {
        target: PathBuf,
    },
    /// Last verified map retained after an explicit refresh cancellation.
    StaleSnapshot,
    FilterInput {
        input: String,
        error: Option<String>,
    },
    Help,
    ScreenTooSmall,
    ThemePicker {
        original: ThemeId,
        selected: ThemeId,
        return_to: ThemePickerReturn,
    },
    DeleteConfirm {
        work_id: DeletionWorkId,
        target: Box<FileToDelete>,
        challenge: ConfirmationChallenge,
        input: String,
        return_to: ThemePickerReturn,
    },
    ErrorMessage {
        message: String,
        return_to: ThemePickerReturn,
    },
    ScanResultsUnavailable(String),
    Notice {
        message: String,
        return_to: ThemePickerReturn,
    },
    Exiting {
        work: ExitWork,
        return_to: ThemePickerReturn,
    },
    WarningMessage,
}

/// The input loop needs to distinguish a real drill from a deliberately ignored Enter.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum EnterAction {
    None,
    Drill,
}

/// Applies page-history changes only after the indexed replacement succeeds.
enum SnapshotPageHistoryChange {
    Reset,
    Push((RelativePath, Option<PageCursor>)),
    Pop((RelativePath, Option<PageCursor>)),
}

/// Exit choices deliberately distinguish work that can be discarded from the
/// single filesystem mutation that must either stop at a boundary or be awaited.
pub enum ExitWork {
    None,
    Pending {
        count: usize,
    },
    Cancelling {
        count: usize,
    },
    Active {
        planned_entries: u64,
        progress: Arc<DeletionExecutionProgress>,
        pending: usize,
    },
    Stopping {
        planned_entries: u64,
        progress: Arc<DeletionExecutionProgress>,
    },
}

#[derive(Clone)]
/// The picker can overlay a live scan without discarding its underlying mode.
pub enum ThemePickerReturn {
    Loading,
    Normal,
    Rebuilding { target: PathBuf },
    StaleSnapshot,
    ScanResultsUnavailable(String),
}

impl ThemePickerReturn {
    fn into_mode(self) -> UiMode {
        match self {
            Self::Loading => UiMode::Loading,
            Self::Normal => UiMode::Normal,
            Self::Rebuilding { target } => UiMode::Rebuilding { target },
            Self::StaleSnapshot => UiMode::StaleSnapshot,
            Self::ScanResultsUnavailable(message) => UiMode::ScanResultsUnavailable(message),
        }
    }
}

impl UiMode {
    #[must_use]
    pub const fn allows_motion(&self) -> bool {
        matches!(
            self,
            Self::Loading | Self::Normal | Self::Rebuilding { .. } | Self::StaleSnapshot
        )
    }

    #[must_use]
    pub const fn has_modal_attention(&self) -> bool {
        matches!(
            self,
            Self::ThemePicker { .. }
                | Self::DeleteConfirm { .. }
                | Self::ErrorMessage { .. }
                | Self::ScanResultsUnavailable(_)
                | Self::Notice { .. }
                | Self::Exiting { .. }
                | Self::WarningMessage
                | Self::Help
        )
    }

    #[must_use]
    pub const fn can_present_deletion_confirmation(&self) -> bool {
        matches!(self, Self::Loading | Self::Normal)
    }
}

#[allow(clippy::struct_excessive_bools)]
pub struct App<B>
where
    B: Backend,
{
    pub is_running: bool,
    pub loaded: bool,
    pub ui_mode: UiMode,
    suspended_ui_mode: Option<UiMode>,
    board: Board,
    scan_root: PathBuf,
    show_apparent_size: bool,
    page_memory_limit: usize,
    root_identity: NativeIdentity,
    scan_store: StoreHandle,
    session_coordinator: SessionCoordinator,
    snapshot_page_cache: Option<SnapshotPageCache>,
    snapshot_page_is_provisional: bool,
    snapshot_filter: Option<(FilterPattern, RelativePath)>,
    scan_store_available: bool,
    generation_rebuild_required: bool,
    scan_store_failure: Option<String>,
    generation_rebuild_active: bool,
    /// Explicit cancellation preserves the stale map without retrying it automatically.
    generation_rebuild_restart_suppressed: bool,
    generation_rebuild_invalidated: bool,
    generation_rebuild_target: Option<RelativePath>,
    /// Publications the store thread owes, oldest first: it answers in the order it was asked.
    publications: VecDeque<PendingPublication>,
    /// Publications that ended, for the owner loop to finish its own part of: the loop drains them
    /// with [`App::take_finished_publication`].
    finished_publications: VecDeque<FinishedPublication>,
    /// When the primary scan's publication has run long enough to show its progress.
    publication_progress_from: Option<Instant>,
    /// An announcement that waits for the reader to finish what they are deciding. A later one
    /// replaces it: the reader sees the latest result.
    waiting_announcement: Option<Announcement>,
    /// The live view of a folder the store thread reported while the map was moving: applied once
    /// it settles.
    provisional_page_waiting: Option<(RelativePath, ScanPage)>,
    /// A drill that waits for the store thread's page, at most one: further drills are ignored
    /// until it lands.
    pending_navigation: Option<PendingNavigation>,
    snapshot_page_history: Vec<(RelativePath, Option<PageCursor>)>,
    scheduler_snapshot: Option<SchedulerSnapshot>,

    display: Display<B>,
    ui_effects: UiEffects,
    pub(crate) deletion_work: DeletionWork,
    deletion_modal_work_id: Option<DeletionWorkId>,
    deletion_plan_cancellation_requested: bool,
    delete_confirmation_disabled: bool,
    deletion_history: Vec<Arc<DeletionReport>>,
    deletion_history_bytes: usize,
    deletion_history_limit: usize,
    keymap: KeyPreset,
    custom_keys: Option<CustomKeyBindings>,
    mouse_enabled: bool,
    dirty: bool,
    /// The runtime decides whether to animate loading. Direct app fixtures stay static.
    loading_animation_enabled: bool,
    /// Last input or state change (scan progress, completion, deletion
    /// progress) that should keep the selected tile's travelling sheen awake.
    /// The sheen plays one more full cycle after this instant and then
    /// settles instead of animating indefinitely (F3): see `rearm_sheen`.
    sheen_armed_at: Duration,
}

impl<B> App<B>
where
    B: Backend,
{
    #[allow(dead_code, clippy::too_many_arguments)]
    pub fn new(
        terminal_backend: B,
        path_in_filesystem: PathBuf,
        show_apparent_size: bool,
        disable_delete_confirmation: bool,
        process_memory_mib: usize,
        keymap: KeyPreset,
        custom_keys: Option<CustomKeyBindings>,
        mouse_enabled: bool,
    ) -> Result<Self, AppError> {
        let root_identity = current_scan_root_identity(&path_in_filesystem)
            .map_err(|error| AppError::Model(error.to_string()))?;
        Self::new_with_root_identity(
            terminal_backend,
            path_in_filesystem,
            root_identity,
            show_apparent_size,
            disable_delete_confirmation,
            process_memory_mib,
            keymap,
            custom_keys,
            mouse_enabled,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_with_root_identity(
        terminal_backend: B,
        path_in_filesystem: PathBuf,
        root_identity: NativeIdentity,
        show_apparent_size: bool,
        disable_delete_confirmation: bool,
        process_memory_mib: usize,
        keymap: KeyPreset,
        custom_keys: Option<CustomKeyBindings>,
        mouse_enabled: bool,
    ) -> Result<Self, AppError> {
        let scan_store_storage = ScanStoreStorage::new(TemporaryStorage::default(), None)
            .map_err(|error| AppError::io("could not create private scan-store session", error))?;
        Self::new_with_root_identity_and_scan_store(
            terminal_backend,
            path_in_filesystem,
            root_identity,
            show_apparent_size,
            disable_delete_confirmation,
            process_memory_mib,
            keymap,
            custom_keys,
            mouse_enabled,
            scan_store_storage,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_with_root_identity_and_scan_store(
        terminal_backend: B,
        path_in_filesystem: PathBuf,
        root_identity: NativeIdentity,
        show_apparent_size: bool,
        disable_delete_confirmation: bool,
        process_memory_mib: usize,
        keymap: KeyPreset,
        custom_keys: Option<CustomKeyBindings>,
        mouse_enabled: bool,
        scan_store_storage: ScanStoreStorage,
    ) -> Result<Self, AppError> {
        validate_scan_root_identity(&path_in_filesystem, &root_identity)
            .map_err(|error| AppError::Model(error.to_string()))?;
        let display = Display::new(terminal_backend)?;
        let board = Board::new();
        let scan_store = StoreHandle::new(
            ScanStore::new_with_storage(ScanGeneration::initial(), scan_store_storage)
                .map_err(scan_store_error)?,
        )
        .map_err(scan_store_error)?;
        let page_memory_limit = MemoryBudget::from_mib(process_memory_mib)
            .map_err(model_error)?
            .model_limit();
        Self::from_parts(
            display,
            board,
            path_in_filesystem,
            show_apparent_size,
            page_memory_limit,
            root_identity,
            scan_store,
            disable_delete_confirmation,
            keymap,
            custom_keys,
            mouse_enabled,
            process_memory_mib,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn from_parts(
        display: Display<B>,
        mut board: Board,
        scan_root: PathBuf,
        show_apparent_size: bool,
        page_memory_limit: usize,
        root_identity: NativeIdentity,
        scan_store: StoreHandle,
        disable_delete_confirmation: bool,
        keymap: KeyPreset,
        custom_keys: Option<CustomKeyBindings>,
        mouse_enabled: bool,
        process_memory_mib: usize,
    ) -> Result<Self, AppError> {
        let deletion_session = scan_store.session();
        let deletion_generation = scan_store
            .active_generation()
            .unwrap_or_else(ScanGeneration::initial);
        let session_coordinator = SessionCoordinator::start(deletion_session, deletion_generation)
            .map_err(|error| AppError::io("could not start scan coordinator", error))?;
        let loading_snapshot = SnapshotTree::loading(
            scan_root.clone(),
            deletion_generation,
            page_memory_limit,
            scan_store.storage_stats(),
        )
        .map_err(model_error)?;
        board.arm_scan_reveal();
        Ok(Self {
            is_running: true,
            loaded: false,
            board,
            scan_root,
            show_apparent_size,
            page_memory_limit,
            root_identity,
            scan_store,
            session_coordinator: session_coordinator.clone(),
            snapshot_page_cache: Some(SnapshotPageCache::new(loading_snapshot, None)),
            snapshot_page_is_provisional: true,
            snapshot_filter: None,
            scan_store_available: true,
            generation_rebuild_required: false,
            scan_store_failure: None,
            generation_rebuild_active: false,
            generation_rebuild_restart_suppressed: false,
            generation_rebuild_invalidated: false,
            generation_rebuild_target: None,
            publications: VecDeque::new(),
            finished_publications: VecDeque::new(),
            publication_progress_from: None,
            waiting_announcement: None,
            provisional_page_waiting: None,
            pending_navigation: None,
            snapshot_page_history: Vec::with_capacity(MAX_SNAPSHOT_PAGE_HISTORY),
            scheduler_snapshot: None,
            display,
            ui_mode: UiMode::Loading,
            suspended_ui_mode: None,

            ui_effects: UiEffects::new(),
            delete_confirmation_disabled: disable_delete_confirmation,
            keymap,
            custom_keys,
            mouse_enabled,
            dirty: true,
            loading_animation_enabled: false,
            sheen_armed_at: Duration::ZERO,
            deletion_history_bytes: 0,
            deletion_history_limit: process_memory_mib.saturating_mul(MIB) / 8,
            deletion_history: Vec::with_capacity(MAX_RETAINED_DELETION_REPORTS),
            deletion_work: DeletionWork::new_for_coordinator(session_coordinator),
            deletion_modal_work_id: None,
            deletion_plan_cancellation_requested: false,
        })
    }

    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        reason = "a render frame must derive the visible tree and all animation activity atomically"
    )]
    pub fn render_if_dirty(
        &mut self,
        animation: &mut AnimationScheduler,
        now: Duration,
        theme_name: &str,
        theme: Theme,
        ascii: bool,
        monochrome: bool,
        reduced_motion: bool,
    ) -> Result<bool, AppError> {
        self.sync_deletion_work_summary();
        if !self.dirty {
            return Ok(false);
        }
        let full_screen_size = self.display.size()?;
        if full_screen_size.width < 32 || full_screen_size.height < 8 {
            self.enter_screen_too_small();
        }
        let selection_before = self.board.currently_selected().is_some();
        let animate_loading_visual = self.loading_animation_enabled
            && !ascii
            && !monochrome
            && !reduced_motion
            && ColorCycle::can_animate(theme.focus);
        // The sheen plays one more full cycle after the last input or state
        // change and then settles instead of animating indefinitely (F3).
        let sheen_settled =
            now.saturating_sub(self.sheen_armed_at) >= crate::animation::ONE_SHEEN_CYCLE;
        let scheduler_snapshot = self.scheduler_snapshot;
        let (display, board, page_cache, ui_mode, ui_effects, deletion_work) = (
            &mut self.display,
            &mut self.board,
            &self.snapshot_page_cache,
            &self.ui_mode,
            &self.ui_effects,
            &self.deletion_work,
        );
        let tree: &dyn TreeView = page_cache
            .as_ref()
            .expect("every app state retains a page")
            .current();
        display.render_with_scheduler(
            tree,
            board,
            ui_mode,
            ui_effects,
            deletion_work,
            scheduler_snapshot,
            animation,
            now,
            theme_name,
            theme,
            ascii,
            monochrome,
            self.keymap,
            self.custom_keys.as_ref(),
            self.mouse_enabled,
            self.delete_confirmation_disabled,
            reduced_motion,
            animate_loading_visual,
            sheen_settled,
        )?;
        let has_selection = self.board.currently_selected().is_some();
        // Rendering lays out the board and can establish or clear its selection.
        let selection_changed = self.ui_mode.allows_motion() && selection_before != has_selection;
        let animate_selected_map = self.ui_mode.allows_motion()
            && has_selection
            && !self.board.is_list_layout()
            && !ascii
            && !monochrome
            && !reduced_motion
            && ColorCycle::can_animate(theme.focus)
            && !sheen_settled;

        let animate_modal = self.ui_mode.has_modal_attention()
            && !ascii
            && !monochrome
            && !reduced_motion
            && ColorCycle::can_animate(theme.focus);
        let animate_deletion_checker = !ascii
            && !monochrome
            && !reduced_motion
            && ColorCycle::can_animate(theme.focus)
            && self.deletion_work.has_checker_animation(now);
        let animate_loading_surface = animate_loading_visual
            && matches!(self.ui_mode, UiMode::Loading | UiMode::Rebuilding { .. })
            && !self.board.is_list_layout()
            && self.board.rendered_tiles().is_empty();
        let animate_scan_reveal = animate_loading_visual && self.board.has_scan_reveal();
        // Modal, selected-map, and deletion feedback animate at the 30 fps
        // cadence needed for a one-cell-per-frame perimeter gradient.
        animation.set_activity_with_cadence(
            animate_selected_map
                || animate_modal
                || animate_deletion_checker
                || animate_loading_surface
                || animate_scan_reveal
                || self.ui_effects.has_deletion_departure(),
            animate_selected_map
                || animate_modal
                || animate_deletion_checker
                || animate_loading_surface
                || animate_scan_reveal
                || self.ui_effects.has_deletion_departure(),
        );
        // The map transition runs on wall-clock time, so the loop has to keep waking up
        // until it settles. Nothing else in the frame would ask for those frames.
        animation.set_geometry_active(self.board.is_transitioning());
        // The workspace pane observes selection before board layout. Queue one
        // corrective draw whenever layout changes that selection.
        self.dirty = selection_changed;
        Ok(true)
    }

    pub fn finish(&mut self) -> Result<(), AppError> {
        self.display.clear()
    }

    pub const fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Whether a render is pending: `render_if_dirty` will draw the next time it runs. Lets the
    /// owner loop avoid sleeping past a frame that is only waiting on the terminal writer to
    /// drain the previous one (see `runtime::OwnerLoop::next_timeout`).
    pub(crate) const fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Input or a state change (scan progress, completion, deletion progress)
    /// wakes the selected tile's sheen for one more full cycle before it
    /// settles (F3). Called by the owner loop, which observes those events.
    pub(crate) const fn rearm_sheen(&mut self, now: Duration) {
        self.sheen_armed_at = now;
    }

    pub(crate) const fn set_loading_animation_enabled(&mut self, enabled: bool) {
        self.loading_animation_enabled = enabled;
    }

    #[must_use]
    pub const fn keymap(&self) -> KeyPreset {
        self.keymap
    }

    #[must_use]
    pub const fn mouse_enabled(&self) -> bool {
        self.mouse_enabled
    }

    #[must_use]
    pub fn custom_keys(&self) -> Option<&CustomKeyBindings> {
        self.custom_keys.as_ref()
    }

    #[must_use]
    pub fn current_folder_path(&self) -> PathBuf {
        self.visible_tree().get_current_path()
    }

    fn visible_tree(&self) -> &SnapshotTree {
        self.snapshot_page_cache
            .as_ref()
            .expect("every navigable app state retains a snapshot page")
            .current()
    }
    #[must_use]
    pub(crate) fn map_is_transitioning(&self) -> bool {
        self.board.is_transitioning()
    }

    pub fn render_and_update_board(&mut self) {
        self.update_board();
        self.mark_dirty();
    }

    /// Applies background scan data only after a reader-visible drill settles.
    ///
    /// A scan refresh lands directly at its final geometry rather than perpetually
    /// retargeting the map tween while the scanner is active. The live view belongs to the scan
    /// the store thread is writing: this asks it for the page, and
    /// [`Self::process_scan_store_events`] applies the page when it arrives, so the owner loop
    /// never waits for it.
    pub fn refresh_board_from_scan(&mut self) -> bool {
        if self.board.is_transitioning() {
            return false;
        }
        if !self.uses_provisional_scan_page() {
            if self.update_board() {
                self.board.settle_geometry();
                self.mark_dirty();
            }
            return true;
        }
        self.request_live_page();
        true
    }

    /// The folder whose live view the reader waits for: the one a drill opens, else the one on
    /// screen.
    fn live_view_folder(&self) -> RelativePath {
        self.pending_navigation.as_ref().map_or_else(
            || self.current_relative(),
            |navigation| navigation.folder.clone(),
        )
    }

    fn current_relative(&self) -> RelativePath {
        self.snapshot_page_cache
            .as_ref()
            .map_or_else(RelativePath::root, |cache| {
                cache.current().current_relative().clone()
            })
    }

    /// The path of the folder the scan should look at first: what a drill opens, once it lands,
    /// else what is on screen.
    pub(crate) fn scan_view_folder_path(&self) -> PathBuf {
        self.pending_navigation.as_ref().map_or_else(
            || self.current_folder_path(),
            |navigation| self.scan_root.join(navigation.folder.to_path_buf()),
        )
    }

    /// Asks the store thread for the live view of the folder the reader looks at or opens.
    fn request_live_page(&mut self) {
        let folder = self.live_view_folder();
        if let Err(error) = self
            .scan_store
            .request_provisional_page(&folder, SNAPSHOT_PAGE_ENTRIES)
        {
            self.fail_scan_store(error.to_string());
        }
    }

    /// The scan store cannot go on: its map is unavailable, and the reason is kept for the
    /// message the reader sees when the scan ends.
    fn fail_scan_store(&mut self, failure: String) {
        if self.scan_store_available {
            self.abandon_scan_store_generation_with_failure(failure);
        }
        self.pending_navigation = None;
        self.provisional_page_waiting = None;
    }

    /// Applies the live view the store thread built, unless the reader has moved on since asking.
    fn apply_provisional_page(
        &mut self,
        folder: RelativePath,
        page: Result<Box<ScanPage>, ScanStoreError>,
    ) {
        if !self.uses_provisional_scan_page() || folder != self.live_view_folder() {
            return;
        }
        match page {
            Ok(page) => {
                let page = *page;
                let navigating = self.pending_navigation.is_some();
                if !navigating && self.board.is_transitioning() {
                    self.provisional_page_waiting = Some((folder, page));
                } else {
                    self.show_provisional_page(&folder, page);
                }
            }
            Err(error) => self.fail_scan_store(error.to_string()),
        }
    }

    /// Applies a live view that waited for the map to stop moving, once it has. Returns whether
    /// it applied one.
    pub(crate) fn apply_waiting_provisional_page(&mut self) -> bool {
        if self.board.is_transitioning() {
            return false;
        }
        let Some((folder, page)) = self.provisional_page_waiting.take() else {
            return false;
        };
        if !self.uses_provisional_scan_page() || folder != self.live_view_folder() {
            return false;
        }
        self.show_provisional_page(&folder, page);
        true
    }

    fn show_provisional_page(&mut self, folder: &RelativePath, page: ScanPage) {
        self.provisional_page_waiting = None;
        if let Some(navigation) = self
            .pending_navigation
            .take_if(|navigation| navigation.folder == *folder)
        {
            let leaving = self
                .snapshot_page_cache
                .as_ref()
                .map(|cache| cache.current().current_id());
            if let Err(error) = self.install_provisional_snapshot(page) {
                self.show_error(format!("Could not open this scan page: {error}"));
                return;
            }
            self.snapshot_page_history.clear();
            self.finish_navigation(navigation.kind, leaving);
            return;
        }
        let selected_relative = self.board.currently_selected().and_then(|tile| {
            self.snapshot_page_cache
                .as_ref()
                .and_then(|cache| cache.current().relative_for_id(tile.node_id))
                .cloned()
        });
        let had_selection = selected_relative.is_some();
        if let Err(error) = self.install_provisional_snapshot(page) {
            self.fail_scan_store(error.to_string());
            return;
        }
        self.board.reset_selected_index();
        let changed = self.update_board();
        if let Some(selected_relative) = selected_relative.as_ref()
            && let Some(selected_id) = self
                .snapshot_page_cache
                .as_ref()
                .and_then(|cache| cache.current().id_for_relative(selected_relative))
        {
            self.board.select_node(selected_id);
        }
        if changed || had_selection {
            self.board.settle_geometry();
            self.mark_dirty();
        }
    }

    fn update_board(&mut self) -> bool {
        let snapshot = self
            .snapshot_page_cache
            .as_ref()
            .expect("every navigable app state retains a snapshot page")
            .current();
        self.board.change_files_for_view(
            snapshot.files_in_current_folder(self.board.zoom_level, self.show_apparent_size),
            snapshot.current_id(),
            snapshot.filter_raw(),
        )
    }

    fn uses_provisional_scan_page(&self) -> bool {
        self.snapshot_page_is_provisional
            && self.scan_store_available
            && self.scan_store.active_generation().is_some()
            && (!self.loaded || self.generation_rebuild_active)
    }

    fn can_retain_published_snapshot(&self) -> bool {
        !self.snapshot_page_is_provisional
            && self.snapshot_page_cache.is_some()
            && self.scan_store.published().is_some()
    }

    /// Whether no deletion can go on from the map the reader sees: the store is lost, or a
    /// rebuild is replacing the map. Pending deletion work is cancelled, not kept.
    fn deletion_is_unavailable(&self) -> bool {
        !self.scan_store_available
            || self.generation_rebuild_required
            || self.generation_rebuild_active
    }

    /// Whether the reader may start a new deletion: not while it is unavailable, and not while
    /// the finished scan's map has not arrived to be deleted from.
    fn deletion_is_blocked(&self) -> bool {
        self.deletion_is_unavailable() || self.primary_publication_pending()
    }

    /// Whether the store thread still owes a map that deletion starts from: the finished scan's,
    /// or the one without the last deletion's target, which the next overlay derives from. Work
    /// already queued waits for it rather than being cancelled, because the wait ends by itself
    /// and the owner loop starts that work when it does.
    fn deletion_waits_for_store(&self) -> bool {
        self.primary_publication_pending() || self.overlay_publication_pending()
    }

    /// Whether the finished scan's map has not arrived from the store thread yet: until it does,
    /// no page of it can be read, and nothing can be deleted from it.
    pub(crate) fn primary_publication_pending(&self) -> bool {
        self.publications
            .iter()
            .any(|pending| matches!(pending, PendingPublication::Primary))
    }

    /// Whether a deletion's map is still being built: until it arrives, the map on screen lists
    /// what the deletion removed.
    fn overlay_publication_pending(&self) -> bool {
        self.publications
            .iter()
            .any(|pending| matches!(pending, PendingPublication::Overlay { .. }))
    }

    /// Whether the primary scan's publication ended and the owner loop has not taken it yet.
    #[cfg(feature = "internal")]
    pub(crate) fn primary_publication_finished(&self) -> bool {
        self.finished_publications
            .iter()
            .any(|finished| matches!(finished, FinishedPublication::Primary))
    }

    /// Brings the progress shown for the finishing map up to date: how many stages the store
    /// thread has completed publishing the primary scan's map, and how long the map has been
    /// finishing, once the publication has run [`PUBLICATION_PROGRESS_DELAY`] and until it ends.
    /// The stage count is the work advancing, but one stage can take longer than the screen may
    /// stand still while the loop is idle and ready for a key, so the clock moves the display
    /// every tenth of a second in between. Returns whether the display changed.
    pub(crate) fn refresh_publication_progress(&mut self) -> bool {
        let now = Instant::now();
        let progress = self
            .publication_progress_from
            .filter(|from| now >= *from && self.primary_publication_pending())
            .and_then(|from| {
                let (done, total) = self.scan_store.publication_progress()?;
                let elapsed = now.duration_since(from) + PUBLICATION_PROGRESS_DELAY;
                Some(PublicationProgress {
                    done,
                    total,
                    elapsed_tenths: u32::try_from(elapsed.as_millis() / 100).unwrap_or(u32::MAX),
                })
            });
        if progress == self.ui_effects.publication_progress {
            return false;
        }
        self.ui_effects.publication_progress = progress;
        self.mark_dirty();
        true
    }

    /// Whether the page a drill opens can only come from the store thread: the live view while a
    /// scan is open, and nothing yet while the map it produced is being published.
    fn navigation_waits_for_store(&self) -> bool {
        self.snapshot_page_is_provisional
            && self.scan_store_available
            && (self.scan_store.active_generation().is_some()
                || self.publications.iter().any(|pending| {
                    matches!(
                        pending,
                        PendingPublication::Primary | PendingPublication::Rebuild
                    )
                }))
    }

    fn files_in_current_view(&self, offset: usize) -> Vec<crate::state::tiles::FileMetadata> {
        self.snapshot_page_cache
            .as_ref()
            .expect("every navigable app state retains a snapshot page")
            .current()
            .files_in_current_folder(offset, self.show_apparent_size)
    }
    #[cfg(test)]
    pub(crate) fn append_scan_store_entry_for_test(
        &mut self,
        metadata: &Metadata,
        path: &Path,
        identity: &NativeIdentity,
    ) {
        let relative = path
            .strip_prefix(&self.scan_root)
            .expect("fixture path should be below the scan root");
        let relative = RelativePath::from_path(relative)
            .expect("fixture path should be a canonical relative path");
        let kind = if metadata.file_type().is_symlink() || identity.reparse_point {
            PathEntryKind::Link
        } else if metadata.is_dir() {
            PathEntryKind::Directory
        } else {
            PathEntryKind::File
        };
        let node_kind = match kind {
            PathEntryKind::Directory => NodeKind::Directory,
            PathEntryKind::File => NodeKind::File,
            PathEntryKind::Link => NodeKind::Link,
        };
        let allocated_bytes = (kind != PathEntryKind::Directory)
            .then(|| physical_size(path, metadata).ok().map(u128::from))
            .flatten();
        let snapshot = EntrySnapshot {
            identity: Some(identity.clone()),
            kind: node_kind,
            apparent_bytes: if kind == PathEntryKind::Directory {
                0
            } else {
                u128::from(metadata.len())
            },
            allocated_bytes,
            modified_nanos: metadata
                .modified()
                .ok()
                .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_nanos()),
        };
        let direct_allocation = kind != PathEntryKind::Directory && identity.link_count == Some(1);
        let physical_bounds = allocated_bytes.map_or_else(ByteBounds::unknown, ByteBounds::exact);
        let (allocated_bounds, reclaimable_bounds) = if direct_allocation {
            (physical_bounds, physical_bounds)
        } else {
            (ByteBounds::exact(0), ByteBounds::exact(0))
        };
        let identities = (kind != PathEntryKind::Directory && identity.link_count != Some(1))
            .then(|| IdentityObservation {
                path: relative.clone(),
                file_id: identity.file_id,
                declared_links: identity.link_count,
                allocated_bytes: allocated_bytes
                    .map_or_else(ByteBounds::unknown, ByteBounds::exact),
            })
            .into_iter()
            .collect();
        self.scan_store
            .with_inline_store(|store| {
                store.append_observation_batch(
                    vec![PathObservation::with_snapshot(
                        relative,
                        kind,
                        SummaryMetrics::leaf(
                            snapshot.apparent_bytes,
                            allocated_bounds,
                            reclaimable_bounds,
                        ),
                        Coverage::Complete,
                        Some(snapshot),
                    )],
                    identities,
                )
            })
            .expect("fixture entry should append to the canonical scan store");
    }

    /// Publishes the finished primary scan the way the owner loop does: the store thread's
    /// result is applied, then the publication that ended is taken, as the loop would to finish
    /// the coordinator's reduction.
    #[cfg(test)]
    pub(crate) fn finalize_scan(&mut self) {
        self.begin_primary_publication();
        self.process_scan_store_events();
        while self.take_finished_publication().is_some() {}
    }

    fn abandon_scan_store_generation(&mut self) {
        if let Err(error) = self.scan_store.discard_active() {
            self.scan_store_failure
                .get_or_insert_with(|| error.to_string());
        }
        self.scan_store_available = false;
    }

    fn abandon_scan_store_generation_with_failure(&mut self, failure: impl Into<String>) {
        self.scan_store_failure = Some(failure.into());
        self.abandon_scan_store_generation();
    }

    fn scan_results_unavailable_message(&self) -> String {
        if self
            .scan_store_failure
            .as_deref()
            .is_some_and(|failure| failure.contains("scan store capacity exhausted"))
        {
            return "Not enough scratch space for the full scan. The detailed map and deletion controls are unavailable. Choose a roomier --scan-store-dir, lower --scan-store-reserve-mib, or raise an explicit --scan-store-mib cap, then run again.".to_string();
        }
        "Excise could not build a complete folder map. Run it again.".to_string()
    }

    fn update_suspended_scan_results_unavailable(&mut self, message: &str) -> bool {
        let Some(suspended) = self.suspended_ui_mode.as_mut() else {
            return false;
        };
        match suspended {
            UiMode::ThemePicker { return_to, .. }
            | UiMode::Exiting { return_to, .. }
            | UiMode::ErrorMessage { return_to, .. }
            | UiMode::Notice { return_to, .. } => {
                *return_to = ThemePickerReturn::ScanResultsUnavailable(message.to_string());
            }
            UiMode::ScreenTooSmall => {}
            _ => *suspended = UiMode::ScanResultsUnavailable(message.to_string()),
        }
        true
    }

    fn show_scan_results_unavailable(&mut self, message: impl Into<String>) {
        self.snapshot_filter = None;
        let message = message.into();
        let suspended_updated = self.update_suspended_scan_results_unavailable(&message);
        match &mut self.ui_mode {
            UiMode::ThemePicker { return_to, .. }
            | UiMode::Exiting { return_to, .. }
            | UiMode::ErrorMessage { return_to, .. }
            | UiMode::Notice { return_to, .. } => {
                *return_to = ThemePickerReturn::ScanResultsUnavailable(message);
                self.mark_dirty();
                return;
            }
            UiMode::ScreenTooSmall if suspended_updated => {
                self.mark_dirty();
                return;
            }
            _ => {}
        }
        self.replace_ui_mode(UiMode::ScanResultsUnavailable(message));
        self.mark_dirty();
    }

    /// Moves the scan store onto its own thread, from where it admits what the scanner seals and
    /// publishes what it finishes. Called once, before the scan begins.
    pub(crate) fn start_scan_store_thread(&mut self) -> Result<(), AppError> {
        #[cfg(test)]
        if SCAN_STORE_ON_OWNER_THREAD.get() {
            return Ok(());
        }
        self.scan_store
            .start_thread()
            .map_err(|error| AppError::io("could not start the scan store thread", error))
    }

    /// Stops the store thread and waits for it, for as long as `wait` allows: nothing writes the
    /// session's scratch storage afterwards, unless a forced stop left the thread behind.
    pub(crate) fn shutdown_scan_store(&mut self, wait: &mut ShutdownWait) -> Result<(), AppError> {
        self.scan_store
            .shutdown_with(wait)
            .map_err(AppError::Worker)
    }

    /// Holds the store thread inside a call, as a hung file system would; it goes on when the
    /// guard is dropped.
    #[cfg(test)]
    pub(crate) fn hold_scan_store_thread_for_test(
        &mut self,
    ) -> crate::scan_store::store_thread::HeldStoreThread {
        self.scan_store.hold_for_test()
    }

    /// Whether the store thread can take another scanner batch: the owner loop takes the next
    /// scanner event only when it can, so a store thread that falls behind backs the scanner up.
    #[must_use]
    pub(crate) fn scan_store_has_room_for_batch(&self) -> bool {
        self.scan_store.has_room_for_batch()
    }

    /// Whether the store thread still owes a publication, or one ended and the loop has not taken
    /// it yet: the loop keeps polling while this holds.
    #[must_use]
    pub(crate) fn scan_store_busy(&self) -> bool {
        !self.publications.is_empty() || !self.finished_publications.is_empty()
    }

    /// The next publication that ended, for the owner loop to finish its own part of.
    pub(crate) fn take_finished_publication(&mut self) -> Option<FinishedPublication> {
        self.finished_publications.pop_front()
    }

    /// Applies what the store thread has reported since the last call. Returns whether it
    /// reported anything.
    pub(crate) fn process_scan_store_events(&mut self) -> bool {
        let mut handled = false;
        for _ in 0..STORE_RESULT_CAPACITY {
            let Some(event) = self.scan_store.poll() else {
                break;
            };
            handled = true;
            match event {
                StoreEvent::AdmissionFailed(message) => self.fail_scan_store(message),
                StoreEvent::ProvisionalPage { folder, page } => {
                    self.apply_provisional_page(folder, page);
                }
                StoreEvent::Published(result) | StoreEvent::Overlaid(result) => {
                    self.apply_publication_result(result.map_err(|error| error.to_string()));
                }
                StoreEvent::Failure(message) => self.scan_store_stopped(&message),
            }
        }
        handled
    }

    /// The store thread reported a failure nothing else carries, or stopped: the results it still
    /// owed are not coming, so every publication outstanding ends as a failed one.
    fn scan_store_stopped(&mut self, failure: &str) {
        self.fail_scan_store(failure.to_owned());
        while let Some(pending) = self.publications.pop_front() {
            self.settle_publication(pending, Err(failure.to_owned()));
        }
    }

    /// Starts publishing the finished primary scan on the store thread. The map, and the
    /// completion the reader sees, wait for the generation: [`Self::process_scan_store_events`]
    /// swaps it in when it arrives. Ends in exactly one [`FinishedPublication::Primary`].
    pub(crate) fn begin_primary_publication(&mut self) {
        if self.begin_publication(PendingPublication::Primary) {
            self.publication_progress_from = Some(Instant::now() + PUBLICATION_PROGRESS_DELAY);
            return;
        }
        let message = self.scan_results_unavailable_message();
        self.show_scan_results_unavailable(message);
        self.finished_publications
            .push_back(FinishedPublication::Primary);
    }

    /// Asks the store thread to publish the active generation. Returns whether it is now
    /// publishing: `false` means the store was already unavailable, or could not be reached.
    fn begin_publication(&mut self, pending: PendingPublication) -> bool {
        if !self.scan_store_available {
            return false;
        }
        if let Err(error) = self.scan_store.begin_publication() {
            self.abandon_scan_store_generation_with_failure(error.to_string());
            return false;
        }
        self.publications.push_back(pending);
        true
    }

    /// The oldest publication outstanding has ended with `result`.
    fn apply_publication_result(&mut self, result: Result<Publication, String>) {
        let Some(pending) = self.publications.pop_front() else {
            return;
        };
        self.settle_publication(pending, result);
    }

    fn settle_publication(
        &mut self,
        pending: PendingPublication,
        result: Result<Publication, String>,
    ) {
        match pending {
            PendingPublication::Primary => self.complete_primary_publication(result),
            PendingPublication::Rebuild => self.complete_rebuild_publication(result),
            PendingPublication::Overlay { prefix } => {
                self.complete_overlay_publication(result, &prefix);
            }
        }
    }

    fn complete_primary_publication(&mut self, result: Result<Publication, String>) {
        self.publication_progress_from = None;
        let current = self.current_relative();
        let published = match result {
            Ok(publication) if self.scan_store_available => {
                self.scan_store.install(publication);
                if self.scan_store.is_summary_only() {
                    self.scan_store_available = false;
                    self.scan_store_failure = Some("scan store capacity exhausted".to_string());
                    false
                } else {
                    true
                }
            }
            // The store was given up on while the publication was under way: its failure stays
            // the reason, and the generation, if any, is dropped.
            Ok(_) => false,
            Err(failure) => {
                if self.scan_store_available {
                    self.abandon_scan_store_generation_with_failure(failure);
                }
                false
            }
        };
        if published {
            self.show_published_map(&current);
        } else {
            let message = self.scan_results_unavailable_message();
            self.show_scan_results_unavailable(message);
        }
        self.finished_publications
            .push_back(FinishedPublication::Primary);
        self.resume_pending_navigation();
    }

    fn complete_rebuild_publication(&mut self, result: Result<Publication, String>) {
        if self.generation_rebuild_restart_suppressed {
            // The reader pressed Esc while the replacement map was being published: the scanner
            // had already finished, so only here can the cancellation take effect. The map the
            // store thread built is dropped, and the stale one stays.
            drop(result);
            self.finished_publications
                .push_back(FinishedPublication::RebuildCancelled);
            return;
        }
        let failure = if self.scan_store_available {
            match result {
                Ok(publication) => {
                    self.scan_store.install(publication);
                    self.scan_store
                        .is_summary_only()
                        .then(|| "scan store capacity exhausted".to_string())
                }
                Err(failure) => Some(failure),
            }
        } else {
            self.scan_store_failure.clone().or_else(|| {
                Some(
                    "scan store was unavailable before the replacement map could publish"
                        .to_string(),
                )
            })
        };
        self.finished_publications
            .push_back(FinishedPublication::Rebuild { failure });
    }

    fn complete_overlay_publication(
        &mut self,
        result: Result<Publication, String>,
        prefix: &RelativePath,
    ) {
        let publication = match result {
            Ok(publication) if self.scan_store_available => publication,
            // The map was given up on while the overlay was under way: whatever gave it up
            // already showed that, and nothing shows this map.
            Ok(_) => return,
            Err(failure) if !self.scan_store_available => {
                // The store was lost while the overlay was under way (its thread stopped, or the
                // map was given up on). No map follows the deletion, and the one on screen still
                // lists what the deletion removed: it must not stay up as the current map.
                self.scan_store_failure.get_or_insert(failure);
                let message = self.scan_results_unavailable_message();
                self.show_scan_results_unavailable(message);
                self.render_and_update_board();
                return;
            }
            Err(_) => {
                self.invalidate_snapshot_view_for_live_mutation();
                self.render_and_update_board();
                return;
            }
        };
        self.scan_store.install(publication);
        if self.scan_store.is_summary_only() {
            self.invalidate_snapshot_view_for_live_mutation();
            self.render_and_update_board();
            return;
        }
        let fallback = relative_parent(prefix);
        let current = self.current_relative();
        let desired = if current.starts_with(prefix) {
            fallback.clone()
        } else {
            current
        };
        self.invalidate_cached_pages_for_overlay(prefix);
        if self.load_snapshot_page(&desired).is_err() && self.load_snapshot_page(&fallback).is_err()
        {
            self.invalidate_snapshot_view_for_live_mutation();
        }
        self.render_and_update_board();
    }

    /// Shows the map the store thread published, at the folder the reader was looking at, else at
    /// the root.
    fn show_published_map(&mut self, current: &RelativePath) {
        self.snapshot_page_cache = None;
        self.snapshot_page_is_provisional = false;
        self.snapshot_page_history.clear();
        self.snapshot_filter = None;
        self.provisional_page_waiting = None;
        if let Err(error) = self
            .load_snapshot_page(current)
            .or_else(|_| self.load_snapshot_page(&RelativePath::root()))
        {
            self.scan_store_available = false;
            self.scan_store_failure = Some(error.to_string());
            // The unavailable-results screen is drawn over a page, as every state is.
            self.reset_loading_snapshot();
            self.pending_navigation = None;
            self.show_scan_results_unavailable(
                "Excise could not open the completed folder map. Run it again.",
            );
        }
    }

    fn load_snapshot_page(&mut self, folder: &RelativePath) -> Result<(), AppError> {
        self.snapshot_page_history.clear();
        self.load_snapshot_page_after(folder, None)
    }

    fn load_snapshot_page_after(
        &mut self,
        folder: &RelativePath,
        after: Option<PageCursor>,
    ) -> Result<(), AppError> {
        if self.uses_provisional_scan_page() {
            // A live view is the store thread's: it arrives as an event, never from here.
            return Err(AppError::Model(
                "a live scan page is not available until the store thread reports it".to_string(),
            ));
        }
        if self.snapshot_page_is_provisional {
            self.snapshot_page_cache = None;
            self.snapshot_page_history.clear();
            self.snapshot_page_is_provisional = false;
        }
        let generation = self
            .scan_store
            .published()
            .ok_or_else(|| AppError::Model("scan generation was not published".to_string()))?
            .generation();
        if self
            .snapshot_page_cache
            .as_mut()
            .is_some_and(|cache| cache.activate(generation, folder, after.as_ref()))
        {
            return Ok(());
        }
        let request = after.as_ref().map_or_else(
            || PageRequest::first(folder.clone(), SNAPSHOT_PAGE_ENTRIES),
            |after| PageRequest::after(folder.clone(), after.clone(), SNAPSHOT_PAGE_ENTRIES),
        );
        let filter = self.snapshot_filter.clone();
        let root_path = self.scan_root.clone();
        let page_memory_limit = self.page_memory_limit;
        let scan_store_stats = self.scan_store.storage_stats();
        let page = {
            let published = self
                .scan_store
                .published()
                .ok_or_else(|| AppError::Model("scan generation was not published".to_string()))?;
            match filter.as_ref() {
                Some((pattern, filter_root)) => published
                    .filtered_page(request, &root_path, pattern, filter_root)
                    .map_err(|error| AppError::Model(error.to_string()))?,
                None => published
                    .page(request)
                    .map_err(|error| AppError::Model(error.to_string()))?,
            }
        };
        let snapshot = SnapshotTree::from_page_with_filter(
            root_path,
            page,
            page_memory_limit,
            scan_store_stats,
            filter,
        )
        .map_err(model_error)?;
        if let Some(cache) = self.snapshot_page_cache.as_mut() {
            cache.install(snapshot, after);
        } else {
            self.snapshot_page_cache = Some(SnapshotPageCache::new(snapshot, after));
        }
        Ok(())
    }

    /// Installs the live view of one folder as the page on screen.
    fn install_provisional_snapshot(&mut self, page: ScanPage) -> Result<(), AppError> {
        let snapshot = SnapshotTree::from_provisional_page(
            self.scan_root.clone(),
            page,
            self.page_memory_limit,
            self.scan_store.storage_stats(),
        )
        .map_err(model_error)?;
        self.snapshot_filter = None;
        if !self.snapshot_page_is_provisional {
            self.snapshot_page_cache = None;
            self.snapshot_page_history.clear();
        }
        if let Some(cache) = self.snapshot_page_cache.as_mut() {
            cache.install(snapshot, None);
        } else {
            self.snapshot_page_cache = Some(SnapshotPageCache::new(snapshot, None));
        }
        self.snapshot_page_is_provisional = true;
        Ok(())
    }

    /// Opens a folder now, or once the store thread has the page for it. Returns false only when
    /// the folder could not be opened; a drill that waits has been accepted.
    fn navigate(&mut self, navigation: PendingNavigation) -> bool {
        if self.pending_navigation.is_some() {
            return true;
        }
        if self.navigation_waits_for_store() {
            self.pending_navigation = Some(navigation);
            if self.uses_provisional_scan_page() {
                self.request_live_page();
            }
            return true;
        }
        self.perform_navigation(navigation)
    }

    /// Opens a folder of the published map.
    fn perform_navigation(&mut self, navigation: PendingNavigation) -> bool {
        let leaving = self
            .snapshot_page_cache
            .as_ref()
            .map(|cache| cache.current().current_id());
        if !self.begin_snapshot_page_navigation(
            &navigation.folder,
            None,
            SnapshotPageHistoryChange::Reset,
        ) {
            return false;
        }
        self.finish_navigation(navigation.kind, leaving);
        true
    }

    /// What a drill does to the map once the page it opens is on screen. `leaving` is the folder
    /// that was on screen before.
    fn finish_navigation(&mut self, kind: NavigationKind, leaving: Option<NodeId>) {
        match kind {
            NavigationKind::Enter { pivot } => {
                self.board.record_current_zoom_level();
                if let Some(pivot) = pivot {
                    self.board.pivot_transition_on_geometry(pivot);
                }
                self.board.reset_zoom_index();
                self.board.reset_selected_index();
                self.render_and_update_board();
            }
            NavigationKind::Up { leaving_relative } => {
                if let Some(zoom_level) = self.board.pop_previous_zoom_level() {
                    self.board.set_zoom_index(zoom_level);
                }
                if let Some(leaving) = leaving {
                    self.board.pivot_transition_on(Pivot::Entry(leaving));
                }
                self.render_and_update_board();
                if let Some(node_id) = self
                    .snapshot_page_cache
                    .as_ref()
                    .and_then(|cache| cache.current().id_for_relative(&leaving_relative))
                {
                    self.board.select_node(node_id);
                } else {
                    self.board.select_largest();
                }
                self.mark_dirty();
            }
        }
    }

    /// Opens the folder a drill waited for, now that the map it belongs to is published.
    fn resume_pending_navigation(&mut self) {
        let Some(navigation) = self.pending_navigation.take() else {
            return;
        };
        if self.scan_store_available {
            self.perform_navigation(navigation);
        }
    }

    fn reset_loading_snapshot(&mut self) {
        let generation = self
            .scan_store
            .active_generation()
            .or_else(|| self.scan_store.published_generation())
            .unwrap_or_else(ScanGeneration::initial);
        let snapshot = SnapshotTree::loading(
            self.scan_root.clone(),
            generation,
            self.page_memory_limit,
            self.scan_store.storage_stats(),
        )
        .expect("the validated root snapshot must fit the page memory limit");
        self.snapshot_page_cache = Some(SnapshotPageCache::new(snapshot, None));
        self.snapshot_page_is_provisional = true;
    }

    /// Replaces the current view synchronously from the bounded child-query
    /// index. A completed scan never yields an empty transitional page.
    fn begin_snapshot_page_navigation(
        &mut self,
        folder: &RelativePath,
        after: Option<&PageCursor>,
        history_change: SnapshotPageHistoryChange,
    ) -> bool {
        if !self.scan_store_available {
            return false;
        }
        if let Err(error) = self.load_snapshot_page_after(folder, after.cloned()) {
            self.show_error(format!("Could not open this scan page: {error}"));
            return false;
        }
        match history_change {
            SnapshotPageHistoryChange::Reset => self.snapshot_page_history.clear(),
            SnapshotPageHistoryChange::Push(prior) => {
                if self.snapshot_page_history.len() == MAX_SNAPSHOT_PAGE_HISTORY {
                    self.snapshot_page_history.remove(0);
                }
                self.snapshot_page_history.push(prior);
            }
            SnapshotPageHistoryChange::Pop(previous) => {
                if self.snapshot_page_history.last() == Some(&previous) {
                    self.snapshot_page_history.pop();
                }
            }
        }
        true
    }

    pub(crate) fn next_snapshot_page(&mut self) -> bool {
        let (folder, next_after, current_after) = self
            .snapshot_page_cache
            .as_ref()
            .map(|cache| {
                (
                    cache.current().current_relative().clone(),
                    cache.current().next_after().cloned(),
                    cache.current_after().cloned(),
                )
            })
            .expect("snapshot use was checked above");
        let Some(next_after) = next_after else {
            return false;
        };
        let prior = (folder.clone(), current_after);
        if !self.begin_snapshot_page_navigation(
            &folder,
            Some(&next_after),
            SnapshotPageHistoryChange::Push(prior),
        ) {
            return false;
        }
        self.board.reset_selected_index();
        self.render_and_update_board();
        true
    }

    pub(crate) fn previous_snapshot_page(&mut self) -> bool {
        let Some(previous) = self.snapshot_page_history.last().cloned() else {
            return false;
        };
        let (folder, after) = previous.clone();
        if !self.begin_snapshot_page_navigation(
            &folder,
            after.as_ref(),
            SnapshotPageHistoryChange::Pop(previous),
        ) {
            return false;
        }
        self.board.reset_selected_index();
        self.render_and_update_board();
        true
    }
    fn invalidate_snapshot_view_for_live_mutation(&mut self) {
        self.pending_navigation = None;
        self.provisional_page_waiting = None;
        if let Err(error) = self.scan_store.discard_active() {
            self.scan_store_failure
                .get_or_insert_with(|| error.to_string());
        }
        let retains_published_snapshot = self.can_retain_published_snapshot();
        self.scan_store_available = retains_published_snapshot;
        self.generation_rebuild_invalidated |= self.generation_rebuild_active;
        self.generation_rebuild_restart_suppressed = false;
        self.generation_rebuild_required = true;
        self.cancel_pending_deletion_work_and_dismiss_confirmation();
        if !retains_published_snapshot {
            self.snapshot_filter = None;
            self.snapshot_page_history.clear();
            self.reset_loading_snapshot();
        }
    }

    fn invalidate_cached_pages_for_overlay(&mut self, removed_prefix: &RelativePath) {
        self.snapshot_page_cache = None;
        self.snapshot_page_history.clear();
        if self
            .snapshot_filter
            .as_ref()
            .is_some_and(|(_, filter_root)| filter_root.starts_with(removed_prefix))
        {
            self.snapshot_filter = None;
        }
    }

    /// Applies a completed mutation to the canonical generation boundary.
    ///
    /// A complete removal of its exact target can safely publish an overlay that drops that
    /// prefix: the store thread builds it from the map the reader has installed, with the folder
    /// that held the target recorded as the file system has it now, and the map swaps it in when
    /// it arrives ([`Self::complete_overlay_publication`]); until then the reader keeps the map
    /// they have. Any partial outcome invalidates the immutable snapshot instead of presenting a
    /// fabricated mixture of pre- and post-deletion facts, and so does a removal the map cannot
    /// describe exactly: one that may have left other links to a file it removed (the map has
    /// its files by path, and does not say where those are), and one the store thread's overlay
    /// then fails to describe. The map is scanned again.
    fn reconcile_generation_after_deletion(&mut self, report: &DeletionReport) {
        if report.deleted_entries() == 0 {
            return;
        }
        if !self.scan_store_available
            || !report.target_was_removed()
            || report.deleted_files_may_have_other_links()
        {
            self.invalidate_snapshot_view_for_live_mutation();
            return;
        }
        let Some(prefix) = RelativePath::from_path(&report.root_relative_path)
            .ok()
            .filter(|path| !path.is_root())
        else {
            self.invalidate_snapshot_view_for_live_mutation();
            return;
        };
        // An overlay derives from the installed map, and only from it: while the primary scan has
        // not published one, or another publication is still owed (the installed map is then
        // about to be replaced, and an overlay of it would replace its successor with a map
        // older than that one), the deletion invalidates the map, as it always has.
        if self.scan_store.published().is_none() || !self.publications.is_empty() {
            self.invalidate_snapshot_view_for_live_mutation();
            return;
        }
        let Ok(next_generation) = self.scan_store.next_generation() else {
            self.invalidate_snapshot_view_for_live_mutation();
            return;
        };
        let root = ScanRoot {
            path: self.scan_root.clone(),
            identity: self.root_identity.clone(),
        };
        if self
            .scan_store
            .begin_overlay(next_generation, &prefix, root)
            .is_err()
        {
            self.invalidate_snapshot_view_for_live_mutation();
            return;
        }
        self.publications
            .push_back(PendingPublication::Overlay { prefix });
        if self
            .session_coordinator
            .advance_generation(next_generation)
            .is_err()
        {
            self.invalidate_snapshot_view_for_live_mutation();
        }
    }

    /// Starts a fresh root generation after a partial deletion invalidated its
    /// prior immutable snapshot. Returns false when no rebuild is pending, or while the store
    /// thread still owes a publication: the new generation would otherwise begin in the middle of
    /// the previous one's.
    pub(crate) fn begin_generation_rebuild(&mut self) -> Result<bool, AppError> {
        if !self.generation_rebuild_required
            || self.generation_rebuild_active
            || self.generation_rebuild_restart_suppressed
            || self.scan_store_busy()
        {
            return Ok(false);
        }
        let retains_published_snapshot = self.can_retain_published_snapshot();
        let next_generation = self
            .scan_store
            .next_generation()
            .map_err(scan_store_error)?;
        self.scan_store
            .begin_generation(next_generation)
            .map_err(store_unavailable_error)?;
        self.session_coordinator
            .advance_generation(next_generation)
            .map_err(|error| AppError::Worker(error.to_string()))?;
        self.scan_store_available = true;
        self.scan_store_failure = None;
        self.generation_rebuild_active = true;
        self.generation_rebuild_invalidated = false;
        self.generation_rebuild_target = Some(RelativePath::root());
        self.generation_rebuild_required = false;
        let target = self.scan_root.clone();
        self.ui_effects.reset_loading_activity();
        if retains_published_snapshot {
            self.board.disarm_scan_reveal();
        } else {
            self.board.arm_scan_reveal();
        }
        self.replace_ui_mode(UiMode::Rebuilding { target });
        self.render_and_update_board();
        Ok(true)
    }

    /// Prevents an explicit cancellation from immediately restarting the same stale refresh.
    pub(crate) fn suppress_generation_rebuild_restart(&mut self) {
        self.generation_rebuild_restart_suppressed = true;
    }

    #[cfg(test)]
    pub(crate) fn require_generation_rebuild_for_test(&mut self) {
        self.generation_rebuild_required = true;
    }

    pub const fn flash_space_freed(&mut self) {
        self.ui_effects.flash_space_freed = true;
        self.mark_dirty();
    }

    pub const fn unflash_space_freed(&mut self) {
        self.ui_effects.flash_space_freed = false;
        self.mark_dirty();
    }

    pub const fn set_path_to_red(&mut self) {
        self.ui_effects.current_path_is_red = true;
        self.mark_dirty();
    }

    pub const fn reset_current_path_color(&mut self) {
        self.ui_effects.current_path_is_red = false;
        self.mark_dirty();
    }

    /// The primary scan is over for the reader: the interface leaves loading and shows the
    /// finished map, if it could be published, with the cursor on the largest entry unless the
    /// reader has already chosen one (`land_untouched_cursor_on_largest` has the rule).
    pub fn start_ui(&mut self) {
        self.loaded = true;
        self.retarget_completed_scan_transient_modals();
        if matches!(self.ui_mode, UiMode::Loading) {
            self.ui_mode = UiMode::Normal;
            self.render_and_update_board();
        } else if let UiMode::ThemePicker { return_to, .. } | UiMode::Exiting { return_to, .. } =
            &mut self.ui_mode
        {
            if matches!(return_to, ThemePickerReturn::Loading) {
                *return_to = ThemePickerReturn::Normal;
            }
            self.render_and_update_board();
        } else {
            self.render_and_update_board();
        }
        // The page is still the live one only when no finished map came to replace it (the
        // publication failed): there is nothing to put the cursor on then.
        if !self.snapshot_page_is_provisional {
            self.land_untouched_cursor_on_largest();
        }
        if let Some(suspended) = self.suspended_ui_mode.as_mut() {
            match suspended {
                UiMode::Loading => *suspended = UiMode::Normal,
                UiMode::ThemePicker { return_to, .. } | UiMode::Exiting { return_to, .. }
                    if matches!(return_to, ThemePickerReturn::Loading) =>
                {
                    *return_to = ThemePickerReturn::Normal;
                }
                _ => {}
            }
        }
    }

    /// A scan the reader watched fill in has ended and its map is on screen. While it filled in,
    /// the cursor stayed on the entry the map selected first, which is whichever entry the scan
    /// listed first, large or not. Unless the reader has moved or clicked, it goes to the largest
    /// actionable entry of the folder on screen now, once. A cursor the reader placed stays where
    /// they put it. Only a scan's end does this, and no refresh: a rebuild that keeps the map the
    /// reader had on screen never calls it, so a deletion leaves the cursor as `complete_deletion`
    /// describes.
    fn land_untouched_cursor_on_largest(&mut self) {
        self.board.select_largest_unless_locked();
        self.mark_dirty();
    }

    pub(crate) fn record_loading_entry(&mut self, entry_path: PathBuf) {
        self.ui_effects.record_loading_entry(entry_path);
    }

    /// Records a scanner path that cannot be represented as a canonical entry.
    /// Published pages remain usable and expose the omitted-path count separately.
    pub(crate) fn record_scan_store_unrecorded_path(&mut self) {
        if self.scan_store_available {
            self.scan_store.record_unrecorded_path();
        }
    }

    /// Records a directory the scanner discovered (and sealed as `Complete`
    /// through its parent's listing) but later failed to open or list. Its
    /// own path, and every ancestor up to the scan root, is reported
    /// `Uncertain` with an open upper bound once the generation publishes.
    pub(crate) fn record_unreadable_directory(&mut self, path: &Path) {
        if !self.scan_store_available {
            return;
        }
        let Ok(relative) = path.strip_prefix(&self.scan_root) else {
            return;
        };
        let Ok(relative) = RelativePath::from_path(relative) else {
            return;
        };
        self.scan_store.record_unreadable_directory(relative);
    }

    pub(crate) fn cancel_primary_scan(&mut self) -> Result<(), AppError> {
        self.scan_store
            .cancel_active()
            .map_err(|error| AppError::Model(error.to_string()))
    }

    #[must_use]
    pub(crate) fn scan_store_stats(&self) -> (u64, u64) {
        self.scan_store.storage_stats()
    }

    #[must_use]
    pub fn internal_scan_paths(&self) -> Vec<PathBuf> {
        self.scan_store.internal_paths()
    }

    #[must_use]
    pub(crate) const fn scan_session_id(&self) -> crate::scan_session::ScanSessionId {
        self.scan_store.session()
    }

    #[must_use]
    pub(crate) fn session_coordinator(&self) -> SessionCoordinator {
        self.session_coordinator.clone()
    }

    pub(crate) fn set_scheduler_snapshot(&mut self, snapshot: Option<SchedulerSnapshot>) {
        if self.scheduler_snapshot != snapshot {
            self.scheduler_snapshot = snapshot;
            self.mark_dirty();
        }
    }

    pub(crate) fn scan_input_run_factory(&self) -> Result<ScanInputRunFactory, AppError> {
        self.scan_store
            .input_run_factory()
            .map_err(scan_store_error)
    }

    /// Hands worker-sealed runs to the store thread for admission, which validates each run's
    /// work lease. Whatever happens to the batch from here, its in-flight credit returns exactly
    /// once, when the batch is dropped: after the store thread admits it, or here when the store
    /// is already unavailable.
    pub(crate) fn admit_scan_input_runs(&mut self, lease: &WorkLease, batch: SealedBatch) {
        if batch.is_empty() || !self.scan_store_available {
            return;
        }
        if let Err(error) = self.scan_store.admit(lease, batch) {
            self.abandon_scan_store_generation_with_failure(error.to_string());
        }
    }

    /// Treats a worker's scan-store capacity failure exactly like a failed admission call
    /// (`admit_scan_input_runs`): marks the scan store unavailable with the scratch-space
    /// remedy and returns `true` the first time. Once it is already unavailable, later calls
    /// are a no-op and return `false`, matching headless's "first one wins" handling of the
    /// same condition: a worker thread's own sealing failure is the common real-world trigger
    /// (the first bytes to fail writing to a full volume), and the disk tends to stay full, so
    /// more than one worker can report this in the same scan.
    pub(crate) fn note_scan_store_capacity_failure_from_worker(&mut self, message: String) -> bool {
        if !self.scan_store_available {
            return false;
        }
        self.abandon_scan_store_generation_with_failure(message);
        true
    }

    pub fn reset_ui_mode(&mut self) {
        if matches!(self.ui_mode, UiMode::ScreenTooSmall) {
            self.ui_mode = self
                .suspended_ui_mode
                .take()
                .unwrap_or_else(|| self.navigation_mode());
            self.mark_dirty();
        }
    }

    fn enter_screen_too_small(&mut self) {
        if matches!(
            self.ui_mode,
            UiMode::ScreenTooSmall | UiMode::Exiting { .. }
        ) {
            return;
        }
        let previous = std::mem::replace(&mut self.ui_mode, UiMode::ScreenTooSmall);
        if matches!(previous, UiMode::DeleteConfirm { .. }) {
            self.ui_mode = previous;
            self.replace_ui_mode(UiMode::ScreenTooSmall);
        } else {
            self.suspended_ui_mode = Some(previous);
        }
    }

    fn navigation_mode(&self) -> UiMode {
        if !self.loaded {
            return UiMode::Loading;
        }
        if !self.scan_store_available {
            return UiMode::ScanResultsUnavailable(self.scan_results_unavailable_message());
        }
        if self.generation_rebuild_required
            && !self.generation_rebuild_active
            && self.can_retain_published_snapshot()
        {
            return UiMode::StaleSnapshot;
        }
        UiMode::Normal
    }

    fn completed_scan_return_target(&self) -> ThemePickerReturn {
        match self.navigation_mode() {
            UiMode::Loading => ThemePickerReturn::Loading,
            UiMode::Rebuilding { target } => ThemePickerReturn::Rebuilding { target },
            UiMode::StaleSnapshot => ThemePickerReturn::StaleSnapshot,
            UiMode::ScanResultsUnavailable(message) => {
                ThemePickerReturn::ScanResultsUnavailable(message)
            }
            _ => ThemePickerReturn::Normal,
        }
    }

    fn retarget_completed_scan_transient_modals(&mut self) {
        let target = self.completed_scan_return_target();
        let retarget = |mode: &mut UiMode| match mode {
            UiMode::ErrorMessage { return_to, .. } | UiMode::Notice { return_to, .. }
                if matches!(
                    return_to,
                    ThemePickerReturn::Loading | ThemePickerReturn::Rebuilding { .. }
                ) =>
            {
                *return_to = target.clone();
            }
            _ => {}
        };
        retarget(&mut self.ui_mode);
        if let Some(suspended) = self.suspended_ui_mode.as_mut() {
            retarget(suspended);
        }
    }

    fn modal_return_target(&self) -> ThemePickerReturn {
        match &self.ui_mode {
            UiMode::Loading => ThemePickerReturn::Loading,
            UiMode::Rebuilding { target } => ThemePickerReturn::Rebuilding {
                target: target.clone(),
            },
            UiMode::StaleSnapshot => ThemePickerReturn::StaleSnapshot,
            UiMode::ScanResultsUnavailable(message) => {
                ThemePickerReturn::ScanResultsUnavailable(message.clone())
            }
            UiMode::ThemePicker { return_to, .. }
            | UiMode::Exiting { return_to, .. }
            | UiMode::ErrorMessage { return_to, .. }
            | UiMode::Notice { return_to, .. } => return_to.clone(),
            _ => ThemePickerReturn::Normal,
        }
    }

    pub fn dismiss_transient_modal(&mut self) {
        let return_to = match &self.ui_mode {
            UiMode::ErrorMessage { return_to, .. } | UiMode::Notice { return_to, .. } => {
                return_to.clone()
            }
            _ => return,
        };
        self.ui_mode = return_to.into_mode();
        self.mark_dirty();
    }
    fn replace_ui_mode(&mut self, mode: UiMode) {
        self.cancel_foreground_deletion_modal();
        self.suspended_ui_mode = None;
        self.ui_mode = mode;
    }

    /// Cancels a visible confirmation and keeps a planning reservation live until
    /// the planner acknowledges it. Resize and modal replacement share this fence.
    pub(crate) fn cancel_foreground_deletion_modal(&mut self) {
        let Some(work_id) = self.deletion_modal_work_id.take() else {
            return;
        };
        self.deletion_plan_cancellation_requested |= self.deletion_work.cancel_modal(work_id);
        self.sync_deletion_work_summary();
    }

    #[must_use]
    pub(crate) fn take_deletion_plan_cancellation(&mut self) -> bool {
        std::mem::take(&mut self.deletion_plan_cancellation_requested)
    }

    pub fn show_warning_modal(&mut self) {
        self.replace_ui_mode(UiMode::WarningMessage);
        self.mark_dirty();
    }

    pub fn prompt_exit(&mut self, work: ExitWork) {
        // A visible confirmation owns the requested target. Return it to the
        // bounded rail before the exit dialog replaces the foreground decision.
        let previous = std::mem::replace(&mut self.ui_mode, UiMode::Loading);
        let return_to = match &previous {
            UiMode::Loading => ThemePickerReturn::Loading,
            UiMode::Rebuilding { target } => ThemePickerReturn::Rebuilding {
                target: target.clone(),
            },
            UiMode::StaleSnapshot => ThemePickerReturn::StaleSnapshot,
            UiMode::ScanResultsUnavailable(message) => {
                ThemePickerReturn::ScanResultsUnavailable(message.clone())
            }
            UiMode::Exiting { return_to, .. } => return_to.clone(),
            _ => ThemePickerReturn::Normal,
        };
        if let UiMode::DeleteConfirm {
            work_id, target, ..
        } = previous
        {
            self.deletion_modal_work_id = None;
            if !self.deletion_work.return_confirmation(work_id, target) {
                self.deletion_plan_cancellation_requested |=
                    self.deletion_work.cancel_modal(work_id);
            }
        }
        self.ui_mode = UiMode::Exiting { work, return_to };
        self.sync_deletion_work_summary();
        crate::test_events::quit_prompt();
        self.mark_dirty();
    }

    pub fn dismiss_exit(&mut self) {
        let navigation = self.navigation_mode();
        let mode = std::mem::replace(&mut self.ui_mode, navigation);
        let UiMode::Exiting { return_to, .. } = mode else {
            self.ui_mode = mode;
            return;
        };
        if self.suspended_ui_mode.is_some() {
            self.ui_mode = UiMode::ScreenTooSmall;
        } else {
            self.ui_mode = return_to.into_mode();
            self.show_next_deletion_confirmation();
        }
        self.mark_dirty();
    }

    pub const fn exit(&mut self) {
        self.is_running = false;
    }

    pub(crate) fn handle_enter_action(&mut self) -> EnterAction {
        if !self.board.has_selected_index() {
            self.board.move_to_largest_folder();
        }
        let Some((id, synthetic_kind)) = self
            .board
            .currently_selected()
            .map(|tile| (tile.node_id, tile.synthetic_kind))
        else {
            return EnterAction::None;
        };
        if self.deletion_target_is_busy(id) {
            return EnterAction::None;
        }
        if synthetic_kind == Some(SyntheticKind::Shared) {
            return EnterAction::None;
        }
        self.enter_selected();
        EnterAction::Drill
    }

    pub fn move_selected_right(&mut self) {
        self.board.move_selected_right();
        self.mark_dirty();
    }

    pub fn select_at(&mut self, x: u16, y: u16) -> bool {
        if self.mouse_enabled && self.board.select_at(x, y) {
            self.mark_dirty();
            true
        } else {
            false
        }
    }

    pub fn move_selected_left(&mut self) {
        self.board.move_selected_left();
        self.mark_dirty();
    }

    pub fn move_selected_down(&mut self) {
        self.board.move_selected_down();
        self.mark_dirty();
    }

    pub fn move_selected_up(&mut self) {
        self.board.move_selected_up();
        self.mark_dirty();
    }

    pub fn enter_selected(&mut self) {
        let Some(target) = self.board.currently_selected().map(|tile| tile.node_id) else {
            return;
        };
        if self.deletion_target_is_busy(target) {
            return;
        }
        let pivot = self
            .board
            .selected_rendered_geometry()
            .or_else(|| self.board.selected_geometry());
        let folder = self
            .snapshot_page_cache
            .as_ref()
            .and_then(|cache| cache.current().selected_folder(target));
        let Some(folder) = folder else {
            return;
        };
        self.navigate(PendingNavigation {
            folder,
            kind: NavigationKind::Enter { pivot },
        });
    }

    pub fn go_up(&mut self) -> bool {
        let (leaving_relative, parent) = self
            .snapshot_page_cache
            .as_ref()
            .map(|cache| {
                let snapshot = cache.current();
                (
                    snapshot.current_relative().clone(),
                    snapshot.parent_folder(),
                )
            })
            .expect("every navigable app state retains a snapshot page");
        let Some(parent) = parent else {
            return false;
        };
        self.navigate(PendingNavigation {
            folder: parent,
            kind: NavigationKind::Up { leaving_relative },
        })
    }

    /// Returns the identity-bound target represented by the rendered tile.
    pub(crate) fn request_deletion(&mut self) -> Option<FileToDelete> {
        if self.deletion_is_blocked() {
            return None;
        }
        if self
            .display
            .size()
            .is_ok_and(|area| area.width < 50 || area.height < 15)
        {
            self.show_error("Resize to at least 50 x 15 before permanent deletion");
            return None;
        }
        // Every deletion allowed to start keeps its report, so the history counts the deletions
        // still running with the reports it holds. An export leaves the reports it is writing in
        // place until it ends, and a deletion that finishes meanwhile must still find room.
        if self
            .deletion_history
            .len()
            .saturating_add(self.deletion_work.len())
            >= MAX_RETAINED_DELETION_REPORTS
            || self.remaining_deletion_history_bytes() < MINIMUM_PLAN_BYTES
        {
            self.show_error("Deletion history is full; export or restart before deleting");
            return None;
        }
        if !deletion_supported() {
            self.show_error("Permanent deletion is unavailable on this platform");
            return None;
        }
        let selected = self.board.currently_selected()?;
        if self.deletion_target_is_busy(selected.node_id) {
            return None;
        }
        if self.uses_provisional_scan_page() {
            let result = self.snapshot_page_cache.as_ref().and_then(|cache| {
                let snapshot = cache.current();
                snapshot
                    .node(selected.node_id)
                    .filter(|node| node.name.as_ref() == selected.name.as_os_str())
                    .map(|_| {
                        snapshot
                            .deletion_target_from_preview(selected.node_id, self.show_apparent_size)
                    })
            });
            return if let Some(Ok(target)) = result {
                Some(target)
            } else {
                self.show_notice(
                    "Wait for this item to receive a verified scan preview before deleting",
                );
                None
            };
        }

        let (relative, path) = self.snapshot_page_cache.as_ref().and_then(|cache| {
            let snapshot = cache.current();
            snapshot
                .relative_for_id(selected.node_id)
                .cloned()
                .zip(snapshot.path_for_id(selected.node_id))
        })?;
        if path.file_name() != Some(selected.name.as_os_str())
            || path.parent() != Some(self.current_folder_path().as_path())
        {
            return None;
        }
        let entry = match self
            .scan_store
            .published()
            .ok_or_else(|| "scan generation was not published".to_string())
            .and_then(|published| {
                published
                    .page_entry(&relative)
                    .map_err(|error| error.to_string())?
                    .ok_or_else(|| "selected item is absent from the scan generation".to_string())
            }) {
            Ok(entry) => entry,
            Err(error) => {
                self.show_error(format!("Could not verify selected item: {error}"));
                return None;
            }
        };
        match SnapshotTree::deletion_target_from_entry(
            self.scan_root.clone(),
            selected.node_id,
            &relative,
            entry,
            self.show_apparent_size,
        ) {
            Ok(target) => Some(target),
            Err(error) => {
                self.show_error(error.to_string());
                None
            }
        }
    }

    #[must_use]
    pub const fn reduced_deletion_guardrails(&self) -> bool {
        self.delete_confirmation_disabled
    }

    /// What the history can still take that no report holds.
    const fn deletion_history_room_bytes(&self) -> usize {
        self.deletion_history_limit
            .saturating_sub(self.deletion_history_bytes)
    }

    /// What a deletion that has not started may plan with: the room no report holds, less what
    /// the reports of the deletions already running may still take.
    #[must_use]
    pub fn remaining_deletion_history_bytes(&self) -> usize {
        self.deletion_history_room_bytes()
            .saturating_sub(self.deletion_work.history_bytes_in_flight())
    }

    /// Each plan may use half of what the history has left, so that the plans of the deletions
    /// running together, each budgeted from what the one before it left, fit it together.
    #[must_use]
    pub fn maximum_deletion_plan_bytes(&self) -> usize {
        self.remaining_deletion_history_bytes() / 2
    }

    pub(crate) fn queue_deletion_confirmation(
        &mut self,
        target: FileToDelete,
        reduced_guardrails: bool,
        maximum_bytes: usize,
        now: Duration,
    ) -> bool {
        if self.deletion_is_blocked() {
            return false;
        }
        match self.deletion_work.enqueue_confirmation(
            target,
            reduced_guardrails,
            maximum_bytes,
            now,
        ) {
            Ok(_) => {
                self.sync_deletion_work_summary();
                true
            }
            Err(error) => {
                self.show_error(error.message());
                false
            }
        }
    }

    #[must_use]
    pub(crate) fn next_deletion_planning_work(&mut self) -> Option<DeletionWorkCommand> {
        if self.deletion_is_unavailable() {
            self.cancel_pending_deletion_work_and_dismiss_confirmation();
            return None;
        }
        if self.deletion_waits_for_store() {
            return None;
        }
        let command = self.deletion_work.next_planning_command();
        self.sync_deletion_work_summary();
        command
    }

    #[must_use]
    pub(crate) fn next_deletion_execution_work(&mut self) -> Option<DeletionWorkCommand> {
        if self.deletion_is_unavailable() {
            self.cancel_pending_deletion_work_and_dismiss_confirmation();
            return None;
        }
        if self.deletion_waits_for_store() {
            return None;
        }
        let command = self.deletion_work.next_execution_command();
        self.sync_deletion_work_summary();
        command
    }

    pub(crate) fn restore_deletion_work(&mut self, command: DeletionWorkCommand) {
        self.deletion_work.restore_unsubmitted(command);
        self.sync_deletion_work_summary();
    }

    pub(crate) fn deletion_plan_ready(
        &mut self,
        work_id: DeletionWorkId,
        plan: Box<DeletionPlan>,
    ) -> bool {
        let ready = self.deletion_work.planning_succeeded(work_id, plan);
        if !ready {
            self.deletion_work.discard_cancelled_event(work_id);
        }
        self.sync_deletion_work_summary();
        ready
    }

    pub(crate) fn deletion_plan_cancelled(&mut self, work_id: DeletionWorkId) {
        self.deletion_work.planning_cancelled(work_id);
        self.deletion_work.discard_cancelled_event(work_id);
        self.sync_deletion_work_summary();
    }

    pub(crate) fn deletion_plan_stale(&mut self, work_id: DeletionWorkId) -> bool {
        let stale = self.deletion_work.planning_stale(work_id);
        if !stale {
            self.deletion_work.discard_cancelled_event(work_id);
        }
        self.sync_deletion_work_summary();
        stale
    }

    pub(crate) fn deletion_plan_failed(&mut self, work_id: DeletionWorkId) -> bool {
        let failed = self.deletion_work.planning_failed(work_id);
        self.deletion_work.discard_cancelled_event(work_id);
        self.sync_deletion_work_summary();
        failed
    }

    pub(crate) fn deletion_execution_stale(&mut self, work_id: DeletionWorkId) -> bool {
        let stale = self.deletion_work.execution_stale(work_id);
        self.sync_deletion_work_summary();
        stale
    }

    pub(crate) fn deletion_execution_failed(&mut self, work_id: DeletionWorkId) -> bool {
        let failed = self.deletion_work.execution_failed(work_id);
        self.sync_deletion_work_summary();
        failed
    }
    pub(crate) fn deletion_execution_finished(
        &mut self,
        work_id: DeletionWorkId,
        completion: WorkCompletion,
    ) -> bool {
        let finished = self.deletion_work.execution_finished(work_id, completion);
        if !finished {
            self.deletion_work.discard_cancelled_event(work_id);
        }
        self.sync_deletion_work_summary();
        finished
    }

    #[must_use]
    pub(crate) fn begin_deletion_departure(
        &mut self,
        scan_root: &Path,
        target_relative_path: &Path,
        deleted_entries: u64,
        now: Duration,
    ) -> bool {
        if !self.ui_mode.allows_motion() || self.board.is_list_layout() {
            return false;
        }
        let Ok(target_relative_path) = RelativePath::from_path(target_relative_path) else {
            return false;
        };
        let Some(tree) = self
            .snapshot_page_cache
            .as_ref()
            .map(SnapshotPageCache::current)
        else {
            return false;
        };
        if tree.scan_root() != scan_root {
            return false;
        }
        let Some(node_id) = tree.id_for_relative(&target_relative_path) else {
            return false;
        };
        let Some(tile) = self
            .board
            .rendered_tiles()
            .iter()
            .find(|tile| tile.node_id == node_id)
        else {
            return false;
        };
        let tile_cells = u64::from(tile.width)
            .saturating_mul(u64::from(tile.height).div_ceil(u64::from(HALF_ROWS_PER_CELL)));
        let duration = crate::state::deletion_departure_duration(deleted_entries, tile_cells);
        self.ui_effects.begin_deletion_departure(
            tile.clone(),
            tree.current_relative().clone(),
            now,
            duration,
        );
        self.mark_dirty();
        true
    }

    #[must_use]
    pub(crate) fn deletion_departure_is_finished(&self, now: Duration) -> bool {
        self.ui_effects.deletion_departure_is_finished(now)
    }

    #[must_use]
    pub(crate) fn deletion_departure_deadline(&self) -> Option<Duration> {
        self.ui_effects
            .deletion_departure()
            .map(|departure| departure.started_at.saturating_add(departure.duration))
    }

    #[must_use]
    pub(crate) const fn has_deletion_departure(&self) -> bool {
        self.ui_effects.has_deletion_departure()
    }

    pub(crate) fn clear_deletion_departure(&mut self) {
        self.ui_effects.clear_deletion_departure();
        self.mark_dirty();
    }

    #[must_use]
    fn deletion_target_is_busy(&self, node_id: NodeId) -> bool {
        self.snapshot_page_cache.as_ref().is_some_and(|cache| {
            let tree = cache.current();
            tree.relative_path_for_id(node_id)
                .is_some_and(|relative_path| {
                    self.deletion_work
                        .rail_item_for_relative_path(tree.scan_root(), relative_path)
                        .is_some()
                })
        })
    }

    #[must_use]
    pub fn deletion_work_summary(&self) -> crate::state::DeletionWorkSummary {
        self.deletion_work.summary()
    }

    pub(crate) fn show_next_deletion_confirmation(&mut self) -> bool {
        if self.deletion_is_blocked() || !self.ui_mode.can_present_deletion_confirmation() {
            return false;
        }
        let return_to = match &self.ui_mode {
            UiMode::Loading => ThemePickerReturn::Loading,
            UiMode::Normal => ThemePickerReturn::Normal,
            _ => return false,
        };
        let Some((work_id, target)) = self.deletion_work.take_next_confirmation() else {
            return false;
        };
        let challenge = match confirmation_challenge_for_target(&target, false) {
            Ok(challenge) => challenge,
            Err(error) => {
                self.deletion_work.discard(work_id);
                self.sync_deletion_work_summary();
                self.show_error(format!("Deletion confirmation failed: {error}"));
                return false;
            }
        };
        self.deletion_modal_work_id = Some(work_id);
        self.ui_mode = UiMode::DeleteConfirm {
            work_id,
            target,
            challenge,
            input: String::new(),
            return_to,
        };
        self.sync_deletion_work_summary();
        self.mark_dirty();
        true
    }

    pub(crate) fn cancel_deletion_confirmation(&mut self) -> bool {
        let return_to = match &self.ui_mode {
            UiMode::DeleteConfirm { return_to, .. } => return_to.clone(),
            _ => return false,
        };
        self.replace_ui_mode(return_to.into_mode());
        self.mark_dirty();
        true
    }

    pub(crate) fn queue_confirmed_deletion(
        &mut self,
        work_id: DeletionWorkId,
        target: Box<FileToDelete>,
        now: Duration,
    ) -> bool {
        if self.deletion_is_unavailable() {
            self.cancel_pending_deletion_work_and_dismiss_confirmation();
            return false;
        }
        if !self.deletion_work.queue_confirmation(work_id, target, now) {
            self.show_error("Deletion confirmation did not match pending work");
            return false;
        }
        self.sync_deletion_work_summary();
        true
    }
    pub(crate) fn record_deletion_notice(&mut self, notice: &'static str) {
        self.ui_effects.record_deletion_notice(notice);
        self.mark_dirty();
    }

    pub(crate) fn cancel_pending_deletion_work(&mut self) {
        self.cancel_foreground_deletion_modal();
        self.deletion_plan_cancellation_requested |= self.deletion_work.cancel_pending();
        self.sync_deletion_work_summary();
    }

    fn cancel_pending_deletion_work_and_dismiss_confirmation(&mut self) {
        let return_to = match &self.ui_mode {
            UiMode::DeleteConfirm { return_to, .. } => Some(return_to.clone()),
            _ => None,
        };
        self.cancel_pending_deletion_work();
        if let Some(return_to) = return_to {
            self.ui_mode = return_to.into_mode();
            self.mark_dirty();
        }
    }

    fn sync_deletion_work_summary(&mut self) {
        let summary = self.deletion_work.summary();
        if self.ui_effects.deletion_work != summary
            || self.ui_effects.deletion_in_progress != summary.mutating
        {
            self.ui_effects.deletion_work = summary;
            self.ui_effects.deletion_in_progress = summary.mutating;
            self.mark_dirty();
        }
    }

    pub fn push_confirmation_character(&mut self, character: char) {
        if character.is_control() {
            return;
        }
        if let UiMode::DeleteConfirm {
            challenge, input, ..
        } = &mut self.ui_mode
        {
            let maximum = challenge.expected_input().chars().count();
            if input.chars().count() < maximum {
                input.push(character);
                self.mark_dirty();
            }
        }
    }

    pub fn pop_confirmation_character(&mut self) {
        if let UiMode::DeleteConfirm { input, .. } = &mut self.ui_mode {
            input.pop();
            self.mark_dirty();
        }
    }

    /// Accepted confirmation returns immediately to the prior navigable map before
    /// background planning, revalidation, and mutation begin.
    pub fn take_confirmed_deletion_target(
        &mut self,
    ) -> Option<(DeletionWorkId, Box<FileToDelete>)> {
        if self.deletion_is_unavailable() {
            self.cancel_pending_deletion_work_and_dismiss_confirmation();
            return None;
        }
        let confirmed = matches!(
            &self.ui_mode,
            UiMode::DeleteConfirm {
                challenge, input, ..
            } if input == challenge.expected_input()
        );
        if !confirmed {
            return None;
        }
        let mode = std::mem::replace(&mut self.ui_mode, UiMode::Loading);
        let UiMode::DeleteConfirm {
            work_id,
            target,
            return_to,
            ..
        } = mode
        else {
            self.ui_mode = mode;
            return None;
        };
        self.ui_mode = return_to.into_mode();
        self.deletion_modal_work_id = None;
        self.mark_dirty();
        Some((work_id, target))
    }

    pub fn arm_and_confirm_deletion_target(
        &mut self,
    ) -> Option<(DeletionWorkId, Box<FileToDelete>)> {
        if let UiMode::DeleteConfirm {
            challenge, input, ..
        } = &mut self.ui_mode
            && matches!(
                challenge,
                ConfirmationChallenge::ConfirmFile | ConfirmationChallenge::ReducedGuard
            )
            && input.is_empty()
        {
            input.push('y');
        }
        self.take_confirmed_deletion_target()
    }

    #[must_use]
    pub fn confirmation_is_single_key(&self) -> bool {
        self.deletion_challenge().is_some_and(|(challenge, _)| {
            matches!(
                challenge,
                ConfirmationChallenge::ConfirmFile | ConfirmationChallenge::ReducedGuard
            )
        })
    }

    pub fn complete_deletion(&mut self, report: DeletionReport) -> bool {
        let deleted = report.deleted_entries() > 0;
        self.reconcile_generation_after_deletion(&report);
        self.ui_effects.record_deletion_result(&report);
        let report = Arc::new(report);
        // The history keeps room for every deletion allowed to start (see `request_deletion`),
        // so a report finds it; the bounds are what keep the history within its limits however
        // that came to be.
        if self.deletion_history.len() < MAX_RETAINED_DELETION_REPORTS
            && report.estimated_bytes <= self.deletion_history_room_bytes()
        {
            self.deletion_history_bytes = self
                .deletion_history_bytes
                .saturating_add(report.estimated_bytes);
            self.deletion_history.push(report);
        }
        // The streamed report may remove the current node. Board replacement keeps a
        // surviving selection; a vanished one re-arms to the largest entry only before
        // the reader's first move and otherwise clears, as an empty view does.
        self.render_and_update_board();
        deleted
    }

    #[must_use]
    pub fn deletion_challenge(&self) -> Option<(&ConfirmationChallenge, &str)> {
        let UiMode::DeleteConfirm {
            challenge, input, ..
        } = &self.ui_mode
        else {
            return None;
        };
        Some((challenge, input))
    }

    pub fn open_help(&mut self) {
        self.replace_ui_mode(UiMode::Help);
        self.mark_dirty();
    }

    pub fn scan_is_uncertain(&self, summary: &RunSummary) -> bool {
        if !self.scan_store_available
            || self.generation_rebuild_required
            || self.generation_rebuild_active
        {
            return true;
        }
        self.scan_store.published().is_none_or(|published| {
            canonical_scan_report_state(published, summary, false)
                == crate::report::ScanReportState::Uncertain
        })
    }

    pub fn write_scan_report(
        &mut self,
        summary: &RunSummary,
        writer: impl Write,
    ) -> Result<(), ReportError> {
        if !self.scan_store_available
            || self.generation_rebuild_required
            || self.generation_rebuild_active
        {
            return Err(ReportError::Invariant(
                "scan export is unavailable while a complete current map is unavailable"
                    .to_string(),
            ));
        }
        if self.overlay_publication_pending() {
            // The map on screen still lists what the last deletion removed, and a report of it
            // would too.
            return Err(ReportError::Invariant(
                "scan export is unavailable until the map reflects the last deletion".to_string(),
            ));
        }
        let root = self.scan_root.clone();
        let published = self.scan_store.published_mut().ok_or_else(|| {
            ReportError::Invariant(
                "no canonical scan generation is available for export".to_string(),
            )
        })?;
        let state = canonical_scan_report_state(published, summary, false);
        write_canonical_scan_report_json(
            &root,
            Some(&self.root_identity),
            published,
            summary,
            state,
            writer,
        )
    }

    /// A handle on each report of the deletion history, for the export thread to serialize
    /// without the owner loop: a few pointers, however long the history is.
    #[must_use]
    pub(crate) fn deletion_history_snapshot(&self) -> Vec<Arc<DeletionReport>> {
        self.deletion_history.clone()
    }

    /// Drops the first `exported` reports of the history: the ones an export just wrote. Reports
    /// that finished while it ran stay, for the next export.
    pub(crate) fn drop_exported_deletion_history(&mut self, exported: usize) {
        let exported = exported.min(self.deletion_history.len());
        let freed = self
            .deletion_history
            .drain(..exported)
            .map(|report| report.estimated_bytes)
            .fold(0_usize, usize::saturating_add);
        self.deletion_history_bytes = self.deletion_history_bytes.saturating_sub(freed);
    }

    pub fn show_notice(&mut self, message: impl Into<String>) {
        let return_to = self.modal_return_target();
        self.replace_ui_mode(UiMode::Notice {
            message: message.into(),
            return_to,
        });
        self.mark_dirty();
    }

    #[must_use]
    pub(crate) fn can_exit_immediately(&self) -> bool {
        !self.deletion_work.has_work() && !self.ui_effects.has_deletion_departure()
    }

    #[must_use]
    pub fn exit_work(&self) -> Option<&ExitWork> {
        match &self.ui_mode {
            UiMode::Exiting { work, .. } => Some(work),
            _ => None,
        }
    }

    pub fn open_theme_picker(&mut self, current: ThemeId) {
        let return_to = match &self.ui_mode {
            UiMode::Loading => ThemePickerReturn::Loading,
            UiMode::Normal => ThemePickerReturn::Normal,
            UiMode::Rebuilding { target } => ThemePickerReturn::Rebuilding {
                target: target.clone(),
            },
            UiMode::StaleSnapshot => ThemePickerReturn::StaleSnapshot,
            _ => return,
        };
        self.ui_mode = UiMode::ThemePicker {
            original: current,
            selected: current,
            return_to,
        };
        self.mark_dirty();
    }

    pub fn move_theme_picker(&mut self, previous: bool) -> Option<ThemeId> {
        let selected = match &mut self.ui_mode {
            UiMode::ThemePicker { selected, .. } => {
                *selected = if previous {
                    selected.previous_picker_item()
                } else {
                    selected.next_picker_item()
                };
                *selected
            }
            _ => return None,
        };
        self.mark_dirty();
        Some(selected)
    }

    pub fn commit_theme_picker(&mut self) -> Option<(ThemeId, ThemeId)> {
        let navigation = self.navigation_mode();
        let mode = std::mem::replace(&mut self.ui_mode, navigation);
        let UiMode::ThemePicker {
            original,
            selected,
            return_to,
        } = mode
        else {
            self.ui_mode = mode;
            return None;
        };
        self.ui_mode = return_to.into_mode();
        self.mark_dirty();
        Some((original, selected))
    }

    pub fn cancel_theme_picker(&mut self) -> Option<ThemeId> {
        let navigation = self.navigation_mode();
        let mode = std::mem::replace(&mut self.ui_mode, navigation);
        let UiMode::ThemePicker {
            original,
            return_to,
            ..
        } = mode
        else {
            self.ui_mode = mode;
            return None;
        };
        self.ui_mode = return_to.into_mode();
        self.mark_dirty();
        Some(original)
    }

    pub fn show_error(&mut self, message: impl Into<String>) {
        let return_to = self.modal_return_target();
        self.replace_ui_mode(UiMode::ErrorMessage {
            message: message.into(),
            return_to,
        });
        self.mark_dirty();
    }

    /// Tells the reader what a background job reported, now if they are not in the middle of a
    /// decision, else as soon as they are not ([`Self::show_waiting_announcement`]). A notice
    /// shown over a confirmation, the quit prompt, a filter being typed, or help would replace it,
    /// and the keys meant for it would land on the notice and the map instead.
    pub(crate) fn announce(&mut self, announcement: Announcement) {
        self.waiting_announcement = Some(announcement);
        self.show_waiting_announcement();
    }

    /// Shows the announcement that waits, if there is one and the reader can take it. Returns
    /// whether it showed one.
    pub(crate) fn show_waiting_announcement(&mut self) -> bool {
        if self.waiting_announcement.is_none() || !self.accepts_announcement() {
            return false;
        }
        match self.waiting_announcement.take() {
            Some(Announcement::Notice(message)) => self.show_notice(message),
            Some(Announcement::Error(message)) => self.show_error(message),
            None => return false,
        }
        true
    }

    /// Whether the reader is looking at the map, or at a notice that an announcement may replace,
    /// rather than deciding something that a new modal would interrupt.
    const fn accepts_announcement(&self) -> bool {
        matches!(
            self.ui_mode,
            UiMode::Loading
                | UiMode::Normal
                | UiMode::Rebuilding { .. }
                | UiMode::StaleSnapshot
                | UiMode::ScanResultsUnavailable(_)
                | UiMode::Notice { .. }
        )
    }

    pub fn normal_mode(&mut self) {
        self.replace_ui_mode(self.navigation_mode());
        self.render_and_update_board();
    }

    /// Starts publishing the replacement map a finished rebuild scan produced. Ends in exactly
    /// one [`FinishedPublication::Rebuild`] (or [`FinishedPublication::RebuildCancelled`]), at
    /// once when there is nothing to publish.
    pub(crate) fn begin_rebuild_publication(&mut self) {
        if self.generation_rebuild_invalidated {
            // A deletion discarded this generation while its worker was still draining: there is
            // nothing to publish.
            self.finished_publications
                .push_back(FinishedPublication::Rebuild { failure: None });
            return;
        }
        if self.generation_rebuild_restart_suppressed {
            // The reader pressed Esc while the scan's last events were still on their way here:
            // the replacement map is no longer wanted, so it is never built.
            self.finished_publications
                .push_back(FinishedPublication::RebuildCancelled);
            return;
        }
        if self.begin_publication(PendingPublication::Rebuild) {
            return;
        }
        let failure = self.scan_store_failure.clone().or_else(|| {
            Some("scan store was unavailable before the replacement map could publish".to_string())
        });
        self.finished_publications
            .push_back(FinishedPublication::Rebuild { failure });
    }

    /// Settles a finished rebuild once its publication ended. `publication_failure` is why the
    /// replacement map could not be published, if it could not.
    pub fn finish_generation_rebuild(
        &mut self,
        publication_failure: Option<String>,
    ) -> Result<(), AppError> {
        let target = self.generation_rebuild_target.take().ok_or_else(|| {
            AppError::Invariant("scan rebuild finished without an active revision".to_string())
        })?;
        let current = self.snapshot_page_cache.as_ref().map_or_else(
            || target.clone(),
            |cache| cache.current().current_relative().clone(),
        );
        let rebuild_invalidated = self.generation_rebuild_invalidated;
        self.generation_rebuild_invalidated = false;
        self.generation_rebuild_active = false;
        self.generation_rebuild_required = rebuild_invalidated;
        let mut watched_live_view = false;
        if rebuild_invalidated {
            // A deletion discarded this generation while its worker was still
            // draining. Its terminal event only releases the stale worker. The
            // owner starts a newer root generation next.
        } else {
            if let Some(failure) = publication_failure {
                self.pending_navigation = None;
                self.abandon_scan_store_generation_with_failure(failure);
                self.show_scan_results_unavailable(self.scan_results_unavailable_message());
                self.render_and_update_board();
                return Ok(());
            }
            self.generation_rebuild_restart_suppressed = false;
            // A rebuild with no map kept on screen was a scan for the reader to watch: its live
            // pages carried the cursor as a first scan's do, so its end settles the cursor as a
            // first scan's does. A rebuild behind the map the reader kept is a refresh of that
            // map, and the cursor follows its entry through it.
            watched_live_view = self.snapshot_page_is_provisional;
            self.snapshot_page_cache = None;
            self.snapshot_page_is_provisional = false;
            self.snapshot_page_history.clear();
            self.snapshot_filter = None;
            if self.load_snapshot_page(&current).is_err()
                && let Err(error) = self.load_snapshot_page(&target)
            {
                self.scan_store_available = false;
                self.scan_store_failure = Some(error.to_string());
                // The unavailable-results screen is drawn over a page, as every state is.
                self.reset_loading_snapshot();
                self.show_scan_results_unavailable(
                    "Excise could not open the completed folder map. Run it again.",
                );
                self.render_and_update_board();
                return Ok(());
            }
            self.provisional_page_waiting = None;
            self.resume_pending_navigation();
        }
        self.retarget_completed_scan_transient_modals();
        if matches!(self.ui_mode, UiMode::Rebuilding { .. }) {
            self.ui_mode = self.navigation_mode();
        } else if let UiMode::ThemePicker { return_to, .. } | UiMode::Exiting { return_to, .. } =
            &mut self.ui_mode
            && matches!(return_to, ThemePickerReturn::Rebuilding { .. })
        {
            *return_to = if self.loaded {
                ThemePickerReturn::Normal
            } else {
                ThemePickerReturn::Loading
            };
        }
        if let Some(suspended) = self.suspended_ui_mode.as_mut() {
            match suspended {
                UiMode::Rebuilding { .. } => {
                    *suspended = if self.loaded {
                        UiMode::Normal
                    } else {
                        UiMode::Loading
                    };
                }
                UiMode::ThemePicker { return_to, .. } | UiMode::Exiting { return_to, .. }
                    if matches!(return_to, ThemePickerReturn::Rebuilding { .. }) =>
                {
                    *return_to = if self.loaded {
                        ThemePickerReturn::Normal
                    } else {
                        ThemePickerReturn::Loading
                    };
                }
                _ => {}
            }
        }
        self.render_and_update_board();
        if watched_live_view {
            self.land_untouched_cursor_on_largest();
        }
        Ok(())
    }

    fn retain_snapshot_mode_after_rebuild_cancellation(&mut self) {
        let replace_mode = match &mut self.ui_mode {
            UiMode::ThemePicker { return_to, .. }
            | UiMode::Exiting { return_to, .. }
            | UiMode::ErrorMessage { return_to, .. }
            | UiMode::Notice { return_to, .. } => {
                *return_to = ThemePickerReturn::StaleSnapshot;
                false
            }
            UiMode::ScreenTooSmall => false,
            _ => true,
        };
        if replace_mode {
            self.ui_mode = self.navigation_mode();
        }
        if let Some(suspended) = self.suspended_ui_mode.as_mut() {
            match suspended {
                UiMode::Rebuilding { .. } => *suspended = UiMode::StaleSnapshot,
                UiMode::ThemePicker { return_to, .. }
                | UiMode::Exiting { return_to, .. }
                | UiMode::ErrorMessage { return_to, .. }
                | UiMode::Notice { return_to, .. } => {
                    *return_to = ThemePickerReturn::StaleSnapshot;
                }
                _ => {}
            }
        }
    }

    pub fn cancel_generation_rebuild(&mut self) -> Result<(), AppError> {
        if self.generation_rebuild_active {
            self.scan_store
                .cancel_active()
                .map_err(|error| AppError::Model(error.to_string()))?;
            self.generation_rebuild_active = false;
            self.generation_rebuild_invalidated = false;
            self.generation_rebuild_target = None;
            self.generation_rebuild_required = true;
            self.scan_store_available =
                self.scan_store_available && self.can_retain_published_snapshot();
            if !self.scan_store_available {
                self.snapshot_filter = None;
                self.snapshot_page_history.clear();
                self.reset_loading_snapshot();
            }
        }
        self.board.disarm_scan_reveal();
        if self.scan_store_available {
            self.retain_snapshot_mode_after_rebuild_cancellation();
        } else {
            self.show_scan_results_unavailable(self.scan_results_unavailable_message());
        }
        self.render_and_update_board();
        Ok(())
    }
    pub fn open_filter(&mut self) {
        let input = self
            .snapshot_filter
            .as_ref()
            .map_or_else(String::new, |(filter, _)| filter.raw().to_string());
        self.replace_ui_mode(UiMode::FilterInput { input, error: None });
        self.mark_dirty();
    }
    pub fn push_filter_character(&mut self, character: char) {
        if character.is_control() {
            return;
        }
        if let UiMode::FilterInput { input, error } = &mut self.ui_mode
            && input.chars().count() < 256
        {
            input.push(character);
            *error = None;
            self.mark_dirty();
        }
    }

    pub fn pop_filter_character(&mut self) {
        if let UiMode::FilterInput { input, error } = &mut self.ui_mode {
            input.pop();
            *error = None;
            self.mark_dirty();
        }
    }

    pub fn apply_filter(&mut self) {
        if !matches!(self.ui_mode, UiMode::FilterInput { .. }) {
            self.replace_ui_mode(UiMode::Normal);
            return;
        }
        let mode = std::mem::replace(&mut self.ui_mode, UiMode::Normal);
        let UiMode::FilterInput { input, .. } = mode else {
            unreachable!("filter mode must remain active after its guard")
        };
        let filter = if input.is_empty() {
            None
        } else {
            match FilterPattern::new(input.clone()) {
                Ok(filter) => Some(filter),
                Err(error) => {
                    self.replace_ui_mode(UiMode::FilterInput {
                        input,
                        error: Some(error.to_string()),
                    });
                    self.mark_dirty();
                    return;
                }
            }
        };
        let folder = self
            .snapshot_page_cache
            .as_ref()
            .expect("every navigable app state retains a snapshot page")
            .current()
            .current_relative()
            .clone();
        if self.uses_provisional_scan_page() {
            // A filter applies to the published map: the live view stays as it is.
            self.snapshot_filter = None;
            self.render_and_update_board();
            return;
        }
        // The cached pages belong to the old filter, so the load must not reuse them. They stay
        // aside until the filtered page has loaded: a page the store cannot serve leaves the
        // map, and the filter it was built with, as they were.
        let previous = (
            std::mem::replace(
                &mut self.snapshot_filter,
                filter.map(|filter| (filter, folder.clone())),
            ),
            self.snapshot_page_cache.take(),
            std::mem::take(&mut self.snapshot_page_history),
            self.snapshot_page_is_provisional,
        );
        if let Err(error) = self.load_snapshot_page(&folder) {
            (
                self.snapshot_filter,
                self.snapshot_page_cache,
                self.snapshot_page_history,
                self.snapshot_page_is_provisional,
            ) = previous;
            self.show_error(format!("Could not filter this scan page: {error}"));
            return;
        }
        self.board.reset_selected_index();
        self.render_and_update_board();
    }

    pub fn increment_failed_to_read(&mut self) {
        self.mark_dirty();
    }

    pub fn zoom_in(&mut self) {
        let files = self.files_in_current_view(self.board.zoom_level.saturating_add(1));
        self.board.zoom_in(files);
        self.mark_dirty();
    }

    pub fn zoom_out(&mut self) {
        let files = self.files_in_current_view(self.board.zoom_level.saturating_sub(1));
        self.board.zoom_out(files);
        self.mark_dirty();
    }

    pub fn reset_zoom(&mut self) {
        self.board.reset_zoom(self.files_in_current_view(0));
        self.mark_dirty();
    }
}

#[allow(clippy::needless_pass_by_value)]
fn model_error(error: ModelError) -> AppError {
    AppError::Model(error.to_string())
}

#[allow(clippy::needless_pass_by_value)]
fn scan_store_error(error: ScanStoreError) -> AppError {
    AppError::Model(error.to_string())
}

#[allow(clippy::needless_pass_by_value)]
fn store_unavailable_error(error: StoreUnavailable) -> AppError {
    AppError::Model(error.to_string())
}

fn relative_parent(path: &RelativePath) -> RelativePath {
    RelativePath::from_components(path.components()[..path.depth().saturating_sub(1)].to_vec())
        .expect("a prefix of a valid scan path remains valid")
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use ratatui::backend::{Backend, TestBackend};

    use crate::scan_coordinator::{
        ScanCoordinator, ScheduleOutcome, WorkKey, WorkKind, WorkPriority,
    };
    use crate::scan_store::session::scan_store_capacity_message;
    use crate::tests::fakes::TestBackend as ResizableTestBackend;

    #[cfg(unix)]
    use crate::deletion::{PlannedKind, PlannedSnapshot, ReviewedEntry, build_plan};
    #[cfg(unix)]
    use crate::native_path::identity_for;
    #[cfg(unix)]
    use crate::state::tiles::FileType;

    use super::*;

    fn map_entry(id: u32, percentage: f64) -> crate::state::tiles::FileMetadata {
        crate::state::tiles::FileMetadata {
            node_id: crate::model::NodeId(id),
            name: std::ffi::OsString::from(format!("entry-{id}")),
            size: 4096,
            apparent_size: 4096,
            descendants: None,
            percentage,
            file_type: crate::state::tiles::FileType::File,
            synthetic_kind: None,
            uncertain: false,
        }
    }

    fn add_fixture_entry(app: &mut App<TestBackend>, path: &std::path::Path) {
        let metadata = std::fs::symlink_metadata(path).expect("fixture metadata should exist");
        let identity = crate::native_path::identity_for(path, &metadata)
            .expect("fixture identity should be readable")
            .expect("fixture should not be a link");
        app.append_scan_store_entry_for_test(&metadata, path, &identity);
    }

    fn draw<B: Backend>(app: &mut App<B>, animation: &mut AnimationScheduler, now: u64) {
        app.mark_dirty();
        app.render_if_dirty(
            animation,
            Duration::from_millis(now),
            "test",
            Theme::for_id(crate::theme::ThemeId::ExciseDark),
            true,
            false,
            false,
        )
        .expect("render should succeed");
    }

    /// Asks for the live view of the folder on screen and applies the page the store thread
    /// answers with. An inline store answers as soon as it is asked.
    fn refresh_live_page(app: &mut App<TestBackend>, expectation: &str) {
        assert!(app.refresh_board_from_scan(), "{expectation}");
        app.process_scan_store_events();
    }

    /// Ends a rebuild scan the way the owner loop does: publishes the replacement map, applies
    /// what the store thread answers, and settles the rebuild with how the publication ended.
    fn finish_rebuild(app: &mut App<TestBackend>) -> Result<(), AppError> {
        app.begin_rebuild_publication();
        app.process_scan_store_events();
        let finished = app.take_finished_publication();
        let Some(FinishedPublication::Rebuild { failure }) = finished else {
            panic!("a rebuild should end in a rebuild completion, not {finished:?}");
        };
        app.finish_generation_rebuild(failure)
    }

    /// Publishes the active generation straight from the store and swaps the result in. The
    /// app's own bookkeeping for a finished scan does not run: the page on screen stays as it is.
    fn publish_store_directly(app: &mut App<TestBackend>) -> Result<(), ScanStoreError> {
        let publication = app.scan_store.with_inline_store(|store| {
            store.publish()?;
            store
                .take_publication()
                .ok_or(ScanStoreError::NoPublishedGeneration)
        })?;
        app.scan_store.install(publication);
        Ok(())
    }

    /// The names the page on screen lists, in a stable order.
    fn listed_names(app: &App<TestBackend>) -> Vec<std::ffi::OsString> {
        let mut names = app
            .files_in_current_view(0)
            .into_iter()
            .map(|file| file.name)
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    fn node_id_of(app: &App<TestBackend>, name: &str) -> NodeId {
        app.files_in_current_view(0)
            .into_iter()
            .find(|file| file.name == name)
            .expect("the page on screen should list the entry")
            .node_id
    }

    /// A file of `len` bytes in `dir`. The cursor tests size entries by length (`app_scanning`),
    /// so which entry is the largest never depends on how a file system rounds up what it
    /// allocates.
    fn file_of(dir: &Path, name: &str, len: usize) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, vec![b'x'; len]).expect("fixture file should be written");
        path
    }

    /// An app that has begun scanning `root` and sizes entries by length, with a map area of
    /// `width` by `height` cells: under 72 columns the map is a list.
    fn app_scanning(root: &Path, width: u16, height: u16, mouse_enabled: bool) -> App<TestBackend> {
        let mut app = App::new(
            TestBackend::new(width, height),
            root.to_path_buf(),
            true,
            false,
            crate::model::MIN_PROCESS_MIB,
            KeyPreset::Vim,
            None,
            mouse_enabled,
        )
        .expect("app should initialize");
        app.board
            .change_area(ratatui::layout::Rect::new(0, 0, width, height));
        app
    }

    /// The name of the entry the cursor is on.
    fn cursor_on(app: &App<TestBackend>) -> Option<String> {
        app.board
            .currently_selected()
            .map(|tile| tile.name.to_string_lossy().into_owned())
    }

    /// A lease to enumerate the root in `generation` of the app's own session.
    fn root_enumeration_lease(app: &App<TestBackend>, generation: ScanGeneration) -> WorkLease {
        let session = app.scan_session_id();
        let mut coordinator = ScanCoordinator::new(session, generation);
        let key = WorkKey::new(
            session,
            generation,
            WorkKind::EnumerateDirectory,
            RelativePath::root(),
        );
        assert_eq!(
            coordinator.schedule(key, WorkPriority::Background),
            ScheduleOutcome::Enqueued
        );
        coordinator
            .lease_next()
            .expect("scheduled test work should receive a lease")
    }

    #[test]
    fn loading_starts_with_a_root_only_snapshot_page() {
        let root = tempfile::tempdir().expect("app root should exist");
        let mut app = App::new(
            TestBackend::new(80, 24),
            root.path().to_path_buf(),
            false,
            false,
            crate::model::MIN_PROCESS_MIB,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");

        assert_eq!(app.current_folder_path(), root.path());
        assert_eq!(
            app.visible_tree().current_node().state,
            crate::model::NodeState::Scanning
        );
        assert!(app.files_in_current_view(0).is_empty());
        let mut animation = AnimationScheduler::new(false, false, Duration::ZERO);
        draw(&mut app, &mut animation, 0);
    }

    #[test]
    fn active_scan_refreshes_a_bounded_canonical_page_before_completion() {
        let root = tempfile::tempdir().expect("app root should exist");
        let first = root.path().join("first");
        let second = root.path().join("second");
        std::fs::write(&first, b"first").expect("first fixture should exist");
        std::fs::write(&second, b"second").expect("second fixture should exist");
        let mut app = App::new(
            TestBackend::new(80, 24),
            root.path().to_path_buf(),
            false,
            false,
            crate::model::MIN_PROCESS_MIB,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        app.board
            .change_area(ratatui::layout::Rect::new(0, 0, 80, 24));
        add_fixture_entry(&mut app, &first);

        refresh_live_page(&mut app, "active scan page should refresh");
        assert!(matches!(app.ui_mode, UiMode::Loading));
        assert_eq!(
            app.files_in_current_view(0)
                .into_iter()
                .map(|file| file.name)
                .collect::<Vec<_>>(),
            vec![std::ffi::OsString::from("first")]
        );

        add_fixture_entry(&mut app, &second);
        refresh_live_page(
            &mut app,
            "later canonical facts should refresh the active page",
        );
        let mut names = app
            .files_in_current_view(0)
            .into_iter()
            .map(|file| file.name)
            .collect::<Vec<_>>();
        names.sort();
        assert_eq!(
            names,
            vec![
                std::ffi::OsString::from("first"),
                std::ffi::OsString::from("second"),
            ]
        );

        app.finalize_scan();
        assert_eq!(
            app.visible_tree().current_node().state,
            crate::model::NodeState::Complete
        );
        app.start_ui();
        assert!(matches!(app.ui_mode, UiMode::Normal));
    }

    #[test]
    fn active_scan_drills_through_provisional_pages() {
        let root = tempfile::tempdir().expect("app root should exist");
        let folder = root.path().join("folder");
        let leaf = folder.join("leaf");
        let sibling = root.path().join("sibling");
        std::fs::create_dir(&folder).expect("folder fixture should exist");
        std::fs::write(&leaf, b"leaf").expect("leaf fixture should exist");
        std::fs::write(&sibling, b"sibling").expect("sibling fixture should exist");
        let mut app = App::new(
            TestBackend::new(120, 32),
            root.path().to_path_buf(),
            false,
            false,
            crate::model::MIN_PROCESS_MIB,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        app.board
            .change_area(ratatui::layout::Rect::new(0, 0, 120, 32));
        for path in [folder.as_path(), leaf.as_path(), sibling.as_path()] {
            add_fixture_entry(&mut app, path);
        }
        refresh_live_page(&mut app, "root provisional page should refresh");
        let folder_id = app
            .files_in_current_view(0)
            .into_iter()
            .find(|file| file.name == "folder")
            .expect("provisional root page should expose the folder")
            .node_id;
        let folder_node = app
            .visible_tree()
            .node(folder_id)
            .expect("provisional folder should remain materialized");
        assert_eq!(folder_node.state, crate::model::NodeState::Scanning);
        assert!(folder_node.unscanned_reason.is_none());
        assert!(app.board.select_node(folder_id));

        app.enter_selected();
        app.process_scan_store_events();

        assert_eq!(app.current_folder_path(), folder);
        assert_eq!(
            app.files_in_current_view(0)
                .into_iter()
                .map(|file| file.name)
                .collect::<Vec<_>>(),
            vec![std::ffi::OsString::from("leaf")]
        );
        assert!(app.go_up());
        app.process_scan_store_events();
        assert_eq!(app.current_folder_path(), root.path());
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn active_scan_allows_concrete_deletion_previews_only() {
        let root = tempfile::tempdir().expect("app root should exist");
        let concrete = root.path().join("concrete");
        std::fs::write(&concrete, b"payload").expect("concrete fixture should exist");
        let mut app = App::new(
            TestBackend::new(80, 24),
            root.path().to_path_buf(),
            false,
            false,
            crate::model::MIN_PROCESS_MIB,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        app.board
            .change_area(ratatui::layout::Rect::new(0, 0, 80, 24));
        add_fixture_entry(&mut app, &concrete);
        refresh_live_page(&mut app, "concrete preview should refresh");
        let concrete_id = app
            .files_in_current_view(0)
            .into_iter()
            .find(|file| file.name == "concrete")
            .expect("concrete preview should be shown")
            .node_id;
        assert!(app.board.select_node(concrete_id));
        assert_eq!(
            app.request_deletion()
                .expect("concrete preview should be deletable")
                .full_path(),
            concrete
        );

        app.scan_store
            .with_inline_store(|store| {
                store.append_observation_batch(
                    vec![PathObservation::new(
                        RelativePath::from_path(Path::new("inferred/leaf"))
                            .expect("fixture path should be relative"),
                        PathEntryKind::File,
                        SummaryMetrics::leaf(
                            8 * 1024 * 1024,
                            ByteBounds::exact(8 * 1024 * 1024),
                            ByteBounds::exact(8 * 1024 * 1024),
                        ),
                        Coverage::Complete,
                    )],
                    Vec::new(),
                )
            })
            .expect("inferred fixture should append");
        refresh_live_page(&mut app, "inferred preview should refresh");
        let inferred_id = app
            .files_in_current_view(0)
            .into_iter()
            .find(|file| file.name == "inferred")
            .expect("inferred preview should be shown")
            .node_id;
        assert!(app.board.select_node(inferred_id));
        assert!(app.request_deletion().is_none());
        assert!(matches!(
            &app.ui_mode,
            UiMode::Notice { message, .. }
                if message == "Wait for this item to receive a verified scan preview before deleting"
        ));
    }

    #[test]
    fn primary_map_is_unpublished_until_the_store_thread_reports_it() {
        let root = tempfile::tempdir().expect("app root should exist");
        let entry = root.path().join("entry");
        std::fs::write(&entry, b"payload").expect("fixture entry should exist");
        let mut app = App::new(
            TestBackend::new(80, 24),
            root.path().to_path_buf(),
            false,
            false,
            128,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        add_fixture_entry(&mut app, &entry);

        app.begin_primary_publication();

        assert!(app.primary_publication_pending());
        assert!(app.scan_store.published().is_none());
        assert!(app.scan_store_busy());
        assert_eq!(app.take_finished_publication(), None);

        assert!(app.process_scan_store_events());

        assert!(!app.primary_publication_pending());
        assert!(app.scan_store.published().is_some());
        assert!(
            app.scan_store_busy(),
            "the loop keeps polling until it has taken the publication that ended"
        );
        assert_eq!(
            app.take_finished_publication(),
            Some(FinishedPublication::Primary)
        );
        assert_eq!(app.take_finished_publication(), None);
        assert!(!app.scan_store_busy());
    }

    #[test]
    fn the_finishing_map_shows_a_clock_that_moves_while_a_stage_runs() {
        let root = tempfile::tempdir().expect("app root should exist");
        let entry = root.path().join("entry");
        std::fs::write(&entry, b"payload").expect("fixture entry should exist");
        let mut app = App::new(
            TestBackend::new(80, 24),
            root.path().to_path_buf(),
            false,
            false,
            128,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        add_fixture_entry(&mut app, &entry);
        app.begin_primary_publication();

        assert!(
            !app.refresh_publication_progress(),
            "a publication that has only just begun shows nothing"
        );
        assert_eq!(app.ui_effects.publication_progress, None);

        // It has now run for 0.35 s, and the stage under way has not ended.
        app.publication_progress_from = Instant::now().checked_sub(Duration::from_millis(250));
        assert!(app.refresh_publication_progress());
        let first = app
            .ui_effects
            .publication_progress
            .expect("a publication past the delay shows its progress");
        assert!(first.elapsed_tenths >= 3, "{first:?}");

        // Half a second on it is still the same stage, and the display moves all the same: a
        // stage can outlast the longest the screen may stand still.
        app.publication_progress_from = Instant::now().checked_sub(Duration::from_millis(750));
        assert!(app.refresh_publication_progress());
        let later = app
            .ui_effects
            .publication_progress
            .expect("the publication is still running");
        assert_eq!((later.done, later.total), (first.done, first.total));
        assert!(later.elapsed_tenths >= 8, "{later:?}");

        // Its end takes the progress off the screen.
        assert!(app.process_scan_store_events());
        assert!(app.refresh_publication_progress());
        assert_eq!(app.ui_effects.publication_progress, None);
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn deletion_is_refused_until_the_primary_map_is_published() {
        let root = tempfile::tempdir().expect("app root should exist");
        let concrete = root.path().join("concrete");
        std::fs::write(&concrete, b"payload").expect("concrete fixture should exist");
        let mut app = App::new(
            TestBackend::new(80, 24),
            root.path().to_path_buf(),
            false,
            false,
            128,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        app.board
            .change_area(ratatui::layout::Rect::new(0, 0, 80, 24));
        add_fixture_entry(&mut app, &concrete);
        refresh_live_page(&mut app, "concrete preview should refresh");
        let concrete_id = node_id_of(&app, "concrete");
        assert!(app.board.select_node(concrete_id));
        assert!(
            app.request_deletion().is_some(),
            "a concrete live preview is deletable while the scan runs"
        );

        app.begin_primary_publication();
        assert!(
            app.request_deletion().is_none(),
            "nothing is deletable while the finished scan's map is being published"
        );

        app.process_scan_store_events();
        app.start_ui();
        let concrete_id = node_id_of(&app, "concrete");
        assert!(app.board.select_node(concrete_id));
        assert!(
            app.request_deletion().is_some(),
            "the published map allows deletion again"
        );
    }

    #[test]
    fn a_drill_during_the_primary_publication_waits_for_the_published_map() {
        let root = tempfile::tempdir().expect("app root should exist");
        let folder = root.path().join("folder");
        let leaf = folder.join("leaf");
        let sibling = root.path().join("sibling");
        std::fs::create_dir(&folder).expect("folder fixture should exist");
        std::fs::write(&leaf, b"leaf").expect("leaf fixture should exist");
        std::fs::write(&sibling, b"sibling").expect("sibling fixture should exist");
        let mut app = App::new(
            TestBackend::new(120, 32),
            root.path().to_path_buf(),
            false,
            false,
            crate::model::MIN_PROCESS_MIB,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        app.board
            .change_area(ratatui::layout::Rect::new(0, 0, 120, 32));
        for path in [folder.as_path(), leaf.as_path(), sibling.as_path()] {
            add_fixture_entry(&mut app, path);
        }
        refresh_live_page(&mut app, "root provisional page should refresh");
        let folder_id = node_id_of(&app, "folder");
        assert!(app.board.select_node(folder_id));
        app.begin_primary_publication();

        app.enter_selected();

        // The folder's page belongs to the map being published, so the drill waits for it.
        assert_eq!(app.current_folder_path(), root.path());

        app.process_scan_store_events();

        assert_eq!(app.current_folder_path(), folder);
        assert_eq!(listed_names(&app), ["leaf"]);
        assert_eq!(
            app.take_finished_publication(),
            Some(FinishedPublication::Primary)
        );
    }

    /// The scan lists a small entry first and larger ones on later pages. The map selects the
    /// first entry it is shown and, with the reader never having moved or clicked, keeps the
    /// cursor there while the scan runs; the scan's end puts it on the largest entry. The test
    /// decides what each page holds, so nothing depends on the order the file system lists a
    /// folder in.
    #[test]
    fn an_untouched_cursor_holds_through_the_scan_and_lands_on_the_largest_entry_at_completion() {
        let root = tempfile::tempdir().expect("app root should exist");
        let small = file_of(root.path(), "small", 4 * 1024);
        let medium = file_of(root.path(), "medium", 16 * 1024);
        let large = file_of(root.path(), "large", 256 * 1024);
        let mut app = app_scanning(root.path(), 80, 24, false);

        add_fixture_entry(&mut app, &small);
        refresh_live_page(&mut app, "the first page should refresh");
        assert_eq!(cursor_on(&app).as_deref(), Some("small"));

        add_fixture_entry(&mut app, &medium);
        refresh_live_page(&mut app, "the second page should refresh");
        assert_eq!(
            cursor_on(&app).as_deref(),
            Some("small"),
            "a larger entry on a later page does not take the cursor while the scan runs"
        );
        add_fixture_entry(&mut app, &large);
        refresh_live_page(&mut app, "the third page should refresh");
        assert_eq!(listed_names(&app), ["large", "medium", "small"]);
        assert_eq!(cursor_on(&app).as_deref(), Some("small"));

        app.finalize_scan();
        app.start_ui();

        assert_eq!(cursor_on(&app).as_deref(), Some("large"));

        // Only the scan's end retargets it: a later layout carries the cursor by name instead.
        let small_id = node_id_of(&app, "small");
        assert!(app.board.select_node(small_id));
        app.board
            .change_area(ratatui::layout::Rect::new(0, 0, 100, 30));
        assert_eq!(cursor_on(&app).as_deref(), Some("small"));
    }

    /// A cursor the reader moved is their choice: the scan's end leaves it on its entry, however
    /// large the entries that arrived since.
    #[test]
    fn a_cursor_the_reader_moved_stays_on_its_entry_through_completion() {
        let root = tempfile::tempdir().expect("app root should exist");
        let small = file_of(root.path(), "small", 4 * 1024);
        let medium = file_of(root.path(), "medium", 16 * 1024);
        let large = file_of(root.path(), "large", 256 * 1024);
        let mut app = app_scanning(root.path(), 80, 24, false);
        add_fixture_entry(&mut app, &medium);
        add_fixture_entry(&mut app, &small);
        refresh_live_page(&mut app, "the first page should refresh");
        assert_eq!(cursor_on(&app).as_deref(), Some("medium"));

        // Which key reaches the other entry depends on how the map tiled the two, so the keys
        // are tried in turn.
        let keys: [fn(&mut App<TestBackend>); 4] = [
            App::move_selected_left,
            App::move_selected_right,
            App::move_selected_up,
            App::move_selected_down,
        ];
        for key in keys {
            if cursor_on(&app).as_deref() == Some("small") {
                break;
            }
            key(&mut app);
        }
        assert_eq!(
            cursor_on(&app).as_deref(),
            Some("small"),
            "a movement key should reach the other entry"
        );

        add_fixture_entry(&mut app, &large);
        refresh_live_page(&mut app, "the second page should refresh");
        app.finalize_scan();
        app.start_ui();

        assert_eq!(cursor_on(&app).as_deref(), Some("small"));
    }

    /// A click places the cursor as deliberately as a key does.
    #[test]
    fn a_cursor_the_reader_clicked_stays_on_its_entry_through_completion() {
        let root = tempfile::tempdir().expect("app root should exist");
        let small = file_of(root.path(), "small", 4 * 1024);
        let medium = file_of(root.path(), "medium", 16 * 1024);
        let large = file_of(root.path(), "large", 256 * 1024);
        let mut app = app_scanning(root.path(), 80, 24, true);
        add_fixture_entry(&mut app, &medium);
        add_fixture_entry(&mut app, &small);
        refresh_live_page(&mut app, "the first page should refresh");
        assert_eq!(cursor_on(&app).as_deref(), Some("medium"));

        let tile = app
            .board
            .tiles
            .iter()
            .find(|tile| tile.name == "small")
            .cloned()
            .expect("the page should tile the small entry");
        let column = tile.x.saturating_add(1);
        let row = u16::try_from(tile.top_row()).expect("the map fits in a terminal row");
        assert!(app.select_at(column, row), "the click lands on the entry");
        assert_eq!(cursor_on(&app).as_deref(), Some("small"));

        add_fixture_entry(&mut app, &large);
        refresh_live_page(&mut app, "the second page should refresh");
        app.finalize_scan();
        app.start_ui();

        assert_eq!(cursor_on(&app).as_deref(), Some("small"));
    }

    /// The cursor that goes to the largest entry is the one of the folder the reader is in when
    /// the scan ends. Opening a folder is not a move or a click, so it leaves the cursor
    /// automatic, and the folder's own first page listed a small file only.
    #[test]
    fn the_cursor_of_a_folder_opened_during_the_scan_lands_on_its_largest_entry_at_completion() {
        let root = tempfile::tempdir().expect("app root should exist");
        let folder = root.path().join("folder");
        std::fs::create_dir(&folder).expect("folder fixture should exist");
        let small_leaf = file_of(&folder, "small-leaf", 4 * 1024);
        let large_leaf = file_of(&folder, "large-leaf", 256 * 1024);
        let sibling = file_of(root.path(), "sibling", 128 * 1024);
        let mut app = app_scanning(root.path(), 120, 32, false);
        for path in [&folder, &small_leaf, &sibling] {
            add_fixture_entry(&mut app, path);
        }
        refresh_live_page(&mut app, "the root page should refresh");
        assert_eq!(cursor_on(&app).as_deref(), Some("sibling"));
        let folder_id = node_id_of(&app, "folder");
        assert!(app.board.select_node(folder_id));

        app.enter_selected();
        app.process_scan_store_events();
        app.board.settle_geometry();
        assert_eq!(app.current_folder_path(), folder);
        assert_eq!(cursor_on(&app).as_deref(), Some("small-leaf"));

        add_fixture_entry(&mut app, &large_leaf);
        refresh_live_page(&mut app, "the folder's page should refresh");
        assert_eq!(listed_names(&app), ["large-leaf", "small-leaf"]);
        assert_eq!(
            cursor_on(&app).as_deref(),
            Some("small-leaf"),
            "the opened folder's cursor holds too while the scan runs"
        );

        app.finalize_scan();
        app.start_ui();

        assert_eq!(app.current_folder_path(), folder);
        assert_eq!(cursor_on(&app).as_deref(), Some("large-leaf"));
    }

    /// A narrow terminal lists the entries instead of tiling them, and the list scrolls to keep
    /// the cursor in view: the early entry the cursor stayed on has scrolled the list to its
    /// bottom, and at completion the cursor, and the list with it, go back to the largest entry
    /// at the top rather than to whichever row the window starts on.
    #[test]
    fn in_a_narrow_list_the_cursor_returns_to_the_largest_entry_at_the_top() {
        let root = tempfile::tempdir().expect("app root should exist");
        let mut app = app_scanning(root.path(), 60, 3, false);
        let small = file_of(root.path(), "small", 4 * 1024);
        add_fixture_entry(&mut app, &small);
        refresh_live_page(&mut app, "the first page should refresh");
        for (name, kib) in [("e", 8), ("d", 16), ("c", 32), ("b", 64), ("a", 128)] {
            let path = file_of(root.path(), name, kib * 1024);
            add_fixture_entry(&mut app, &path);
            refresh_live_page(&mut app, "a later page should refresh");
        }
        assert!(app.board.is_list_layout());
        assert_eq!(cursor_on(&app).as_deref(), Some("small"));
        assert_eq!(
            app.board.tiles.first().map(|tile| tile.name.clone()),
            Some(std::ffi::OsString::from("d")),
            "the list scrolled to keep the cursor in view"
        );

        app.finalize_scan();
        app.start_ui();

        assert_eq!(cursor_on(&app).as_deref(), Some("a"));
        assert_eq!(app.board.selected_index, Some(0));
    }

    /// A rebuild behind the map the reader kept is a refresh of it, not a scan they watched fill
    /// in: the cursor follows its entry through the new map instead of going to the largest.
    #[test]
    fn a_rebuild_behind_the_map_the_reader_kept_leaves_the_cursor_on_its_entry() {
        let root = tempfile::tempdir().expect("app root should exist");
        let small = file_of(root.path(), "small", 4 * 1024);
        let large = file_of(root.path(), "large", 256 * 1024);
        let mut app = app_scanning(root.path(), 80, 24, false);
        add_fixture_entry(&mut app, &large);
        add_fixture_entry(&mut app, &small);
        app.finalize_scan();
        app.start_ui();
        assert_eq!(cursor_on(&app).as_deref(), Some("large"));

        // The cursor is on the smaller entry without the reader having moved or clicked, as it
        // is on a folder they have just come out of.
        let small_id = node_id_of(&app, "small");
        assert!(app.board.select_node(small_id));
        app.require_generation_rebuild_for_test();
        assert!(
            app.begin_generation_rebuild()
                .expect("the rebuild should start behind the map the reader kept")
        );
        add_fixture_entry(&mut app, &large);
        add_fixture_entry(&mut app, &small);
        finish_rebuild(&mut app).expect("the replacement map should publish");

        assert_eq!(cursor_on(&app).as_deref(), Some("small"));
    }

    /// A rebuild with no map to keep on screen, because the scan it replaces never published
    /// one, is a scan the reader watches fill in, and its end settles the cursor as a first
    /// scan's does.
    #[test]
    fn a_rebuild_the_reader_watched_fill_in_ends_with_the_cursor_on_its_largest_entry() {
        let root = tempfile::tempdir().expect("app root should exist");
        let small = file_of(root.path(), "small", 4 * 1024);
        let large = file_of(root.path(), "large", 256 * 1024);
        let mut app = app_scanning(root.path(), 80, 24, false);
        add_fixture_entry(&mut app, &small);
        refresh_live_page(&mut app, "the first scan's page should refresh");

        // A deletion the live map cannot describe ends while the first scan runs: that map is
        // given up, and the scan's end leads to a rebuild with nothing published to keep.
        app.invalidate_snapshot_view_for_live_mutation();
        app.begin_primary_publication();
        assert_eq!(
            app.take_finished_publication(),
            Some(FinishedPublication::Primary)
        );
        app.start_ui();
        assert!(
            app.begin_generation_rebuild()
                .expect("the rebuild should start")
        );

        add_fixture_entry(&mut app, &small);
        refresh_live_page(&mut app, "the rebuild's first page should refresh");
        assert_eq!(cursor_on(&app).as_deref(), Some("small"));
        add_fixture_entry(&mut app, &large);
        refresh_live_page(&mut app, "the rebuild's second page should refresh");
        assert_eq!(
            cursor_on(&app).as_deref(),
            Some("small"),
            "the rebuild's cursor holds while it runs"
        );

        finish_rebuild(&mut app).expect("the replacement map should publish");

        assert_eq!(cursor_on(&app).as_deref(), Some("large"));
    }

    #[test]
    fn unrecorded_metadata_path_keeps_paged_scan_available() {
        let root = tempfile::tempdir().expect("app root should exist");
        let visible = root.path().join("visible");
        std::fs::write(&visible, b"payload").expect("visible fixture should exist");
        let mut app = App::new(
            TestBackend::new(80, 24),
            root.path().to_path_buf(),
            false,
            false,
            crate::model::MIN_PROCESS_MIB,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        add_fixture_entry(&mut app, &visible);
        app.record_scan_store_unrecorded_path();
        app.finalize_scan();

        assert_eq!(
            app.visible_tree().current_node().state,
            crate::model::NodeState::Complete
        );
        assert_eq!(app.visible_tree().unreadable_path_count(), 1);
        assert_eq!(
            app.visible_tree()
                .total_node()
                .metrics
                .allocated_bytes
                .upper,
            None
        );
        assert_eq!(
            app.files_in_current_view(0)
                .into_iter()
                .map(|file| file.name)
                .collect::<Vec<_>>(),
            vec![std::ffi::OsString::from("visible")]
        );
    }

    #[test]
    fn unreadable_directory_marks_itself_and_every_ancestor_uncertain() {
        let root = tempfile::tempdir().expect("app root should exist");
        let parent = root.path().join("parent");
        std::fs::create_dir(&parent).expect("parent dir should exist");
        let locked = parent.join("locked");
        std::fs::create_dir(&locked).expect("locked dir should exist");
        let mut app = App::new(
            TestBackend::new(80, 24),
            root.path().to_path_buf(),
            false,
            false,
            crate::model::MIN_PROCESS_MIB,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        add_fixture_entry(&mut app, &parent);
        add_fixture_entry(&mut app, &locked);
        app.record_unreadable_directory(&locked);
        app.finalize_scan();

        assert_eq!(
            app.visible_tree().current_node().state,
            crate::model::NodeState::Uncertain,
            "the root is an ancestor of the unreadable folder"
        );

        app.load_snapshot_page(
            &RelativePath::from_path(Path::new("parent"))
                .expect("fixture folder should be canonical"),
        )
        .expect("parent page should materialize");
        assert_eq!(
            app.visible_tree().current_node().state,
            crate::model::NodeState::Uncertain,
            "parent is also an ancestor of the unreadable folder"
        );
        let locked_entry = app
            .files_in_current_view(0)
            .into_iter()
            .find(|file| file.name == "locked")
            .expect("the unreadable folder itself should be listed");
        assert!(
            locked_entry.uncertain,
            "the tile for the unreadable folder itself must show uncertain"
        );
    }

    #[test]
    fn modal_and_selected_map_request_fast_animation_frames() {
        let root = tempfile::tempdir().expect("app root should exist");
        let mut app = App::new(
            TestBackend::new(80, 24),
            root.path().to_path_buf(),
            false,
            false,
            crate::model::MIN_PROCESS_MIB,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        app.loaded = true;
        app.ui_mode = UiMode::Normal;
        app.board.change_files(vec![map_entry(1, 1.0)]);
        let mut animation = AnimationScheduler::new(false, false, Duration::ZERO);

        app.render_if_dirty(
            &mut animation,
            Duration::ZERO,
            "test",
            Theme::for_id(crate::theme::ThemeId::CatppuccinMocha),
            false,
            false,
            false,
        )
        .expect("normal map should render");
        assert!(app.board.currently_selected().is_some());
        assert_eq!(
            animation.next_frame_at(),
            Some(crate::animation::ACTIVE_FRAME_INTERVAL)
        );

        app.ui_mode = UiMode::Help;
        app.mark_dirty();
        app.render_if_dirty(
            &mut animation,
            Duration::from_millis(1),
            "test",
            Theme::for_id(crate::theme::ThemeId::CatppuccinMocha),
            false,
            false,
            false,
        )
        .expect("modal should render");
        assert_eq!(
            animation.next_frame_at(),
            Some(Duration::from_millis(1) + crate::animation::ACTIVE_FRAME_INTERVAL)
        );
    }

    #[test]
    fn initial_scan_completion_keeps_the_theme_picker_and_populates_the_map() {
        let root = tempfile::tempdir().expect("app root should exist");
        let entry = root.path().join("entry");
        std::fs::write(&entry, b"payload").expect("fixture entry should be created");
        let mut app = App::new(
            TestBackend::new(80, 24),
            root.path().to_path_buf(),
            false,
            false,
            128,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        app.board
            .change_area(ratatui::layout::Rect::new(0, 0, 80, 24));
        add_fixture_entry(&mut app, &entry);
        app.finalize_scan();
        app.open_theme_picker(ThemeId::ExciseDark);

        app.start_ui();

        assert!(matches!(
            app.ui_mode,
            UiMode::ThemePicker {
                return_to: ThemePickerReturn::Normal,
                ..
            }
        ));
        assert!(app.board.currently_selected().is_some());
    }

    #[test]
    fn single_link_entries_skip_identity_runs_and_keep_physical_metrics() {
        let root = tempfile::tempdir().expect("app root should exist");
        let entry = root.path().join("entry");
        std::fs::write(&entry, b"payload").expect("fixture entry should be created");
        let metadata = std::fs::symlink_metadata(&entry).expect("fixture metadata should load");
        let mut identity = crate::native_path::identity_for(&entry, &metadata)
            .expect("fixture identity should be readable")
            .expect("fixture should not be a link");
        identity.link_count = Some(1);
        let expected_physical = ByteBounds::exact(u128::from(
            crate::os::physical_size(&entry, &metadata)
                .expect("fixture physical size should be readable"),
        ));
        let mut app = App::new(
            TestBackend::new(80, 24),
            root.path().to_path_buf(),
            false,
            false,
            128,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");

        app.append_scan_store_entry_for_test(&metadata, &entry, &identity);

        assert_eq!(
            app.scan_store
                .with_inline_store(|store| store.active_run_counts()),
            Some((1, 0))
        );
        publish_store_directly(&mut app)
            .expect("single-link path should publish without an identity run");
        let page = app
            .scan_store
            .published()
            .expect("published page should exist")
            .page(PageRequest::first(RelativePath::root(), 1))
            .expect("published page should load");
        assert_eq!(page.entries[0].metrics.allocated_bytes, expected_physical);
        assert_eq!(page.entries[0].metrics.reclaimable_bytes, expected_physical);
        assert_eq!(page.root_metrics.allocated_bytes, expected_physical);
        assert_eq!(page.root_metrics.reclaimable_bytes, expected_physical);
    }

    #[test]
    fn canonical_storage_failure_names_scratch_remedy() {
        let root = tempfile::tempdir().expect("app root should exist");
        let mut app = App::new(
            TestBackend::new(80, 24),
            root.path().to_path_buf(),
            false,
            false,
            128,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");

        app.abandon_scan_store_generation_with_failure(
            "scan store capacity exhausted while publishing",
        );
        app.finalize_scan();
        app.start_ui();

        assert!(matches!(
            &app.ui_mode,
            UiMode::ScanResultsUnavailable(message)
                if message.contains("Not enough scratch space")
                    && message.contains("deletion controls")
                    && message.contains("--scan-store-dir")
                    && message.contains("--scan-store-reserve-mib")
        ));
    }

    #[test]
    fn out_of_space_io_error_gets_the_scratch_space_remedy() {
        let root = tempfile::tempdir().expect("app root should exist");
        let mut app = App::new(
            TestBackend::new(80, 24),
            root.path().to_path_buf(),
            false,
            false,
            128,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");

        // A raw out-of-space error from the volume itself, not the session's own tracked
        // quota: OS-native wording, no mention of "capacity exhausted" or `--scan-store-mib`.
        // This is exactly the typed error `admit_scan_input_runs` sees from a failed
        // admission; its production call site classifies it through
        // `scan_store_capacity_message` before the typed error becomes a string.
        let out_of_space = ScanStoreError::Run(crate::scan_store::run_file::RunError::Io(
            std::io::Error::new(
                std::io::ErrorKind::StorageFull,
                "No space left on device (os error 28)",
            ),
        ));
        app.abandon_scan_store_generation_with_failure(scan_store_capacity_message(&out_of_space));
        app.finalize_scan();
        app.start_ui();

        assert!(matches!(
            &app.ui_mode,
            UiMode::ScanResultsUnavailable(message)
                if message.contains("Not enough scratch space")
                    && message.contains("--scan-store-dir")
                    && message.contains("--scan-store-reserve-mib")
        ));
    }

    #[test]
    fn failed_canonical_publication_keeps_the_compacted_map_unavailable() {
        let root = tempfile::tempdir().expect("app root should exist");
        let entry = root.path().join("entry");
        std::fs::write(&entry, b"payload").expect("fixture entry should be created");
        let mut app = App::new(
            TestBackend::new(80, 24),
            root.path().to_path_buf(),
            false,
            false,
            128,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        add_fixture_entry(&mut app, &entry);

        app.abandon_scan_store_generation();
        app.finalize_scan();
        app.start_ui();

        assert!(matches!(
            &app.ui_mode,
            UiMode::ScanResultsUnavailable(message) if message.contains("complete folder map")
        ));
        let command = crate::input::handle_keypress(
            &crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Esc,
                crossterm::event::KeyModifiers::NONE,
            )),
            &mut app,
        );
        assert!(matches!(command, crate::input::InputCommand::None));
        assert!(matches!(app.ui_mode, UiMode::ScanResultsUnavailable(_)));
        let command = crate::input::handle_keypress(
            &crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('q'),
                crossterm::event::KeyModifiers::NONE,
            )),
            &mut app,
        );
        assert!(matches!(command, crate::input::InputCommand::None));
        assert!(!app.is_running);
    }

    #[test]
    fn a_batch_the_store_rejects_makes_the_scan_store_unavailable() {
        let root = tempfile::tempdir().expect("app root should exist");
        let mut app = App::new(
            TestBackend::new(80, 24),
            root.path().to_path_buf(),
            false,
            false,
            128,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        let factory = app
            .scan_input_run_factory()
            .expect("the first generation should accept worker runs");
        let batch = factory
            .seal_observation_batch(
                vec![PathObservation::new(
                    RelativePath::from_path(Path::new("entry"))
                        .expect("fixture path should be relative"),
                    PathEntryKind::File,
                    SummaryMetrics::leaf(1, ByteBounds::exact(0), ByteBounds::exact(0)),
                    Coverage::Complete,
                )],
                Vec::new(),
            )
            .expect("worker batch should seal");
        // The run was sealed for the first generation and the lease names a later one, so the
        // store refuses the run.
        let stale_lease = root_enumeration_lease(&app, ScanGeneration::from_value(7));

        app.admit_scan_input_runs(&stale_lease, batch);

        // The store thread reports the refusal; until the owner applies it the scan store is
        // still available.
        assert!(app.scan_store_available);
        assert!(app.process_scan_store_events());
        assert!(!app.scan_store_available);
        assert_eq!(
            app.scan_store_failure,
            Some(ScanStoreError::LeaseGenerationMismatch.to_string())
        );
    }

    #[test]
    fn a_map_tween_keeps_the_frame_clock_running_until_it_settles() {
        let root = tempfile::tempdir().expect("temp dir should be created");
        let mut app = App::new(
            TestBackend::new(160, 48),
            root.path().to_path_buf(),
            false,
            false,
            128,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        let mut animation = AnimationScheduler::new(false, false, Duration::ZERO);

        app.board
            .change_files(vec![map_entry(1, 0.6), map_entry(2, 0.4)]);
        draw(&mut app, &mut animation, 0);
        assert!(!app.board.is_list_layout());
        assert_eq!(animation.next_frame_at(), None);

        // A new dataset arms the layout transition. Nothing else in the frame would ask
        // the owner loop to wake up for it.
        app.board
            .change_files(vec![map_entry(1, 0.2), map_entry(2, 0.8)]);
        draw(&mut app, &mut animation, 10);
        assert!(app.board.is_transitioning());
        assert_eq!(
            animation.next_frame_at(),
            Some(Duration::from_millis(10) + crate::animation::ACTIVE_FRAME_INTERVAL)
        );

        draw(&mut app, &mut animation, 400);
        assert!(!app.board.is_transitioning());
        assert_eq!(animation.next_frame_at(), None);
    }

    #[test]
    fn resizing_from_too_small_to_an_ascii_map_keeps_focus_static_after_layout() {
        let root = tempfile::tempdir().expect("temp dir should be created");
        let terminal_events = Arc::new(Mutex::new(Vec::new()));
        let draw_events = Arc::new(Mutex::new(Vec::new()));
        let terminal_width = Arc::new(Mutex::new(31));
        let terminal_height = Arc::new(Mutex::new(8));
        let backend = ResizableTestBackend::new(
            terminal_events,
            draw_events,
            Arc::clone(&terminal_width),
            Arc::clone(&terminal_height),
        );
        let mut app = App::new(
            backend,
            root.path().to_path_buf(),
            false,
            false,
            128,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        let mut animation = AnimationScheduler::new(false, false, Duration::ZERO);

        app.loaded = true;
        app.ui_mode = UiMode::Normal;
        app.board
            .change_files(vec![map_entry(1, 0.6), map_entry(2, 0.4)]);
        draw(&mut app, &mut animation, 0);
        assert!(matches!(&app.ui_mode, UiMode::ScreenTooSmall));
        assert!(app.board.currently_selected().is_none());
        assert_eq!(animation.next_frame_at(), None);

        *terminal_width
            .lock()
            .expect("terminal width should be writable") = 160;
        *terminal_height
            .lock()
            .expect("terminal height should be writable") = 48;
        app.reset_ui_mode();
        assert!(matches!(&app.ui_mode, UiMode::Normal));
        draw(&mut app, &mut animation, 10);

        assert!(app.board.currently_selected().is_some());
        assert!(
            !app.board.is_transitioning(),
            "the initial valid layout must not be what keeps the frame clock running"
        );
        assert!(
            !animation.is_running(),
            "ASCII rendering must not schedule colour animation after layout"
        );
        assert_eq!(animation.next_frame_at(), None);
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "the resize regression covers every non-animated focus mode together"
    )]
    fn resizing_from_too_small_to_a_map_queues_pane_correction_without_activity() {
        for (case, theme_id, monochrome, reduced_motion) in [
            (
                "reduced motion",
                crate::theme::ThemeId::ExciseDark,
                false,
                true,
            ),
            ("monochrome", crate::theme::ThemeId::ExciseDark, true, false),
            (
                "non-RGB focus",
                crate::theme::ThemeId::HighContrast,
                false,
                false,
            ),
        ] {
            let root = tempfile::tempdir().expect("temp dir should be created");
            let terminal_events = Arc::new(Mutex::new(Vec::new()));
            let draw_events = Arc::new(Mutex::new(Vec::new()));
            let terminal_width = Arc::new(Mutex::new(31));
            let terminal_height = Arc::new(Mutex::new(8));
            let backend = ResizableTestBackend::new(
                terminal_events,
                Arc::clone(&draw_events),
                Arc::clone(&terminal_width),
                Arc::clone(&terminal_height),
            );
            let mut app = App::new(
                backend,
                root.path().to_path_buf(),
                false,
                false,
                128,
                KeyPreset::Vim,
                None,
                false,
            )
            .expect("app should initialize");
            let mut animation = AnimationScheduler::new(reduced_motion, monochrome, Duration::ZERO);

            app.loaded = true;
            app.ui_mode = UiMode::Normal;
            app.board
                .change_files(vec![map_entry(1, 0.6), map_entry(2, 0.4)]);
            app.render_if_dirty(
                &mut animation,
                Duration::ZERO,
                "test",
                Theme::for_id(theme_id),
                true,
                monochrome,
                reduced_motion,
            )
            .expect("too-small render should succeed");

            *terminal_width
                .lock()
                .expect("terminal width should be writable") = 160;
            *terminal_height
                .lock()
                .expect("terminal height should be writable") = 48;
            app.reset_ui_mode();
            assert!(matches!(&app.ui_mode, UiMode::Normal));

            assert!(
                app.render_if_dirty(
                    &mut animation,
                    Duration::from_millis(10),
                    "test",
                    Theme::for_id(theme_id),
                    true,
                    monochrome,
                    reduced_motion,
                )
                .expect("valid resize render should succeed"),
                "{case} should render the resized map"
            );
            assert!(app.board.currently_selected().is_some());
            assert_eq!(
                animation.next_frame_at(),
                None,
                "{case} should not depend on activity to redraw the pane"
            );
            assert!(
                app.dirty,
                "{case} should queue the active-pane correction without input"
            );

            assert!(
                app.render_if_dirty(
                    &mut animation,
                    Duration::from_millis(11),
                    "test",
                    Theme::for_id(theme_id),
                    true,
                    monochrome,
                    reduced_motion,
                )
                .expect("corrective render should succeed"),
                "{case} should perform the queued correction"
            );
            assert!(
                !app.dirty,
                "{case} correction should settle the dirty state"
            );
            assert_eq!(
                draw_events
                    .lock()
                    .expect("draw events should be readable")
                    .len(),
                2,
                "{case} should draw once for layout and once for the active pane"
            );
        }
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "the resize regression covers every focus mode together"
    )]
    fn resizing_to_an_empty_map_queues_pane_correction_after_selection_removal() {
        for (case, theme_id, monochrome, reduced_motion) in [
            (
                "animated focus",
                crate::theme::ThemeId::ExciseDark,
                false,
                false,
            ),
            (
                "reduced motion",
                crate::theme::ThemeId::ExciseDark,
                false,
                true,
            ),
            ("monochrome", crate::theme::ThemeId::ExciseDark, true, false),
            (
                "non-RGB focus",
                crate::theme::ThemeId::HighContrast,
                false,
                false,
            ),
        ] {
            let root = tempfile::tempdir().expect("temp dir should be created");
            let terminal_events = Arc::new(Mutex::new(Vec::new()));
            let draw_events = Arc::new(Mutex::new(Vec::new()));
            let terminal_width = Arc::new(Mutex::new(71));
            let terminal_height = Arc::new(Mutex::new(48));
            let backend = ResizableTestBackend::new(
                terminal_events,
                Arc::clone(&draw_events),
                Arc::clone(&terminal_width),
                Arc::clone(&terminal_height),
            );
            let mut app = App::new(
                backend,
                root.path().to_path_buf(),
                false,
                false,
                128,
                KeyPreset::Vim,
                None,
                false,
            )
            .expect("app should initialize");
            let mut animation = AnimationScheduler::new(reduced_motion, monochrome, Duration::ZERO);

            app.loaded = true;
            app.ui_mode = UiMode::Normal;
            // The list can select entries that a map must summarize as overflow.
            app.board
                .change_files(vec![map_entry(1, 0.0), map_entry(2, 0.0)]);
            assert!(
                app.render_if_dirty(
                    &mut animation,
                    Duration::ZERO,
                    "test",
                    Theme::for_id(theme_id),
                    true,
                    monochrome,
                    reduced_motion,
                )
                .expect("narrow list render should succeed")
            );
            assert!(app.board.is_list_layout());
            assert!(app.board.currently_selected().is_some());
            assert!(
                app.dirty,
                "{case} list layout should queue its initial correction"
            );
            assert!(
                app.render_if_dirty(
                    &mut animation,
                    Duration::from_millis(1),
                    "test",
                    Theme::for_id(theme_id),
                    true,
                    monochrome,
                    reduced_motion,
                )
                .expect("initial list correction should succeed")
            );
            assert!(!app.dirty);
            let draws_before_resize = draw_events
                .lock()
                .expect("draw events should be readable")
                .len();

            *terminal_width
                .lock()
                .expect("terminal width should be writable") = 160;
            app.mark_dirty();
            assert!(
                app.render_if_dirty(
                    &mut animation,
                    Duration::from_millis(10),
                    "test",
                    Theme::for_id(theme_id),
                    true,
                    monochrome,
                    reduced_motion,
                )
                .expect("wide map render should succeed"),
                "{case} should render the resized map"
            );
            assert!(!app.board.is_list_layout());
            assert!(app.board.currently_selected().is_none());
            assert_eq!(
                animation.next_frame_at(),
                None,
                "{case} should not depend on activity to redraw the pane"
            );
            assert!(
                app.dirty,
                "{case} should queue the inactive-pane correction without input"
            );

            assert!(
                app.render_if_dirty(
                    &mut animation,
                    Duration::from_millis(11),
                    "test",
                    Theme::for_id(theme_id),
                    true,
                    monochrome,
                    reduced_motion,
                )
                .expect("corrective render should succeed"),
                "{case} should perform the queued correction"
            );
            assert!(
                !app.dirty,
                "{case} correction should settle the dirty state"
            );
            assert_eq!(
                draw_events
                    .lock()
                    .expect("draw events should be readable")
                    .len(),
                draws_before_resize + 2,
                "{case} should draw once for layout and once for the inactive pane"
            );
        }
    }

    #[test]
    fn a_frame_that_hides_the_map_settles_the_tween_instead_of_holding_the_clock() {
        let root = tempfile::tempdir().expect("temp dir should be created");
        let mut app = App::new(
            TestBackend::new(160, 48),
            root.path().to_path_buf(),
            false,
            false,
            128,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        let mut animation = AnimationScheduler::new(false, false, Duration::ZERO);

        app.board
            .change_files(vec![map_entry(1, 0.6), map_entry(2, 0.4)]);
        draw(&mut app, &mut animation, 0);
        app.board
            .change_files(vec![map_entry(1, 0.2), map_entry(2, 0.8)]);
        assert!(app.board.is_transitioning());

        app.ui_mode = UiMode::ScreenTooSmall;
        draw(&mut app, &mut animation, 10);

        assert!(!app.board.is_transitioning());
        assert_eq!(animation.next_frame_at(), None);
    }

    fn report(estimated_bytes: usize) -> DeletionReport {
        DeletionReport {
            target_node_id: crate::model::NodeId(1),
            root_relative_path: PathBuf::from("target"),
            scan_root: PathBuf::from("root"),
            entries: Vec::new().into(),
            soft_cancelled: false,
            precise: true,
            estimated_bytes,
        }
    }
    #[cfg(unix)]
    fn file_plan(root: &std::path::Path) -> DeletionPlan {
        use std::os::unix::fs::MetadataExt as _;
        use std::time::UNIX_EPOCH;

        let path = root.join("target");
        std::fs::write(&path, b"original").expect("target should be written");
        let metadata = std::fs::symlink_metadata(&path).expect("target metadata should exist");
        let identity = identity_for(&path, &metadata)
            .expect("target identity should be readable")
            .expect("target should not be a symbolic link");
        let snapshot = PlannedSnapshot {
            identity: identity.clone(),
            kind: PlannedKind::File,
            apparent_bytes: u128::from(metadata.len()),
            allocated_bytes: Some(u128::from(metadata.blocks()).saturating_mul(512)),
            modified_nanos: metadata
                .modified()
                .ok()
                .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_nanos()),
        };
        let target = FileToDelete {
            node_id: crate::model::NodeId(1),
            synthetic: false,
            path_in_filesystem: root.to_path_buf(),
            path_to_file: vec![std::ffi::OsString::from("target")],
            file_type: FileType::File,
            num_descendants: None,
            size: snapshot.apparent_bytes,
            expected_snapshot: crate::model::EntrySnapshot {
                identity: Some(identity),
                kind: crate::model::NodeKind::File,
                apparent_bytes: snapshot.apparent_bytes,
                allocated_bytes: snapshot.allocated_bytes,
                modified_nanos: snapshot.modified_nanos,
            },
            reviewed_entries: vec![ReviewedEntry {
                relative_path: PathBuf::from("target"),
                snapshot,
            }],
        };
        build_plan(root, target, false).expect("deletion plan should build")
    }

    #[cfg(unix)]
    fn stage_deletion_confirmation<B: Backend>(app: &mut App<B>, plan: &DeletionPlan) {
        assert!(app.queue_deletion_confirmation(plan.target.clone(), false, 1024, Duration::ZERO));
        assert!(app.show_next_deletion_confirmation());
        assert!(app.deletion_challenge().is_some());
    }

    #[test]
    fn deletion_history_never_exceeds_its_budget_and_export_can_reclaim_it() {
        let root = tempfile::tempdir().expect("app root should exist");
        let mut app = App::new(
            TestBackend::new(80, 24),
            root.path().to_path_buf(),
            false,
            false,
            128,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        let limit = app.deletion_history_limit;

        app.complete_deletion(report(limit.saturating_add(1)));
        assert!(app.deletion_history.is_empty());
        assert_eq!(app.remaining_deletion_history_bytes(), limit);

        app.complete_deletion(report(limit));
        assert_eq!(app.deletion_history.len(), 1);
        assert_eq!(app.remaining_deletion_history_bytes(), 0);

        let exported = app.deletion_history_snapshot().len();
        app.drop_exported_deletion_history(exported);
        assert!(app.deletion_history.is_empty());
        assert_eq!(app.remaining_deletion_history_bytes(), limit);

        for _ in 0..MAX_RETAINED_DELETION_REPORTS {
            app.complete_deletion(report(0));
        }
        assert_eq!(app.deletion_history.len(), MAX_RETAINED_DELETION_REPORTS);
        app.complete_deletion(report(0));
        assert_eq!(app.deletion_history.len(), MAX_RETAINED_DELETION_REPORTS);
    }

    #[test]
    fn deletion_history_count_bounds_zero_byte_reports() {
        let root = tempfile::tempdir().expect("app root should exist");
        let mut app = App::new(
            TestBackend::new(80, 24),
            root.path().to_path_buf(),
            false,
            false,
            128,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");

        for _ in 0..=MAX_RETAINED_DELETION_REPORTS {
            assert!(!app.complete_deletion(report(0)));
        }

        assert_eq!(app.deletion_history.len(), MAX_RETAINED_DELETION_REPORTS);
        assert_eq!(
            app.remaining_deletion_history_bytes(),
            app.deletion_history_limit
        );
    }

    #[test]
    fn exported_deletion_history_is_dropped_and_later_reports_stay() {
        let root = tempfile::tempdir().expect("app root should exist");
        let mut app = App::new(
            TestBackend::new(80, 24),
            root.path().to_path_buf(),
            false,
            false,
            128,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        let limit = app.deletion_history_limit;
        app.complete_deletion(report(1024));
        app.complete_deletion(report(2048));
        let exported = app.deletion_history_snapshot();
        assert_eq!(exported.len(), 2);

        // This report finished while the export ran, so the export did not write it.
        app.complete_deletion(report(4096));
        app.drop_exported_deletion_history(exported.len());

        let kept = app.deletion_history_snapshot();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].estimated_bytes, 4096);
        assert_eq!(app.remaining_deletion_history_bytes(), limit - 4096);
    }

    /// An app whose published map lists the files `a`, `b` and `c`, `c` the largest and so the
    /// one the map has selected, with a queued deletion target for each name asked for.
    #[cfg(any(unix, windows))]
    fn app_with_three_files_and_targets(
        root: &std::path::Path,
        queued: &[&str],
    ) -> (App<TestBackend>, Vec<FileToDelete>) {
        for (name, size) in [("a", 1_usize), ("b", 2), ("c", 4096)] {
            std::fs::write(root.join(name), vec![0_u8; size]).expect("fixture file should exist");
        }
        let mut app = App::new(
            TestBackend::new(160, 48),
            root.to_path_buf(),
            false,
            false,
            128,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        for name in ["a", "b", "c"] {
            add_fixture_entry(&mut app, &root.join(name));
        }
        app.finalize_scan();
        app.start_ui();
        let mut animation = AnimationScheduler::new(false, false, Duration::ZERO);
        draw(&mut app, &mut animation, 0);
        for index in 0..3 {
            app.board.set_selected_index(index);
            if app
                .board
                .currently_selected()
                .is_some_and(|tile| tile.name == "c")
            {
                break;
            }
        }
        assert!(
            app.board
                .currently_selected()
                .is_some_and(|tile| tile.name == "c"),
            "the largest file should be the one selected"
        );
        let targets = queued
            .iter()
            .map(|name| {
                let relative =
                    RelativePath::from_path(Path::new(name)).expect("fixture path is canonical");
                let entry = app
                    .scan_store
                    .published()
                    .expect("published generation should exist")
                    .page_entry(&relative)
                    .expect("canonical query should succeed")
                    .expect("the file should be in the published generation");
                SnapshotTree::deletion_target_from_entry(
                    root.to_path_buf(),
                    node_id_of(&app, name),
                    &relative,
                    entry,
                    false,
                )
                .expect("the file should retain its canonical snapshot")
            })
            .collect();
        (app, targets)
    }

    /// The history keeps a place for the report of every deletion that was allowed to start: the
    /// cap counts the deletions in flight, so none can finish into a full history and have its
    /// report dropped, neither from the file an export writes nor from the history it leaves.
    #[cfg(any(unix, windows))]
    #[test]
    fn every_deletion_allowed_to_start_finds_room_for_its_report() {
        let root = tempfile::tempdir().expect("app root should exist");
        let (mut app, targets) = app_with_three_files_and_targets(root.path(), &["a", "b"]);
        let retained = MAX_RETAINED_DELETION_REPORTS - 2;
        for _ in 0..retained {
            app.complete_deletion(report(0));
        }
        // Two deletions were asked for while the history still had room.
        let mut started = 0;
        for target in targets {
            assert!(app.queue_deletion_confirmation(target, false, 1024, Duration::ZERO));
            started += 1;
        }
        // A third is asked for while those two are in flight, the history two reports short of full.
        if let Some(target) = app.request_deletion() {
            assert!(app.queue_deletion_confirmation(target, false, 1024, Duration::ZERO));
            started += 1;
        }

        for _ in 0..started {
            app.complete_deletion(report(0));
        }

        assert_eq!(
            app.deletion_history.len(),
            retained + started,
            "a deletion that was allowed to start finished without room for its report"
        );
    }

    /// Each deletion's plan may use half of what the history has left, so with two in flight the
    /// second must be budgeted from what the first leaves, or both reports could not be kept.
    #[cfg(any(unix, windows))]
    #[test]
    fn the_plan_budgets_of_deletions_in_flight_fit_the_history_together() {
        let root = tempfile::tempdir().expect("app root should exist");
        let (mut app, targets) = app_with_three_files_and_targets(root.path(), &["a", "b"]);
        let limit = app.deletion_history_limit;
        let mut budgets = Vec::new();
        for target in targets {
            let budget = app.maximum_deletion_plan_bytes();
            assert!(app.queue_deletion_confirmation(target, false, budget, Duration::ZERO));
            budgets.push(budget);
        }

        assert!(
            budgets.iter().sum::<usize>() < limit,
            "two plans each allowed half of the history cannot both be kept: {budgets:?} of {limit}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn confirmed_target_starts_background_planning_after_returning_to_map() {
        let root = tempfile::tempdir().expect("app root should exist");
        let plan = file_plan(root.path());
        let expected_path = plan.target.full_path();
        let mut app = App::new(
            TestBackend::new(80, 24),
            root.path().to_path_buf(),
            false,
            false,
            128,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        app.loaded = true;
        app.ui_mode = UiMode::Normal;
        assert!(app.queue_deletion_confirmation(plan.target, false, 1024, Duration::ZERO));
        assert!(app.show_next_deletion_confirmation());
        assert!(app.next_deletion_planning_work().is_none());

        let (work_id, target) = app
            .arm_and_confirm_deletion_target()
            .expect("Enter should return the target for background planning");
        assert_eq!(target.full_path(), expected_path);
        assert!(matches!(app.ui_mode, UiMode::Normal));
        assert!(app.queue_confirmed_deletion(work_id, target, Duration::ZERO));
        assert!(matches!(
            app.next_deletion_planning_work(),
            Some(DeletionWorkCommand::Plan { work_id: id, .. }) if id == work_id
        ));
    }

    #[cfg(unix)]
    #[test]
    fn resize_and_too_small_transitions_cancel_confirmation_before_retry() {
        let root = tempfile::tempdir().expect("app root should exist");
        let terminal_events = Arc::new(Mutex::new(Vec::new()));
        let draw_events = Arc::new(Mutex::new(Vec::new()));
        let terminal_width = Arc::new(Mutex::new(80));
        let terminal_height = Arc::new(Mutex::new(24));
        let backend = ResizableTestBackend::new(
            terminal_events,
            draw_events,
            Arc::clone(&terminal_width),
            Arc::clone(&terminal_height),
        );
        let mut app = App::new(
            backend,
            root.path().to_path_buf(),
            false,
            false,
            128,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        app.loaded = true;

        stage_deletion_confirmation(&mut app, &file_plan(root.path()));
        *terminal_width
            .lock()
            .expect("terminal width should be writable") = 31;
        *terminal_height
            .lock()
            .expect("terminal height should be writable") = 8;
        let mut animation = AnimationScheduler::new(false, false, Duration::ZERO);
        draw(&mut app, &mut animation, 0);
        assert!(matches!(&app.ui_mode, UiMode::ScreenTooSmall));
        assert!(!app.deletion_work_summary().has_work());

        *terminal_width
            .lock()
            .expect("terminal width should be writable") = 80;
        *terminal_height
            .lock()
            .expect("terminal height should be writable") = 24;
        app.reset_ui_mode();
        stage_deletion_confirmation(&mut app, &file_plan(root.path()));
        assert_eq!(app.deletion_work_summary().pending_operations, 1);
    }

    #[test]
    fn resize_keeps_theme_preview_state_until_explicit_restore() {
        let root = tempfile::tempdir().expect("app root should exist");
        let mut app = App::new(
            TestBackend::new(80, 24),
            root.path().to_path_buf(),
            false,
            false,
            128,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        app.loaded = true;
        app.ui_mode = UiMode::Normal;
        app.open_theme_picker(ThemeId::ExciseDark);
        assert_eq!(app.move_theme_picker(false), Some(ThemeId::ExciseLight));

        app.reset_ui_mode();

        assert!(matches!(
            app.ui_mode,
            UiMode::ThemePicker {
                original: ThemeId::ExciseDark,
                selected: ThemeId::ExciseLight,
                ..
            }
        ));

        app.enter_screen_too_small();
        assert!(matches!(app.ui_mode, UiMode::ScreenTooSmall));
        app.reset_ui_mode();
        assert!(matches!(
            app.ui_mode,
            UiMode::ThemePicker {
                original: ThemeId::ExciseDark,
                selected: ThemeId::ExciseLight,
                ..
            }
        ));
        assert_eq!(app.cancel_theme_picker(), Some(ThemeId::ExciseDark));
        assert!(matches!(app.ui_mode, UiMode::Normal));
    }

    #[test]
    fn scan_completion_updates_a_theme_picker_suspended_by_small_screen() {
        let root = tempfile::tempdir().expect("app root should exist");
        let mut app = App::new(
            TestBackend::new(80, 24),
            root.path().to_path_buf(),
            false,
            false,
            128,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        app.open_theme_picker(ThemeId::ExciseDark);
        assert_eq!(app.move_theme_picker(false), Some(ThemeId::ExciseLight));
        app.enter_screen_too_small();

        app.start_ui();
        app.reset_ui_mode();

        assert_eq!(app.cancel_theme_picker(), Some(ThemeId::ExciseDark));
        assert!(matches!(app.ui_mode, UiMode::Normal));
    }

    #[test]
    fn dismissing_exit_returns_to_an_active_generation_rebuild() {
        let root = tempfile::tempdir().expect("app root should exist");
        let target = root.path().join("refreshing");
        let mut app = App::new(
            TestBackend::new(80, 24),
            root.path().to_path_buf(),
            false,
            false,
            128,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        app.loaded = true;
        app.ui_mode = UiMode::Rebuilding {
            target: target.clone(),
        };

        app.prompt_exit(ExitWork::None);
        app.dismiss_exit();

        assert!(matches!(
            &app.ui_mode,
            UiMode::Rebuilding { target: current } if current == &target
        ));
    }

    #[test]
    fn dismissing_exit_returns_to_unavailable_scan_results() {
        let root = tempfile::tempdir().expect("app root should exist");
        let mut app = App::new(
            TestBackend::new(80, 24),
            root.path().to_path_buf(),
            false,
            false,
            128,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        app.loaded = true;
        app.scan_store_available = false;
        app.ui_mode = UiMode::ScanResultsUnavailable("scan failed".to_string());

        app.prompt_exit(ExitWork::None);
        app.dismiss_exit();

        assert!(matches!(app.ui_mode, UiMode::ScanResultsUnavailable(_)));
    }

    #[test]
    fn initial_scan_completion_updates_an_open_exit_target() {
        let root = tempfile::tempdir().expect("app root should exist");
        let mut app = App::new(
            TestBackend::new(80, 24),
            root.path().to_path_buf(),
            false,
            false,
            128,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");

        app.prompt_exit(ExitWork::None);
        app.start_ui();
        app.dismiss_exit();

        assert!(matches!(app.ui_mode, UiMode::Normal));
    }

    #[test]
    fn refreshing_exit_prompt_keeps_its_stale_return_target() {
        let root = tempfile::tempdir().expect("app root should exist");
        let mut app = App::new(
            TestBackend::new(80, 24),
            root.path().to_path_buf(),
            false,
            false,
            128,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        app.loaded = true;
        app.ui_mode = UiMode::StaleSnapshot;

        app.prompt_exit(ExitWork::None);
        app.prompt_exit(ExitWork::None);
        app.dismiss_exit();

        assert!(matches!(app.ui_mode, UiMode::StaleSnapshot));
    }

    #[test]
    fn unavailable_rebuild_failure_preserves_exit_overlay() {
        let root = tempfile::tempdir().expect("app root should exist");
        let mut app = App::new(
            TestBackend::new(80, 24),
            root.path().to_path_buf(),
            false,
            false,
            128,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        app.loaded = true;
        app.ui_mode = UiMode::Exiting {
            work: ExitWork::None,
            return_to: ThemePickerReturn::Rebuilding {
                target: root.path().to_path_buf(),
            },
        };

        app.show_scan_results_unavailable("rebuild publication failed");

        assert!(matches!(
            &app.ui_mode,
            UiMode::Exiting {
                return_to: ThemePickerReturn::ScanResultsUnavailable(message),
                ..
            } if message == "rebuild publication failed"
        ));
        app.dismiss_exit();
        assert!(matches!(app.ui_mode, UiMode::ScanResultsUnavailable(_)));
    }

    #[test]
    fn unavailable_failure_updates_a_suspended_exit_destination() {
        let root = tempfile::tempdir().expect("app root should exist");
        let mut app = App::new(
            TestBackend::new(80, 24),
            root.path().to_path_buf(),
            false,
            false,
            128,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        app.loaded = true;
        app.ui_mode = UiMode::Exiting {
            work: ExitWork::None,
            return_to: ThemePickerReturn::Normal,
        };
        app.suspended_ui_mode = Some(UiMode::Loading);

        app.show_scan_results_unavailable("scan failed");
        app.dismiss_exit();
        app.reset_ui_mode();

        assert!(matches!(app.ui_mode, UiMode::ScanResultsUnavailable(_)));
    }

    #[test]
    fn transient_modals_restore_stale_and_unavailable_targets() {
        let root = tempfile::tempdir().expect("app root should exist");
        let mut app = App::new(
            TestBackend::new(80, 24),
            root.path().to_path_buf(),
            false,
            false,
            128,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        app.loaded = true;

        app.ui_mode = UiMode::StaleSnapshot;
        app.show_notice("deletion unavailable");
        app.dismiss_transient_modal();
        assert!(matches!(app.ui_mode, UiMode::StaleSnapshot));

        app.ui_mode = UiMode::ScanResultsUnavailable("scan failed".to_string());
        app.show_error("theme save failed");
        app.dismiss_transient_modal();
        assert!(matches!(app.ui_mode, UiMode::ScanResultsUnavailable(_)));
    }

    #[test]
    fn initial_completion_retargets_an_open_error_modal() {
        let root = tempfile::tempdir().expect("app root should exist");
        let mut app = App::new(
            TestBackend::new(80, 24),
            root.path().to_path_buf(),
            false,
            false,
            128,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        app.ui_mode = UiMode::ErrorMessage {
            message: "theme save failed".to_string(),
            return_to: ThemePickerReturn::Loading,
        };

        app.start_ui();
        app.dismiss_transient_modal();

        assert!(matches!(app.ui_mode, UiMode::Normal));
    }

    #[test]
    fn transient_modals_survive_unavailable_and_cancelled_rebuilds() {
        let root = tempfile::tempdir().expect("app root should exist");
        let mut app = App::new(
            TestBackend::new(80, 24),
            root.path().to_path_buf(),
            false,
            false,
            128,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        app.loaded = true;
        app.ui_mode = UiMode::ErrorMessage {
            message: "theme save failed".to_string(),
            return_to: ThemePickerReturn::Normal,
        };

        app.show_scan_results_unavailable("scan failed");
        assert!(matches!(
            app.ui_mode,
            UiMode::ErrorMessage {
                return_to: ThemePickerReturn::ScanResultsUnavailable(_),
                ..
            }
        ));
        app.dismiss_transient_modal();
        assert!(matches!(app.ui_mode, UiMode::ScanResultsUnavailable(_)));

        app.scan_store_available = true;
        app.ui_mode = UiMode::Notice {
            message: "theme save failed".to_string(),
            return_to: ThemePickerReturn::Rebuilding {
                target: root.path().to_path_buf(),
            },
        };
        app.retain_snapshot_mode_after_rebuild_cancellation();
        assert!(matches!(
            app.ui_mode,
            UiMode::Notice {
                return_to: ThemePickerReturn::StaleSnapshot,
                ..
            }
        ));
    }

    #[test]
    fn cancelling_rebuild_preserves_a_theme_picker_overlay() {
        let root = tempfile::tempdir().expect("app root should exist");
        let target_path = root.path().join("target");
        std::fs::write(&target_path, b"payload").expect("fixture target should exist");
        let mut app = App::new(
            TestBackend::new(160, 48),
            root.path().to_path_buf(),
            false,
            false,
            128,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        add_fixture_entry(&mut app, &target_path);
        app.finalize_scan();
        app.start_ui();
        let mut animation = AnimationScheduler::new(false, false, Duration::ZERO);
        draw(&mut app, &mut animation, 0);
        app.require_generation_rebuild_for_test();
        assert!(
            app.begin_generation_rebuild()
                .expect("rebuild should start with the retained map")
        );
        app.open_theme_picker(ThemeId::ExciseDark);
        assert_eq!(app.move_theme_picker(false), Some(ThemeId::ExciseLight));

        app.cancel_generation_rebuild()
            .expect("retained rebuild should cancel cleanly");

        assert!(matches!(
            app.ui_mode,
            UiMode::ThemePicker {
                original: ThemeId::ExciseDark,
                selected: ThemeId::ExciseLight,
                return_to: ThemePickerReturn::StaleSnapshot,
            }
        ));
        assert_eq!(app.cancel_theme_picker(), Some(ThemeId::ExciseDark));
        assert!(matches!(app.ui_mode, UiMode::StaleSnapshot));
    }

    /// An app with a published one-file map that has begun rebuilding it.
    fn app_rebuilding_a_published_map() -> (tempfile::TempDir, App<TestBackend>) {
        let root = tempfile::tempdir().expect("app root should exist");
        let target_path = root.path().join("target");
        std::fs::write(&target_path, b"payload").expect("fixture target should exist");
        let mut app = App::new(
            TestBackend::new(160, 48),
            root.path().to_path_buf(),
            false,
            false,
            128,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        add_fixture_entry(&mut app, &target_path);
        app.finalize_scan();
        app.start_ui();
        app.require_generation_rebuild_for_test();
        assert!(
            app.begin_generation_rebuild()
                .expect("rebuild should start with the retained map")
        );
        (root, app)
    }

    /// The scanner has finished by the time the replacement map is published, so its own
    /// cancellation flag no longer reaches the rebuild: Esc has to be honored by the app, which
    /// still shows `Rebuilding` and its `[Esc] cancel` hint until the map arrives.
    #[test]
    fn cancelling_a_rebuild_while_its_map_is_published_drops_that_map_and_keeps_the_stale_one() {
        let (_root, mut app) = app_rebuilding_a_published_map();
        app.begin_rebuild_publication();
        assert!(app.scan_store_busy(), "the replacement map is being built");

        app.suppress_generation_rebuild_restart();
        app.process_scan_store_events();

        assert_eq!(
            app.take_finished_publication(),
            Some(FinishedPublication::RebuildCancelled)
        );
        assert_eq!(
            app.scan_store.published_generation(),
            Some(ScanGeneration::initial()),
            "the replacement map was dropped, not swapped in"
        );
        app.cancel_generation_rebuild()
            .expect("the cancelled rebuild should settle");
        assert!(matches!(app.ui_mode, UiMode::StaleSnapshot));
        assert!(
            app.begin_generation_rebuild().is_ok_and(|started| !started),
            "a cancelled rebuild is not restarted by itself"
        );
    }

    /// Esc can land after the scanner's last event was queued and before the loop handled it:
    /// the rebuild is then cancelled without ever asking the store to build its map.
    #[test]
    fn a_rebuild_cancelled_before_its_scan_end_is_handled_never_publishes() {
        let (_root, mut app) = app_rebuilding_a_published_map();
        app.suppress_generation_rebuild_restart();

        app.begin_rebuild_publication();

        assert!(app.publications.is_empty(), "no map was asked for");
        assert_eq!(
            app.take_finished_publication(),
            Some(FinishedPublication::RebuildCancelled)
        );
    }

    /// What a test removes when it is about the map that follows a removal that left no link
    /// behind: a file where the executor can show that (Linux, and Windows through the handle
    /// that removes it), and on macOS, which opens nothing it removes and counts every file or
    /// link it removes as possibly linked, an empty folder.
    #[cfg(any(unix, windows))]
    fn make_a_removable_target(path: &Path) {
        if cfg!(target_vendor = "apple") {
            std::fs::create_dir(path).expect("target fixture should exist");
        } else {
            std::fs::write(path, b"target").expect("target fixture should exist");
        }
    }

    /// An app whose published map lists `target` and `survivor`, and the report of deleting
    /// `target` from disk completely. The target is a file where the executor can show that its
    /// removal left no link behind (Linux, and Windows through the handle that removes it); on
    /// macOS, which opens nothing it removes and so counts every file or link it removes as
    /// possibly linked, it is an empty folder, which has no other links to speak of. Either way
    /// the report is one the owner may answer with a map updated in place.
    #[cfg(any(unix, windows))]
    fn app_and_target_removal_report() -> (tempfile::TempDir, App<TestBackend>, DeletionReport) {
        app_and_target_removal_report_beside(false)
    }

    /// As [`app_and_target_removal_report`], with a `folder` holding one file beside them when
    /// `with_folder` is set.
    #[cfg(any(unix, windows))]
    fn app_and_target_removal_report_beside(
        with_folder: bool,
    ) -> (tempfile::TempDir, App<TestBackend>, DeletionReport) {
        let root = tempfile::tempdir().expect("app root should exist");
        let target_path = root.path().join("target");
        let survivor_path = root.path().join("survivor");
        make_a_removable_target(&target_path);
        std::fs::write(&survivor_path, b"survivor").expect("survivor fixture should exist");
        let mut app = App::new(
            TestBackend::new(160, 48),
            root.path().to_path_buf(),
            false,
            false,
            128,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        let mut entries = vec![target_path.clone(), survivor_path.clone()];
        if with_folder {
            let folder = root.path().join("folder");
            std::fs::create_dir(&folder).expect("folder fixture should exist");
            std::fs::write(folder.join("inner"), b"inner").expect("inner fixture should exist");
            entries.extend([folder.clone(), folder.join("inner")]);
        }
        for path in &entries {
            add_fixture_entry(&mut app, path);
        }
        app.finalize_scan();
        let target_id = app
            .files_in_current_view(0)
            .into_iter()
            .find(|file| file.name == "target")
            .expect("target should be visible")
            .node_id;
        let relative =
            RelativePath::from_path(Path::new("target")).expect("target path should be canonical");
        let entry = app
            .scan_store
            .published()
            .expect("published generation should exist")
            .page_entry(&relative)
            .expect("canonical target query should succeed")
            .expect("target should be in the published generation");
        let target = SnapshotTree::deletion_target_from_entry(
            root.path().to_path_buf(),
            target_id,
            &relative,
            entry,
            false,
        )
        .expect("target should retain its canonical snapshot");
        let plan = crate::deletion::build_plan(root.path(), target, false)
            .expect("target plan should build");
        let report = crate::deletion::execute_plan(
            root.path(),
            plan,
            &std::sync::atomic::AtomicBool::new(false),
            &std::sync::atomic::AtomicBool::new(false),
        );
        assert!(report.target_was_removed());
        // These tests are about what the owner does with a report of a removal that left no link
        // behind. Whether the executor can prove that of a file is a fact about the platform: on
        // Linux it does, and the report says so; on Windows it depends on what the file system
        // says through the handle that removed the file, and is tested where the executor reads
        // it (`deletion`); macOS proves it of no file (the target there is a folder, which has
        // no other links to speak of).
        #[cfg(unix)]
        assert!(
            !report.deleted_files_may_have_other_links(),
            "an entry removed whole that left no link behind"
        );
        #[cfg(windows)]
        let report = report.assuming_no_link_survived();
        (root, app, report)
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn completed_deletion_republishes_the_snapshot_without_the_removed_target() {
        let (_root, mut app, report) = app_and_target_removal_report();
        app.snapshot_filter = Some((
            FilterPattern::new("target").expect("filter should compile"),
            RelativePath::from_path(Path::new("target")).expect("target path should be canonical"),
        ));
        assert!(app.complete_deletion(report));
        app.process_scan_store_events();
        assert_eq!(
            app.scan_store.published_generation(),
            Some(ScanGeneration::from_value(1))
        );
        assert!(
            app.snapshot_filter.is_none(),
            "a filter rooted inside the removed prefix must not survive the new generation"
        );
        assert_eq!(
            app.files_in_current_view(0)
                .into_iter()
                .map(|file| file.name)
                .collect::<Vec<_>>(),
            vec![std::ffi::OsString::from("survivor")]
        );
    }

    /// The map a deletion leaves is built from the map before it, folders included: a folder
    /// beside the deleted entry must not stop the overlay from publishing, or the deletion falls
    /// back to rebuilding the whole map.
    #[cfg(any(unix, windows))]
    #[test]
    fn a_deletion_beside_a_folder_republishes_the_map_instead_of_rebuilding_it() {
        let (_root, mut app, report) = app_and_target_removal_report_beside(true);

        assert!(app.complete_deletion(report));
        assert!(app.process_scan_store_events());

        assert!(
            !app.generation_rebuild_required,
            "the overlay published, so no rebuild is needed"
        );
        assert_eq!(listed_names(&app), ["folder", "survivor"]);
        assert_eq!(
            app.scan_store.published_generation(),
            Some(ScanGeneration::from_value(1))
        );
    }

    /// A rebuild the reader cancelled while its replacement map was being published leaves the
    /// map they had installed. The map a later deletion leaves derives from that map, not from
    /// the replacement the reader's cancellation dropped, whose files are gone.
    #[cfg(any(unix, windows))]
    #[test]
    fn a_deletion_after_a_cancelled_rebuild_republishes_the_map_that_stayed_installed() {
        let (_root, mut app, report) = app_and_target_removal_report();
        app.require_generation_rebuild_for_test();
        assert!(
            app.begin_generation_rebuild()
                .expect("the rebuild should start with the retained map")
        );
        app.begin_rebuild_publication();
        app.suppress_generation_rebuild_restart();
        app.process_scan_store_events();
        assert_eq!(
            app.take_finished_publication(),
            Some(FinishedPublication::RebuildCancelled)
        );
        app.cancel_generation_rebuild()
            .expect("the cancelled rebuild should settle");
        assert_eq!(
            app.scan_store.published_generation(),
            Some(ScanGeneration::initial()),
            "the replacement map was dropped, so the first map stays installed"
        );

        // A deletion that was already running ends.
        assert!(app.complete_deletion(report));
        assert!(app.process_scan_store_events());

        assert_eq!(
            listed_names(&app),
            ["survivor"],
            "the map without the deleted entry was not published"
        );
        assert!(
            app.scan_store.published_generation() > Some(ScanGeneration::initial()),
            "the overlay should have replaced the installed map"
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn removed_entry_stays_listed_until_the_overlay_result_arrives() {
        let (_root, mut app, report) = app_and_target_removal_report();

        assert!(app.complete_deletion(report));

        // The store thread builds the overlay: until its result arrives, the reader keeps the
        // map they had.
        assert_eq!(listed_names(&app), ["survivor", "target"]);
        assert_eq!(
            app.scan_store.published_generation(),
            Some(ScanGeneration::initial())
        );
        assert!(app.scan_store_busy());

        assert!(app.process_scan_store_events());

        assert_eq!(listed_names(&app), ["survivor"]);
        assert_eq!(
            app.scan_store.published_generation(),
            Some(ScanGeneration::from_value(1))
        );
        assert!(!app.scan_store_busy());
    }

    /// The next overlay derives from the last one's map, so a deletion queued behind one whose
    /// map is still being built starts only once that map has arrived. It waits: blocking would
    /// cancel it, and the reader never asked for that.
    #[cfg(any(unix, windows))]
    #[test]
    fn a_queued_deletion_waits_for_the_map_of_the_one_before_it() {
        let (root, mut app, report) = app_and_target_removal_report();
        let survivor =
            RelativePath::from_path(Path::new("survivor")).expect("survivor path is canonical");
        let entry = app
            .scan_store
            .published()
            .expect("published generation should exist")
            .page_entry(&survivor)
            .expect("canonical survivor query should succeed")
            .expect("survivor should be in the published generation");
        let survivor_target = SnapshotTree::deletion_target_from_entry(
            root.path().to_path_buf(),
            node_id_of(&app, "survivor"),
            &survivor,
            entry,
            false,
        )
        .expect("survivor should retain its canonical snapshot");
        app.start_ui();
        assert!(app.complete_deletion(report));
        assert!(
            app.scan_store_busy(),
            "the first deletion's map is still being built"
        );
        assert!(app.queue_deletion_confirmation(survivor_target, false, 1 << 20, Duration::ZERO));
        assert!(app.show_next_deletion_confirmation());
        let (work_id, confirmed) = app
            .arm_and_confirm_deletion_target()
            .expect("the reader confirms the second deletion");
        assert!(app.queue_confirmed_deletion(work_id, confirmed, Duration::ZERO));

        assert!(
            app.next_deletion_planning_work().is_none(),
            "nothing starts from the map the last deletion made stale"
        );
        assert!(
            app.deletion_work_summary().has_work(),
            "the second deletion waits for the map; it is not cancelled"
        );

        assert!(app.process_scan_store_events());

        assert!(matches!(
            app.next_deletion_planning_work(),
            Some(DeletionWorkCommand::Plan { work_id: id, .. }) if id == work_id
        ));
    }

    /// The map on screen lists what the last deletion removed until the overlay arrives, and a
    /// report written from it would too.
    #[cfg(any(unix, windows))]
    #[test]
    fn a_scan_report_waits_for_the_map_that_reflects_the_last_deletion() {
        let (_root, mut app, report) = app_and_target_removal_report();
        assert!(app.complete_deletion(report));
        let mut exported = Vec::new();

        assert!(matches!(
            app.write_scan_report(&RunSummary::default(), &mut exported),
            Err(ReportError::Invariant(message)) if message.contains("last deletion")
        ));
        assert!(exported.is_empty());

        assert!(app.process_scan_store_events());
        app.write_scan_report(&RunSummary::default(), &mut exported)
            .expect("the map without the deleted target can be reported");
        assert!(
            String::from_utf8(exported)
                .expect("the report is JSON text")
                .contains("survivor")
        );
    }

    /// No map follows a deletion whose store was lost while the map was being built: the one on
    /// screen lists what the deletion removed, and nothing will replace it, so it must not stay
    /// up as if it were current.
    #[cfg(any(unix, windows))]
    #[test]
    fn losing_the_store_while_a_deletions_map_is_built_hides_the_map_that_lists_it() {
        let (_root, mut app, report) = app_and_target_removal_report();
        assert!(app.complete_deletion(report));
        assert!(app.scan_store_busy());
        assert_eq!(listed_names(&app), ["survivor", "target"]);

        app.scan_store_stopped("the scan store thread stopped");

        assert!(matches!(app.ui_mode, UiMode::ScanResultsUnavailable(_)));
        assert!(!app.scan_store_busy());
        assert!(!app.scan_store_available);
        assert!(app.request_deletion().is_none());
    }

    /// An app whose published map lists the folder `holder` and what is in it: `target` and
    /// `survivor`, and what `before_scan` adds beside them. The target is what a test removes
    /// when it is about the map that follows a removal that left no link behind
    /// ([`make_a_removable_target`]).
    #[cfg(any(unix, windows))]
    fn app_listing_a_folder(
        before_scan: impl FnOnce(&Path),
    ) -> (tempfile::TempDir, App<TestBackend>) {
        app_listing_a_folder_with(make_a_removable_target, before_scan)
    }

    /// As [`app_listing_a_folder`], with a file as the target on every platform: for a test that
    /// is about what the executor says of the links of a file it removes.
    #[cfg(unix)]
    fn app_listing_a_folder_holding_a_file(
        before_scan: impl FnOnce(&Path),
    ) -> (tempfile::TempDir, App<TestBackend>) {
        app_listing_a_folder_with(
            |path| std::fs::write(path, b"target").expect("target fixture should exist"),
            before_scan,
        )
    }

    #[cfg(any(unix, windows))]
    fn app_listing_a_folder_with(
        make_the_target: impl FnOnce(&Path),
        before_scan: impl FnOnce(&Path),
    ) -> (tempfile::TempDir, App<TestBackend>) {
        let root = tempfile::tempdir().expect("app root should exist");
        let holder = root.path().join("holder");
        std::fs::create_dir(&holder).expect("holder fixture should exist");
        make_the_target(&holder.join("target"));
        std::fs::write(holder.join("survivor"), b"survivor")
            .expect("survivor fixture should exist");
        before_scan(&holder);
        let mut app = App::new(
            TestBackend::new(160, 48),
            root.path().to_path_buf(),
            false,
            false,
            128,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        add_fixture_entry(&mut app, &holder);
        for entry in std::fs::read_dir(&holder).expect("holder should list") {
            add_fixture_entry(&mut app, &entry.expect("holder entry should read").path());
        }
        app.finalize_scan();
        (root, app)
    }

    /// The target of a deletion of `relative` below `root`, as the published map describes it.
    #[cfg(any(unix, windows))]
    fn deletion_target_in_the_map(
        root: &Path,
        app: &App<TestBackend>,
        relative: &str,
    ) -> FileToDelete {
        let relative =
            RelativePath::from_path(Path::new(relative)).expect("target path should be canonical");
        let entry = app
            .scan_store
            .published()
            .expect("published generation should exist")
            .page_entry(&relative)
            .expect("canonical target query should succeed")
            .expect("target should be in the published generation");
        SnapshotTree::deletion_target_from_entry(
            root.to_path_buf(),
            crate::model::NodeId(1),
            &relative,
            entry,
            false,
        )
        .expect("target should retain its canonical snapshot")
    }

    /// Deletes `holder/target` from disk, completely, the way the executor does, and returns the
    /// report that ends the deletion. `after_planning` is what happens to the file system between
    /// the plan the reader confirmed and the execution of it.
    #[cfg(any(unix, windows))]
    fn delete_the_target_in_the_folder(
        root: &Path,
        app: &App<TestBackend>,
        after_planning: impl FnOnce(),
    ) -> DeletionReport {
        let target = deletion_target_in_the_map(root, app, "holder/target");
        let plan =
            crate::deletion::build_plan(root, target, false).expect("target plan should build");
        after_planning();
        let report = crate::deletion::execute_plan(
            root,
            plan,
            &std::sync::atomic::AtomicBool::new(false),
            &std::sync::atomic::AtomicBool::new(false),
        );
        assert!(report.target_was_removed());
        report
    }

    /// What the deletion ends in: the app is told, and the store thread's answer is applied.
    #[cfg(any(unix, windows))]
    fn finish_the_deletion(app: &mut App<TestBackend>, report: DeletionReport) {
        assert!(app.complete_deletion(report));
        app.process_scan_store_events();
    }

    /// The map a deletion leaves says what the folder that held the entry is now, so that the
    /// folder can be deleted next. That is only true of a folder whose entries are the ones the
    /// map lists but the removed one: the folder's modification time is what ties a later
    /// deletion of it to the map, and the time the file system now reports also describes
    /// whatever another process did to the folder meanwhile. A map that recorded it as it is
    /// would let the folder be deleted with entries in it that the map never showed.
    #[cfg(any(unix, windows))]
    #[test]
    fn a_folder_that_changed_beside_the_removed_entry_is_scanned_again_not_blessed() {
        type Change = fn(&Path);
        let changes: [(&str, Change); 5] = [
            ("a sibling was created", |holder| {
                std::fs::write(holder.join("intruder"), b"new").expect("sibling should be created");
            }),
            ("a sibling was removed", |holder| {
                std::fs::remove_file(holder.join("survivor")).expect("sibling should be removed");
            }),
            ("a sibling was renamed", |holder| {
                std::fs::rename(holder.join("survivor"), holder.join("renamed"))
                    .expect("sibling should be renamed");
            }),
            (
                "a sibling was replaced by another file of the same name",
                |holder| {
                    let replacement = holder.join("replacement");
                    std::fs::write(&replacement, b"another file")
                        .expect("replacement should exist");
                    std::fs::rename(&replacement, holder.join("survivor"))
                        .expect("sibling should be replaced");
                },
            ),
            ("the removed entry was created again", |holder| {
                std::fs::write(holder.join("target"), b"again").expect("entry should return");
            }),
        ];

        let mut failures = Vec::new();
        for (what, change) in changes {
            let (root, mut app) = app_listing_a_folder(|_| {});
            let report = delete_the_target_in_the_folder(root.path(), &app, || {});
            change(&root.path().join("holder"));

            finish_the_deletion(&mut app, report);

            if !app.generation_rebuild_required {
                failures.push(format!("{what}: the map recorded a folder it had not seen"));
            }
            if app.scan_store.published_generation() != Some(ScanGeneration::initial()) {
                failures.push(format!(
                    "{what}: the map the reader has was replaced before the scan that follows"
                ));
            }
        }

        assert!(failures.is_empty(), "{failures:#?}");
    }

    /// The check above must not turn every deletion into a scan: a folder that holds only what
    /// the map lists, of every kind, and one the program cannot open (the scan listed it from
    /// its parent), is still described in place.
    #[cfg(unix)]
    #[test]
    fn a_folder_whose_other_entries_did_not_change_is_still_described_in_place() {
        use std::os::unix::fs::PermissionsExt as _;

        let (root, mut app) = app_listing_a_folder(|holder| {
            std::fs::create_dir(holder.join("sub")).expect("subfolder should exist");
            std::os::unix::fs::symlink("survivor", holder.join("link")).expect("link should exist");
            let sealed = holder.join("sealed");
            std::fs::create_dir(&sealed).expect("sealed folder should exist");
            std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o000))
                .expect("sealed folder should be locked");
        });
        let report = delete_the_target_in_the_folder(root.path(), &app, || {});

        finish_the_deletion(&mut app, report);
        std::fs::set_permissions(
            root.path().join("holder/sealed"),
            std::fs::Permissions::from_mode(0o755),
        )
        .expect("sealed folder should be unlocked for its removal");

        assert!(
            !app.generation_rebuild_required,
            "nothing but the removed entry changed, so no scan should be needed"
        );
        assert_eq!(
            app.scan_store.published_generation(),
            Some(ScanGeneration::from_value(1))
        );
    }

    /// A file can gain a link after the plan the reader confirmed and before the executor removes
    /// it: the plan saw one, the executor removes one of two. Whatever names the other link is
    /// outside what the map can say, and the space the removal frees is not what the map would
    /// count: the map is scanned again.
    #[cfg(unix)]
    #[test]
    fn a_file_that_gained_a_link_before_its_removal_sends_the_map_back_to_a_scan() {
        let (root, mut app) = app_listing_a_folder_holding_a_file(|_| {});
        let report = delete_the_target_in_the_folder(root.path(), &app, || {
            std::fs::hard_link(
                root.path().join("holder/target"),
                root.path().join("another-name-for-target"),
            )
            .expect("the second link should be created");
        });

        finish_the_deletion(&mut app, report);

        assert!(
            app.generation_rebuild_required,
            "the file had a second link when it was removed, so the map cannot be updated in place"
        );
        assert_eq!(
            app.scan_store.published_generation(),
            Some(ScanGeneration::initial())
        );
    }

    /// macOS opens nothing it removes, so it cannot show that a file with one link left no link
    /// behind, and every file or link it removes counts as possibly linked: the map is scanned
    /// again after a deletion of a file, as it was before the map could be updated in place at
    /// all. (A folder with no file in it is still described in place:
    /// `a_deletion_whose_report_spilled_is_still_described_in_place`.)
    #[cfg(target_vendor = "apple")]
    #[test]
    fn a_deletion_of_a_file_sends_the_map_back_to_a_scan_on_macos() {
        let (root, mut app) = app_listing_a_folder_holding_a_file(|_| {});
        let report = delete_the_target_in_the_folder(root.path(), &app, || {});
        assert!(
            report.deleted_files_may_have_other_links(),
            "the file had one link, and the executor opened nothing to show that none survived"
        );

        finish_the_deletion(&mut app, report);

        assert!(
            app.generation_rebuild_required,
            "a removed file counts as possibly linked, so the map is scanned again"
        );
        assert_eq!(
            app.scan_store.published_generation(),
            Some(ScanGeneration::initial())
        );
    }

    /// An entry another process unlinked before the executor reached it records `Missing`, though
    /// the deletion removed the rest of its folder, and the link another process made to it
    /// keeps its object alive: whatever names it now is outside what the map can say. The map is
    /// scanned again.
    #[cfg(any(unix, windows))]
    #[test]
    fn an_entry_that_vanished_before_its_removal_sends_the_map_back_to_a_scan() {
        let (root, mut app) = app_listing_a_folder(|holder| {
            let folder = holder.join("folder");
            std::fs::create_dir(&folder).expect("folder fixture should exist");
            std::fs::write(folder.join("kept"), b"kept").expect("kept fixture should exist");
            std::fs::write(folder.join("vanishes"), b"vanishes")
                .expect("vanishing fixture should exist");
        });
        let target = deletion_target_in_the_map(root.path(), &app, "holder/folder");
        let plan = crate::deletion::build_plan(root.path(), target, false)
            .expect("the folder's plan should build");
        // Between the plan and the run, another process gives the file another name and unlinks
        // this one.
        let vanishing = root.path().join("holder/folder/vanishes");
        std::fs::hard_link(&vanishing, root.path().join("another-name"))
            .expect("the other name should be made");
        std::fs::remove_file(&vanishing).expect("the file should be unlinked");
        let report = crate::deletion::execute_plan(
            root.path(),
            plan,
            &std::sync::atomic::AtomicBool::new(false),
            &std::sync::atomic::AtomicBool::new(false),
        );
        assert!(report.target_was_removed());

        finish_the_deletion(&mut app, report);

        assert!(
            app.generation_rebuild_required,
            "an entry that was gone when the executor reached it may live on under another name"
        );
        assert_eq!(
            app.scan_store.published_generation(),
            Some(ScanGeneration::initial())
        );
    }

    /// The executor can fail after it has removed a file: the placeholder that held the file's
    /// name is removed once the file is gone, and when that fails the file is recorded as failed
    /// though it is gone, and the folder that held it is still removed. What became of the file's
    /// other links is then no more known than of a file that was removed whole: the map is
    /// scanned again, not updated in place.
    #[cfg(any(target_os = "linux", target_vendor = "apple"))]
    #[test]
    fn a_file_removed_and_then_failed_on_its_cleanup_sends_the_map_back_to_a_scan() {
        let (root, mut app) = app_listing_a_folder(|holder| {
            let folder = holder.join("folder");
            std::fs::create_dir(&folder).expect("folder fixture should exist");
            std::fs::write(folder.join("file"), b"payload").expect("file fixture should exist");
        });
        let folder = root.path().join("holder/folder");
        let target = deletion_target_in_the_map(root.path(), &app, "holder/folder");
        let plan = crate::deletion::build_plan(root.path(), target, false)
            .expect("the folder's plan should build");
        // The file has another link, outside the folder: whatever the cleanup does, it lives on.
        std::fs::hard_link(folder.join("file"), root.path().join("another-name"))
            .expect("the other link should be made");
        let spoiled = std::cell::Cell::new(false);

        let report = crate::deletion::execute_plan_with_hook_after_inspection(
            root.path(),
            plan,
            |detached| {
                // At the file's turn its name holds the placeholder. Take it away, so that the
                // check that follows the file's removal finds nothing there.
                if !spoiled.get() && folder.join(detached).exists() {
                    std::fs::remove_file(folder.join("file"))
                        .expect("the placeholder should be removed");
                    spoiled.set(true);
                }
            },
        );

        assert!(spoiled.get(), "the placeholder was never taken away");
        assert_eq!(report.failed_entries(), 1, "the file's cleanup failed");
        assert!(report.target_was_removed(), "the folder went all the same");
        finish_the_deletion(&mut app, report);

        assert!(
            app.generation_rebuild_required,
            "a file with another link was removed, so the map cannot be updated in place"
        );
        assert_eq!(
            app.scan_store.published_generation(),
            Some(ScanGeneration::initial())
        );
    }

    /// A deletion large enough to spill keeps a file of its own beside the folder that held its
    /// target on Windows, for as long as its report stays in the history. That file is Excise's,
    /// not the user's: the map that follows the deletion is checked against what the folder holds
    /// of the user's, and is described in place. (Only folders are removed, so that what the
    /// executor can prove of a file's links does not matter here.)
    #[cfg(any(unix, windows))]
    #[test]
    fn a_deletion_whose_report_spilled_is_still_described_in_place() {
        let (root, mut app) = app_listing_a_folder(|holder| {
            let big = holder.join("big");
            std::fs::create_dir(&big).expect("big folder should exist");
            for index in 0..64 {
                std::fs::create_dir(big.join(format!("folder-{index:02}")))
                    .expect("fixture folder should exist");
            }
        });
        let target = deletion_target_in_the_map(root.path(), &app, "holder/big");
        // The deletion's storage and the scan store's are one session's: they share the registry
        // of the files Excise keeps open in the tree.
        let temporary_storage =
            crate::temporary_storage::TemporaryStorage::with_limit_bytes(8 * 1024 * 1024)
                .sharing_private_files_with(&app.scan_store.quota());
        let plan = crate::deletion::build_plan_cancellable_with_temporary_storage(
            root.path(),
            target,
            false,
            &std::sync::atomic::AtomicBool::new(false),
            1,
            &temporary_storage,
        )
        .expect("the folder's plan should spill, not fail");
        let report = crate::deletion::execute_plan(
            root.path(),
            plan,
            &std::sync::atomic::AtomicBool::new(false),
            &std::sync::atomic::AtomicBool::new(false),
        );
        assert!(report.target_was_removed());
        assert!(
            report.entries.is_spilled(),
            "the report must have spilled for this to test anything"
        );

        // The history keeps the report, and so its spill, while the overlay is built.
        finish_the_deletion(&mut app, report);

        assert!(
            !app.generation_rebuild_required,
            "Excise's own file beside the removed folder made the map a scan"
        );
        assert_eq!(
            app.scan_store.published_generation(),
            Some(ScanGeneration::from_value(1))
        );
    }

    /// A directory plan that outgrows its budget spills to temporary storage, and what stays on
    /// disk is not what the history holds in memory. The report of a deletion that ran is always
    /// kept: it must not be charged for entries that are not resident, and so be larger than the
    /// place the history kept for it.
    #[cfg(unix)]
    #[test]
    fn the_report_of_a_deletion_whose_plan_spilled_finds_room_in_the_history() {
        let root = tempfile::tempdir().expect("app root should exist");
        let folder = root.path().join("folder");
        std::fs::create_dir(&folder).expect("folder fixture should exist");
        for index in 0..200 {
            std::fs::write(folder.join(format!("file-{index:03}")), b"payload")
                .expect("fixture file should exist");
        }
        let mut app = App::new(
            TestBackend::new(160, 48),
            root.path().to_path_buf(),
            false,
            false,
            128,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        add_fixture_entry(&mut app, &folder);
        app.finalize_scan();
        // The history is nearly full: its room is a little more than one plan may take.
        app.complete_deletion(report(app.deletion_history_limit - 64 * 1024));
        let budget = app.maximum_deletion_plan_bytes();
        let target = deletion_target_in_the_map(root.path(), &app, "folder");
        let temporary_storage =
            crate::temporary_storage::TemporaryStorage::with_limit_bytes(8 * 1024 * 1024);
        let plan = crate::deletion::build_plan_cancellable_with_temporary_storage(
            root.path(),
            target,
            false,
            &std::sync::atomic::AtomicBool::new(false),
            budget,
            &temporary_storage,
        )
        .expect("the folder's plan should spill, not fail");
        let report = crate::deletion::execute_plan(
            root.path(),
            plan,
            &std::sync::atomic::AtomicBool::new(false),
            &std::sync::atomic::AtomicBool::new(false),
        );
        assert!(report.target_was_removed());

        assert!(app.complete_deletion(report));

        assert_eq!(
            app.deletion_history.len(),
            2,
            "the report of a deletion that ran was dropped"
        );
    }

    #[cfg(any(unix, windows))]
    fn app_after_partial_deletion() -> (tempfile::TempDir, App<TestBackend>, PathBuf) {
        let root = tempfile::tempdir().expect("app root should exist");
        let target_path = root.path().join("target");
        let deleted = target_path.join("deleted");
        let changed = target_path.join("changed");
        std::fs::create_dir(&target_path).expect("target directory should exist");
        std::fs::write(&deleted, b"delete").expect("deleted fixture should exist");
        std::fs::write(&changed, b"old").expect("changed fixture should exist");
        let mut app = App::new(
            TestBackend::new(160, 48),
            root.path().to_path_buf(),
            false,
            false,
            128,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        for path in [target_path.as_path(), deleted.as_path(), changed.as_path()] {
            add_fixture_entry(&mut app, path);
        }
        app.finalize_scan();
        app.start_ui();
        let mut animation = AnimationScheduler::new(false, false, Duration::ZERO);
        draw(&mut app, &mut animation, 0);
        let target_id = app
            .files_in_current_view(0)
            .into_iter()
            .find(|file| file.name == "target")
            .expect("target should be visible")
            .node_id;
        let relative =
            RelativePath::from_path(Path::new("target")).expect("target path should be canonical");
        let entry = app
            .scan_store
            .published()
            .expect("published generation should exist")
            .page_entry(&relative)
            .expect("canonical target query should succeed")
            .expect("target should be in the published generation");
        let target = SnapshotTree::deletion_target_from_entry(
            root.path().to_path_buf(),
            target_id,
            &relative,
            entry,
            false,
        )
        .expect("target should retain its canonical snapshot");
        let plan = crate::deletion::build_plan(root.path(), target, false)
            .expect("target plan should build");
        std::fs::write(&changed, b"changed-after-confirmation")
            .expect("fixture should change after confirmation");
        let report = crate::deletion::execute_plan(
            root.path(),
            plan,
            &std::sync::atomic::AtomicBool::new(false),
            &std::sync::atomic::AtomicBool::new(false),
        );
        assert!(report.deleted_entries() > 0);
        assert!(!report.target_was_removed());
        assert!(app.complete_deletion(report));
        (root, app, target_path)
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn partial_deletion_invalidates_the_published_generation() {
        let (_root, mut app, target_path) = app_after_partial_deletion();
        assert!(
            app.scan_store_available,
            "the last published map must remain available while rebuilding"
        );
        assert_eq!(
            app.files_in_current_view(0)
                .into_iter()
                .map(|file| file.name)
                .collect::<Vec<_>>(),
            vec![std::ffi::OsString::from("target")]
        );
        assert!(
            app.request_deletion().is_none(),
            "a pending rebuild must lock deletion before its worker starts"
        );
        assert!(
            app.begin_generation_rebuild()
                .expect("invalidated generation should start a root rebuild")
        );
        assert!(matches!(app.ui_mode, UiMode::Rebuilding { .. }));
        assert!(
            !app.snapshot_page_is_provisional,
            "rebuild must retain the prior verified page instead of replacing it with a scan field"
        );
        assert!(
            app.request_deletion().is_none(),
            "destructive actions must stay disabled while the displayed page is stale"
        );
        assert_eq!(
            app.handle_enter_action(),
            EnterAction::Drill,
            "the retained page must still permit drill navigation"
        );
        assert_eq!(app.current_folder_path(), target_path);
        assert_eq!(
            app.scan_store.active_generation(),
            Some(ScanGeneration::from_value(1))
        );
        app.suppress_generation_rebuild_restart();
        app.cancel_generation_rebuild()
            .expect("retained rebuild should cancel cleanly");
        assert!(app.generation_rebuild_required);
        assert!(
            !app.begin_generation_rebuild()
                .expect("cancelled rebuild state should remain readable"),
            "an explicit cancellation must suppress automatic rebuild retry"
        );
        assert!(
            app.request_deletion().is_none(),
            "cancelling a rebuild must preserve the deletion lock"
        );
        assert!(matches!(app.ui_mode, UiMode::StaleSnapshot));
        assert!(app.scan_is_uncertain(&RunSummary::default()));
        let mut exported = Vec::new();
        assert!(matches!(
            app.write_scan_report(&RunSummary::default(), &mut exported),
            Err(ReportError::Invariant(message))
                if message.contains("complete current map is unavailable")
        ));
        assert!(exported.is_empty());
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn rebuild_invalidation_cancels_an_outstanding_deletion_confirmation() {
        let root = tempfile::tempdir().expect("app root should exist");
        let target_path = root.path().join("target");
        std::fs::write(&target_path, b"payload").expect("fixture target should exist");
        let mut app = App::new(
            TestBackend::new(160, 48),
            root.path().to_path_buf(),
            false,
            false,
            128,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        add_fixture_entry(&mut app, &target_path);
        app.finalize_scan();
        app.start_ui();
        let mut animation = AnimationScheduler::new(false, false, Duration::ZERO);
        draw(&mut app, &mut animation, 0);
        let target = app
            .request_deletion()
            .expect("verified target should be deletable before invalidation");
        assert!(app.queue_deletion_confirmation(target, false, 1024, Duration::ZERO));
        assert!(app.show_next_deletion_confirmation());
        assert!(matches!(app.ui_mode, UiMode::DeleteConfirm { .. }));

        app.invalidate_snapshot_view_for_live_mutation();

        assert!(app.generation_rebuild_required);
        assert!(!app.deletion_work_summary().has_work());
        assert!(!matches!(app.ui_mode, UiMode::DeleteConfirm { .. }));
        assert!(!app.show_next_deletion_confirmation());
        assert!(app.next_deletion_planning_work().is_none());
        assert!(app.next_deletion_execution_work().is_none());
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn blocked_deletion_dispatch_dismisses_the_confirmation() {
        let root = tempfile::tempdir().expect("app root should exist");
        let target_path = root.path().join("target");
        std::fs::write(&target_path, b"payload").expect("fixture target should exist");
        let mut app = App::new(
            TestBackend::new(160, 48),
            root.path().to_path_buf(),
            false,
            false,
            128,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        add_fixture_entry(&mut app, &target_path);
        app.finalize_scan();
        app.start_ui();
        let mut animation = AnimationScheduler::new(false, false, Duration::ZERO);
        draw(&mut app, &mut animation, 0);
        let target = app
            .request_deletion()
            .expect("verified target should be deletable before the store fails");
        assert!(app.queue_deletion_confirmation(target, false, 1024, Duration::ZERO));
        assert!(app.show_next_deletion_confirmation());
        assert!(matches!(app.ui_mode, UiMode::DeleteConfirm { .. }));

        app.abandon_scan_store_generation_with_failure("test scan-store failure");

        assert!(app.next_deletion_planning_work().is_none());
        assert!(!app.deletion_work_summary().has_work());
        assert!(!matches!(app.ui_mode, UiMode::DeleteConfirm { .. }));
    }

    #[test]
    fn failed_rebuild_publication_hides_the_stale_map_and_locks_deletion() {
        let root = tempfile::tempdir().expect("app root should exist");
        let target_path = root.path().join("target");
        std::fs::write(&target_path, b"payload").expect("fixture target should exist");
        let mut app = App::new(
            TestBackend::new(160, 48),
            root.path().to_path_buf(),
            false,
            false,
            128,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        add_fixture_entry(&mut app, &target_path);
        app.finalize_scan();
        app.start_ui();
        let mut animation = AnimationScheduler::new(false, false, Duration::ZERO);
        draw(&mut app, &mut animation, 0);
        app.require_generation_rebuild_for_test();
        assert!(
            app.begin_generation_rebuild()
                .expect("rebuild should begin before publication fails")
        );
        app.scan_store
            .discard_active()
            .expect("test should make the replacement generation unavailable");

        finish_rebuild(&mut app)
            .expect("failed publication should settle into the unavailable state");

        assert!(matches!(app.ui_mode, UiMode::ScanResultsUnavailable(_)));
        assert!(!app.scan_store_available);
        assert!(app.request_deletion().is_none());

        assert!(app.scan_is_uncertain(&RunSummary::default()));
        let mut exported = Vec::new();
        assert!(matches!(
            app.write_scan_report(&RunSummary::default(), &mut exported),
            Err(ReportError::Invariant(message))
                if message.contains("complete current map is unavailable")
        ));
        assert!(exported.is_empty());
    }

    #[test]
    fn failed_rebuild_publication_preserves_a_theme_picker_preview() {
        let root = tempfile::tempdir().expect("app root should exist");
        let target_path = root.path().join("target");
        std::fs::write(&target_path, b"payload").expect("fixture target should exist");
        let mut app = App::new(
            TestBackend::new(160, 48),
            root.path().to_path_buf(),
            false,
            false,
            128,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        add_fixture_entry(&mut app, &target_path);
        app.finalize_scan();
        app.start_ui();
        app.require_generation_rebuild_for_test();
        assert!(
            app.begin_generation_rebuild()
                .expect("rebuild should begin before publication fails")
        );
        app.open_theme_picker(ThemeId::ExciseDark);
        assert_eq!(app.move_theme_picker(false), Some(ThemeId::ExciseLight));
        app.scan_store
            .discard_active()
            .expect("test should make the replacement generation unavailable");

        finish_rebuild(&mut app).expect("failed publication should preserve the theme picker");

        assert!(matches!(
            &app.ui_mode,
            UiMode::ThemePicker {
                original: ThemeId::ExciseDark,
                selected: ThemeId::ExciseLight,
                return_to: ThemePickerReturn::ScanResultsUnavailable(message),
            } if message.contains("could not build a complete folder map")
        ));
        assert_eq!(
            app.commit_theme_picker(),
            Some((ThemeId::ExciseDark, ThemeId::ExciseLight))
        );
        assert!(matches!(app.ui_mode, UiMode::ScanResultsUnavailable(_)));
        app.show_error("theme preference could not be saved");
        app.normal_mode();
        assert!(matches!(app.ui_mode, UiMode::ScanResultsUnavailable(_)));
    }

    #[test]
    fn rebuild_capacity_failure_keeps_the_scratch_remedy() {
        let root = tempfile::tempdir().expect("app root should exist");
        let mut app = App::new(
            TestBackend::new(80, 24),
            root.path().to_path_buf(),
            false,
            false,
            128,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        app.finalize_scan();
        app.start_ui();
        app.require_generation_rebuild_for_test();
        assert!(
            app.begin_generation_rebuild()
                .expect("rebuild should begin before the store fails")
        );
        app.abandon_scan_store_generation_with_failure(
            "scan store capacity exhausted while publishing",
        );

        finish_rebuild(&mut app)
            .expect("capacity failure should settle into the unavailable state");

        assert!(matches!(
            &app.ui_mode,
            UiMode::ScanResultsUnavailable(message)
                if message.contains("Not enough scratch space")
                    && message.contains("--scan-store-dir")
                    && message.contains("--scan-store-reserve-mib")
                    && message.contains("--scan-store-mib")
        ));
    }

    #[test]
    fn cancelling_a_rebuild_without_a_verified_map_shows_unavailable() {
        let root = tempfile::tempdir().expect("app root should exist");
        let mut app = App::new(
            TestBackend::new(80, 24),
            root.path().to_path_buf(),
            false,
            false,
            128,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        app.abandon_scan_store_generation();
        app.require_generation_rebuild_for_test();
        assert!(
            app.begin_generation_rebuild()
                .expect("rebuild should start without a published map")
        );
        app.suppress_generation_rebuild_restart();

        app.cancel_generation_rebuild()
            .expect("mapless rebuild cancellation should settle safely");

        assert!(matches!(app.ui_mode, UiMode::ScanResultsUnavailable(_)));
        assert!(!app.scan_store_available);
        assert!(app.generation_rebuild_required);
        assert!(
            !app.begin_generation_rebuild()
                .expect("suppressed rebuild retry should remain readable")
        );
    }

    #[test]
    fn invalidated_rebuild_waits_for_its_stale_worker_and_uses_a_new_generation() {
        let root = tempfile::tempdir().expect("app root should exist");
        let mut app = App::new(
            TestBackend::new(80, 24),
            root.path().to_path_buf(),
            false,
            false,
            128,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        app.finalize_scan();
        app.require_generation_rebuild_for_test();
        assert!(
            app.begin_generation_rebuild()
                .expect("first rebuild should begin")
        );
        assert_eq!(
            app.scan_store.active_generation(),
            Some(ScanGeneration::from_value(1))
        );

        app.invalidate_snapshot_view_for_live_mutation();
        assert!(app.generation_rebuild_active);
        assert!(app.generation_rebuild_required);
        finish_rebuild(&mut app)
            .expect("stale rebuild completion should settle without publication");
        assert!(!app.generation_rebuild_active);
        assert!(app.generation_rebuild_required);

        assert!(
            app.begin_generation_rebuild()
                .expect("second rebuild should begin")
        );
        assert_eq!(
            app.scan_store.active_generation(),
            Some(ScanGeneration::from_value(2))
        );
    }

    #[test]
    fn explicit_cancellation_survives_invalidated_rebuild_completion() {
        let root = tempfile::tempdir().expect("app root should exist");
        let mut app = App::new(
            TestBackend::new(80, 24),
            root.path().to_path_buf(),
            false,
            false,
            128,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        app.finalize_scan();
        app.require_generation_rebuild_for_test();
        assert!(
            app.begin_generation_rebuild()
                .expect("rebuild should begin before invalidation")
        );
        app.invalidate_snapshot_view_for_live_mutation();
        app.suppress_generation_rebuild_restart();

        finish_rebuild(&mut app).expect("stale rebuild completion should settle safely");

        assert!(app.generation_rebuild_required);
        assert!(
            !app.begin_generation_rebuild()
                .expect("explicit cancellation should still suppress retry")
        );
    }

    #[test]
    fn snapshot_page_controls_reach_every_concrete_child_without_an_aggregate() {
        let root = tempfile::tempdir().expect("app root should exist");
        let mut app = App::new(
            TestBackend::new(160, 48),
            root.path().to_path_buf(),
            true,
            false,
            128,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        for index in 0..=SNAPSHOT_PAGE_ENTRIES {
            let path = root.path().join(format!("entry-{index:03}"));
            std::fs::write(&path, b"x").expect("fixture entry should exist");
            add_fixture_entry(&mut app, &path);
        }
        app.finalize_scan();
        let first_page = app.files_in_current_view(0);
        assert_eq!(first_page.len(), SNAPSHOT_PAGE_ENTRIES);
        assert!(!first_page.iter().any(|file| file.name == "entry-512"));

        let command = crate::input::handle_keypress(
            &crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::PageDown,
                crossterm::event::KeyModifiers::NONE,
            )),
            &mut app,
        );
        assert!(matches!(command, crate::input::InputCommand::Navigation));
        let second_page = app.files_in_current_view(0);
        assert_eq!(second_page.len(), 1);
        assert_eq!(second_page[0].name, "entry-512");

        let command = crate::input::handle_keypress(
            &crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::PageUp,
                crossterm::event::KeyModifiers::NONE,
            )),
            &mut app,
        );
        assert!(matches!(command, crate::input::InputCommand::Navigation));
        assert_eq!(app.files_in_current_view(0).len(), SNAPSHOT_PAGE_ENTRIES);
    }

    #[test]
    fn snapshot_drill_and_return_use_indexed_pages_without_live_fallback() {
        let root = tempfile::tempdir().expect("app root should exist");
        let folder = root.path().join("folder");
        let child = folder.join("child");
        let sibling = root.path().join("sibling");
        std::fs::create_dir(&folder).expect("fixture folder should exist");
        std::fs::write(&child, b"child").expect("fixture child should exist");
        std::fs::write(&sibling, b"sibling").expect("fixture sibling should exist");
        let mut app = App::new(
            TestBackend::new(160, 48),
            root.path().to_path_buf(),
            true,
            false,
            128,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        app.board
            .change_area(ratatui::layout::Rect::new(0, 0, 160, 48));
        for path in [folder.as_path(), child.as_path(), sibling.as_path()] {
            add_fixture_entry(&mut app, path);
        }
        app.finalize_scan();
        app.start_ui();
        let folder_id = app
            .files_in_current_view(0)
            .into_iter()
            .find(|file| file.name == "folder")
            .expect("folder should be shown on the root page")
            .node_id;
        assert!(app.board.select_node(folder_id));

        app.enter_selected();

        assert_eq!(app.current_folder_path(), folder);
        assert_eq!(
            app.files_in_current_view(0)
                .into_iter()
                .map(|file| file.name)
                .collect::<Vec<_>>(),
            vec![std::ffi::OsString::from("child")]
        );

        assert!(app.go_up());
        assert_eq!(app.current_folder_path(), root.path());
        let mut root_entries = app
            .files_in_current_view(0)
            .into_iter()
            .map(|file| file.name)
            .collect::<Vec<_>>();
        root_entries.sort();
        assert_eq!(
            root_entries,
            vec![
                std::ffi::OsString::from("folder"),
                std::ffi::OsString::from("sibling"),
            ]
        );
    }
    #[test]
    fn snapshot_filter_uses_canonical_page_without_live_arena_fallback() {
        let root = tempfile::tempdir().expect("app root should exist");
        let retained = root.path().join("retained.log");
        let hidden = root.path().join("hidden.tmp");
        std::fs::write(&retained, b"retained").expect("retained fixture should exist");
        std::fs::write(&hidden, b"hidden").expect("hidden fixture should exist");
        let mut app = App::new(
            TestBackend::new(160, 48),
            root.path().to_path_buf(),
            true,
            false,
            128,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        app.board
            .change_area(ratatui::layout::Rect::new(0, 0, 160, 48));
        for path in [&retained, &hidden] {
            add_fixture_entry(&mut app, path);
        }
        app.finalize_scan();
        assert!(
            app.scan_store.published().is_some(),
            "canonical snapshot should publish"
        );
        app.load_snapshot_page(&RelativePath::root())
            .expect("root page should materialize");
        app.start_ui();

        app.open_filter();
        for character in "retained.log".chars() {
            app.push_filter_character(character);
        }
        app.apply_filter();

        assert!(app.visible_tree().has_filter());
        assert_eq!(
            app.files_in_current_view(0)
                .into_iter()
                .map(|file| file.name)
                .collect::<Vec<_>>(),
            vec![std::ffi::OsString::from("retained.log")]
        );
    }

    #[test]
    fn snapshot_filter_retains_canonical_subtree_ancestors() {
        let root = tempfile::tempdir().expect("app root should exist");
        let folder = root.path().join("folder");
        let needle = folder.join("needle.log");
        let other = root.path().join("other.tmp");
        std::fs::create_dir(&folder).expect("fixture folder should exist");
        std::fs::write(&needle, b"needle").expect("needle fixture should exist");
        std::fs::write(&other, b"other").expect("other fixture should exist");
        let mut app = App::new(
            TestBackend::new(160, 48),
            root.path().to_path_buf(),
            true,
            false,
            128,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        for path in [&folder, &needle, &other] {
            add_fixture_entry(&mut app, path);
        }
        app.finalize_scan();
        assert!(
            app.scan_store.published().is_some(),
            "canonical snapshot should publish"
        );
        app.load_snapshot_page(&RelativePath::root())
            .expect("root page should materialize");
        app.start_ui();

        app.open_filter();
        for character in "needle.log".chars() {
            app.push_filter_character(character);
        }
        app.apply_filter();
        assert_eq!(
            app.files_in_current_view(0)
                .into_iter()
                .map(|file| file.name)
                .collect::<Vec<_>>(),
            vec![std::ffi::OsString::from("folder")],
            "the matching descendant must retain its direct ancestor"
        );

        app.load_snapshot_page(
            &RelativePath::from_path(Path::new("folder"))
                .expect("fixture folder should be canonical"),
        )
        .expect("filtered child page should materialize");
        assert_eq!(
            app.files_in_current_view(0)
                .into_iter()
                .map(|file| file.name)
                .collect::<Vec<_>>(),
            vec![std::ffi::OsString::from("needle.log")]
        );
    }

    /// An app whose scan finished over `victim/partAA/partBB/blob-NN.bin`: `top` folders of
    /// `middle` folders of `files` files each, which is enough entries for the published page
    /// index to need more than one block.
    fn nested_app(root: &Path, top: usize, middle: usize, files: usize) -> App<TestBackend> {
        let victim = root.join("victim");
        std::fs::create_dir(&victim).expect("victim fixture should exist");
        let mut paths = vec![victim.clone()];
        for first in 0..top {
            let folder = victim.join(format!("part{first:02}"));
            std::fs::create_dir(&folder).expect("folder fixture should exist");
            paths.push(folder.clone());
            for second in 0..middle {
                let leaf_folder = folder.join(format!("part{second:02}"));
                std::fs::create_dir(&leaf_folder).expect("leaf folder fixture should exist");
                paths.push(leaf_folder.clone());
                for file in 0..files {
                    let path = leaf_folder.join(format!("blob-{file:02}.bin"));
                    std::fs::write(&path, vec![0_u8; 100 + first * 11 + second * 3 + file])
                        .expect("file fixture should exist");
                    paths.push(path);
                }
            }
        }
        let mut app = App::new(
            TestBackend::new(160, 48),
            root.to_path_buf(),
            true,
            false,
            128,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        app.board
            .change_area(ratatui::layout::Rect::new(0, 0, 160, 48));
        for path in &paths {
            add_fixture_entry(&mut app, path);
        }
        app.finalize_scan();
        assert!(
            app.scan_store.published().is_some(),
            "canonical snapshot should publish"
        );
        app.load_snapshot_page(&RelativePath::root())
            .expect("root page should materialize");
        app.start_ui();
        app
    }

    /// Types `text` into the filter prompt, in place of whatever filter is in force, and applies it.
    fn apply_filter_text(app: &mut App<TestBackend>, text: &str) {
        app.open_filter();
        while matches!(&app.ui_mode, UiMode::FilterInput { input, .. } if !input.is_empty()) {
            app.pop_filter_character();
        }
        for character in text.chars() {
            app.push_filter_character(character);
        }
        app.apply_filter();
    }

    fn view_names(app: &App<TestBackend>) -> Vec<String> {
        let mut names = app
            .files_in_current_view(0)
            .into_iter()
            .map(|file| file.name.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    fn open_folder(app: &mut App<TestBackend>, name: &str) {
        let id = app
            .files_in_current_view(0)
            .into_iter()
            .find(|file| file.name == name)
            .expect("the page should list the folder")
            .node_id;
        assert!(app.board.select_node(id));
        app.enter_selected();
    }

    #[test]
    fn a_filter_matching_at_several_depths_filters_the_page_at_the_root_and_in_a_folder() {
        let root = tempfile::tempdir().expect("app root should exist");
        let mut app = nested_app(root.path(), 6, 6, 12);
        let mut animation = AnimationScheduler::new(false, false, Duration::ZERO);

        // Below the root, `part00` is two and three levels down, and only `victim` holds it.
        apply_filter_text(&mut app, "part00");
        assert!(!matches!(app.ui_mode, UiMode::ErrorMessage { .. }));
        assert!(app.visible_tree().has_filter());
        assert_eq!(view_names(&app), ["victim"]);
        draw(&mut app, &mut animation, 0);

        // Inside `victim` it is one of the folders and the name of a folder inside each other one.
        open_folder(&mut app, "victim");
        apply_filter_text(&mut app, "part00");
        assert!(!matches!(app.ui_mode, UiMode::ErrorMessage { .. }));
        assert_eq!(app.current_folder_path(), root.path().join("victim"));
        assert_eq!(
            view_names(&app),
            ["part00", "part01", "part02", "part03", "part04", "part05"]
        );
        draw(&mut app, &mut animation, 1);

        // The folder it leads into lists only the match.
        open_folder(&mut app, "part03");
        assert_eq!(view_names(&app), ["part00"]);
        draw(&mut app, &mut animation, 2);
    }

    /// Makes every run file of the app's scan store unreadable, as a failing disk would.
    fn truncate_scan_store_runs(app: &App<TestBackend>) {
        let mut truncated = 0;
        for session in app.internal_scan_paths() {
            for directory in ["runs", "merge"] {
                let Ok(entries) = std::fs::read_dir(session.join(directory)) else {
                    continue;
                };
                for entry in entries {
                    let path = entry.expect("scan store entry should read").path();
                    if path.extension().is_some_and(|extension| extension == "run") {
                        std::fs::OpenOptions::new()
                            .write(true)
                            .open(&path)
                            .expect("run file should open")
                            .set_len(0)
                            .expect("run file should truncate");
                        truncated += 1;
                    }
                }
            }
        }
        assert!(truncated > 0, "the scan store should hold run files");
    }

    #[test]
    fn a_filter_the_store_cannot_serve_leaves_the_page_and_the_filter_in_place() {
        let root = tempfile::tempdir().expect("app root should exist");
        let mut app = nested_app(root.path(), 2, 2, 3);
        let mut animation = AnimationScheduler::new(false, false, Duration::ZERO);
        apply_filter_text(&mut app, "part00");
        assert_eq!(view_names(&app), ["victim"]);

        truncate_scan_store_runs(&app);
        apply_filter_text(&mut app, "part01");

        assert!(matches!(
            &app.ui_mode,
            UiMode::ErrorMessage { message, .. } if message.starts_with("Could not filter this scan page")
        ));
        // The frame that reports the failure needs a page to draw over.
        draw(&mut app, &mut animation, 0);
        assert!(app.visible_tree().has_filter());
        assert_eq!(view_names(&app), ["victim"]);
        assert_eq!(
            app.snapshot_filter.as_ref().map(|(filter, _)| filter.raw()),
            Some("part00"),
            "the filter in force stays the one the page on screen was built with"
        );
        app.dismiss_transient_modal();
        assert!(matches!(app.ui_mode, UiMode::Normal));
        draw(&mut app, &mut animation, 1);
    }

    #[test]
    fn interactive_report_streams_published_generation_without_live_entries() {
        let root = tempfile::tempdir().expect("app root should exist");
        let entry = root.path().join("entry");
        std::fs::write(&entry, b"payload").expect("fixture entry should exist");
        let mut app = App::new(
            TestBackend::new(80, 24),
            root.path().to_path_buf(),
            false,
            false,
            128,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        add_fixture_entry(&mut app, &entry);
        publish_store_directly(&mut app).expect("canonical generation should publish");

        let mut encoded = Vec::new();
        app.write_scan_report(&RunSummary::default(), &mut encoded)
            .expect("canonical report should serialize");
        let report: crate::report::ScanReportDocument =
            serde_json::from_slice(&encoded).expect("canonical report should deserialize");
        assert_eq!(report.entries.len(), 2);
        let expected_path = crate::native_path::NativePath::new(&entry).encode();
        assert!(report.entries.iter().any(|item| item.path == expected_path));
    }

    #[test]
    fn snapshot_navigation_uses_canonical_pages_without_live_entries() {
        let root = tempfile::tempdir().expect("app root should exist");
        let folder = root.path().join("folder");
        let child = folder.join("child");
        let leaf = child.join("leaf");
        std::fs::create_dir(&folder).expect("folder fixture should exist");
        std::fs::create_dir(&child).expect("child fixture should exist");
        std::fs::write(&leaf, b"leaf").expect("leaf fixture should exist");
        let mut app = App::new(
            TestBackend::new(160, 48),
            root.path().to_path_buf(),
            true,
            false,
            128,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        app.board
            .change_area(ratatui::layout::Rect::new(0, 0, 160, 48));
        // Populate only the canonical source: completed navigation must not
        // need a retained live-model path or a compacted fallback.
        for path in [folder.as_path(), child.as_path(), leaf.as_path()] {
            add_fixture_entry(&mut app, path);
        }
        app.finalize_scan();
        assert!(
            app.scan_store.published().is_some(),
            "canonical snapshot should publish"
        );
        app.load_snapshot_page(&RelativePath::root())
            .expect("root page should materialize");
        app.start_ui();

        let folder_id = app
            .files_in_current_view(0)
            .into_iter()
            .find(|file| file.name == "folder")
            .expect("canonical root page should expose the folder")
            .node_id;
        assert!(app.board.select_node(folder_id));
        app.enter_selected();
        assert_eq!(app.current_folder_path(), folder);
        assert_eq!(
            app.files_in_current_view(0)
                .into_iter()
                .map(|file| file.name)
                .collect::<Vec<_>>(),
            vec![std::ffi::OsString::from("child")]
        );
        let child_id = app
            .files_in_current_view(0)
            .into_iter()
            .find(|file| file.name == "child")
            .expect("canonical folder page should expose its child")
            .node_id;
        assert!(app.board.select_node(child_id));
        app.enter_selected();
        assert_eq!(app.current_folder_path(), child);
        assert!(!matches!(app.ui_mode, UiMode::ErrorMessage { .. }));

        assert!(app.go_up());
        assert_eq!(app.current_folder_path(), folder);

        let child_id = app
            .files_in_current_view(0)
            .into_iter()
            .find(|file| file.name == "child")
            .expect("cancelled page load should restore the parent page")
            .node_id;
        assert!(app.board.select_node(child_id));
        app.enter_selected();
        assert_eq!(app.current_folder_path(), child);
        assert_eq!(
            app.files_in_current_view(0)
                .into_iter()
                .map(|file| file.name)
                .collect::<Vec<_>>(),
            vec![std::ffi::OsString::from("leaf")]
        );
    }

    fn announcing_app(root: &std::path::Path) -> App<TestBackend> {
        let mut app = App::new(
            TestBackend::new(80, 24),
            root.to_path_buf(),
            false,
            false,
            128,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        app.ui_mode = UiMode::Normal;
        app
    }

    #[test]
    fn an_announcement_shows_at_once_over_the_map() {
        let root = tempfile::tempdir().expect("app root should exist");
        let mut app = announcing_app(root.path());

        app.announce(Announcement::Notice("exported".to_string()));

        assert!(
            matches!(&app.ui_mode, UiMode::Notice { message, .. } if message == "exported"),
            "a reader looking at the map is told at once"
        );
        assert!(!app.show_waiting_announcement(), "nothing is left waiting");
    }

    #[test]
    fn an_announcement_never_replaces_a_modal_the_reader_is_deciding_in() {
        type StillShown = fn(&UiMode) -> bool;
        let root = tempfile::tempdir().expect("app root should exist");
        let deciding: [(UiMode, StillShown); 3] = [
            (UiMode::Help, |mode| matches!(mode, UiMode::Help)),
            (
                UiMode::FilterInput {
                    input: "ab".to_string(),
                    error: None,
                },
                |mode| matches!(mode, UiMode::FilterInput { input, .. } if input == "ab"),
            ),
            (
                UiMode::Exiting {
                    work: ExitWork::None,
                    return_to: ThemePickerReturn::Normal,
                },
                |mode| matches!(mode, UiMode::Exiting { .. }),
            ),
        ];
        for (mode, still_shown) in deciding {
            let mut app = announcing_app(root.path());
            app.ui_mode = mode;

            app.announce(Announcement::Error("export failed".to_string()));

            assert!(
                still_shown(&app.ui_mode),
                "the modal stays as the reader left it"
            );
            assert!(!app.show_waiting_announcement(), "it still waits");

            app.ui_mode = UiMode::Normal;
            assert!(app.show_waiting_announcement());
            assert!(
                matches!(&app.ui_mode, UiMode::ErrorMessage { message, .. } if message == "export failed"),
                "it shows once the reader is back at the map"
            );
            assert!(!app.show_waiting_announcement(), "it is shown once");
        }
    }

    #[test]
    fn a_later_announcement_replaces_one_that_still_waits() {
        let root = tempfile::tempdir().expect("app root should exist");
        let mut app = announcing_app(root.path());
        app.ui_mode = UiMode::Help;

        app.announce(Announcement::Notice("first".to_string()));
        app.announce(Announcement::Notice("second".to_string()));
        app.ui_mode = UiMode::Normal;

        assert!(app.show_waiting_announcement());
        assert!(
            matches!(&app.ui_mode, UiMode::Notice { message, .. } if message == "second"),
            "the reader sees the latest result"
        );
    }
}
