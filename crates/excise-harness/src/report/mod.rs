//! Versioned machine-output documents.
//!
//! Every document carries a `document_kind` and a `schema_version`, like the published Excise
//! report formats, and has a draft 2020-12 JSON Schema in this crate's `schemas/` directory. The
//! schemas are deliberately not published with the product (`docs/schemas` ships in release
//! archives); the tests in this crate keep them in step with the Rust types.
//!
//! * [`HarnessSummary`] (`harness-summary`): the result of one run.
//! * [`HarnessFailure`] (`harness-failure`): the evidence bundle for one failed scenario.
//! * [`HarnessAb`] (`harness-ab`): paired, interleaved comparison evidence for two builds.
//! * [`HarnessCounts`] (`harness-counts`): the deterministic counts of one build, the record that
//!   the count history and a pull request's count comment are made of.
//! * [`HarnessSweep`] (`harness-sweep`): what `cargo xtask sweep` found of every published version,
//!   and the version-by-defect table built from it.
//! * [`HarnessTui`] (`harness-tui`): what one `cargo xtask tui` command prints.
//! * [`HarnessShapeProfile`] (`harness-shape-profile`): the shape of one tree as aggregates only,
//!   written by `excise-shape profile`, from which a fixture specification can be built.
//! * [`HarnessSoak`] (`harness-soak`): what one read-only `cargo xtask soak` measured, with no path
//!   and no name from the tree.
//!
//! The types reject unknown fields, so a document with a field this build does not know is an
//! error rather than silently ignored. Removing or retyping a field, or making an optional one
//! required, needs a new `schema_version`. An optional field that its writer leaves out when it has
//! nothing to say (`session_diagnostics`, `latency_budget_scale`, `timing_informational`,
//! `quick_tier_ms`, and a result's `timing_warnings`) is additive and keeps version 1.

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;

mod ab;
mod counts;
mod failure;
mod shape;
mod soak;
mod summary;
mod sweep;
pub mod tui;

#[cfg(test)]
mod tests;

pub use ab::{
    AbContext, AbFixture, AbKind, AbVerdict, BuildIdentity, ConfidenceInterval, HarnessAb,
    MetricComparison, Samples, Side,
};
pub use counts::{
    CountsCase, CountsContext, CountsFixture, CountsInvalid, CountsKind, CountsRunner,
    HarnessCounts, MAX_CASES, MAX_COUNT, PullRequestOrigin,
};
pub use failure::{
    FailedStep, FailureKind, FixtureIdentity, HarnessFailure, Rusage, ScreenComparison,
    SessionDiagnostics, TerminalModes,
};
pub use shape::{
    ClassHistograms, HarnessShapeProfile, MAX_PROFILE_DEPTH, ShapeDepth, ShapeEntries,
    ShapeHardLinks, ShapeHistogram, ShapeNameLengths, ShapePlatform, ShapeProblems,
    ShapeProfileKind, ShapeSymbolicLinks, ShapeUnreadable, ShapeWalk,
};
pub use soak::{
    AccountingHeadline, HarnessSoak, QuirkKind, SoakAccounting, SoakExit, SoakHeadless, SoakKind,
    SoakLimits, SoakOutcome, SoakPhase, SoakQuirkCount, SoakReportFacts, SoakRounds,
    SoakScanSummary, SoakTui,
};
pub use summary::{
    BinaryIdentity, HarnessSummary, ScenarioResult, SummaryKind, Tier, TimingWarning, Verdict,
};
pub use sweep::{
    BuildStatus, CellState, CheckStatus, HarnessSweep, SweepBuild, SweepCell, SweepCheck,
    SweepContext, SweepInvalid, SweepKind, SweepMeasurement, SweepRatio, SweepRow, SweepSeries,
    SweepTier, SweepToolchain, SweepTraits, SweepVersion,
};
pub use tui::{HarnessTui, TuiKind};

/// The `schema_version` of every document in this module.
pub const SCHEMA_VERSION: u32 = 1;

/// The `schema_version` field: serializes as [`SCHEMA_VERSION`] and accepts nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(try_from = "u32", into = "u32")]
pub struct SchemaVersion;

/// A `schema_version` other than [`SCHEMA_VERSION`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("unsupported schema_version {0}; this build reads only version {SCHEMA_VERSION}")]
pub struct UnsupportedSchemaVersion(pub u32);

impl From<SchemaVersion> for u32 {
    fn from(SchemaVersion: SchemaVersion) -> Self {
        SCHEMA_VERSION
    }
}

impl TryFrom<u32> for SchemaVersion {
    type Error = UnsupportedSchemaVersion;

    fn try_from(version: u32) -> Result<Self, Self::Error> {
        if version == SCHEMA_VERSION {
            Ok(Self)
        } else {
            Err(UnsupportedSchemaVersion(version))
        }
    }
}

/// A machine-output document with a published JSON Schema.
pub trait Document: Serialize + DeserializeOwned {
    /// The value of the document's `document_kind` field.
    const KIND: &'static str;
    /// The `$id` of the document's JSON Schema.
    const SCHEMA_ID: &'static str;
    /// The text of the document's JSON Schema.
    const SCHEMA_JSON: &'static str;

    /// Renders the canonical form: pretty-printed JSON in field order with a final newline.
    ///
    /// # Errors
    ///
    /// Returns an error if the document contains a number JSON cannot carry. NaN and infinity
    /// would be written as `null`, so the output is read back to prove this build can parse it.
    fn to_json_pretty(&self) -> Result<String, serde_json::Error> {
        let mut text = serde_json::to_string_pretty(self)?;
        Self::from_json_str(&text)?;
        text.push('\n');
        Ok(text)
    }

    /// Parses a document, rejecting unknown fields and any other `document_kind` or
    /// `schema_version`.
    ///
    /// # Errors
    ///
    /// Returns the JSON error, with line and column.
    fn from_json_str(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }
}
