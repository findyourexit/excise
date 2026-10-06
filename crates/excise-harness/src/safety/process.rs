//! Process-group control.
//!
//! # Unix
//!
//! The pseudo-terminal library starts a child with `setsid`, so the child leads a new session and
//! its process group id is its pid. Every process it forks stays in that group, so signalling the
//! group reaches all of them. The calls go through `rustix`, which wraps them safely.
//!
//! # Windows
//!
//! There is no process group to signal. The pseudo-terminal runner ends a child with the library's
//! `TerminateProcess` call, which does not reach descendants; `excise` starts none. A job object
//! would, but creating one needs `unsafe`, which this workspace does not allow outside
//! `src/os/windows.rs`. The `signal` step's `close` event does not go through [`send_signal`]: it
//! is delivered by closing the pseudo console (`pty::session::PtySession::close_console`), which
//! needs no unsafe call. `break` would need `GenerateConsoleCtrlEvent`, which does, so
//! [`send_signal`] reports it, and every Unix signal, as unsupported here.

use std::io;

use thiserror::Error;

use crate::scenario::Signal;

/// What [`kill_process_group`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillOutcome {
    /// The group had members and was sent `SIGKILL`.
    Killed,
    /// No process was left in the group.
    AlreadyGone,
}

/// A signal could not be delivered.
#[derive(Debug, Error)]
pub enum SignalError {
    /// This platform cannot deliver the signal.
    #[error("the `{signal}` signal cannot be delivered on this platform: {reason}")]
    Unsupported {
        /// The requested signal.
        signal: Signal,
        /// Why it is unsupported.
        reason: &'static str,
    },
    /// The process id is not a valid id.
    #[error("process id {0} is not valid")]
    InvalidPid(u32),
    /// The system call failed.
    #[error("cannot signal process {pid}: {source}")]
    Io {
        /// The target process.
        pid: u32,
        /// The underlying error.
        source: io::Error,
    },
}

/// Sends `SIGKILL` to every process in the process group `pgid`.
///
/// # Errors
///
/// Returns an error if `pgid` is not a valid id or the call fails for a reason other than the
/// group being gone.
#[cfg(unix)]
pub fn kill_process_group(pgid: u32) -> io::Result<KillOutcome> {
    use rustix::{io::Errno, process};

    let group = unix_pid(pgid)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid process group id"))?;
    match process::kill_process_group(group, process::Signal::KILL) {
        Ok(()) => Ok(KillOutcome::Killed),
        Err(Errno::SRCH) => Ok(KillOutcome::AlreadyGone),
        Err(error) => Err(error.into()),
    }
}

/// Whether any process is left in the process group `pgid`.
///
/// A zombie that has not been reaped still counts.
#[cfg(unix)]
#[must_use]
pub fn process_group_exists(pgid: u32) -> bool {
    use rustix::process;

    unix_pid(pgid).is_some_and(|pid| process::test_kill_process_group(pid).is_ok())
}

/// Whether the child `pid` of this process has exited and is still waiting to be reaped.
///
/// It looks and does not reap: the child stays a zombie, and a zombie keeps its id, and with it
/// the id of a process group of that number, from being given to another process until it is
/// waited for. A caller that has to signal the group the child led looks first, signals while
/// this still says yes, and reaps the child after, so that the signal cannot reach a process that
/// is not one of the group's own.
///
/// # Errors
///
/// Returns an error if `pid` is not a valid id, or is not a child of this process that has not
/// been waited for yet: one that is reaped already, which [`std::process::Child::try_wait`] does
/// when it finds the child gone, answers `ECHILD`.
#[cfg(unix)]
pub fn has_exited_unreaped(pid: u32) -> io::Result<bool> {
    use rustix::{
        io::Errno,
        process::{WaitId, WaitIdOptions, waitid},
    };

    let child = unix_pid(pid)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid process id"))?;
    let options = WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT;
    loop {
        match waitid(WaitId::Pid(child), options) {
            Ok(status) => return Ok(status.is_some()),
            Err(Errno::INTR) => {}
            Err(error) => return Err(error.into()),
        }
    }
}

/// Sends `SIGKILL` to the process `pid` alone, not to its group.
///
/// # Errors
///
/// Returns an error if `pid` is not a valid id or the call fails for a reason other than the
/// process being gone.
#[cfg(unix)]
pub fn kill_process(pid: u32) -> io::Result<KillOutcome> {
    use rustix::{io::Errno, process};

    let target = unix_pid(pid)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid process id"))?;
    match process::kill_process(target, process::Signal::KILL) {
        Ok(()) => Ok(KillOutcome::Killed),
        Err(Errno::SRCH) => Ok(KillOutcome::AlreadyGone),
        Err(error) => Err(error.into()),
    }
}

/// The id of the process group that the process `pid` belongs to, or `None` if there is no such
/// process.
#[cfg(unix)]
#[must_use]
pub fn process_group_of(pid: u32) -> Option<u32> {
    let target = unix_pid(pid)?;
    let group = rustix::process::getpgid(Some(target)).ok()?;
    u32::try_from(rustix::process::Pid::as_raw(Some(group))).ok()
}

/// Delivers `signal` to the process `pid`.
///
/// # Errors
///
/// Returns [`SignalError::Unsupported`] for a signal this platform cannot deliver, and the other
/// variants when the process id is invalid or the call fails.
#[cfg(unix)]
pub fn send_signal(pid: u32, signal: Signal) -> Result<(), SignalError> {
    use rustix::process::{self, Signal as Unix};

    let number = match signal {
        Signal::Term => Unix::TERM,
        Signal::Hup => Unix::HUP,
        Signal::Quit => Unix::QUIT,
        Signal::Int => Unix::INT,
        Signal::Close | Signal::Break => {
            return Err(SignalError::Unsupported {
                signal,
                reason: "it is a Windows console event",
            });
        }
    };
    let target = unix_pid(pid).ok_or(SignalError::InvalidPid(pid))?;
    process::kill_process(target, number).map_err(|error| SignalError::Io {
        pid,
        source: error.into(),
    })
}

/// Delivers `signal` to the process `pid`.
///
/// # Errors
///
/// Always returns [`SignalError::Unsupported`] on this platform; see the module documentation.
#[cfg(not(unix))]
pub fn send_signal(_pid: u32, signal: Signal) -> Result<(), SignalError> {
    Err(SignalError::Unsupported {
        signal,
        reason: "delivering signals or console events to a pseudo-terminal child needs unsafe \
                 operating-system calls",
    })
}

#[cfg(unix)]
fn unix_pid(raw: u32) -> Option<rustix::process::Pid> {
    i32::try_from(raw)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
}

#[cfg(all(test, unix))]
mod tests {
    use std::{
        process::{Command, Stdio},
        thread,
        time::{Duration, Instant},
    };

    use super::*;

    /// Calls `condition` until it holds, for at most ten seconds.
    fn eventually(mut condition: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if condition() {
                return true;
            }
            thread::sleep(Duration::from_millis(5));
        }
        condition()
    }

    #[test]
    fn a_child_that_exited_is_seen_without_being_reaped() {
        let mut child = Command::new("sh")
            .args(["-c", "exit 3"])
            .spawn()
            .expect("a child");
        let pid = child.id();

        assert!(
            eventually(|| has_exited_unreaped(pid).expect("the child can be looked at")),
            "the child exited"
        );
        // Looking did not reap it: it can be looked at again, and waited for, with its status.
        assert!(has_exited_unreaped(pid).expect("it is still there"));
        let status = child.wait().expect("the child is waited for");
        assert_eq!(status.code(), Some(3));
        // Reaped, it is no longer a child to look at.
        assert!(has_exited_unreaped(pid).is_err());
    }

    #[test]
    fn a_child_that_is_running_has_not_exited() {
        let mut child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .spawn()
            .expect("a child");

        let seen = has_exited_unreaped(child.id());

        child.kill().expect("the child is killed");
        child.wait().expect("and reaped");
        assert!(matches!(seen, Ok(false)), "{seen:?}");
    }

    #[test]
    fn an_id_that_cannot_be_a_process_is_an_error() {
        let error = has_exited_unreaped(0).expect_err("0 names no process");

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
}
