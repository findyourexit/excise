//! The `harness-summary` document: the result of one harness run.

use std::{collections::BTreeMap, fmt};

use serde::{Deserialize, Serialize};

use super::{Document, SchemaVersion};
use crate::{
    scenario::{Budget, Expect, Profile},
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

/// A timing budget that a result missed and was not failed for, because the run held timing
/// informational: the miss is recorded here and changed neither the result's verdict nor the exit
/// status.
///
/// Only the budgets a hosted machine's speed decides are ever reported this way: the four latency
/// budgets of a scenario, a comparison's ratio, and a headless scan's ratio against `du`. Every
/// other budget keeps blocking, and so does a result that is expected to fail.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimingWarning {
    /// The budget that was missed.
    pub budget: Budget,
    /// The metric that was held to it.
    pub metric: String,
    /// What was measured.
    pub value: f64,
    /// The limit it was held to, after any latency scale: `value` is above it.
    pub limit: f64,
}

/// `value` to three decimals, without trailing zeros: `588`, `2.193`.
fn trimmed(value: f64) -> String {
    let text = format!("{value:.3}");
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}

impl fmt::Display for TimingWarning {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (metric, value, limit) = (&self.metric, trimmed(self.value), trimmed(self.limit));
        if *metric == self.budget.as_str() {
            write!(
                formatter,
                "`{metric}` is {value}, over its limit of {limit}"
            )
        } else {
            write!(
                formatter,
                "`{metric}` is {value}, over the `{}` limit of {limit}",
                self.budget
            )
        }
    }
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
    /// The timing budgets this result missed while the run held timing informational: warnings
    /// that left the verdict as it was. Absent when there are none, and always absent from a
    /// strict run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub timing_warnings: Vec<TimingWarning>,
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
    /// The factor the latency budgets (`input_to_frame_p99_ms`, `max_stall_ms`, `first_frame_ms`,
    /// `quit_ms`) of the scenarios that were expected to pass were multiplied by, when it is not
    /// 1: the budgets this run held its scenarios to were that many times looser than the strict
    /// ones. Absent in a strict run. No other budget is ever scaled, and a scenario expected to
    /// fail was judged against the strict budgets either way.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_budget_scale: Option<f64>,
    /// Whether the run held timing informational: a timing budget one of its results missed (a
    /// scenario's four latency budgets, a headless scan's ratio against `du`) is a warning in
    /// that result's `timing_warnings`, not a failure. Absent in a strict run. Every other budget
    /// blocked as usual, and a result that was expected to fail was judged strictly either way.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub timing_informational: bool,
    /// One result per scenario and profile.
    pub scenarios: Vec<ScenarioResult>,
}

impl Document for HarnessSummary {
    const KIND: &'static str = SummaryKind::HarnessSummary.as_str();
    const SCHEMA_ID: &'static str =
        "https://github.com/findyourexit/excise/harness/schemas/harness-summary-v1.json";
    const SCHEMA_JSON: &'static str = include_str!("../../schemas/harness-summary.schema.json");
}
