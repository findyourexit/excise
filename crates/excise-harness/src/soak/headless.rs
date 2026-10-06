//! Headless scans of the root: `excise --format json --output <scratch>/scan-report.json <root>`.
//!
//! The scan runs under the existing process supervision (`headless::process`: its own process
//! group, bounded by a deadline, and measured for wall time, CPU time, and peak memory) and the
//! isolation of every run (`safety::isolated_env`: a scratch `HOME`, configuration, working
//! directory, temporary directory, and scan store, all outside the root). The report is written to
//! the scratch area, read for the three things the soak keeps (`state`, `accounting`, and `summary`,
//! by [`facts::read_facts`]), and deleted with the scratch area. It is a large file on a real tree:
//! the soak needs room for it in the system's temporary directory. A tree of very deep folders
//! makes it grow with the square of the depth (every entry carries its whole path, twice), so its
//! size is watched while the scan runs and looked at once more when it has ended, and above
//! [`Limits::report_bytes`](super::Limits) (or a quarter of the free space where the scratch area
//! is, when that is less) the scan's process group is killed, or a report that passed it between
//! two looks is not read, and the soak notes it ([`QuirkKind::ReportTooLarge`]). The thread that
//! looks is the soak's own and is not waited for: a file system that has stopped answering cannot
//! hold the run past its bound with it, and a size that could not be had in time leaves the report
//! unread ([`QuirkKind::ReportUnreadable`]).
//!
//! Once the scan has ended, what it left in the scratch area besides the report is looked at,
//! under a bound ([`Scratch::visit_residue`]): a program that left a vast hierarchy behind keeps
//! neither the run from ending nor the memory of the soak. The report's own entry is expected
//! whatever it is and is never entered; what a program left there is the report reader's to say.

use std::{ffi::OsString, fmt::Write as _, process::Command, time::Instant};
use std::{
    fs,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use crate::{
    headless::{
        document::ScanState,
        process::{self, Ended, Finished},
    },
    report::QuirkKind,
    safety::{Scratch, ScratchError, available_bytes, isolated_env},
    scenario::Profile,
};

use super::{
    Context, Interrupt, SoakError,
    facts::{self, Cancelled, FactsError, ScanFacts},
    quirks::QuirkLog,
    session::End,
};

/// One headless scan, after it ended.
#[derive(Debug)]
pub(super) struct HeadlessRun {
    pub round: u32,
    pub profile: Profile,
    pub finished: Finished,
    /// What the scan left in its scratch area besides its report, as far as the look at it went.
    /// Its names are local.
    pub residue: ScanResidue,
    /// What the report said, when it could be read.
    pub facts: Option<ScanFacts>,
    pub end: End,
    pub quirks: QuirkLog,
}

/// How many entries of the scratch area of a scan are read once the scan has ended. A scan leaves
/// next to nothing, so this is far more than any residue that matters and little enough to take
/// no time: what is left beyond it is counted as at least this many.
const RESIDUE_ENTRY_LIMIT: usize = 10_000;

/// How many of the paths found are kept, to be named in the log.
const RESIDUE_NAMES: usize = 20;

/// Why the look at what a scan left was cut short.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ResidueCut {
    /// It had read as many entries as it may.
    EntryLimit,
    /// The bound on the run passed or the person interrupted it.
    Stopped,
}

/// What a scan left in its scratch area besides its report, as far as the look at it went. The
/// look is bounded ([`Scratch::visit_residue`]): a program that left a vast hierarchy behind keeps
/// neither the run from ending nor the memory of the soak, and a look that was cut short says so.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct ScanResidue {
    /// The paths, relative to the scratch area, of the first entries found: at most
    /// `RESIDUE_NAMES`. They can name things in the tree, so they are local.
    pub names: Vec<String>,
    /// How many entries were found: all of them when the look was not cut short, and at least
    /// this many when it was.
    pub found: usize,
    /// Why the look ended before the end of the scratch area, when it did.
    pub cut: Option<ResidueCut>,
}

impl ScanResidue {
    /// How many entries were found, which is what the document counts as `residue_files`: a lower
    /// bound when the look was cut short.
    pub(super) const fn len(&self) -> usize {
        self.found
    }

    /// Whether nothing was found.
    pub(super) const fn is_empty(&self) -> bool {
        self.found == 0
    }

    /// The line of the quirk log for what `who` (the scan, or the program of a session) left: what
    /// was found, with the first paths, and when the look was cut short, that the count is a lower
    /// bound and why.
    pub(super) fn describe_for(&self, who: &str) -> String {
        let more = self.found.saturating_sub(self.names.len());
        let more = if more > 0 {
            format!(", and {more} more")
        } else {
            String::new()
        };
        let names = self.names.join(", ");
        match self.cut {
            None => format!(
                "the {who} left {} entries in its scratch area: {names}{more}",
                self.found
            ),
            Some(ResidueCut::EntryLimit) => format!(
                "the {who} left at least {} entries in its scratch area (the look at it was cut \
                 short at {RESIDUE_ENTRY_LIMIT} entries): {names}{more}",
                self.found
            ),
            Some(ResidueCut::Stopped) => format!(
                "the {who} left at least {} entries in its scratch area (the look at it was cut \
                 short because the run was stopped): {names}{more}",
                self.found
            ),
        }
    }
}

/// Runs one headless scan of the root.
///
/// The scan is bounded by [`Limits::headless_scan`](super::Limits) and by the end of the run, and
/// its whole process group is killed when either passes and when the person interrupts.
///
/// # Errors
///
/// Returns [`SoakError`] when the scratch area cannot be made or the process cannot be run, and
/// [`SoakError::NotStarted`] when the person asked the soak to stop in the moment before the scan
/// would have started (nothing was started). A scan that fails, times out, or writes nothing is a
/// successful call: what it did is in the [`HeadlessRun`].
pub(super) fn run_scan(
    context: &Context<'_>,
    round: u32,
    profile: Profile,
) -> Result<HeadlessRun, SoakError> {
    scan_watching(context, round, profile, &Watching::default())
}

/// [`run_scan`] with the way the report is watched given, so that a test can stand in for a file
/// system that does not answer.
fn scan_watching(
    context: &Context<'_>,
    round: u32,
    profile: Profile,
    watching: &Watching,
) -> Result<HeadlessRun, SoakError> {
    let scope = format!("round {round}, headless {profile}");
    let scratch = Scratch::create(context.work_dir)?;
    let (finished, growth) = supervise(context, &scratch, profile, watching)?;
    let residue = look_at_residue(context.run_deadline, context.interrupt, &scratch)?;

    let interrupted = context.interrupt.is_set();
    let mut end = if interrupted {
        End::Interrupted
    } else if finished.timed_out && Instant::now() >= context.run_deadline {
        End::RunBound
    } else {
        End::Session
    };
    // The scan was ended by the soak: its exit says nothing. A report that the soak does not read
    // is a different matter: the scan can have ended by itself and be answerable for its exit.
    let killed = finished.timed_out || interrupted || growth.passed;
    let report_left_unread = killed || growth.passed_at_the_end || !growth.looked;
    let mut quirks = QuirkLog::new();
    if finished.timed_out && end == End::Session {
        quirks.note(
            QuirkKind::Timeout,
            &scope,
            format!(
                "the scan passed its bound of {} s; the process group was killed",
                context.limits.headless_scan.as_secs()
            ),
        );
    }
    if growth.passed {
        quirks.note(
            QuirkKind::ReportTooLarge,
            &scope,
            format!(
                "the scan's report passed {} bytes in its scratch area, so the scan's process \
                 group was killed",
                growth.cap
            ),
        );
    } else if growth.passed_at_the_end {
        quirks.note(
            QuirkKind::ReportTooLarge,
            &scope,
            format!(
                "the scan's report was past {} bytes in its scratch area when the scan ended, \
                 so it was not read",
                growth.cap
            ),
        );
    }
    if !growth.looked {
        quirks.note(
            QuirkKind::ReportUnreadable,
            &scope,
            format!(
                "the size of the scan's report could not be read when the scan ended: the file \
                 system of the scratch area did not answer within {:?}, so the report was not read",
                watching.last_look_within
            ),
        );
    }
    if !killed {
        note_exit(&mut quirks, &scope, finished.ended);
    }
    note_output(&mut quirks, &scope, &finished, killed);
    if !residue.is_empty() {
        quirks.note(QuirkKind::Residue, &scope, residue.describe_for("scan"));
    }
    let facts = if report_left_unread {
        None
    } else {
        match read_report(context, &mut quirks, &scope, &scratch) {
            Ok(facts) => facts,
            Err(cancelled) => {
                // The scan ended, and the run was stopped while its report was being read: the
                // report goes with the scratch area, below, and the run ends as the stop says.
                end = end_of_a_stopped_read(cancelled);
                None
            }
        }
    };
    // The scratch area, and the report in it, go here.
    drop(scratch);
    Ok(HeadlessRun {
        round,
        profile,
        finished,
        residue,
        facts,
        end,
        quirks,
    })
}

/// Looks at what a program left in its scratch area, once it has ended: at most
/// `RESIDUE_ENTRY_LIMIT` entries, keeping the first `RESIDUE_NAMES` paths, and no longer than the
/// run goes on (`run_deadline`) and the person lets it (`interrupt`). The report's own entry is
/// not residue and is not entered, whatever the scan left there: reading it says what is wrong
/// with it. It serves the headless scan and the session in a terminal alike.
///
/// An area that cannot be read is an error and not residue.
pub(super) fn look_at_residue(
    run_deadline: Instant,
    interrupt: &super::Interrupt,
    scratch: &Scratch,
) -> Result<ScanResidue, ScratchError> {
    let mut residue = ScanResidue::default();
    let mut stopped = false;
    let complete = scratch.visit_residue(
        RESIDUE_ENTRY_LIMIT,
        &mut || {
            stopped = interrupt.is_set() || Instant::now() >= run_deadline;
            stopped
        },
        &mut |path| {
            residue.found += 1;
            if residue.names.len() < RESIDUE_NAMES {
                residue.names.push(path.to_owned());
            }
        },
    )?;
    if !complete {
        residue.cut = Some(if stopped {
            ResidueCut::Stopped
        } else {
            ResidueCut::EntryLimit
        });
    }
    Ok(residue)
}

/// How the scan's report grew while the scan ran, and what it was when the scan had ended.
#[derive(Debug, Clone, Copy)]
struct Growth {
    /// The most the report was allowed in the scratch area.
    cap: u64,
    /// Whether it passed the cap while the scan ran, and the scan was killed for it.
    passed: bool,
    /// Whether it was past the cap when the scan had ended, which no look while the scan ran had
    /// seen: the scan wrote the rest between two looks, or before the first.
    passed_at_the_end: bool,
    /// Whether the last look at the size of the report, made once the scan had ended, was made. It
    /// is not when the file system of the scratch area did not answer in time: the size is then
    /// not known.
    looked: bool,
}

/// How often the size of the report is looked at, and the person's interrupt is passed on to the
/// scan.
const WATCH_EVERY: Duration = Duration::from_millis(25);

/// How long the supervisor of a scan waits, once the scan has ended, for the last look at the size
/// of its report. The look is one `lstat`: a thread that has not made it by then is stuck in the
/// file system (a mount that has stopped answering) and is left behind.
const LAST_LOOK_WITHIN: Duration = Duration::from_secs(2);

/// How the report of a scan is watched. A run uses [`Watching::default`]. A test gives a measure
/// that does not return, or looks that come less often than a scan lasts.
#[derive(Clone)]
struct Watching {
    /// The size of the report at a path ([`report_size`]).
    size: Arc<dyn Fn(&Path) -> u64 + Send + Sync>,
    /// How often the report is looked at while the scan runs.
    every: Duration,
    /// How long the supervisor waits for the last look, once the scan has ended.
    last_look_within: Duration,
}

impl Default for Watching {
    fn default() -> Self {
        Self {
            size: Arc::new(report_size),
            every: WATCH_EVERY,
            last_look_within: LAST_LOOK_WITHIN,
        }
    }
}

/// What the supervisor of a scan and the thread that watches its report tell each other. Each
/// holds a share, and the thread keeps its own for as long as it lives, which can be longer than
/// the supervisor when the file system does not let it go.
#[derive(Default)]
struct Watch {
    /// The scan is to be ended: the person interrupted the run, or the report passed the cap.
    /// What `process::run_cancellable` watches.
    stop: AtomicBool,
    /// The report passed the cap while the scan ran.
    passed: AtomicBool,
    /// The scan has ended: the thread is to make its last look and go.
    over: AtomicBool,
    /// The report was past the cap at the last look, and had not been before.
    passed_at_the_end: AtomicBool,
    /// The thread has made its last look.
    looked: AtomicBool,
}

/// What a report may grow to: `limit`, and no more than a quarter of what is free where the scratch
/// area is. The report is not all that a scan writes there (its scan store is kept to a share of
/// the free space by the program itself), and a disk that fills up is worse than a scan that is
/// cut short. Where the free space cannot be read, the limit is all there is.
fn report_cap(limit: u64, free: Option<u64>) -> u64 {
    free.map_or(limit, |free| limit.min(free / 4))
}

/// The size of the file at `report`: 0 for what is not a regular file, a link, a folder, a FIFO, or
/// nothing. A link is looked at and never followed.
fn report_size(report: &Path) -> u64 {
    fs::symlink_metadata(report)
        .ok()
        .filter(fs::Metadata::is_file)
        .map_or(0, |metadata| metadata.len())
}

/// The thread that watches the report of a scan, until the scan has ended. Every
/// `watching.every` it passes the person's interrupt on to the scan and looks at the size of the
/// report, and when the report is past `cap` it has the scan ended.
///
/// When the scan has ended it looks once more, and once only. A report that passed the cap
/// between two looks (the scan wrote the rest and ended, or ended before the first look) is a report
/// that passed it, and no look made while the scan ran would say so. The last look is not made
/// after an interrupt: the report is not read then, and the interrupt is why.
///
/// It owns everything it uses, and the supervisor does not wait for it past
/// [`Watching::last_look_within`]: a look at a file that does not answer (a mount that has hung)
/// cannot be woken, and a thread that is waited for would hold the run with it.
fn watch_report(
    watch: &Watch,
    report: &Path,
    cap: u64,
    interrupt: &Interrupt,
    watching: &Watching,
    supervisor: &thread::Thread,
) {
    while !watch.over.load(Ordering::SeqCst) {
        if interrupt.is_set() {
            watch.stop.store(true, Ordering::SeqCst);
        } else if (watching.size)(report) > cap {
            watch.passed.store(true, Ordering::SeqCst);
            watch.stop.store(true, Ordering::SeqCst);
        }
        thread::park_timeout(watching.every);
    }
    if !watch.passed.load(Ordering::SeqCst) && !interrupt.is_set() && (watching.size)(report) > cap
    {
        watch.passed_at_the_end.store(true, Ordering::SeqCst);
    }
    watch.looked.store(true, Ordering::SeqCst);
    supervisor.unpark();
}

/// Waits until the watcher has made its last look at the report, at most `within`, and says
/// whether it has.
fn wait_for_the_last_look(watch: &Watch, within: Duration) -> bool {
    let until = Instant::now() + within;
    while !watch.looked.load(Ordering::SeqCst) {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        thread::park_timeout(left);
    }
    true
}

/// Runs the scan to its end, or to the bound, or to the person's interrupt, or until its report
/// has grown past what it may.
///
/// The scan is run by `process::run_cancellable`, which kills its whole process group when the
/// flag it watches is set. A thread of its own (`watch_report`) sets that flag when the person has
/// interrupted the run and when the report is larger than the cap, and makes a last look at the
/// report when the scan has ended. The scan is a program that the soak has to end, which the
/// interrupt is told for as long as it runs ([`Interrupt::supervising`]), and no longer: the
/// guard is dropped as soon as the scan has been ended and waited for, or given up on, before the
/// last look at the report, which is file system work.
///
/// The thread is not joined: it is stopped, asked for its last look for no longer than
/// [`Watching::last_look_within`], and let go. A report on a file system that has stopped
/// answering would otherwise hold the soak here after the bound of the run or the interrupt has
/// ended the scan, and then the size of the report is not known: it is not read.
fn supervise(
    context: &Context<'_>,
    scratch: &Scratch,
    profile: Profile,
    watching: &Watching,
) -> Result<(Finished, Growth), SoakError> {
    let arguments: Vec<OsString> = vec![
        "--format".into(),
        "json".into(),
        "--output".into(),
        scratch.report().into_os_string(),
        context.root.path().as_os_str().to_owned(),
    ];
    let mut command = Command::new(context.binary);
    command
        .args(&arguments)
        .env_clear()
        .envs(isolated_env(scratch, profile, false, None))
        .current_dir(scratch.cwd());
    let remaining = context
        .run_deadline
        .saturating_duration_since(Instant::now());
    let cap = report_cap(context.limits.report_bytes, available_bytes(scratch.root()));
    // The scan is a program that the soak has to end. Refused when the person has already asked it
    // to stop: nothing starts after that.
    let supervision = context.interrupt.supervising()?;
    let watch = Arc::new(Watch::default());
    let watcher = {
        let watch = Arc::clone(&watch);
        let (report, interrupt, watching) = (
            scratch.report(),
            context.interrupt.clone(),
            watching.clone(),
        );
        let supervisor = thread::current();
        thread::Builder::new()
            .name("soak-report-watch".to_owned())
            .spawn(move || watch_report(&watch, &report, cap, &interrupt, &watching, &supervisor))
            .map_err(|source| SoakError::Io {
                context: "cannot start the thread that watches the report of the scan",
                source,
            })?
    };
    let finished = process::run_cancellable(
        &mut command,
        context.limits.headless_scan.min(remaining),
        true,
        false,
        &watch.stop,
    );
    // The scan has been ended and waited for, or given up on: it is no longer the soak's to end.
    // What follows is file system work (the last look at the report, the report, the scratch
    // area) that can block for as long as a file system takes to answer, and the exit is not
    // armed while a program is held.
    drop(supervision);
    watch.over.store(true, Ordering::SeqCst);
    watcher.thread().unpark();
    // A scan that could not be ended is the error of the call, and the size of its report no
    // longer matters: the watcher is let go without a wait.
    let finished = finished?;
    let looked = wait_for_the_last_look(&watch, watching.last_look_within);
    Ok((
        finished,
        Growth {
            cap,
            passed: watch.passed.load(Ordering::SeqCst),
            passed_at_the_end: watch.passed_at_the_end.load(Ordering::SeqCst),
            looked,
        },
    ))
}

/// How the run ends when its stop came while a report was being read.
const fn end_of_a_stopped_read(cancelled: Cancelled) -> End {
    match cancelled {
        Cancelled::Interrupted => End::Interrupted,
        Cancelled::TimedOut => End::RunBound,
    }
}

/// Reads what the report says, and notes what is odd about it or about its absence. The reading
/// is bounded by the end of the run and by the person's interrupt, and a stop of either is the
/// error.
fn read_report(
    context: &Context<'_>,
    quirks: &mut QuirkLog,
    scope: &str,
    scratch: &Scratch,
) -> Result<Option<ScanFacts>, Cancelled> {
    match facts::read_facts(&scratch.report(), context.run_deadline, context.interrupt) {
        Ok(read) => {
            note_uncertainty(quirks, scope, &read);
            Ok(Some(read))
        }
        Err(FactsError::Cancelled(cancelled)) => Err(cancelled),
        Err(error) => {
            quirks.note(QuirkKind::ReportUnreadable, scope, error.to_string());
            Ok(None)
        }
    }
}

/// Notes a scan that did not exit with 0. The codes are the ones of `docs/reports.md`.
fn note_exit(quirks: &mut QuirkLog, scope: &str, ended: Ended) {
    let meaning = match ended {
        Ended::Exited(0) => return,
        Ended::Exited(2) => " (the result is usable but uncertain)",
        Ended::Exited(3) => " (the result is partial: a summary only)",
        Ended::Exited(130) => " (the scan was cancelled)",
        Ended::Exited(_) | Ended::Signaled(_) => "",
    };
    quirks.note(
        QuirkKind::NonZeroExit,
        scope,
        format!("the scan {}{meaning}", ended.describe()),
    );
}

/// Notes what the scan wrote to its standard streams: nothing is expected on either, since the
/// report goes to a file.
fn note_output(quirks: &mut QuirkLog, scope: &str, finished: &Finished, cut_short: bool) {
    if finished.stdout.bytes > 0 {
        quirks.note(
            QuirkKind::Protocol,
            scope,
            format!(
                "the scan wrote {} bytes to standard output, although its report goes to a \
                 file: {}",
                finished.stdout.bytes,
                finished.stdout.first_line()
            ),
        );
    }
    if !cut_short && finished.stderr.bytes > 0 {
        quirks.note(
            QuirkKind::Protocol,
            scope,
            format!(
                "the scan wrote {} bytes to standard error: {}",
                finished.stderr.bytes,
                finished.stderr.first_line()
            ),
        );
    }
}

/// Notes what an uncertain report says: how many folders it could not read, and the text that
/// names the last of them, which stays in the local log.
///
/// The four counts are looked at one by one: the report is what the program under test wrote, and
/// four numbers that are each a count can add up to more than a count can hold.
fn note_uncertainty(quirks: &mut QuirkLog, scope: &str, read: &ScanFacts) {
    let summary = &read.facts.summary;
    let nothing_uncertain = summary.unreadable_entries == 0
        && summary.unscanned_entries == 0
        && summary.excluded_entries == 0
        && summary.filesystem_boundaries == 0;
    if read.facts.state == ScanState::Exact && nothing_uncertain {
        return;
    }
    let mut detail = format!(
        "the report is {}: {} unreadable, {} unscanned, {} excluded, {} file system boundaries",
        read.facts.state,
        summary.unreadable_entries,
        summary.unscanned_entries,
        summary.excluded_entries,
        summary.filesystem_boundaries
    );
    for (what, text) in [
        ("last unreadable", &read.text.unreadable_path),
        ("last unscanned", &read.text.unscanned_path),
        ("reason", &read.text.unscanned_reason),
        ("worker error", &read.text.worker_error),
    ] {
        if let Some(text) = text {
            let _ = write!(detail, "; {what}: {text}");
        }
    }
    quirks.note(QuirkKind::UncertainScan, scope, detail);
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::Path,
        time::{Duration, Instant},
    };

    use super::*;
    use crate::soak::{Interrupt, Limits, SoakRoot};

    /// Reads a report in a scratch area, with a run that ends at `run_deadline` and an interrupt
    /// that is `interrupted`. Returns what the read said, the quirks it noted, and whether the
    /// report was gone once the scratch area had been dropped.
    fn read_in_a_scratch_area(
        report: &str,
        run_deadline: Instant,
        interrupted: bool,
    ) -> (Result<Option<ScanFacts>, Cancelled>, QuirkLog, bool) {
        let tree = tempfile::tempdir().expect("a tree");
        let work = tempfile::tempdir().expect("a work directory");
        let root = SoakRoot::open(tree.path()).expect("a root");
        let limits = Limits::default();
        let interrupt = Interrupt::new();
        if interrupted {
            interrupt.trigger();
        }
        let context = Context {
            root: &root,
            binary: Path::new("/bin/sh"),
            work_dir: work.path(),
            limits: &limits,
            run_deadline,
            interrupt: &interrupt,
            record: false,
        };
        let scratch = Scratch::create(work.path()).expect("a scratch area");
        let path = scratch.report();
        fs::write(&path, report).expect("a report");
        let mut quirks = QuirkLog::new();

        let read = read_report(&context, &mut quirks, "round 1, headless default", &scratch);

        // What `run_scan` does with the scratch area once it has what it needs.
        drop(scratch);
        (read, quirks, !path.exists())
    }

    #[test]
    fn a_report_read_after_the_bound_of_the_run_ends_the_run_at_its_bound_and_is_still_removed() {
        let passed = Instant::now()
            .checked_sub(Duration::from_secs(1))
            .unwrap_or_else(Instant::now);

        let (read, quirks, removed) = read_in_a_scratch_area("{}", passed, false);

        let cancelled = read.expect_err("the bound had passed");
        assert_eq!(cancelled, Cancelled::TimedOut);
        assert_eq!(end_of_a_stopped_read(cancelled), End::RunBound);
        assert!(
            !quirks.has(QuirkKind::ReportUnreadable),
            "a read that was stopped is not a report that cannot be read"
        );
        assert!(removed, "the report goes with the scratch area");
    }

    #[test]
    fn a_report_read_after_the_person_interrupted_ends_the_run_as_interrupted_and_is_still_removed()
    {
        let later = Instant::now() + Duration::from_secs(3600);

        let (read, quirks, removed) = read_in_a_scratch_area("{}", later, true);

        let cancelled = read.expect_err("the person had interrupted");
        assert_eq!(cancelled, Cancelled::Interrupted);
        assert_eq!(end_of_a_stopped_read(cancelled), End::Interrupted);
        assert!(!quirks.has(QuirkKind::ReportUnreadable));
        assert!(removed, "the report goes with the scratch area");
    }

    #[test]
    fn a_report_that_cannot_be_read_is_a_quirk_and_not_a_stop() {
        let later = Instant::now() + Duration::from_secs(3600);

        let (read, quirks, removed) = read_in_a_scratch_area("not a report", later, false);

        assert!(matches!(read, Ok(None)), "{read:?}");
        assert!(quirks.has(QuirkKind::ReportUnreadable));
        assert!(removed);
    }

    /// What a program under test leaves when it makes a FIFO at its `--output` path and exits:
    /// opening one for reading waits for a writer that never comes, and neither the bound of the
    /// run nor the interrupt could end that wait. The report is opened without waiting, and
    /// anything but a regular file is a report that cannot be read. The read has ten seconds on a
    /// thread of its own, so that a wait fails this test and does not keep it from ever ending.
    #[cfg(unix)]
    #[test]
    fn a_report_that_is_a_fifo_is_a_quirk_and_is_not_waited_for() {
        use std::{sync::mpsc, thread};

        use nix::{sys::stat::Mode, unistd::mkfifo};

        let tree = tempfile::tempdir().expect("a tree");
        let work = tempfile::tempdir().expect("a work directory");
        let root = SoakRoot::open(tree.path()).expect("a root");
        let limits = Limits::default();
        let interrupt = Interrupt::new();
        let context = Context {
            root: &root,
            binary: Path::new("/bin/sh"),
            work_dir: work.path(),
            limits: &limits,
            run_deadline: Instant::now() + Duration::from_secs(3600),
            interrupt: &interrupt,
            record: false,
        };
        let scratch = Scratch::create(work.path()).expect("a scratch area");
        let path = scratch.report();
        mkfifo(&path, Mode::S_IRUSR | Mode::S_IWUSR).expect("a FIFO");
        let mut quirks = QuirkLog::new();
        let (sender, receiver) = mpsc::channel();

        let read = thread::scope(|scope| {
            let reader = scope.spawn(|| {
                let read =
                    read_report(&context, &mut quirks, "round 1, headless default", &scratch);
                let _ = sender.send(());
                read
            });
            if receiver.recv_timeout(Duration::from_secs(10)).is_err() {
                // The read is waiting for a writer. Give it one, so that it goes on and the
                // scope can end, and fail.
                let writer = fs::OpenOptions::new().read(true).write(true).open(&path);
                thread::sleep(Duration::from_millis(200));
                drop(writer);
                panic!("reading a report that is a FIFO waited for a writer");
            }
            reader.join().expect("the read ended")
        });

        assert!(matches!(read, Ok(None)), "{read:?}");
        assert!(quirks.has(QuirkKind::ReportUnreadable));
        let text = quirks.render("20261006T120000Z-4242");
        assert!(text.contains("not a regular file"), "{text}");
        // What `run_scan` does with the scratch area once it has what it needs: the FIFO goes too.
        drop(scratch);
        assert!(!path.exists(), "the FIFO goes with the scratch area");
    }

    /// A report of the version the soak reads, whose `summary` has the four counts that say how
    /// uncertain it is.
    fn report_with_counts(
        state: &str,
        unreadable: u64,
        unscanned: u64,
        excluded: u64,
        boundaries: u64,
    ) -> String {
        format!(
            r#"{{"document_kind": "scan-report", "schema_version": {version},
            "root": {{"unix": "/a"}}, "display_root": "/a", "state": "{state}",
            "accounting": {{"headline": "identity-unique-allocated-bytes",
                "hard_links_deduplicated": true, "shared_extents_deduplicated": false,
                "directory_metadata_included": false}},
            "summary": {{
                "scanned_entries": 12, "identified_entries": 11,
                "unreadable_entries": {unreadable}, "unscanned_entries": {unscanned},
                "excluded_entries": {excluded}, "filesystem_boundaries": {boundaries},
                "link_entries": 4, "deleted_entries": 0, "deletion_changed_entries": 0,
                "deletion_missing_entries": 0, "deletion_failed_entries": 0,
                "deletion_unattempted_entries": 0, "scan_store_bytes": 4096,
                "scan_store_limit_bytes": 1048576, "last_unreadable_path": null,
                "last_unscanned_path": null, "last_unscanned_reason": null,
                "last_worker_error": null}},
            "entries": []}}"#,
            version = crate::headless::document::SCAN_REPORT_VERSION,
        )
    }

    #[test]
    fn counts_that_are_each_a_count_and_add_up_to_more_than_a_count_are_a_report_that_is_classified()
     {
        let later = Instant::now() + Duration::from_secs(3600);
        for (state, counts) in [
            ("uncertain", [u64::MAX, 1, 0, 0]),
            ("exact", [u64::MAX, 1, 0, 0]),
            ("uncertain", [u64::MAX; 4]),
            ("exact", [0, 0, 0, u64::MAX]),
        ] {
            let report = report_with_counts(state, counts[0], counts[1], counts[2], counts[3]);

            let (read, quirks, _) = read_in_a_scratch_area(&report, later, false);

            assert!(matches!(read, Ok(Some(_))), "{state} {counts:?}: {read:?}");
            assert!(quirks.has(QuirkKind::UncertainScan), "{state} {counts:?}");
            let text = quirks.render("20261006T120000Z-4242");
            assert!(
                text.contains(&format!(
                    "{} unreadable, {} unscanned, {} excluded, {} file system boundaries",
                    counts[0], counts[1], counts[2], counts[3]
                )),
                "{text}"
            );
        }
    }

    #[test]
    fn an_exact_report_with_nothing_uncertain_in_it_is_no_quirk() {
        let later = Instant::now() + Duration::from_secs(3600);

        let (read, quirks, _) =
            read_in_a_scratch_area(&report_with_counts("exact", 0, 0, 0, 0), later, false);

        assert!(matches!(read, Ok(Some(_))), "{read:?}");
        assert!(!quirks.has(QuirkKind::UncertainScan));
    }

    fn residue_of(names: &[&str], found: usize, cut: Option<ResidueCut>) -> ScanResidue {
        ScanResidue {
            names: names.iter().map(|name| (*name).to_owned()).collect(),
            found,
            cut,
        }
    }

    #[test]
    fn what_a_scan_left_is_counted_and_named_and_a_look_that_was_cut_short_says_so() {
        let whole = residue_of(&["cwd/a", "tmp/b"], 2, None);
        let more = residue_of(&["cwd/a"], 3, None);
        let limit = residue_of(&["cwd/a"], 9_994, Some(ResidueCut::EntryLimit));
        let stopped = residue_of(&["cwd/a"], 4, Some(ResidueCut::Stopped));

        assert_eq!(
            whole.describe_for("scan"),
            "the scan left 2 entries in its scratch area: cwd/a, tmp/b"
        );
        assert_eq!(
            more.describe_for("scan"),
            "the scan left 3 entries in its scratch area: cwd/a, and 2 more"
        );
        assert_eq!(
            limit.describe_for("scan"),
            "the scan left at least 9994 entries in its scratch area (the look at it was cut \
             short at 10000 entries): cwd/a, and 9993 more"
        );
        assert_eq!(
            stopped.describe_for("scan"),
            "the scan left at least 4 entries in its scratch area (the look at it was cut short \
             because the run was stopped): cwd/a, and 3 more"
        );
        assert_eq!(limit.len(), 9_994, "the document counts what was found");
        assert!(!limit.is_empty() && ScanResidue::default().is_empty());
    }

    /// An executable script that stands in for `excise`, in `dir`.
    #[cfg(unix)]
    fn script_in(dir: &Path, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt as _;

        let path = dir.join("excise");
        fs::write(&path, format!("#!/bin/sh\n{body}")).expect("a script");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("executable");
        path
    }

    /// Makes every directory below `dir` usable by its owner again, so that a directory that a
    /// scan locked can be removed.
    #[cfg(unix)]
    fn open_up(dir: &Path) {
        use std::os::unix::fs::PermissionsExt as _;

        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                open_up(&entry.path());
            }
        }
    }

    /// Opens every directory below the directory it holds up when it is dropped, so that a test
    /// that fails with a locked directory in the work directory does not leave it behind.
    #[cfg(unix)]
    struct OpenUpOnDrop(std::path::PathBuf);

    #[cfg(unix)]
    impl Drop for OpenUpOnDrop {
        fn drop(&mut self) {
            open_up(&self.0);
        }
    }

    /// One headless scan by the script `body`, in a tree and a work directory of their own, with
    /// the bound of the run at `run_deadline` and the interrupt that is given. `work` is the work
    /// directory, which the caller holds so as to see what is left in it.
    #[cfg(unix)]
    fn scan_by_script(
        body: &str,
        work: &Path,
        run_deadline: Instant,
        interrupt: &Interrupt,
    ) -> Result<HeadlessRun, SoakError> {
        scan_watching_by_script(
            body,
            work,
            run_deadline,
            interrupt,
            &Limits::default(),
            &Watching::default(),
        )
    }

    /// [`scan_by_script`] with the limits of the run, and the way the report is watched, given.
    #[cfg(unix)]
    fn scan_watching_by_script(
        body: &str,
        work: &Path,
        run_deadline: Instant,
        interrupt: &Interrupt,
        limits: &Limits,
        watching: &Watching,
    ) -> Result<HeadlessRun, SoakError> {
        let tree = tempfile::tempdir().expect("a tree");
        let root = SoakRoot::open(tree.path()).expect("a root");
        let binary_dir = tempfile::tempdir().expect("a directory for the program");
        let binary = script_in(binary_dir.path(), body);
        let context = Context {
            root: &root,
            binary: &binary,
            work_dir: work,
            limits,
            run_deadline,
            interrupt,
            record: false,
        };
        scan_watching(&context, 1, Profile::Default, watching)
    }

    #[cfg(unix)]
    #[test]
    fn a_scan_that_left_a_few_files_says_how_many_and_names_them() {
        let work = tempfile::tempdir().expect("a work directory");

        let run = scan_by_script(
            ": > c; : > a; : > b\nexit 0\n",
            work.path(),
            Instant::now() + Duration::from_secs(3600),
            &Interrupt::new(),
        )
        .expect("a run");

        assert_eq!(run.residue.found, 3);
        assert_eq!(run.residue.cut, None);
        assert_eq!(run.residue.names, ["cwd/a", "cwd/b", "cwd/c"]);
        let text = run.quirks.render("20261006T120000Z-4242");
        assert!(
            text.contains("the scan left 3 entries in its scratch area: cwd/a, cwd/b, cwd/c"),
            "{text}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_scan_that_left_more_than_the_look_reads_is_counted_as_at_least_that_many() {
        let work = tempfile::tempdir().expect("a work directory");
        let files = RESIDUE_ENTRY_LIMIT + 100;

        let run = scan_by_script(
            &format!(
                "i=0\nwhile [ \"$i\" -lt {files} ]; do : > \"f$i\"; i=$((i+1)); done\nexit 0\n"
            ),
            work.path(),
            Instant::now() + Duration::from_secs(3600),
            &Interrupt::new(),
        )
        .expect("a run");

        assert_eq!(run.residue.cut, Some(ResidueCut::EntryLimit));
        assert!(
            run.residue.found <= RESIDUE_ENTRY_LIMIT
                && run.residue.found > RESIDUE_ENTRY_LIMIT - 20,
            "{}",
            run.residue.found
        );
        assert_eq!(
            run.residue.names.len(),
            RESIDUE_NAMES,
            "only the first are kept"
        );
        assert_eq!(run.residue.len(), run.residue.found);
        let text = run.quirks.render("20261006T120000Z-4242");
        assert!(
            text.contains("the scan left at least")
                && text.contains("(the look at it was cut short at 10000 entries)"),
            "{text}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_interrupt_after_the_scan_was_killed_stops_the_look_at_its_scratch_area_at_once() {
        use std::thread;

        let work = tempfile::tempdir().expect("a work directory");
        let ready = work.path().join("ready");
        let interrupt = Interrupt::new();
        // The scan leaves a few hundred files and then waits to be killed.
        let body = format!(
            "i=0\nwhile [ \"$i\" -lt 300 ]; do : > \"f$i\"; i=$((i+1)); done\n\
             echo ready > '{}'\nexec /bin/sleep 120\n",
            ready.display()
        );

        let run = thread::scope(|scope| {
            scope.spawn(|| {
                let deadline = Instant::now() + Duration::from_secs(30);
                while !ready.exists() && Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(10));
                }
                interrupt.trigger();
            });
            scan_by_script(
                &body,
                work.path(),
                Instant::now() + Duration::from_secs(3600),
                &interrupt,
            )
        })
        .expect("a run");

        assert_eq!(run.end, End::Interrupted);
        assert!(
            ready.exists(),
            "the scan had left its files when it was killed"
        );
        assert_eq!(run.residue.found, 0, "{:?}", run.residue);
        assert_eq!(run.residue.cut, Some(ResidueCut::Stopped));
        assert!(
            !run.quirks.has(QuirkKind::Residue),
            "a look that was stopped before it found anything says nothing about residue"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_scan_that_leaves_a_folder_in_the_place_of_its_report_has_made_a_report_that_cannot_be_read()
     {
        let work = tempfile::tempdir().expect("a work directory");
        let _open_up = OpenUpOnDrop(work.path().to_path_buf());
        // The folder holds a file and a folder that its owner cannot read.
        let body = "if [ \"$1\" = --format ]; then\n\
                    mkdir -p \"$4/locked/inner\" && : > \"$4/plain\" && chmod 000 \"$4/locked\"\n\
                    fi\nexit 0\n";

        let run = scan_by_script(
            body,
            work.path(),
            Instant::now() + Duration::from_secs(3600),
            &Interrupt::new(),
        )
        .expect("a folder in the place of the report is not a harness error");
        open_up(work.path());

        assert!(run.residue.is_empty(), "{:?}", run.residue);
        assert!(run.facts.is_none());
        assert!(run.quirks.has(QuirkKind::ReportUnreadable));
        assert!(!run.quirks.has(QuirkKind::Residue));
        let text = run.quirks.render("20261006T120000Z-4242");
        assert!(text.contains("not a regular file"), "{text}");
    }

    #[test]
    fn a_report_may_grow_to_the_limit_and_to_no_more_than_a_quarter_of_the_free_space() {
        let gib = 1_u64 << 30;

        assert_eq!(report_cap(16 * gib, Some(1000 * gib)), 16 * gib);
        assert_eq!(report_cap(16 * gib, Some(40 * gib)), 10 * gib);
        assert_eq!(report_cap(16 * gib, Some(0)), 0);
        assert_eq!(report_cap(16 * gib, None), 16 * gib);
    }

    #[test]
    fn the_size_of_a_report_is_that_of_a_regular_file_and_nothing_else_has_one() {
        let dir = tempfile::tempdir().expect("a directory");
        let file = dir.path().join("report");
        fs::write(&file, vec![0_u8; 1234]).expect("a file");

        assert_eq!(report_size(&file), 1234);
        assert_eq!(report_size(&dir.path().join("not-there")), 0);
        assert_eq!(report_size(dir.path()), 0, "a folder has none");
        #[cfg(unix)]
        {
            let link = dir.path().join("link");
            std::os::unix::fs::symlink(&file, &link).expect("a link");
            assert_eq!(
                report_size(&link),
                0,
                "a link is looked at and never followed"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_report_that_passed_the_cap_between_two_looks_is_caught_when_the_scan_ends() {
        let work = tempfile::tempdir().expect("a work directory");
        let limits = Limits {
            report_bytes: 1024,
            ..Limits::default()
        };
        // The report is looked at once, when the watcher starts, and not again for an hour. The
        // scan writes two kilobytes after that look and ends at once, so that no look made while
        // it ran could have seen them.
        let watching = Watching {
            every: Duration::from_secs(3600),
            ..Watching::default()
        };
        let body = "if [ \"$1\" = --format ]; then\nsleep 0.3\nhead -c 2048 /dev/zero > \"$4\"\nfi\n\
                    exit 0\n";

        let run = scan_watching_by_script(
            body,
            work.path(),
            Instant::now() + Duration::from_secs(3600),
            &Interrupt::new(),
            &limits,
            &watching,
        )
        .expect("a run");

        let text = run.quirks.render("20261006T120000Z-4242");
        assert!(run.quirks.has(QuirkKind::ReportTooLarge), "{text}");
        assert!(
            text.contains(
                "the scan's report was past 1024 bytes in its scratch area when the scan ended, \
                 so it was not read"
            ),
            "{text}"
        );
        assert!(
            run.facts.is_none(),
            "a report that is too large is not read"
        );
        assert!(
            !run.quirks.has(QuirkKind::ReportUnreadable),
            "it was not read, and it was not tried: {text}"
        );
        assert!(!run.finished.timed_out, "the scan ended by itself");
        assert_eq!(run.end, End::Session);
    }

    #[cfg(unix)]
    #[test]
    fn a_report_within_the_cap_is_read_and_the_last_look_leaves_it_alone() {
        let work = tempfile::tempdir().expect("a work directory");
        let limits = Limits {
            report_bytes: 1 << 20,
            ..Limits::default()
        };
        let watching = Watching {
            every: Duration::from_secs(3600),
            ..Watching::default()
        };
        // Two bytes in a report that may have a megabyte: it is read. They are not a scan report,
        // which is what the reader says, so the quirk that is there shows that it was tried.
        let body = "if [ \"$1\" = --format ]; then\nsleep 0.2\nprintf '{}' > \"$4\"\nfi\nexit 0\n";

        let run = scan_watching_by_script(
            body,
            work.path(),
            Instant::now() + Duration::from_secs(3600),
            &Interrupt::new(),
            &limits,
            &watching,
        )
        .expect("a run");

        let text = run.quirks.render("20261006T120000Z-4242");
        assert!(!run.quirks.has(QuirkKind::ReportTooLarge), "{text}");
        assert!(
            run.quirks.has(QuirkKind::ReportUnreadable),
            "the report was read, and it is not a scan report: {text}"
        );
        assert!(
            !text.contains("when the scan ended") && !text.contains("did not answer"),
            "the size was looked at and was fine: {text}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_watcher_that_is_stuck_in_the_file_system_does_not_hold_the_run_past_its_bound() {
        use std::sync::mpsc;

        // The report is on a file system that does not answer: every look at its size blocks until
        // the test lets it go. A watcher that the supervisor waits for, as a scoped thread is when
        // its scope ends, would hold the run with it.
        let release = Arc::new(AtomicBool::new(false));
        let watching = Watching {
            size: Arc::new({
                let release = Arc::clone(&release);
                move |_| {
                    while !release.load(Ordering::SeqCst) {
                        thread::sleep(Duration::from_millis(10));
                    }
                    0
                }
            }),
            every: WATCH_EVERY,
            last_look_within: Duration::from_millis(300),
        };
        let (done, result) = mpsc::channel();
        thread::spawn(move || {
            let work = tempfile::tempdir().expect("a work directory");
            let started = Instant::now();
            let run = scan_watching_by_script(
                "exec /bin/sleep 120\n",
                work.path(),
                started + Duration::from_millis(400),
                &Interrupt::new(),
                &Limits::default(),
                &watching,
            );
            let _ = done.send((run, started.elapsed()));
        });

        let received = result.recv_timeout(Duration::from_secs(10));
        // Whatever happened, the stuck look is let go, so that no thread outlives the test.
        release.store(true, Ordering::SeqCst);
        let (run, elapsed) = received.expect("the run was held by a watcher that cannot return");

        let run = run.expect("a run: the scan is ended at the bound, and that is not an error");
        assert!(run.finished.timed_out, "the bound ended the scan");
        assert_eq!(run.end, End::RunBound);
        assert!(
            elapsed < Duration::from_secs(5),
            "the run took {elapsed:?} to end, where its bound was 400 ms"
        );
        let text = run.quirks.render("20261006T120000Z-4242");
        assert!(
            run.quirks.has(QuirkKind::ReportUnreadable)
                && text.contains("the file system of the scratch area did not answer within"),
            "a size that could not be read is said so: {text}"
        );
        assert!(
            run.facts.is_none(),
            "a report of an unknown size is not read"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_scan_is_a_program_the_soak_has_to_end_until_it_has_been_ended() {
        let work = tempfile::tempdir().expect("a work directory");
        let interrupt = Interrupt::new();
        let held_while_it_ran = AtomicBool::new(false);

        let run = thread::scope(|scope| {
            scope.spawn(|| {
                let give_up = Instant::now() + Duration::from_secs(5);
                while interrupt.supervised() == 0 && Instant::now() < give_up {
                    thread::sleep(Duration::from_millis(5));
                }
                held_while_it_ran.store(interrupt.supervised() == 1, Ordering::SeqCst);
                interrupt.trigger();
            });
            scan_by_script(
                "exec /bin/sleep 120\n",
                work.path(),
                Instant::now() + Duration::from_secs(3600),
                &interrupt,
            )
        })
        .expect("a run");

        assert!(
            held_while_it_ran.load(Ordering::SeqCst),
            "the soak held a program to end while the scan ran"
        );
        assert_eq!(run.end, End::Interrupted);
        assert_eq!(interrupt.supervised(), 0, "the scan has been ended");
        assert!(interrupt.is_acted_on(), "and the interrupt acted on");
    }

    #[cfg(unix)]
    #[test]
    fn the_scan_is_let_go_of_before_the_last_look_at_its_report() {
        use std::sync::{Mutex, PoisonError};

        // The last look at the size of the report is file system work: on a mount that has stopped
        // answering it blocks for as long as the supervisor waits for it, and the exit is not
        // armed while the scan is held. Every look says how many programs were held when it was
        // made.
        let work = tempfile::tempdir().expect("a work directory");
        let interrupt = Interrupt::new();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let watching = Watching {
            size: Arc::new({
                let (seen, interrupt) = (Arc::clone(&seen), interrupt.clone());
                move |_| {
                    seen.lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push(interrupt.supervised());
                    0
                }
            }),
            every: Duration::from_millis(20),
            last_look_within: Duration::from_secs(5),
        };

        scan_watching_by_script(
            "sleep 0.3\nexit 0\n",
            work.path(),
            Instant::now() + Duration::from_secs(3600),
            &interrupt,
            &Limits::default(),
            &watching,
        )
        .expect("a run");

        let seen = seen.lock().unwrap_or_else(PoisonError::into_inner).clone();
        assert!(
            seen.contains(&1),
            "the scan was held while it ran: {seen:?}"
        );
        assert_eq!(
            seen.last(),
            Some(&0),
            "the scan had been let go of when the last look was made: {seen:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_scan_is_not_started_once_the_person_has_asked_to_stop() {
        let work = tempfile::tempdir().expect("a work directory");
        let started = work.path().join("started");
        let interrupt = Interrupt::new();
        interrupt.trigger();

        let run = scan_by_script(
            &format!(": > '{}'\nexit 0\n", started.display()),
            work.path(),
            Instant::now() + Duration::from_secs(3600),
            &interrupt,
        );

        assert!(
            matches!(run, Err(SoakError::NotStarted(_))),
            "the interrupt came before the scan was started: {run:?}"
        );
        assert!(!started.exists(), "the scan was started");
        assert_eq!(interrupt.supervised(), 0);
        let left: Vec<_> = fs::read_dir(work.path())
            .expect("the work directory")
            .flatten()
            .map(|entry| entry.file_name())
            .collect();
        assert!(
            left.is_empty(),
            "the scratch area of a scan that was not started was removed: {left:?}"
        );
    }
}
