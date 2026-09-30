//! Checking a scenario before it runs.
//!
//! A scenario is refused, before anything starts, when it is invalid, when it contains a step the
//! in-process runner cannot perform, or when a regular expression in it does not compile. What is
//! left is a [`Plan`]: the steps, each with its description and compiled patterns.

use std::fmt::Write as _;

use excise_harness::fixture::OwnershipError;
use excise_harness::scenario::{Region, Scenario, Step, ValidationErrors};
use regex::Regex;
use thiserror::Error;

/// A step the in-process runner cannot perform.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnsupportedStep {
    /// The zero-based index of the step.
    pub index: usize,
    /// The step's name in the scenario file.
    pub kind: &'static str,
    /// Why the in-process runner cannot perform it.
    pub reason: &'static str,
}

/// Why a scenario was not run.
#[derive(Debug, Error)]
pub enum RunError {
    /// The scenario breaks a rule of the scenario format.
    #[error("{0}")]
    Invalid(#[from] ValidationErrors),
    /// The scenario contains steps that only another runner can perform.
    #[error("{}", unsupported_message(steps))]
    Unsupported { steps: Vec<UnsupportedStep> },
    /// A regular expression in the scenario does not compile.
    #[error(
        "step {step} (`{kind}`): the pattern {pattern:?} is not a valid regular expression: {message}"
    )]
    InvalidRegex {
        step: usize,
        kind: &'static str,
        pattern: String,
        message: String,
    },
    /// The root is not a harness fixture, so the runner will not touch it.
    #[error("refusing to run: {0}")]
    UnownedRoot(#[from] OwnershipError),
    /// The run could not be set up.
    #[error("could not set up the run: {0}")]
    Setup(String),
}

fn unsupported_message(steps: &[UnsupportedStep]) -> String {
    let mut message = String::from("the in-process runner cannot run this scenario:");
    for step in steps {
        let _ = write!(
            message,
            "\n  - step {} (`{}`): {}",
            step.index, step.kind, step.reason
        );
    }
    message
}

/// A step ready to run.
pub struct PlannedStep {
    pub step: Step,
    /// What the step does, for failure reports.
    pub description: String,
    /// The compiled patterns of a `wait_text` or `expect_screen` step, in the order the scenario
    /// lists them.
    pub regexes: Vec<Regex>,
}

/// A scenario that passed every check.
pub struct Plan {
    pub sentinels: Vec<String>,
    pub steps: Vec<PlannedStep>,
}

/// The steps of `scenario` that the in-process runner cannot perform.
///
/// * `signal` needs a process to receive it.
/// * `wait_event` needs the event channel, which belongs to a process.
/// * `expect_budget` and `measure` judge timing and resources; the barrier drains the owner loop
///   outside the production scheduling path, so in-process timing means nothing.
/// * `expect_exit` with `terminal_restored = false` asserts a terminal that was left unrestored,
///   which an in-process run cannot produce. `terminal_restored = true` is accepted and not
///   evaluated: there is no terminal to inspect.
/// * Nothing can follow `expect_exit`: the run has ended.
/// * Typed text must be printable; control characters mean different things in different
///   terminals.
pub fn unsupported_steps(scenario: &Scenario) -> Vec<UnsupportedStep> {
    let last = scenario.steps.len().saturating_sub(1);
    let mut unsupported = Vec::new();
    for (index, step) in scenario.steps.iter().enumerate() {
        let reason = match step {
            Step::Signal(_) => Some("signals need a separate process to receive them"),
            Step::WaitEvent(_) => {
                Some("the event channel belongs to a separate process; an in-process run has none")
            }
            Step::ExpectBudget(_) | Step::Measure(_) => Some(
                "timing and resources are never judged in-process: the barrier drains the owner \
                 loop outside the production scheduling path",
            ),
            Step::ExpectExit(exit) if !exit.terminal_restored => {
                Some("an in-process run cannot leave the terminal unrestored")
            }
            Step::ExpectExit(_) if index != last => {
                Some("nothing can follow `expect_exit`: the run has ended")
            }
            Step::Type(typed) if typed.text.chars().any(char::is_control) => {
                Some("typed text must be printable")
            }
            _ => None,
        };
        if let Some(reason) = reason {
            unsupported.push(UnsupportedStep {
                index,
                kind: step.kind(),
                reason,
            });
        }
    }
    unsupported
}

/// Checks `scenario` and prepares it to run.
///
/// # Errors
///
/// Returns why the scenario cannot run: it is invalid, it has unsupported steps, or a pattern in
/// it does not compile.
pub fn plan(scenario: &Scenario) -> Result<Plan, RunError> {
    scenario.validate()?;
    let unsupported = unsupported_steps(scenario);
    if !unsupported.is_empty() {
        return Err(RunError::Unsupported { steps: unsupported });
    }
    let mut steps = Vec::with_capacity(scenario.steps.len());
    for (index, step) in scenario.steps.iter().enumerate() {
        steps.push(PlannedStep {
            step: step.clone(),
            description: describe(step),
            regexes: compile_patterns(index, step)?,
        });
    }
    Ok(Plan {
        sentinels: scenario.sentinels.clone(),
        steps,
    })
}

fn compile_patterns(index: usize, step: &Step) -> Result<Vec<Regex>, RunError> {
    let patterns: &[String] = match step {
        Step::WaitText(wait) => wait.regex.as_slice(),
        Step::ExpectScreen(expect) => &expect.regex,
        _ => &[],
    };
    patterns
        .iter()
        .map(|pattern| {
            Regex::new(pattern).map_err(|error| RunError::InvalidRegex {
                step: index,
                kind: step.kind(),
                pattern: pattern.clone(),
                message: error.to_string(),
            })
        })
        .collect()
}

/// A one-line description of what a step does.
pub fn describe(step: &Step) -> String {
    match step {
        Step::WaitText(wait) => {
            let matcher = match (&wait.text, &wait.regex) {
                (Some(text), _) => format!("{text:?}"),
                (None, Some(regex)) => format!("/{regex}/"),
                (None, None) => String::new(),
            };
            format!("wait_text {matcher}{}", in_region(wait.region))
        }
        Step::WaitHeader(wait) => format!("wait_header {}", wait.state),
        Step::WaitEvent(wait) => format!("wait_event {}", wait.event),
        Step::Key(press) => {
            let ctrl = if press.ctrl { "ctrl+" } else { "" };
            let alt = if press.alt { "alt+" } else { "" };
            format!("key {alt}{ctrl}{}", press.key)
        }
        Step::Type(typed) => format!("type {:?}", typed.text),
        Step::Select(select) => format!("select {:?}", select.name),
        Step::Delete(delete) => format!("delete {:?} ({})", delete.name, delete.kind),
        Step::WaitFsAbsent(wait) => format!("wait_fs_absent {:?}", wait.path),
        Step::WaitFsPresent(wait) => format!("wait_fs_present {:?}", wait.path),
        Step::FsMutate(mutate) => format!("fs_mutate {} {:?}", mutate.op, mutate.path),
        Step::Resize(resize) => format!("resize {}x{}", resize.cols, resize.rows),
        Step::Signal(signal) => format!("signal {}", signal.signal),
        Step::ExpectScreen(expect) => format!("expect_screen{}", in_region(expect.region)),
        Step::ExpectFs(_) => "expect_fs".to_owned(),
        Step::ExpectExit(exit) => format!("expect_exit code {}", exit.code),
        Step::ExpectBudget(budget) => format!("expect_budget {}", budget.budget),
        Step::Measure(measure) => format!("measure {} {}", measure.name, measure.marker),
        Step::Settle(_) => "settle".to_owned(),
        Step::Quit(_) => "quit".to_owned(),
    }
}

fn in_region(region: Option<Region>) -> String {
    match region {
        None => String::new(),
        Some(Region::Header) => " in the header".to_owned(),
        Some(Region::Dialog) => " in the dialog".to_owned(),
        Some(Region::Rows([first, last])) => format!(" in rows {first} to {last}"),
    }
}
