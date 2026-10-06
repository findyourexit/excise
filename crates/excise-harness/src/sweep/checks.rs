//! The checks that drive a live program, one version at a time: what a published release does when
//! the terminal closes, when it is killed and started again, when nothing happens for a while,
//! when a filter is applied, and how many descriptors it holds.
//!
//! Every check is screen-only (see [`super::probe`]), runs on a generated fixture that carries the
//! ownership marker, in the isolated environment and a scratch area of its own, and ends its program
//! before it returns. None of them deletes, and none sends a key that could ask for a deletion; to
//! prove it, each check compares the fixture with its state before the check and stops the whole
//! sweep if anything differs ([`Fatal`]).
//!
//! A check that cannot run on a version (the header never reads COMPLETE, the prompt does not
//! open) says why and is not a finding. The classification of what a check observed into a defect
//! is [`super::classify`]'s.

use std::{collections::BTreeMap, fmt::Write as _, path::Path, time::Duration};

use thiserror::Error;

use crate::{
    bench::cases::SharedFixture,
    fixture::Fixtures,
    pty::{
        ExitInfo,
        ui::{Inspector, filter_prompt, header_path, inspector},
    },
    report::CheckStatus,
    safety::{FixtureRoot, FixtureSnapshot, Scratch},
    scenario::{Profile, Signal},
};

use super::probe::{Key, Probe, ProbeError, ProbeSpec, Wait};

/// How long a program that was told to end gets to do so.
const END_BOUND: Duration = Duration::from_secs(15);
/// How long output must be quiet before the screen is read after a key, and the most such a read
/// waits: the bounded read that stands in for a frame mark.
const QUIET: Duration = Duration::from_millis(150);
const QUIET_LIMIT: Duration = Duration::from_secs(2);
/// How long a filter that is going to crash the program takes to do it: it ends on the Enter.
const CRASH_WINDOW: Duration = Duration::from_millis(1500);
/// How long, with nothing sent, a program has been at rest after COMPLETE when the idle window
/// starts: one cycle of the selected entry's sheen, 3.2 s.
pub(crate) const IDLE_AFTER: Duration = Duration::from_millis(3200);
/// How long the idle window lasts.
pub(crate) const IDLE_WINDOW: Duration = Duration::from_millis(5000);

/// How long the idle check waits after COMPLETE with nothing sent, and how long it then looks. The
/// defaults are the validation program's own; a test that only needs the check to run shortens
/// them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdleWindow {
    /// How long after COMPLETE the window starts.
    pub after: Duration,
    /// How long the window lasts.
    pub window: Duration,
}

impl Default for IdleWindow {
    fn default() -> Self {
        Self {
            after: IDLE_AFTER,
            window: IDLE_WINDOW,
        }
    }
}
/// The text the filter checks type, and the folder they open: the `delete-folder` fixture's
/// `victim/` holds a folder called `part00` and, in each of its other folders, one more, so the
/// text matches two and three levels below the root.
pub(crate) const FILTER_TEXT: &str = "part00";
/// The folder the filter check opens.
pub(crate) const FILTER_FOLDER: &str = "victim";
/// The most arrow presses spent looking for the folder to open.
const SELECT_ATTEMPTS: usize = 8;

/// The sweep must stop: a check changed a fixture, so a build ran that deletes or rewrites what it
/// was only to read.
#[derive(Debug, Error)]
#[error("{0}")]
pub struct Fatal(pub String);

/// What every check needs.
#[derive(Clone, Copy)]
pub(crate) struct Context<'a> {
    /// The `excise` under test.
    pub binary: &'a Path,
    /// Where fixtures come from.
    pub fixtures: &'a Fixtures,
    /// The directory scratch areas are made in.
    pub work_dir: &'a Path,
    /// How long a scan may take to reach COMPLETE before a check gives up on it.
    pub bound: Duration,
}

/// What one check recorded, whatever it came to: the row of the document and the file of evidence.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Recorded {
    /// The check's name.
    pub check: String,
    /// The fixture it ran on.
    pub fixture: Option<String>,
    /// The profile it ran under.
    pub profile: Option<Profile>,
    /// What came of it.
    pub status: CheckStatus,
    /// Why it did not run.
    pub reason: Option<String>,
    /// What it measured.
    pub metrics: BTreeMap<String, f64>,
    /// What it observed that is not a number.
    pub notes: Vec<String>,
    /// The text of the evidence file: the observations in full and the last screen.
    pub evidence: String,
}

impl Recorded {
    /// A check that ran, with nothing recorded of it yet.
    pub(crate) fn new(check: &str, fixture: Option<&str>, profile: Option<Profile>) -> Self {
        Self {
            check: check.to_owned(),
            fixture: fixture.map(str::to_owned),
            profile,
            status: CheckStatus::Ran,
            reason: None,
            metrics: BTreeMap::new(),
            notes: Vec::new(),
            evidence: String::new(),
        }
    }

    /// A check that did not run, and why.
    pub(crate) fn not_run(
        check: &str,
        fixture: Option<&str>,
        profile: Option<Profile>,
        reason: impl Into<String>,
    ) -> Self {
        let reason = reason.into();
        let mut recorded = Self::new(check, fixture, profile);
        recorded.status = CheckStatus::NotRun;
        recorded.evidence = format!("not run: {reason}\n");
        recorded.reason = Some(reason);
        recorded
    }

    /// A check the harness could not carry out, and why.
    pub(crate) fn errored(
        check: &str,
        fixture: Option<&str>,
        profile: Option<Profile>,
        reason: impl Into<String>,
    ) -> Self {
        let reason = reason.into();
        let mut recorded = Self::new(check, fixture, profile);
        recorded.status = CheckStatus::Errored;
        recorded.evidence = format!("errored: {reason}\n");
        recorded.reason = Some(reason);
        recorded
    }

    fn metric(&mut self, name: &str, value: f64) {
        self.metrics.insert(name.to_owned(), value);
    }

    /// A count, as a metric. A count of entries or bytes stays far below 2^52, where an `f64`
    /// stops holding every integer.
    #[allow(clippy::cast_precision_loss)]
    fn metric_count(&mut self, name: &str, count: u64) {
        self.metric(name, count as f64);
    }

    fn note(&mut self, text: impl Into<String>) {
        self.notes.push(text.into());
    }
}

/// A check: what it observed in a form the classification can read, and what it recorded.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Checked<T> {
    /// What it observed, when it ran.
    pub observation: Option<T>,
    /// What it recorded.
    pub record: Recorded,
}

impl<T> Checked<T> {
    fn ran(observation: T, record: Recorded) -> Self {
        Self {
            observation: Some(observation),
            record,
        }
    }

    fn without(record: Recorded) -> Self {
        Self {
            observation: None,
            record,
        }
    }
}

/// How a program ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Exit {
    /// The exit code, when the program exited by itself.
    pub code: Option<i32>,
    /// The signal that ended it, when one did.
    pub signal: Option<i32>,
}

impl Exit {
    pub(crate) const fn of(info: ExitInfo) -> Self {
        Self {
            code: info.code,
            signal: info.signal,
        }
    }

    /// `exit code 130` or `killed by signal 15`.
    pub(crate) fn describe(self) -> String {
        match (self.code, self.signal) {
            (_, Some(signal)) => format!("killed by signal {signal}"),
            (Some(code), None) => format!("exit code {code}"),
            (None, None) => "an unknown status".to_owned(),
        }
    }
}

fn describe_exit(exit: Option<ExitInfo>) -> String {
    exit.map_or_else(
        || "it is still running".to_owned(),
        |exit| Exit::of(exit).describe(),
    )
}

fn seconds(duration: Duration) -> String {
    format!("{:.1} s", duration.as_secs_f64())
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

/// A fixture the checks run on, and its state before the first of them.
struct Subject {
    id: String,
    _shared: SharedFixture,
    root: FixtureRoot,
    baseline: FixtureSnapshot,
}

impl Subject {
    fn open(context: &Context<'_>, id: &str) -> Result<Self, String> {
        let shared = SharedFixture::acquire(context.fixtures, id, context.work_dir)
            .map_err(|error| error.to_string())?;
        let root = FixtureRoot::open(shared.root()).map_err(|error| error.to_string())?;
        let baseline = FixtureSnapshot::take(root.path()).map_err(|error| error.to_string())?;
        Ok(Self {
            id: id.to_owned(),
            _shared: shared,
            root,
            baseline,
        })
    }

    /// Stops the sweep if the fixture is not what it was: nothing a check does may change it.
    fn verify_unchanged(&self, version: &str) -> Result<(), Fatal> {
        let after = FixtureSnapshot::take(self.root.path()).map_err(|error| {
            Fatal(format!(
                "cannot look at fixture `{}` again: {error}",
                self.id
            ))
        })?;
        let changes = self.baseline.diff(&after).unexpected(&[], &[]);
        if changes.is_empty() {
            Ok(())
        } else {
            Err(Fatal(format!(
                "fixture `{}` changed while {version} ran a check that never deletes: {}; the \
                 sweep stops here",
                self.id,
                changes.join("; ")
            )))
        }
    }
}

/// How a scan got to COMPLETE, or why it did not.
enum Reach {
    Complete(Duration),
    Gave(String),
}

fn reach_complete(probe: &mut Probe, bound: Duration) -> Result<Reach, ProbeError> {
    Ok(match probe.wait_complete(bound)? {
        Wait::Ready(after) => Reach::Complete(after),
        Wait::TimedOut => Reach::Gave(format!(
            "the header did not read COMPLETE within {}: it reads `{}`",
            seconds(bound),
            probe.screen().row_text(0).trim()
        )),
        Wait::Exited => Reach::Gave(format!(
            "the program ended before COMPLETE ({})",
            describe_exit(probe.exit())
        )),
    })
}

fn screen_evidence(probe: &Probe) -> String {
    let text = probe.screen().text();
    let trimmed = text.trim_end();
    format!("--- the screen at the end ---\n{trimmed}\n")
}

fn start(
    context: &Context<'_>,
    subject: &Subject,
    scratch: &Scratch,
    profile: Profile,
) -> Result<Probe, ProbeError> {
    Probe::start(&ProbeSpec {
        binary: context.binary,
        fixture: &subject.root,
        scratch,
        profile,
        drain_bytes_per_sec: None,
    })
}

// ---------------------------------------------------------------------------------------------
// Signals (F6).

/// What a signal did to a program that was idle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SignalObservation {
    /// The signal.
    pub signal: Signal,
    /// How the program ended, or `None` when it was still running when the bound passed.
    pub exit: Option<Exit>,
    /// Whether the terminal was left as a shell expects it. `None` when the console was closed,
    /// after which nothing the program writes reaches the screen.
    pub restored: Option<bool>,
    /// What the scratch area held afterwards.
    pub residue: Vec<String>,
}

impl SignalObservation {
    /// Whether the program did what a confirmed quit does: exit 130, restore the terminal, and
    /// leave nothing behind.
    pub(crate) fn is_clean(&self) -> bool {
        self.exit.is_some_and(|exit| exit.code == Some(130))
            && self.restored != Some(false)
            && self.residue.is_empty()
    }

    /// A one-line summary: `exit code 130, terminal restored, no residue`.
    pub(crate) fn summary(&self) -> String {
        let exit = self
            .exit
            .map_or_else(|| "still running".to_owned(), Exit::describe);
        let terminal = match self.restored {
            Some(true) => "terminal restored",
            Some(false) => "terminal not restored",
            None => "terminal not checked",
        };
        let residue = match self.residue.len() {
            0 => "no residue".to_owned(),
            count => format!("{count} left behind"),
        };
        format!("{exit}, {terminal}, {residue}")
    }
}

/// The name a signal's check goes by.
pub(crate) fn signal_check_name(signal: Signal) -> String {
    format!("signal-{signal}")
}

/// Delivers `signal` to a program that has finished its scan, and observes how it ends.
pub(crate) fn signal(
    context: &Context<'_>,
    fixture_id: &str,
    version: &str,
    signal: Signal,
) -> Result<Checked<SignalObservation>, Fatal> {
    let check = signal_check_name(signal);
    let profile = Profile::Deterministic;
    let subject = match Subject::open(context, fixture_id) {
        Ok(subject) => subject,
        Err(error) => {
            return Ok(Checked::without(Recorded::errored(
                &check,
                Some(fixture_id),
                Some(profile),
                error,
            )));
        }
    };
    let checked = match signal_inner(context, &subject, signal, &check) {
        Ok(checked) => checked,
        Err(error) => Checked::without(Recorded::errored(
            &check,
            Some(fixture_id),
            Some(profile),
            error.to_string(),
        )),
    };
    subject.verify_unchanged(version)?;
    Ok(checked)
}

fn signal_inner(
    context: &Context<'_>,
    subject: &Subject,
    signal: Signal,
    check: &str,
) -> Result<Checked<SignalObservation>, ProbeError> {
    let profile = Profile::Deterministic;
    let scratch =
        Scratch::create(context.work_dir).map_err(|error| ProbeError::Binary(error.to_string()))?;
    let mut probe = start(context, subject, &scratch, profile)?;
    let after = match reach_complete(&mut probe, context.bound)? {
        Reach::Complete(after) => after,
        Reach::Gave(why) => {
            probe.kill();
            return Ok(Checked::without(Recorded::not_run(
                check,
                Some(&subject.id),
                Some(profile),
                why,
            )));
        }
    };
    probe.settle(QUIET, QUIET_LIMIT)?;
    probe.signal(signal)?;
    let exit = probe.wait_exit(END_BOUND)?;
    let restored = if signal == Signal::Close {
        None
    } else {
        Some(probe.modes().restored())
    };
    if exit.is_none() {
        probe.kill();
    }
    let modes = probe.modes();
    let screen = screen_evidence(&probe);
    let residue = scratch
        .residue()
        .map_err(|error| ProbeError::Binary(error.to_string()))?;
    let observation = SignalObservation {
        signal,
        exit: exit.map(Exit::of),
        restored,
        residue,
    };

    let mut record = Recorded::new(check, Some(&subject.id), Some(profile));
    record.metric("complete_ms", millis(after));
    if let Some(exit) = observation.exit {
        if let Some(code) = exit.code {
            record.metric("exit_code", f64::from(code));
        }
        if let Some(signal) = exit.signal {
            record.metric("exit_signal", f64::from(signal));
        }
    } else {
        record.note(format!(
            "still running {} after the signal; it was killed",
            seconds(END_BOUND)
        ));
    }
    record.metric_count("residue_files", observation.residue.len() as u64);
    record.note(format!("{signal}: {}", observation.summary()));
    for entry in &observation.residue {
        record.note(format!("left behind: {entry}"));
    }
    let _ = writeln!(
        record.evidence,
        "signal {signal} after COMPLETE ({}): {}\nterminal modes: {modes:?}",
        seconds(after),
        observation.summary()
    );
    for entry in &observation.residue {
        let _ = writeln!(record.evidence, "left behind: {entry}");
    }
    record.evidence.push_str(&screen);
    Ok(Checked::ran(observation, record))
}

// ---------------------------------------------------------------------------------------------
// SIGKILL, then a second start (F4).

/// What a killed run left, and whether the next start swept it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KillRestartObservation {
    /// What the scratch area held after the SIGKILL.
    pub left_by_kill: Vec<String>,
    /// What of that was still there when a second start, in the same scratch area, had finished
    /// its scan.
    pub survivors: Vec<String>,
}

/// The name of the kill-and-restart check.
pub(crate) const KILL_RESTART: &str = "kill-restart";

/// Kills a program with `SIGKILL` once it is idle, starts another in the same scratch area, and
/// looks at what the first left once the second has finished its scan.
pub(crate) fn kill_restart(
    context: &Context<'_>,
    fixture_id: &str,
    version: &str,
) -> Result<Checked<KillRestartObservation>, Fatal> {
    let profile = Profile::Deterministic;
    let subject = match Subject::open(context, fixture_id) {
        Ok(subject) => subject,
        Err(error) => {
            return Ok(Checked::without(Recorded::errored(
                KILL_RESTART,
                Some(fixture_id),
                Some(profile),
                error,
            )));
        }
    };
    let checked = match kill_restart_inner(context, &subject) {
        Ok(checked) => checked,
        Err(error) => Checked::without(Recorded::errored(
            KILL_RESTART,
            Some(fixture_id),
            Some(profile),
            error.to_string(),
        )),
    };
    subject.verify_unchanged(version)?;
    Ok(checked)
}

fn kill_restart_inner(
    context: &Context<'_>,
    subject: &Subject,
) -> Result<Checked<KillRestartObservation>, ProbeError> {
    let profile = Profile::Deterministic;
    let not_run = |why: String| {
        Checked::without(Recorded::not_run(
            KILL_RESTART,
            Some(&subject.id),
            Some(profile),
            why,
        ))
    };
    let scratch =
        Scratch::create(context.work_dir).map_err(|error| ProbeError::Binary(error.to_string()))?;
    let residue = |scratch: &Scratch| {
        scratch
            .residue()
            .map_err(|error| ProbeError::Binary(error.to_string()))
    };

    let mut first = start(context, subject, &scratch, profile)?;
    if let Reach::Gave(why) = reach_complete(&mut first, context.bound)? {
        first.kill();
        return Ok(not_run(format!("the first start: {why}")));
    }
    first.settle(QUIET, QUIET_LIMIT)?;
    first.kill();
    let left_by_kill = residue(&scratch)?;
    drop(first);

    let mut second = start(context, subject, &scratch, profile)?;
    let reached = reach_complete(&mut second, context.bound)?;
    if let Reach::Gave(why) = reached {
        second.kill();
        return Ok(not_run(format!("the second start: {why}")));
    }
    second.settle(QUIET, QUIET_LIMIT)?;
    let now = residue(&scratch)?;
    let screen = screen_evidence(&second);
    second.kill();
    let survivors: Vec<String> = left_by_kill
        .iter()
        .filter(|entry| now.contains(entry))
        .cloned()
        .collect();

    let mut record = Recorded::new(KILL_RESTART, Some(&subject.id), Some(profile));
    record.metric_count("left_by_kill", left_by_kill.len() as u64);
    record.metric_count("survivors", survivors.len() as u64);
    record.note(format!(
        "SIGKILL left {} entries; {} of them were still there when the second start finished its scan",
        left_by_kill.len(),
        survivors.len()
    ));
    let _ = writeln!(record.evidence, "left by SIGKILL:");
    for entry in &left_by_kill {
        let _ = writeln!(record.evidence, "  {entry}");
    }
    let _ = writeln!(
        record.evidence,
        "in the scratch area when the second start had finished its scan:"
    );
    for entry in &now {
        let marker = if left_by_kill.contains(entry) {
            "(left by the kill)"
        } else {
            "(the second run's own)"
        };
        let _ = writeln!(record.evidence, "  {entry} {marker}");
    }
    record.evidence.push_str(&screen);
    Ok(Checked::ran(
        KillRestartObservation {
            left_by_kill,
            survivors,
        },
        record,
    ))
}

// ---------------------------------------------------------------------------------------------
// Idle (F3).

/// What a program does with nothing to do.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct IdleObservation {
    /// The bytes of terminal output over the window.
    pub output_bytes: u64,
    /// The CPU time the program used over the window, in milliseconds, where the platform can say.
    pub cpu_ms: Option<f64>,
    /// How long after COMPLETE the window started.
    pub after: Duration,
    /// How long the window lasted.
    pub window: Duration,
}

/// The name of the idle check.
pub(crate) const IDLE: &str = "idle";

/// Waits for COMPLETE and then `window.after`, sends nothing, and counts the output and the CPU
/// time over the next `window.window`, under default motion.
pub(crate) fn idle(
    context: &Context<'_>,
    fixture_id: &str,
    version: &str,
    window: IdleWindow,
) -> Result<Checked<IdleObservation>, Fatal> {
    let profile = Profile::Default;
    let subject = match Subject::open(context, fixture_id) {
        Ok(subject) => subject,
        Err(error) => {
            return Ok(Checked::without(Recorded::errored(
                IDLE,
                Some(fixture_id),
                Some(profile),
                error,
            )));
        }
    };
    let checked = match idle_inner(context, &subject, window) {
        Ok(checked) => checked,
        Err(error) => Checked::without(Recorded::errored(
            IDLE,
            Some(fixture_id),
            Some(profile),
            error.to_string(),
        )),
    };
    subject.verify_unchanged(version)?;
    Ok(checked)
}

fn idle_inner(
    context: &Context<'_>,
    subject: &Subject,
    window: IdleWindow,
) -> Result<Checked<IdleObservation>, ProbeError> {
    let profile = Profile::Default;
    let scratch =
        Scratch::create(context.work_dir).map_err(|error| ProbeError::Binary(error.to_string()))?;
    let mut probe = start(context, subject, &scratch, profile)?;
    let after = match reach_complete(&mut probe, context.bound)? {
        Reach::Complete(after) => after,
        Reach::Gave(why) => {
            probe.kill();
            return Ok(Checked::without(Recorded::not_run(
                IDLE,
                Some(&subject.id),
                Some(profile),
                why,
            )));
        }
    };
    let ended = |probe: &Probe, when: &str| {
        Checked::without(Recorded::not_run(
            IDLE,
            Some(&subject.id),
            Some(profile),
            format!(
                "the program ended {when} ({}), so there is no idle window to look at",
                describe_exit(probe.exit())
            ),
        ))
    };
    if probe.pass(window.after)? {
        return Ok(ended(&probe, "while it was left alone"));
    }
    let start_bytes = probe.output_bytes();
    let start_cpu = probe.cpu_ms();
    if probe.pass(window.window)? {
        return Ok(ended(&probe, "during the idle window"));
    }
    let output_bytes = probe.output_bytes().saturating_sub(start_bytes);
    let cpu_ms = match (start_cpu, probe.cpu_ms()) {
        (Some(start), Some(end)) => Some((end - start).max(0.0)),
        _ => None,
    };
    let screen = screen_evidence(&probe);
    probe.kill();

    let mut record = Recorded::new(IDLE, Some(&subject.id), Some(profile));
    record.metric("complete_ms", millis(after));
    record.metric_count("idle_output_bytes", output_bytes);
    if let Some(cpu) = cpu_ms {
        record.metric("idle_cpu_ms", cpu);
    }
    record.note(format!(
        "{output_bytes} bytes of output and {} of CPU over a {} window that started {} after COMPLETE",
        cpu_ms.map_or_else(|| "unknown CPU".to_owned(), |cpu| format!("{cpu:.0} ms")),
        seconds(window.window),
        seconds(window.after)
    ));
    record.evidence = format!(
        "COMPLETE after {}; no input; window {} to {} after it\noutput bytes in the window: {output_bytes}\nCPU ms in the window: {}\n{screen}",
        seconds(after),
        seconds(window.after),
        seconds(window.after + window.window),
        cpu_ms.map_or_else(|| "unknown".to_owned(), |cpu| format!("{cpu:.1}")),
    );
    Ok(Checked::ran(
        IdleObservation {
            output_bytes,
            cpu_ms,
            after: window.after,
            window: window.window,
        },
        record,
    ))
}

// ---------------------------------------------------------------------------------------------
// The cursor at COMPLETE (F23b) and the filter (F22).

/// What came of one variant of the filter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FilterOutcome {
    /// The variant could not be carried out, and why: the scan did not reach COMPLETE, the folder
    /// did not open, or the filter prompt did not open. It shows nothing about the program, one
    /// way or the other.
    NotRun(String),
    /// The filter was applied and the program went on running.
    Survived {
        /// Whether the screen shows that the page the filter asked for could not be loaded.
        error_shown: bool,
    },
    /// The program ended on the Enter that applied the filter.
    Ended(Exit),
}

/// One variant of the filter: where it was applied and what came of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FilterAttempt {
    /// Where the filter was applied: `at the root` or `inside victim`.
    pub place: String,
    /// What came of it.
    pub outcome: FilterOutcome,
}

impl FilterAttempt {
    /// A variant that could not be carried out.
    fn not_run(place: impl Into<String>, why: impl Into<String>) -> Self {
        Self {
            place: place.into(),
            outcome: FilterOutcome::NotRun(why.into()),
        }
    }

    /// Why the variant did not run, when it did not.
    pub(crate) fn why_not(&self) -> Option<&str> {
        match &self.outcome {
            FilterOutcome::NotRun(why) => Some(why),
            FilterOutcome::Survived { .. } | FilterOutcome::Ended(_) => None,
        }
    }

    /// How the program ended on the Enter that applied the filter, when it did.
    pub(crate) fn exit(&self) -> Option<Exit> {
        match &self.outcome {
            FilterOutcome::Ended(exit) => Some(*exit),
            FilterOutcome::NotRun(_) | FilterOutcome::Survived { .. } => None,
        }
    }

    /// Whether the program ended on the Enter that applied the filter.
    pub(crate) fn ended(&self) -> bool {
        self.exit().is_some()
    }

    /// What came of it, in words.
    pub(crate) fn describe(&self) -> String {
        match &self.outcome {
            FilterOutcome::NotRun(why) => format!("did not run: {why}"),
            FilterOutcome::Survived { error_shown: true } => {
                "the program went on and says the page could not be filtered".to_owned()
            }
            FilterOutcome::Survived { error_shown: false } => "the program went on".to_owned(),
            FilterOutcome::Ended(exit) => format!("the program ended: {}", exit.describe()),
        }
    }
}

/// What the filter check observed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FilterObservation {
    /// The entry the selected-item pane showed at COMPLETE, with no key pressed.
    pub selected_at_complete: Inspector,
    /// The largest entry of the fixture's root, which an untouched cursor must land on.
    pub largest: Option<String>,
    /// The filter applied at the root.
    pub at_root: FilterAttempt,
    /// The filter applied inside the opened folder.
    pub in_folder: FilterAttempt,
}

/// The name of the filter check.
pub(crate) const FILTER: &str = "filter";

/// How the filter check names the variant that applies the filter at the root; the other is
/// [`folder_place`].
const ROOT_PLACE: &str = "at the root";

/// How the filter check names the variant that applies the filter inside the opened folder.
fn folder_place() -> String {
    format!("inside {FILTER_FOLDER}")
}

/// Applies a filter whose matches lie two and three levels below the folder it is applied in, once
/// at the root and once inside an opened folder, and watches what the program does. A variant that
/// cannot be carried out (the folder does not open, the prompt does not open, the scan does not
/// reach COMPLETE) is recorded as not run, with the reason.
///
/// Before any key it also reads which entry the selected-item pane shows at COMPLETE.
pub(crate) fn filter(
    context: &Context<'_>,
    fixture_id: &str,
    largest: Option<String>,
    version: &str,
) -> Result<Checked<FilterObservation>, Fatal> {
    let profile = Profile::Deterministic;
    let subject = match Subject::open(context, fixture_id) {
        Ok(subject) => subject,
        Err(error) => {
            return Ok(Checked::without(Recorded::errored(
                FILTER,
                Some(fixture_id),
                Some(profile),
                error,
            )));
        }
    };
    let checked = match filter_inner(context, &subject, largest) {
        Ok(checked) => checked,
        Err(error) => Checked::without(Recorded::errored(
            FILTER,
            Some(fixture_id),
            Some(profile),
            error.to_string(),
        )),
    };
    subject.verify_unchanged(version)?;
    Ok(checked)
}

fn filter_inner(
    context: &Context<'_>,
    subject: &Subject,
    largest: Option<String>,
) -> Result<Checked<FilterObservation>, ProbeError> {
    let profile = Profile::Deterministic;
    let mut evidence = String::new();

    // The folder variant first: it needs the cursor, which is also what F23b reads.
    let scratch =
        Scratch::create(context.work_dir).map_err(|error| ProbeError::Binary(error.to_string()))?;
    let mut probe = start(context, subject, &scratch, profile)?;
    let after = match reach_complete(&mut probe, context.bound)? {
        Reach::Complete(after) => after,
        Reach::Gave(why) => {
            probe.kill();
            return Ok(Checked::without(Recorded::not_run(
                FILTER,
                Some(&subject.id),
                Some(profile),
                why,
            )));
        }
    };
    probe.settle(QUIET, QUIET_LIMIT)?;
    let selected_at_complete = inspector(probe.screen());
    let _ = writeln!(
        evidence,
        "COMPLETE after {}; the selected-item pane at COMPLETE: {selected_at_complete:?}; the largest entry: {largest:?}",
        seconds(after)
    );

    let place = folder_place();
    let in_folder = match open_folder(&mut probe, FILTER_FOLDER, context.bound)? {
        Ok(()) => apply_filter(&mut probe, &place)?,
        Err(why) => FilterAttempt::not_run(place, why),
    };
    let _ = writeln!(evidence, "{}", filter_line(&in_folder));
    evidence.push_str(&screen_evidence(&probe));
    probe.kill();
    drop(probe);

    let scratch =
        Scratch::create(context.work_dir).map_err(|error| ProbeError::Binary(error.to_string()))?;
    let mut probe = start(context, subject, &scratch, profile)?;
    let at_root = match reach_complete(&mut probe, context.bound)? {
        Reach::Gave(why) => FilterAttempt::not_run(ROOT_PLACE, why),
        Reach::Complete(_) => {
            probe.settle(QUIET, QUIET_LIMIT)?;
            apply_filter(&mut probe, ROOT_PLACE)?
        }
    };
    let _ = writeln!(evidence, "{}", filter_line(&at_root));
    evidence.push_str(&screen_evidence(&probe));
    probe.kill();

    let observation = FilterObservation {
        selected_at_complete,
        largest,
        at_root,
        in_folder,
    };
    let record = filter_record(&subject.id, profile, evidence, &observation);
    Ok(Checked::ran(observation, record))
}

/// What the filter check notes of one variant: a line of the record and of the evidence.
fn filter_line(attempt: &FilterAttempt) -> String {
    format!(
        "filter `{FILTER_TEXT}` {}: {}",
        attempt.place,
        attempt.describe()
    )
}

/// What the filter check recorded of what it observed.
fn filter_record(
    fixture_id: &str,
    profile: Profile,
    evidence: String,
    observation: &FilterObservation,
) -> Recorded {
    let mut record = Recorded::new(FILTER, Some(fixture_id), Some(profile));
    for (attempt, metric) in [
        (&observation.at_root, "root_exit_code"),
        (&observation.in_folder, "folder_exit_code"),
    ] {
        record.note(filter_line(attempt));
        if let Some(code) = attempt.exit().and_then(|exit| exit.code) {
            record.metric(metric, f64::from(code));
        }
    }
    record.note(format!(
        "the selected-item pane at COMPLETE: {:?}; the largest entry: {:?}",
        observation.selected_at_complete, observation.largest
    ));
    record.evidence = evidence;
    record
}

/// Moves the cursor to `name` with the arrows and opens it with Enter, then waits for the header to
/// show the folder and for its scan to complete. `Err(why)` when it could not.
fn open_folder(
    probe: &mut Probe,
    name: &str,
    bound: Duration,
) -> Result<Result<(), String>, ProbeError> {
    let selected = |probe: &Probe| match inspector(probe.screen()) {
        Inspector::Item(item) => Some(item.name),
        Inspector::NotShown | Inspector::NothingSelected => None,
    };
    let mut cycle = [Key::Right, Key::Down, Key::Left, Key::Up]
        .into_iter()
        .cycle();
    let mut attempts = 0;
    while selected(probe).as_deref() != Some(name) {
        if attempts == SELECT_ATTEMPTS {
            return Ok(Err(format!(
                "the cursor did not reach `{name}` in {SELECT_ATTEMPTS} arrow presses: the selected-item pane shows {:?}",
                inspector(probe.screen())
            )));
        }
        if let Some(key) = cycle.next() {
            probe.send(key)?;
        }
        probe.settle(QUIET, QUIET_LIMIT)?;
        attempts += 1;
    }
    probe.send(Key::Enter)?;
    let deadline = std::time::Instant::now() + bound;
    let shown = probe.wait_until(deadline, |probe| {
        header_path(probe.screen()).filter(|path| path.contains(name))
    })?;
    if shown.ready().is_none() {
        return Ok(Err(format!(
            "Enter did not open `{name}`: the header reads `{}`",
            probe.screen().row_text(0).trim()
        )));
    }
    match reach_complete(probe, bound)? {
        Reach::Complete(_) => {
            probe.settle(QUIET, QUIET_LIMIT)?;
            Ok(Ok(()))
        }
        Reach::Gave(why) => Ok(Err(format!("inside `{name}`: {why}"))),
    }
}

/// Opens the filter, types [`FILTER_TEXT`], applies it, and watches for the program to end. A
/// filter prompt that does not open makes the variant one that did not run.
fn apply_filter(probe: &mut Probe, place: &str) -> Result<FilterAttempt, ProbeError> {
    probe.send(Key::Slash)?;
    probe.settle(QUIET, QUIET_LIMIT)?;
    if filter_prompt(probe.screen()).is_none() {
        let why = format!(
            "the filter prompt did not open {place}: the header reads `{}`",
            probe.screen().row_text(0).trim()
        );
        return Ok(FilterAttempt::not_run(place, why));
    }
    probe.type_text(FILTER_TEXT)?;
    probe.settle(QUIET, QUIET_LIMIT)?;
    probe.send(Key::Enter)?;
    let ended = probe.wait_until(std::time::Instant::now() + CRASH_WINDOW, |probe| {
        probe.exit().map(|_| ())
    })?;
    let exit = match ended {
        Wait::Ready(()) | Wait::Exited => probe.exit().map(Exit::of),
        Wait::TimedOut => None,
    };
    let outcome = if let Some(exit) = exit {
        FilterOutcome::Ended(exit)
    } else {
        probe.settle(QUIET, QUIET_LIMIT)?;
        FilterOutcome::Survived {
            error_shown: probe
                .screen()
                .text()
                .contains("Could not filter this scan page"),
        }
    };
    Ok(FilterAttempt {
        place: place.to_owned(),
        outcome,
    })
}

// ---------------------------------------------------------------------------------------------
// The cursor through COMPLETE (F5).

/// What one attempt to reproduce the selection drift saw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DriftAttempt {
    /// The entry that the scan lists first, and that is the largest until the very end of the
    /// walk, was never selected while the scan ran: the precondition did not occur. There was no
    /// choice to keep, so the attempt tested nothing.
    NoPrecondition,
    /// The precondition held and the cursor was moved, but the scan did not reach COMPLETE: it
    /// outlasted the bound, or the program ended. The selection was never read after COMPLETE, so
    /// the attempt shows neither the drift nor its absence.
    Incomplete,
    /// The precondition held, the cursor was moved, and the scan reached COMPLETE.
    Complete {
        /// What the selected-item pane showed before COMPLETE, after the cursor was moved: the
        /// entry the user chose, or `None` when the pane showed no entry.
        before: Option<String>,
        /// What it showed after COMPLETE and after the map settled, or `None` when it showed no
        /// entry.
        after: Option<String>,
    },
}

impl DriftAttempt {
    /// Whether the precondition held: the entry the scan lists first was selected before COMPLETE.
    pub(crate) const fn held_precondition(&self) -> bool {
        !matches!(self, Self::NoPrecondition)
    }

    /// Whether the scan reached COMPLETE after the cursor was moved, so that the selection was
    /// read after it.
    pub(crate) const fn reached_complete(&self) -> bool {
        matches!(self, Self::Complete { .. })
    }

    /// Whether the selection moved off the entry the user chose: the pane, read after COMPLETE and
    /// the settling, names another entry than before. An attempt that never reached COMPLETE, and
    /// one whose pane showed no entry before or after, did not read a different entry and is not a
    /// drift.
    pub(crate) fn drifted(&self) -> bool {
        match self {
            Self::Complete {
                before: Some(before),
                after: Some(after),
            } => after != before,
            Self::NoPrecondition | Self::Incomplete | Self::Complete { .. } => false,
        }
    }
}

/// What every attempt saw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DriftObservation {
    /// The attempts, in order.
    pub attempts: Vec<DriftAttempt>,
}

impl DriftObservation {
    /// How many attempts held the precondition.
    pub(crate) fn preconditions(&self) -> usize {
        self.count(DriftAttempt::held_precondition)
    }

    /// How many attempts reached COMPLETE after the cursor was moved.
    pub(crate) fn completed(&self) -> usize {
        self.count(DriftAttempt::reached_complete)
    }

    /// How many attempts held the precondition and never reached COMPLETE.
    pub(crate) fn incomplete(&self) -> usize {
        self.count(|attempt| matches!(attempt, DriftAttempt::Incomplete))
    }

    /// How many attempts saw the selection leave the entry the cursor was moved onto.
    pub(crate) fn drifted(&self) -> usize {
        self.count(DriftAttempt::drifted)
    }

    /// What the attempts came to, in a sentence: the record's note and the table's value.
    pub(crate) fn summary(&self) -> String {
        format!(
            "{} of {} attempts saw the selection leave the entry the cursor was moved onto after COMPLETE; the precondition held in {}, and the scan reached COMPLETE after it in {} of those ({} did not)",
            self.drifted(),
            self.attempts.len(),
            self.preconditions(),
            self.completed(),
            self.incomplete()
        )
    }

    fn count(&self, matching: impl Fn(&DriftAttempt) -> bool) -> usize {
        self.attempts
            .iter()
            .filter(|attempt| matching(attempt))
            .count()
    }
}

/// The name of the selection-drift check.
pub(crate) const DRIFT: &str = "selection-drift";
/// The entry the `selection-drift` fixture's scan lists first and which is the largest until the
/// tail of the walk.
pub(crate) const DRIFT_ENTRY: &str = "big-file.bin";

/// Tries to reproduce the selection drift: moves the cursor onto the provisional largest entry
/// while the folder that will end up larger is still being measured, and watches whether the
/// selection stays there through COMPLETE.
///
/// An attempt whose scan never reaches COMPLETE reads nothing after it: it is counted apart and
/// is not a drift.
pub(crate) fn drift(
    context: &Context<'_>,
    fixture_id: &str,
    attempts: u32,
    version: &str,
) -> Result<Checked<DriftObservation>, Fatal> {
    let profile = Profile::Deterministic;
    let subject = match Subject::open(context, fixture_id) {
        Ok(subject) => subject,
        Err(error) => {
            return Ok(Checked::without(Recorded::errored(
                DRIFT,
                Some(fixture_id),
                Some(profile),
                error,
            )));
        }
    };
    let mut seen = Vec::new();
    let mut evidence = String::new();
    for number in 1..=attempts {
        match drift_attempt(context, &subject, &mut evidence, number) {
            Ok(attempt) => seen.push(attempt),
            Err(error) => {
                subject.verify_unchanged(version)?;
                return Ok(Checked::without(Recorded::errored(
                    DRIFT,
                    Some(fixture_id),
                    Some(profile),
                    error.to_string(),
                )));
            }
        }
    }
    subject.verify_unchanged(version)?;
    let observation = DriftObservation { attempts: seen };
    let record = drift_record(&subject.id, profile, &observation, evidence);
    Ok(Checked::ran(observation, record))
}

/// What the selection-drift check recorded of what it observed: how many attempts held the
/// precondition, how many of those reached COMPLETE, and how many saw the selection drift. The
/// evidence file ends with the same counts, after each attempt's own lines.
fn drift_record(
    fixture_id: &str,
    profile: Profile,
    observation: &DriftObservation,
    mut evidence: String,
) -> Recorded {
    let mut record = Recorded::new(DRIFT, Some(fixture_id), Some(profile));
    record.metric_count("attempts", observation.attempts.len() as u64);
    record.metric_count("precondition_met", observation.preconditions() as u64);
    record.metric_count("reached_complete", observation.completed() as u64);
    record.metric_count("drifted", observation.drifted() as u64);
    let summary = observation.summary();
    let _ = writeln!(evidence, "{summary}");
    record.note(summary);
    record.evidence = evidence;
    record
}

fn drift_attempt(
    context: &Context<'_>,
    subject: &Subject,
    evidence: &mut String,
    number: u32,
) -> Result<DriftAttempt, ProbeError> {
    let selected = |probe: &Probe| match inspector(probe.screen()) {
        Inspector::Item(item) => Some(item.name),
        Inspector::NotShown | Inspector::NothingSelected => None,
    };
    let scratch =
        Scratch::create(context.work_dir).map_err(|error| ProbeError::Binary(error.to_string()))?;
    let mut probe = start(context, subject, &scratch, Profile::Deterministic)?;
    let deadline = std::time::Instant::now() + context.bound;
    let first = probe.wait_until(deadline, |probe| {
        (selected(probe).as_deref() == Some(DRIFT_ENTRY)
            && !matches!(probe.header(), Some(crate::pty::ui::HeaderState::Complete)))
        .then_some(())
    })?;
    let _ = writeln!(evidence, "attempt {number}:");
    if first.ready().is_none() {
        let _ = writeln!(
            evidence,
            "  the precondition did not occur: the selected-item pane shows {:?} and the header reads {:?}",
            inspector(probe.screen()),
            probe.header()
        );
        probe.kill();
        return Ok(DriftAttempt::NoPrecondition);
    }
    // The user's deliberate move: in a two-tile map `Down` has no neighbour, so it never changes
    // the selection on its own, and sending it is what counts as input before COMPLETE.
    probe.send(Key::Down)?;
    probe.settle(QUIET, QUIET_LIMIT)?;
    let before = selected(&probe);
    let reached = reach_complete(&mut probe, context.bound)?;
    if let Reach::Gave(why) = reached {
        let _ = writeln!(
            evidence,
            "  {why}; the cursor was moved onto {before:?} and the scan never reached COMPLETE, so the selection was not read after it: this attempt shows neither the drift nor its absence"
        );
        probe.kill();
        return Ok(DriftAttempt::Incomplete);
    }
    probe.settle(QUIET, QUIET_LIMIT)?;
    probe.pass(QUIET)?;
    probe.settle(QUIET, QUIET_LIMIT)?;
    let after = selected(&probe);
    let _ = writeln!(
        evidence,
        "  before COMPLETE, after the cursor moved: {before:?}; after COMPLETE and the settling: {after:?}"
    );
    probe.kill();
    Ok(DriftAttempt::Complete { before, after })
}

// ---------------------------------------------------------------------------------------------
// Descriptors (F13).

/// How many descriptors a program holds while it scans a small tree and a large one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DescriptorObservation {
    /// The most descriptors seen while the small tree was scanned.
    pub small: u32,
    /// The most seen while the large tree was scanned, when the scan finished within the bound.
    pub large: Option<u32>,
}

/// The name of the descriptor check.
pub(crate) const DESCRIPTORS: &str = "descriptors";

/// Scans `small_id` and `large_id` to COMPLETE and reads the most descriptors the program held.
pub(crate) fn descriptors(
    context: &Context<'_>,
    small_id: &str,
    large_id: &str,
    version: &str,
) -> Result<Checked<DescriptorObservation>, Fatal> {
    let profile = Profile::Deterministic;
    let mut peaks = Vec::new();
    let mut evidence = String::new();
    for id in [small_id, large_id] {
        let subject = match Subject::open(context, id) {
            Ok(subject) => subject,
            Err(error) => {
                return Ok(Checked::without(Recorded::errored(
                    DESCRIPTORS,
                    Some(large_id),
                    Some(profile),
                    error,
                )));
            }
        };
        let measured = descriptors_of(context, &subject);
        subject.verify_unchanged(version)?;
        match measured {
            Ok((peak, text)) => {
                evidence.push_str(&text);
                peaks.push(peak);
            }
            Err(error) => {
                return Ok(Checked::without(Recorded::errored(
                    DESCRIPTORS,
                    Some(large_id),
                    Some(profile),
                    error.to_string(),
                )));
            }
        }
    }
    let (Some(small), Some(large)) = (peaks.first().copied(), peaks.get(1).copied()) else {
        return Ok(Checked::without(Recorded::errored(
            DESCRIPTORS,
            Some(large_id),
            Some(profile),
            "a descriptor count is missing",
        )));
    };
    let Some(small) = small else {
        return Ok(Checked::without(Recorded::not_run(
            DESCRIPTORS,
            Some(small_id),
            Some(profile),
            "the small tree did not reach COMPLETE, or the platform cannot count descriptors",
        )));
    };
    let mut record = Recorded::new(DESCRIPTORS, Some(large_id), Some(profile));
    record.metric("fds_small", f64::from(small));
    if let Some(large) = large {
        record.metric("fds_large", f64::from(large));
    }
    record.note(format!(
        "{small} descriptors on `{small_id}`, {} on `{large_id}`",
        large.map_or_else(|| "no count".to_owned(), |large| large.to_string())
    ));
    record.evidence = evidence;
    Ok(Checked::ran(DescriptorObservation { small, large }, record))
}

fn descriptors_of(
    context: &Context<'_>,
    subject: &Subject,
) -> Result<(Option<u32>, String), ProbeError> {
    let scratch =
        Scratch::create(context.work_dir).map_err(|error| ProbeError::Binary(error.to_string()))?;
    let mut probe = start(context, subject, &scratch, Profile::Deterministic)?;
    let text = match reach_complete(&mut probe, context.bound)? {
        Reach::Complete(after) => {
            probe.settle(QUIET, QUIET_LIMIT)?;
            format!(
                "`{}`: COMPLETE after {}; at most {:?} descriptors held\n",
                subject.id,
                seconds(after),
                probe.max_fds()
            )
        }
        Reach::Gave(why) => format!("`{}`: {why}\n", subject.id),
    };
    let peak = if text.contains("COMPLETE after") {
        probe.max_fds()
    } else {
        None
    };
    probe.kill();
    Ok((peak, text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_confirmed_quit_is_exit_130_with_the_terminal_restored_and_nothing_left() {
        let clean = SignalObservation {
            signal: Signal::Term,
            exit: Some(Exit {
                code: Some(130),
                signal: None,
            }),
            restored: Some(true),
            residue: Vec::new(),
        };
        assert!(clean.is_clean());
        assert_eq!(
            clean.summary(),
            "exit code 130, terminal restored, no residue"
        );

        for broken in [
            SignalObservation {
                exit: Some(Exit {
                    code: None,
                    signal: Some(15),
                }),
                ..clean.clone()
            },
            SignalObservation {
                restored: Some(false),
                ..clean.clone()
            },
            SignalObservation {
                residue: vec!["store/.excise-scan-x".to_owned()],
                ..clean.clone()
            },
            SignalObservation {
                exit: None,
                ..clean.clone()
            },
            SignalObservation {
                exit: Some(Exit {
                    code: Some(2),
                    signal: None,
                }),
                ..clean.clone()
            },
        ] {
            assert!(!broken.is_clean(), "{}", broken.summary());
        }
    }

    #[test]
    fn a_closed_console_is_not_held_to_a_terminal_it_cannot_show() {
        let closed = SignalObservation {
            signal: Signal::Close,
            exit: Some(Exit {
                code: Some(130),
                signal: None,
            }),
            restored: None,
            residue: Vec::new(),
        };
        assert!(closed.is_clean());
        assert!(closed.summary().contains("terminal not checked"));
    }

    #[test]
    fn an_exit_is_described_by_its_code_or_its_signal() {
        assert_eq!(
            Exit {
                code: Some(101),
                signal: None
            }
            .describe(),
            "exit code 101"
        );
        assert_eq!(
            Exit {
                code: None,
                signal: Some(9)
            }
            .describe(),
            "killed by signal 9"
        );
    }

    fn complete(before: Option<&str>, after: Option<&str>) -> DriftAttempt {
        DriftAttempt::Complete {
            before: before.map(str::to_owned),
            after: after.map(str::to_owned),
        }
    }

    #[test]
    fn a_drift_needs_another_entry_read_after_complete() {
        assert!(complete(Some("big-file.bin"), Some("victim")).drifted());
        assert!(!complete(Some("big-file.bin"), Some("big-file.bin")).drifted());
        assert!(
            !DriftAttempt::NoPrecondition.drifted(),
            "without the precondition there was no choice to keep"
        );
        assert!(
            !complete(None, Some("victim")).drifted(),
            "a pane that showed no entry after the move names no entry the user chose"
        );
        assert!(
            !complete(Some("big-file.bin"), None).drifted(),
            "a pane that shows no entry after COMPLETE does not name a different one"
        );
    }

    #[test]
    fn an_attempt_that_never_reached_complete_is_not_a_drift() {
        let incomplete = DriftAttempt::Incomplete;

        assert!(incomplete.held_precondition());
        assert!(!incomplete.reached_complete());
        assert!(
            !incomplete.drifted(),
            "nothing was read after COMPLETE, so there is no different entry"
        );
        assert!(complete(Some("big-file.bin"), None).reached_complete());
        assert!(!DriftAttempt::NoPrecondition.held_precondition());
    }

    #[test]
    fn the_drift_record_counts_what_held_the_precondition_reached_complete_and_drifted() {
        let observation = DriftObservation {
            attempts: vec![
                DriftAttempt::NoPrecondition,
                DriftAttempt::Incomplete,
                complete(Some("big-file.bin"), Some("big-file.bin")),
                complete(Some("big-file.bin"), Some("victim")),
            ],
        };

        let record = drift_record(
            "selection-drift",
            Profile::Deterministic,
            &observation,
            "attempt 1:\n".to_owned(),
        );

        for (metric, expected) in [
            ("attempts", 4.0),
            ("precondition_met", 3.0),
            ("reached_complete", 2.0),
            ("drifted", 1.0),
        ] {
            assert!(
                (record.metrics[metric] - expected).abs() < f64::EPSILON,
                "{metric}: {:?}",
                record.metrics
            );
        }
        assert_eq!(record.notes.len(), 1);
        let note = &record.notes[0];
        assert!(note.starts_with("1 of 4 attempts"), "{note}");
        assert!(note.contains("the precondition held in 3"), "{note}");
        assert!(
            note.contains("reached COMPLETE after it in 2 of those (1 did not)"),
            "{note}"
        );
        assert_eq!(
            record.evidence,
            format!("attempt 1:\n{}\n", observation.summary()),
            "the evidence keeps each attempt's lines and ends with the counts"
        );
    }

    fn filter_attempt(place: &str, outcome: FilterOutcome) -> FilterAttempt {
        FilterAttempt {
            place: place.to_owned(),
            outcome,
        }
    }

    fn ended_with(code: i32) -> FilterOutcome {
        FilterOutcome::Ended(Exit {
            code: Some(code),
            signal: None,
        })
    }

    #[test]
    fn a_filter_variant_that_did_not_run_says_why_and_is_not_a_survival() {
        let missing = FilterAttempt::not_run(ROOT_PLACE, "the filter prompt did not open");
        let ended = filter_attempt("inside victim", ended_with(101));
        let survived = filter_attempt(ROOT_PLACE, FilterOutcome::Survived { error_shown: false });

        assert_eq!(missing.why_not(), Some("the filter prompt did not open"));
        assert_eq!(missing.exit(), None);
        assert!(!missing.ended());
        assert_eq!(
            missing.describe(),
            "did not run: the filter prompt did not open"
        );
        assert_eq!(ended.why_not(), None);
        assert!(ended.ended());
        assert_eq!(ended.describe(), "the program ended: exit code 101");
        assert_eq!(survived.why_not(), None);
        assert!(!survived.ended());
        assert_eq!(survived.describe(), "the program went on");
    }

    #[test]
    fn the_filter_record_says_what_came_of_each_variant() {
        let observation = FilterObservation {
            selected_at_complete: Inspector::NothingSelected,
            largest: Some("victim".to_owned()),
            at_root: FilterAttempt::not_run(ROOT_PLACE, "the header did not read COMPLETE"),
            in_folder: filter_attempt("inside victim", ended_with(101)),
        };

        let record = filter_record(
            "delete-folder",
            Profile::Deterministic,
            String::new(),
            &observation,
        );

        assert_eq!(
            record.notes[0],
            "filter `part00` at the root: did not run: the header did not read COMPLETE"
        );
        assert_eq!(
            record.notes[1],
            "filter `part00` inside victim: the program ended: exit code 101"
        );
        assert!((record.metrics["folder_exit_code"] - 101.0).abs() < f64::EPSILON);
        assert!(
            !record.metrics.contains_key("root_exit_code"),
            "{:?}",
            record.metrics
        );
    }

    #[test]
    fn what_a_check_could_not_do_is_recorded_with_its_reason() {
        let not_run = Recorded::not_run(
            "idle",
            Some("navigate-folders"),
            Some(Profile::Default),
            "no COMPLETE",
        );
        assert_eq!(not_run.status, CheckStatus::NotRun);
        assert_eq!(not_run.reason.as_deref(), Some("no COMPLETE"));
        let errored = Recorded::errored("idle", None, None, "cannot spawn");
        assert_eq!(errored.status, CheckStatus::Errored);
        assert_eq!(errored.reason.as_deref(), Some("cannot spawn"));
    }
}
