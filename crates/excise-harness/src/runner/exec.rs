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
//! program had consumed when it drew that frame. The executor counts every input event it writes.
//! A frame *reflects* the inputs sent so far when its counter is at least that count and it was
//! observed after the last input was written. Waiting for such a frame is what `settle` means, and
//! it is how a step knows that the screen it is about to read shows the effect of a key and not the
//! moment before it.

use std::{
    collections::BTreeMap,
    path::PathBuf,
    time::{Duration, Instant},
};

use crate::{
    events::{Event, EventLog, Payload},
    metrics::{Recorder, StoreSampler},
    pty::PtySession,
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
        self.inputs_sent += 1;
        let at = self.session.send(bytes)?;
        self.last_input_at = Some(at);
        self.recorder.record_input(at, self.inputs_sent, isolated);
        Ok(at)
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
        (inputs >= self.inputs_sent && self.last_input_at.is_none_or(|sent| frame.observed >= sent))
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
            Waited::TimedOut => self.fail(
                index,
                FailureCause::Timeout,
                expected,
                format!(
                    "not within {timeout_ms} ms; {}\nsession: {}",
                    observed(),
                    self.session.diagnostics()
                ),
            ),
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
