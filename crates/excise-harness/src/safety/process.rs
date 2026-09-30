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
//! `src/os/windows.rs`. Delivering a console close or break event to a `ConPTY` child needs unsafe
//! console calls as well, so [`send_signal`] reports every signal as unsupported there.

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
