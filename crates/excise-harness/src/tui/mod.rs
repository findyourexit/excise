//! The interactive session driver behind `cargo xtask tui`: one live `excise` session at a time,
//! driven step by step, on a fixture only.
//!
//! Scenarios replay fixed scripts. An agent or a person investigating the interface wants to open
//! a session, send keys, read the screen and the event stream, and close it, without writing a
//! scenario first and without touching a real path. This module is that driver, and everything in
//! it is the harness's own: the pseudo-terminal session and its screen model ([`crate::pty`]), the
//! key encoding, the event channel reader ([`crate::events`]), the fixture generator
//! ([`crate::fixture`]), the profiles and the isolated environment ([`crate::safety`]), and, from
//! the scenario runner, the way a step waits for the program to settle and the whole protocol of
//! a deletion (`crate::runner::live`).
//!
//! # Commands
//!
//! Every command prints exactly one [`HarnessTui`](crate::report::HarnessTui) document, and a
//! failure is one too; see [`Command`] and [`execute`].
//!
//! | Command | What it does |
//! |---|---|
//! | `open` | Makes a run copy of a fixture, starts the release `excise` on it, and returns the session id and the first screen. |
//! | `keys` | Sends keys, waits for the program to settle, and returns the screen and the events since the previous command, with the frames summarized. |
//! | `delete` | Deletes one entry of the folder the session shows through the verified deletion protocol; an entry elsewhere fails at once with `not_in_view`. |
//! | `screen` | The screen, its cursor, modes, boxes, and dialog. |
//! | `events` | The event channel's records. |
//! | `close` | Quits the program the way a user does, and removes everything of the session. |
//! | `list` | The open sessions; a session whose supervisor is gone is reported and cleaned. |
//!
//! # Sessions
//!
//! A session outlives the command that opened it, because one **supervisor** process per session
//! owns the pseudo-terminal, the screen model, and the event reader ([`serve`]). The commands talk
//! to it through files in the session directory, `target/excise-tui/<id>/` ([`mailbox`]): no
//! socket, no port, and nothing reachable from anywhere but this machine's file system.
//!
//! A supervisor ends, and removes everything of its session, after its idle timeout (15 minutes
//! unless `open` says otherwise), when the program ends, and on `close`. A session whose
//! supervisor died without cleaning up is *stale*: `open` and `list` find it (the supervisor's lock
//! is free), kill its program if it still runs and can be proven to be the session's (its recorded
//! start time and executable, and a mark in its environment that only the session's processes
//! carry: see [`identity`]), and remove its files ([`stale`]). A process that cannot be proven to
//! be the session's is never signaled, whatever its id and its command line say.
//!
//! # Safety
//!
//! * **Fixtures only.** `open` takes the id of one of the harness's fixture specs, never a path; the
//!   program runs on a fresh run copy that carries the ownership marker, in an isolated
//!   environment, and every runner refuses a root without the marker.
//! * **Deletions only through `delete`.** It runs the protocol of the scenario `delete` step, and
//!   `keys` refuses any key that could confirm a deletion dialog (see the supervisor).
//! * **No orphans, no residue.** Every wait is bounded, the program's whole process group is killed
//!   on a timeout, and every end of a session removes the run copy, the scratch area, and the
//!   session directory.
//!
//! # Platforms
//!
//! macOS and Linux. Windows builds this module, and every command there fails with a document that
//! names the platform: the driver needs Unix process groups and file locks, and a session driver
//! nobody has run on Windows would promise more than it is known to do.

mod command;
mod duration;
mod keys;

#[cfg(unix)]
mod client;
#[cfg(unix)]
mod identity;
#[cfg(unix)]
mod layout;
#[cfg(unix)]
mod mailbox;
#[cfg(unix)]
mod screen_info;
#[cfg(unix)]
mod stale;
#[cfg(unix)]
mod supervisor;
#[cfg(all(unix, test))]
mod testing;

pub use command::{
    Command, DEFAULT_DELETE_TIMEOUT, DEFAULT_IDLE_TIMEOUT, DEFAULT_KEYS_TIMEOUT, DEFAULT_SIZE,
    Launcher, MAX_COLS, MAX_COMMAND_TIMEOUT, MAX_IDLE_TIMEOUT, MAX_ROWS, MIN_IDLE_TIMEOUT,
    OpenRequest, Options, Outcome, check_fixture, parse_size,
};
pub use duration::parse_duration;
pub use keys::{Input, parse_keys};

#[cfg(unix)]
pub use client::execute;
#[cfg(unix)]
pub use supervisor::{ServeError, serve};

/// Whether the driver runs on this platform. Where it does not, every command fails with a
/// document that names the platform.
pub const SUPPORTED: bool = cfg!(unix);

/// The name of the directory sessions live in, below the target directory.
#[cfg(unix)]
pub const STATE_DIR_NAME: &str = layout::STATE_DIR_NAME;
/// The name of the directory sessions live in, below the target directory.
#[cfg(not(unix))]
pub const STATE_DIR_NAME: &str = "excise-tui";

/// Runs `command`. On this platform every command fails, with a document that names the platform.
#[cfg(not(unix))]
#[must_use]
pub fn execute(_options: &Options, command: &Command) -> Outcome {
    use crate::report::tui::{TuiError, TuiErrorKind};

    Outcome::failure(
        Some(command.name()),
        None,
        TuiError::new(
            TuiErrorKind::UnsupportedPlatform,
            format!(
                "`cargo xtask tui` needs Unix pseudo-terminals, process groups, and file locks, \
                 and runs on macOS and Linux; this platform is `{}`",
                std::env::consts::OS
            ),
        ),
    )
}

/// The supervisor of a session cannot run on this platform.
#[cfg(not(unix))]
#[derive(Debug, thiserror::Error)]
#[error(
    "`cargo xtask tui` runs on macOS and Linux; this platform is `{}`",
    std::env::consts::OS
)]
pub struct ServeError;

/// Runs a session's supervisor. On this platform it fails, naming the platform.
///
/// # Errors
///
/// Always: the driver does not run here.
#[cfg(not(unix))]
pub fn serve(_session_dir: &std::path::Path) -> Result<(), ServeError> {
    Err(ServeError)
}
