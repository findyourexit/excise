//! The step executor: the state of one run and the primitives every step is built from.
//!
//! An [`Executor`] owns the running program, its event log, and the bookkeeping that latency and
//! settling depend on. Steps (in the `steps` module) never sleep and never look at output bytes.
//! They wait through [`Executor::wait_until`], which drives the session and the event log and
//! evaluates a condition after every bit of progress.
//!
//! # Waiting
//!
//! Every wait has a deadline and ends in one of three ways: the condition held, the deadline
//! passed, or the program ended (its exit seen and all its output read) without the condition
//! holding. The last case fails at once instead of running the clock out.
//!
//! # Inputs and frames
//!
//! The event channel's `frame` event carries `inputs`, the number of terminal input events the
//! program had consumed when it drew that frame. The executor counts every input event it writes,
//! but the program may have counted some of its own before the first: `excise` on Windows counts
//! one before any key is sent. So the executor takes the `inputs` of the latest frame, or 0 before
//! the first, when it writes the first input, and calls it the input baseline. A frame *reflects*
//! the inputs sent so far when its counter is at least the baseline plus their number and it was
//! observed after the last input was written. An input that the program counts of its own only
//! after the first one was written is not accounted for. Waiting for such a frame is what `settle`
//! means, and it is how a step knows that the screen it is about to read shows the effect of a key
//! and not the moment before it.
//!
//! # Catching up with a frame
//!
//! A `frame` event says that the program drew, not that the drawing has reached the screen model:
//! the event arrives through a file, the screen through the terminal. A step that ends on a frame
//! event (`settle`, `delete`, `resize`) therefore keeps reading the terminal for a while before it
//! returns (`Executor::catch_up`, in the `steps` module), so that the step after it can read the
//! screen once. How long depends on the terminal: see [`FRAME_WINDOW`].

use std::{
    collections::BTreeMap,
    path::PathBuf,
    time::{Duration, Instant},
};

use crate::{
    events::{Event, EventLog, Payload},
    metrics::{Recorder, StoreSampler},
    pty::{Diagnostics, PtySession},
    safety::{FixtureRoot, FixtureSnapshot, Scratch},
    scenario::{Scenario, Step},
};

use super::{
    outcome::{FailureCause, RunError, StepFailure, Stop},
    plan::{Prepared, describe_step},
};

/// How long a wait blocks for terminal output before it looks at everything else again.
pub(super) const POLL_INTERVAL: Duration = Duration::from_millis(1);
/// How often a wait for a file system condition looks at the file system.
pub(super) const FS_POLL_INTERVAL: Duration = Duration::from_millis(10);
/// How often the run's scan-store directory is sampled for its peak size.
pub(super) const STORE_SAMPLE_INTERVAL: Duration = Duration::from_millis(50);
/// How long `ConPTY` can hold a frame's output back after the program has reported the frame.
///
/// `ConPTY` is a renderer, not a pipe: it keeps its own copy of the screen and sends what changed
/// in paints that are typically 16 ms apart, so a frame that is drawn soon after a paint waits for
/// the next one. In the recordings of failed runs on GitHub-hosted Windows runners, 30 isolated
/// frames reached the harness 4 to 22 ms after the program reported them (median 12.5 ms), and the
/// gaps between 188 paints of a console that was being redrawn had a median of 15.6 ms and a 99th
/// percentile of 23 ms; the longest, 54 ms, came while the program was starting. These are upper
/// bounds, because the program's clock and the recording's differ by an offset that only the
/// moments the keys were sent bound. 100 ms is more than four times the longest delay once the
/// program was running, and about twice the longest of all.
pub(super) const CONPTY_FRAME_WINDOW: Duration = Duration::from_millis(100);
/// How long a step that waited for a frame event keeps reading the terminal before it trusts the
/// screen: `ConPTY`'s window on Windows, none elsewhere. The program hands a frame to its writer
/// thread before it reports the frame, and a Unix pseudo-terminal passes bytes on as they are
/// written, so the frame is at most a thread switch behind its event, which the bounded tail in
/// `Executor::catch_up` covers.
pub(super) const FRAME_WINDOW: Duration = if cfg!(windows) {
    CONPTY_FRAME_WINDOW
} else {
    Duration::ZERO
};

/// How a bounded wait ended.
#[derive(Debug)]
pub(super) enum Waited<T> {
    /// The condition held.
    Ready(T),
    /// The deadline passed.
    TimedOut,
    /// The program ended first.
    Exited,
}

pub(crate) struct Executor<'a> {
    pub(super) scenario: &'a Scenario,
    pub(super) prepared: &'a [Prepared],
    pub(super) fixture: &'a FixtureRoot,
    pub(super) scratch: &'a Scratch,
    pub(super) baseline: &'a FixtureSnapshot,
    pub(crate) session: PtySession,
    pub(crate) events: EventLog,
    pub(super) recorder: Recorder,
    /// Terminal input events written so far.
    pub(super) inputs_sent: u64,
    pub(super) last_input_at: Option<Instant>,
    /// The `inputs` the program had counted when the first input was written: inputs of its own,
    /// which the executor did not send. `None` until that moment. A frame that reflects the inputs
    /// sent counts them on top of this baseline.
    pub(super) input_baseline: Option<u64>,
    /// How long `catch_up` reads the terminal after a frame event, before its bounded tail. A field
    /// so that a test can give a Unix terminal the delay `ConPTY` has.
    pub(super) frame_window: Duration,
    /// Fixture-relative paths of deletions that were confirmed.
    pub(super) intended_deletions: Vec<String>,
    /// Fixture-relative paths that `fs_mutate` steps changed.
    pub(super) intended_mutations: Vec<String>,
    pub(super) residue_files: Option<usize>,
    /// The scan-store directory the program was given: the scratch area's own, or the directory a
    /// scenario moved it to (`scan_store_on_volume`).
    store_dir: PathBuf,
    store_sampler: StoreSampler,
    last_store_sample: Instant,
    hello_checked: bool,
}

impl<'a> Executor<'a> {
    pub(crate) fn new(
        scenario: &'a Scenario,
        prepared: &'a [Prepared],
        fixture: &'a FixtureRoot,
        scratch: &'a Scratch,
        baseline: &'a FixtureSnapshot,
        store_dir: PathBuf,
        session: PtySession,
    ) -> Self {
        let recorder = Recorder::new(session.started());
        let events = EventLog::new(scratch.events());
        let started = session.started();
        Self {
            scenario,
            prepared,
            fixture,
            scratch,
            baseline,
            session,
            events,
            recorder,
            inputs_sent: 0,
            last_input_at: None,
            input_baseline: None,
            frame_window: FRAME_WINDOW,
            intended_deletions: Vec::new(),
            intended_mutations: Vec::new(),
            residue_files: None,
            store_dir,
            store_sampler: StoreSampler::default(),
            last_store_sample: started
                .checked_sub(STORE_SAMPLE_INTERVAL)
                .unwrap_or(started),
            hello_checked: false,
        }
    }

    /// Runs every step, then the checks that hold after the last one.
    ///
    /// # Errors
    ///
    /// Returns the failed step or the harness error that stopped the run.
    pub(crate) fn run(&mut self) -> Result<(), Stop> {
        for (index, step) in self.scenario.steps.iter().enumerate() {
            self.execute(index, step)?;
        }
        self.final_checks()
    }

    /// The metrics of the run so far: `cgroup_memory_peak_bytes` joins them once the session has
    /// seen the exit (see `PtySession::mark_cgroup_wrapped`), so an `expect_budget` step placed
    /// after the program has exited (for example after `expect_exit`) can check it like any other
    /// metric.
    pub(crate) fn metrics(&self) -> BTreeMap<String, f64> {
        let mut metrics = self.recorder.finish(
            self.events.events(),
            self.session.output_bytes(),
            self.session.sampler(),
            self.store_sampler.peak_bytes(),
            self.session.cpu_time(),
            Instant::now(),
        );
        if let Some(files) = self.residue_files {
            #[allow(clippy::cast_precision_loss)]
            metrics.insert("residue_files".to_owned(), files as f64);
        }
        if let Some(bytes) = self.session.cgroup_memory_peak_bytes() {
            #[allow(
                clippy::cast_precision_loss,
                reason = "byte counts far below 2^52, like every other byte metric in this crate"
            )]
            metrics.insert("cgroup_memory_peak_bytes".to_owned(), bytes as f64);
        }
        metrics
    }

    /// Reads all pending terminal output and events.
    pub(super) fn pump(&mut self) -> Result<(), Stop> {
        self.session.pump()?;
        if self.last_store_sample.elapsed() >= STORE_SAMPLE_INTERVAL {
            self.last_store_sample = Instant::now();
            self.store_sampler.sample(&self.store_dir);
        }
        if self.events.poll(Instant::now())? > 0 && !self.hello_checked {
            self.check_hello()?;
        }
        if let Some(exit) = self.session.exit() {
            self.recorder.record_exit(exit.at);
        }
        Ok(())
    }

    /// The first event must be the `hello` of the process that was started. An event file that
    /// belongs to another process would make every wait meaningless.
    fn check_hello(&mut self) -> Result<(), Stop> {
        let Some(first) = self.events.events().first() else {
            return Ok(());
        };
        self.hello_checked = true;
        match &first.payload {
            Payload::Hello { pid, .. } if *pid == u64::from(self.session.pid()) => Ok(()),
            Payload::Hello { pid, .. } => Err(RunError::Protocol(format!(
                "the event channel says it belongs to process {pid}, but the process started \
                 was {}",
                self.session.pid()
            ))
            .into()),
            other => Err(RunError::Protocol(format!(
                "the event channel starts with `{}` instead of `hello`",
                other.kind_name()
            ))
            .into()),
        }
    }

    /// Drives the session until `probe` returns a value, the deadline passes, or the program ends.
    pub(super) fn wait_until<T>(
        &mut self,
        deadline: Instant,
        mut probe: impl FnMut(&Self) -> Option<T>,
    ) -> Result<Waited<T>, Stop> {
        loop {
            self.pump()?;
            if let Some(value) = probe(self) {
                return Ok(Waited::Ready(value));
            }
            if self.session.finished() {
                return Ok(Waited::Exited);
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(Waited::TimedOut);
            }
            self.session
                .wait_activity(deadline.saturating_duration_since(now).min(POLL_INTERVAL))?;
        }
    }

    /// The deadline of a step whose bound is `timeout_ms`.
    pub(super) fn deadline(timeout_ms: u64) -> Instant {
        Instant::now() + Duration::from_millis(timeout_ms)
    }

    /// Writes one terminal input event and records it.
    pub(super) fn send_input(&mut self, bytes: &[u8]) -> Result<Instant, Stop> {
        let isolated = self.frame_reflecting_inputs().is_some();
        self.take_input_baseline();
        self.inputs_sent += 1;
        let at = self.session.send(bytes)?;
        self.last_input_at = Some(at);
        self.recorder
            .record_input(at, self.inputs_sent, self.reflecting_counter(), isolated);
        Ok(at)
    }

    /// Fixes the input baseline when the first input is about to be written, and leaves it alone
    /// afterwards: the `inputs` of the latest frame, or 0 before the first frame. A program may
    /// count inputs of its own before it is sent any, and a counter that includes them must not be
    /// taken for one that includes the first key.
    pub(super) fn take_input_baseline(&mut self) {
        if self.input_baseline.is_none() {
            self.input_baseline = Some(self.latest_frame_inputs());
        }
    }

    /// The `inputs` counter at which a frame reflects every input sent so far.
    pub(super) fn reflecting_counter(&self) -> u64 {
        self.input_baseline.unwrap_or(0) + self.inputs_sent
    }

    /// The latest frame, if it reflects every input sent so far.
    pub(super) fn frame_reflecting_inputs(&self) -> Option<&Event> {
        let frame = self
            .events
            .events()
            .iter()
            .rev()
            .find(|event| matches!(event.payload, Payload::Frame { .. }))?;
        let Payload::Frame { inputs, .. } = frame.payload else {
            return None;
        };
        (inputs >= self.reflecting_counter()
            && self.last_input_at.is_none_or(|sent| frame.observed >= sent))
        .then_some(frame)
    }

    /// The `inputs` counter of the latest frame, or 0 before the first one.
    pub(super) fn latest_frame_inputs(&self) -> u64 {
        self.events
            .events()
            .iter()
            .rev()
            .find_map(|event| match event.payload {
                Payload::Frame { inputs, .. } => Some(inputs),
                _ => None,
            })
            .unwrap_or(0)
    }

    /// How many of the inputs sent the latest frame counts: its `inputs` counter less the
    /// program's own inputs (the baseline), or 0 before the first frame.
    pub(super) fn latest_frame_inputs_sent(&self) -> u64 {
        self.latest_frame_inputs()
            .saturating_sub(self.input_baseline.unwrap_or(0))
    }

    /// A summary of the events read so far, for failure messages.
    pub(super) fn events_summary(&self) -> String {
        let events = self.events.events();
        let latest: Vec<&str> = events
            .iter()
            .rev()
            .take(8)
            .map(|event| event.payload.kind_name())
            .collect();
        format!(
            "{} events were read; the latest were {:?}",
            events.len(),
            latest.into_iter().rev().collect::<Vec<_>>()
        )
    }

    /// How the program ended, for failure messages.
    pub(super) fn exit_summary(&self) -> String {
        self.session
            .exit()
            .map_or_else(|| "it is still running".to_owned(), |exit| exit.describe())
    }

    /// A failure of step `index`.
    pub(super) fn fail(
        &self,
        index: usize,
        cause: FailureCause,
        expected: impl Into<String>,
        detail: impl Into<String>,
    ) -> Stop {
        let description = self.scenario.steps.get(index).map_or_else(
            || "the checks after the last step".to_owned(),
            describe_step,
        );
        Stop::Fail(Box::new(StepFailure {
            index,
            description,
            cause,
            expected: expected.into(),
            detail: detail.into(),
            session_diagnostics: None,
        }))
    }

    /// The failure for a wait that did not end in success.
    pub(super) fn unmet(
        &self,
        index: usize,
        waited: &Waited<()>,
        expected: impl Into<String>,
        timeout_ms: u64,
        observed: impl FnOnce() -> String,
    ) -> Stop {
        let expected = expected.into();
        match waited {
            Waited::TimedOut => {
                let diagnostics = self.session.diagnostics();
                let sampled = self
                    .session
                    .sample_process()
                    .map(|text| format!("\n{text}"))
                    .unwrap_or_default();
                let stop = self.fail(
                    index,
                    FailureCause::Timeout,
                    expected,
                    format!(
                        "not within {timeout_ms} ms; {}\nsession: {diagnostics}{sampled}",
                        observed()
                    ),
                );
                attach_session_diagnostics(stop, diagnostics)
            }
            Waited::Exited => self.fail(
                index,
                FailureCause::ProcessExited,
                expected,
                format!(
                    "the program ended first ({}); {}",
                    self.exit_summary(),
                    observed()
                ),
            ),
            Waited::Ready(()) => self.fail(index, FailureCause::Mismatch, expected, observed()),
        }
    }

    fn execute(&mut self, index: usize, step: &Step) -> Result<(), Stop> {
        self.dispatch(index, step)
    }
}

/// Attaches `diagnostics` to a timed-out failure, so the failure document can carry them
/// structurally alongside the text `unmet` already folds into the failure's `detail`.
fn attach_session_diagnostics(mut stop: Stop, diagnostics: Diagnostics) -> Stop {
    if let Stop::Fail(failure) = &mut stop {
        failure.session_diagnostics = Some(diagnostics);
    }
    stop
}
