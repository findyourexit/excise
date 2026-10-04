//! What a `cargo xtask tui` command line asks for, in the form the driver executes.

use std::{ffi::OsString, path::PathBuf, time::Duration};

use crate::{
    fixture::{Fixtures, spec::Part},
    report::{
        HarnessTui,
        tui::{TuiCommand, TuiError, TuiErrorKind},
    },
    scenario::{EntryKind, MAX_TIMEOUT_MS, MIN_TERMINAL_COLS, MIN_TERMINAL_ROWS, Profile},
};

/// How long a session may go without a command before it ends, unless `open` says otherwise.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_mins(15);
/// The shortest idle timeout `open` accepts.
pub const MIN_IDLE_TIMEOUT: Duration = Duration::from_secs(1);
/// The longest idle timeout `open` accepts.
pub const MAX_IDLE_TIMEOUT: Duration = Duration::from_hours(24);
/// How long `keys` waits for the program to draw a frame that counts what was sent.
pub const DEFAULT_KEYS_TIMEOUT: Duration = Duration::from_secs(2);
/// The bound of a whole `delete`: the scan completing, the entry selected, the dialog verified,
/// and the deletion finished.
pub const DEFAULT_DELETE_TIMEOUT: Duration = Duration::from_secs(60);
/// The longest bound `keys` and `delete` accept: the longest a scenario step may wait.
pub const MAX_COMMAND_TIMEOUT: Duration = Duration::from_millis(MAX_TIMEOUT_MS);
/// The default terminal size, as a scenario's.
pub const DEFAULT_SIZE: (u16, u16) = (
    crate::scenario::DEFAULT_TERMINAL_COLS,
    crate::scenario::DEFAULT_TERMINAL_ROWS,
);
/// The widest terminal `open` accepts.
pub const MAX_COLS: u16 = 300;
/// The tallest terminal `open` accepts.
pub const MAX_ROWS: u16 = 100;

/// What `open` asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenRequest {
    /// The id of the fixture to open: one of the specs the harness ships. Never a path.
    pub fixture: String,
    /// The profile to run under.
    pub profile: Profile,
    /// The terminal size, as columns and rows. A profile can fix the width.
    pub size: (u16, u16),
    /// Whether to record the session as an asciicast that outlives it.
    pub record: bool,
    /// How long the session may go without a command before it ends.
    pub idle_timeout: Duration,
    /// The `excise` binary to run on the fixture.
    pub binary: PathBuf,
}

/// One command of `cargo xtask tui`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Starts a session.
    Open(OpenRequest),
    /// Sends keys.
    Keys {
        /// The session.
        session: String,
        /// The keys, in the notation of [`parse_keys`](crate::tui::parse_keys).
        keys: Vec<String>,
        /// How long to wait for the program to draw a frame that counts the keys.
        timeout: Duration,
    },
    /// Deletes one entry of the fixture.
    Delete {
        /// The session.
        session: String,
        /// The fixture-relative path of the entry.
        name: String,
        /// Whether the entry is a file or a folder.
        kind: EntryKind,
        /// The bound of the whole command.
        timeout: Duration,
    },
    /// Reads the screen.
    Screen {
        /// The session.
        session: String,
    },
    /// Reads the event channel's records.
    Events {
        /// The session.
        session: String,
        /// The index of the first record; the first record when `None`.
        since: Option<u64>,
    },
    /// Ends a session.
    Close {
        /// The session.
        session: String,
    },
    /// Lists the open sessions.
    List,
}

impl Command {
    /// The command's name.
    #[must_use]
    pub const fn name(&self) -> TuiCommand {
        match self {
            Self::Open(_) => TuiCommand::Open,
            Self::Keys { .. } => TuiCommand::Keys,
            Self::Delete { .. } => TuiCommand::Delete,
            Self::Screen { .. } => TuiCommand::Screen,
            Self::Events { .. } => TuiCommand::Events,
            Self::Close { .. } => TuiCommand::Close,
            Self::List => TuiCommand::List,
        }
    }
}

/// How to start a supervisor: the program, and the arguments before the session directory. The
/// supervisor is started as `program args... <session directory>` and runs [`serve`](crate::tui::serve).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launcher {
    /// The program that runs a supervisor.
    pub program: PathBuf,
    /// Its arguments, before the session directory.
    pub args: Vec<OsString>,
}

/// Where sessions live and how to start one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// The directory of the sessions: `<target dir>/excise-tui`.
    pub state_dir: PathBuf,
    /// How a session's supervisor is started.
    pub launcher: Launcher,
}

/// What a command came to: the document it prints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// The document, the only thing the command prints on stdout.
    pub document: HarnessTui,
}

impl Outcome {
    /// A command that failed.
    #[must_use]
    pub fn failure(command: Option<TuiCommand>, session: Option<String>, error: TuiError) -> Self {
        Self {
            document: HarnessTui::failure(command, session, error),
        }
    }

    /// Whether the command did what it was asked.
    #[must_use]
    pub const fn is_success(&self) -> bool {
        self.document.ok
    }

    /// The process exit code of the command: 0 on success, 2 for a command line that is not
    /// valid, and 1 for any other failure.
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        match &self.document.error {
            None => 0,
            Some(error) if error.kind == TuiErrorKind::Usage => 2,
            Some(_) => 1,
        }
    }
}

/// Parses a terminal size written `<columns>x<rows>`.
///
/// # Errors
///
/// Returns why the text is not a size the driver accepts: not two numbers, below the narrowest
/// and shortest terminal a scenario may start in (32 by 8), or above 300 by 100.
pub fn parse_size(text: &str) -> Result<(u16, u16), String> {
    let invalid =
        || format!("`{text}` is not a size: write it <columns>x<rows>, for example 120x40");
    let (cols, rows) = text.split_once('x').ok_or_else(invalid)?;
    let (Ok(cols), Ok(rows)) = (cols.parse::<u16>(), rows.parse::<u16>()) else {
        return Err(invalid());
    };
    if cols < MIN_TERMINAL_COLS || rows < MIN_TERMINAL_ROWS {
        return Err(format!(
            "`{text}` is smaller than the smallest terminal a session starts in, \
             {MIN_TERMINAL_COLS}x{MIN_TERMINAL_ROWS}"
        ));
    }
    if cols > MAX_COLS || rows > MAX_ROWS {
        return Err(format!(
            "`{text}` is larger than the largest terminal a session starts in, {MAX_COLS}x{MAX_ROWS}"
        ));
    }
    Ok((cols, rows))
}

/// Checks that `id` names a fixture the driver can open: one of the bundled fixture specs, and
/// one that needs no volume.
///
/// This is all of `open`'s validation of the fixture. It needs no session, so a front end can say
/// that a fixture id is wrong before it spends minutes building the binary.
///
/// # Errors
///
/// Returns a `usage` error that names the fixtures there are.
pub fn check_fixture(id: &str) -> Result<(), TuiError> {
    let fixtures = Fixtures::bundled();
    let spec = match fixtures.spec(id) {
        Ok(spec) => spec,
        Err(error) => {
            let known = fixtures.ids().unwrap_or_default().join(", ");
            return Err(TuiError::new(
                TuiErrorKind::Usage,
                format!("fixture `{id}`: {error}; the fixtures are {known}"),
            ));
        }
    };
    if spec
        .parts
        .iter()
        .any(|part| matches!(part, Part::Volume(_)))
    {
        return Err(TuiError::new(
            TuiErrorKind::Usage,
            format!("the fixture `{id}` needs a volume, which the driver never attaches"),
        ));
    }
    Ok(())
}
