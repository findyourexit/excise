use std::fs::Metadata;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use ratatui::backend::Backend;

use crate::animation::AnimationScheduler;
use crate::config::{CustomKeyBindings, KeyPreset};
use crate::deletion::{
    ConfirmationChallenge, DeletionPlan, DeletionReport, confirmation_challenge_for_target,
    current_scan_root_identity, deletion_supported,
};
use crate::error::AppError;
use crate::filter::FilterPattern;
use crate::model::{ModelError, NodeId, SyntheticKind, UnscannedReason};
use crate::native_path::NativeIdentity;
use crate::outcome::RunSummary;
use crate::report::{
    ReportError, scan_is_uncertain, scan_report_state, write_deletion_history_json,
    write_scan_report_json,
};
use crate::state::deletion_work::{DeletionWork, DeletionWorkCommand, DeletionWorkId};
use crate::state::files::FileTree;
use crate::state::tiles::{Board, HALF_ROWS_PER_CELL, Pivot};
use crate::state::{FileToDelete, UiEffects};
use crate::temporary_storage::TemporaryStorage;
use crate::theme::{Theme, ThemeId};
use crate::ui::Display;
use crate::ui::palette::ColorCycle;

const MIB: usize = 1024 * 1024;
const MINIMUM_PLAN_BYTES: usize = 4 * 1024;
const MAX_RETAINED_DELETION_REPORTS: usize = 32;

fn emit_pty_test_marker(label: &str) {
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
    Rescanning {
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
    Rescan(PathBuf),
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
    Rescanning { target: PathBuf },
}

impl ThemePickerReturn {
    fn into_mode(self) -> UiMode {
        match self {
            Self::Loading => UiMode::Loading,
            Self::Normal => UiMode::Normal,
            Self::Rescanning { target } => UiMode::Rescanning { target },
        }
    }
}

impl UiMode {
    #[must_use]
    pub const fn allows_motion(&self) -> bool {
        matches!(self, Self::Loading | Self::Normal | Self::Rescanning { .. })
    }

    #[must_use]
    pub const fn has_modal_attention(&self) -> bool {
        matches!(
            self,
            Self::ThemePicker { .. }
                | Self::DeleteConfirm { .. }
                | Self::ErrorMessage(_)
                | Self::Notice(_)
                | Self::Exiting { .. }
                | Self::WarningMessage
                | Self::Help
        )
    }

    #[must_use]
    pub const fn can_present_deletion_confirmation(&self) -> bool {
        matches!(self, Self::Loading | Self::Normal | Self::Rescanning { .. })
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
    file_tree: FileTree,
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
        Self::new_with_root_identity_and_temporary_storage(
            terminal_backend,
            path_in_filesystem,
            root_identity,
            show_apparent_size,
            disable_delete_confirmation,
            process_memory_mib,
            keymap,
            custom_keys,
            mouse_enabled,
            TemporaryStorage::default(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_with_root_identity_and_temporary_storage(
        terminal_backend: B,
        path_in_filesystem: PathBuf,
        root_identity: NativeIdentity,
        show_apparent_size: bool,
        disable_delete_confirmation: bool,
        process_memory_mib: usize,
        keymap: KeyPreset,
        custom_keys: Option<CustomKeyBindings>,
        mouse_enabled: bool,
        temporary_storage: TemporaryStorage,
    ) -> Result<Self, AppError> {
        let display = Display::new(terminal_backend)?;
        let board = Board::new();
        let file_tree = FileTree::new_with_root_identity_and_temporary_storage(
            path_in_filesystem,
            root_identity,
            show_apparent_size,
            process_memory_mib,
            temporary_storage,
        )
        .map_err(model_error)?;
        Ok(Self::from_parts(
            display,
            board,
            file_tree,
            disable_delete_confirmation,
            keymap,
            custom_keys,
            mouse_enabled,
            process_memory_mib,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn from_parts(
        display: Display<B>,
        board: Board,
        file_tree: FileTree,
        disable_delete_confirmation: bool,
        keymap: KeyPreset,
        custom_keys: Option<CustomKeyBindings>,
        mouse_enabled: bool,
        process_memory_mib: usize,
    ) -> Self {
        Self {
            is_running: true,
            loaded: false,
            board,
            file_tree,
            display,
            ui_mode: UiMode::Loading,
            suspended_ui_mode: None,
            ui_effects: UiEffects::new(),
            delete_confirmation_disabled: disable_delete_confirmation,
            keymap,
            custom_keys,
            mouse_enabled,
            dirty: true,
            deletion_history_bytes: 0,
            deletion_history_limit: process_memory_mib.saturating_mul(MIB) / 8,
            deletion_history: Vec::with_capacity(MAX_RETAINED_DELETION_REPORTS),
            deletion_work: DeletionWork::new(),
            deletion_modal_work_id: None,
            deletion_plan_cancellation_requested: false,
        }
    }

    #[allow(clippy::too_many_arguments)]
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
        self.display.render(
            &self.file_tree,
            &mut self.board,
            &self.ui_mode,
            &self.ui_effects,
            &self.deletion_work,
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
        )?;
        let has_selection = self.board.currently_selected().is_some();
        // Rendering lays out the board and can establish or clear its selection.
        let selection_changed = self.ui_mode.allows_motion() && selection_before != has_selection;
        let background_work = matches!(self.ui_mode, UiMode::Loading | UiMode::Rescanning { .. })
            || self.deletion_work.has_background_activity();
        let animate_focus = self.ui_mode.allows_motion()
            && !background_work
            && has_selection
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
        // Short map feedback keeps its responsive cadence. Persistent chrome is
        // deliberately slower, and execution progress redraws from its own timer.
        animation.set_activity_with_cadence(
            animate_focus
                || animate_modal
                || animate_deletion_checker
                || self.ui_effects.has_deletion_departure(),
            animate_deletion_checker || self.ui_effects.has_deletion_departure(),
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
        self.file_tree.get_current_path()
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
    pub fn refresh_board_from_scan(&mut self) -> bool {
        if self.board.is_transitioning() {
            return false;
        }
        if self.update_board() {
            self.board.settle_geometry();
            self.mark_dirty();
        }
        true
    }

    fn update_board(&mut self) -> bool {
        let folder = self.file_tree.current_id();
        let filter = self.file_tree.filter().map(FilterPattern::raw);
        let files = self
            .file_tree
            .files_in_current_folder(self.board.zoom_level);
        self.board.change_files_for_view(files, folder, filter)
    }

    pub const fn increment_loading_progress_indicator(&mut self) {
        self.ui_effects.increment_loading_progress_indicator();
        self.mark_dirty();
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

    pub fn add_entry_to_base_folder(
        &mut self,
        file_metadata: &Metadata,
        entry_path: PathBuf,
        identity: NativeIdentity,
    ) -> Result<(), AppError> {
        self.file_tree
            .add_primary_entry(file_metadata, &entry_path, identity)
            .map_err(model_error)?;
        self.ui_effects.last_read_path = Some(entry_path);
        Ok(())
    }

    pub(crate) fn add_entry_to_focused_folder(
        &mut self,
        file_metadata: &Metadata,
        entry_path: PathBuf,
        identity: NativeIdentity,
    ) -> Result<(), AppError> {
        self.file_tree
            .add_focused_entry(file_metadata, &entry_path, identity)
            .map_err(model_error)?;
        self.ui_effects.last_read_path = Some(entry_path);
        Ok(())
    }

    pub fn record_unscanned(
        &mut self,
        path: &std::path::Path,
        reason: UnscannedReason,
    ) -> Result<(), AppError> {
        self.file_tree
            .record_primary_unscanned(path, reason)
            .map_err(model_error)
    }

    pub(crate) fn record_focused_unscanned(
        &mut self,
        path: &std::path::Path,
        reason: UnscannedReason,
    ) -> Result<(), AppError> {
        self.file_tree
            .record_focused_unscanned(path, reason)
            .map_err(model_error)
    }

    pub fn complete_directory(
        &mut self,
        path: &std::path::Path,
        expected_identity: Option<&NativeIdentity>,
    ) -> Result<(), AppError> {
        self.file_tree
            .complete_primary_directory(path, expected_identity)
            .map_err(model_error)
    }

    pub(crate) fn complete_focused_directory(
        &mut self,
        path: &std::path::Path,
        expected_identity: Option<&NativeIdentity>,
    ) -> Result<(), AppError> {
        self.file_tree
            .complete_focused_directory(path, expected_identity)
            .map_err(model_error)
    }
    #[must_use]
    pub(crate) fn primary_scan_path_is_stale(&self, path: &std::path::Path) -> bool {
        self.file_tree.primary_scan_path_is_stale(path)
    }

    pub fn finalize_scan(&mut self) -> Result<(), AppError> {
        self.file_tree.finalize().map_err(model_error)
    }

    #[must_use]
    pub fn model_stats(&self) -> (usize, usize, bool) {
        self.file_tree.model_stats()
    }

    #[must_use]
    pub fn internal_scan_paths(&self) -> Vec<PathBuf> {
        self.file_tree.internal_scan_paths()
    }

    #[must_use]
    pub fn identity_for_path(&self, path: &std::path::Path) -> Option<NativeIdentity> {
        self.file_tree.identity_for_path(path)
    }

    #[must_use]
    pub fn identity_count(&self) -> usize {
        self.file_tree.identity_count()
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
            UiMode::Rescanning { target } => ThemePickerReturn::Rescanning {
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

    pub fn handle_enter(&mut self) -> Option<PathBuf> {
        match self.handle_enter_action() {
            EnterAction::Rescan(path) => Some(path),
            EnterAction::None | EnterAction::Drill => None,
        }
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
        match synthetic_kind {
            Some(SyntheticKind::Aggregate) => self
                .file_tree
                .path_for_id(id)
                .map_or(EnterAction::None, EnterAction::Rescan),
            Some(SyntheticKind::Other | SyntheticKind::Shared) => EnterAction::None,
            None => {
                self.enter_selected();
                EnterAction::Drill
            }
        }
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
        if !self.file_tree.enter_folder(target) {
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
        let leaving = self.file_tree.current_id();
        let succeeded = self.file_tree.leave_folder();
        if let Some(zoom_level) = self.board.pop_previous_zoom_level() {
            self.board.set_zoom_index(zoom_level);
        }
        if succeeded {
            self.board.pivot_transition_on(Pivot::Entry(leaving));
        }
        self.render_and_update_board();
        if succeeded && !self.board.select_node(leaving) {
            self.board.select_largest();
        }
        succeeded
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
        let path = self.file_tree.path_for_id(selected.node_id)?;
        let current_folder = self.file_tree.get_current_path();
        if path.file_name() != Some(selected.name.as_os_str())
            || path.parent() != Some(current_folder.as_path())
        {
            return None;
        }
        match self.file_tree.deletion_target_for_path(&path) {
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

    pub(crate) fn deletion_execution_finished(&mut self, work_id: DeletionWorkId) -> bool {
        let finished = self.deletion_work.execution_finished(work_id);
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
        self.deletion_work.status_for_node(node_id).is_some()
            || self.ui_effects.has_deletion_departure_for(node_id)
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
            UiMode::Rescanning { target } => ThemePickerReturn::Rescanning {
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

    #[allow(dead_code)]
    pub fn complete_deletion(&mut self, report: DeletionReport) -> bool {
        self.try_complete_deletion(report).unwrap_or(false)
    }

    pub fn try_complete_deletion(&mut self, report: DeletionReport) -> Result<bool, AppError> {
        let deleted = report.deleted_entries() > 0;
        // A visible departure owns its copied pre-reflow tile until its timer expires.
        if let Err(error) = self.file_tree.try_apply_deletion_report(&report) {
            self.replace_ui_mode(UiMode::ErrorMessage(format!(
                "Deletion accounting failed: {error}"
            )));
            self.mark_dirty();
            return Err(model_error(error));
        }
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
        Ok(deleted)
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
        scan_is_uncertain(&self.file_tree, summary)
    }

    pub fn write_scan_report(
        &self,
        summary: &RunSummary,
        writer: impl Write,
    ) -> Result<(), ReportError> {
        write_scan_report_json(
            &self.file_tree.path_in_filesystem,
            &self.file_tree,
            summary,
            scan_report_state(&self.file_tree, summary, false),
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
            UiMode::Rescanning { target } => ThemePickerReturn::Rescanning {
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

    pub fn begin_rescan(&mut self, target: PathBuf) -> Result<(), AppError> {
        let filter = self.file_tree.filter().cloned();
        self.file_tree
            .begin_rescan(target.clone(), filter)
            .map_err(model_error)?;
        if target != self.current_folder_path() && !self.file_tree.enter_path(&target) {
            self.file_tree.cancel_rescan().map_err(model_error)?;
            return Err(AppError::Invariant(
                "focused scan target disappeared before navigation".to_string(),
            ));
        }
        self.replace_ui_mode(UiMode::Rescanning { target });
        self.render_and_update_board();
        Ok(())
    }

    pub fn finish_rescan(&mut self) -> Result<(), AppError> {
        self.file_tree.finish_rescan().map_err(model_error)?;
        if matches!(self.ui_mode, UiMode::Rescanning { .. }) {
            self.ui_mode = self.navigation_mode();
        } else if let UiMode::ThemePicker { return_to, .. } = &mut self.ui_mode
            && matches!(return_to, ThemePickerReturn::Rescanning { .. })
        {
            *return_to = if self.loaded {
                ThemePickerReturn::Normal
            } else {
                ThemePickerReturn::Loading
            };
        }
        if let Some(suspended) = self.suspended_ui_mode.as_mut() {
            match suspended {
                UiMode::Rescanning { .. } => {
                    *suspended = if self.loaded {
                        UiMode::Normal
                    } else {
                        UiMode::Loading
                    };
                }
                UiMode::ThemePicker { return_to, .. }
                    if matches!(return_to, ThemePickerReturn::Rescanning { .. }) =>
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

    pub fn cancel_rescan(&mut self) -> Result<(), AppError> {
        self.file_tree.cancel_rescan().map_err(model_error)?;
        self.ui_mode = self.navigation_mode();
        self.render_and_update_board();
        Ok(())
    }
    pub fn open_filter(&mut self) {
        let input = self
            .file_tree
            .filter()
            .map_or_else(String::new, |filter| filter.raw().to_string());
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
        if input.is_empty() {
            self.file_tree.set_filter(None);
            self.render_and_update_board();
            return;
        }
        match FilterPattern::new(input.clone()) {
            Ok(filter) => {
                self.file_tree.set_filter(Some(filter));
                self.board.reset_selected_index();
                self.render_and_update_board();
            }
            Err(error) => {
                self.replace_ui_mode(UiMode::FilterInput {
                    input,
                    error: Some(error.to_string()),
                });
                self.mark_dirty();
            }
        }
    }

    pub fn increment_failed_to_read(&mut self) {
        self.file_tree.increment_failed_to_read();
    }

    pub fn zoom_in(&mut self) {
        let files = self
            .file_tree
            .files_in_current_folder(self.board.zoom_level.saturating_add(1));
        self.board.zoom_in(files);
        self.mark_dirty();
    }

    pub fn zoom_out(&mut self) {
        let offset = self.board.zoom_level.saturating_sub(1);
        let files = self.file_tree.files_in_current_folder(offset);
        self.board.zoom_out(files);
        self.mark_dirty();
    }

    pub fn reset_zoom(&mut self) {
        let files = self.file_tree.files_in_current_folder(0);
        self.board.reset_zoom(files);
        self.mark_dirty();
    }
}

#[allow(clippy::needless_pass_by_value)]
fn model_error(error: ModelError) -> AppError {
    AppError::Model(error.to_string())
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
        app.add_entry_to_base_folder(&metadata, path.to_path_buf(), identity)
            .expect("fixture entry should be added");
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

    #[cfg(any(unix, windows))]
    #[test]
    fn loading_scan_accepts_visible_incomplete_directory_deletion() {
        let root = tempfile::tempdir().expect("app root should exist");
        let directory = root.path().join("directory");
        let child = directory.join("child");
        std::fs::create_dir(&directory).expect("fixture directory should be created");
        std::fs::write(&child, b"payload").expect("fixture child should be created");
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
        add_fixture_entry(&mut app, &directory);
        add_fixture_entry(&mut app, &child);
        app.render_and_update_board();

        let command = crate::input::handle_keypress(
            &crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Backspace,
                crossterm::event::KeyModifiers::NONE,
            )),
            &mut app,
        );
        let crate::input::InputCommand::RequestDeletion(target) = command else {
            panic!("Backspace should open immediate deletion confirmation for a visible directory");
        };
        assert_eq!(target.file_type, crate::state::tiles::FileType::Folder);
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn confirmed_directory_deletion_stays_selectable_but_cannot_open() {
        let root = tempfile::tempdir().expect("app root should exist");
        let directory = root.path().join("directory");
        let child = directory.join("child");
        std::fs::create_dir(&directory).expect("fixture directory should be created");
        std::fs::write(&child, b"payload").expect("fixture child should be created");
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
        app.board
            .change_area(ratatui::layout::Rect::new(0, 0, 80, 24));
        add_fixture_entry(&mut app, &directory);
        add_fixture_entry(&mut app, &child);
        app.render_and_update_board();

        let target = app
            .request_deletion()
            .expect("selected directory should produce a deletion target");
        let selected_node = target.node_id;
        assert_eq!(target.file_type, FileType::Folder);
        assert!(app.queue_deletion_confirmation(target, false, 1024, Duration::ZERO));
        assert!(app.show_next_deletion_confirmation());
        let (work_id, target) = app
            .arm_and_confirm_deletion_target()
            .expect("directory confirmation should arm background planning");
        assert!(app.queue_confirmed_deletion(work_id, target, Duration::ZERO));

        let command = crate::input::handle_keypress(
            &crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Enter,
                crossterm::event::KeyModifiers::NONE,
            )),
            &mut app,
        );
        assert!(matches!(command, crate::input::InputCommand::None));
        assert_eq!(app.current_folder_path(), root.path());
        assert_eq!(
            app.board.currently_selected().map(|tile| tile.node_id),
            Some(selected_node)
        );
    }

    #[test]
    fn loading_selection_does_not_keep_focus_chrome_animating() {
        let root = tempfile::tempdir().expect("app root should exist");
        let entry = root.path().join("entry");
        std::fs::write(&entry, b"payload").expect("fixture entry should be created");
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
        add_fixture_entry(&mut app, &entry);
        app.board
            .change_area(ratatui::layout::Rect::new(0, 0, 80, 24));
        app.render_and_update_board();
        let mut animation = AnimationScheduler::new(false, false, Duration::ZERO);

        draw(&mut app, &mut animation, 0);
        app.mark_dirty();
        draw(&mut app, &mut animation, 200);

        assert!(matches!(app.ui_mode, UiMode::Loading));
        assert!(app.board.currently_selected().is_some());
        assert_eq!(animation.next_frame_at(), None);
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
        app.complete_directory(root.path(), None)
            .expect("fixture root should complete");
        app.finalize_scan().expect("fixture tree should finalize");
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
    fn entering_a_folder_mid_tween_uses_its_rendered_rectangle_as_the_pivot() {
        let root = tempfile::tempdir().expect("app root should exist");
        let folder = root.path().join("folder");
        let first = folder.join("first");
        let second = folder.join("second");
        std::fs::create_dir(&folder).expect("fixture folder should be created");
        std::fs::write(&first, b"first").expect("first fixture should be written");
        std::fs::write(&second, b"second").expect("second fixture should be written");
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
        for path in [folder.as_path(), first.as_path(), second.as_path()] {
            add_fixture_entry(&mut app, path);
        }
        for path in [folder.as_path(), root.path()] {
            app.complete_directory(path, None)
                .expect("fixture directory should complete");
        }
        app.finalize_scan().expect("fixture tree should finalize");
        app.board
            .change_area(ratatui::layout::Rect::new(0, 0, 120, 24));
        app.render_and_update_board();
        app.board.advance_geometry(Duration::ZERO, true);

        let settled_rendered = app
            .board
            .selected_rendered_rect()
            .expect("a settled map should render its selection");
        let settled_target = app
            .board
            .selected_rect()
            .expect("a settled map should have a selection");
        assert_eq!(settled_rendered, settled_target);

        app.board
            .change_area(ratatui::layout::Rect::new(8, 3, 140, 32));
        app.board.advance_geometry(Duration::ZERO, false);
        app.board.advance_geometry(Duration::from_millis(80), false);
        assert!(app.board.is_transitioning());
        let rendered = app
            .board
            .selected_rendered_rect()
            .expect("the selected entry should still be rendered mid-tween");
        let target = app
            .board
            .selected_rect()
            .expect("the selected entry should have a target rectangle");
        assert_ne!(rendered, target);

        app.enter_selected();
        app.board.advance_geometry(Duration::from_millis(81), false);

        let child_origin = app
            .board
            .selected_rendered_rect()
            .expect("opening the fixture folder should render its selected entry");
        assert_eq!(child_origin, rendered);
        assert_ne!(child_origin, target);
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
    fn dismissing_exit_returns_to_an_active_rescan() {
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
        app.ui_mode = UiMode::Rescanning {
            target: target.clone(),
        };

        app.prompt_exit(ExitWork::None);
        app.dismiss_exit();

        assert!(matches!(
            &app.ui_mode,
            UiMode::Rescanning { target: current } if current == &target
        ));
    }
}
