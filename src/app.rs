use std::fs::Metadata;
use std::mem::ManuallyDrop;
use std::path::PathBuf;
use std::time::Duration;

use ratatui::backend::Backend;

use crate::animation::AnimationScheduler;
use crate::error::AppError;
use crate::state::files::{FileOrFolder, FileTree, Folder};
use crate::state::tiles::Board;
use crate::state::{FileToDelete, UiEffects};
use crate::ui::Display;

#[derive(Clone)]
pub enum UiMode {
    Loading,
    Normal,
    ScreenTooSmall,
    DeleteFile(FileToDelete),
    ErrorMessage(String),
    Exiting { app_loaded: bool },
    WarningMessage,
}

impl UiMode {
    #[must_use]
    pub const fn allows_motion(&self) -> bool {
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
    board: Board,
    file_tree: ManuallyDrop<FileTree>,
    display: Display<B>,
    ui_effects: UiEffects,
    delete_confirmation_disabled: bool,
    dirty: bool,
}

impl<B> App<B>
where
    B: Backend,
{
    pub fn new(
        terminal_backend: B,
        path_in_filesystem: PathBuf,
        show_apparent_size: bool,
        disable_delete_confirmation: bool,
    ) -> Result<Self, AppError> {
        let display = Display::new(terminal_backend)?;
        let board = Board::new(&Folder::new());
        let file_tree = ManuallyDrop::new(FileTree::new(
            Folder::new(),
            path_in_filesystem,
            show_apparent_size,
        ));
        Ok(Self {
            is_running: true,
            loaded: false,
            board,
            file_tree,
            display,
            ui_mode: UiMode::Loading,
            ui_effects: UiEffects::new(),
            delete_confirmation_disabled: disable_delete_confirmation,
            dirty: true,
        })
    }

    pub fn render_if_dirty(
        &mut self,
        animation: &mut AnimationScheduler,
        now: Duration,
    ) -> Result<bool, AppError> {
        if !self.dirty {
            return Ok(false);
        }
        let full_screen_size = self.display.size()?;
        if full_screen_size.width < 50 || full_screen_size.height < 15 {
            self.ui_mode = UiMode::ScreenTooSmall;
        }
        self.display.render(
            &self.file_tree,
            &mut self.board,
            &self.ui_mode,
            &self.ui_effects,
            animation,
            now,
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

    pub fn render_and_update_board(&mut self) {
        let current_folder = self.file_tree.get_current_folder();
        self.board.change_files(current_folder);
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

    pub fn add_entry_to_base_folder(&mut self, file_metadata: &Metadata, entry_path: PathBuf) {
        self.file_tree.add_entry(file_metadata, &entry_path);
        self.ui_effects.last_read_path = Some(entry_path);
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
            app_loaded: self.loaded,
        };
        self.mark_dirty();
    }

    pub const fn exit(&mut self) {
        self.is_running = false;
    }

    pub fn handle_enter(&mut self) {
        if !self.board.has_selected_index() {
            self.board.move_to_largest_folder();
        }
        self.enter_selected();
    }

    pub fn move_selected_right(&mut self) {
        self.board.move_selected_right();
        self.mark_dirty();
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
            && let Some(FileOrFolder::Folder(_)) = self.file_tree.item_in_current_folder(&tile.name)
        {
            self.file_tree.enter_folder(&tile.name);
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
        let mut path_to_file = self.file_tree.current_folder_names.clone();
        path_to_file.push(currently_selected.name.clone());
        Some(FileToDelete {
            path_in_filesystem: self.file_tree.path_in_filesystem.clone(),
            path_to_file,
            file_type: currently_selected.file_type,
            num_descendants: currently_selected.descendants,
            size: currently_selected.size,
        })
    }

    pub fn prompt_file_deletion(&mut self) -> Option<FileToDelete> {
        let file_to_delete = self.get_file_to_delete()?;
        self.ui_mode = UiMode::DeleteFile(file_to_delete.clone());
        self.mark_dirty();
        self.delete_confirmation_disabled.then_some(file_to_delete)
    }

    pub fn begin_deletion(&mut self, file_to_delete: &FileToDelete) {
        self.ui_mode = UiMode::DeleteFile(file_to_delete.clone());
        self.ui_effects.deletion_in_progress = true;
        self.mark_dirty();
    }

    pub fn complete_deletion(
        &mut self,
        file_to_delete: &FileToDelete,
        error: Option<String>,
    ) -> bool {
        self.ui_effects.deletion_in_progress = false;
        if let Some(error) = error {
            self.ui_mode = UiMode::ErrorMessage(error);
            self.mark_dirty();
            return false;
        }

        self.remove_file_from_ui(file_to_delete);
        self.ui_mode = UiMode::Normal;
        self.render_and_update_board();
        true
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

    pub fn increment_failed_to_read(&mut self) {
        self.file_tree.failed_to_read += 1;
    }

    pub fn zoom_in(&mut self) {
        self.board.zoom_in(self.file_tree.get_current_folder());
        self.mark_dirty();
    }

    pub fn zoom_out(&mut self) {
        self.board.zoom_out(self.file_tree.get_current_folder());
        self.mark_dirty();
    }

    pub fn reset_zoom(&mut self) {
        self.board.reset_zoom(self.file_tree.get_current_folder());
        self.mark_dirty();
    }

    fn remove_file_from_ui(&mut self, file_to_delete: &FileToDelete) {
        self.file_tree.space_freed += file_to_delete.size;
        self.file_tree.delete_file(file_to_delete);
        self.board.reset_selected_index();
    }
}
