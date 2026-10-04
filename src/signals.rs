//! Delivers an external signal (Unix) or console control event (Windows) into the owner loop
//! and the headless scanner as one confirmed-quit request, instead of the process
//! being killed outright with none of its cleanup run. See `docs/architecture/threat-model.md`'s
//! "Terminal restoration" control and `docs/architecture/overview.md`'s Main Loop section.
//!
//! Nothing in the in-process test or scenario runners can deliver a real signal or console
//! event; [`install`] is called only by `cli::run_main`, once per process.

use std::convert::Infallible;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

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
    /// out anything the first started. What stays is the executor's own entry-boundary check, the
    /// shutdown's wait for the deletion executor, which nothing bounds, and the threads that are
    /// stopping, which the shutdown waits for only as long as the grace (`FORCED_STOP_GRACE`)
    /// after it sees this request.
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

/// How long a shutdown that a [`StopRequest::Forced`] ended still waits for the threads whose
/// waits it bounds (all but the deletion executor) that have not ended, counted from when the
/// shutdown sees that request. A thread stopping normally ends within a block of I/O or a
/// directory entry, so the grace costs a healthy exit nothing, and it lets the session's storage
/// be removed after an impatient pair of signals. A thread that never ends, because it is inside
/// a call that a hung file system never answers, cannot be stopped from outside: past the grace
/// the shutdown leaves it to the exit of the process.
pub(crate) const FORCED_STOP_GRACE: Duration = Duration::from_secs(1);

/// What a thread's wait ended with.
pub(crate) enum Joined<T> {
    /// The thread ended: what it returned, or the panic that ended it.
    Ended(thread::Result<T>),
    /// A forced stop gave up on the thread, which goes on until the process exits.
    Abandoned,
}

/// A thread that a [`ShutdownWait`] can wait for without ever asking whether it ended: the thread
/// tells the wait itself, the moment it does.
///
/// [`spawn_waitable`] is the only way to make one. It gives the thread a guard that owns the
/// sending end of a channel nothing is ever sent on (its message type is [`Infallible`]), and the
/// thread's closure drops that guard last, after everything the thread's body owned, whether it
/// returns or unwinds. The receiving end disconnects at that moment, which is what the wait
/// selects on, beside the stop requests and the grace. A thread that panics is therefore seen to
/// end like any other, and reported as the panic it ended with.
pub(crate) struct WaitableThread<T> {
    handle: JoinHandle<T>,
    /// Disconnects once the thread's closure is done, however it ended.
    ended: Receiver<Infallible>,
}

/// Starts a thread called `name` that runs `body`, and tells whoever waits for it through a
/// [`ShutdownWait`] when it ends.
///
/// # Errors
///
/// Returns an error when the operating system cannot start the thread.
pub(crate) fn spawn_waitable<T, F>(name: &str, body: F) -> io::Result<WaitableThread<T>>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let (end, ended) = crossbeam_channel::bounded::<Infallible>(0);
    let handle = thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            // Dropped after `body` and everything it owns, on a return and on an unwind alike.
            let _end = end;
            body()
        })?;
    Ok(WaitableThread { handle, ended })
}

impl<T> WaitableThread<T> {
    /// Waits for the thread to end however long it takes, with no stop request or grace to end
    /// the wait early.
    pub(crate) fn join(self) -> thread::Result<T> {
        self.handle.join()
    }

    /// Whether the thread ended within `bound`: a test tells a thread that ends from one that
    /// never does without waiting for it for ever.
    #[cfg(test)]
    pub(crate) fn ended_within(&self, bound: Duration) -> bool {
        matches!(
            self.ended.recv_timeout(bound),
            Err(crossbeam_channel::RecvTimeoutError::Disconnected)
        )
    }
}

/// How a shutdown waits for the threads it stops, and when it stops waiting.
///
/// The owner loop leaves its own loop before it stops the workers and the store thread, and from
/// then on it is the one waiting, so a second stop request has nobody else to be acted on by.
/// The waits it bounds ([`Self::join`]) therefore also watch for it: a thread stuck in a call that
/// no flag can interrupt would otherwise leave the terminal unrestored for good. Terminal
/// restoration stays with the owner, after the waits.
///
/// One wait is never bounded ([`Self::join_patiently`]): the deletion executor's. A deletion
/// entry is moved aside before it is removed, so a process that ended inside one could leave a
/// target that is neither intact nor gone, and a second signal is not worth that. A deletion stuck
/// in a call that never returns therefore still holds a forced exit, as it always has.
///
/// A wait never asks whether its thread ended, and never sleeps: it blocks on the thread's own end
/// signal ([`WaitableThread`]), on the stop requests, and on the grace's deadline together. It
/// returns the moment the thread ends, reacts to a forced stop the moment one arrives, and gives
/// up only when the grace runs out.
pub(crate) struct ShutdownWait {
    stops: Option<Receiver<StopRequest>>,
    /// When the wait gives up on the threads still running: set once a forced stop was seen.
    give_up_at: Option<Instant>,
}

/// Why a wait for a thread's end stopped waiting.
#[derive(Debug, Eq, PartialEq)]
enum Wake {
    /// The thread ended.
    Ended,
    /// The grace of a forced stop ran out first.
    GaveUp,
}

impl ShutdownWait {
    /// A wait that nothing ends early: for a run no stop request reaches, and for dropping a
    /// handle.
    pub(crate) const fn patient() -> Self {
        Self {
            stops: None,
            give_up_at: None,
        }
    }

    /// A wait that a forced stop ends. `forced` says the run already took one, so no more is left
    /// in `stops` to arrive.
    pub(crate) fn until_forced(stops: Option<Receiver<StopRequest>>, forced: bool) -> Self {
        Self {
            stops,
            give_up_at: forced.then(|| Instant::now() + FORCED_STOP_GRACE),
        }
    }

    /// Waits for `thread` to end, unless a forced stop ends the wait first.
    pub(crate) fn join<T>(&mut self, thread: WaitableThread<T>) -> Joined<T> {
        match self.wait_for_end(&thread.ended) {
            // The thread's closure is done, so this returns as soon as the thread itself has gone.
            Wake::Ended => Joined::Ended(thread.handle.join()),
            Wake::GaveUp => Joined::Abandoned,
        }
    }

    /// Waits for `thread` to end however long it takes, whatever the stop requests: for the one
    /// thread that a process ending in the middle of its work could leave the file system in a
    /// state nobody chose. The grace of a forced stop does not run meanwhile, so the waits that
    /// follow still get it in full, and a request that arrives during this wait is seen by them.
    pub(crate) fn join_patiently<T>(&mut self, thread: JoinHandle<T>) -> thread::Result<T> {
        let started = Instant::now();
        let result = thread.join();
        if let Some(give_up_at) = &mut self.give_up_at {
            *give_up_at += started.elapsed();
        }
        result
    }

    /// Blocks until `ended` disconnects or the grace of a forced stop runs out, and takes the stop
    /// requests that arrive meanwhile: the first forced one starts the grace. A thread that has
    /// ended wins over a grace that ran out in the same instant, as it always did.
    fn wait_for_end(&mut self, ended: &Receiver<Infallible>) -> Wake {
        let no_stops = crossbeam_channel::never();
        loop {
            let stops = self.stops.as_ref().unwrap_or(&no_stops);
            let deadline = self
                .give_up_at
                .map_or_else(crossbeam_channel::never, crossbeam_channel::at);
            let request = crossbeam_channel::select_biased! {
                recv(ended) -> _ => return Wake::Ended,
                recv(deadline) -> _ => return Wake::GaveUp,
                recv(stops) -> request => request,
            };
            match request {
                Ok(StopRequest::Forced) => {
                    self.give_up_at
                        .get_or_insert_with(|| Instant::now() + FORCED_STOP_GRACE);
                }
                // A first request is the quit this shutdown already is.
                Ok(StopRequest::Graceful) => {}
                // Nothing can arrive any more: wait as a run does that no request reaches.
                Err(_) => self.stops = None,
            }
        }
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
/// caller to discard (see `docs/reports.md`). The session still holds its lock when this runs.
/// That does not stop the removal on Unix. On Windows it can make it fail, which is
/// acceptable: the directory is then a dead session's, and the next start's sweep
/// (`scan_store::sweep`) removes it. No wait is added here for that.
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
    use std::convert::Infallible;
    use std::path::PathBuf;
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};

    use crossbeam_channel::Sender;

    use super::{
        FORCED_STOP_GRACE, ForcedExitAction, Joined, ShutdownWait, StopRequest, WaitableThread,
        Wake, forced_exit_action, spawn_waitable,
    };

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

    /// A thread that ends only when its sender is sent to or dropped, to stand for one inside a
    /// call that a hung file system never answers.
    fn held_thread() -> (WaitableThread<()>, Sender<()>) {
        let (release, released) = crossbeam_channel::bounded::<()>(0);
        let thread = spawn_waitable("excise-test-held", move || {
            let _ = released.recv();
        })
        .expect("the held thread should start");
        (thread, release)
    }

    /// As [`held_thread`], for the one wait that takes a plain handle: the deletion executor's.
    fn held_executor() -> (JoinHandle<()>, Sender<()>) {
        let (release, released) = crossbeam_channel::bounded::<()>(0);
        let thread = thread::spawn(move || {
            let _ = released.recv();
        });
        (thread, release)
    }

    /// A wait that nothing but the thread's end can wake: its stop requests never come, and with
    /// no forced stop there is no grace to run out. It must wake on the end signal, whether the
    /// thread ends before the wait blocks or after.
    #[test]
    fn a_wait_wakes_on_the_end_signal_and_nothing_else() {
        let (stops, receiver) = crossbeam_channel::unbounded::<StopRequest>();
        let (held, release) = held_thread();
        let (blocking, waiter_started) = crossbeam_channel::bounded::<()>(0);
        let (reported, wake) = crossbeam_channel::bounded(1);
        let waiter = thread::spawn(move || {
            let mut wait = ShutdownWait::until_forced(Some(receiver), false);
            blocking.send(()).expect("the test should be listening");
            let _ = reported.send(wait.wait_for_end(&held.ended));
        });

        waiter_started.recv().expect("the waiter should start");
        drop(release);

        assert_eq!(
            wake.recv_timeout(Duration::from_secs(60))
                .expect("the wait must wake once the thread ends"),
            Wake::Ended
        );
        waiter.join().expect("the waiting thread should end");
        drop(stops);
    }

    /// An end signal that has fired is all a wait needs: there is no thread here to ask whether
    /// it ended.
    #[test]
    fn a_fired_end_signal_ends_a_wait_with_no_thread_to_poll() {
        let (end, ended) = crossbeam_channel::bounded::<Infallible>(0);
        drop(end);

        assert_eq!(ShutdownWait::patient().wait_for_end(&ended), Wake::Ended);
    }

    /// A grace that has run out gives up on a thread that has not ended, without a request being
    /// read; a thread that has ended is joined all the same, even with the grace out and a request
    /// still waiting, as it always was.
    #[test]
    fn the_end_of_a_thread_outranks_a_grace_that_ran_out_and_a_waiting_request() {
        let (stops, receiver) = crossbeam_channel::unbounded();
        stops
            .send(StopRequest::Forced)
            .expect("the second request should be accepted");
        let mut wait = ShutdownWait::until_forced(Some(receiver), true);
        wait.give_up_at = Some(Instant::now());
        let (end, ended) = crossbeam_channel::bounded::<Infallible>(0);

        assert_eq!(
            wait.wait_for_end(&ended),
            Wake::GaveUp,
            "the grace had run out on a thread that had not ended"
        );
        drop(end);
        assert_eq!(
            wait.wait_for_end(&ended),
            Wake::Ended,
            "the grace that ran out was preferred to a thread that had ended"
        );
    }

    /// A thread that panics ends like any other: the unwind drops the guard, so the wait wakes for
    /// it, and the join reports the panic it ended with. The wait has no grace to give up on, so a
    /// guard that did not fire would hang it: the test bounds how long it waits instead.
    #[test]
    fn a_thread_that_panics_is_seen_to_end_and_reported_as_a_panic() {
        let panicking = spawn_waitable::<(), _>("excise-test-panic", || {
            std::panic::resume_unwind(Box::new("the thread failed"))
        })
        .expect("the panicking thread should start");
        let (joined, outcome) = crossbeam_channel::bounded(1);
        let waiter = thread::spawn(move || {
            let _ = joined.send(ShutdownWait::patient().join(panicking));
        });

        let Joined::Ended(Err(payload)) = outcome
            .recv_timeout(Duration::from_secs(60))
            .expect("a thread that panicked must still be seen to end")
        else {
            panic!("the wait did not report the thread's panic");
        };
        assert_eq!(payload.downcast_ref::<&str>(), Some(&"the thread failed"));
        waiter.join().expect("the waiting thread should end");
    }

    /// A first request is the quit a shutdown already is, so it leaves the wait for a thread
    /// alone; a second one ends it, but only after the grace, which threads that are merely
    /// stopping can use.
    #[test]
    fn a_second_request_ends_a_wait_after_the_grace_and_a_first_one_does_not() {
        let (stops, receiver) = crossbeam_channel::unbounded();
        let (held, release) = held_thread();
        let (ended, outcome) = crossbeam_channel::bounded(1);
        let waiter = thread::spawn(move || {
            let mut wait = ShutdownWait::until_forced(Some(receiver), false);
            let _ = ended.send(matches!(wait.join(held), Joined::Abandoned));
        });

        stops
            .send(StopRequest::Graceful)
            .expect("the first request should be accepted");
        assert!(
            outcome.recv_timeout(Duration::from_millis(300)).is_err(),
            "a first request ended the wait for a thread that had not ended"
        );
        let forced_at = Instant::now();
        stops
            .send(StopRequest::Forced)
            .expect("the second request should be accepted");

        assert!(
            outcome
                .recv_timeout(FORCED_STOP_GRACE + Duration::from_secs(4))
                .expect("a second request must end the wait"),
            "the wait ended without giving up on the thread"
        );
        assert!(
            forced_at.elapsed() >= FORCED_STOP_GRACE,
            "the thread was given no grace to end"
        );
        drop(release);
        waiter.join().expect("the waiting thread should end");
    }

    /// An impatient pair of signals must still let a thread that is stopping normally end, or
    /// the session's storage it holds would be left behind for no reason.
    #[test]
    fn a_forced_stop_still_joins_a_thread_that_ends_within_the_grace() {
        let (held, release) = held_thread();
        let releaser = thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            drop(release);
        });
        let mut wait = ShutdownWait::until_forced(None, true);

        assert!(matches!(wait.join(held), Joined::Ended(Ok(()))));
        releaser.join().expect("the releasing thread should end");
    }

    /// The deletion executor is waited for however long it takes: a forced stop, already taken or
    /// still waiting in the channel, does not end that wait. The grace does not run while it
    /// lasts, so the waits after it still get it in full: a thread that ends a moment later is
    /// joined, not left behind, which is what keeps the session's storage from being left too.
    #[test]
    fn a_patient_wait_outlasts_the_grace_and_leaves_the_grace_to_the_waits_after_it() {
        let (stops, receiver) = crossbeam_channel::unbounded();
        stops
            .send(StopRequest::Forced)
            .expect("the second request should be accepted");
        let (slow, release_slow) = held_executor();
        let (next, release_next) = held_thread();
        let releaser = thread::spawn(move || {
            thread::sleep(FORCED_STOP_GRACE + Duration::from_millis(400));
            drop(release_slow);
            thread::sleep(Duration::from_millis(200));
            drop(release_next);
        });
        let mut wait = ShutdownWait::until_forced(Some(receiver), true);
        let started = Instant::now();

        assert!(wait.join_patiently(slow).is_ok());
        assert!(
            started.elapsed() >= FORCED_STOP_GRACE,
            "the patient wait ended with the grace"
        );
        assert!(
            matches!(wait.join(next), Joined::Ended(Ok(()))),
            "the grace was used up while the patient wait ran"
        );
        releaser.join().expect("the releasing thread should end");
    }
}
