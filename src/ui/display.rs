use std::time::Duration;

use ::std::path::PathBuf;
use ratatui::Terminal;
use ratatui::backend::Backend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};

use crate::UiMode;
use crate::animation::AnimationScheduler;
use crate::error::AppError;
use crate::state::UiEffects;
use crate::state::files::FileTree;
use crate::state::tiles::Board;
use crate::ui::grid::RectangleGrid;
use crate::ui::modals::{ConfirmBox, ErrorBox, MessageBox, WarningBox};
use crate::ui::title::TitleLine;
use crate::ui::{BottomLine, TermTooSmall};

pub struct FolderInfo<'a> {
    pub path: &'a PathBuf,
    pub size: u128,
    pub num_descendants: u64,
}

pub struct Display<B>
where
    B: Backend,
{
    terminal: Terminal<B>,
}

impl<B> Display<B>
where
    B: Backend,
{
    /// # Errors
    /// Returns a terminal error if initialization, clearing, or cursor setup fails.
    pub fn new(terminal_backend: B) -> Result<Self, AppError> {
        let mut terminal = Terminal::new(terminal_backend)
            .map_err(|error| AppError::terminal("initialization", error))?;
        terminal
            .backend_mut()
            .clear()
            .map_err(|error| AppError::terminal("clear", error))?;
        terminal
            .hide_cursor()
            .map_err(|error| AppError::terminal("cursor hide", error))?;
        Ok(Self { terminal })
    }
    /// # Errors
    /// Returns a terminal error if the terminal size cannot be read.
    pub fn size(&self) -> Result<Rect, AppError> {
        let size = self
            .terminal
            .size()
            .map_err(|error| AppError::terminal("size query", error))?;
        Ok(Rect::new(0, 0, size.width, size.height))
    }
    /// Renders the application UI based on the current mode
    ///
    /// # Errors
    /// Returns a terminal error if drawing fails.
    #[allow(clippy::too_many_lines)]
    pub fn render(
        &mut self,
        file_tree: &FileTree,
        board: &mut Board,
        ui_mode: &UiMode,
        ui_effects: &UiEffects,
        animation: &mut AnimationScheduler,
        now: Duration,
    ) -> Result<(), AppError> {
        self.terminal
            .draw(|f| {
                let full_screen = f.area();
                let current_path = file_tree.get_current_path();
                let current_path_size = file_tree.get_current_folder_size();
                let current_path_descendants = file_tree.get_current_folder().num_descendants;
                let base_path_size = file_tree.get_total_size();
                let base_path_descendants = file_tree.get_total_descendants();
                let current_path_info = FolderInfo {
                    path: &current_path,
                    size: current_path_size,
                    num_descendants: current_path_descendants,
                };
                let path_in_filesystem = &file_tree.path_in_filesystem;
                let base_path_info = FolderInfo {
                    path: path_in_filesystem,
                    size: base_path_size,
                    num_descendants: base_path_descendants,
                };
                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .margin(0)
                    .constraints(
                        [
                            Constraint::Length(1),
                            Constraint::Min(10),
                            Constraint::Length(2),
                        ]
                        .as_ref(),
                    )
                    .split(full_screen);

                // -1 cos we draw starting at offset 1 in both x and y directions
                let mut main_area = chunks[1];
                main_area.width = main_area.width.saturating_sub(1);
                main_area.height = main_area.height.saturating_sub(1);
                board.change_area(main_area);
                match ui_mode {
                    UiMode::Loading => {
                        f.render_widget(
                            TitleLine::new(
                                base_path_info,
                                current_path_info,
                                file_tree.space_freed,
                            )
                            .progress_indicator(ui_effects.loading_progress_indicator)
                            .path_error(ui_effects.current_path_is_red)
                            .read_errors(file_tree.failed_to_read)
                            .zoom_level(board.zoom_level)
                            .show_loading(),
                            chunks[0],
                        );
                        f.render_widget(
                            RectangleGrid::new(
                                &board.tiles,
                                board.unrenderable_tile_coordinates,
                                board.selected_index,
                            ),
                            chunks[1],
                        );
                        f.render_widget(
                            BottomLine::new()
                                .currently_selected(board.currently_selected())
                                .last_read_path(ui_effects.last_read_path.as_ref())
                                .hide_delete()
                                .hide_small_files_legend(
                                    board.unrenderable_tile_coordinates.is_none(),
                                ),
                            chunks[2],
                        );
                    }
                    UiMode::Normal => {
                        f.render_widget(
                            TitleLine::new(
                                base_path_info,
                                current_path_info,
                                file_tree.space_freed,
                            )
                            .path_error(ui_effects.current_path_is_red)
                            .flash_space(ui_effects.flash_space_freed)
                            .zoom_level(board.zoom_level)
                            .read_errors(file_tree.failed_to_read),
                            chunks[0],
                        );
                        f.render_widget(
                            RectangleGrid::new(
                                &board.tiles,
                                board.unrenderable_tile_coordinates,
                                board.selected_index,
                            ),
                            chunks[1],
                        );
                        f.render_widget(
                            BottomLine::new()
                                .currently_selected(board.currently_selected())
                                .hide_small_files_legend(
                                    board.unrenderable_tile_coordinates.is_none(),
                                ),
                            chunks[2],
                        );
                    }
                    UiMode::ScreenTooSmall => {
                        f.render_widget(TermTooSmall::new(), f.area());
                    }
                    UiMode::DeleteFile(file_to_delete) => {
                        f.render_widget(
                            TitleLine::new(
                                base_path_info,
                                current_path_info,
                                file_tree.space_freed,
                            )
                            .path_error(ui_effects.current_path_is_red)
                            .zoom_level(board.zoom_level)
                            .read_errors(file_tree.failed_to_read),
                            chunks[0],
                        );
                        f.render_widget(
                            RectangleGrid::new(
                                &board.tiles,
                                board.unrenderable_tile_coordinates,
                                board.selected_index,
                            ),
                            chunks[1],
                        );
                        f.render_widget(
                            BottomLine::new()
                                .currently_selected(board.currently_selected())
                                .hide_small_files_legend(
                                    board.unrenderable_tile_coordinates.is_none(),
                                ),
                            chunks[2],
                        );
                        f.render_widget(
                            MessageBox::new(file_to_delete, ui_effects.deletion_in_progress),
                            full_screen,
                        );
                    }
                    UiMode::ErrorMessage(message) => {
                        f.render_widget(
                            TitleLine::new(
                                base_path_info,
                                current_path_info,
                                file_tree.space_freed,
                            )
                            .path_error(ui_effects.current_path_is_red)
                            .flash_space(ui_effects.flash_space_freed)
                            .zoom_level(board.zoom_level)
                            .read_errors(file_tree.failed_to_read),
                            chunks[0],
                        );
                        f.render_widget(
                            RectangleGrid::new(
                                &board.tiles,
                                board.unrenderable_tile_coordinates,
                                board.selected_index,
                            ),
                            chunks[1],
                        );
                        f.render_widget(
                            BottomLine::new()
                                .currently_selected(board.currently_selected())
                                .hide_small_files_legend(
                                    board.unrenderable_tile_coordinates.is_none(),
                                ),
                            chunks[2],
                        );
                        f.render_widget(ErrorBox::new(message), full_screen);
                    }
                    UiMode::Exiting { app_loaded } => {
                        if *app_loaded {
                            // render normal ui mode
                            f.render_widget(
                                TitleLine::new(
                                    base_path_info,
                                    current_path_info,
                                    file_tree.space_freed,
                                )
                                .path_error(ui_effects.current_path_is_red)
                                .flash_space(ui_effects.flash_space_freed)
                                .zoom_level(board.zoom_level)
                                .read_errors(file_tree.failed_to_read),
                                chunks[0],
                            );
                            f.render_widget(
                                BottomLine::new()
                                    .currently_selected(board.currently_selected())
                                    .hide_small_files_legend(
                                        board.unrenderable_tile_coordinates.is_none(),
                                    ),
                                chunks[2],
                            );
                        } else {
                            // render loading ui mode
                            f.render_widget(
                                TitleLine::new(
                                    base_path_info,
                                    current_path_info,
                                    file_tree.space_freed,
                                )
                                .progress_indicator(ui_effects.loading_progress_indicator)
                                .path_error(ui_effects.current_path_is_red)
                                .zoom_level(board.zoom_level)
                                .read_errors(file_tree.failed_to_read)
                                .show_loading(),
                                chunks[0],
                            );
                            f.render_widget(
                                BottomLine::new()
                                    .currently_selected(board.currently_selected())
                                    .last_read_path(ui_effects.last_read_path.as_ref())
                                    .hide_delete()
                                    .hide_small_files_legend(
                                        board.unrenderable_tile_coordinates.is_none(),
                                    ),
                                chunks[2],
                            );
                        }
                        // render common widgets
                        f.render_widget(
                            RectangleGrid::new(
                                &board.tiles,
                                board.unrenderable_tile_coordinates,
                                board.selected_index,
                            ),
                            chunks[1],
                        );
                        f.render_widget(ConfirmBox::new(), f.area());
                    }
                    UiMode::WarningMessage => {
                        f.render_widget(
                            TitleLine::new(
                                base_path_info,
                                current_path_info,
                                file_tree.space_freed,
                            )
                            .progress_indicator(ui_effects.loading_progress_indicator)
                            .path_error(ui_effects.current_path_is_red)
                            .read_errors(file_tree.failed_to_read)
                            .show_loading(),
                            chunks[0],
                        );
                        f.render_widget(
                            RectangleGrid::new(
                                &board.tiles,
                                board.unrenderable_tile_coordinates,
                                board.selected_index,
                            ),
                            chunks[1],
                        );
                        f.render_widget(
                            BottomLine::new()
                                .currently_selected(board.currently_selected())
                                .last_read_path(ui_effects.last_read_path.as_ref())
                                .hide_delete()
                                .hide_small_files_legend(
                                    board.unrenderable_tile_coordinates.is_none(),
                                ),
                            chunks[2],
                        );
                        f.render_widget(WarningBox::new(), full_screen);
                    }
                }
                animation.process(now, f.buffer_mut(), full_screen);
            })
            .map_err(|error| AppError::terminal("draw", error))?;
        Ok(())
    }

    /// # Errors
    /// Returns a terminal error if clearing or cursor restoration fails.
    pub fn clear(&mut self) -> Result<(), AppError> {
        self.terminal
            .backend_mut()
            .clear()
            .map_err(|error| AppError::terminal("clear", error))?;
        self.terminal
            .show_cursor()
            .map_err(|error| AppError::terminal("cursor show", error))
    }
}
