//! Expected failures of the oracle diff: strict xfail for fixtures that fail for a known reason.
//!
//! A fixture can fail the diff because of a defect that is known and not yet fixed. Listing it in
//! `expectations/headless.toml` turns that into a strict expected failure, with the semantics the
//! scenario model gives `expect = "fail"`:
//!
//! | Listed | Discrepancy kinds the runs show | Verdict | Fails the run |
//! |---|---|---|---|
//! | no | none | `pass` | no |
//! | no | any | `fail` | yes |
//! | yes | exactly the listed kinds | `xfail` | no |
//! | yes | none | `xpass` | **yes** |
//! | yes | other kinds | `fail` | yes |
//!
//! So an expected failure must fail in the way it was written down: a different failure is a new
//! defect, and no failure means the defect is fixed and the entry has to go. An entry applies only
//! on the platforms it names; elsewhere the fixture is held to a clean diff like any other.
//!
//! The kinds are those of [`DiscrepancyKind`]. An entry names the findings it documents (`F10`),
//! so that the change that fixes one finds every entry that waits for it.

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
}

/// The fixtures that are expected to fail, and why.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Expectations {
    failures: Vec<ExpectedFailure>,
}

impl Expectations {
    /// No expectations: every fixture must have a clean diff.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            failures: Vec::new(),
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
                problems.push(format!("`{fixture}` is listed twice"));
            }
            if entry.platforms.is_empty() {
                problems.push(format!("`{fixture}` names no platform"));
            }
            for platform in &entry.platforms {
                if !PLATFORMS.contains(&platform.as_str()) {
                    problems.push(format!(
                        "`{fixture}` names the platform `{platform}`; the platforms are {}",
                        PLATFORMS.join(", ")
                    ));
                }
            }
            if entry.findings.is_empty() {
                problems.push(format!("`{fixture}` names no finding"));
            }
            for finding in &entry.findings {
                if !is_finding_id(finding) {
                    problems.push(format!(
                        "`{fixture}` names the finding `{finding}`, which is not an uppercase \
                         letter and up to seven uppercase letters or digits"
                    ));
                }
            }
            if entry.kinds.is_empty() {
                problems.push(format!("`{fixture}` lists no discrepancy kind"));
            }
            if entry.reason.trim().is_empty() {
                problems.push(format!("`{fixture}` has no reason"));
            }
        }
        if problems.is_empty() {
            Ok(Self {
                failures: file.expect_fail,
            })
        } else {
            Err(ExpectationError::Invalid { problems })
        }
    }

    /// Every entry, whether or not it applies on this platform.
    #[must_use]
    pub fn failures(&self) -> &[ExpectedFailure] {
        &self.failures
    }

    /// The entry for `fixture` that applies on this platform.
    #[must_use]
    pub fn expected_failure(&self, fixture: &str) -> Option<&ExpectedFailure> {
        self.failures
            .iter()
            .find(|entry| entry.fixture == fixture && entry.applies_here())
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
