#[cfg(test)]
use std::fs::Metadata;
use std::io::{self, Write};
#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;
#[cfg(test)]
use std::time::UNIX_EPOCH;

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
use crate::report::{
    ReportError, canonical_scan_report_state, write_canonical_scan_report_json,
    write_deletion_history_json,
};
use crate::scan_coordinator::{
    RelativePath, ScanGeneration, SchedulerSnapshot, SessionCoordinator, WorkCompletion, WorkLease,
};
#[cfg(test)]
use crate::scan_store::identity_observation::IdentityObservation;
use crate::scan_store::page::{PageCursor, PageRequest};
#[cfg(test)]
use crate::scan_store::path_reducer::{Coverage, PathEntryKind, PathObservation, SummaryMetrics};
use crate::scan_store::run_file::SealedRun;
use crate::scan_store::session::{ScanInputRunFactory, ScanStore, ScanStoreError};
use crate::scan_store::storage::ScanStoreStorage;
use crate::state::deletion_work::{DeletionWork, DeletionWorkCommand, DeletionWorkId};
use crate::state::files::snapshot_page_cache::SnapshotPageCache;
use crate::state::files::snapshot_tree::SnapshotTree;
use crate::state::files::tree_view::TreeView;
use crate::state::tiles::{Board, HALF_ROWS_PER_CELL, Pivot};
use crate::state::{FileToDelete, UiEffects};
use crate::temporary_storage::TemporaryStorage;
use crate::theme::{Theme, ThemeId};
use crate::ui::Display;
use crate::ui::palette::ColorCycle;

const MIB: usize = 1024 * 1024;
const MINIMUM_PLAN_BYTES: usize = 4 * 1024;
const MAX_RETAINED_DELETION_REPORTS: usize = 32;
const SNAPSHOT_PAGE_ENTRIES: usize = 512;
const MAX_SNAPSHOT_PAGE_HISTORY: usize = 32;

pub(crate) fn emit_pty_test_marker(label: &str) {
    if std::env::var_os("EXCISE_PTY_TEST_MARKERS").is_none() {
        return;
    }
    let mut stdout = io::stdout();
    let _ = writeln!(stdout, "\n__EXCISE_PTY_{label}__");
    let _ = stdout.flush();
}

pub enum UiMode {
    Loading,
    Normal,
    Rebuilding {
        target: PathBuf,
    },
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
    ErrorMessage(String),
    ScanResultsUnavailable(String),
    Notice(String),
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
        completed: Arc<AtomicU64>,
        pending: usize,
    },
    Stopping {
        planned_entries: u64,
        completed: Arc<AtomicU64>,
    },
}

#[derive(Clone)]
/// The picker can overlay a live scan without discarding its underlying mode.
pub enum ThemePickerReturn {
    Loading,
    Normal,
    Rebuilding { target: PathBuf },
}

impl ThemePickerReturn {
    fn into_mode(self) -> UiMode {
        match self {
            Self::Loading => UiMode::Loading,
            Self::Normal => UiMode::Normal,
            Self::Rebuilding { target } => UiMode::Rebuilding { target },
        }
    }
}

impl UiMode {
    #[must_use]
    pub const fn allows_motion(&self) -> bool {
        matches!(self, Self::Loading | Self::Normal | Self::Rebuilding { .. })
    }

    #[must_use]
    pub const fn has_modal_attention(&self) -> bool {
        matches!(
            self,
            Self::ThemePicker { .. }
                | Self::DeleteConfirm { .. }
                | Self::ErrorMessage(_)
                | Self::ScanResultsUnavailable(_)
                | Self::Notice(_)
                | Self::Exiting { .. }
                | Self::WarningMessage
                | Self::Help
        )
    }

    #[must_use]
    pub const fn can_present_deletion_confirmation(&self) -> bool {
        matches!(self, Self::Loading | Self::Normal | Self::Rebuilding { .. })
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
    scan_store: ScanStore,
    session_coordinator: SessionCoordinator,
    snapshot_page_cache: Option<SnapshotPageCache>,
    snapshot_page_is_provisional: bool,
    snapshot_filter: Option<(FilterPattern, RelativePath)>,
    scan_store_available: bool,
    generation_rebuild_required: bool,
    scan_store_failure: Option<String>,
    generation_rebuild_active: bool,
    generation_rebuild_target: Option<RelativePath>,
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
    /// Runtime owns whether loading animation is enabled; direct app fixtures stay static.
    loading_animation_enabled: bool,
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
        let scan_store = ScanStore::new_with_storage(ScanGeneration::initial(), scan_store_storage)
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
        scan_store: ScanStore,
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
            generation_rebuild_target: None,
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
            .expect("every app state except the unavailable-result surface retains a page")
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
            && ColorCycle::can_animate(theme.focus);

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
    /// retargeting the map tween while the scanner is active.
    pub fn refresh_board_from_scan(&mut self) -> Result<bool, AppError> {
        if self.board.is_transitioning() {
            return Ok(false);
        }
        if !self.uses_provisional_scan_page() {
            if self.update_board() {
                self.board.settle_geometry();
                self.mark_dirty();
            }
            return Ok(true);
        }
        let selected_relative = self.board.currently_selected().and_then(|tile| {
            self.snapshot_page_cache
                .as_ref()
                .and_then(|cache| cache.current().relative_for_id(tile.node_id))
                .cloned()
        });
        let had_selection = selected_relative.is_some();
        let folder = self
            .snapshot_page_cache
            .as_ref()
            .map_or_else(RelativePath::root, |cache| {
                cache.current().current_relative().clone()
            });
        self.load_provisional_snapshot_page(&folder)?;
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
        Ok(true)
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
        self.scan_store_available
            && self.scan_store.active_generation().is_some()
            && (!self.loaded || self.generation_rebuild_active)
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
            .append_observation_batch(
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
            .expect("fixture entry should append to the canonical scan store");
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

    fn show_scan_results_unavailable(&mut self, message: impl Into<String>) {
        self.snapshot_filter = None;
        self.replace_ui_mode(UiMode::ScanResultsUnavailable(message.into()));
        self.mark_dirty();
    }

    fn publish_scan_store(&mut self) {
        let current = self
            .snapshot_page_cache
            .as_ref()
            .map_or_else(RelativePath::root, |cache| {
                cache.current().current_relative().clone()
            });
        if !self.publish_scan_store_generation() {
            let message = self.scan_results_unavailable_message();
            self.show_scan_results_unavailable(message);
            return;
        }
        if self.scan_store.is_summary_only() {
            self.scan_store_available = false;
            self.scan_store_failure = Some("scan store capacity exhausted".to_string());
            self.show_scan_results_unavailable(self.scan_results_unavailable_message());
            return;
        }
        self.snapshot_page_cache = None;
        self.snapshot_page_is_provisional = false;
        self.snapshot_page_history.clear();
        self.snapshot_filter = None;
        if let Err(error) = self
            .load_snapshot_page(&current)
            .or_else(|_| self.load_snapshot_page(&RelativePath::root()))
        {
            self.scan_store_available = false;
            self.scan_store_failure = Some(error.to_string());
            self.show_scan_results_unavailable(
                "Excise could not open the completed folder map. Run it again.",
            );
        }
    }

    fn publish_scan_store_generation(&mut self) -> bool {
        if !self.scan_store_available {
            return false;
        }
        if let Err(error) = self.scan_store.publish() {
            self.abandon_scan_store_generation_with_failure(error.to_string());
            return false;
        }
        true
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
            if after.is_some() {
                return Err(AppError::Model(
                    "live scan pages cannot be paginated before the scan finishes".to_string(),
                ));
            }
            return self.load_provisional_snapshot_page(folder);
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

    fn load_provisional_snapshot_page(&mut self, folder: &RelativePath) -> Result<(), AppError> {
        let request = PageRequest::first(folder.clone(), SNAPSHOT_PAGE_ENTRIES);
        let page = self
            .scan_store
            .provisional_page(&request)
            .map_err(scan_store_error)?;
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
        self.snapshot_filter = None;
        self.snapshot_page_history.clear();
        if let Err(error) = self.scan_store.discard_active() {
            self.scan_store_failure
                .get_or_insert_with(|| error.to_string());
        }
        self.scan_store_available = false;
        self.generation_rebuild_required = true;
        self.reset_loading_snapshot();
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
    /// A complete removal of its exact target can safely publish an overlay
    /// that drops that prefix. Any partial outcome invalidates the immutable
    /// snapshot instead of presenting a fabricated mixture of pre- and
    /// post-deletion facts.
    fn reconcile_generation_after_deletion(&mut self, report: &DeletionReport) {
        if report.deleted_entries() == 0 {
            return;
        }
        if !self.scan_store_available || !report.target_was_removed() {
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
        let Ok(next_generation) = self.scan_store.next_generation() else {
            self.invalidate_snapshot_view_for_live_mutation();
            return;
        };
        let current = self
            .snapshot_page_cache
            .as_ref()
            .map_or_else(RelativePath::root, |cache| {
                cache.current().current_relative().clone()
            });

        let fallback = relative_parent(&prefix);
        let desired = if current.starts_with(&prefix) {
            fallback.clone()
        } else {
            current
        };
        self.invalidate_cached_pages_for_overlay(&prefix);
        if self
            .scan_store
            .begin_overlay_generation(next_generation, &prefix)
            .and_then(|()| self.scan_store.publish())
            .is_err()
        {
            self.invalidate_snapshot_view_for_live_mutation();
            return;
        }
        if self
            .session_coordinator
            .advance_generation(next_generation)
            .is_err()
        {
            self.invalidate_snapshot_view_for_live_mutation();
            return;
        }
        if self.load_snapshot_page(&desired).is_err() && self.load_snapshot_page(&fallback).is_err()
        {
            self.invalidate_snapshot_view_for_live_mutation();
        }
    }

    /// Starts a fresh root generation after a partial deletion invalidated its
    /// prior immutable snapshot. Returns false when no rebuild is pending.
    pub(crate) fn begin_generation_rebuild(&mut self) -> Result<bool, AppError> {
        if !self.generation_rebuild_required || self.generation_rebuild_active {
            return Ok(false);
        }
        let next_generation = self
            .scan_store
            .next_generation()
            .map_err(scan_store_error)?;
        self.scan_store
            .begin_generation(next_generation)
            .map_err(scan_store_error)?;
        self.session_coordinator
            .advance_generation(next_generation)
            .map_err(|error| AppError::Worker(error.to_string()))?;
        self.scan_store_available = true;
        self.scan_store_failure = None;
        self.generation_rebuild_active = true;
        self.generation_rebuild_target = Some(RelativePath::root());
        self.generation_rebuild_required = false;
        let target = self.scan_root.clone();
        self.ui_effects.reset_loading_activity();
        self.board.arm_scan_reveal();
        self.replace_ui_mode(UiMode::Rebuilding { target });
        self.render_and_update_board();
        Ok(true)
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
    pub fn start_ui(&mut self) {
        self.loaded = true;
        if matches!(self.ui_mode, UiMode::Loading) {
            self.ui_mode = UiMode::Normal;
            self.render_and_update_board();
        } else if let UiMode::ThemePicker { return_to, .. } = &mut self.ui_mode {
            if matches!(return_to, ThemePickerReturn::Loading) {
                *return_to = ThemePickerReturn::Normal;
            }
            self.render_and_update_board();
        } else {
            self.mark_dirty();
        }
        if let Some(suspended) = self.suspended_ui_mode.as_mut() {
            match suspended {
                UiMode::Loading => *suspended = UiMode::Normal,
                UiMode::ThemePicker { return_to, .. }
                    if matches!(return_to, ThemePickerReturn::Loading) =>
                {
                    *return_to = ThemePickerReturn::Normal;
                }
                _ => {}
            }
        }
        emit_pty_test_marker("SCAN_COMPLETE");
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

    pub(crate) fn cancel_primary_scan(&mut self) -> Result<(), AppError> {
        self.scan_store
            .cancel_active()
            .map_err(|error| AppError::Model(error.to_string()))
    }

    pub(crate) fn finalize_scan(&mut self) {
        self.publish_scan_store();
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

    /// Admits worker-sealed runs only after the scanner coordinator validated
    /// the corresponding work lease.
    pub(crate) fn admit_scan_input_runs(&mut self, lease: &WorkLease, runs: Vec<SealedRun>) {
        if runs.is_empty() || !self.scan_store_available {
            return;
        }
        if !self.scan_store_available {
            return;
        }
        for run in runs {
            if let Err(error) = self.scan_store.accept_leased_input_run(lease, run) {
                self.abandon_scan_store_generation_with_failure(error.to_string());
                return;
            }
        }
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
        if self.loaded {
            UiMode::Normal
        } else {
            UiMode::Loading
        }
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
        emit_pty_test_marker("QUIT_PROMPT");
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
        if !self.begin_snapshot_page_navigation(&folder, None, SnapshotPageHistoryChange::Reset) {
            return;
        }
        self.board.record_current_zoom_level();
        if let Some(pivot) = pivot {
            self.board.pivot_transition_on_geometry(pivot);
        }
        self.board.reset_zoom_index();
        self.board.reset_selected_index();
        self.render_and_update_board();
    }

    pub fn go_up(&mut self) -> bool {
        let (leaving, leaving_relative, parent) = self
            .snapshot_page_cache
            .as_ref()
            .map(|cache| {
                let snapshot = cache.current();
                (
                    snapshot.current_id(),
                    snapshot.current_relative().clone(),
                    snapshot.parent_folder(),
                )
            })
            .expect("every navigable app state retains a snapshot page");
        let Some(parent) = parent else {
            return false;
        };
        if !self.begin_snapshot_page_navigation(&parent, None, SnapshotPageHistoryChange::Reset) {
            return false;
        }
        if let Some(zoom_level) = self.board.pop_previous_zoom_level() {
            self.board.set_zoom_index(zoom_level);
        }
        self.board.pivot_transition_on(Pivot::Entry(leaving));
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
        true
    }

    /// Returns the identity-bound target represented by the rendered tile.
    pub(crate) fn request_deletion(&mut self) -> Option<FileToDelete> {
        if self
            .display
            .size()
            .is_ok_and(|area| area.width < 50 || area.height < 15)
        {
            self.show_error("Resize to at least 50 x 15 before permanent deletion");
            return None;
        }
        if self.deletion_history.len() >= MAX_RETAINED_DELETION_REPORTS
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

    #[must_use]
    pub const fn remaining_deletion_history_bytes(&self) -> usize {
        self.deletion_history_limit
            .saturating_sub(self.deletion_history_bytes)
    }

    #[must_use]
    pub const fn maximum_deletion_plan_bytes(&self) -> usize {
        self.remaining_deletion_history_bytes() / 2
    }

    pub(crate) fn queue_deletion_confirmation(
        &mut self,
        target: FileToDelete,
        reduced_guardrails: bool,
        maximum_bytes: usize,
        now: Duration,
    ) -> bool {
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
        let command = self.deletion_work.next_planning_command();
        self.sync_deletion_work_summary();
        command
    }

    #[must_use]
    pub(crate) fn next_deletion_execution_work(&mut self) -> Option<DeletionWorkCommand> {
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
        node_id: NodeId,
        deleted_entries: u64,
        now: Duration,
    ) -> bool {
        if !self.ui_mode.allows_motion() || self.board.is_list_layout() {
            return false;
        }
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
        self.ui_effects
            .begin_deletion_departure(tile.clone(), now, duration);
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
        self.snapshot_page_cache
            .as_ref()
            .and_then(|cache| cache.current().path_for_id(node_id))
            .is_some_and(|path| self.deletion_work.status_for_path(&path).is_some())
    }

    #[must_use]
    pub fn deletion_work_summary(&self) -> crate::state::DeletionWorkSummary {
        self.deletion_work.summary()
    }

    pub(crate) fn show_next_deletion_confirmation(&mut self) -> bool {
        if !self.ui_mode.can_present_deletion_confirmation() {
            return false;
        }
        let return_to = match &self.ui_mode {
            UiMode::Loading => ThemePickerReturn::Loading,
            UiMode::Normal => ThemePickerReturn::Normal,
            UiMode::Rebuilding { target } => ThemePickerReturn::Rebuilding {
                target: target.clone(),
            },
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
        if self.deletion_history.len() < MAX_RETAINED_DELETION_REPORTS
            && report.estimated_bytes <= self.remaining_deletion_history_bytes()
        {
            self.deletion_history_bytes = self
                .deletion_history_bytes
                .saturating_add(report.estimated_bytes);
            self.deletion_history.push(report);
        }
        // The streamed report may remove the current node. Board replacement keeps
        // a surviving actionable selection or deliberately clears an empty view.
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

    #[must_use]
    pub fn scan_is_uncertain(&self, summary: &RunSummary) -> bool {
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

    pub fn write_deletion_history(&self, writer: impl Write) -> Result<(), ReportError> {
        write_deletion_history_json(&self.deletion_history, writer)
    }

    pub fn clear_deletion_history(&mut self) {
        self.deletion_history.clear();
        self.deletion_history_bytes = 0;
    }

    pub fn show_notice(&mut self, message: impl Into<String>) {
        self.replace_ui_mode(UiMode::Notice(message.into()));
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
        self.replace_ui_mode(UiMode::ErrorMessage(message.into()));
        self.mark_dirty();
    }

    pub fn normal_mode(&mut self) {
        self.replace_ui_mode(self.navigation_mode());
        self.render_and_update_board();
    }

    pub fn finish_generation_rebuild(&mut self) -> Result<(), AppError> {
        let target = self.generation_rebuild_target.take().ok_or_else(|| {
            AppError::Invariant("scan rebuild finished without an active revision".to_string())
        })?;
        let current = self.snapshot_page_cache.as_ref().map_or_else(
            || target.clone(),
            |cache| cache.current().current_relative().clone(),
        );
        let rebuild_invalidated = !self.scan_store_available && self.generation_rebuild_required;
        self.generation_rebuild_active = false;
        self.generation_rebuild_required = rebuild_invalidated;
        if rebuild_invalidated {
            // A deletion discarded this generation while its worker was still
            // draining. Its terminal event merely releases that stale worker;
            // the owner will start a strictly newer root generation next.
        } else if self.scan_store_available && self.scan_store.publish().is_ok() {
            self.snapshot_page_cache = None;
            self.snapshot_page_is_provisional = false;
            self.snapshot_page_history.clear();
            self.snapshot_filter = None;
            if self.load_snapshot_page(&current).is_err() {
                let _ = self.load_snapshot_page(&target);
            }
        } else {
            self.scan_store
                .discard_active()
                .map_err(|error| AppError::Model(error.to_string()))?;
            self.scan_store_available = false;
        }
        if matches!(self.ui_mode, UiMode::Rebuilding { .. }) {
            self.ui_mode = self.navigation_mode();
        } else if let UiMode::ThemePicker { return_to, .. } = &mut self.ui_mode
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
                UiMode::ThemePicker { return_to, .. }
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
        Ok(())
    }

    pub fn cancel_generation_rebuild(&mut self) -> Result<(), AppError> {
        let rebuild_cancelled = self.generation_rebuild_active && self.uses_provisional_scan_page();
        if self.generation_rebuild_active {
            self.scan_store
                .cancel_active()
                .map_err(|error| AppError::Model(error.to_string()))?;
            self.generation_rebuild_active = false;
            self.generation_rebuild_target = None;
            if rebuild_cancelled {
                self.snapshot_filter = None;
                self.snapshot_page_history.clear();
                self.scan_store_available = false;
                self.generation_rebuild_required = true;
                self.reset_loading_snapshot();
            }
        }
        self.board.disarm_scan_reveal();
        self.ui_mode = self.navigation_mode();
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
        self.snapshot_filter = filter.map(|filter| (filter, folder.clone()));
        self.snapshot_page_cache = None;
        self.snapshot_page_history.clear();
        if let Err(error) = self.load_snapshot_page(&folder) {
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

fn relative_parent(path: &RelativePath) -> RelativePath {
    RelativePath::from_components(path.components()[..path.depth().saturating_sub(1)].to_vec())
        .expect("a prefix of a valid scan path remains valid")
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use ratatui::backend::{Backend, TestBackend};

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

        assert!(
            app.refresh_board_from_scan()
                .expect("active scan page should refresh")
        );
        assert!(matches!(app.ui_mode, UiMode::Loading));
        assert_eq!(
            app.files_in_current_view(0)
                .into_iter()
                .map(|file| file.name)
                .collect::<Vec<_>>(),
            vec![std::ffi::OsString::from("first")]
        );

        add_fixture_entry(&mut app, &second);
        assert!(
            app.refresh_board_from_scan()
                .expect("later canonical facts should refresh the active page")
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
        app.refresh_board_from_scan()
            .expect("root provisional page should refresh");
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

        assert_eq!(app.current_folder_path(), folder);
        assert_eq!(
            app.files_in_current_view(0)
                .into_iter()
                .map(|file| file.name)
                .collect::<Vec<_>>(),
            vec![std::ffi::OsString::from("leaf")]
        );
        assert!(app.go_up());
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
        app.refresh_board_from_scan()
            .expect("concrete preview should refresh");
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
            .append_observation_batch(
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
            .expect("inferred fixture should append");
        app.refresh_board_from_scan()
            .expect("inferred preview should refresh");
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
            UiMode::Notice(message)
                if message == "Wait for this item to receive a verified scan preview before deleting"
        ));
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

        assert_eq!(app.scan_store.active_run_counts(), Some((1, 0)));
        app.scan_store
            .publish()
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

        app.clear_deletion_history();
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
    #[cfg(any(unix, windows))]
    #[test]
    fn completed_deletion_republishes_the_snapshot_without_the_removed_target() {
        let root = tempfile::tempdir().expect("app root should exist");
        let target_path = root.path().join("target");
        let survivor_path = root.path().join("survivor");
        std::fs::write(&target_path, b"target").expect("target fixture should exist");
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
        for path in [target_path.as_path(), survivor_path.as_path()] {
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
        app.snapshot_filter = Some((
            FilterPattern::new("target").expect("filter should compile"),
            relative.clone(),
        ));
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
        assert!(app.complete_deletion(report));
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

    #[cfg(any(unix, windows))]
    #[test]
    fn partial_deletion_invalidates_the_published_generation() {
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
        assert!(!app.scan_store_available);
        assert!(
            app.begin_generation_rebuild()
                .expect("invalidated generation should start a root rebuild")
        );
        assert_eq!(
            app.scan_store.active_generation(),
            Some(ScanGeneration::from_value(1))
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
        app.finish_generation_rebuild()
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
        app.scan_store
            .publish()
            .expect("canonical snapshot should publish");
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
        app.scan_store
            .publish()
            .expect("canonical snapshot should publish");
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
        app.scan_store
            .publish()
            .expect("canonical generation should publish");

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
        app.scan_store
            .publish()
            .expect("canonical snapshot should publish");
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
        assert!(!matches!(app.ui_mode, UiMode::ErrorMessage(_)));

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
}
