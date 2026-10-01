//! Running one process under supervision: in a process group of its own, bounded by a deadline,
//! and measured.
//!
//! Both the scan and the `du` it is timed against run here, so that the two are timed by the same
//! mechanism: the clock starts just before the process is spawned and stops when the process is
//! seen to have exited. On Unix the exit is awaited without polling (`waitid` with `WNOWAIT`, which
//! leaves the child unreaped), so the measured time carries no polling interval, and the child's
//! final resource figures can still be read from the zombie. A watcher thread enforces the
//! deadline by killing the whole process group.
//!
//! Where the platform has no process groups (Windows), the exit is polled and a timeout ends the
//! child alone. See [`crate::safety::process`].

use std::{
    io::{self, Read},
    process::{Child, Command, Stdio},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use thiserror::Error;

use crate::metrics::{CpuTimes, reaped_children_cpu};

/// How much of a stream a run keeps. The rest is counted and dropped.
const KEPT_OUTPUT: usize = 16 * 1024;

/// A process could not be run.
#[derive(Debug, Error)]
pub enum ProcessError {
    /// The process could not be started.
    #[error("cannot start `{program}`: {source}")]
    Spawn {
        /// The program.
        program: String,
        /// The underlying error.
        source: io::Error,
    },
    /// The process could not be waited for.
    #[error("cannot wait for `{program}`: {source}")]
    Wait {
        /// The program.
        program: String,
        /// The underlying error.
        source: io::Error,
    },
}

/// How a process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// It exited with this code.
    Exited(i32),
    /// A signal ended it.
    Signaled(i32),
}

impl Ended {
    /// The exit code, or `None` when a signal ended the process.
    #[must_use]
    pub const fn code(self) -> Option<i32> {
        match self {
            Self::Exited(code) => Some(code),
            Self::Signaled(_) => None,
        }
    }

    /// A short description: `exited with code 0` or `was killed by signal 9`.
    #[must_use]
    pub fn describe(self) -> String {
        match self {
            Self::Exited(code) => format!("exited with code {code}"),
            Self::Signaled(signal) => format!("was killed by signal {signal}"),
        }
    }
}

/// The start of a stream a process wrote, and how much it wrote in all.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Captured {
    /// Every byte written.
    pub bytes: u64,
    /// The first bytes written.
    pub head: Vec<u8>,
}

impl Captured {
    /// The first line of the head, lossily decoded and trimmed.
    #[must_use]
    pub fn first_line(&self) -> String {
        String::from_utf8_lossy(&self.head)
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .to_owned()
    }
}

/// What a supervised run found out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finished {
    /// How the process ended.
    pub ended: Ended,
    /// Whether the deadline passed and the process was killed.
    pub timed_out: bool,
    /// From just before the spawn to the moment the exit was seen.
    pub wall: Duration,
    /// The CPU time the process used, where the platform can say. Exact while this process reaps
    /// one child at a time.
    pub cpu: Option<CpuTimes>,
    /// The peak memory in bytes: the peak physical footprint on macOS, the peak resident set size
    /// where `/proc` can say (sampled every few milliseconds, so a peak that falls between two
    /// samples is missed), and nothing elsewhere. Unaffected by `cgroup`: `systemd-run --scope`
    /// execs into the wrapped program, so this crate only ever samples one pid either way.
    pub peak_memory_bytes: Option<u64>,
    /// The Linux cgroup v2 `memory.peak` of the scope the process ran in, when `cgroup` asked for
    /// the wrap and the counter could be read (`safety::cgroup::read_memory_peak`). `None` off
    /// Linux, without the wrap, or when the counter could not be read.
    pub cgroup_memory_peak_bytes: Option<u64>,
    /// What the process wrote to standard output.
    pub stdout: Captured,
    /// What the process wrote to standard error.
    pub stderr: Captured,
}

/// Runs `command` to its end, or kills it when `timeout` has passed.
///
/// The caller sets the program, arguments, environment, and working directory. This sets the
/// standard streams (none in, both out captured) and, on Unix, the process group. The whole group
/// is killed when the deadline passes and once more after the process has exited, so nothing it
/// started outlives the run.
///
/// `cgroup` says whether `command` was already rewritten to run under the Linux cgroup memory cap
/// (`safety::cgroup::wrap`): when true, this function also reads the cgroup's `memory.peak` once
/// the process has exited, before it is reaped.
///
/// # Errors
///
/// Returns [`ProcessError`] when the process cannot be started or waited for.
pub fn run(
    command: &mut Command,
    timeout: Duration,
    measure_memory: bool,
    cgroup: bool,
) -> Result<Finished, ProcessError> {
    let program = command.get_program().to_string_lossy().into_owned();
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;

        command.process_group(0);
    }

    let cpu_before = reaped_children_cpu();
    let started = Instant::now();
    let mut child = command.spawn().map_err(|source| ProcessError::Spawn {
        program: program.clone(),
        source,
    })?;
    let stdout = capture_in_background(child.stdout.take());
    let stderr = capture_in_background(child.stderr.take());

    let waited = supervise(&mut child, started + timeout, measure_memory, cgroup);
    let (ended, ended_at, timed_out, sampler, cgroup_memory_peak_bytes) = match waited {
        Ok(waited) => waited,
        Err(source) => {
            kill(&mut child);
            let _ = child.wait();
            return Err(ProcessError::Wait { program, source });
        }
    };
    // The child is a zombie until it is reaped, so its group id cannot have been reused: this
    // reaches everything it left running.
    kill(&mut child);
    let _ = child.wait();
    let cpu = match (cpu_before, reaped_children_cpu()) {
        (Some(before), Some(after)) => Some(after.since(before)),
        _ => None,
    };
    Ok(Finished {
        ended,
        timed_out,
        wall: ended_at.saturating_duration_since(started),
        cpu,
        peak_memory_bytes: sampler.peak_memory_bytes(),
        cgroup_memory_peak_bytes,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
}

/// Kills the child's process group (on Unix) or the child itself.
fn kill(child: &mut Child) {
    #[cfg(unix)]
    {
        let _ = crate::safety::kill_process_group(child.id());
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }
}

fn capture_in_background(stream: Option<impl Read + Send + 'static>) -> JoinHandle<Captured> {
    thread::spawn(move || {
        let mut captured = Captured::default();
        let Some(mut stream) = stream else {
            return captured;
        };
        let mut buffer = [0_u8; 8 * 1024];
        loop {
            match stream.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    captured.bytes += read as u64;
                    let room = KEPT_OUTPUT.saturating_sub(captured.head.len());
                    captured.head.extend_from_slice(&buffer[..read.min(room)]);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        captured
    })
}

// ---------------------------------------------------------------------------------------------
// Unix: block in `waitid`, and let a watcher enforce the deadline.

#[cfg(unix)]
use unix::supervise;

#[cfg(unix)]
mod unix {
    use std::{
        io,
        process::Child,
        sync::{Condvar, Mutex, PoisonError},
        thread,
        time::{Duration, Instant},
    };

    use crate::{
        metrics::ProcessSampler,
        safety::{cgroup, kill_process_group},
    };

    use super::Ended;

    /// How often the resident set size is sampled where the platform keeps no lifetime peak that
    /// can be read after the exit.
    const SAMPLE_EVERY: Duration = Duration::from_millis(2);
    /// How often a process that outlived its deadline is killed again.
    const KILL_AGAIN_EVERY: Duration = Duration::from_millis(20);

    struct Gate {
        done: Mutex<bool>,
        wake: Condvar,
    }

    /// Waits for `child` to exit. Returns how it ended, when the exit was seen, whether the
    /// deadline killed it, the memory samples, and the cgroup's `memory.peak` when `cgroup` wrapped
    /// the spawn and the counter could be read.
    pub(super) fn supervise(
        child: &mut Child,
        deadline: Instant,
        measure_memory: bool,
        cgroup: bool,
    ) -> io::Result<(Ended, Instant, bool, ProcessSampler, Option<u64>)> {
        let pid = child.id();
        let gate = Gate {
            done: Mutex::new(false),
            wake: Condvar::new(),
        };
        // The platforms that keep a lifetime peak (macOS) read it once, from the zombie.
        let sample_during = measure_memory && cfg!(not(target_os = "macos"));
        thread::scope(|scope| {
            let watcher = scope.spawn(|| watch(pid, deadline, &gate, sample_during));
            let waited = wait_exited(pid);
            let ended_at = Instant::now();
            *gate.done.lock().unwrap_or_else(PoisonError::into_inner) = true;
            gate.wake.notify_all();
            let (mut sampler, timed_out) = watcher.join().unwrap_or_default();
            let ended = waited?;
            if measure_memory {
                // The final figures: exact on macOS, and a last chance elsewhere.
                sampler.sample(pid);
            }
            // `pid` is a member of the scope's cgroup throughout: `systemd-run --scope` execs
            // into the wrapped program, which keeps the pid this crate spawned (see
            // `safety::cgroup`). It is still a zombie here, not yet reaped by the caller, so the
            // scope is not yet garbage collected.
            let cgroup_memory_peak_bytes = cgroup.then(|| cgroup::read_memory_peak(pid)).flatten();
            Ok((
                ended,
                ended_at,
                timed_out,
                sampler,
                cgroup_memory_peak_bytes,
            ))
        })
    }

    /// Waits until the process has exited, without reaping it.
    fn wait_exited(pid: u32) -> io::Result<Ended> {
        use rustix::{
            io::Errno,
            process::{Pid, WaitId, WaitIdOptions, waitid},
        };

        let pid = i32::try_from(pid)
            .ok()
            .and_then(Pid::from_raw)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid process id"))?;
        loop {
            match waitid(
                WaitId::Pid(pid),
                WaitIdOptions::EXITED | WaitIdOptions::NOWAIT,
            ) {
                Ok(Some(status)) => {
                    return match (status.exit_status(), status.terminating_signal()) {
                        (Some(code), _) => Ok(Ended::Exited(code)),
                        (None, Some(signal)) => Ok(Ended::Signaled(signal)),
                        (None, None) => {
                            Err(io::Error::other("the process ended in an unknown way"))
                        }
                    };
                }
                Ok(None) | Err(Errno::INTR) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }

    /// Kills the process group once the deadline has passed, and samples `pid`'s memory while it
    /// waits.
    fn watch(
        pid: u32,
        deadline: Instant,
        gate: &Gate,
        sample_during: bool,
    ) -> (ProcessSampler, bool) {
        let mut sampler = ProcessSampler::default();
        let mut timed_out = false;
        loop {
            let now = Instant::now();
            if now >= deadline {
                timed_out = true;
                let _ = kill_process_group(pid);
            } else if sample_during {
                sampler.sample(pid);
            }
            let wait = if timed_out {
                KILL_AGAIN_EVERY
            } else if sample_during {
                SAMPLE_EVERY.min(deadline.saturating_duration_since(now))
            } else {
                deadline.saturating_duration_since(now)
            };
            let guard = gate.done.lock().unwrap_or_else(PoisonError::into_inner);
            let (guard, _) = gate
                .wake
                .wait_timeout_while(guard, wait, |done| !*done)
                .unwrap_or_else(PoisonError::into_inner);
            if *guard {
                return (sampler, timed_out);
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Elsewhere: poll, and end the child alone.

#[cfg(not(unix))]
fn supervise(
    child: &mut Child,
    deadline: Instant,
    measure_memory: bool,
    cgroup: bool,
) -> io::Result<(
    Ended,
    Instant,
    bool,
    crate::metrics::ProcessSampler,
    Option<u64>,
)> {
    let _ = (measure_memory, cgroup);
    let mut timed_out = false;
    loop {
        if let Some(status) = child.try_wait()? {
            let ended_at = Instant::now();
            return Ok((
                Ended::Exited(status.code().unwrap_or(-1)),
                ended_at,
                timed_out,
                crate::metrics::ProcessSampler::default(),
                None,
            ));
        }
        if !timed_out && Instant::now() >= deadline {
            timed_out = true;
            let _ = child.kill();
        }
        thread::sleep(Duration::from_millis(1));
    }
}
