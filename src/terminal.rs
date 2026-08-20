use std::io::{self, IsTerminal as _};
use std::panic::{self, PanicHookInfo};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crossterm::cursor::Show;
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::style::ResetColor;
use crossterm::terminal::{
    DisableLineWrap, EnableLineWrap, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
    enable_raw_mode,
};
use thiserror::Error;

use crate::error::AppError;

type PanicHook = Box<dyn for<'a> Fn(&PanicHookInfo<'a>) + Send + Sync + 'static>;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TerminalState {
    #[default]
    Inactive,
    Raw,
    Active,
    Restored,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalTransition {
    EnterRaw,
    EnterAlternate,
    Restore,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("invalid terminal transition {transition:?} from {state:?}")]
pub struct TerminalTransitionError {
    state: TerminalState,
    transition: TerminalTransition,
}

impl TerminalState {
    /// # Errors
    /// Returns a transition error when the requested lifecycle edge is invalid.
    pub fn transition(
        self,
        transition: TerminalTransition,
    ) -> Result<Self, TerminalTransitionError> {
        match (self, transition) {
            (Self::Inactive, TerminalTransition::EnterRaw) => Ok(Self::Raw),
            (Self::Raw, TerminalTransition::EnterAlternate) => Ok(Self::Active),
            (Self::Inactive | Self::Raw | Self::Active, TerminalTransition::Restore)
            | (Self::Restored, TerminalTransition::Restore) => Ok(Self::Restored),
            (state, transition) => Err(TerminalTransitionError { state, transition }),
        }
    }

    const fn has_raw_mode(self) -> bool {
        matches!(self, Self::Raw | Self::Active)
    }

    const fn has_alternate_screen(self) -> bool {
        matches!(self, Self::Active)
    }
}

/// # Errors
/// Returns a TTY error when stdin or stdout is not attached to a terminal.
pub fn validate_terminal() -> Result<(), AppError> {
    if !io::stdin().is_terminal() {
        return Err(AppError::Tty("standard input is not a TTY".to_string()));
    }
    if !io::stdout().is_terminal() {
        return Err(AppError::Tty("standard output is not a TTY".to_string()));
    }
    Ok(())
}

pub struct TerminalSession {
    state: TerminalState,
    active: Arc<AtomicBool>,
    previous_panic_hook: Arc<Mutex<Option<PanicHook>>>,
}

impl TerminalSession {
    /// # Errors
    /// Returns a terminal error if raw mode or alternate-screen entry fails.
    pub fn enter() -> Result<Self, AppError> {
        Self::enter_with_mouse(false)
    }

    /// # Errors
    /// Returns a terminal error if raw mode or alternate-screen entry fails.
    pub fn enter_with_mouse(mouse_capture: bool) -> Result<Self, AppError> {
        let mut session = Self {
            state: TerminalState::Inactive,
            active: Arc::new(AtomicBool::new(false)),
            previous_panic_hook: Arc::new(Mutex::new(None)),
        };

        enable_raw_mode().map_err(|error| AppError::terminal("raw-mode entry", error))?;
        session.state = session
            .state
            .transition(TerminalTransition::EnterRaw)
            .map_err(|error| AppError::Invariant(error.to_string()))?;
        session.active.store(true, Ordering::Release);
        let enter_result = if mouse_capture {
            execute!(
                io::stdout(),
                EnterAlternateScreen,
                EnableMouseCapture,
                DisableLineWrap
            )
        } else {
            execute!(io::stdout(), EnterAlternateScreen, DisableLineWrap)
        };
        if let Err(error) = enter_result {
            let _ = session.restore();
            return Err(AppError::terminal("alternate-screen entry", error));
        }
        session.state = session
            .state
            .transition(TerminalTransition::EnterAlternate)
            .map_err(|error| AppError::Invariant(error.to_string()))?;
        session.install_panic_hook();
        Ok(session)
    }

    /// # Errors
    /// Returns a terminal error if any explicit restoration operation fails.
    pub fn restore(&mut self) -> Result<(), AppError> {
        if self.state == TerminalState::Restored {
            return Ok(());
        }

        let was_active = self.active.swap(false, Ordering::AcqRel);
        let terminal_result = if was_active && self.state.has_alternate_screen() {
            restore_commands().map_err(|error| AppError::terminal("restoration", error))
        } else {
            Ok(())
        };
        let raw_result = if was_active && self.state.has_raw_mode() {
            disable_raw_mode().map_err(|error| AppError::terminal("raw-mode restoration", error))
        } else {
            Ok(())
        };
        self.state = self
            .state
            .transition(TerminalTransition::Restore)
            .map_err(|error| AppError::Invariant(error.to_string()))?;
        self.restore_panic_hook();

        terminal_result.and(raw_result)
    }

    fn install_panic_hook(&mut self) {
        let previous = panic::take_hook();
        *self
            .previous_panic_hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(previous);
        let active = self.active.clone();
        let previous = self.previous_panic_hook.clone();
        panic::set_hook(Box::new(move |info| {
            if active.swap(false, Ordering::AcqRel) {
                let _ = restore_commands();
                let _ = disable_raw_mode();
            }
            if let Some(previous) = previous
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
            {
                previous(info);
            }
        }));
    }

    fn restore_panic_hook(&mut self) {
        if std::thread::panicking() {
            return;
        }
        let _installed = panic::take_hook();
        if let Some(previous) = self
            .previous_panic_hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            panic::set_hook(previous);
        }
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

fn restore_commands() -> io::Result<()> {
    execute!(
        io::stdout(),
        ResetColor,
        Show,
        DisableMouseCapture,
        EnableLineWrap,
        LeaveAlternateScreen
    )
}
