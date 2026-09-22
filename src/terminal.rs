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

/// Writes terminal output while splitting the paired truecolour SGR command
/// emitted by `CrosstermBackend`. Each colour command is otherwise unchanged.
/// Some terminal renderers parse the foreground half but display the paired
/// background parameters as text, so equivalent sequential commands are safer.
pub(crate) struct SplitColorWriter<W> {
    inner: W,
    pending_csi: Vec<u8>,
}

impl<W> SplitColorWriter<W> {
    pub(crate) fn new(inner: W) -> Self {
        Self {
            inner,
            pending_csi: Vec::with_capacity(48),
        }
    }

    fn write_pending_csi(&mut self) -> io::Result<()>
    where
        W: io::Write,
    {
        if let Some(background_start) = combined_truecolour_background_start(&self.pending_csi) {
            // The separator before the background belongs to neither new
            // command: terminate the foreground, then begin a new CSI.
            self.inner
                .write_all(&self.pending_csi[..background_start - 1])?;
            self.inner.write_all(b"m\x1b[")?;
            self.inner
                .write_all(&self.pending_csi[background_start..])?;
        } else {
            self.inner.write_all(&self.pending_csi)?;
        }
        self.pending_csi.clear();
        Ok(())
    }
}

impl<W> io::Write for SplitColorWriter<W>
where
    W: io::Write,
{
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let mut offset = 0;
        while offset < buffer.len() {
            if self.pending_csi.is_empty() {
                let Some(escape_offset) = buffer[offset..].iter().position(|&byte| byte == b'\x1b')
                else {
                    self.inner.write_all(&buffer[offset..])?;
                    break;
                };
                let escape = offset + escape_offset;
                self.inner.write_all(&buffer[offset..escape])?;
                self.pending_csi.push(b'\x1b');
                offset = escape + 1;
                continue;
            }

            self.pending_csi.push(buffer[offset]);
            offset += 1;
            let complete_csi = self.pending_csi.len() > 2
                && self
                    .pending_csi
                    .last()
                    .is_some_and(|byte| (b'@'..=b'~').contains(byte));
            if (self.pending_csi.len() == 2 && self.pending_csi[1] != b'[')
                || complete_csi
                || self.pending_csi.len() >= 64
            {
                self.write_pending_csi()?;
            }
        }
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.write_pending_csi()?;
        self.inner.flush()
    }
}

/// Locates the truecolour background parameter after a complete truecolour
/// foreground parameter. Counting the three foreground components prevents a
/// red component with the value `48` from being mistaken for a background SGR.
fn combined_truecolour_background_start(sequence: &[u8]) -> Option<usize> {
    let mut remainder = sequence.strip_prefix(b"\x1b[38;2;")?;
    for _ in 0..3 {
        let delimiter = remainder.iter().position(|&byte| byte == b';')?;
        remainder = &remainder[delimiter + 1..];
    }
    remainder
        .starts_with(b"48;2;")
        .then_some(sequence.len() - remainder.len())
}

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
    mouse_capture: bool,
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
            mouse_capture,
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
            restore_commands(self.mouse_capture)
                .map_err(|error| AppError::terminal("restoration", error))
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
        let mouse_capture = self.mouse_capture;
        panic::set_hook(Box::new(move |info| {
            if active.swap(false, Ordering::AcqRel) {
                let _ = restore_commands(mouse_capture);
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

fn restore_commands(mouse_capture: bool) -> io::Result<()> {
    if mouse_capture {
        execute!(
            io::stdout(),
            ResetColor,
            Show,
            DisableMouseCapture,
            EnableLineWrap,
            LeaveAlternateScreen
        )
    } else {
        execute!(
            io::stdout(),
            ResetColor,
            Show,
            EnableLineWrap,
            LeaveAlternateScreen
        )
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::SplitColorWriter;

    #[test]
    fn split_color_writer_keeps_truecolour_commands_separate_across_writes() {
        let mut writer = SplitColorWriter::new(Vec::new());
        writer
            .write_all(b"\x1b[38;2;48;192;149;48")
            .expect("first ANSI fragment should write");
        writer
            .write_all(b";2;30;30;46mX")
            .expect("second ANSI fragment should write");
        writer.flush().expect("ANSI output should flush");

        assert_eq!(writer.inner, b"\x1b[38;2;48;192;149m\x1b[48;2;30;30;46mX");
    }
}
