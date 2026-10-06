//! A person's interruption of a soak.
//!
//! A soak can run for most of an hour on a real tree, so the person who started it can stop it. The
//! command's signal handler (the harness itself installs none) sets an [`Interrupt`], and the soak
//! looks at it where it waits: it ends the program it is running with the program's whole process
//! group, writes down what it has, and returns.
//!
//! The handler may end the command at once when the person asks again, and it must not while the
//! soak still has a program to end: the build, a scan, and a session each run in a process group
//! that the signal of a terminal does not reach, so a command that ended then would leave the
//! program running without the bounds of the run. The soak says when it may, and what it says is
//! never stale, because it is said under one lock ([`Interrupt::acted_on_flag`] is what the handler
//! reads):
//!
//! * The soak holds a [`Supervision`] for as long as it has a program that it has not ended and
//!   waited for, or given up on. Taking one is refused once the person has asked to stop
//!   ([`Interrupted`]), so no program starts after that, and none starts after the exit is armed.
//! * The exit is armed when the person has asked to stop and no program is held: by the drop of the
//!   last [`Supervision`], and by a thread of its own ([`Interrupt::start_armer`]). The thread that
//!   runs the soak cannot be relied on for it: it can be blocked in the file system (a scratch
//!   area on a mount that has stopped answering) and look at nothing.
//! * A look ([`Interrupt::is_set`]) arms nothing. Any thread may look at any time, the report
//!   watcher that a file system that does not answer has left behind included.

use std::{
    io,
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use thiserror::Error;

/// How often the thread that arms the exit looks ([`Interrupt::start_armer`]). It is the longest
/// that a second press is taken for the first when nothing else can arm the exit.
pub const ARM_EVERY: Duration = Duration::from_millis(25);

/// The person had already asked the soak to stop when it was about to start a program, so the
/// program was not started ([`Interrupt::supervising`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("the soak was asked to stop, so no program was started")]
pub struct Interrupted;

/// A flag that says the person asked the soak to stop, whether the exit that a second press ends
/// the command with is armed, and how many programs the soak holds. Clones share all three.
#[derive(Debug, Clone, Default)]
pub struct Interrupt {
    /// The person asked the soak to stop. A signal handler can only store to it, which is all it
    /// does.
    flag: Arc<AtomicBool>,
    /// Whether the exit is armed: the person asked the soak to stop and no program was left to end.
    /// Read by the signal handlers, so it is an atomic, and written only with `programs` locked.
    acted_on: Arc<AtomicBool>,
    /// How many programs the soak has started and has not yet ended or given up on. Its lock is
    /// the one under which a program is counted or refused and the exit is armed, so that no
    /// program starts between the look that arms the exit and the arming.
    programs: Arc<Mutex<usize>>,
}

impl Interrupt {
    /// A flag that is not set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Asks the soak to stop. Safe to call from any thread, and idempotent.
    pub fn trigger(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }

    /// Whether the soak was asked to stop. A look only reads: it arms nothing, so that any thread
    /// may make it at any time. The exit is armed by the drop of the last [`Supervision`] and by
    /// the thread of [`Interrupt::start_armer`].
    #[must_use]
    pub fn is_set(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// The flag itself, for a signal handler that sets a shared `AtomicBool` (the handler of
    /// `signal_hook::flag::register` does).
    #[must_use]
    pub fn flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.flag)
    }

    /// The flag, borrowed: what [`run_cancellable`](crate::headless::process::run_cancellable)
    /// watches.
    #[must_use]
    pub fn as_flag(&self) -> &AtomicBool {
        &self.flag
    }

    /// The flag that says the exit is armed: the person asked the soak to stop, and no program is
    /// left that the soak has to end (the last one has been ended, or given up on after the
    /// bounded wait for it, or none was running). It is set only after an interrupt and only then,
    /// by the drop of the last [`Supervision`] and by the thread of [`Interrupt::start_armer`],
    /// never by a look. A signal handler that ends the command on a second signal checks it, so
    /// that a second signal that comes while a program is still being ended is the same request as
    /// the first and does not leave the program running, and the one that comes after is the
    /// person asking again.
    #[must_use]
    pub fn acted_on_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.acted_on)
    }

    /// Whether the exit is armed ([`Interrupt::acted_on_flag`]).
    #[must_use]
    pub fn is_acted_on(&self) -> bool {
        self.acted_on.load(Ordering::SeqCst)
    }

    /// Notes that the soak has a program to end, until the [`Supervision`] is dropped. Taken by
    /// the code that starts a program, before it starts it, and dropped as soon as the program has
    /// been ended and waited for, or given up on after the bounded wait for it: before the report
    /// is read, the scratch area is looked at, the recording is finished, or the scratch area is
    /// removed. That work can block for as long as a file system takes to answer, and the exit is
    /// not armed while a program is held.
    ///
    /// # Errors
    ///
    /// Returns [`Interrupted`] when the person has already asked the soak to stop: no program
    /// starts after that. The check and the count are made under the lock that arms the exit, so a
    /// program never starts once the exit is armed.
    pub fn supervising(&self) -> Result<Supervision, Interrupted> {
        let mut programs = self.lock();
        if self.flag.load(Ordering::SeqCst) {
            return Err(Interrupted);
        }
        *programs += 1;
        Ok(Supervision {
            interrupt: self.clone(),
        })
    }

    /// How many programs the soak has started and has not yet ended or given up on.
    #[must_use]
    pub fn supervised(&self) -> usize {
        *self.lock()
    }

    /// Starts the thread that arms the exit by itself, every [`ARM_EVERY`], until the [`Armer`]
    /// that it returns is dropped. The soak's own thread arms it when it drops the last
    /// [`Supervision`], but it cannot be relied on to look: when the person asks to stop and no
    /// program is held, that thread can be blocked in the file system, and then nothing else would
    /// arm the exit and every later press would be swallowed.
    ///
    /// # Errors
    ///
    /// Returns the error of the system when the thread cannot be started.
    pub fn start_armer(&self) -> io::Result<Armer> {
        self.start_armer_every(ARM_EVERY)
    }

    /// [`Interrupt::start_armer`] with the time between two looks given, so that a test need not
    /// wait for the cadence of a run.
    fn start_armer_every(&self, every: Duration) -> io::Result<Armer> {
        let over = Arc::new(AtomicBool::new(false));
        let interrupt = self.clone();
        let ended = Arc::clone(&over);
        let thread = thread::Builder::new()
            .name("soak-interrupt".to_owned())
            .spawn(move || {
                while !ended.load(Ordering::SeqCst) {
                    interrupt.arm_if_idle();
                    thread::park_timeout(every);
                }
            })?;
        Ok(Armer {
            over,
            thread: Some(thread),
        })
    }

    /// Arms the exit when the person has asked the soak to stop and no program is held. The only
    /// way to arm it besides the drop of the last [`Supervision`].
    fn arm_if_idle(&self) {
        let programs = self.lock();
        self.arm_when_idle(*programs);
    }

    /// Arms the exit if the person has asked to stop and `programs` is none. The caller holds the
    /// lock that `programs` was read under.
    fn arm_when_idle(&self, programs: usize) {
        if programs == 0 && self.flag.load(Ordering::SeqCst) {
            self.acted_on.store(true, Ordering::SeqCst);
        }
    }

    /// The count of programs, locked. A thread that panicked with it held left a count that is
    /// still the count: nothing is half-done under it.
    fn lock(&self) -> MutexGuard<'_, usize> {
        self.programs.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// What the soak holds for as long as a program that it started is its to end. Dropping it says
/// that the program is ended and waited for (or was given up on), and the exit is then armed, if the
/// person has asked to stop and no other program is left.
#[derive(Debug)]
#[must_use = "dropping it at once says that the program has ended"]
pub struct Supervision {
    interrupt: Interrupt,
}

impl Drop for Supervision {
    fn drop(&mut self) {
        let mut programs = self.interrupt.lock();
        *programs = programs.saturating_sub(1);
        self.interrupt.arm_when_idle(*programs);
    }
}

/// The thread that arms the exit by itself ([`Interrupt::start_armer`]). It ends when this is
/// dropped.
#[derive(Debug)]
#[must_use = "the thread ends when this is dropped"]
pub struct Armer {
    over: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Drop for Armer {
    /// Ends the thread and waits for it, which is quick: it holds the lock for no longer than it
    /// takes to read two atomics and a count, and does nothing else.
    fn drop(&mut self) {
        self.over.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::atomic::AtomicUsize, time::Instant};

    use super::*;

    /// A program held, where the test has not asked the soak to stop yet.
    fn held(interrupt: &Interrupt) -> Supervision {
        interrupt
            .supervising()
            .expect("the interrupt is not set, so a program may start")
    }

    /// Whether the exit is armed within `bound`, read as a signal handler reads it: by loading the
    /// atomic, with no look at the interrupt and no call that takes the lock.
    fn armed_within(interrupt: &Interrupt, bound: Duration) -> bool {
        let armed = interrupt.acted_on_flag();
        let give_up = Instant::now() + bound;
        while !armed.load(Ordering::SeqCst) {
            if Instant::now() >= give_up {
                return false;
            }
            thread::sleep(Duration::from_millis(1));
        }
        true
    }

    #[test]
    fn a_trigger_is_seen_by_every_clone_and_stays_set() {
        let interrupt = Interrupt::new();
        let clone = interrupt.clone();
        let flag = interrupt.flag();

        assert!(!clone.is_set());
        interrupt.trigger();
        interrupt.trigger();

        assert!(clone.is_set());
        assert!(flag.load(Ordering::SeqCst));
    }

    #[test]
    fn setting_the_shared_flag_directly_is_an_interrupt() {
        let interrupt = Interrupt::new();

        interrupt.flag().store(true, Ordering::SeqCst);

        assert!(interrupt.is_set());
    }

    #[test]
    fn nothing_is_armed_until_an_interrupt_has_come() {
        let interrupt = Interrupt::new();
        let armer = interrupt
            .start_armer_every(Duration::from_millis(1))
            .expect("the thread starts");

        assert!(!interrupt.is_set());
        drop(held(&interrupt));
        thread::sleep(Duration::from_millis(50));

        assert!(!interrupt.is_acted_on(), "no interrupt came");
        assert!(!interrupt.acted_on_flag().load(Ordering::SeqCst));
        drop(armer);
    }

    #[test]
    fn a_look_arms_nothing_whichever_thread_makes_it() {
        // Before the exit was armed only by the drop of the last supervision and the thread that
        // arms it, a look from the report watcher that a file system had left behind could arm it
        // just before the next program started.
        let interrupt = Interrupt::new();
        interrupt.trigger();

        assert!(interrupt.is_set());
        let clone = interrupt.clone();
        thread::spawn(move || {
            assert!(clone.is_set());
            assert!(clone.as_flag().load(Ordering::SeqCst));
            assert_eq!(clone.supervised(), 0);
        })
        .join()
        .expect("the look of another thread");

        assert!(!interrupt.is_acted_on(), "a look has no effect");
        interrupt.arm_if_idle();
        assert!(interrupt.is_acted_on(), "only the arming path arms it");
    }

    #[test]
    fn an_interrupt_is_not_armed_while_a_program_is_still_to_be_ended_and_is_when_it_is() {
        let interrupt = Interrupt::new();
        let armer = interrupt
            .start_armer_every(Duration::from_millis(1))
            .expect("the thread starts");
        let program = held(&interrupt);
        assert_eq!(interrupt.supervised(), 1);

        interrupt.trigger();
        // The supervisor of the program looks at the interrupt every few milliseconds, which is
        // how it learns that it must end the program, and the thread that arms the exit looks at
        // it too: neither is the program ended.
        for _ in 0..3 {
            assert!(interrupt.is_set());
        }
        thread::sleep(Duration::from_millis(100));
        assert!(!interrupt.is_acted_on(), "the program is still there");

        drop(program);

        assert_eq!(interrupt.supervised(), 0);
        assert!(
            interrupt.is_acted_on(),
            "the program is ended, and the drop arms the exit by itself: no look is waited for"
        );
        drop(armer);
    }

    #[test]
    fn the_last_of_several_programs_is_the_one_whose_end_arms_the_exit() {
        let interrupt = Interrupt::new();
        let first = held(&interrupt);
        let second = held(&interrupt);
        interrupt.trigger();

        drop(first);
        assert!(
            !interrupt.is_acted_on(),
            "one program is still left to end: {}",
            interrupt.supervised()
        );
        interrupt.arm_if_idle();
        assert!(!interrupt.is_acted_on());
        drop(second);

        assert!(interrupt.is_acted_on());
    }

    #[test]
    fn with_no_program_held_and_nothing_looking_an_interrupt_arms_the_exit_within_a_bound() {
        // The thread that runs the soak can be blocked in the file system (a scratch trust check, a
        // removal, a look at what a program left on a mount that has stopped answering): it makes
        // no call into the interrupt, which is modelled by this test's thread reading the atomic
        // and nothing else. A program that was held and ended before makes no difference.
        let interrupt = Interrupt::new();
        drop(held(&interrupt));
        let armer = interrupt.start_armer().expect("the thread starts");

        // What a signal handler does: a store to the flag, and nothing else.
        interrupt.flag().store(true, Ordering::SeqCst);

        assert!(
            armed_within(&interrupt, Duration::from_secs(5)),
            "nothing armed the exit, so every later press would be swallowed"
        );
        assert_eq!(interrupt.supervised(), 0);
        drop(armer);
    }

    #[test]
    fn no_program_is_started_once_the_person_has_asked_to_stop() {
        let interrupt = Interrupt::new();
        let program = held(&interrupt);
        interrupt.trigger();

        assert!(matches!(interrupt.supervising(), Err(Interrupted)));
        assert_eq!(
            interrupt.supervised(),
            1,
            "the program that was refused was not counted"
        );
        assert!(!interrupt.is_acted_on());
        drop(program);
        assert_eq!(interrupt.supervised(), 0);
        assert!(matches!(interrupt.supervising(), Err(Interrupted)), "armed");
        assert!(Interrupted.to_string().contains("no program was started"));
    }

    #[test]
    fn a_program_is_never_started_once_the_exit_is_armed_and_the_exit_is_never_armed_while_one_is_held()
     {
        // Several threads start and end programs as fast as they can while the interrupt comes and
        // the thread that arms the exit looks every millisecond. A program that was started has
        // never found the exit armed, whether just after it was started or just before it was
        // ended, and once the exit is armed nothing is started.
        for _ in 0..30 {
            let interrupt = Interrupt::new();
            let armer = interrupt
                .start_armer_every(Duration::from_millis(1))
                .expect("the thread starts");
            let violations = AtomicUsize::new(0);
            let started_after_armed = AtomicUsize::new(0);
            let over = AtomicBool::new(false);

            thread::scope(|scope| {
                for _ in 0..3 {
                    scope.spawn(|| {
                        while !over.load(Ordering::SeqCst) {
                            let armed_before = interrupt.is_acted_on();
                            let Ok(program) = interrupt.supervising() else {
                                continue;
                            };
                            if armed_before {
                                started_after_armed.fetch_add(1, Ordering::SeqCst);
                            }
                            if interrupt.is_acted_on() {
                                violations.fetch_add(1, Ordering::SeqCst);
                            }
                            thread::yield_now();
                            if interrupt.is_acted_on() {
                                violations.fetch_add(1, Ordering::SeqCst);
                            }
                            drop(program);
                        }
                    });
                }
                thread::sleep(Duration::from_millis(3));
                interrupt.trigger();
                assert!(armed_within(&interrupt, Duration::from_secs(10)));
                over.store(true, Ordering::SeqCst);
            });

            assert_eq!(violations.load(Ordering::SeqCst), 0);
            assert_eq!(started_after_armed.load(Ordering::SeqCst), 0);
            assert_eq!(interrupt.supervised(), 0);
            drop(armer);
        }
    }

    #[test]
    fn the_acted_on_flag_is_shared_with_every_clone() {
        let interrupt = Interrupt::new();
        let clone = interrupt.clone();
        let flag = interrupt.acted_on_flag();
        let program = held(&clone);
        interrupt.trigger();
        assert!(clone.is_set());
        assert!(!flag.load(Ordering::SeqCst));

        drop(program);

        assert!(flag.load(Ordering::SeqCst));
        assert!(interrupt.is_acted_on());
        assert!(clone.is_acted_on());
    }

    #[test]
    fn dropping_the_armer_ends_its_thread() {
        let interrupt = Interrupt::new();
        let armer = interrupt
            .start_armer_every(Duration::from_millis(1))
            .expect("the thread starts");
        assert!(
            Arc::strong_count(&interrupt.programs) > 1,
            "the thread holds the interrupt"
        );

        drop(armer);

        assert_eq!(
            Arc::strong_count(&interrupt.programs),
            1,
            "the thread has ended, and nothing holds the interrupt but this"
        );
        interrupt.trigger();
        thread::sleep(Duration::from_millis(50));
        assert!(!interrupt.is_acted_on(), "no thread is left to arm it");
    }
}
