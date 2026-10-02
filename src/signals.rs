//! Delivers an external signal (Unix) or console control event (Windows) into the owner loop
//! and the headless scanner as one confirmed-quit request, instead of the process
//! being killed outright with none of its cleanup run. See `docs/architecture/threat-model.md`'s
//! "Terminal restoration" control and `docs/architecture/overview.md`'s Main Loop section.
//!
//! Nothing in the in-process test or scenario runners can deliver a real signal or console
//! event; [`install`] is called only by `cli::run_main`, once per process.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crossbeam_channel::Receiver;
#[cfg(windows)]
use crossbeam_channel::Sender;

/// A request to end the run immediately as a confirmed quit: cancel pending plans, stop an
/// active deletion at its next entry boundary, restore the terminal if one is attached, remove
/// session storage, and exit with the `Interrupted` class.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StopRequest {
    /// The first request. The owner loop (or, headless, the scan loop) waits for that quit to
    /// settle before exiting: an active deletion's current entry, any dialog bookkeeping, and
    /// (interactively) a rescan the interrupted deletion may have scheduled.
    Graceful,
    /// A further request while the first is still settling: exit immediately instead of waiting
    /// out anything the first started, other than what the executor's own entry-boundary check
    /// and the worker shutdown every exit already goes through still bound.
    Forced,
}

/// Installed once per process, before the scan or terminal session begins.
pub(crate) struct StopSignals {
    receiver: Receiver<StopRequest>,
    #[cfg(windows)]
    done: Sender<()>,
}

impl StopSignals {
    /// A clone of the receiving end, held by the owner loop or the headless scan loop.
    pub(crate) fn receiver(&self) -> Receiver<StopRequest> {
        self.receiver.clone()
    }

    /// Tells a Windows console control handler that is waiting for this run's confirmed quit to
    /// finish (terminal restored, session storage removed, any report written) that it has: see
    /// `os::windows::install_console_ctrl_handler`. Idempotent and a no-op on Unix, where nothing
    /// waits on it and process exit is not on a deadline the way a Windows close, logoff, or
    /// shutdown event is. Called exactly once, by `cli::run_main`, after every other exit step.
    #[allow(
        clippy::unused_self,
        reason = "self is read on Windows (the `done` sender); unused on Unix, where nothing \
                  waits on it"
    )]
    pub(crate) fn acknowledge_done(&self) {
        #[cfg(windows)]
        let _ = self.done.send(());
    }
}

/// This run's headless scan-store session directory, set once by
/// `runtime::scan_headless_with_stop_signals` right after creating its session so a second
/// confirmed-quit request can remove it directly (see [`act_on_second_request`]); left unset
/// for the lifetime of an interactive (TUI) run, which never calls the setter.
static HEADLESS_SESSION_ROOT: OnceLock<PathBuf> = OnceLock::new();

/// Records this run's headless scan-store session directory for a second confirmed-quit
/// request to remove directly. A later call than the first is a caller bug (one process runs
/// one headless scan), so it is ignored rather than panicking: the first recorded path remains
/// the one a second request acts on.
pub(crate) fn record_headless_session_root(root: PathBuf) {
    let _ = HEADLESS_SESSION_ROOT.set(root);
}

/// What a second confirmed-quit request does, decided once from whether this run ever recorded
/// a headless session directory. Pure and side-effect free on purpose: the decision itself is
/// unit-testable without ever calling `std::process::exit`, unlike acting on it
/// ([`act_on_second_request`]).
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ForcedExitAction {
    /// Interactive: nothing here owns a session directory to remove. Fall back to sending
    /// `Forced` through the channel, for the owner loop's own handling to end the run.
    DeferToOwnerLoop,
    /// Headless: this run's session directory, to be removed outright before exiting, because a
    /// long, otherwise-uninterruptible phase (publication, building the report) has no other
    /// bound than this.
    RemoveAndExit(PathBuf),
}

fn forced_exit_action(headless_session_root: Option<&Path>) -> ForcedExitAction {
    match headless_session_root {
        Some(root) => ForcedExitAction::RemoveAndExit(root.to_path_buf()),
        None => ForcedExitAction::DeferToOwnerLoop,
    }
}

/// Carries out a second confirmed-quit request's bound immediate exit for a headless run, or
/// returns so the caller falls back to its usual `Forced` handling (interactive: there is no
/// recorded session directory here to act on). Removing the directory before exiting uses the
/// same `fs::remove_dir_all` normal unwinding already uses elsewhere (hardened against a path
/// replaced by a symlink after creation; std's recursive removal does not follow one); a
/// directory already gone, or any other failure to remove it, does not stop the exit that
/// follows - nothing could act on that error once the process is gone anyway, and a partially
/// written `--output` file from whatever the main thread was doing is simply left behind for the
/// caller to discard (see `docs/reports.md`).
///
/// Never returns when it removes a directory: ends the process directly, bypassing whatever the
/// main thread is doing.
pub(crate) fn act_on_second_request() {
    if let ForcedExitAction::RemoveAndExit(root) =
        forced_exit_action(HEADLESS_SESSION_ROOT.get().map(PathBuf::as_path))
    {
        let _ = fs::remove_dir_all(&root);
        std::process::exit(crate::error::ExitClass::Interrupted.code());
    }
}

/// Thread name for the dedicated Unix signal-listener thread.
#[cfg(unix)]
const LISTENER_THREAD_NAME: &str = "excise-signal-listener";

#[cfg(unix)]
pub(crate) fn install() -> io::Result<StopSignals> {
    use signal_hook::consts::signal::{SIGHUP, SIGQUIT, SIGTERM};
    use signal_hook::iterator::Signals;

    let mut signals = Signals::new([SIGTERM, SIGHUP, SIGQUIT])?;
    let (sender, receiver) = crossbeam_channel::unbounded();
    std::thread::Builder::new()
        .name(LISTENER_THREAD_NAME.to_string())
        .spawn(move || {
            let mut seen_first = false;
            for _signal in signals.forever() {
                let request = if seen_first {
                    StopRequest::Forced
                } else {
                    StopRequest::Graceful
                };
                seen_first = true;
                if matches!(request, StopRequest::Forced) {
                    act_on_second_request();
                }
                if sender.send(request).is_err() {
                    // The receiving end is gone (the run already finished): nothing left to
                    // notify, and the process is already on its way out regardless.
                    break;
                }
            }
        })?;
    Ok(StopSignals { receiver })
}

#[cfg(windows)]
pub(crate) fn install() -> io::Result<StopSignals> {
    let (receiver, done) = crate::os::windows::install_console_ctrl_handler()?;
    Ok(StopSignals { receiver, done })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{ForcedExitAction, forced_exit_action};

    #[test]
    fn an_interactive_run_with_no_recorded_session_defers_to_the_owner_loop() {
        assert_eq!(forced_exit_action(None), ForcedExitAction::DeferToOwnerLoop);
    }

    #[test]
    fn a_headless_run_with_a_recorded_session_removes_it_before_exiting() {
        let root = PathBuf::from("/some/recorded/headless/session/root");
        assert_eq!(
            forced_exit_action(Some(&root)),
            ForcedExitAction::RemoveAndExit(root)
        );
    }
}
