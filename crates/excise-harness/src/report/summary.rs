//! The `harness-summary` document: the result of one harness run.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{Document, SchemaVersion};
use crate::{
    scenario::{Expect, Profile},
    string_enum::string_enum,
};

string_enum! {
    /// The value of a summary's `document_kind` field.
    #[derive(Default)]
    pub enum SummaryKind {
        /// The only kind a summary can have.
        #[default]
        HarnessSummary => "harness-summary",
    }
}

string_enum! {
    /// How much of the validation program a run covered.
    pub enum Tier {
        /// The pull-request tier, run in two minutes or less.
        Quick => "quick",
        /// Every scenario, including the slow ones.
        Full => "full",
        /// The nightly tier with the largest fixtures.
        Nightly => "nightly",
        /// The weekly or manual tier.
        Weekly => "weekly",
    }
}

string_enum! {
    /// The outcome of one scenario under one profile.
    pub enum Verdict {
        /// An expected-to-pass scenario passed.
        Pass => "pass",
        /// An expected-to-pass scenario failed.
        Fail => "fail",
        /// An expected-to-fail scenario failed, as documented.
        Xfail => "xfail",
        /// An expected-to-fail scenario passed. The defect it documents is fixed, so the run
        /// fails until the scenario is flipped to `expect = "pass"`.
        Xpass => "xpass",
        /// The harness itself could not run the scenario (fixture, spawn, or isolation failure).
        Error => "error",
    }
}

impl Verdict {
    /// The verdict of a scenario the harness ran to completion: whether it `passed` against what
    /// it `expect`s.
    ///
    /// This is strict xfail: an expected failure that passes is [`Verdict::Xpass`], never a pass.
    #[must_use]
    pub const fn resolve(expect: Expect, passed: bool) -> Self {
        match (expect, passed) {
            (Expect::Pass, true) => Self::Pass,
            (Expect::Pass, false) => Self::Fail,
            (Expect::Fail, false) => Self::Xfail,
            (Expect::Fail, true) => Self::Xpass,
        }
    }

    /// Whether a run containing this verdict must fail.
    #[must_use]
    pub const fn blocks_run(self) -> bool {
        !matches!(self, Self::Pass | Self::Xfail)
    }
}

/// The identity of the `excise` binary under test.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BinaryIdentity {
    /// Where the binary was.
    pub path: String,
    /// The lowercase hexadecimal SHA-256 of the binary.
    pub sha256: String,
}

/// The result of one scenario under one profile.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScenarioResult {
    /// The scenario name.
    pub name: String,
    /// The profile it ran under.
    pub profile: Profile,
    /// The outcome.
    pub verdict: Verdict,
    /// Wall time in milliseconds.
    pub duration_ms: u64,
    /// Named measurements. Every value is a finite number.
    pub metrics: BTreeMap<String, f64>,
    /// The failure bundle directory, when the scenario produced one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_bundle: Option<String>,
}

/// The result of one harness run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessSummary {
    /// Always `harness-summary`.
    pub document_kind: SummaryKind,
    /// Always the current schema version.
    pub schema_version: SchemaVersion,
    /// The run identifier, which is also the name of the run's output directory.
    pub run_id: String,
    /// The tier that was run.
    pub tier: Tier,
    /// When the run started, as an RFC 3339 timestamp.
    pub started_at: String,
    /// When the run finished, as an RFC 3339 timestamp.
    pub finished_at: String,
    /// The host name.
    pub host: String,
    /// The operating system, as in `std::env::consts::OS`.
    pub os: String,
    /// The CPU architecture, as in `std::env::consts::ARCH`.
    pub arch: String,
    /// The binary under test.
    pub excise_binary: BinaryIdentity,
    /// The 40-character commit the binary was built from.
    pub git_sha: String,
    /// One result per scenario and profile.
    pub scenarios: Vec<ScenarioResult>,
}

impl Document for HarnessSummary {
    const KIND: &'static str = SummaryKind::HarnessSummary.as_str();
    const SCHEMA_ID: &'static str =
        "https://github.com/findyourexit/excise/harness/schemas/harness-summary-v1.json";
    const SCHEMA_JSON: &'static str = include_str!("../../schemas/harness-summary.schema.json");
}
