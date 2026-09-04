use std::ffi::OsStr;
use std::fs::Metadata;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use ratatui::backend::Backend;

use crate::animation::AnimationScheduler;
use crate::config::{CustomKeyBindings, KeyPreset};
use crate::deletion::{
    ConfirmationChallenge, DeletionPlan, DeletionReport, current_scan_root_identity,
    deletion_supported,
};
use crate::error::AppError;
use crate::filter::FilterPattern;
use crate::model::{ModelError, NodeState, SyntheticKind, UnscannedReason};
use crate::native_path::{NativeIdentity, identity_for, safe_display_os_str};
use crate::outcome::RunSummary;
use crate::report::{
    ReportError, scan_is_uncertain, scan_report_state, write_deletion_history_json,
    write_scan_report_json,
};
use crate::state::files::FileTree;
use crate::state::tiles::{Board, FileType, Pivot};
use crate::state::{FileToDelete, UiEffects};
use crate::temporary_storage::TemporaryStorage;
use crate::theme::Theme;
use crate::ui::Display;
use crate::ui::palette::ColorCycle;

const MIB: usize = 1024 * 1024;
const MINIMUM_PLAN_BYTES: usize = 4 * 1024;

fn directory_target_was_replaced(path: &Path, expected: &NativeIdentity) -> bool {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return true;
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return true;
    }
    let Ok(Some(actual)) = identity_for(path, &metadata) else {
        return true;
    };
    actual.file_id != expected.file_id || actual.reparse_point != expected.reparse_point
}
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
    PlanningDeletion(Box<FileToDelete>),
    DeleteConfirm {
        plan: Option<Box<DeletionPlan>>,
        input: String,
    },
    Deleting {
        planned_entries: u64,
        completed: Arc<AtomicU64>,
        stopping: bool,
    },
    DeletionCancel {
        planned_entries: u64,
        completed: Arc<AtomicU64>,
    },
    DeletionResult {
        report: Arc<DeletionReport>,
    },
    ErrorMessage(String),
    Notice(String),
    Exiting {
        save_preferences: bool,
    },
    WarningMessage,
}

pub(crate) enum DeletionReplanResult {
    Ready(Box<FileToDelete>),
    Missing,
}

impl UiMode {
    #[must_use]
    pub const fn allows_motion(&self) -> bool {
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
    board: Board,
    file_tree: FileTree,
    display: Display<B>,
    ui_effects: UiEffects,
    delete_confirmation_disabled: bool,
    deletion_history: Vec<Arc<DeletionReport>>,
    deletion_history_bytes: usize,
    deletion_history_limit: usize,
    deletion_replan: Option<FileToDelete>,
    deletion_enter_armed: bool,
    preferences_dirty: bool,
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
            ui_effects: UiEffects::new(),
            delete_confirmation_disabled: disable_delete_confirmation,
            keymap,
            custom_keys,
            mouse_enabled,
            preferences_dirty: false,
            dirty: true,
            deletion_history_bytes: 0,
            deletion_history_limit: process_memory_mib.saturating_mul(MIB) / 8,
            deletion_history: Vec::new(),
            deletion_enter_armed: false,
            deletion_replan: None,
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
        if !self.dirty {
            return Ok(false);
        }
        let full_screen_size = self.display.size()?;
        if full_screen_size.width < 32 || full_screen_size.height < 8 {
            self.ui_mode = UiMode::ScreenTooSmall;
        }
        let selection_before = self.board.currently_selected().is_some();
        self.display.render(
            &self.file_tree,
            &mut self.board,
            &self.ui_mode,
            &self.ui_effects,
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
            self.deletion_enter_armed,
            reduced_motion,
        )?;
        let has_selection = self.board.currently_selected().is_some();
        // Rendering lays out the board and can establish or clear its selection.
        let selection_changed =
            matches!(&self.ui_mode, UiMode::Normal) && selection_before != has_selection;
        let animate_focus = matches!(&self.ui_mode, UiMode::Normal)
            && has_selection
            && ColorCycle::can_animate(theme.focus);
        // Keep the render loop awake during deletion so the progress counter
        // in UiMode::Deleting stays live. The animation scheduler fires at
        // ~30fps when activity is true; without this the screen is static until
        // DeletionFinished arrives.
        animation.set_activity(animate_focus || self.ui_effects.deletion_in_progress);
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
        // Only move to Normal if no deletion flow is active; a deletion started
        // during loading must not be silently discarded when the scan finishes.
        if matches!(
            self.ui_mode,
            UiMode::PlanningDeletion(_)
                | UiMode::DeleteConfirm { .. }
                | UiMode::Deleting { .. }
                | UiMode::DeletionCancel { .. }
                | UiMode::DeletionResult { .. }
        ) {
            self.mark_dirty();
        } else {
            // The arm is only valid inside PlanningDeletion. Any mode not in the
            // preserved list (e.g. Exiting when q was pressed during planning) has
            // no valid deletion target, so clear it.
            self.deletion_enter_armed = false;
            self.ui_mode = UiMode::Normal;
            self.render_and_update_board();
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
            .add_entry(file_metadata, &entry_path, identity)
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
            .record_unscanned(path, reason)
            .map_err(model_error)
    }

    pub fn complete_directory(
        &mut self,
        path: &std::path::Path,
        expected_identity: Option<&NativeIdentity>,
    ) -> Result<(), AppError> {
        self.file_tree
            .complete_directory(path, expected_identity)
            .map_err(model_error)
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
        self.deletion_enter_armed = false;
        if !matches!(self.ui_mode, UiMode::Loading | UiMode::Normal) {
            self.ui_mode = if self.loaded {
                UiMode::Normal
            } else {
                UiMode::Loading
            };
            self.mark_dirty();
        }
    }

    pub fn show_warning_modal(&mut self) {
        if self.get_file_to_delete().is_some() {
            self.ui_mode = UiMode::WarningMessage;
            self.mark_dirty();
        }
    }

    pub fn prompt_exit(&mut self) {
        self.ui_mode = UiMode::Exiting {
            save_preferences: self.preferences_dirty,
        };
        emit_pty_test_marker("QUIT_PROMPT");
        self.mark_dirty();
    }

    pub const fn exit(&mut self) {
        self.is_running = false;
    }

    pub fn handle_enter(&mut self) -> Option<PathBuf> {
        if !self.board.has_selected_index() {
            self.board.move_to_largest_folder();
        }
        let selected = self
            .board
            .currently_selected()
            .map(|tile| (tile.node_id, tile.synthetic_kind));
        match selected {
            Some((id, Some(SyntheticKind::Aggregate))) => self.file_tree.path_for_id(id),
            Some((_, Some(SyntheticKind::Other))) => Some(self.file_tree.get_current_path()),
            Some((_, Some(SyntheticKind::Shared))) => None,
            _ => {
                self.enter_selected();
                None
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
        let pivot = self
            .board
            .selected_rendered_geometry()
            .or_else(|| self.board.selected_geometry());
        // Nothing is recorded and nothing moves unless the folder actually opens:
        // pressing Enter on a file used to push a history entry and arm a
        // transition that the next unrelated refresh would then play back.
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
            // The folder being left becomes the pivot: its contents collapse into
            // the rectangle it occupies in the parent, and the parent grows out of
            // the same rectangle. The cursor lands on it too, so the reader comes
            // back out standing where they went in.
            self.board.pivot_transition_on(Pivot::Entry(leaving));
        }
        self.render_and_update_board();
        if succeeded && !self.board.select_node(leaving) {
            self.board.select_largest();
        }
        succeeded
    }

    #[must_use]
    pub fn get_file_to_delete(&self) -> Option<FileToDelete> {
        let currently_selected = self.board.currently_selected()?;
        let kind = self.file_tree.node_kind(currently_selected.node_id)?;
        let synthetic = kind.is_synthetic();
        let full_path = self.file_tree.path_for_id(currently_selected.node_id)?;
        // Guard against NodeId reuse during loading: verify the model's current
        // leaf name AND parent directory for this NodeId match what the board
        // tile was displaying. A same-basename entry in a different directory
        // would pass a name-only check; the parent check catches that case.
        let current_folder = self.file_tree.get_current_path();
        if full_path.file_name() != Some(currently_selected.name.as_os_str())
            || full_path.parent() != Some(current_folder.as_path())
        {
            return None;
        }
        let relative = full_path
            .strip_prefix(&self.file_tree.path_in_filesystem)
            .ok()?;
        Some(FileToDelete {
            node_id: currently_selected.node_id,
            synthetic,
            path_in_filesystem: self.file_tree.path_in_filesystem.clone(),
            path_to_file: relative.iter().map(OsStr::to_os_string).collect(),
            file_type: currently_selected.file_type,
            num_descendants: currently_selected.descendants,
            size: currently_selected.size,
            expected_snapshot: self.file_tree.entry_snapshot(currently_selected.node_id)?,
            reviewed_entries: Vec::new(),
        })
    }

    pub fn prompt_file_deletion(&mut self) -> Option<FileToDelete> {
        if self
            .display
            .size()
            .is_ok_and(|area| area.width < 50 || area.height < 15)
        {
            self.show_error("Resize to at least 50 x 15 before permanent deletion");
            return None;
        }
        if self.remaining_deletion_history_bytes() < MINIMUM_PLAN_BYTES {
            self.show_error("Deletion history memory is full; export or restart before deleting");
            return None;
        }
        if !deletion_supported() {
            self.show_error("Permanent deletion is unavailable on this platform");
            return None;
        }
        let mut file_to_delete = self.get_file_to_delete()?;
        if file_to_delete.synthetic {
            self.show_error("Synthetic aggregate nodes cannot be deleted");
            return None;
        }
        if self.file_tree.node_state(file_to_delete.node_id) != Some(NodeState::Complete) {
            self.show_error("Deletion requires a complete, materialized entry");
            return None;
        }
        // For file and link targets, build a per-entry reviewed list used by
        // the planning worker to detect model-vs-filesystem drift. For directory
        // targets, skip this: the worker performs its own live filesystem walk
        // and uses that as the authoritative plan, matching diskonaut's simpler
        // deletion model. The directory identity check in the worker (via
        // validate_model_snapshot) still ensures we target the right directory.
        if file_to_delete.file_type != FileType::Folder {
            file_to_delete.reviewed_entries = match self
                .file_tree
                .reviewed_subtree(file_to_delete.node_id, self.maximum_deletion_plan_bytes())
            {
                Ok(entries) => entries,
                Err(error) => {
                    self.show_error(error.to_string());
                    return None;
                }
            };
        }
        self.deletion_replan = None;
        self.ui_mode = UiMode::PlanningDeletion(Box::new(file_to_delete.display_copy()));
        self.mark_dirty();
        Some(file_to_delete)
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

    /// Called when the deletion worker finishes planning. Returns the plan when
    /// it should be immediately forwarded to the worker for revalidation
    /// (Enter was pre-armed and the challenge is single-key).
    #[must_use]
    pub fn deletion_plan_ready(
        &mut self,
        target_node_id: crate::model::NodeId,
        result: Result<Box<DeletionPlan>, String>,
    ) -> Option<Box<DeletionPlan>> {
        // Always consume the arm flag; it no longer matters once we leave planning.
        let enter_armed = std::mem::replace(&mut self.deletion_enter_armed, false);
        if !matches!(
            &self.ui_mode,
            UiMode::PlanningDeletion(current) if current.node_id == target_node_id
        ) {
            return None;
        }
        match result {
            Ok(plan) => {
                // If Enter was pre-armed and the challenge is a single-key type,
                // skip the confirm dialog and proceed straight to revalidation.
                if enter_armed
                    && matches!(
                        plan.challenge,
                        ConfirmationChallenge::ConfirmFile | ConfirmationChallenge::ReducedGuard
                    )
                {
                    let planned_entries = plan.planned_entries();
                    let completed = Arc::new(AtomicU64::new(0));
                    self.ui_mode = UiMode::Deleting {
                        planned_entries,
                        completed,
                        stopping: false,
                    };
                    self.ui_effects.deletion_in_progress = true;
                    self.mark_dirty();
                    return Some(plan);
                }
                self.ui_mode = UiMode::DeleteConfirm {
                    plan: Some(plan),
                    input: String::new(),
                };
                self.mark_dirty();
                None
            }
            Err(error) => {
                self.ui_mode = UiMode::ErrorMessage(error);
                self.mark_dirty();
                None
            }
        }
    }

    pub fn push_confirmation_character(&mut self, character: char) {
        if character.is_control() {
            return;
        }
        if let UiMode::DeleteConfirm { plan, input } = &mut self.ui_mode {
            let maximum = plan
                .as_deref()
                .map_or(0, |plan| plan.challenge.expected_input().chars().count());
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
    pub fn take_confirmed_deletion_plan(&mut self) -> Option<DeletionPlan> {
        let confirmed = matches!(
            &self.ui_mode,
            UiMode::DeleteConfirm { plan: Some(plan), input }
                if input == plan.challenge.expected_input()
        );
        if !confirmed {
            return None;
        }
        let mode = std::mem::replace(&mut self.ui_mode, UiMode::Normal);
        let UiMode::DeleteConfirm {
            plan: Some(plan), ..
        } = mode
        else {
            return None;
        };
        let planned_entries = plan.planned_entries();
        let completed = Arc::new(AtomicU64::new(0));
        self.ui_mode = UiMode::Deleting {
            planned_entries,
            completed,
            stopping: false,
        };
        self.ui_effects.deletion_in_progress = true;
        self.mark_dirty();
        Some(*plan)
    }

    pub fn take_deletion_replan(&mut self) -> Option<FileToDelete> {
        self.deletion_replan.take()
    }

    pub fn prompt_deletion_cancel(&mut self) {
        if let UiMode::Deleting {
            planned_entries,
            ref completed,
            stopping: false,
        } = self.ui_mode
        {
            let completed = Arc::clone(completed);
            self.ui_mode = UiMode::DeletionCancel {
                planned_entries,
                completed,
            };
            self.mark_dirty();
        }
    }

    pub(crate) fn begin_deletion_replan(
        &mut self,
        target_node_id: crate::model::NodeId,
        plan: DeletionPlan,
    ) -> Result<Option<PathBuf>, AppError> {
        if plan.target.node_id != target_node_id {
            self.show_error("Deletion validation returned an unexpected target");
            return Ok(None);
        }
        self.begin_deletion_replan_target(plan.target)
    }

    pub(crate) fn begin_pending_deletion_replan(
        &mut self,
        target_node_id: crate::model::NodeId,
    ) -> Result<Option<PathBuf>, AppError> {
        let target = match &self.ui_mode {
            UiMode::PlanningDeletion(target) if target.node_id == target_node_id => {
                (**target).clone()
            }
            _ => return Ok(None),
        };
        self.begin_deletion_replan_target(target)
    }

    fn begin_deletion_replan_target(
        &mut self,
        mut target: FileToDelete,
    ) -> Result<Option<PathBuf>, AppError> {
        let target_path = target.full_path();
        let target_is_directory =
            target.expected_snapshot.kind == crate::model::NodeKind::Directory;
        let target_was_replaced = target_is_directory
            && target
                .expected_snapshot
                .identity
                .as_ref()
                .is_none_or(|expected| directory_target_was_replaced(&target_path, expected));
        let rescan_target = if target_was_replaced {
            target_path
                .parent()
                .map_or_else(|| target.path_in_filesystem.clone(), Path::to_path_buf)
        } else if target_is_directory {
            target_path.clone()
        } else {
            target_path
                .parent()
                .map_or_else(|| target.path_in_filesystem.clone(), Path::to_path_buf)
        };
        target.reviewed_entries.clear();
        // The plan was stale: the target may have changed since the user armed Enter.
        // Clear the arm so a replacement identity requires fresh confirmation.
        self.deletion_enter_armed = false;
        self.begin_rescan(rescan_target.clone())?;
        self.deletion_replan = Some(target);
        self.ui_effects.deletion_in_progress = false;
        Ok(Some(rescan_target))
    }

    pub(crate) fn rebuild_deletion_replan(&mut self) -> Option<DeletionReplanResult> {
        let stale = self.take_deletion_replan()?;
        let path = stale.full_path();
        let mut target = match self.file_tree.deletion_target_for_path(&path) {
            Ok(target) => target,
            Err(crate::model::ModelError::InvalidPath(_)) => {
                return Some(DeletionReplanResult::Missing);
            }
            Err(error) => {
                self.show_error(format!("Deletion rescan could not refresh target: {error}"));
                return None;
            }
        };
        target.reviewed_entries = match self
            .file_tree
            .reviewed_subtree(target.node_id, self.maximum_deletion_plan_bytes())
        {
            Ok(entries) => entries,
            Err(error) => {
                self.show_error(format!("Deletion rescan could not review target: {error}"));
                return None;
            }
        };
        self.ui_mode = UiMode::PlanningDeletion(Box::new(target.display_copy()));
        self.mark_dirty();
        Some(DeletionReplanResult::Ready(Box::new(target)))
    }

    /// Defers a stale `PlanningDeletion` target for later without staging a
    /// model rescan. Used when the deletion plan came back stale but the
    /// initial scan is still active; starting a competing rescan would corrupt
    /// untagged scan-event routing. The stored replan is picked up by
    /// `rebuild_deletion_replan` once `ScanFinished` fires.
    pub(crate) fn defer_pending_deletion_replan(&mut self, target_node_id: crate::model::NodeId) {
        let target = match &self.ui_mode {
            UiMode::PlanningDeletion(t) if t.node_id == target_node_id => (**t).clone(),
            _ => return,
        };
        self.deletion_replan = Some(target);
        self.deletion_enter_armed = false;
        self.ui_effects.deletion_in_progress = false;
        // Return to Loading; the initial scan is still running and the user
        // should see that, not a stale planning overlay.
        self.ui_mode = UiMode::Loading;
        self.mark_dirty();
    }

    /// Defers a stale revalidation replan without staging a model rescan.
    /// Used when revalidation returned stale but the initial scan is still active.
    pub(crate) fn defer_deletion_replan_from_plan(
        &mut self,
        target_node_id: crate::model::NodeId,
        plan: DeletionPlan,
    ) {
        if plan.target.node_id != target_node_id {
            self.show_error("Deletion validation returned an unexpected target");
            return;
        }
        let mut target = plan.target;
        target.reviewed_entries.clear();
        self.deletion_replan = Some(target);
        self.deletion_enter_armed = false;
        self.ui_effects.deletion_in_progress = false;
        self.ui_mode = UiMode::Loading;
        self.mark_dirty();
    }

    pub(crate) fn complete_missing_deletion(&mut self) {
        self.deletion_enter_armed = false;
        self.ui_effects.deletion_in_progress = false;
        self.ui_mode = if self.loaded {
            UiMode::Normal
        } else {
            UiMode::Loading
        };
        self.render_and_update_board();
    }

    pub fn resume_deletion(&mut self, stopping: bool) {
        if let UiMode::DeletionCancel {
            planned_entries,
            ref completed,
        } = self.ui_mode
        {
            let completed = Arc::clone(completed);
            self.ui_mode = UiMode::Deleting {
                planned_entries,
                completed,
                stopping,
            };
            self.mark_dirty();
        }
    }

    #[allow(dead_code)]
    pub fn complete_deletion(&mut self, report: DeletionReport) -> bool {
        self.try_complete_deletion(report).unwrap_or(false)
    }

    pub fn try_complete_deletion(&mut self, report: DeletionReport) -> Result<bool, AppError> {
        self.ui_effects.deletion_in_progress = false;
        let deleted = report.deleted_entries() > 0;
        if let Err(error) = self.file_tree.try_apply_deletion_report(&report) {
            self.ui_mode = UiMode::ErrorMessage(format!("Deletion accounting failed: {error}"));
            self.mark_dirty();
            return Err(model_error(error));
        }
        let report = Arc::new(report);
        if report.estimated_bytes <= self.remaining_deletion_history_bytes() {
            self.deletion_history_bytes = self
                .deletion_history_bytes
                .saturating_add(report.estimated_bytes);
            self.deletion_history.push(report.clone());
        }
        self.ui_mode = UiMode::DeletionResult { report };
        self.board.reset_selected_index();
        self.render_and_update_board();
        Ok(deleted)
    }

    #[must_use]
    pub fn deletion_challenge(&self) -> Option<(&DeletionPlan, &str)> {
        let UiMode::DeleteConfirm {
            plan: Some(plan),
            input,
        } = &self.ui_mode
        else {
            return None;
        };
        Some((plan, input))
    }

    pub fn open_help(&mut self) {
        self.ui_mode = UiMode::Help;
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
        self.ui_mode = UiMode::Notice(message.into());
        self.mark_dirty();
    }

    pub fn preferences_changed(&mut self) {
        self.preferences_dirty = true;
        self.mark_dirty();
    }

    pub fn preferences_saved(&mut self) {
        self.preferences_dirty = false;
    }

    #[must_use]
    pub const fn preferences_dirty(&self) -> bool {
        self.preferences_dirty
    }
    #[must_use]
    pub fn confirmation_is_single_key(&self) -> bool {
        self.deletion_challenge().is_some_and(|(plan, _)| {
            matches!(
                plan.challenge,
                ConfirmationChallenge::ConfirmFile | ConfirmationChallenge::ReducedGuard
            )
        })
    }

    /// Sets the deletion enter arm flag when in `PlanningDeletion` mode for a
    /// challenge that will be a single-key confirm (files, or any entry when
    /// guardrails are reduced). No-ops for directories with the name-typing
    /// challenge and for any entry with a deceptive name (`TypePhrase` challenge).
    pub fn arm_deletion_enter(&mut self) {
        if let UiMode::PlanningDeletion(target) = &self.ui_mode {
            // Deceptive names always produce a TypePhrase challenge; arming is
            // meaningless and the UI would display "Armed" while the plan later
            // opens a typing prompt. Mirror the challenge_for() deceptive check.
            let leaf_deceptive = target
                .path_to_file
                .last()
                .is_some_and(|name| safe_display_os_str(name).deceptive);
            if !leaf_deceptive
                && (self.delete_confirmation_disabled || target.file_type != FileType::Folder)
            {
                self.deletion_enter_armed = true;
                self.mark_dirty();
            }
        }
    }

    /// Whether deletion enter is currently armed.
    #[must_use]
    pub const fn deletion_enter_is_armed(&self) -> bool {
        self.deletion_enter_armed
    }

    /// Returns the live completion counter stored in `UiMode::Deleting`, if
    /// currently in that mode. The runtime clones this and passes it to the
    /// deletion worker so both threads share the same atomic.
    #[must_use]
    pub fn deletion_progress_counter(&self) -> Option<Arc<AtomicU64>> {
        if let UiMode::Deleting { ref completed, .. } = self.ui_mode {
            Some(Arc::clone(completed))
        } else {
            None
        }
    }

    /// Confirms the active deletion plan, auto-filling the expected single
    /// character for `ConfirmFile`/`ReducedGuard` challenges so that Enter
    /// works as a primary confirmation key alongside the typed keys.
    pub fn arm_and_confirm_deletion_plan(&mut self) -> Option<DeletionPlan> {
        if let UiMode::DeleteConfirm {
            plan: Some(plan),
            input,
        } = &mut self.ui_mode
            && matches!(
                plan.challenge,
                ConfirmationChallenge::ConfirmFile | ConfirmationChallenge::ReducedGuard
            )
            && input.is_empty()
        {
            input.push('y');
        }
        self.take_confirmed_deletion_plan()
    }

    pub fn show_error(&mut self, message: impl Into<String>) {
        self.deletion_enter_armed = false;
        self.ui_effects.deletion_in_progress = false;
        self.ui_mode = UiMode::ErrorMessage(message.into());
        self.mark_dirty();
    }

    pub fn normal_mode(&mut self) {
        self.deletion_replan = None;
        self.deletion_enter_armed = false;
        self.ui_effects.deletion_in_progress = false;
        self.ui_mode = if self.loaded {
            UiMode::Normal
        } else {
            UiMode::Loading
        };
        self.render_and_update_board();
    }

    pub fn begin_rescan(&mut self, target: PathBuf) -> Result<(), AppError> {
        let filter = self.file_tree.filter().cloned();
        self.file_tree
            .begin_rescan(target.clone(), filter)
            .map_err(model_error)?;
        self.ui_mode = UiMode::Rescanning { target };
        self.render_and_update_board();
        Ok(())
    }

    pub fn finish_rescan(&mut self) -> Result<(), AppError> {
        self.file_tree.finish_rescan().map_err(model_error)?;
        self.ui_mode = UiMode::Normal;
        self.render_and_update_board();
        Ok(())
    }

    pub fn cancel_rescan(&mut self) -> Result<(), AppError> {
        self.file_tree.cancel_rescan().map_err(model_error)?;
        self.deletion_replan = None;
        self.deletion_enter_armed = false;
        self.ui_mode = UiMode::Normal;
        self.render_and_update_board();
        Ok(())
    }

    pub fn open_filter(&mut self) {
        let input = self
            .file_tree
            .filter()
            .map_or_else(String::new, |filter| filter.raw().to_string());
        self.ui_mode = UiMode::FilterInput { input, error: None };
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
        let mode = std::mem::replace(&mut self.ui_mode, UiMode::Normal);
        let UiMode::FilterInput { input, .. } = mode else {
            return;
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
                self.ui_mode = UiMode::FilterInput {
                    input,
                    error: Some(error.to_string()),
                };
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
    fn resizing_from_too_small_to_a_map_arms_focus_activity_after_layout() {
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
            animation.is_running(),
            "the new selection must activate focused chrome immediately"
        );
        assert!(animation.next_frame_at().is_some());
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
            entries: Vec::new(),
            soft_cancelled: false,
            precise: true,
            estimated_bytes,
        }
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
    }
    #[cfg(unix)]
    #[test]
    fn confirmed_plan_is_deferred_to_worker_revalidation() {
        use std::os::unix::fs::MetadataExt as _;
        use std::time::UNIX_EPOCH;

        let root = tempfile::tempdir().expect("app root should exist");
        let path = root.path().join("target");
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
            path_in_filesystem: root.path().to_path_buf(),
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
        let plan = build_plan(root.path(), target, false).expect("deletion plan should build");
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
        app.ui_mode = UiMode::DeleteConfirm {
            input: plan.challenge.expected_input().to_string(),
            plan: Some(Box::new(plan)),
        };
        std::fs::write(&path, b"replacement").expect("target should change");

        let confirmed = app
            .take_confirmed_deletion_plan()
            .expect("confirmed plan should be handed to the worker");
        assert_eq!(confirmed.entries.len(), 1);
        assert!(matches!(app.ui_mode, UiMode::Deleting { .. }));
        assert!(app.take_deletion_replan().is_none());
    }
}
