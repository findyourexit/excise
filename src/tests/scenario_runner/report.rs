//! What a run reports: the verdict, and for a failure the step, the reason, and the screen.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use crossterm::event::KeyEvent;
use excise_harness::report::{FailedStep, ScenarioResult, ScreenComparison, Verdict};
use excise_harness::scenario::{Expect, Profile};

/// The step a run failed at and what it saw there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepFailure {
    /// The step's index and description.
    pub step: FailedStep,
    /// Why the step failed, precisely.
    pub message: String,
    /// The expectation that failed and the whole screen at that moment.
    pub screen: ScreenComparison,
}

impl fmt::Display for StepFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "step {} (`{}`) failed: {}\nexpected: {}\nscreen:\n{}",
            self.step.index,
            self.step.description,
            self.message,
            self.screen.expected,
            self.screen.actual,
        )
    }
}

/// The outcome of one scenario under one profile.
#[derive(Clone, Debug)]
pub struct ScenarioRun {
    pub name: String,
    pub profile: Profile,
    /// Whether the scenario is expected to pass or, as a documented defect, to fail.
    pub expect: Expect,
    pub duration: Duration,
    /// The step that failed, or `None` when the scenario passed.
    pub failure: Option<StepFailure>,
    /// The key events the runner delivered to the program, in order.
    pub sent_keys: Vec<KeyEvent>,
}

impl ScenarioRun {
    pub fn passed(&self) -> bool {
        self.failure.is_none()
    }

    /// The verdict under strict xfail: an expected failure that passes is an `xpass`, which
    /// blocks the run.
    pub fn verdict(&self) -> Verdict {
        Verdict::resolve(self.expect, self.passed())
    }

    /// The run as a row of a harness summary. In-process runs record no metrics and write no
    /// failure bundle.
    pub fn result(&self) -> ScenarioResult {
        ScenarioResult {
            name: self.name.clone(),
            profile: self.profile,
            verdict: self.verdict(),
            duration_ms: u64::try_from(self.duration.as_millis()).unwrap_or(u64::MAX),
            metrics: BTreeMap::new(),
            failure_bundle: None,
        }
    }
}

impl fmt::Display for ScenarioRun {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "scenario `{}` under profile `{}`: {}",
            self.name,
            self.profile,
            self.verdict()
        )?;
        if let Some(failure) = &self.failure {
            write!(formatter, "\n{failure}")?;
        }
        Ok(())
    }
}
