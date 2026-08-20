use std::ffi::OsStr;
use std::fs::Metadata;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ratatui::backend::Backend;

use crate::animation::AnimationScheduler;
use crate::config::{CustomKeyBindings, KeyPreset};
use crate::deletion::{ConfirmationChallenge, DeletionPlan, DeletionReport, deletion_supported};
use crate::error::AppError;
use crate::filter::FilterPattern;
use crate::model::{ModelError, NodeState, SyntheticKind, UnscannedReason};
use crate::native_path::NativeIdentity;
use crate::outcome::RunSummary;
use crate::report::{
    ReportError, scan_is_uncertain, scan_report_state, write_deletion_history_json,
    write_scan_report_json,
};
use crate::state::files::FileTree;
use crate::state::tiles::Board;
use crate::state::{FileToDelete, UiEffects};
use crate::theme::Theme;
use crate::ui::Display;
const MIB: usize = 1024 * 1024;
const MINIMUM_PLAN_BYTES: usize = 4 * 1024;

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
        stopping: bool,
    },
    DeletionCancel {
        planned_entries: u64,
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
    #[allow(clippy::too_many_arguments)]
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
        let display = Display::new(terminal_backend)?;
        let board = Board::new();
        let file_tree = FileTree::new(path_in_filesystem, show_apparent_size, process_memory_mib)
            .map_err(model_error)?;
        Ok(Self {
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
        })
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
            self.mouse_enabled,
            self.delete_confirmation_disabled,
            reduced_motion,
        )?;
        self.dirty = false;
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

    pub fn render_and_update_board(&mut self) {
        let folder = self.file_tree.current_id();
        let filter = self.file_tree.filter().map(FilterPattern::raw);
        let files = self
            .file_tree
            .files_in_current_folder(self.board.zoom_level);
        self.board.change_files_for_view(files, folder, filter);
        self.mark_dirty();
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
        self.ui_mode = UiMode::Normal;
        self.loaded = true;
        self.render_and_update_board();
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

    pub fn complete_directory(&mut self, path: &std::path::Path) -> Result<(), AppError> {
        self.file_tree.complete_directory(path).map_err(model_error)
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
    pub fn identity_count(&self) -> usize {
        self.file_tree.identity_count()
    }

    pub fn reset_ui_mode(&mut self) {
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
        self.board.record_current_index_and_zoom_level();
        if let Some(tile) = self.board.currently_selected()
            && self.file_tree.enter_folder(tile.node_id)
        {
            self.board.reset_zoom_index();
            self.board.reset_selected_index();
            self.render_and_update_board();
        }
    }

    pub fn go_up(&mut self) -> bool {
        let succeeded = self.file_tree.leave_folder();
        if let Some((index, zoom_level)) = self.board.pop_previous_index_and_zoom_level() {
            if let Some(index) = index {
                self.board.set_selected_index(index);
            }
            self.board.set_zoom_index(zoom_level);
        }
        self.render_and_update_board();
        succeeded
    }

    #[must_use]
    pub fn get_file_to_delete(&self) -> Option<FileToDelete> {
        let currently_selected = self.board.currently_selected()?;
        let kind = self.file_tree.node_kind(currently_selected.node_id)?;
        let synthetic = kind.is_synthetic();
        let full_path = self.file_tree.path_for_id(currently_selected.node_id)?;
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

    pub fn deletion_plan_ready(
        &mut self,
        target_node_id: crate::model::NodeId,
        result: Result<Box<DeletionPlan>, String>,
    ) {
        if !matches!(
            &self.ui_mode,
            UiMode::PlanningDeletion(current) if current.node_id == target_node_id
        ) {
            return;
        }
        match result {
            Ok(plan) => {
                self.ui_mode = UiMode::DeleteConfirm {
                    plan: Some(plan),
                    input: String::new(),
                };
            }
            Err(error) => self.ui_mode = UiMode::ErrorMessage(error),
        }
        self.mark_dirty();
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
        self.ui_mode = UiMode::Deleting {
            planned_entries,
            stopping: false,
        };
        self.ui_effects.deletion_in_progress = true;
        self.mark_dirty();
        Some(*plan)
    }

    pub fn prompt_deletion_cancel(&mut self) {
        if let UiMode::Deleting {
            planned_entries,
            stopping: false,
        } = self.ui_mode
        {
            self.ui_mode = UiMode::DeletionCancel { planned_entries };
            self.mark_dirty();
        }
    }

    pub fn resume_deletion(&mut self, stopping: bool) {
        if let UiMode::DeletionCancel { planned_entries } = self.ui_mode {
            self.ui_mode = UiMode::Deleting {
                planned_entries,
                stopping,
            };
            self.mark_dirty();
        }
    }

    pub fn complete_deletion(&mut self, report: DeletionReport) -> bool {
        self.ui_effects.deletion_in_progress = false;
        let deleted = report.deleted_entries() > 0;
        self.file_tree.apply_deletion_report(&report);
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
        deleted
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

    pub fn show_error(&mut self, message: impl Into<String>) {
        self.ui_effects.deletion_in_progress = false;
        self.ui_mode = UiMode::ErrorMessage(message.into());
        self.mark_dirty();
    }

    pub fn normal_mode(&mut self) {
        self.ui_mode = UiMode::Normal;
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
    use ratatui::backend::TestBackend;

    use super::*;

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
}
