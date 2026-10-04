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
#[cfg(any(windows, test))]
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;

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
    quit_end: QuitEnd,
}

impl StopSignals {
    /// A clone of the receiving end, held by the owner loop or the headless scan loop.
    pub(crate) fn receiver(&self) -> Receiver<StopRequest> {
        self.receiver.clone()
    }

    /// Tells every Windows console control handler thread, those waiting for this run's confirmed
    /// quit to finish (terminal restored, session storage removed, any report written) and any
    /// that starts waiting later, that it has: see `os::windows::install_console_ctrl_handler`. A
    /// handler does not answer Windows after that, so the exit that follows, with this run's own
    /// status, is what ends the process (see `await_quit_end`). Idempotent and a no-op on Unix,
    /// where nothing waits on it and process exit is not on a deadline the way a Windows close,
    /// logoff, or shutdown event is. Called exactly once, by `cli::run_main`, as soon as the run
    /// has returned; on Windows the end of the run tells the handlers the same if it never gets
    /// there.
    #[allow(
        clippy::unused_self,
        reason = "self is read on Windows (the quit's end); unused on Unix, where nothing waits \
                  on it"
    )]
    pub(crate) fn acknowledge_done(&self) {
        #[cfg(windows)]
        self.quit_end.end();
    }
}

/// The end of the run, however it ends, tells the handlers what the acknowledgement does: a panic
/// that unwinds out of `cli::run_main` before it gets there included.
#[cfg(windows)]
impl Drop for StopSignals {
    fn drop(&mut self) {
        self.quit_end.end();
    }
}

/// Whether this run's confirmed quit has ended, as every Windows console control handler thread
/// sees it: those waiting for it when it ends and those that start waiting later alike. Windows
/// runs each event's handler on a thread of its own, and events overlap (a break, then a close)
/// and arrive late (while the run is already exiting), so the end of the quit is a state that all
/// of them read, never a message that one of them would take from the others. Clones share it.
#[cfg(any(windows, test))]
#[derive(Clone, Default)]
pub(crate) struct QuitEnd(Arc<QuitEndState>);

#[cfg(any(windows, test))]
#[derive(Default)]
struct QuitEndState {
    ended: Mutex<bool>,
    changed: Condvar,
}

#[cfg(any(windows, test))]
impl QuitEnd {
    /// Records that the quit has ended and wakes every thread waiting for that. A later call
    /// changes nothing.
    fn end(&self) {
        *self.0.ended.lock().unwrap_or_else(PoisonError::into_inner) = true;
        self.0.changed.notify_all();
    }

    /// Waits up to `wait` for the quit to end, and says whether it has: at once, when it already
    /// had.
    fn ended_within(&self, wait: Duration) -> bool {
        let ended = self.0.ended.lock().unwrap_or_else(PoisonError::into_inner);
        let (ended, _) = self
            .0
            .changed
            .wait_timeout_while(ended, wait, |ended| !*ended)
            .unwrap_or_else(PoisonError::into_inner);
        *ended
    }
}

/// What the Windows console control handler does once it has handed its request to the run, up to
/// the point where it may answer Windows. Waits up to `wait` for the quit to end (at once, when
/// it already has: see [`QuitEnd`]). A quit that has not ended by then is given up on, and this
/// returns, so that the handler answers as it always has. One that has ended is handed to `hold`,
/// which must not return while the process lives: from then on the handler does not answer at
/// all.
///
/// Answering a close, logoff, or shutdown event is what lets Windows end the process, at once,
/// with a status of its own (an `NTSTATUS` such as `STATUS_CONTROL_C_EXIT`). When the quit has
/// ended, the run is already on its way out with its own status, `130` for a quit, and whichever
/// of the two ends the process first decides the status that a parent process reads. A handler
/// that answered as soon as the quit ended made that a race. One that does not answer leaves the
/// end of the process to the run, and the run's exit takes the handler's thread with it, which
/// the `HandlerRoutine` documentation allows for ("it is possible that the handler function will
/// be terminated by another thread in the process"). `hold` is a parameter so that a test can
/// stand in for the end of the process.
#[cfg(any(windows, test))]
pub(crate) fn await_quit_end(quit_end: &QuitEnd, wait: Duration, hold: impl FnOnce()) {
    if quit_end.ended_within(wait) {
        hold();
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
    let (receiver, quit_end) = crate::os::windows::install_console_ctrl_handler()?;
    Ok(StopSignals { receiver, quit_end })
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::convert::Infallible;
    use std::path::PathBuf;
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};

    use crossbeam_channel::{Receiver, Sender};

    #[cfg(windows)]
    use super::StopSignals;
    use super::{
        FORCED_STOP_GRACE, ForcedExitAction, Joined, QuitEnd, ShutdownWait, StopRequest,
        WaitableThread, Wake, await_quit_end, forced_exit_action, spawn_waitable,
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

    /// A console control handler's thread, running `await_quit_end` as the handler does, with a
    /// `hold` that says it was entered (`entered`) and then waits for the test to let a stand-in
    /// for the end of the process fire (`process_ends`). `answered` hears when the handler would
    /// answer Windows, which only a `hold` that returned lets it do. A handler that answers
    /// without ever holding is therefore told from one that holds by which of the two it reports
    /// first, with no stretch of silence to wait out.
    struct HandlerThread {
        entered: Receiver<()>,
        answered: Receiver<()>,
        process_ends: Sender<()>,
        thread: JoinHandle<()>,
    }

    impl HandlerThread {
        fn start(quit_end: &QuitEnd, wait: Duration) -> Self {
            let quit_end = quit_end.clone();
            let (entered_hold, entered) = crossbeam_channel::bounded::<()>(1);
            let (answer, answered) = crossbeam_channel::bounded::<()>(1);
            let (process_ends, process_ended) = crossbeam_channel::bounded::<()>(1);
            let thread = thread::spawn(move || {
                await_quit_end(&quit_end, wait, || {
                    let _ = entered_hold.send(());
                    let _ = process_ended.recv();
                });
                let _ = answer.send(());
            });
            Self {
                entered,
                answered,
                process_ends,
                thread,
            }
        }

        /// Waits for the handler to take its hold, and checks that it has not answered Windows.
        fn assert_holds(&self) {
            crossbeam_channel::select! {
                recv(self.entered) -> _ => {}
                recv(self.answered) -> _ => panic!(
                    "the handler answered Windows without holding: Windows would end the process \
                     with its own status, racing the run's exit"
                ),
                default(Duration::from_secs(10)) => panic!(
                    "the handler neither held nor answered within ten seconds of the quit's end"
                ),
            }
            assert!(
                self.answered.try_recv().is_err(),
                "the handler answered Windows while the process still lived"
            );
        }

        /// Lets the stand-in for the process end, and the handler end with it.
        fn end_process(self) {
            self.process_ends
                .send(())
                .expect("the stand-in for the process should accept its end");
            self.answered
                .recv_timeout(Duration::from_secs(10))
                .expect("the handler must be free to end once the process has");
            self.thread.join().expect("the handler thread should end");
        }
    }

    /// Windows ends the process the moment a close handler answers, in a race with the exit the
    /// run is by then on its way to. So a handler whose quit has ended must not answer while the
    /// process lives: the exit of the process, with the run's status, ends it.
    #[test]
    fn a_console_handler_does_not_answer_after_its_quit_ended_while_the_process_lives() {
        let quit_end = QuitEnd::default();
        let handler = HandlerThread::start(&quit_end, Duration::from_secs(60));

        quit_end.end();

        handler.assert_holds();
        handler.end_process();
    }

    /// Windows runs the handlers of overlapping events (a break, then a close) on threads of their
    /// own, and the end of the quit is no message that one of them could take from the others:
    /// every one that is waiting for it must hold.
    #[test]
    fn every_console_handler_waiting_for_the_quit_holds_when_it_ends() {
        let quit_end = QuitEnd::default();
        let first = HandlerThread::start(&quit_end, Duration::from_secs(60));
        let second = HandlerThread::start(&quit_end, Duration::from_secs(60));
        // Both are in their wait by now, but for a badly stalled thread, which would only make
        // this test weaker, not fail it: however the threads interleave, both must hold.
        thread::sleep(Duration::from_millis(100));

        quit_end.end();

        first.assert_holds();
        second.assert_holds();
        first.end_process();
        second.end_process();
    }

    /// An event that arrives late, while the run is already exiting, finds the end of the quit
    /// known already, though an earlier handler has heard of it: the late handler holds at once.
    /// Its bound is a minute and `assert_holds` waits ten seconds, so a handler that had to wait
    /// out its bound before holding could not pass.
    #[test]
    fn a_console_handler_that_starts_after_the_quit_ended_holds_at_once() {
        let quit_end = QuitEnd::default();
        let earlier = HandlerThread::start(&quit_end, Duration::from_secs(60));
        quit_end.end();
        earlier.assert_holds();

        let late = HandlerThread::start(&quit_end, Duration::from_secs(60));

        late.assert_holds();
        earlier.end_process();
        late.end_process();
    }

    /// A quit that does not end in time is still given up on, as before: the handler then
    /// answers, and for a close, logoff, or shutdown event Windows ends the process, before its
    /// own time-out does.
    #[test]
    fn a_console_handler_gives_up_on_a_quit_that_does_not_end_in_time() {
        let quit_end = QuitEnd::default();
        let held = Cell::new(false);
        let wait = Duration::from_millis(150);
        let started = Instant::now();

        await_quit_end(&quit_end, wait, || held.set(true));

        assert!(started.elapsed() >= wait, "the handler gave up early");
        assert!(
            !held.get(),
            "the handler held for a quit that had not ended"
        );
    }

    /// The run reaches the handlers through `StopSignals`: when it acknowledges the end of its
    /// quit, and when it just ends, as a panic unwinding out of `cli::run_main` would end it.
    #[cfg(windows)]
    #[test]
    fn the_run_tells_the_console_handlers_when_it_acknowledges_and_when_it_just_ends() {
        let run_of = |quit_end: &QuitEnd| StopSignals {
            receiver: crossbeam_channel::unbounded().1,
            quit_end: quit_end.clone(),
        };
        let acknowledged = QuitEnd::default();
        let run = run_of(&acknowledged);
        assert!(
            !acknowledged.ended_within(Duration::ZERO),
            "the quit had not ended yet"
        );

        run.acknowledge_done();

        assert!(
            acknowledged.ended_within(Duration::ZERO),
            "the acknowledgement did not tell the handlers"
        );
        drop(run);
        let unwound = QuitEnd::default();

        drop(run_of(&unwound));

        assert!(
            unwound.ended_within(Duration::ZERO),
            "the end of the run did not tell the handlers"
        );
    }
}
