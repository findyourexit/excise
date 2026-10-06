//! A screen-only driver for one live `excise`: what a person at a terminal could see and do, and
//! nothing a published release does not offer.
//!
//! The sweep runs builds that predate the test event channel, the frame marks, and the input
//! barrier, and it runs the candidate the same way, so that every column of the table is measured
//! with the same instruments. A [`Probe`] therefore reads only the screen model, the exit status,
//! the terminal modes, the output byte count, the process's CPU time and descriptors, and the
//! scratch area; it never sets `EXCISE_TEST_EVENTS`.
//!
//! # No deletion, ever
//!
//! A deletion dialog opens on one key, Backspace, and a program that has none open cannot be sent a
//! confirmation that does anything. The sweep never asks for a dialog: the keys a probe can send
//! are the closed vocabulary [`Key`] (the filter prompt's `/`, Enter, the arrows) and the
//! characters of a name, and every write is read through [`InputScan`] first, which says whether
//! the program could read the bytes so far as a request for a deletion (Backspace, Ctrl+H, or an
//! escape sequence that a later write finishes). A write that could is refused before it is sent.
//! With no request ever sent, no dialog can exist, so the Enter that applies a filter or opens a
//! folder can only do that. A probe never confirms a quit either: it ends a program with a signal
//! or `SIGKILL`. This is the same guarantee the scenario runner gives a build without frame marks
//! (`runner::live`): it refuses every deletion and every confirmation after one, and the sweep goes
//! further and never asks.
//!
//! Where the screen has to be read after a key (a prompt, the selected item), the probe reads it
//! after the output has been quiet for a while, bounded: the fallback `runner::live` uses for a
//! build that does not mark its frames, for what decides nothing destructive.

use std::{
    ffi::OsString,
    path::Path,
    time::{Duration, Instant},
};

use thiserror::Error;

use crate::{
    metrics::live_cpu_ms,
    pty::{
        ExitInfo, PtyError, PtySession, Screen, SpawnSpec, TerminalModes,
        input::InputScan,
        keys::encode_key,
        ui::{HeaderState, header_state},
    },
    runner::resolve_binary,
    safety::{FixtureRoot, ProfileSettings, Scratch, SignalError, isolated_env, send_signal},
    scenario::{KeyName, Profile, Signal},
};

/// The width of the terminal every probe runs in.
pub(crate) const COLS: u16 = 120;
/// The height of the terminal every probe runs in.
pub(crate) const ROWS: u16 = 40;
/// How long a wait blocks for terminal output before it looks at everything else again.
const POLL_INTERVAL: Duration = Duration::from_millis(1);
/// How long a program may take to end after its output stops, once its exit was seen.
const EXIT_OUTPUT_LIMIT: Duration = Duration::from_secs(2);

/// A probe could not be started or driven.
#[derive(Debug, Error)]
pub(crate) enum ProbeError {
    /// The binary cannot be used.
    #[error("{0}")]
    Binary(String),
    /// The pseudo-terminal failed.
    #[error(transparent)]
    Pty(#[from] PtyError),
    /// A signal could not be delivered.
    #[error(transparent)]
    Signal(#[from] SignalError),
    /// A write was refused before it was sent, because the program could read it as a request for
    /// a deletion.
    #[error("refused to send {0}: the program could read it as a request for a deletion")]
    Refused(String),
    /// Text that is not a name was offered.
    #[error("`{0}` is not text a probe types: only letters, digits, `_`, `.`, and `-`")]
    Text(String),
}

/// How a bounded wait ended.
#[derive(Debug)]
pub(crate) enum Wait<T> {
    /// The condition held.
    Ready(T),
    /// The deadline passed first.
    TimedOut,
    /// The program ended first, its exit seen and all its output read.
    Exited,
}

impl<T> Wait<T> {
    /// The value, when the condition held.
    pub(crate) fn ready(self) -> Option<T> {
        match self {
            Self::Ready(value) => Some(value),
            Self::TimedOut | Self::Exited => None,
        }
    }
}

/// The only keys a probe sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Key {
    /// `/`, which opens the filter prompt.
    Slash,
    /// Enter: opens a folder, or applies a filter.
    Enter,
    /// An arrow.
    Up,
    /// An arrow.
    Down,
    /// An arrow.
    Left,
    /// An arrow.
    Right,
}

impl Key {
    /// Every key, for the tests that hold the vocabulary to its promise.
    #[cfg(test)]
    pub(crate) const ALL: [Self; 6] = [
        Self::Slash,
        Self::Enter,
        Self::Up,
        Self::Down,
        Self::Left,
        Self::Right,
    ];

    /// The bytes a terminal sends for the key.
    pub(crate) fn bytes(self) -> Vec<u8> {
        let name = match self {
            Self::Slash => KeyName::Char('/'),
            Self::Enter => KeyName::Enter,
            Self::Up => KeyName::Up,
            Self::Down => KeyName::Down,
            Self::Left => KeyName::Left,
            Self::Right => KeyName::Right,
        };
        // None of these has a modifier, so each has an encoding.
        encode_key(name, false, false).unwrap_or_default()
    }
}

impl std::fmt::Display for Key {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Slash => "`/`",
            Self::Enter => "Enter",
            Self::Up => "Up",
            Self::Down => "Down",
            Self::Left => "Left",
            Self::Right => "Right",
        })
    }
}

/// What a probe starts.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ProbeSpec<'a> {
    /// The `excise` under test.
    pub binary: &'a Path,
    /// The fixture to scan: a directory that carries the ownership marker.
    pub fixture: &'a FixtureRoot,
    /// The scratch area the program runs in. It outlives the probe, so a second probe can start in
    /// what the first left.
    pub scratch: &'a Scratch,
    /// The profile it runs under.
    pub profile: Profile,
    /// How fast the terminal's output is read, in bytes per second, when it is slow.
    pub drain_bytes_per_sec: Option<u64>,
}

/// One live program in a pseudo-terminal of its own process group, ended when the probe is dropped.
pub(crate) struct Probe {
    session: PtySession,
    input: InputScan,
}

impl Probe {
    /// Starts the program with the isolated environment of the harness and no event channel.
    pub(crate) fn start(spec: &ProbeSpec<'_>) -> Result<Self, ProbeError> {
        let program =
            resolve_binary(spec.binary).map_err(|error| ProbeError::Binary(error.to_string()))?;
        let settings = ProfileSettings::for_profile(spec.profile);
        let session = PtySession::spawn(&SpawnSpec {
            program,
            args: vec![OsString::from(spec.fixture.path().as_os_str())],
            env: isolated_env(spec.scratch, spec.profile, false, None),
            cwd: spec.scratch.cwd(),
            cols: settings.cols.unwrap_or(COLS),
            rows: ROWS,
            drain_bytes_per_sec: spec.drain_bytes_per_sec,
            recording: None,
            title: None,
        })?;
        Ok(Self {
            session,
            input: InputScan::default(),
        })
    }

    /// The screen model.
    pub(crate) const fn screen(&self) -> &Screen {
        self.session.screen()
    }

    /// The scan state in the header badge, if the header is drawn.
    pub(crate) fn header(&self) -> Option<HeaderState> {
        header_state(self.session.screen())
    }

    /// How the program ended, once it has.
    pub(crate) const fn exit(&self) -> Option<ExitInfo> {
        self.session.exit()
    }

    /// The terminal modes now, which the pseudo-terminal keeps after the program is gone.
    pub(crate) fn modes(&self) -> TerminalModes {
        self.session.modes()
    }

    /// The bytes of terminal output read so far.
    pub(crate) const fn output_bytes(&self) -> u64 {
        self.session.output_bytes()
    }

    /// The CPU time the program has used so far, in milliseconds, where the platform can say.
    pub(crate) fn cpu_ms(&self) -> Option<f64> {
        live_cpu_ms(self.session.pid())
    }

    /// The most descriptors the program held at one of the samples taken while it ran.
    pub(crate) const fn max_fds(&self) -> Option<u32> {
        self.session.sampler().max_fds()
    }

    /// Drives the session until `ready` returns a value, `deadline` passes, or the program ends.
    pub(crate) fn wait_until<T>(
        &mut self,
        deadline: Instant,
        mut ready: impl FnMut(&Self) -> Option<T>,
    ) -> Result<Wait<T>, ProbeError> {
        loop {
            self.session.pump()?;
            if let Some(value) = ready(self) {
                return Ok(Wait::Ready(value));
            }
            if self.session.finished() {
                return Ok(Wait::Exited);
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(Wait::TimedOut);
            }
            self.session
                .wait_activity(deadline.saturating_duration_since(now).min(POLL_INTERVAL))?;
        }
    }

    /// Waits for the header badge to read COMPLETE and returns how long after the spawn that was
    /// first seen. The badge is the only valid completion signal: the word also appears in the
    /// inspector while a scan is still running.
    pub(crate) fn wait_complete(&mut self, bound: Duration) -> Result<Wait<Duration>, ProbeError> {
        let deadline = Instant::now() + bound;
        let started = self.session.started();
        self.wait_until(deadline, |probe| {
            matches!(probe.header(), Some(HeaderState::Complete)).then(|| started.elapsed())
        })
    }

    /// Lets `duration` pass with nothing sent, reading what the program writes. Returns whether
    /// the program ended first.
    pub(crate) fn pass(&mut self, duration: Duration) -> Result<bool, ProbeError> {
        Ok(matches!(
            self.wait_until(Instant::now() + duration, |_| None::<()>)?,
            Wait::Exited
        ))
    }

    /// Reads on until the output has been quiet for `quiet`, but for no longer than `limit`: the
    /// bounded read that stands in for a frame mark in a build that marks none.
    pub(crate) fn settle(&mut self, quiet: Duration, limit: Duration) -> Result<(), ProbeError> {
        Ok(self.session.drain(quiet, limit)?)
    }

    /// Sends one key, after checking that the program cannot read it as a request for a deletion.
    pub(crate) fn send(&mut self, key: Key) -> Result<(), ProbeError> {
        self.write(&key.bytes(), &key.to_string())
    }

    /// Types `text`, one key press per character. Only letters, digits, `_`, `.`, and `-` are
    /// text: a name, never a control sequence.
    pub(crate) fn type_text(&mut self, text: &str) -> Result<(), ProbeError> {
        if text.is_empty()
            || !text
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
        {
            return Err(ProbeError::Text(text.to_owned()));
        }
        for character in text.chars() {
            let mut utf8 = [0_u8; 4];
            let bytes = character.encode_utf8(&mut utf8).as_bytes().to_vec();
            self.write(&bytes, &format!("`{character}`"))?;
        }
        Ok(())
    }

    fn write(&mut self, bytes: &[u8], what: &str) -> Result<(), ProbeError> {
        if self.input.peek(bytes).request {
            return Err(ProbeError::Refused(what.to_owned()));
        }
        self.input.note(bytes);
        self.session.send(bytes)?;
        Ok(())
    }

    /// Delivers `signal`: a Unix signal to the program, or the console close.
    pub(crate) fn signal(&mut self, signal: Signal) -> Result<(), ProbeError> {
        self.session.pump()?;
        match signal {
            Signal::Close => self.session.close_console(),
            other => send_signal(self.session.pid(), other)?,
        }
        Ok(())
    }

    /// Waits for the program to end and its output to be read, for at most `bound`, and returns how
    /// it ended.
    pub(crate) fn wait_exit(&mut self, bound: Duration) -> Result<Option<ExitInfo>, ProbeError> {
        let waited = self.wait_until(Instant::now() + bound, |probe| probe.exit().map(|_| ()))?;
        if matches!(waited, Wait::TimedOut) {
            return Ok(None);
        }
        let deadline = Instant::now() + EXIT_OUTPUT_LIMIT;
        while !self.session.finished() && Instant::now() < deadline {
            self.session.pump()?;
            self.session.wait_activity(POLL_INTERVAL)?;
        }
        self.session.pump()?;
        Ok(self.session.exit())
    }

    /// Ends the program and everything it started with `SIGKILL` (or the platform's equivalent), and
    /// reaps it.
    pub(crate) fn kill(&mut self) {
        self.session.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_key_of_the_vocabulary_can_be_read_as_a_request_for_a_deletion() {
        for key in Key::ALL {
            let bytes = key.bytes();
            assert!(!bytes.is_empty(), "{key} has an encoding");
            assert!(
                !InputScan::default().peek(&bytes).request,
                "{key} ({bytes:?}) must not ask for a deletion"
            );
        }
    }

    #[test]
    fn no_two_keys_in_a_row_can_be_read_as_a_request_for_a_deletion() {
        for first in Key::ALL {
            for second in Key::ALL {
                let mut scan = InputScan::default();
                assert!(!scan.note(&first.bytes()).request, "{first}");
                assert!(
                    !scan.note(&second.bytes()).request,
                    "{first} then {second} must not ask for a deletion"
                );
            }
        }
    }

    #[test]
    fn the_keys_are_the_bytes_a_terminal_sends() {
        assert_eq!(Key::Slash.bytes(), b"/");
        assert_eq!(Key::Enter.bytes(), b"\r");
        assert_eq!(Key::Up.bytes(), b"\x1b[A");
        assert_eq!(Key::Down.bytes(), b"\x1b[B");
        assert_eq!(Key::Right.bytes(), b"\x1b[C");
        assert_eq!(Key::Left.bytes(), b"\x1b[D");
    }

    #[test]
    fn the_names_a_probe_types_are_letters_digits_and_a_few_marks() {
        for text in ["part00", "victim", "node_modules", "a.b-c"] {
            let mut scan = InputScan::default();
            for character in text.chars() {
                let mut utf8 = [0_u8; 4];
                let bytes = character.encode_utf8(&mut utf8).as_bytes().to_vec();
                assert!(!scan.note(&bytes).request, "{text}");
            }
        }
    }
}
