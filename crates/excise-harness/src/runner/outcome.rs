//! What a run can end in: a passed scenario, a failed step, or a harness error.

use std::io;

use thiserror::Error;

use crate::{
    events::EventError,
    pty::PtyError,
    safety::{SafetyError, ScratchError, SnapshotError},
    scenario::ValidationErrors,
};

/// Why a step failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureCause {
    /// A bounded wait elapsed before its condition held.
    Timeout,
    /// The program showed or did something other than what the step expected.
    Mismatch,
    /// The program ended before the step's condition held.
    ProcessExited,
    /// A `delete` step refused to confirm, and never sent `y`.
    DeleteRefused,
}

/// The step at which a scenario failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepFailure {
    /// The zero-based index of the step; one past the last step for the final checks.
    pub index: usize,
    /// A human-readable description of the step.
    pub description: String,
    /// Why it failed.
    pub cause: FailureCause,
    /// What the step expected, rendered as text.
    pub expected: String,
    /// What was found instead.
    pub detail: String,
}

impl std::fmt::Display for StepFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "step {} ({}): {}",
            self.index, self.description, self.detail
        )
    }
}

/// The harness could not run the scenario, which is different from the scenario failing.
///
/// A run that ends in one of these has the verdict `error`, and it fails the run: nothing was
/// learned about `excise`.
#[derive(Debug, Error)]
pub enum RunError {
    /// The scenario breaks a rule of the scenario format.
    #[error("{0}")]
    InvalidScenario(ValidationErrors),
    /// A pattern in the scenario is not a valid regular expression.
    #[error("steps[{index}]: `{pattern}` is not a valid regular expression: {reason}")]
    InvalidPattern {
        /// The step.
        index: usize,
        /// The pattern.
        pattern: String,
        /// Why it does not compile.
        reason: String,
    },
    /// A step cannot be performed: a key without a terminal encoding, or a name `select` cannot
    /// type.
    #[error("steps[{index}] ({step}): {reason}")]
    InvalidStep {
        /// The step.
        index: usize,
        /// The step's kind.
        step: &'static str,
        /// Why it cannot be performed.
        reason: String,
    },
    /// This runner cannot perform the step. A scenario that needs it must not be reported as a
    /// pass.
    #[error("steps[{index}] ({step}) is not supported by the pseudo-terminal runner: {reason}")]
    Unsupported {
        /// The step.
        index: usize,
        /// The step's kind.
        step: &'static str,
        /// Why it is unsupported.
        reason: String,
    },
    /// The fixture is not a harness fixture, or a path cannot be resolved inside it.
    #[error(transparent)]
    Safety(#[from] SafetyError),
    /// The scratch area could not be created or read.
    #[error(transparent)]
    Scratch(#[from] ScratchError),
    /// The fixture could not be walked.
    #[error(transparent)]
    Snapshot(#[from] SnapshotError),
    /// The pseudo-terminal session failed.
    #[error(transparent)]
    Pty(#[from] PtyError),
    /// The event channel could not be read or is not protocol version 1.
    #[error(transparent)]
    Events(#[from] EventError),
    /// The binary under test cannot be used.
    #[error("cannot use the binary `{}`: {reason}", path.display())]
    Binary {
        /// The path that was given.
        path: std::path::PathBuf,
        /// Why not.
        reason: String,
    },
    /// The event channel does not belong to the process that was started.
    #[error("event channel error: {0}")]
    Protocol(String),
    /// A signal could not be delivered.
    #[error(transparent)]
    Signal(#[from] crate::safety::SignalError),
    /// Another operating-system call failed.
    #[error("{context}: {source}")]
    Io {
        /// What was being done.
        context: &'static str,
        /// The underlying error.
        source: io::Error,
    },
}

/// How a step stopped the run.
#[derive(Debug)]
pub(crate) enum Stop {
    /// The scenario failed at this step.
    Fail(Box<StepFailure>),
    /// The harness could not go on.
    Error(RunError),
}

impl<E: Into<RunError>> From<E> for Stop {
    fn from(error: E) -> Self {
        Self::Error(error.into())
    }
}
