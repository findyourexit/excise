#![allow(clippy::unnested_or_patterns)]

use std::time::Duration;

use crossterm::event::Event;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyModifiers;
use crossterm::event::{KeyCode, KeyEvent, poll, read};
use ratatui::backend::Backend;

use crate::App;
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
    Delete(FileToDelete),
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
        crate::UiMode::ScreenTooSmall => handle_keypress_screen_too_small(evt, app),
        crate::UiMode::DeleteFile(file_to_delete) => {
            let file_to_delete = file_to_delete.clone();
            handle_keypress_delete_file_mode(evt, app, file_to_delete)
        }
        crate::UiMode::ErrorMessage(_) => handle_keypress_error_message(evt, app),
        crate::UiMode::Exiting { app_loaded: _ } => handle_keypress_exiting_mode(evt, app),
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
    if matches!(evt, key!(Backspace)) {
        return app
            .prompt_file_deletion()
            .map_or(InputCommand::None, InputCommand::Delete);
    }
    handle_navigation(evt, app, false)
}

fn handle_navigation<B: Backend>(evt: &Event, app: &mut App<B>, loading: bool) -> InputCommand {
    match evt {
        key!(ctrl 'c') | key!(char 'q') => {
            app.prompt_exit();
            InputCommand::None
        }
        key!(char 'l') | key!(Right) | key!(ctrl 'f') => {
            app.move_selected_right();
            InputCommand::Navigation
        }
        key!(char 'h') | key!(Left) | key!(ctrl 'b') => {
            app.move_selected_left();
            InputCommand::Navigation
        }
        key!(char 'j') | key!(Down) | key!(ctrl 'n') => {
            app.move_selected_down();
            InputCommand::Navigation
        }
        key!(char 'k') | key!(Up) | key!(ctrl 'p') => {
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
        key!(char '\n') | key!(Enter) => {
            app.handle_enter();
            InputCommand::Navigation
        }
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
        _ => InputCommand::None,
    }
}

fn handle_keypress_delete_file_mode<B: Backend>(
    evt: &Event,
    app: &mut App<B>,
    file_to_delete: FileToDelete,
) -> InputCommand {
    match evt {
        key!(ctrl 'c') | key!(char 'q') | key!(Esc) | key!(char 'n') => {
            app.normal_mode();
            InputCommand::None
        }
        key!(char 'y') => InputCommand::Delete(file_to_delete),
        _ => InputCommand::None,
    }
}

fn handle_keypress_error_message<B: Backend>(evt: &Event, app: &mut App<B>) -> InputCommand {
    if matches!(evt, key!(ctrl 'c') | key!(char 'q') | key!(Esc)) {
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
        key!(char 'y') => {
            app.exit();
            InputCommand::None
        }
        _ => InputCommand::None,
    }
}
