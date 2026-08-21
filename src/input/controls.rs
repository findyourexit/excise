#![allow(clippy::unnested_or_patterns)]
use std::path::PathBuf;

use std::time::Duration;

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind, poll, read,
};
use ratatui::backend::Backend;

use crate::App;
use crate::config::{KeyPreset, is_supported_custom_movement_key};
use crate::deletion::DeletionPlan;
use crate::error::AppError;
use crate::state::FileToDelete;

pub enum InputEvent {
    Terminal(Event),
    Barrier,
}

pub trait InputSource {
    /// # Errors
    /// Returns an input I/O error when terminal readiness cannot be queried.
    fn poll(&mut self, timeout: Duration) -> Result<bool, AppError>;
    /// # Errors
    /// Returns an input I/O error when the next terminal event cannot be read.
    fn read(&mut self) -> Result<InputEvent, AppError>;
}

#[derive(Clone)]
pub struct TerminalEvents;

impl InputSource for TerminalEvents {
    fn poll(&mut self, timeout: Duration) -> Result<bool, AppError> {
        poll(timeout).map_err(|error| AppError::io("could not poll terminal input", error))
    }

    fn read(&mut self) -> Result<InputEvent, AppError> {
        loop {
            let event =
                read().map_err(|error| AppError::io("could not read terminal input", error))?;
            if let Event::Key(key_event) = &event
                && key_event.kind == KeyEventKind::Release
            {
                continue;
            }
            return Ok(InputEvent::Terminal(event));
        }
    }
}

pub(crate) enum InputCommand {
    None,
    Navigation,
    PathError,
    StartRescan(PathBuf),
    CancelRescan,
    PlanDeletion(FileToDelete),
    ExecuteDeletion(DeletionPlan),
    ExportScan,
    ExportDeletionHistory,
    CycleTheme,
    SavePreferencesAndExit,
    DiscardPreferencesAndExit,
    SoftCancelDeletion,
    HardCancel,
}

macro_rules! key {
    (char $x:expr) => {
        Event::Key(KeyEvent {
            code: KeyCode::Char($x),
            modifiers: KeyModifiers::NONE,
            ..
        })
    };
    (shift $x:expr) => {
        Event::Key(KeyEvent {
            code: KeyCode::Char($x),
            modifiers: KeyModifiers::SHIFT,
            ..
        })
    };
    (ctrl $x:expr) => {
        Event::Key(KeyEvent {
            code: KeyCode::Char($x),
            modifiers: KeyModifiers::CONTROL,
            ..
        })
    };
    ($x:ident) => {
        Event::Key(KeyEvent {
            code: KeyCode::$x,
            modifiers: KeyModifiers::NONE,
            ..
        })
    };
}

pub(crate) fn handle_keypress<B: Backend>(evt: &Event, app: &mut App<B>) -> InputCommand {
    match &app.ui_mode {
        crate::UiMode::Loading => handle_keypress_loading_mode(evt, app),
        crate::UiMode::Normal => handle_keypress_normal_mode(evt, app),
        crate::UiMode::Rescanning { .. } => handle_keypress_rescanning_mode(evt, app),
        crate::UiMode::FilterInput { .. } => handle_keypress_filter_mode(evt, app),
        crate::UiMode::Help => handle_keypress_help_mode(evt, app),
        crate::UiMode::ScreenTooSmall => handle_keypress_screen_too_small(evt, app),
        crate::UiMode::PlanningDeletion(_) => handle_keypress_planning_mode(evt, app),
        crate::UiMode::DeleteConfirm { .. } => handle_keypress_delete_confirm_mode(evt, app),
        crate::UiMode::Deleting { .. } => handle_keypress_deleting_mode(evt, app),
        crate::UiMode::DeletionCancel { .. } => handle_keypress_deletion_cancel_mode(evt, app),
        crate::UiMode::DeletionResult { .. } => handle_keypress_deletion_result_mode(evt, app),
        crate::UiMode::ErrorMessage(_) => handle_keypress_error_message(evt, app),
        crate::UiMode::Exiting { .. } => handle_keypress_exiting_mode(evt, app),
        crate::UiMode::Notice(_) => handle_keypress_notice_mode(evt, app),
        crate::UiMode::WarningMessage => {
            app.reset_ui_mode();
            InputCommand::None
        }
    }
}

fn handle_keypress_loading_mode<B: Backend>(evt: &Event, app: &mut App<B>) -> InputCommand {
    handle_navigation(evt, app, true)
}

fn handle_keypress_normal_mode<B: Backend>(evt: &Event, app: &mut App<B>) -> InputCommand {
    if matches!(evt, key!(char '/')) {
        app.open_filter();
        return InputCommand::None;
    }
    if matches!(evt, key!(char '?')) {
        app.open_help();
        return InputCommand::None;
    }
    if matches!(evt, key!(char 'e')) {
        return InputCommand::ExportScan;
    }
    if matches!(evt, key!(char 't')) {
        return InputCommand::CycleTheme;
    }
    if matches!(evt, key!(Backspace)) {
        return app
            .prompt_file_deletion()
            .map_or(InputCommand::None, InputCommand::PlanDeletion);
    }
    handle_navigation(evt, app, false)
}

#[allow(clippy::too_many_lines)]
fn handle_navigation<B: Backend>(evt: &Event, app: &mut App<B>, loading: bool) -> InputCommand {
    match evt {
        key!(ctrl 'c') | key!(char 'q') => {
            app.prompt_exit();
            InputCommand::None
        }
        key!(Right) => {
            app.move_selected_right();
            InputCommand::Navigation
        }
        key!(Left) => {
            app.move_selected_left();
            InputCommand::Navigation
        }
        key!(Down) => {
            app.move_selected_down();
            InputCommand::Navigation
        }
        key!(Up) => {
            app.move_selected_up();
            InputCommand::Navigation
        }
        key!(char 'l') if app.keymap() == KeyPreset::Vim => {
            app.move_selected_right();
            InputCommand::Navigation
        }
        key!(char 'h') if app.keymap() == KeyPreset::Vim => {
            app.move_selected_left();
            InputCommand::Navigation
        }
        key!(char 'j') if app.keymap() == KeyPreset::Vim => {
            app.move_selected_down();
            InputCommand::Navigation
        }
        key!(char 'k') if app.keymap() == KeyPreset::Vim => {
            app.move_selected_up();
            InputCommand::Navigation
        }
        Event::Key(KeyEvent {
            code: KeyCode::Char(character),
            modifiers,
            ..
        }) if app.keymap() == KeyPreset::Custom
            && modifiers.is_empty()
            && is_supported_custom_movement_key(*character) =>
        {
            let Some(bindings) = app.custom_keys() else {
                return InputCommand::None;
            };
            if *character == bindings.left {
                app.move_selected_left();
            } else if *character == bindings.down {
                app.move_selected_down();
            } else if *character == bindings.up {
                app.move_selected_up();
            } else if *character == bindings.right {
                app.move_selected_right();
            } else {
                return InputCommand::None;
            }
            InputCommand::Navigation
        }
        key!(ctrl 'f') if app.keymap() == KeyPreset::Emacs => {
            app.move_selected_right();
            InputCommand::Navigation
        }
        key!(ctrl 'b') if app.keymap() == KeyPreset::Emacs => {
            app.move_selected_left();
            InputCommand::Navigation
        }
        key!(ctrl 'n') if app.keymap() == KeyPreset::Emacs => {
            app.move_selected_down();
            InputCommand::Navigation
        }
        key!(ctrl 'p') if app.keymap() == KeyPreset::Emacs => {
            app.move_selected_up();
            InputCommand::Navigation
        }
        key!(char '+') | key!(shift '+') => {
            app.zoom_in();
            InputCommand::Navigation
        }
        key!(char '-') => {
            app.zoom_out();
            InputCommand::Navigation
        }
        key!(char '0') => {
            app.reset_zoom();
            InputCommand::Navigation
        }
        key!(char '\n') | key!(Enter) => app
            .handle_enter()
            .map_or(InputCommand::Navigation, InputCommand::StartRescan),
        key!(Backspace) if loading => {
            app.show_warning_modal();
            InputCommand::None
        }
        key!(Esc) => {
            if app.go_up() {
                InputCommand::Navigation
            } else {
                InputCommand::PathError
            }
        }
        Event::Mouse(mouse) if app.mouse_enabled() => match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                app.select_at(mouse.column, mouse.row);
                InputCommand::Navigation
            }
            MouseEventKind::ScrollDown => {
                app.move_selected_down();
                InputCommand::Navigation
            }
            MouseEventKind::ScrollUp => {
                app.move_selected_up();
                InputCommand::Navigation
            }
            _ => InputCommand::None,
        },
        _ => InputCommand::None,
    }
}

fn handle_keypress_rescanning_mode<B: Backend>(evt: &Event, app: &mut App<B>) -> InputCommand {
    if matches!(evt, key!(Esc)) {
        InputCommand::CancelRescan
    } else {
        handle_navigation(evt, app, true)
    }
}

fn handle_keypress_filter_mode<B: Backend>(evt: &Event, app: &mut App<B>) -> InputCommand {
    match evt {
        key!(Esc) => {
            app.normal_mode();
            InputCommand::None
        }
        key!(Enter) => {
            app.apply_filter();
            InputCommand::Navigation
        }
        key!(Backspace) => {
            app.pop_filter_character();
            InputCommand::None
        }
        Event::Key(KeyEvent {
            code: KeyCode::Char(character),
            modifiers,
            ..
        }) if modifiers.is_empty() || *modifiers == KeyModifiers::SHIFT => {
            app.push_filter_character(*character);
            InputCommand::None
        }
        _ => InputCommand::None,
    }
}
fn handle_keypress_planning_mode<B: Backend>(evt: &Event, app: &mut App<B>) -> InputCommand {
    match evt {
        key!(Esc) => {
            app.normal_mode();
            InputCommand::None
        }
        key!(ctrl 'c') => {
            app.exit();
            InputCommand::HardCancel
        }
        key!(char 'q') => {
            app.prompt_exit();
            InputCommand::None
        }
        _ => InputCommand::None,
    }
}

fn handle_keypress_help_mode<B: Backend>(evt: &Event, app: &mut App<B>) -> InputCommand {
    if matches!(evt, key!(Esc) | key!(char '?') | key!(char 'q')) {
        app.normal_mode();
    }
    InputCommand::None
}

fn handle_keypress_delete_confirm_mode<B: Backend>(evt: &Event, app: &mut App<B>) -> InputCommand {
    match evt {
        key!(ctrl 'c') | key!(char 'q') | key!(Esc) => {
            app.normal_mode();
            InputCommand::None
        }
        key!(char 'n') if app.confirmation_is_single_key() => {
            app.normal_mode();
            InputCommand::None
        }
        key!(Backspace) => {
            app.pop_confirmation_character();
            InputCommand::None
        }
        key!(Enter) => app.take_confirmed_deletion_plan().map_or_else(
            || {
                app.take_deletion_replan()
                    .map_or(InputCommand::None, InputCommand::PlanDeletion)
            },
            InputCommand::ExecuteDeletion,
        ),
        Event::Key(KeyEvent {
            code: KeyCode::Char(character),
            modifiers,
            ..
        }) if modifiers.is_empty() || *modifiers == KeyModifiers::SHIFT => {
            app.push_confirmation_character(*character);
            if app.confirmation_is_single_key() {
                app.take_confirmed_deletion_plan().map_or_else(
                    || {
                        app.take_deletion_replan()
                            .map_or(InputCommand::None, InputCommand::PlanDeletion)
                    },
                    InputCommand::ExecuteDeletion,
                )
            } else {
                InputCommand::None
            }
        }
        _ => InputCommand::None,
    }
}

fn handle_keypress_deleting_mode<B: Backend>(evt: &Event, app: &mut App<B>) -> InputCommand {
    if matches!(&app.ui_mode, crate::UiMode::Deleting { stopping: true, .. }) {
        if matches!(evt, key!(ctrl 'c') | key!(char 'h')) {
            app.exit();
            return InputCommand::HardCancel;
        }
        return InputCommand::None;
    }
    if matches!(evt, key!(ctrl 'c') | key!(char 'q') | key!(Esc)) {
        app.prompt_deletion_cancel();
    }
    InputCommand::None
}

fn handle_keypress_deletion_cancel_mode<B: Backend>(evt: &Event, app: &mut App<B>) -> InputCommand {
    match evt {
        key!(char 's') => {
            app.resume_deletion(true);
            InputCommand::SoftCancelDeletion
        }
        key!(ctrl 'c') | key!(char 'h') => {
            app.exit();
            InputCommand::HardCancel
        }
        key!(Esc) | key!(char 'b') => {
            app.resume_deletion(false);
            InputCommand::None
        }
        _ => InputCommand::None,
    }
}

fn handle_keypress_deletion_result_mode<B: Backend>(evt: &Event, app: &mut App<B>) -> InputCommand {
    match evt {
        key!(char 'e') => InputCommand::ExportDeletionHistory,
        key!(ctrl 'c') => {
            app.prompt_exit();
            InputCommand::None
        }
        key!(Enter) | key!(Esc) | key!(char 'q') => {
            app.normal_mode();
            InputCommand::None
        }
        _ => InputCommand::None,
    }
}

fn handle_keypress_error_message<B: Backend>(evt: &Event, app: &mut App<B>) -> InputCommand {
    if matches!(evt, key!(ctrl 'c') | key!(char 'q') | key!(Esc)) {
        app.normal_mode();
    }
    InputCommand::None
}

fn handle_keypress_notice_mode<B: Backend>(evt: &Event, app: &mut App<B>) -> InputCommand {
    if matches!(evt, key!(Enter) | key!(Esc) | key!(char 'q')) {
        app.normal_mode();
    }
    InputCommand::None
}

fn handle_keypress_screen_too_small<B: Backend>(evt: &Event, app: &mut App<B>) -> InputCommand {
    if matches!(evt, key!(ctrl 'c') | key!(char 'q')) {
        app.exit();
    }
    InputCommand::None
}

fn handle_keypress_exiting_mode<B: Backend>(evt: &Event, app: &mut App<B>) -> InputCommand {
    match evt {
        key!(ctrl 'c') => {
            app.exit();
            InputCommand::HardCancel
        }
        key!(char 'q') | key!(Esc) | key!(char 'n') => {
            app.reset_ui_mode();
            InputCommand::None
        }
        key!(char 's') if app.preferences_dirty() => InputCommand::SavePreferencesAndExit,
        key!(char 'd') if app.preferences_dirty() => InputCommand::DiscardPreferencesAndExit,
        key!(char 'y') if !app.preferences_dirty() => {
            app.exit();
            InputCommand::None
        }
        _ => InputCommand::None,
    }
}
