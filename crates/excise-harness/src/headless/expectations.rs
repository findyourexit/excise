//! Expected failures of the headless suite: strict xfail for fixtures that fail the oracle diff,
//! whose `headless_scan_ratio` misses the scan-time budget, or whose peak memory misses its
//! budget, for a known reason.
//!
//! A fixture can fail the oracle diff because of a defect that is known and not yet fixed, its
//! ratio can miss the budget (`crate::headless::suite::RATIO_BUDGET`) for the same reason, or its
//! peak memory can exceed the budget the memory gate checks it against. Listing it in
//! `expectations/headless.toml` turns that into a strict expected failure, with the semantics the
//! scenario model gives `expect = "fail"`:
//!
//! | Listed | The run | Verdict | Fails the run |
//! |---|---|---|---|
//! | no | clean, or within budget | `pass` | no |
//! | no | not clean, or over budget | `fail` | yes |
//! | yes | fails the way it is written down | `xfail` | no |
//! | yes | clean, or within budget | `xpass` | **yes** |
//! | yes | fails a different way | `fail` | yes |
//!
//! So an expected failure must fail in the way it was written down: a different failure is a new
//! defect, and no failure means the defect is fixed and the entry has to go. An entry applies only
//! on the platforms it names; elsewhere the fixture is held to the ordinary rule like any other.
//!
//! The three tables are independent. `[[expect_fail]]` is the oracle diff; the kinds are those of
//! [`DiscrepancyKind`], named exactly. `[[expect_ratio_fail]]` is the ratio budget and
//! `[[expect_memory_fail]]` is the peak-memory budget (`crate::headless::suite::FixtureReport::
//! memory_verdict`); neither has `kinds`, because there is only one way to miss either. The ratio
//! is judged only where it is gated (`crate::headless::suite::FixtureReport::ratio_is_gated`, a
//! deterministic oracle-entry-count threshold, never a measured time): a gated fixture over budget
//! with no entry for this platform fails the run, exactly like an undocumented oracle-diff
//! discrepancy. Each entry names the findings it documents (`F1`, `F10`), so that the change that
//! fixes one finds every entry that waits for it.

use std::{collections::BTreeSet, fmt};

use serde::Deserialize;
use thiserror::Error;

use super::diff::DiscrepancyKind;
use crate::platform::PLATFORMS;

/// The only version of the expectations file this module reads.
pub const EXPECTATIONS_VERSION: u32 = 1;

/// The expectations this crate ships.
const BUNDLED: &str = include_str!("../../expectations/headless.toml");

/// A fixture that is expected to fail the oracle diff.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpectedFailure {
    /// The fixture's id.
    pub fixture: String,
    /// The operating systems the failure shows on, as `std::env::consts::OS` spells them.
    pub platforms: Vec<String>,
    /// The findings the failure documents, each an uppercase letter and up to seven uppercase
    /// letters or digits (`F10`).
    pub findings: Vec<String>,
    /// The kinds of discrepancy the runs must show: exactly these.
    pub kinds: BTreeSet<DiscrepancyKind>,
    /// What is wrong, in a sentence or two.
    pub reason: String,
}

impl ExpectedFailure {
    /// Whether the entry applies on the operating system this is running on.
    #[must_use]
    pub fn applies_here(&self) -> bool {
        self.platforms
            .iter()
            .any(|platform| platform == std::env::consts::OS)
    }
}

/// A fixture whose `headless_scan_ratio` is expected to miss the budget today.
///
/// Unlike [`ExpectedFailure`] there is no `kinds` field: there is only one way to miss the ratio
/// budget, so an entry applies wherever the fixture is gated (its `du` time is precise enough to
/// judge) and the measured median ratio is over budget.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RatioExpectedFailure {
    /// The fixture's id.
    pub fixture: String,
    /// The operating systems the failure shows on, as `std::env::consts::OS` spells them.
    pub platforms: Vec<String>,
    /// The findings the failure documents, each an uppercase letter and up to seven uppercase
    /// letters or digits (`F1`).
    pub findings: Vec<String>,
    /// What is wrong, in a sentence or two.
    pub reason: String,
}

impl RatioExpectedFailure {
    /// Whether the entry applies on the operating system this is running on.
    #[must_use]
    pub fn applies_here(&self) -> bool {
        self.platforms
            .iter()
            .any(|platform| platform == std::env::consts::OS)
    }
}

/// A fixture whose peak memory is expected to exceed the budget the memory gate checks it
/// against, strictly (see the module documentation).
///
/// Unlike [`ExpectedFailure`] there is no `kinds` field: there is only one way to miss the memory
/// budget.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpectedMemoryFailure {
    /// The fixture's id.
    pub fixture: String,
    /// The operating systems the failure shows on, as `std::env::consts::OS` spells them.
    pub platforms: Vec<String>,
    /// The findings the failure documents, each an uppercase letter and up to seven uppercase
    /// letters or digits (`F10`).
    pub findings: Vec<String>,
    /// What is wrong, in a sentence or two.
    pub reason: String,
}

impl ExpectedMemoryFailure {
    /// Whether the entry applies on the operating system this is running on.
    #[must_use]
    pub fn applies_here(&self) -> bool {
        self.platforms
            .iter()
            .any(|platform| platform == std::env::consts::OS)
    }
}

/// The expectations file could not be used.
#[derive(Debug, Error)]
pub enum ExpectationError {
    /// The text is not a valid expectations file.
    #[error("the expectations are not valid: {0}")]
    Parse(#[from] toml::de::Error),
    /// The file parses but breaks a rule.
    #[error("the expectations break {} rule(s): {}", problems.len(), problems.join("; "))]
    Invalid {
        /// Every rule it breaks.
        problems: Vec<String>,
    },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    schema_version: u32,
    #[serde(default)]
    expect_fail: Vec<ExpectedFailure>,
    #[serde(default)]
    expect_ratio_fail: Vec<RatioExpectedFailure>,
    #[serde(default)]
    expect_memory_fail: Vec<ExpectedMemoryFailure>,
}

/// The fixtures that are expected to fail, and why.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Expectations {
    failures: Vec<ExpectedFailure>,
    ratio_failures: Vec<RatioExpectedFailure>,
    memory_failures: Vec<ExpectedMemoryFailure>,
}

impl Expectations {
    /// No expectations: every fixture must have a clean diff, a ratio within budget wherever it
    /// is gated, and peak memory within its budget.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            failures: Vec::new(),
            ratio_failures: Vec::new(),
            memory_failures: Vec::new(),
        }
    }

    /// The expectations this crate ships, `expectations/headless.toml`.
    ///
    /// # Errors
    ///
    /// Returns [`ExpectationError`] when the shipped file is not valid.
    pub fn bundled() -> Result<Self, ExpectationError> {
        Self::from_toml_str(BUNDLED)
    }

    /// Parses and validates an expectations file.
    ///
    /// # Errors
    ///
    /// Returns [`ExpectationError::Parse`] for text that is not an expectations file (unknown
    /// fields and kinds included) and [`ExpectationError::Invalid`] for one that breaks a rule.
    pub fn from_toml_str(text: &str) -> Result<Self, ExpectationError> {
        let file: File = toml::from_str(text)?;
        let mut problems = Vec::new();
        if file.schema_version != EXPECTATIONS_VERSION {
            problems.push(format!(
                "schema_version is {}, this build reads only {EXPECTATIONS_VERSION}",
                file.schema_version
            ));
        }
        let mut seen = BTreeSet::new();
        for entry in &file.expect_fail {
            let fixture = &entry.fixture;
            if !seen.insert(fixture.as_str()) {
                problems.push(format!("`{fixture}` is listed twice in `[[expect_fail]]`"));
            }
            check_platforms(&mut problems, fixture, "expect_fail", &entry.platforms);
            check_findings(&mut problems, fixture, "expect_fail", &entry.findings);
            if entry.kinds.is_empty() {
                problems.push(format!("`{fixture}` lists no discrepancy kind"));
            }
            if entry.reason.trim().is_empty() {
                problems.push(format!("`{fixture}` has no reason"));
            }
        }
        let mut seen_ratio = BTreeSet::new();
        for entry in &file.expect_ratio_fail {
            let fixture = &entry.fixture;
            if !seen_ratio.insert(fixture.as_str()) {
                problems.push(format!(
                    "`{fixture}` is listed twice in `[[expect_ratio_fail]]`"
                ));
            }
            check_platforms(
                &mut problems,
                fixture,
                "expect_ratio_fail",
                &entry.platforms,
            );
            check_findings(&mut problems, fixture, "expect_ratio_fail", &entry.findings);
            if entry.reason.trim().is_empty() {
                problems.push(format!("`{fixture}` has no reason"));
            }
        }
        let mut seen_memory = BTreeSet::new();
        for entry in &file.expect_memory_fail {
            let fixture = &entry.fixture;
            if !seen_memory.insert(fixture.as_str()) {
                problems.push(format!(
                    "`{fixture}` is listed twice in `[[expect_memory_fail]]`"
                ));
            }
            check_platforms(
                &mut problems,
                fixture,
                "expect_memory_fail",
                &entry.platforms,
            );
            check_findings(
                &mut problems,
                fixture,
                "expect_memory_fail",
                &entry.findings,
            );
            if entry.reason.trim().is_empty() {
                problems.push(format!("`{fixture}` has no reason"));
            }
        }
        if problems.is_empty() {
            Ok(Self {
                failures: file.expect_fail,
                ratio_failures: file.expect_ratio_fail,
                memory_failures: file.expect_memory_fail,
            })
        } else {
            Err(ExpectationError::Invalid { problems })
        }
    }

    /// Every oracle-diff entry, whether or not it applies on this platform.
    #[must_use]
    pub fn failures(&self) -> &[ExpectedFailure] {
        &self.failures
    }

    /// Every ratio-budget entry, whether or not it applies on this platform.
    #[must_use]
    pub fn ratio_failures(&self) -> &[RatioExpectedFailure] {
        &self.ratio_failures
    }

    /// Every memory-budget entry, whether or not it applies on this platform.
    #[must_use]
    pub fn memory_failures(&self) -> &[ExpectedMemoryFailure] {
        &self.memory_failures
    }

    /// The oracle-diff entry for `fixture` that applies on this platform.
    #[must_use]
    pub fn expected_failure(&self, fixture: &str) -> Option<&ExpectedFailure> {
        self.failures
            .iter()
            .find(|entry| entry.fixture == fixture && entry.applies_here())
    }

    /// The ratio-budget entry for `fixture` that applies on this platform.
    #[must_use]
    pub fn expected_ratio_failure(&self, fixture: &str) -> Option<&RatioExpectedFailure> {
        self.ratio_failures
            .iter()
            .find(|entry| entry.fixture == fixture && entry.applies_here())
    }

    /// The memory-budget entry for `fixture` that applies on this platform.
    #[must_use]
    pub fn expected_memory_failure(&self, fixture: &str) -> Option<&ExpectedMemoryFailure> {
        self.memory_failures
            .iter()
            .find(|entry| entry.fixture == fixture && entry.applies_here())
    }
}

/// Checks a `platforms`-shaped list: rejects an empty list and a name this harness does not know.
fn check_platforms(problems: &mut Vec<String>, fixture: &str, table: &str, platforms: &[String]) {
    if platforms.is_empty() {
        problems.push(format!("`{fixture}` in `[[{table}]]` names no platform"));
    }
    for platform in platforms {
        if !PLATFORMS.contains(&platform.as_str()) {
            problems.push(format!(
                "`{fixture}` in `[[{table}]]` names the platform `{platform}`; the platforms are {}",
                PLATFORMS.join(", ")
            ));
        }
    }
}

/// Checks a `findings`-shaped list: rejects an empty list and a name that is not a finding id.
fn check_findings(problems: &mut Vec<String>, fixture: &str, table: &str, findings: &[String]) {
    if findings.is_empty() {
        problems.push(format!("`{fixture}` in `[[{table}]]` names no finding"));
    }
    for finding in findings {
        if !is_finding_id(finding) {
            problems.push(format!(
                "`{fixture}` in `[[{table}]]` names the finding `{finding}`, which is not an \
                 uppercase letter and up to seven uppercase letters or digits"
            ));
        }
    }
}

fn is_finding_id(text: &str) -> bool {
    let mut characters = text.chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_uppercase())
        && text.len() <= 8
        && characters.all(|rest| rest.is_ascii_uppercase() || rest.is_ascii_digit())
}

impl fmt::Display for ExpectedFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} ({})", self.findings.join(", "), self.reason)
    }
}

impl fmt::Display for RatioExpectedFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} ({})", self.findings.join(", "), self.reason)
    }
}

impl fmt::Display for ExpectedMemoryFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} ({})", self.findings.join(", "), self.reason)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = "schema_version = 1\n";

    #[test]
    fn the_bundled_file_parses_and_validates() {
        Expectations::bundled().expect("the bundled expectations should be valid");
    }

    #[test]
    fn no_tables_is_valid_and_expects_nothing() {
        let expectations = Expectations::from_toml_str(MINIMAL).expect("minimal file is valid");
        assert!(expectations.failures().is_empty());
        assert!(expectations.ratio_failures().is_empty());
        assert!(expectations.memory_failures().is_empty());
    }

    #[test]
    fn a_ratio_entry_round_trips_and_is_found_by_fixture() {
        let text = r#"
schema_version = 1

[[expect_ratio_fail]]
fixture = "example"
platforms = ["linux", "macos", "windows"]
findings = ["F1"]
reason = "test reason"
"#;
        let expectations = Expectations::from_toml_str(text).expect("a valid ratio entry parses");
        let found = expectations
            .expected_ratio_failure("example")
            .expect("the entry applies on every platform");
        assert_eq!(found.findings, ["F1"]);
        assert!(expectations.expected_ratio_failure("other").is_none());
    }

    #[test]
    fn a_ratio_entry_rejects_an_unknown_field() {
        let text = r#"
schema_version = 1

[[expect_ratio_fail]]
fixture = "example"
platforms = ["macos"]
findings = ["F1"]
reason = "test reason"
kinds = ["missing"]
"#;
        assert!(matches!(
            Expectations::from_toml_str(text),
            Err(ExpectationError::Parse(_))
        ));
    }

    #[test]
    fn a_ratio_entry_is_rejected_for_every_broken_rule() {
        let cases = [
            (
                r#"
schema_version = 1
[[expect_ratio_fail]]
fixture = "dup"
platforms = ["macos"]
findings = ["F1"]
reason = "one"
[[expect_ratio_fail]]
fixture = "dup"
platforms = ["macos"]
findings = ["F1"]
reason = "two"
"#,
                "listed twice",
            ),
            (
                r#"
schema_version = 1
[[expect_ratio_fail]]
fixture = "no-platforms"
platforms = []
findings = ["F1"]
reason = "one"
"#,
                "names no platform",
            ),
            (
                r#"
schema_version = 1
[[expect_ratio_fail]]
fixture = "bad-platform"
platforms = ["atari"]
findings = ["F1"]
reason = "one"
"#,
                "names the platform",
            ),
            (
                r#"
schema_version = 1
[[expect_ratio_fail]]
fixture = "no-findings"
platforms = ["macos"]
findings = []
reason = "one"
"#,
                "names no finding",
            ),
            (
                r#"
schema_version = 1
[[expect_ratio_fail]]
fixture = "bad-finding"
platforms = ["macos"]
findings = ["f1"]
reason = "one"
"#,
                "names the finding",
            ),
            (
                r#"
schema_version = 1
[[expect_ratio_fail]]
fixture = "no-reason"
platforms = ["macos"]
findings = ["F1"]
reason = "   "
"#,
                "has no reason",
            ),
        ];
        for (text, expected_substring) in cases {
            match Expectations::from_toml_str(text) {
                Err(ExpectationError::Invalid { problems }) => assert!(
                    problems
                        .iter()
                        .any(|problem| problem.contains(expected_substring)),
                    "{problems:?} should mention {expected_substring:?}"
                ),
                other => panic!("{text} should be invalid, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_memory_entry_round_trips_and_is_found_by_fixture() {
        let text = r#"
schema_version = 1

[[expect_memory_fail]]
fixture = "example"
platforms = ["linux", "macos", "windows"]
findings = ["F1"]
reason = "test reason"
"#;
        let expectations = Expectations::from_toml_str(text).expect("a valid memory entry parses");
        let found = expectations
            .expected_memory_failure("example")
            .expect("the entry applies on every platform");
        assert_eq!(found.findings, ["F1"]);
        assert!(expectations.expected_memory_failure("other").is_none());
    }

    #[test]
    fn a_memory_entry_rejects_an_unknown_field() {
        let text = r#"
schema_version = 1

[[expect_memory_fail]]
fixture = "example"
platforms = ["macos"]
findings = ["F1"]
reason = "test reason"
kinds = ["missing"]
"#;
        assert!(matches!(
            Expectations::from_toml_str(text),
            Err(ExpectationError::Parse(_))
        ));
    }

    #[test]
    fn a_memory_entry_is_rejected_for_every_broken_rule() {
        let cases = [
            (
                r#"
schema_version = 1
[[expect_memory_fail]]
fixture = "dup"
platforms = ["macos"]
findings = ["F1"]
reason = "one"
[[expect_memory_fail]]
fixture = "dup"
platforms = ["macos"]
findings = ["F1"]
reason = "two"
"#,
                "listed twice",
            ),
            (
                r#"
schema_version = 1
[[expect_memory_fail]]
fixture = "no-platforms"
platforms = []
findings = ["F1"]
reason = "one"
"#,
                "names no platform",
            ),
            (
                r#"
schema_version = 1
[[expect_memory_fail]]
fixture = "bad-platform"
platforms = ["atari"]
findings = ["F1"]
reason = "one"
"#,
                "names the platform",
            ),
            (
                r#"
schema_version = 1
[[expect_memory_fail]]
fixture = "no-findings"
platforms = ["macos"]
findings = []
reason = "one"
"#,
                "names no finding",
            ),
            (
                r#"
schema_version = 1
[[expect_memory_fail]]
fixture = "bad-finding"
platforms = ["macos"]
findings = ["f1"]
reason = "one"
"#,
                "names the finding",
            ),
            (
                r#"
schema_version = 1
[[expect_memory_fail]]
fixture = "no-reason"
platforms = ["macos"]
findings = ["F1"]
reason = "   "
"#,
                "has no reason",
            ),
        ];
        for (text, expected_substring) in cases {
            match Expectations::from_toml_str(text) {
                Err(ExpectationError::Invalid { problems }) => assert!(
                    problems
                        .iter()
                        .any(|problem| problem.contains(expected_substring)),
                    "{problems:?} should mention {expected_substring:?}"
                ),
                other => panic!("{text} should be invalid, got {other:?}"),
            }
        }
    }

    #[test]
    fn the_same_fixture_may_appear_in_every_table() {
        let text = r#"
schema_version = 1

[[expect_fail]]
fixture = "shared"
platforms = ["linux", "macos", "windows"]
findings = ["F10"]
kinds = ["missing"]
reason = "diff reason"

[[expect_ratio_fail]]
fixture = "shared"
platforms = ["linux", "macos", "windows"]
findings = ["F1"]
reason = "ratio reason"

[[expect_memory_fail]]
fixture = "shared"
platforms = ["linux", "macos", "windows"]
findings = ["F1"]
reason = "memory reason"
"#;
        let expectations =
            Expectations::from_toml_str(text).expect("one fixture may appear in every table");
        assert!(expectations.expected_failure("shared").is_some());
        assert!(expectations.expected_ratio_failure("shared").is_some());
        assert!(expectations.expected_memory_failure("shared").is_some());
    }
}
