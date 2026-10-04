//! The step executor: the state of one scenario run and the primitives every step is built from.
//!
//! An [`Executor`] owns the running program (a [`Live`]), the measurements of the run, and the
//! bookkeeping the final checks need. How waits, inputs, and frames work is described in the
//! [`live`](super::live) module, which the executor shares with the interactive driver; steps (in
//! the `steps` module) never sleep and never look at output bytes. They wait through
//! [`Drive::wait_until`], which drives the session and the event log and evaluates a condition
//! after every bit of progress.

use std::{
    collections::BTreeMap,
    path::PathBuf,
    time::{Duration, Instant},
};

use crate::{
    metrics::{Recorder, StoreSampler},
    pty::{Diagnostics, PtySession},
    report::TimingWarning,
    safety::{FixtureRoot, FixtureSnapshot, Scratch},
    scenario::{Scenario, Step},
};

use super::{
    budget::LatencyScale,
    live::{Drive, Live, ProtocolError, Sent, Unmet, Waited},
    outcome::{FailureCause, RunError, StepFailure, Stop},
    plan::{Prepared, describe_step},
};

/// How often a wait for a file system condition looks at the file system.
pub(super) const FS_POLL_INTERVAL: Duration = Duration::from_millis(10);
/// How often the run's scan-store directory is sampled for its peak size.
pub(super) const STORE_SAMPLE_INTERVAL: Duration = Duration::from_millis(50);

pub(crate) struct Executor<'a> {
    pub(super) scenario: &'a Scenario,
    pub(super) prepared: &'a [Prepared],
    pub(super) fixture: &'a FixtureRoot,
    pub(super) scratch: &'a Scratch,
    pub(super) baseline: &'a FixtureSnapshot,
    /// The program, its event log, and the input accounting.
    pub(crate) live: Live,
    pub(super) recorder: Recorder,
    /// The factor the latency budgets are multiplied by in this run. Strict until the caller of
    /// the run sets it, as the tests set the frame window of [`Live`].
    pub(super) latency_scale: LatencyScale,
    /// Whether a latency budget that `expect_budget` finds missed is recorded in
    /// `timing_warnings` and passes, instead of failing the step. Off until the caller of the
    /// run sets it, like `latency_scale`.
    pub(super) timing_informational: bool,
    /// The latency budgets that `expect_budget` found missed and passed, in step order.
    pub(super) timing_warnings: Vec<TimingWarning>,
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
        let started = session.started();
        Self {
            scenario,
            prepared,
            fixture,
            scratch,
            baseline,
            live: Live::new(session, scratch.events()),
            recorder,
            latency_scale: LatencyScale::STRICT,
            timing_informational: false,
            timing_warnings: Vec::new(),
            intended_deletions: Vec::new(),
            intended_mutations: Vec::new(),
            residue_files: None,
            store_dir,
            store_sampler: StoreSampler::default(),
            last_store_sample: started
                .checked_sub(STORE_SAMPLE_INTERVAL)
                .unwrap_or(started),
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
        let session = &self.live.session;
        let mut metrics = self.recorder.finish(
            self.live.events.events(),
            session.output_bytes(),
            session.sampler(),
            self.store_sampler.peak_bytes(),
            session.cpu_time(),
            Instant::now(),
        );
        if let Some(files) = self.residue_files {
            #[allow(clippy::cast_precision_loss)]
            metrics.insert("residue_files".to_owned(), files as f64);
        }
        if let Some(bytes) = session.cgroup_memory_peak_bytes() {
            #[allow(
                clippy::cast_precision_loss,
                reason = "byte counts far below 2^52, like every other byte metric in this crate"
            )]
            metrics.insert("cgroup_memory_peak_bytes".to_owned(), bytes as f64);
        }
        metrics
    }

    /// The deadline of a step whose bound is `timeout_ms`.
    pub(super) fn deadline(timeout_ms: u64) -> Instant {
        Instant::now() + Duration::from_millis(timeout_ms)
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
                let session = &self.live.session;
                let diagnostics = session.diagnostics();
                let sampled = session
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
                    self.live.exit_summary(),
                    observed()
                ),
            ),
            Waited::Ready(()) => self.fail(index, FailureCause::Mismatch, expected, observed()),
        }
    }

    /// The failure of step `index` for a protocol that did not complete: the same failure the
    /// step would have reported had it done the waiting itself.
    pub(super) fn protocol_stop(
        &self,
        index: usize,
        timeout_ms: u64,
        error: ProtocolError,
    ) -> Stop {
        match error {
            ProtocolError::Run(error) => Stop::Error(error),
            ProtocolError::Unmet(Unmet {
                waited: Waited::Ready(()),
                cause,
                expected,
                observed,
            }) => self.fail(index, cause, expected, observed),
            ProtocolError::Unmet(Unmet {
                waited,
                expected,
                observed,
                ..
            }) => self.unmet(index, &waited, expected, timeout_ms, || observed),
        }
    }

    fn execute(&mut self, index: usize, step: &Step) -> Result<(), Stop> {
        self.dispatch(index, step)
    }
}

impl Drive for Executor<'_> {
    fn live(&self) -> &Live {
        &self.live
    }

    fn live_mut(&mut self) -> &mut Live {
        &mut self.live
    }

    /// Reads all pending terminal output and events, and samples the scan-store directory.
    fn pump(&mut self) -> Result<(), RunError> {
        self.live.session.pump()?;
        if self.last_store_sample.elapsed() >= STORE_SAMPLE_INTERVAL {
            self.last_store_sample = Instant::now();
            self.store_sampler.sample(&self.store_dir);
        }
        self.live.poll_events()?;
        if let Some(exit) = self.live.session.exit() {
            self.recorder.record_exit(exit.at);
        }
        Ok(())
    }

    fn input_sent(&mut self, sent: &Sent) {
        self.recorder
            .record_input(sent.at, sent.number, sent.reflecting, sent.isolated);
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
