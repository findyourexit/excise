//! Semantic validation: the rules TOML typing cannot express.

use std::fmt;

use thiserror::Error;

use super::{
    Budget, Expect, MAX_IDENTIFIER_LEN, MAX_TIMEOUT_MS, MIN_TERMINAL_COLS, MIN_TERMINAL_ROWS,
    Profile, SCHEMA_VERSION, Scenario,
    path::{PathViolation, check_fixture_relative_path},
    step::{
        EventField, EventKind, ExpectFs, ExpectScreen, Idle, Marker, Region, Step, WaitEvent,
        WaitText,
    },
};
use crate::platform::PLATFORMS;

/// The longest slice id.
const MAX_SLICE_LEN: usize = 8;

/// A field of a scenario or of a step, with the index of the entry when the field is a list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Field {
    name: &'static str,
    index: Option<usize>,
}

impl Field {
    /// A scalar field.
    #[must_use]
    pub const fn new(name: &'static str) -> Self {
        Self { name, index: None }
    }

    /// One entry of a list field.
    #[must_use]
    pub const fn at(name: &'static str, index: usize) -> Self {
        Self {
            name,
            index: Some(index),
        }
    }
}

impl fmt::Display for Field {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.index {
            Some(index) => write!(formatter, "{}[{index}]", self.name),
            None => formatter.write_str(self.name),
        }
    }
}

/// A rule that a scenario as a whole breaks.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ValidationError {
    /// `schema_version` is not the version this crate reads.
    #[error("`schema_version` is {found}, but this harness reads only version {SCHEMA_VERSION}")]
    UnsupportedSchemaVersion {
        /// The version found.
        found: u32,
    },
    /// `name` or `fixture` is not a valid identifier.
    #[error(
        "`{field}` {value:?} is not an identifier: use 1 to {MAX_IDENTIFIER_LEN} lowercase ASCII \
         letters, digits, `-` or `_`, starting with a letter or digit"
    )]
    InvalidIdentifier {
        /// The field that holds the value.
        field: &'static str,
        /// The offending value.
        value: String,
    },
    /// `profiles` is empty.
    #[error("`profiles` must list at least one profile")]
    NoProfiles,
    /// `profiles` names a profile more than once.
    #[error("profile `{profile}` is listed more than once")]
    DuplicateProfile {
        /// The repeated profile.
        profile: Profile,
    },
    /// The initial terminal is smaller than the supported minimum.
    #[error(
        "terminal size {cols}x{rows} is below the {MIN_TERMINAL_COLS}x{MIN_TERMINAL_ROWS} minimum"
    )]
    TerminalTooSmall {
        /// The configured width.
        cols: u16,
        /// The configured height.
        rows: u16,
    },
    /// `expect` is `fail` but no `slice` says which slice will fix it.
    #[error("`expect = \"fail\"` requires `slice`")]
    ExpectedFailureWithoutSlice,
    /// `slice` is not a slice id.
    #[error(
        "`slice` {value:?} is not a slice id: use an uppercase letter followed by uppercase \
         letters or digits, at most {MAX_SLICE_LEN} in all"
    )]
    InvalidSlice {
        /// The offending value.
        value: String,
    },
    /// `fails_on` is set without `expect = "fail"`.
    #[error("`fails_on` requires `expect = \"fail\"`")]
    FailsOnWithoutExpectedFailure,
    /// `platforms` or `fails_on` is present but lists nothing.
    #[error("`{field}` is present but empty; omit it to use every platform")]
    EmptyPlatformList {
        /// `platforms` or `fails_on`.
        field: &'static str,
    },
    /// A platform name is not one this harness knows.
    #[error("`{field}` names the platform {value:?}, which is not `linux`, `macos`, or `windows`")]
    UnknownPlatform {
        /// The field and index of the offending entry.
        field: Field,
        /// The offending value.
        value: String,
    },
    /// `fails_on` names a platform outside `platforms`.
    #[error("`{field}` names the platform {value:?}, which `platforms` does not include")]
    FailsOnOutsidePlatforms {
        /// The field and index of the offending entry.
        field: Field,
        /// The offending value.
        value: String,
    },
    /// A platform name is listed more than once in `platforms` or in `fails_on`.
    #[error("`{field}` lists the platform {value:?} more than once")]
    DuplicatePlatform {
        /// The field and index of the second occurrence.
        field: Field,
        /// The offending value.
        value: String,
    },
    /// A budget override is negative, infinite, or not a number.
    #[error("budget `{budget}` needs a finite, non-negative limit")]
    InvalidBudgetLimit {
        /// The budget whose override is invalid.
        budget: Budget,
    },
    /// A sentinel path is not fixture-relative.
    #[error("`{field}` {path:?} is not a fixture-relative path: {violation}")]
    InvalidPath {
        /// The sentinel entry.
        field: Field,
        /// The offending path.
        path: String,
        /// The rule it breaks.
        violation: PathViolation,
    },
    /// The scenario deletes something but declares nothing that must survive.
    #[error("a scenario with a `delete` step must declare at least one sentinel")]
    DeleteWithoutSentinel,
    /// `steps` is empty.
    #[error("`steps` must contain at least one step")]
    NoSteps,
    /// A step breaks a rule.
    #[error("steps[{index}] ({kind}): {error}")]
    Step {
        /// The zero-based index of the step.
        index: usize,
        /// The name of the step.
        kind: &'static str,
        /// The rule it breaks.
        error: StepError,
    },
}

/// A rule that one step breaks.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum StepError {
    /// `timeout_ms` is zero or above the cap.
    #[error("`timeout_ms` is {timeout_ms}, but must be between 1 and {MAX_TIMEOUT_MS}")]
    TimeoutOutOfRange {
        /// The offending value.
        timeout_ms: u64,
    },
    /// A path is not fixture-relative.
    #[error("`{field}` {path:?} is not a fixture-relative path: {violation}")]
    InvalidPath {
        /// The field that holds the path.
        field: Field,
        /// The offending path.
        path: String,
        /// The rule it breaks.
        violation: PathViolation,
    },
    /// `wait_text` sets neither `text` nor `regex`.
    #[error("exactly one of `text` and `regex` is required, but neither is set")]
    MissingMatcher,
    /// `wait_text` sets both `text` and `regex`.
    #[error("exactly one of `text` and `regex` is required, but both are set")]
    ConflictingMatchers,
    /// A text, regular expression, or name is empty, and would match everything or nothing.
    #[error("`{field}` must not be empty")]
    EmptyValue {
        /// The empty field.
        field: Field,
    },
    /// An assertion lists nothing to check.
    #[error("at least one of {fields} must not be empty")]
    NothingToCheck {
        /// The fields that are all empty.
        fields: &'static str,
    },
    /// A row region ends before it starts.
    #[error("`region` rows are reversed: the first row {first} is after the last row {last}")]
    ReversedRows {
        /// The first row.
        first: u16,
        /// The last row.
        last: u16,
    },
    /// A resize has a zero dimension.
    #[error("resize to {cols}x{rows} has a zero dimension")]
    ZeroDimension {
        /// The new width.
        cols: u16,
        /// The new height.
        rows: u16,
    },
    /// A `wait_event` tests a field its event does not carry.
    #[error("event `{event}` has no field `{field}`")]
    UnknownEventField {
        /// The event waited for.
        event: EventKind,
        /// The field that does not belong to it.
        field: EventField,
    },
    /// A metric or measurement name is not a valid identifier.
    #[error(
        "`{field}` {value:?} is not an identifier: use 1 to {MAX_IDENTIFIER_LEN} lowercase ASCII \
         letters, digits, `-` or `_`, starting with a letter or digit"
    )]
    InvalidIdentifier {
        /// The field that holds the value.
        field: &'static str,
        /// The offending value.
        value: String,
    },
    /// A measurement starts a second time.
    #[error("measurement `{name}` starts more than once")]
    MeasureRestarted {
        /// The measurement name.
        name: String,
    },
    /// A measurement stops while it is not running.
    #[error("measurement `{name}` stops without a matching start")]
    MeasureStopWithoutStart {
        /// The measurement name.
        name: String,
    },
    /// A measurement starts and is never stopped.
    #[error("measurement `{name}` starts but never stops")]
    MeasureNeverStopped {
        /// The measurement name.
        name: String,
    },
    /// An `idle` duration field is zero or above the cap.
    #[error("`{field}` is {value}, but must be between 1 and {MAX_TIMEOUT_MS}")]
    DurationOutOfRange {
        /// The field that holds the value.
        field: &'static str,
        /// The offending value.
        value: u64,
    },
}

/// Every rule a scenario breaks, in scenario order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationErrors(Vec<ValidationError>);

impl ValidationErrors {
    /// The broken rules. Never empty.
    #[must_use]
    pub fn errors(&self) -> &[ValidationError] {
        &self.0
    }

    /// Takes the broken rules.
    #[must_use]
    pub fn into_errors(self) -> Vec<ValidationError> {
        self.0
    }
}

impl fmt::Display for ValidationErrors {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("scenario is invalid")?;
        for error in &self.0 {
            write!(formatter, "\n  - {error}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ValidationErrors {}

impl Scenario {
    /// Applies the semantic rules of the scenario format.
    ///
    /// The rules are: a supported `schema_version`; identifier-shaped `name` and `fixture`; a
    /// non-empty, duplicate-free `profiles`; a terminal of at least 32x8; at least one step;
    /// `slice` present whenever `expect = "fail"`; finite non-negative budget limits; every
    /// fixture-relative path relative and canonical (see
    /// [`check_fixture_relative_path`]); at least one sentinel whenever a step deletes; every
    /// `timeout_ms` between 1 and [`MAX_TIMEOUT_MS`]; and the per-step rules documented on
    /// [`StepError`].
    ///
    /// # Errors
    ///
    /// Returns every broken rule, not only the first.
    pub fn validate(&self) -> Result<(), ValidationErrors> {
        let mut errors = Vec::new();
        self.validate_header(&mut errors);
        self.validate_steps(&mut errors);
        if errors.is_empty() {
            Ok(())
        } else {
            Err(ValidationErrors(errors))
        }
    }

    fn validate_header(&self, errors: &mut Vec<ValidationError>) {
        if self.schema_version != SCHEMA_VERSION {
            errors.push(ValidationError::UnsupportedSchemaVersion {
                found: self.schema_version,
            });
        }
        for (field, value) in [("name", &self.name), ("fixture", &self.fixture)] {
            if !is_identifier(value) {
                errors.push(ValidationError::InvalidIdentifier {
                    field,
                    value: value.clone(),
                });
            }
        }
        if self.profiles.is_empty() {
            errors.push(ValidationError::NoProfiles);
        }
        for (index, &profile) in self.profiles.iter().enumerate() {
            if self.profiles[..index].contains(&profile) {
                errors.push(ValidationError::DuplicateProfile { profile });
            }
        }
        if self.terminal.cols < MIN_TERMINAL_COLS || self.terminal.rows < MIN_TERMINAL_ROWS {
            errors.push(ValidationError::TerminalTooSmall {
                cols: self.terminal.cols,
                rows: self.terminal.rows,
            });
        }
        if self.expect == Expect::Fail && self.slice.is_none() {
            errors.push(ValidationError::ExpectedFailureWithoutSlice);
        }
        if let Some(slice) = &self.slice
            && !is_slice_id(slice)
        {
            errors.push(ValidationError::InvalidSlice {
                value: slice.clone(),
            });
        }
        if let Some(platforms) = &self.platforms {
            check_platforms("platforms", platforms, errors);
        }
        if let Some(fails_on) = &self.fails_on {
            if self.expect != Expect::Fail {
                errors.push(ValidationError::FailsOnWithoutExpectedFailure);
            }
            check_platforms("fails_on", fails_on, errors);
            for (index, platform) in fails_on.iter().enumerate() {
                if PLATFORMS.contains(&platform.as_str()) && !self.runs_on(platform) {
                    errors.push(ValidationError::FailsOnOutsidePlatforms {
                        field: Field::at("fails_on", index),
                        value: platform.clone(),
                    });
                }
            }
        }
        for (&budget, &limit) in &self.budgets {
            if !limit.is_finite() || limit < 0.0 {
                errors.push(ValidationError::InvalidBudgetLimit { budget });
            }
        }
        for (index, path) in self.sentinels.iter().enumerate() {
            if let Err(violation) = check_fixture_relative_path(path) {
                errors.push(ValidationError::InvalidPath {
                    field: Field::at("sentinels", index),
                    path: path.clone(),
                    violation,
                });
            }
        }
        if self.sentinels.is_empty()
            && self
                .steps
                .iter()
                .any(|step| matches!(step, Step::Delete(_)))
        {
            errors.push(ValidationError::DeleteWithoutSentinel);
        }
    }

    fn validate_steps(&self, errors: &mut Vec<ValidationError>) {
        if self.steps.is_empty() {
            errors.push(ValidationError::NoSteps);
        }
        for (index, step) in self.steps.iter().enumerate() {
            for error in check_step(step) {
                errors.push(ValidationError::Step {
                    index,
                    kind: step.kind(),
                    error,
                });
            }
        }
        for (index, error) in check_measurements(&self.steps) {
            errors.push(ValidationError::Step {
                index,
                kind: "measure",
                error,
            });
        }
    }
}

/// Lowercase ASCII letters, digits, `-` and `_`, starting with a letter or digit.
fn is_identifier(value: &str) -> bool {
    let mut bytes = value.bytes();
    matches!(bytes.next(), Some(b'a'..=b'z' | b'0'..=b'9'))
        && value.len() <= MAX_IDENTIFIER_LEN
        && bytes.all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_'))
}

/// An uppercase letter followed by uppercase letters and digits, such as `X2` or `R10`.
fn is_slice_id(value: &str) -> bool {
    let mut bytes = value.bytes();
    matches!(bytes.next(), Some(b'A'..=b'Z'))
        && value.len() <= MAX_SLICE_LEN
        && bytes.all(|byte| matches!(byte, b'A'..=b'Z' | b'0'..=b'9'))
}

/// Checks a `platforms`-shaped list: rejects a present-and-empty list, a name this harness does
/// not know, and a name repeated later in the same list.
fn check_platforms(field: &'static str, platforms: &[String], errors: &mut Vec<ValidationError>) {
    if platforms.is_empty() {
        errors.push(ValidationError::EmptyPlatformList { field });
    }
    for (index, platform) in platforms.iter().enumerate() {
        if !PLATFORMS.contains(&platform.as_str()) {
            errors.push(ValidationError::UnknownPlatform {
                field: Field::at(field, index),
                value: platform.clone(),
            });
        } else if platforms[..index].contains(platform) {
            errors.push(ValidationError::DuplicatePlatform {
                field: Field::at(field, index),
                value: platform.clone(),
            });
        }
    }
}

fn check_step(step: &Step) -> Vec<StepError> {
    let mut errors = Vec::new();
    if let Some(timeout_ms) = step.timeout_ms()
        && !(1..=MAX_TIMEOUT_MS).contains(&timeout_ms)
    {
        errors.push(StepError::TimeoutOutOfRange { timeout_ms });
    }
    match step {
        Step::WaitText(step) => check_wait_text(step, &mut errors),
        Step::WaitEvent(step) => check_wait_event(step, &mut errors),
        Step::Select(step) => require_text(Field::new("name"), &step.name, &mut errors),
        Step::Delete(step) => require_text(Field::new("name"), &step.name, &mut errors),
        Step::Type(step) => require_text(Field::new("text"), &step.text, &mut errors),
        Step::WaitFsAbsent(step) | Step::WaitFsPresent(step) => {
            check_path(Field::new("path"), &step.path, &mut errors);
        }
        Step::FsMutate(step) => check_path(Field::new("path"), &step.path, &mut errors),
        Step::Resize(step) => {
            if step.cols == 0 || step.rows == 0 {
                errors.push(StepError::ZeroDimension {
                    cols: step.cols,
                    rows: step.rows,
                });
            }
        }
        Step::ExpectScreen(step) => check_expect_screen(step, &mut errors),
        Step::ExpectFs(step) => check_expect_fs(step, &mut errors),
        Step::ExpectBudget(step) => check_identifier("metric", &step.metric, &mut errors),
        Step::Measure(step) => check_identifier("name", &step.name, &mut errors),
        Step::Idle(step) => check_idle(step, &mut errors),
        Step::WaitHeader(_)
        | Step::Key(_)
        | Step::Signal(_)
        | Step::ExpectExit(_)
        | Step::Settle(_)
        | Step::Quit(_) => {}
    }
    errors
}

fn check_wait_text(step: &WaitText, errors: &mut Vec<StepError>) {
    match (&step.text, &step.regex) {
        (None, None) => errors.push(StepError::MissingMatcher),
        (Some(_), Some(_)) => errors.push(StepError::ConflictingMatchers),
        (Some(text), None) => require_text(Field::new("text"), text, errors),
        (None, Some(regex)) => require_text(Field::new("regex"), regex, errors),
    }
    if let Some(region) = step.region {
        check_region(region, errors);
    }
}

fn check_wait_event(step: &WaitEvent, errors: &mut Vec<StepError>) {
    for &field in step.fields.keys() {
        if !step.event.fields().contains(&field) {
            errors.push(StepError::UnknownEventField {
                event: step.event,
                field,
            });
        }
    }
}

fn check_idle(step: &Idle, errors: &mut Vec<StepError>) {
    for (field, value) in [("after_ms", step.after_ms), ("window_ms", step.window_ms)] {
        if !(1..=MAX_TIMEOUT_MS).contains(&value) {
            errors.push(StepError::DurationOutOfRange { field, value });
        }
    }
}

fn check_expect_screen(step: &ExpectScreen, errors: &mut Vec<StepError>) {
    if step.contains.is_empty() && step.not_contains.is_empty() && step.regex.is_empty() {
        errors.push(StepError::NothingToCheck {
            fields: "`contains`, `not_contains`, and `regex`",
        });
    }
    for (name, list) in [
        ("contains", &step.contains),
        ("not_contains", &step.not_contains),
        ("regex", &step.regex),
    ] {
        for (index, value) in list.iter().enumerate() {
            require_text(Field::at(name, index), value, errors);
        }
    }
    if let Some(region) = step.region {
        check_region(region, errors);
    }
}

fn check_expect_fs(step: &ExpectFs, errors: &mut Vec<StepError>) {
    if step.present.is_empty() && step.absent.is_empty() {
        errors.push(StepError::NothingToCheck {
            fields: "`present` and `absent`",
        });
    }
    for (name, list) in [("present", &step.present), ("absent", &step.absent)] {
        for (index, path) in list.iter().enumerate() {
            check_path(Field::at(name, index), path, errors);
        }
    }
}

fn check_region(region: Region, errors: &mut Vec<StepError>) {
    if let Region::Rows([first, last]) = region
        && first > last
    {
        errors.push(StepError::ReversedRows { first, last });
    }
}

fn require_text(field: Field, value: &str, errors: &mut Vec<StepError>) {
    if value.is_empty() {
        errors.push(StepError::EmptyValue { field });
    }
}

fn check_path(field: Field, path: &str, errors: &mut Vec<StepError>) {
    if let Err(violation) = check_fixture_relative_path(path) {
        errors.push(StepError::InvalidPath {
            field,
            path: path.to_owned(),
            violation,
        });
    }
}

fn check_identifier(field: &'static str, value: &str, errors: &mut Vec<StepError>) {
    if !is_identifier(value) {
        errors.push(StepError::InvalidIdentifier {
            field,
            value: value.to_owned(),
        });
    }
}

/// Pairs each `measure` start with a stop. Names are single-use, so a metric has one value.
fn check_measurements(steps: &[Step]) -> Vec<(usize, StepError)> {
    let mut errors = Vec::new();
    let mut started: Vec<&str> = Vec::new();
    let mut running: Vec<(&str, usize)> = Vec::new();
    for (index, step) in steps.iter().enumerate() {
        let Step::Measure(measure) = step else {
            continue;
        };
        let name = measure.name.as_str();
        match measure.marker {
            Marker::Start if started.contains(&name) => {
                errors.push((
                    index,
                    StepError::MeasureRestarted {
                        name: name.to_owned(),
                    },
                ));
            }
            Marker::Start => {
                started.push(name);
                running.push((name, index));
            }
            Marker::Stop => {
                if let Some(position) = running.iter().position(|&(open, _)| open == name) {
                    running.remove(position);
                } else {
                    errors.push((
                        index,
                        StepError::MeasureStopWithoutStart {
                            name: name.to_owned(),
                        },
                    ));
                }
            }
        }
    }
    for (name, index) in running {
        errors.push((
            index,
            StepError::MeasureNeverStopped {
                name: name.to_owned(),
            },
        ));
    }
    errors
}
