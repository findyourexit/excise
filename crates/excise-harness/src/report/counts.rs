//! The `harness-counts` document: the deterministic counts of what one build of `excise` costs.
//!
//! A counts document records numbers that, for a given binary and fixture, do not depend on
//! timing or load (see [`crate::counts`] for which numbers and why each is stable). It is the
//! record the history job appends to the `bench-data` branch, and the artifact a pull request's
//! counts job uploads. Both are read back by the same strict reader, so a document from anywhere
//! is held to this schema before a single field of it is used.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{Document, SchemaVersion};
use crate::{scenario::Profile, string_enum::string_enum};

/// The largest count a document may carry: `2^53 - 1`, the largest integer that every JSON
/// consumer (`jq`, JavaScript) reads back exactly.
pub const MAX_COUNT: u64 = 9_007_199_254_740_991;

/// The most cases (fixtures counted under a profile) a document may hold, which its schema
/// limits to the same number.
pub const MAX_CASES: usize = 16;

string_enum! {
    /// The value of a counts document's `document_kind` field.
    #[derive(Default)]
    pub enum CountsKind {
        /// The only kind a counts document can have.
        #[default]
        HarnessCounts => "harness-counts",
    }
}

/// The fixture one case counted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CountsFixture {
    /// The fixture's id.
    pub id: String,
    /// The lowercase hexadecimal hash of the fixture manifest. Counts of two documents are
    /// comparable only where this is equal.
    pub hash: String,
    /// The seed the fixture was generated from.
    pub seed: u64,
}

/// One case: a fixture counted under a profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CountsCase {
    /// The fixture that was counted.
    pub fixture: CountsFixture,
    /// The profile the program ran under.
    pub profile: Profile,
    /// The counts, by metric name. The names are an open vocabulary (a build that counts one
    /// more thing adds a name and keeps version 1), each a non-negative whole number of at most
    /// [`MAX_COUNT`].
    pub metrics: BTreeMap<String, u64>,
}

/// The machine the counts were taken on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CountsRunner {
    /// The operating system, as in `std::env::consts::OS`.
    pub os: String,
    /// The operating system and its version, for example `Ubuntu 24.04.3 LTS`: printable ASCII
    /// only, because a pull request's artifact is untrusted input and this is free text.
    pub os_version: String,
    /// The CPU architecture, as in `std::env::consts::ARCH`.
    pub arch: String,
}

/// The pull request a counts document was taken for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PullRequestOrigin {
    /// The pull request's number.
    pub number: u64,
    /// The commit the pull request was based on when the counts were taken. A comment compares
    /// with the record of this commit, or of its nearest ancestor that has one.
    pub base_sha: String,
    /// The tip of the pull request's branch. The counts themselves are of the merge of this tip
    /// into the base, which `context.git_sha` names.
    pub head_sha: String,
}

/// What the counts were taken of, and where.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CountsContext {
    /// The 40-character commit that was built and counted.
    pub git_sha: String,
    /// When that commit was committed, as an RFC 3339 timestamp: a property of the commit, so
    /// that two counts of one commit write the same document.
    pub committed_at: String,
    /// The machine.
    pub runner: CountsRunner,
    /// The Rust toolchain that built the binary: the first line of `rustc --version`, printable
    /// ASCII only.
    pub toolchain: String,
    /// The pull request the counts were taken for. Absent in a record of a commit on `main`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull_request: Option<PullRequestOrigin>,
}

/// The deterministic counts of one build.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessCounts {
    /// Always `harness-counts`.
    pub document_kind: CountsKind,
    /// Always the current schema version.
    pub schema_version: SchemaVersion,
    /// What was counted, and where.
    pub context: CountsContext,
    /// One entry per fixture and profile, in the order they were counted.
    pub cases: Vec<CountsCase>,
}

impl Document for HarnessCounts {
    const KIND: &'static str = CountsKind::HarnessCounts.as_str();
    const SCHEMA_ID: &'static str =
        "https://github.com/findyourexit/excise/harness/schemas/harness-counts-v1.json";
    const SCHEMA_JSON: &'static str = include_str!("../../schemas/harness-counts.schema.json");
}

/// A counts document that breaks a rule its JSON Schema cannot express.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CountsInvalid {
    /// A fixture and profile appear in more than one case, so a count would have two values.
    #[error("the fixture `{fixture}` under the profile `{profile}` is counted more than once")]
    DuplicateCase {
        /// The fixture.
        fixture: String,
        /// The profile.
        profile: Profile,
    },
    /// A count is larger than a JSON consumer can read back exactly.
    #[error(
        "`{metric}` of `{fixture}` is {value}, more than the largest count a document may carry, \
         {MAX_COUNT}"
    )]
    CountTooLarge {
        /// The fixture.
        fixture: String,
        /// The metric.
        metric: String,
        /// Its value.
        value: u64,
    },
}

impl HarnessCounts {
    /// Checks the rules that the JSON Schema cannot say: a fixture appears once under a profile,
    /// and no count exceeds [`MAX_COUNT`].
    ///
    /// # Errors
    ///
    /// Returns the first rule the document breaks.
    pub fn check(&self) -> Result<(), CountsInvalid> {
        for (index, case) in self.cases.iter().enumerate() {
            if self.cases[..index].iter().any(|earlier| {
                earlier.fixture.id == case.fixture.id && earlier.profile == case.profile
            }) {
                return Err(CountsInvalid::DuplicateCase {
                    fixture: case.fixture.id.clone(),
                    profile: case.profile,
                });
            }
            if let Some((metric, value)) =
                case.metrics.iter().find(|(_, value)| **value > MAX_COUNT)
            {
                return Err(CountsInvalid::CountTooLarge {
                    fixture: case.fixture.id.clone(),
                    metric: metric.clone(),
                    value: *value,
                });
            }
        }
        Ok(())
    }

    /// The case counted for `fixture` under `profile`, if there is one.
    #[must_use]
    pub fn case(&self, fixture: &str, profile: Profile) -> Option<&CountsCase> {
        self.cases
            .iter()
            .find(|case| case.fixture.id == fixture && case.profile == profile)
    }
}
