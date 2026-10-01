//! The `harness-failure` document: the evidence bundle for one failed scenario.

use serde::{Deserialize, Serialize};

use super::{Document, SchemaVersion};
use crate::{scenario::Profile, string_enum::string_enum};

string_enum! {
    /// The value of a failure bundle's `document_kind` field.
    #[derive(Default)]
    pub enum FailureKind {
        /// The only kind a failure bundle can have.
        #[default]
        HarnessFailure => "harness-failure",
    }
}

/// The step at which a scenario failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FailedStep {
    /// The zero-based index into the scenario's `steps`.
    pub index: u32,
    /// A human-readable description of the step.
    pub description: String,
}

/// What the step expected against what the screen showed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScreenComparison {
    /// The expectation that failed, rendered as text.
    pub expected: String,
    /// The screen text, from the screen model rather than raw output bytes.
    pub actual: String,
}

/// The terminal state when the scenario failed.
//
// These four flags are the terminal modes the restore contract names; they are independent and
// fixed, so a bitset or state enum would only obscure them.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalModes {
    /// The alternate screen is active.
    pub alternate_screen: bool,
    /// The cursor is visible.
    pub cursor_visible: bool,
    /// The terminal echoes input.
    pub echo: bool,
    /// The terminal is in canonical (line-buffered) mode.
    pub icanon: bool,
}

/// Resource use of the process under test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rusage {
    /// Peak resident set size in bytes.
    pub max_rss_bytes: u64,
    /// User CPU time in milliseconds.
    pub user_ms: u64,
    /// System CPU time in milliseconds.
    pub sys_ms: u64,
}

/// The generated fixture the scenario ran against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureIdentity {
    /// The lowercase hexadecimal hash of the fixture manifest.
    pub hash: String,
    /// The seed the fixture was generated from.
    pub seed: u64,
}

/// The session's diagnostics when a step failed by timing out: everything
/// `PtySession::diagnostics` could say about the output and the child at the moment the step gave
/// up. `None` for any other failure cause (a mismatch, the process exiting first, or a `delete`
/// step that was refused).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionDiagnostics {
    /// Every byte of terminal output the session had read when the step gave up.
    pub output_bytes: u64,
    /// Milliseconds from the spawn to the first byte of output, once any had arrived.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_byte_after_ms: Option<u64>,
    /// Whether the child was still running.
    pub child_running: bool,
    /// How many `ESC[6n` cursor-position report requests the screen model had answered.
    pub cursor_reports_answered: u32,
    /// The first bytes of output, escaped for display.
    pub head: String,
    /// The last bytes of output, escaped for display.
    pub tail: String,
}

/// The evidence bundle for one failed scenario.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessFailure {
    /// Always `harness-failure`.
    pub document_kind: FailureKind,
    /// Always the current schema version.
    pub schema_version: SchemaVersion,
    /// The scenario name.
    pub scenario: String,
    /// The profile it ran under.
    pub profile: Profile,
    /// The step that failed.
    pub failed_step: FailedStep,
    /// The failed expectation and the screen at that moment.
    pub screen: ScreenComparison,
    /// The terminal modes at that moment.
    pub terminal_modes: TerminalModes,
    /// The session's diagnostics, recorded when the step failed by timing out. See
    /// [`SessionDiagnostics`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_diagnostics: Option<SessionDiagnostics>,
    /// The recording of the session, in asciicast format.
    pub cast_path: String,
    /// Resource use of the process.
    pub rusage: Rusage,
    /// The fixture the scenario ran against.
    pub fixture: FixtureIdentity,
    /// A command that reruns exactly this scenario, profile, and fixture.
    pub repro_command: String,
}

impl Document for HarnessFailure {
    const KIND: &'static str = FailureKind::HarnessFailure.as_str();
    const SCHEMA_ID: &'static str =
        "https://github.com/findyourexit/excise/harness/schemas/harness-failure-v1.json";
    const SCHEMA_JSON: &'static str = include_str!("../../schemas/harness-failure.schema.json");
}
