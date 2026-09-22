#![allow(clippy::unnested_or_patterns)]
use std::path::PathBuf;

use std::time::Duration;

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind, poll, read,
};
use ratatui::backend::Backend;

use crate::App;
use crate::app::{EnterAction, ExitWork};
use crate::config::{KeyPreset, is_supported_custom_movement_key};
use crate::error::AppError;
use crate::state::{DeletionWorkId, FileToDelete};
use crate::theme::ThemeId;

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
    /// A folder entry or exit (Enter/Esc drill). Treated as a batch boundary so
    /// each folder change gets its own render before the next input is processed.
    /// Arrow-key selection moves remain in Navigation (no boundary) to stay fast.
    Drill,
    Navigation,
    PathError,
    StartRescan(PathBuf),
    CancelRescan,
    RequestDeletion(Box<FileToDelete>),
    CancelDeletionConfirmation,
    ConfirmDeletion {
        work_id: DeletionWorkId,
        target: Box<FileToDelete>,
    },
    ExportScan,
    ExportDeletionHistory,
    OpenThemePicker,
    PreviewTheme(ThemeId),
    CommitTheme {
        original: ThemeId,
        selected: ThemeId,
    },
    RestoreTheme(ThemeId),
    PromptExit,
    CancelPendingWorkAndExit,
    StopDeletionAndExit,
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
        crate::UiMode::ThemePicker { .. } => handle_keypress_theme_picker_mode(evt, app),
        crate::UiMode::DeleteConfirm { .. } => handle_keypress_delete_confirm_mode(evt, app),
        crate::UiMode::ErrorMessage(_) => handle_keypress_error_message(evt, app),
        crate::UiMode::Exiting { .. } => handle_keypress_exiting_mode(evt, app),
        crate::UiMode::Notice(_) => handle_keypress_notice_mode(evt, app),
        crate::UiMode::WarningMessage => {
            app.normal_mode();
            InputCommand::None
        }
    }
}

fn deletion_request<B: Backend>(app: &mut App<B>) -> InputCommand {
    app.request_deletion().map_or(InputCommand::None, |target| {
        InputCommand::RequestDeletion(Box::new(target))
    })
}

fn handle_keypress_loading_mode<B: Backend>(evt: &Event, app: &mut App<B>) -> InputCommand {
    if matches!(evt, key!(char 't')) {
        return InputCommand::OpenThemePicker;
    }
    if matches!(evt, key!(Backspace)) {
        return deletion_request(app);
    }
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
    if matches!(evt, key!(shift 'E')) {
        return InputCommand::ExportDeletionHistory;
    }
    if matches!(evt, key!(char 't')) {
        return InputCommand::OpenThemePicker;
    }
    if matches!(evt, key!(Backspace)) {
        return deletion_request(app);
    }
    handle_navigation(evt, app, false)
}

#[allow(clippy::too_many_lines)]
fn handle_navigation<B: Backend>(evt: &Event, app: &mut App<B>, loading: bool) -> InputCommand {
    match evt {
        key!(ctrl 'c') | key!(char 'q') => InputCommand::PromptExit,
        key!(PageDown) => {
            app.next_snapshot_page();
            InputCommand::Navigation
        }
        key!(PageUp) => {
            app.previous_snapshot_page();
            InputCommand::Navigation
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
        key!(char '\n') | key!(Enter) => match app.handle_enter_action() {
            EnterAction::None => InputCommand::None,
            EnterAction::Drill => InputCommand::Drill,
            EnterAction::Rescan(path) => InputCommand::StartRescan(path),
        },
        key!(Backspace) if loading => {
            app.show_warning_modal();
            InputCommand::None
        }
        key!(Esc) => {
            if app.go_up() {
                InputCommand::Drill
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
    if matches!(evt, key!(char 't')) {
        InputCommand::OpenThemePicker
    } else if matches!(evt, key!(Backspace)) {
        deletion_request(app)
    } else if matches!(evt, key!(Esc)) {
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
fn handle_keypress_theme_picker_mode<B: Backend>(evt: &Event, app: &mut App<B>) -> InputCommand {
    match evt {
        key!(Up) | key!(char 'k') => app
            .move_theme_picker(true)
            .map_or(InputCommand::None, InputCommand::PreviewTheme),
        key!(Down) | key!(char 'j') => app
            .move_theme_picker(false)
            .map_or(InputCommand::None, InputCommand::PreviewTheme),
        key!(Enter) => app
            .commit_theme_picker()
            .map_or(InputCommand::None, |(original, selected)| {
                InputCommand::CommitTheme { original, selected }
            }),
        key!(Esc) | key!(char 'q') => app
            .cancel_theme_picker()
            .map_or(InputCommand::None, InputCommand::RestoreTheme),
        _ => InputCommand::None,
    }
}

fn handle_keypress_help_mode<B: Backend>(evt: &Event, app: &mut App<B>) -> InputCommand {
    if matches!(evt, key!(Esc) | key!(char '?') | key!(char 'q')) {
        app.normal_mode();
    }
    InputCommand::None
}

fn cancel_deletion_confirmation<B: Backend>(app: &mut App<B>) -> InputCommand {
    if app.cancel_deletion_confirmation() {
        InputCommand::CancelDeletionConfirmation
    } else {
        InputCommand::None
    }
}

fn handle_keypress_delete_confirm_mode<B: Backend>(evt: &Event, app: &mut App<B>) -> InputCommand {
    match evt {
        key!(ctrl 'c') => InputCommand::PromptExit,
        key!(char 'q') | key!(Esc) | key!(char 'n') => cancel_deletion_confirmation(app),
        key!(Backspace) => {
            app.pop_confirmation_character();
            InputCommand::None
        }
        key!(Enter) | key!(char 'y') => app
            .arm_and_confirm_deletion_target()
            .map_or(InputCommand::None, |(work_id, target)| {
                InputCommand::ConfirmDeletion { work_id, target }
            }),
        Event::Key(KeyEvent {
            code: KeyCode::Char(character),
            modifiers,
            ..
        }) if modifiers.is_empty() || *modifiers == KeyModifiers::SHIFT => {
            app.push_confirmation_character(*character);
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
    if !matches!(evt, key!(ctrl 'c') | key!(char 'q')) {
        return InputCommand::None;
    }
    if app.can_exit_immediately() {
        app.exit();
        InputCommand::None
    } else {
        InputCommand::PromptExit
    }
}

fn handle_keypress_exiting_mode<B: Backend>(evt: &Event, app: &mut App<B>) -> InputCommand {
    match evt {
        key!(char 'q') | key!(Esc) | key!(char 'n')
            if matches!(
                app.exit_work(),
                Some(ExitWork::None | ExitWork::Pending { .. } | ExitWork::Active { .. })
            ) =>
        {
            app.dismiss_exit();
            InputCommand::None
        }
        key!(char 'w')
            if matches!(
                app.exit_work(),
                Some(ExitWork::Pending { .. } | ExitWork::Active { .. })
            ) =>
        {
            app.dismiss_exit();
            InputCommand::None
        }
        key!(char 'c') if matches!(app.exit_work(), Some(ExitWork::Pending { .. })) => {
            InputCommand::CancelPendingWorkAndExit
        }
        key!(char 's') if matches!(app.exit_work(), Some(ExitWork::Active { .. })) => {
            InputCommand::StopDeletionAndExit
        }
        key!(char 'y') if matches!(app.exit_work(), Some(ExitWork::None)) => {
            app.exit();
            InputCommand::None
        }
        _ => InputCommand::None,
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use ratatui::backend::TestBackend;

    use crate::UiMode;
    use crate::config::KeyPreset;

    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }

    fn app() -> (tempfile::TempDir, App<TestBackend>) {
        let root = tempfile::tempdir().expect("input root should exist");
        let app = App::new(
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
        (root, app)
    }

    #[test]
    fn theme_picker_previews_and_restores_without_committing() {
        let (_root, mut app) = app();
        app.loaded = true;
        app.ui_mode = UiMode::Normal;
        assert!(matches!(
            handle_keypress(&key(KeyCode::Char('t'), KeyModifiers::NONE), &mut app),
            InputCommand::OpenThemePicker
        ));

        app.open_theme_picker(ThemeId::ExciseDark);
        assert!(matches!(
            handle_keypress(&key(KeyCode::Char('t'), KeyModifiers::NONE), &mut app),
            InputCommand::None
        ));
        assert!(matches!(
            handle_keypress(&key(KeyCode::Down, KeyModifiers::NONE), &mut app),
            InputCommand::PreviewTheme(ThemeId::ExciseLight)
        ));
        assert!(matches!(
            handle_keypress(&key(KeyCode::Esc, KeyModifiers::NONE), &mut app),
            InputCommand::RestoreTheme(ThemeId::ExciseDark)
        ));
        assert!(matches!(app.ui_mode, UiMode::Normal));
    }

    #[test]
    fn small_screen_exit_without_work_is_immediate() {
        let (_root, mut app) = app();
        app.ui_mode = UiMode::ScreenTooSmall;

        let command = handle_keypress(&key(KeyCode::Char('q'), KeyModifiers::NONE), &mut app);

        assert!(matches!(command, InputCommand::None));
        assert!(!app.is_running);
    }

    #[test]
    fn active_exit_offers_safe_stop_without_forced_detach() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicU64;

        let (_root, mut app) = app();
        app.ui_mode = UiMode::Exiting {
            work: ExitWork::Active {
                planned_entries: 1,
                completed: Arc::new(AtomicU64::new(0)),
                pending: 0,
            },
            return_to: crate::app::ThemePickerReturn::Normal,
        };

        let command = handle_keypress(&key(KeyCode::Char('s'), KeyModifiers::NONE), &mut app);
        assert!(matches!(command, InputCommand::StopDeletionAndExit));
        let command = handle_keypress(&key(KeyCode::Char('c'), KeyModifiers::CONTROL), &mut app);
        assert!(matches!(command, InputCommand::None));
        assert!(app.is_running);
    }

    #[test]
    fn confirmation_escape_returns_its_job_to_runtime() {
        let (_root, mut app) = app();
        app.loaded = true;
        app.ui_mode = UiMode::DeleteConfirm {
            work_id: DeletionWorkId::for_test(7),
            target: Box::new(FileToDelete {
                node_id: crate::model::NodeId(1),
                synthetic: false,
                path_in_filesystem: std::path::PathBuf::from("/scan-root"),
                path_to_file: vec![std::ffi::OsString::from("target")],
                file_type: crate::state::tiles::FileType::File,
                num_descendants: None,
                size: 0,
                expected_snapshot: crate::model::EntrySnapshot {
                    identity: None,
                    kind: crate::model::NodeKind::File,
                    apparent_bytes: 0,
                    allocated_bytes: None,
                    modified_nanos: None,
                },
                reviewed_entries: Vec::new(),
            }),
            challenge: crate::deletion::ConfirmationChallenge::ConfirmFile,
            input: String::new(),
            return_to: crate::app::ThemePickerReturn::Normal,
        };

        let command = handle_keypress(&key(KeyCode::Esc, KeyModifiers::NONE), &mut app);
        assert!(matches!(command, InputCommand::CancelDeletionConfirmation));
        assert!(matches!(app.ui_mode, UiMode::Normal));
    }
}
