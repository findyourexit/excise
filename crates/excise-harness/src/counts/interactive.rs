//! The interactive session of a count: the program run in a pseudo-terminal until its scan is
//! complete, checked to be the scan that was counted, quit as a user quits it, and the scratch
//! area that it leaves behind counted.
//!
//! # The scan has to be the one that was counted
//!
//! `scan_complete` also follows a scan that recoverable failures left inexact, and it carries
//! only an entry count. Two signals the program already gives are used, so that no change to it
//! is needed. The entries it reports must be those of the headless scan of the same fixture,
//! which was required to end exact: a scan that lost entries to a failure counts fewer. And the
//! header badge must read `COMPLETE`, which the program shows only while the scan root's own
//! state is complete: a folder that cannot be read makes the root, and every folder above it,
//! uncertain, and the badge then says why (`READ ERROR`, `NEEDS REVIEW`, ...). The frame that
//! shows how the scan ended is drawn after `scan_complete`, so the badge is read once it has
//! stopped reading `SCANNING`.
//!
//! # The deadline
//!
//! The timeout is the whole session's. It is counted from the start that the session recorded
//! before it launched the program, so time spent starting the program counts, and every wait ends
//! by it. Progress that is first seen after it is not accepted: an event, and the exit, are
//! judged by the time the harness first saw them, which is recorded with them, so a parent that
//! was not scheduled for a while cannot take late progress for timely progress.
//!
//! # What is not counted
//!
//! The descriptors and threads that the program holds once its scan is complete are not counted,
//! though they once were. Nothing outside the program says that it has finished what follows its
//! scan, and no window of silence proves it: a worker that is blocked, or not scheduled, for
//! longer than the window yields a value that is too early, and two sessions agree on it, so the
//! check that makes a count deterministic cannot see it. They can return when the program says
//! so itself: an `idle` test event, written once the work that follows its scan is done, would be
//! the barrier.

use std::{
    path::Path,
    time::{Duration, Instant},
};

use crate::{
    events::{EventLog, Payload},
    pty::{
        PtySession, SpawnSpec,
        ui::{HeaderState, header_state},
    },
    runner::resolve_binary,
    safety::{FixtureRoot, Scratch, isolated_env},
};

use super::{PROFILE, measure::CountsError, text::log_safe};

/// The terminal the program is shown.
const COLS: u16 = 120;
const ROWS: u16 = 40;
/// How often the session is read while nothing has happened.
const POLL: Duration = Duration::from_millis(20);
/// The longest the header has, once the scan is complete, to show how it ended. The frame that
/// shows it is drawn within tens of milliseconds: this is for a program that never draws it. The
/// session's own timeout caps it as well.
const HEADER_LIMIT: Duration = Duration::from_secs(10);
/// The longest the program has to quit once the quit is confirmed. The session's own timeout caps
/// it as well.
const QUIT_LIMIT: Duration = Duration::from_secs(60);
/// What a timeout that is too long to add to an instant is taken for: ten years, longer than any
/// run.
const NEVER: Duration = Duration::from_hours(87_600);

/// Whether this platform runs the interactive session: where the harness has exercised it.
pub(super) const SUPPORTED: bool = cfg!(any(target_os = "linux", target_os = "macos"));

/// When a session that began at `started` must be over: `timeout` after it.
fn deadline_after(started: Instant, timeout: Duration) -> Instant {
    started
        .checked_add(timeout)
        .unwrap_or_else(|| started + NEVER)
}

/// A running interactive session, and what it takes to follow it.
struct Probe<'a> {
    fixture: &'a str,
    session: PtySession,
    events: EventLog,
    /// When the whole session must be over: its timeout after its start, which the session
    /// recorded before it launched the program. Every wait ends by then, and nothing first seen
    /// after it is progress.
    deadline: Instant,
    timeout: Duration,
}

impl<'a> Probe<'a> {
    fn new(fixture: &'a str, session: PtySession, events: EventLog, timeout: Duration) -> Self {
        Self {
            fixture,
            deadline: deadline_after(session.started(), timeout),
            session,
            events,
            timeout,
        }
    }

    fn failed(&self, what: impl Into<String>) -> CountsError {
        CountsError::Failed {
            fixture: self.fixture.to_owned(),
            what: what.into(),
        }
    }

    fn timed_out(&self, what: &'static str) -> CountsError {
        CountsError::TimedOut {
            fixture: self.fixture.to_owned(),
            what,
            timeout: self.timeout,
        }
    }

    /// Reads what the program wrote to the terminal and to the event channel.
    fn pump(&mut self) -> Result<(), CountsError> {
        self.session.pump()?;
        self.events.poll(Instant::now())?;
        Ok(())
    }

    /// Reads the session until `wanted` has been seen in the event channel, by the deadline.
    fn wait_for(
        &mut self,
        wanted: fn(&Payload) -> bool,
        what: &'static str,
    ) -> Result<(), CountsError> {
        loop {
            self.pump()?;
            if let Some(event) = self
                .events
                .events()
                .iter()
                .find(|event| wanted(&event.payload))
            {
                // The time that the harness read it is recorded with the event: a parent that
                // was not scheduled until after the deadline reads it all at once, and what it
                // reads then is not progress that was made in time.
                return if event.observed < self.deadline {
                    Ok(())
                } else {
                    Err(self.timed_out(what))
                };
            }
            if self.session.finished() {
                return Err(self.failed(format!(
                    "the program ended while waiting for {what} ({})",
                    self.session
                        .exit()
                        .map_or_else(|| "no exit status".to_owned(), |exit| exit.describe())
                )));
            }
            if Instant::now() >= self.deadline {
                return Err(self.timed_out(what));
            }
            self.session.wait_activity(POLL)?;
        }
    }

    /// The entries that `scan_complete` reported, which must be those of the headless scan.
    fn require_the_headless_entries(&self, expected: u64) -> Result<(), CountsError> {
        let reported = self
            .events
            .events()
            .iter()
            .find_map(|event| match &event.payload {
                Payload::ScanComplete { entries } => Some(*entries),
                _ => None,
            });
        match reported {
            Some(entries) if entries == expected => Ok(()),
            Some(entries) => Err(self.failed(format!(
                "the interactive scan counted {entries} entries where the headless scan of the \
                 same fixture counted {expected}: it is not the scan that was counted, so its \
                 counts are not recorded"
            ))),
            None => Err(self.failed("the scan's completion carried no entry count")),
        }
    }

    /// Waits, for at most `within`, until the header shows how the scan ended, and requires
    /// `COMPLETE`.
    ///
    /// The event comes before the frame that shows what it reports, so the header reads
    /// `SCANNING` (or is not drawn yet) until that frame arrives. Whatever it reads then is how
    /// the scan ended.
    fn require_an_exact_scan(&mut self, within: Duration) -> Result<(), CountsError> {
        let limit = (Instant::now() + within).min(self.deadline);
        loop {
            self.pump()?;
            let now = Instant::now();
            if now >= self.deadline {
                return Err(self.timed_out("the header to show how the scan ended"));
            }
            match header_state(self.session.screen()) {
                Some(HeaderState::Complete) => return Ok(()),
                Some(HeaderState::Other(label)) => {
                    return Err(self.failed(format!(
                        "the interactive scan did not end exact: its header reads `{}` where an \
                         exact scan reads `COMPLETE`, so its counts are not recorded",
                        log_safe(&label, 40)
                    )));
                }
                Some(HeaderState::Scanning) | None => {}
            }
            if self.session.finished() {
                return Err(
                    self.failed("the program ended before its header showed how the scan ended")
                );
            }
            if now >= limit {
                return Err(self.failed(format!(
                    "the header still reads {} {within:?} after the scan completed, so it is not \
                     known whether the scan ended exact",
                    header_state(self.session.screen()).map_or_else(
                        || "nothing".to_owned(),
                        |state| { format!("`{}`", log_safe(state.label(), 40)) }
                    )
                )));
            }
            self.session.wait_activity(POLL)?;
        }
    }

    /// Quits as a user does (`q`, then `y` at the dialog) and requires a normal exit.
    fn quit(&mut self) -> Result<(), CountsError> {
        self.session.send(b"q")?;
        self.wait_for(
            |payload| matches!(payload, Payload::QuitPrompt),
            "the quit dialog",
        )?;
        self.session.send(b"y")?;
        self.await_exit(QUIT_LIMIT)
    }

    /// Waits, for at most `within`, until the program has exited, and requires a normal exit
    /// that the session first saw before the deadline.
    fn await_exit(&mut self, within: Duration) -> Result<(), CountsError> {
        let limit = (Instant::now() + within).min(self.deadline);
        while !self.session.finished() {
            self.session.pump()?;
            let now = Instant::now();
            if now >= limit {
                return Err(if now >= self.deadline {
                    self.timed_out("the quit")
                } else {
                    self.failed(format!(
                        "the program had not quit {within:?} after the quit was confirmed"
                    ))
                });
            }
            self.session.wait_activity(POLL)?;
        }
        let exit = self
            .session
            .exit()
            .ok_or_else(|| self.failed("the program's exit was not seen"))?;
        // The session may already have seen the exit when this began, so the loop above is no
        // check of when it was seen: the time is recorded with the exit.
        if exit.at >= self.deadline {
            return Err(self.timed_out("the quit"));
        }
        if exit.code == Some(0) {
            Ok(())
        } else {
            Err(self.failed(format!(
                "the program did not exit normally ({})",
                exit.describe()
            )))
        }
    }
}

/// Starts `binary` on `root`, in a new pseudo-terminal and an isolated environment.
fn start(binary: &Path, root: &FixtureRoot, scratch: &Scratch) -> Result<PtySession, CountsError> {
    let program = resolve_binary(binary).map_err(|error| CountsError::Binary(error.to_string()))?;
    PtySession::spawn(&SpawnSpec {
        program,
        args: vec![root.path().as_os_str().to_owned()],
        env: isolated_env(scratch, PROFILE, true, None),
        cwd: scratch.cwd(),
        cols: COLS,
        rows: ROWS,
        drain_bytes_per_sec: None,
        recording: None,
        title: None,
    })
    .map_err(CountsError::from)
}

/// Runs `excise` interactively on `root` under [`PROFILE`], waits for the scan to complete,
/// checks that it is the scan that was counted, quits it as a user would, and returns how many
/// files it left behind in its scratch area.
///
/// `expected_entries` is what the headless scan of the same fixture counted: the interactive
/// scan must report the same, and end exact, or the session is not counted.
///
/// The whole run is bounded by `timeout`, from the start of the program to its exit, and the
/// program's process group is killed if it ends any other way: the session ends it when it is
/// dropped.
///
/// # Errors
///
/// Returns why the program could not be run, did not complete its scan, completed a scan that is
/// not the one that was counted, or did not quit normally, all in time.
pub(super) fn session_residue(
    fixture: &str,
    binary: &Path,
    root: &FixtureRoot,
    work_dir: &Path,
    timeout: Duration,
    expected_entries: u64,
) -> Result<usize, CountsError> {
    let scratch = Scratch::create(work_dir)?;
    let session = start(binary, root, &scratch)?;
    let mut probe = Probe::new(fixture, session, EventLog::new(scratch.events()), timeout);
    probe.wait_for(
        |payload| matches!(payload, Payload::ScanComplete { .. }),
        "the scan to complete",
    )?;
    probe.require_the_headless_entries(expected_entries)?;
    probe.require_an_exact_scan(HEADER_LIMIT)?;
    probe.quit()?;
    Ok(scratch.residue()?.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_timeout_too_long_to_add_to_the_start_is_no_deadline_at_all() {
        let started = Instant::now();

        assert_eq!(
            deadline_after(started, Duration::from_secs(5)),
            started + Duration::from_secs(5)
        );
        assert!(
            deadline_after(started, Duration::MAX) > started + Duration::from_hours(8_760),
            "adding the longest duration to an instant is an overflow, not a deadline"
        );
    }

    /// Stand-ins for `excise`, run through the real session: a shell script that writes the event
    /// channel and the header as the program does.
    #[cfg(all(unix, any(target_os = "linux", target_os = "macos")))]
    mod programs {
        use std::{fs, os::unix::fs::PermissionsExt as _, path::PathBuf, thread};

        use super::*;
        use crate::fixture::MARKER_FILE_NAME;

        /// What the headless scan of the stand-in's fixture counted.
        const EXPECTED_ENTRIES: u64 = 5;
        const GENEROUS: Duration = Duration::from_secs(60);
        /// Draws the header again, as the frame after `scan_complete` does: the badge the
        /// stand-in showed first is replaced.
        const REDRAW: &str = "sleep 0.8; printf '\\033[1;1H EXCISE  /stub  %s\\n'";

        const fn ms(milliseconds: u64) -> Duration {
            Duration::from_millis(milliseconds)
        }

        fn scan_complete(payload: &Payload) -> bool {
            matches!(payload, Payload::ScanComplete { .. })
        }

        fn quit_prompt(payload: &Payload) -> bool {
            matches!(payload, Payload::QuitPrompt)
        }

        /// A stand-in for `excise`: it answers on the event channel and the terminal as the
        /// program does (a header badge, `scan_complete`, the quit dialog, `exit`), and does what
        /// a test says at the moments in between.
        struct Stub {
            /// Run before anything is shown.
            before_scan: &'static str,
            /// The entries `scan_complete` reports.
            entries: u64,
            /// The header badge when it is first drawn: the marker, a space, and the state.
            badge: &'static str,
            /// Run once `scan_complete` has been written.
            after_scan: String,
            /// Run when the quit is confirmed, before the program exits.
            on_confirm: &'static str,
        }

        impl Stub {
            fn normal() -> Self {
                Self {
                    before_scan: "",
                    entries: EXPECTED_ENTRIES,
                    badge: "C COMPLETE",
                    after_scan: String::new(),
                    on_confirm: "",
                }
            }

            /// One that draws `badge` first and, after `scan_complete`, `later`.
            fn redraws(badge: &'static str, later: &str) -> Self {
                Self {
                    badge,
                    after_scan: format!("{REDRAW} '{later}'"),
                    ..Self::normal()
                }
            }

            fn script(&self) -> String {
                "#!/bin/sh
stty -icanon -echo min 1 time 0 2>/dev/null
emit() { printf '%s\\n' \"$1\" >> \"$EXCISE_TEST_EVENTS\"; }
key() { dd bs=1 count=1 2>/dev/null; }
@BEFORE@
printf ' EXCISE  /stub  %s\\n' '@BADGE@'
emit '{\"v\":1,\"kind\":\"hello\",\"version\":\"stub\",\"pid\":'\"$$\"',\"t_us\":1}'
emit '{\"v\":1,\"kind\":\"scan_complete\",\"entries\":@ENTRIES@,\"t_us\":2}'
@AFTER@
if [ \"$(key)\" = q ]; then
  emit '{\"v\":1,\"kind\":\"quit_prompt\",\"t_us\":3}'
  if [ \"$(key)\" = y ]; then
    @CONFIRM@
    emit '{\"v\":1,\"kind\":\"exit\",\"code\":0,\"t_us\":4}'
    exit 0
  fi
fi
exit 1
"
                .replace("@BEFORE@", self.before_scan)
                .replace("@BADGE@", self.badge)
                .replace("@ENTRIES@", &self.entries.to_string())
                .replace("@AFTER@", &self.after_scan)
                .replace("@CONFIRM@", self.on_confirm)
            }
        }

        /// A stand-in program in a directory of its own, with a fixture it owns.
        struct Setup {
            /// Removes everything when dropped.
            _dir: tempfile::TempDir,
            program: PathBuf,
            root: FixtureRoot,
            work: PathBuf,
        }

        impl Setup {
            fn new(stub: &Stub) -> Self {
                let dir = tempfile::tempdir().expect("a directory");
                let program = dir.path().join("excise");
                fs::write(&program, stub.script()).expect("a stub program");
                fs::set_permissions(&program, fs::Permissions::from_mode(0o755))
                    .expect("executable");
                let root = dir.path().join("fixture");
                fs::create_dir(&root).expect("a fixture directory");
                fs::write(root.join(MARKER_FILE_NAME), b"owned").expect("the marker");
                let root = FixtureRoot::open(&root).expect("an owned fixture");
                let work = dir.path().join("work");
                fs::create_dir(&work).expect("a work directory");
                Self {
                    _dir: dir,
                    program,
                    root,
                    work,
                }
            }

            /// The stand-in started as the real program is, and the scratch area that holds its
            /// event channel.
            fn started(&self) -> (Scratch, PtySession) {
                let scratch = Scratch::create(&self.work).expect("a scratch area");
                let session = start(&self.program, &self.root, &scratch).expect("the stub starts");
                (scratch, session)
            }
        }

        /// Runs `session_residue` on `stub` and says how long it took.
        fn run(stub: &Stub, timeout: Duration) -> (Result<usize, CountsError>, Duration) {
            let setup = Setup::new(stub);
            let began = Instant::now();
            let result = session_residue(
                "stub",
                &setup.program,
                &setup.root,
                &setup.work,
                timeout,
                EXPECTED_ENTRIES,
            );
            (result, began.elapsed())
        }

        #[test]
        fn a_program_that_is_exact_is_counted() {
            let (result, _) = run(&Stub::normal(), GENEROUS);

            assert_eq!(result.expect("an exact scan is counted"), 0);
        }

        /// The event comes before the frame that shows it, so the header reads `SCANNING` for a
        /// while after `scan_complete`: that is not how the scan ended.
        #[test]
        fn a_header_that_shows_how_the_scan_ended_late_is_waited_for() {
            let (exact, _) = run(&Stub::redraws("~ SCANNING", "C COMPLETE"), GENEROUS);
            let (inexact, _) = run(&Stub::redraws("~ SCANNING", "? READ ERROR"), GENEROUS);

            assert_eq!(
                exact.expect("it ended exact, and says so once it is drawn"),
                0
            );
            let error = inexact
                .expect_err("it did not end exact, and says so once it is drawn")
                .to_string();
            assert!(error.contains("`READ ERROR`"), "{error}");
        }

        #[test]
        fn a_scan_that_did_not_end_exact_is_not_counted() {
            let unreadable = Stub {
                badge: "? READ ERROR",
                ..Stub::normal()
            };

            let (result, _) = run(&unreadable, GENEROUS);

            let error = result
                .expect_err("an inexact scan is not counted")
                .to_string();
            assert!(error.contains("`READ ERROR`"), "{error}");
            assert!(error.contains("did not end exact"), "{error}");
        }

        #[test]
        fn a_scan_that_counted_other_entries_than_the_headless_scan_is_not_counted() {
            let lost_entries = Stub {
                entries: EXPECTED_ENTRIES - 1,
                ..Stub::normal()
            };

            let (result, _) = run(&lost_entries, GENEROUS);

            let error = result
                .expect_err("a different scan is not counted")
                .to_string();
            assert!(
                error.contains(
                    "counted 4 entries where the headless scan of the same fixture counted 5"
                ),
                "{error}"
            );
        }

        /// The timeout is the session's, whichever step the time runs out in. A scan that
        /// completes late leaves the header only what is left of it.
        #[test]
        fn waiting_for_the_header_is_bounded_by_the_sessions_timeout_not_by_a_fresh_allowance() {
            let timeout = Duration::from_secs(6);
            let never_draws_it = Stub {
                before_scan: "sleep 2.5",
                badge: "~ SCANNING",
                ..Stub::normal()
            };

            let (result, took) = run(&never_draws_it, timeout);

            let error = result
                .expect_err("a header that never says how the scan ended is an error")
                .to_string();
            assert!(
                error
                    .contains("gave up waiting for the header to show how the scan ended after 6s"),
                "{error}"
            );
            assert!(
                took < timeout + ms(1_300),
                "the run ended after {took:?}; its timeout was {timeout:?}"
            );
        }

        #[test]
        fn a_header_that_never_shows_how_the_scan_ended_is_an_error_that_names_what_it_reads() {
            let setup = Setup::new(&Stub {
                badge: "~ SCANNING",
                ..Stub::normal()
            });
            let (scratch, session) = setup.started();
            let mut probe = Probe::new("stub", session, EventLog::new(scratch.events()), GENEROUS);
            probe
                .wait_for(scan_complete, "the scan to complete")
                .expect("the scan completes");

            let error = probe
                .require_an_exact_scan(ms(1_000))
                .expect_err("it is not known whether the scan ended exact")
                .to_string();

            assert!(
                error.contains("the header still reads `SCANNING` 1s after the scan completed"),
                "{error}"
            );
        }

        #[test]
        fn quitting_is_bounded_by_the_sessions_timeout_not_by_a_fresh_allowance() {
            let timeout = Duration::from_secs(6);
            let hangs = Stub {
                on_confirm: "sleep 120",
                ..Stub::normal()
            };

            let (result, took) = run(&hangs, timeout);

            let error = result
                .expect_err("a program that never quits is an error")
                .to_string();
            assert!(
                error.contains("gave up waiting for the quit after 6s"),
                "{error}"
            );
            assert!(
                took < timeout + ms(1_500),
                "the run ended after {took:?}; its timeout was {timeout:?}"
            );
        }

        #[test]
        fn a_quit_that_outlasts_its_own_limit_is_an_error_that_names_the_limit() {
            let setup = Setup::new(&Stub {
                on_confirm: "sleep 120",
                ..Stub::normal()
            });
            let (scratch, session) = setup.started();
            let mut probe = Probe::new("stub", session, EventLog::new(scratch.events()), GENEROUS);
            probe
                .wait_for(scan_complete, "the scan to complete")
                .expect("the scan completes");
            probe.session.send(b"q").expect("q is sent");
            probe
                .wait_for(quit_prompt, "the quit dialog")
                .expect("the dialog opens");
            probe.session.send(b"y").expect("y is sent");

            let began = Instant::now();
            let error = probe
                .await_exit(ms(1_000))
                .expect_err("the program does not quit")
                .to_string();

            assert!(
                error.contains("had not quit 1s after the quit was confirmed"),
                "{error}"
            );
            assert!(began.elapsed() < ms(3_000), "{:?}", began.elapsed());
        }

        /// A parent that is slow to look at a program it has started (a spawn that stalls, a
        /// process that is not scheduled) must not be given the time back: the timeout is
        /// counted from the start that the session recorded before it launched the program.
        #[test]
        fn the_deadline_is_counted_from_the_start_of_the_session_not_from_the_first_look() {
            let setup = Setup::new(&Stub::normal());
            let (scratch, session) = setup.started();
            let started = session.started();
            let timeout = Duration::from_secs(2);
            thread::sleep(ms(1_200));

            let probe = Probe::new("stub", session, EventLog::new(scratch.events()), timeout);

            assert_eq!(probe.deadline, started + timeout);
            assert!(
                probe.deadline.saturating_duration_since(Instant::now()) <= ms(900),
                "only what is left of the timeout remains"
            );
        }

        /// What the harness first reads after the deadline was not necessarily done by it: the
        /// quit dialog opened at once, but a parent that was not scheduled reads it late.
        #[test]
        fn an_event_first_read_after_the_deadline_is_not_progress() {
            let setup = Setup::new(&Stub::normal());
            let (scratch, session) = setup.started();
            let mut probe = Probe::new(
                "stub",
                session,
                EventLog::new(scratch.events()),
                Duration::from_secs(4),
            );
            probe
                .wait_for(scan_complete, "the scan to complete")
                .expect("the scan completes in time");
            probe.session.send(b"q").expect("q is sent");
            // Not looked at until after the deadline.
            thread::sleep(probe.deadline.saturating_duration_since(Instant::now()) + ms(300));

            let error = probe
                .wait_for(quit_prompt, "the quit dialog")
                .expect_err("the dialog was first read after the deadline");

            assert!(
                matches!(
                    error,
                    CountsError::TimedOut {
                        what: "the quit dialog",
                        ..
                    }
                ),
                "{error:?}"
            );
        }

        /// The same for an exit: the program quit at once, but the session first saw it after
        /// the deadline, and `await_exit` finds the session already finished.
        #[test]
        fn an_exit_first_seen_after_the_deadline_is_not_a_quit_in_time() {
            let setup = Setup::new(&Stub::normal());
            let (scratch, session) = setup.started();
            let mut probe = Probe::new(
                "stub",
                session,
                EventLog::new(scratch.events()),
                Duration::from_secs(4),
            );
            probe
                .wait_for(scan_complete, "the scan to complete")
                .expect("the scan completes in time");
            probe.session.send(b"q").expect("q is sent");
            probe.session.send(b"y").expect("y is sent");
            // Not looked at until after the deadline.
            thread::sleep(probe.deadline.saturating_duration_since(Instant::now()) + ms(300));
            for _ in 0..200 {
                probe.session.pump().expect("the session is read");
                if probe.session.finished() {
                    break;
                }
                thread::sleep(ms(10));
            }
            assert!(
                probe.session.finished(),
                "the stand-in has quit, and the session has seen it"
            );

            let error = probe
                .await_exit(GENEROUS)
                .expect_err("the exit was first seen after the deadline");

            assert!(
                matches!(
                    error,
                    CountsError::TimedOut {
                        what: "the quit",
                        ..
                    }
                ),
                "{error:?}"
            );
        }
    }
}
