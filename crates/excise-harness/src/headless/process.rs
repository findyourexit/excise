//! Running one process under supervision: in a process group of its own, bounded by a deadline,
//! and measured.
//!
//! Both the scan and the `du` it is timed against run here, so that the two are timed by the same
//! mechanism: the clock starts just before the process is spawned and stops when the process is
//! seen to have exited. On Unix the exit is awaited without polling (`waitid` with `WNOWAIT`, which
//! leaves the child unreaped, on a thread of its own), so the measured time carries no polling
//! interval, and the child's final resource figures can still be read from the zombie. The
//! calling thread enforces the deadline by killing the whole process group, and ends the group at
//! once when a caller's cancellation flag is set ([`run_cancellable`]: the read-only soak, which a
//! person can interrupt).
//!
//! Where the platform has no process groups (Windows), the exit is polled and a timeout ends the
//! child alone. See [`crate::safety::process`].
//!
//! No wait for a process that has been killed is unbounded. A process that SIGKILL does not end is
//! stuck where a signal does not reach it (a hung network mount, a device that does not answer),
//! so once the process has been killed and [`KILL_GRACE`] has passed the supervisor gives up: it
//! reports that the process could not be ended, leaves it unreaped, and never signals it again.
//! What the thread that waits for the exit has published is taken first, though, whatever the
//! clock says: a process that ended within the grace is not stuck, however late the supervisor is
//! scheduled again. The deadline and an interrupt end the run, and not only the process.

use std::{
    io::{self, Read},
    process::{Child, Command, Stdio},
    sync::atomic::AtomicBool,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use thiserror::Error;

use crate::metrics::{CpuTimes, reaped_children_cpu};

/// How much of a stream a run keeps. The rest is counted and dropped.
const KEPT_OUTPUT: usize = 16 * 1024;

/// How long a process that has been killed is given to be gone. After this nothing waits for it
/// any longer, and nothing signals it again.
const KILL_GRACE: Duration = Duration::from_secs(5);

/// Why a supervisor could not say how a process ended.
#[derive(Debug)]
enum Unsupervised {
    /// The wait itself failed, or the thread for it could not be made. The process may still be
    /// running.
    Wait(io::Error),
    /// The process was killed and was still there after [`KILL_GRACE`]. It is left running and
    /// unreaped, and is not signalled again.
    Stuck(io::Error),
}

/// The error for a process that the kill did not end.
fn stuck(grace: Duration) -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        format!(
            "the process could not be ended: it was still there {} s after it was killed, so it \
             is left running and unreaped",
            grace.as_secs_f64()
        ),
    )
}

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
    run_supervised(command, timeout, measure_memory, cgroup, None)
}

/// Like [`run`], and kills the process's whole group as soon as `cancel` is set, whatever time is
/// left. Nothing but the caller sets the flag, and it is looked at about every 25 ms.
///
/// The result does not say that the run was cancelled: the flag stays set, and the caller that set
/// it knows. `timed_out` is only for a deadline that passed.
///
/// # Errors
///
/// Returns [`ProcessError`] when the process cannot be started or waited for.
pub fn run_cancellable(
    command: &mut Command,
    timeout: Duration,
    measure_memory: bool,
    cgroup: bool,
    cancel: &AtomicBool,
) -> Result<Finished, ProcessError> {
    run_supervised(command, timeout, measure_memory, cgroup, Some(cancel))
}

fn run_supervised(
    command: &mut Command,
    timeout: Duration,
    measure_memory: bool,
    cgroup: bool,
    cancel: Option<&AtomicBool>,
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

    let waited = supervise(
        &mut child,
        started + timeout,
        measure_memory,
        cgroup,
        cancel,
    );
    let (ended, ended_at, timed_out, sampler, cgroup_memory_peak_bytes) = match waited {
        Ok(waited) => waited,
        // Killed, and still there after the grace: left alone. Another signal would only name a
        // group that may no longer be the process's, and a wait would never end.
        Err(Unsupervised::Stuck(source)) => return Err(ProcessError::Wait { program, source }),
        Err(Unsupervised::Wait(source)) => {
            kill(&mut child);
            let source = if reap_within(&mut child, KILL_GRACE) {
                source
            } else {
                io::Error::other(format!("{source}; and {}", stuck(KILL_GRACE)))
            };
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

/// Reaps `child`, which has been killed, polling for at most `grace`, and says whether it did. A
/// child that is still there is left as it is: nothing waits for it, and nothing signals it again.
fn reap_within(child: &mut Child, grace: Duration) -> bool {
    let until = Instant::now() + grace;
    loop {
        match child.try_wait() {
            Ok(None) => {}
            // Reaped, or there is nothing left that is this process's to wait for.
            Ok(Some(_)) | Err(_) => return true,
        }
        if Instant::now() >= until {
            return false;
        }
        thread::sleep(Duration::from_millis(5));
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
// Unix: a thread blocks in `waitid`, and the calling thread enforces the deadline.

#[cfg(unix)]
use unix::supervise;

#[cfg(unix)]
mod unix {
    use std::{
        io,
        process::Child,
        sync::{
            Arc, Condvar, Mutex, PoisonError,
            atomic::{AtomicBool, Ordering},
        },
        thread,
        time::{Duration, Instant},
    };

    use crate::{
        metrics::ProcessSampler,
        safety::{cgroup, kill_process_group},
    };

    use super::{Ended, KILL_GRACE, Unsupervised, stuck};

    /// How often the resident set size is sampled where the platform keeps no lifetime peak that
    /// can be read after the exit.
    const SAMPLE_EVERY: Duration = Duration::from_millis(2);
    /// How often a process that outlived its deadline is killed again, until the grace is over.
    const KILL_AGAIN_EVERY: Duration = Duration::from_millis(20);
    /// How often a caller's cancellation flag is looked at.
    const CANCEL_EVERY: Duration = Duration::from_millis(25);

    /// How a process is ended, and how long it is given to be gone once it has been. The kill is
    /// a field so that a test can stand in for what a real one cannot be made to do: leave a
    /// process that cannot be ended.
    pub(super) struct Ending<'a> {
        /// Ends the process group that `pid` leads. Called again every [`KILL_AGAIN_EVERY`] until
        /// the grace is over, and never after.
        pub(super) kill: &'a dyn Fn(u32),
        /// How long the process is given to be gone after the first kill.
        pub(super) grace: Duration,
        /// Called at the start of every pass of the supervisor's loop, before it reads the clock.
        /// It does nothing in a run. A test holds the supervisor here while the process ends and
        /// its waiter says so, which is what a supervisor that is not scheduled for a while
        /// looks like.
        pub(super) between_looks: &'a dyn Fn(),
    }

    /// What the thread that blocks in `waitid` hands over: how the process ended, and the moment
    /// that was seen.
    struct Exit {
        result: Mutex<Option<io::Result<(Ended, Instant)>>>,
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
        cancel: Option<&AtomicBool>,
    ) -> Result<(Ended, Instant, bool, ProcessSampler, Option<u64>), Unsupervised> {
        let ending = Ending {
            kill: &|pid| {
                let _ = kill_process_group(pid);
            },
            grace: KILL_GRACE,
            between_looks: &|| {},
        };
        supervise_ending(
            child.id(),
            deadline,
            measure_memory,
            cgroup,
            cancel,
            &ending,
        )
    }

    /// [`supervise`] with the way the process is ended given.
    ///
    /// The blocking wait is made by a thread of its own, which stamps the moment the exit is seen,
    /// so that the measured time carries no polling interval and so that this thread, which
    /// enforces the deadline, can stop waiting for a process that cannot be ended. That thread is
    /// left behind then, blocked until the process goes (a `WNOWAIT` wait reaps nothing).
    pub(super) fn supervise_ending(
        pid: u32,
        deadline: Instant,
        measure_memory: bool,
        cgroup: bool,
        cancel: Option<&AtomicBool>,
        ending: &Ending<'_>,
    ) -> Result<(Ended, Instant, bool, ProcessSampler, Option<u64>), Unsupervised> {
        let exit = Arc::new(Exit {
            result: Mutex::new(None),
            wake: Condvar::new(),
        });
        let exit_thread = {
            let exit = Arc::clone(&exit);
            thread::Builder::new()
                .name("process-exit".to_owned())
                .spawn(move || {
                    let waited = wait_exited(pid).map(|ended| (ended, Instant::now()));
                    *exit.result.lock().unwrap_or_else(PoisonError::into_inner) = Some(waited);
                    exit.wake.notify_all();
                })
                .map_err(Unsupervised::Wait)?
        };
        // The platforms that keep a lifetime peak (macOS) read it once, from the zombie.
        let sample_during = measure_memory && cfg!(not(target_os = "macos"));
        let mut sampler = ProcessSampler::default();
        let mut timed_out = false;
        let mut cancelled = false;
        let mut killed_at: Option<Instant> = None;
        let seen = loop {
            (ending.between_looks)();
            let now = Instant::now();
            cancelled = cancelled || cancel.is_some_and(|flag| flag.load(Ordering::SeqCst));
            timed_out = timed_out || now >= deadline;
            // What the waiter has published is taken first, whatever the clock says: a process that
            // ended within the grace is not stuck, however late this thread is scheduled again. The
            // verdict that it is stuck is given under the same lock, so that nothing is published
            // between the look and the verdict.
            let mut published = exit.result.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(result) = published.take() {
                break result;
            }
            if timed_out || cancelled {
                let first = *killed_at.get_or_insert(now);
                if now.saturating_duration_since(first) >= ending.grace {
                    // Killed over and over, and still there: stuck where a signal does not reach.
                    // The waiting thread is left behind, and nothing signals the process again.
                    return Err(Unsupervised::Stuck(stuck(ending.grace)));
                }
            }
            drop(published);
            if timed_out || cancelled {
                (ending.kill)(pid);
            } else if sample_during {
                sampler.sample(pid);
            }
            let wait = if timed_out || cancelled {
                KILL_AGAIN_EVERY
            } else if sample_during {
                SAMPLE_EVERY.min(deadline.saturating_duration_since(now))
            } else if cancel.is_some() {
                CANCEL_EVERY.min(deadline.saturating_duration_since(now))
            } else {
                deadline.saturating_duration_since(now)
            };
            let guard = exit.result.lock().unwrap_or_else(PoisonError::into_inner);
            let (mut guard, _) = exit
                .wake
                .wait_timeout_while(guard, wait, |result| result.is_none())
                .unwrap_or_else(PoisonError::into_inner);
            if let Some(result) = guard.take() {
                break result;
            }
        };
        // The thread has handed its result over, so it is finished or about to be.
        let _ = exit_thread.join();
        let (ended, ended_at) = seen.map_err(Unsupervised::Wait)?;
        if measure_memory {
            // The final figures: exact on macOS, and a last chance elsewhere.
            sampler.sample(pid);
        }
        // `pid` is a member of the scope's cgroup throughout: `systemd-run --scope` execs into the
        // wrapped program, which keeps the pid this crate spawned (see `safety::cgroup`). It is
        // still a zombie here, not yet reaped by the caller, so the scope is not yet garbage
        // collected.
        let cgroup_memory_peak_bytes = cgroup.then(|| cgroup::read_memory_peak(pid)).flatten();
        Ok((
            ended,
            ended_at,
            timed_out,
            sampler,
            cgroup_memory_peak_bytes,
        ))
    }

    /// Waits until the process has exited, without reaping it. Blocks for as long as the process
    /// lives: the thread that calls it is the one that may be left behind.
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
}

// ---------------------------------------------------------------------------------------------
// Elsewhere: poll, and end the child alone.

#[cfg(not(unix))]
fn supervise(
    child: &mut Child,
    deadline: Instant,
    measure_memory: bool,
    cgroup: bool,
    cancel: Option<&AtomicBool>,
) -> Result<
    (
        Ended,
        Instant,
        bool,
        crate::metrics::ProcessSampler,
        Option<u64>,
    ),
    Unsupervised,
> {
    use std::sync::atomic::Ordering;

    let _ = (measure_memory, cgroup);
    let mut timed_out = false;
    let mut cancelled = false;
    let mut killed_at: Option<Instant> = None;
    loop {
        if let Some(status) = child.try_wait().map_err(Unsupervised::Wait)? {
            let ended_at = Instant::now();
            return Ok((
                Ended::Exited(status.code().unwrap_or(-1)),
                ended_at,
                timed_out,
                crate::metrics::ProcessSampler::default(),
                None,
            ));
        }
        let now = Instant::now();
        if !timed_out && now >= deadline {
            timed_out = true;
            killed_at.get_or_insert(now);
            let _ = child.kill();
        }
        if !cancelled && cancel.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
            cancelled = true;
            killed_at.get_or_insert(now);
            let _ = child.kill();
        }
        if killed_at.is_some_and(|at| now.saturating_duration_since(at) >= KILL_GRACE) {
            return Err(Unsupervised::Stuck(stuck(KILL_GRACE)));
        }
        thread::sleep(Duration::from_millis(1));
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::{
        os::unix::process::CommandExt as _,
        process::Child,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
            mpsc,
        },
    };

    use super::{unix::Ending, unix::supervise_ending, *};

    /// A child in a process group of its own that sleeps for a long time.
    fn sleeper() -> Child {
        let mut command = Command::new("/bin/sleep");
        command
            .arg("60")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        command.spawn().expect("a sleeper")
    }

    #[test]
    fn a_process_that_the_kill_does_not_end_is_given_up_on_after_the_grace_and_left_alone() {
        // What a process stuck in uninterruptible I/O looks like to the supervisor: the kill goes
        // out and nothing happens. The kill here is counted and does nothing.
        let mut child = sleeper();
        let pid = child.id();
        let kills = Arc::new(AtomicUsize::new(0));
        let (done, result) = mpsc::channel();
        thread::spawn({
            let kills = Arc::clone(&kills);
            move || {
                let ending = Ending {
                    kill: &|_| {
                        kills.fetch_add(1, Ordering::SeqCst);
                    },
                    grace: Duration::from_millis(300),
                    between_looks: &|| {},
                };
                let started = Instant::now();
                let supervised = supervise_ending(
                    pid,
                    started + Duration::from_millis(100),
                    false,
                    false,
                    None,
                    &ending,
                );
                let _ = done.send((supervised, started.elapsed()));
            }
        });

        // A supervisor that waits for ever would hang the test, so the wait for it is bounded, and
        // then the sleeper is ended here, which is what lets the thread that waits for it go.
        let given_up = result.recv_timeout(Duration::from_secs(10));
        let Ok((supervised, elapsed)) = given_up else {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the supervisor did not give up on a process that the kill does not end");
        };

        let Err(Unsupervised::Stuck(error)) = supervised else {
            let _ = child.kill();
            let _ = child.wait();
            panic!("a process that is still there was reported as {supervised:?}");
        };
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(error.to_string().contains("could not be ended"), "{error}");
        assert!(
            elapsed >= Duration::from_millis(350) && elapsed < Duration::from_secs(5),
            "it gave up after the deadline and the grace, and not before or long after: {elapsed:?}"
        );
        // The kill was repeated while the grace ran, and never after it.
        let at_the_end = kills.load(Ordering::SeqCst);
        assert!(at_the_end > 1, "{at_the_end} kills");
        thread::sleep(Duration::from_millis(200));
        assert_eq!(kills.load(Ordering::SeqCst), at_the_end, "signalled again");
        // Left running and unreaped.
        assert!(
            child.try_wait().expect("a status").is_none(),
            "the process is still there"
        );
        child.kill().expect("the test ends its own sleeper");
        child.wait().expect("and reaps it");
    }

    #[test]
    fn a_process_that_ended_within_the_grace_is_not_reported_stuck_by_a_supervisor_that_looks_late()
    {
        // The deadline has passed at the start, so the kill goes out at once and the grace begins.
        // The kill of this test does nothing. The test ends the process itself, within the grace,
        // while the supervisor is held where it is: its waiter says so, and the supervisor is not
        // scheduled again until the grace is over, as can happen to any thread on a loaded machine.
        let mut child = sleeper();
        let pid = child.id();
        let passes = AtomicUsize::new(0);
        let ending = Ending {
            kill: &|_| {},
            grace: Duration::from_millis(200),
            between_looks: &|| {
                if passes.fetch_add(1, Ordering::SeqCst) == 1 {
                    let _ = crate::safety::kill_process_group(pid);
                    thread::sleep(Duration::from_millis(500));
                }
            },
        };

        let supervised = supervise_ending(pid, Instant::now(), false, false, None, &ending);

        let _ = child.kill();
        let _ = child.wait();
        let Ok((ended, _, timed_out, _, _)) = supervised else {
            panic!("a process that ended within the grace was reported as {supervised:?}");
        };
        assert_eq!(ended, Ended::Signaled(9));
        assert!(timed_out, "the deadline had passed");
    }

    #[test]
    fn a_process_that_the_kill_ends_is_seen_to_end_at_once_and_not_after_the_grace() {
        let mut child = sleeper();
        let started = Instant::now();

        let supervised = unix::supervise(
            &mut child,
            started + Duration::from_millis(100),
            false,
            false,
            None,
        )
        .expect("a process that the kill ends");

        let (ended, _, timed_out, _, _) = supervised;
        assert_eq!(ended, Ended::Signaled(9));
        assert!(timed_out, "the deadline did it");
        assert!(
            started.elapsed() < KILL_GRACE,
            "it was not waited for until the grace was over: {:?}",
            started.elapsed()
        );
        child.wait().expect("a zombie is reaped");
    }

    #[test]
    fn a_cancellation_ends_the_process_and_is_not_a_timeout() {
        let mut child = sleeper();
        let cancel = AtomicBool::new(true);

        let (ended, _, timed_out, _, _) = unix::supervise(
            &mut child,
            Instant::now() + Duration::from_mins(10),
            false,
            false,
            Some(&cancel),
        )
        .expect("a process that the kill ends");

        assert_eq!(ended, Ended::Signaled(9));
        assert!(!timed_out, "no deadline passed");
        child.wait().expect("a zombie is reaped");
    }

    #[test]
    fn a_process_that_ends_by_itself_is_seen_to_end_without_waiting_for_the_deadline() {
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "exit 3"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        let mut child = command.spawn().expect("a shell");
        let started = Instant::now();

        let (ended, ended_at, timed_out, _, _) = unix::supervise(
            &mut child,
            started + Duration::from_mins(10),
            false,
            false,
            None,
        )
        .expect("a process that ends");

        assert_eq!(ended, Ended::Exited(3));
        assert!(!timed_out);
        assert!(ended_at.duration_since(started) < Duration::from_secs(5));
        child.wait().expect("a zombie is reaped");
    }

    #[test]
    fn a_wait_that_cannot_reap_is_given_a_grace_and_no_more() {
        // `reap_within` for a child that is still there: it says so after the grace, and the
        // child is left alone.
        let mut child = sleeper();
        let started = Instant::now();

        let reaped = reap_within(&mut child, Duration::from_millis(150));

        assert!(!reaped);
        assert!(started.elapsed() >= Duration::from_millis(150));
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(child.try_wait().expect("a status").is_none());
        child.kill().expect("ended by the test");
        assert!(reap_within(&mut child, Duration::from_secs(5)), "reaped");
    }
}
