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
    /// A folder entry or exit (Enter/Esc drill). Treated as a batch boundary so
    /// each folder change gets its own render before the next input is processed.
    /// Arrow-key selection moves remain in Navigation (no boundary) to stay fast.
    Drill,
    Navigation,
    PathError,
    StartRescan(PathBuf),
    CancelRescan,
    PlanDeletion(Box<FileToDelete>),
    CancelDeletionPlan,
    RevalidateDeletion(Box<DeletionPlan>),
    ExportScan,
    ExportDeletionHistory,
    CycleTheme,
    SavePreferencesAndExit,
    DiscardPreferencesAndExit,
    SoftCancelDeletion,
    ResumeDeletion,
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
    if matches!(evt, key!(Backspace)) {
        return app
            .prompt_file_deletion()
            .map_or(InputCommand::None, |target| {
                InputCommand::PlanDeletion(Box::new(target))
            });
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
    if matches!(evt, key!(char 't')) {
        return InputCommand::CycleTheme;
    }
    if matches!(evt, key!(Backspace)) {
        return app
            .prompt_file_deletion()
            .map_or(InputCommand::None, |target| {
                InputCommand::PlanDeletion(Box::new(target))
            });
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
            .map_or(InputCommand::Drill, InputCommand::StartRescan),
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
            InputCommand::CancelDeletionPlan
        }
        key!(ctrl 'c') => {
            app.exit();
            InputCommand::HardCancel
        }
        key!(char 'q') => {
            app.prompt_exit();
            InputCommand::None
        }
        // Pre-arm deletion so it fires automatically once planning completes,
        // but only for single-key challenges (files, or reduced-guardrails dirs).
        // Silently no-ops for directories requiring a typed name.
        key!(Enter) => {
            app.arm_deletion_enter();
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
        // For single-key challenges (ConfirmFile / ReducedGuard), Enter acts as
        // a primary confirm key by auto-filling the expected 'y' before
        // delegating to take_confirmed_deletion_plan. For TypeName/TypePhrase,
        // Enter confirms if and only if the typed input already matches.
        key!(Enter) => app
            .arm_and_confirm_deletion_plan()
            .map_or(InputCommand::None, |plan| {
                InputCommand::RevalidateDeletion(Box::new(plan))
            }),
        Event::Key(KeyEvent {
            code: KeyCode::Char(character),
            modifiers,
            ..
        }) if modifiers.is_empty() || *modifiers == KeyModifiers::SHIFT => {
            app.push_confirmation_character(*character);
            if app.confirmation_is_single_key() {
                app.take_confirmed_deletion_plan()
                    .map_or(InputCommand::None, |plan| {
                        InputCommand::RevalidateDeletion(Box::new(plan))
                    })
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
        key!(char 's') => InputCommand::SoftCancelDeletion,
        key!(ctrl 'c') | key!(char 'h') => {
            app.exit();
            InputCommand::HardCancel
        }
        key!(Esc) | key!(char 'b') => InputCommand::ResumeDeletion,
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
    fn deletion_cancel_keys_defer_state_changes_to_runtime() {
        let (_root, mut app) = app();
        app.ui_mode = UiMode::DeletionCancel {
            planned_entries: 1,
            completed: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        };

        let command = handle_keypress(&key(KeyCode::Char('s'), KeyModifiers::NONE), &mut app);
        assert!(matches!(command, InputCommand::SoftCancelDeletion));
        assert!(matches!(
            &app.ui_mode,
            UiMode::DeletionCancel {
                planned_entries: 1,
                ..
            }
        ));

        let command = handle_keypress(&key(KeyCode::Char('b'), KeyModifiers::NONE), &mut app);
        assert!(matches!(command, InputCommand::ResumeDeletion));
        assert!(matches!(
            &app.ui_mode,
            UiMode::DeletionCancel {
                planned_entries: 1,
                ..
            }
        ));
    }

    #[test]
    fn deletion_cancel_hard_key_still_exits() {
        let (_root, mut app) = app();
        app.ui_mode = UiMode::DeletionCancel {
            planned_entries: 1,
            completed: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        };

        let command = handle_keypress(&key(KeyCode::Char('c'), KeyModifiers::CONTROL), &mut app);
        assert!(matches!(command, InputCommand::HardCancel));
        assert!(!app.is_running);
    }

    #[test]
    fn planning_escape_cancels_pending_plan() {
        use crate::model::{EntrySnapshot, NodeId, NodeKind};
        use crate::state::FileToDelete;
        use crate::state::tiles::FileType;

        let (root, mut app) = app();
        // Mark loaded so normal_mode() returns Normal, not Loading.
        app.loaded = true;
        app.ui_mode = UiMode::PlanningDeletion(Box::new(FileToDelete {
            node_id: NodeId(1),
            synthetic: false,
            path_in_filesystem: root.path().to_path_buf(),
            path_to_file: vec!["target".into()],
            file_type: FileType::File,
            num_descendants: None,
            size: 0,
            expected_snapshot: EntrySnapshot {
                identity: None,
                kind: NodeKind::File,
                apparent_bytes: 0,
                allocated_bytes: None,
                modified_nanos: None,
            },
            reviewed_entries: Vec::new(),
        }));

        let command = handle_keypress(&key(KeyCode::Esc, KeyModifiers::NONE), &mut app);
        assert!(matches!(command, InputCommand::CancelDeletionPlan));
        assert!(matches!(app.ui_mode, UiMode::Normal));
    }
}
