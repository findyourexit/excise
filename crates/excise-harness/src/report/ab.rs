//! The `harness-ab` document: paired, interleaved comparison evidence for two builds.

use serde::{Deserialize, Serialize};

use super::{Document, SchemaVersion};
use crate::string_enum::string_enum;

string_enum! {
    /// The value of an A/B document's `document_kind` field.
    #[derive(Default)]
    pub enum AbKind {
        /// The only kind an A/B document can have.
        #[default]
        HarnessAb => "harness-ab",
    }
}

string_enum! {
    /// One side of a comparison.
    pub enum Side {
        /// The build being compared against.
        Baseline => "baseline",
        /// The build under evaluation.
        Candidate => "candidate",
    }
}

string_enum! {
    /// What a metric's comparison means for the change.
    pub enum AbVerdict {
        /// A regression large enough, with enough confidence, to block the change.
        Block => "block",
        /// A difference worth reporting that does not block.
        Warn => "warn",
        /// No meaningful regression.
        Pass => "pass",
    }
}

/// One build in a comparison.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildIdentity {
    /// The git reference the build was made from.
    pub git_ref: String,
    /// The lowercase hexadecimal SHA-256 of the binary.
    pub binary_sha256: String,
}

/// The measurements of one metric, one entry per trial and index-aligned across the sides.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Samples {
    /// The baseline measurements.
    pub baseline: Vec<f64>,
    /// The candidate measurements.
    pub candidate: Vec<f64>,
}

/// A bootstrap confidence interval for the median of the per-trial candidate/baseline ratios.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfidenceInterval {
    /// The lower bound.
    pub lower: f64,
    /// The upper bound.
    pub upper: f64,
    /// The confidence level strictly between 0 and 1, for example `0.95`.
    pub confidence: f64,
}

/// The comparison of one metric.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricComparison {
    /// The metric name: the case it came from (a fixture id, or `<scenario>-<profile>`), then
    /// `__`, then the metric, for example `wide-1k__wall_time_ms` or
    /// `delete-folder-lifecycle-default__scan_complete_ms`. Qualifying every name this way means
    /// one comparison can hold several fixtures and scenarios without their metrics colliding.
    pub name: String,
    /// The raw measurements.
    pub samples: Samples,
    /// The median of the per-trial candidate/baseline ratios.
    pub median_ratio: f64,
    /// The bootstrap confidence interval for `median_ratio`.
    pub bootstrap_ci: ConfidenceInterval,
    /// What the comparison means for the change.
    pub verdict: AbVerdict,
}

/// One underlying fixture a comparison ran against: a `--fixture` value, or the fixture a
/// `--scenario` names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AbFixture {
    /// The fixture's id.
    pub id: String,
    /// The lowercase hexadecimal hash of the fixture manifest.
    pub hash: String,
    /// The seed the fixture was generated from.
    pub seed: u64,
}

/// The conditions the comparison ran under, recorded because timings do not transfer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AbContext {
    /// The host name.
    pub host: String,
    /// The CPU model.
    pub cpu: String,
    /// The operating system and version, for example `macOS 14.8.9`.
    pub os: String,
    /// The CPU architecture, for example `aarch64`.
    pub arch: String,
    /// The number of logical CPUs.
    pub logical_cpus: u32,
    /// The Rust toolchain that built both binaries (`rustc -Vv`).
    pub toolchain: String,
    /// The power state, for example `ac`, `battery`, or `unknown`.
    pub power: String,
    /// The 1-minute load average when the comparison started.
    pub load_average_start: f64,
    /// The 1-minute load average when the comparison finished.
    pub load_average_end: f64,
    /// How many other `excise` processes were running during the comparison.
    pub concurrent_excise_processes: u32,
    /// The fixtures the comparison ran against, one entry per distinct fixture (a run that
    /// compares several fixtures or scenarios lists one each).
    pub fixtures: Vec<AbFixture>,
}

/// Paired, interleaved comparison evidence for two builds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessAb {
    /// Always `harness-ab`.
    pub document_kind: AbKind,
    /// Always the current schema version.
    pub schema_version: SchemaVersion,
    /// The build being compared against.
    pub baseline: BuildIdentity,
    /// The build under evaluation.
    pub candidate: BuildIdentity,
    /// The number of paired trials.
    pub trials: u32,
    /// The order in which the individual runs executed, so the interleaving pattern is auditable.
    pub interleaving: Vec<Side>,
    /// One comparison per metric.
    pub metrics: Vec<MetricComparison>,
    /// The conditions the comparison ran under.
    pub context: AbContext,
}

impl Document for HarnessAb {
    const KIND: &'static str = AbKind::HarnessAb.as_str();
    const SCHEMA_ID: &'static str =
        "https://github.com/findyourexit/excise/harness/schemas/harness-ab-v1.json";
    const SCHEMA_JSON: &'static str = include_str!("../../schemas/harness-ab.schema.json");
}
