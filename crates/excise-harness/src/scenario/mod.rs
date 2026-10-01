//! Typed model of TOML scenario files.
//!
//! A scenario describes one black-box interaction with an `excise` process running against a
//! generated fixture: what the terminal looks like, what to do, and what must hold afterwards.
//!
//! [`Scenario::from_toml_str`] and [`Scenario::from_path`] parse a document and reject unknown
//! fields at every level. [`Scenario::validate`] then applies the semantic rules that TOML typing
//! cannot express. A runner MUST call `validate` and refuse to run a scenario that fails it:
//! parsing alone does not make a scenario safe to execute.

use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{platform::PLATFORMS, string_enum::string_enum};

mod path;
mod step;
mod validate;

#[cfg(test)]
mod tests;

pub use path::{PathViolation, check_fixture_relative_path};
pub use step::{
    Comparison, Delete, DeleteWait, EntryKind, EventField, EventKind, ExpectBudget, ExpectExit,
    ExpectFs, ExpectScreen, FsMutate, KeyName, KeyNameError, Marker, Measure, MutateOp, PressKey,
    Quit, Region, Residue, Resize, ScanState, Select, SendSignal, Settle, Signal, Step, TypeText,
    WaitEvent, WaitFs, WaitHeader, WaitText,
};
pub use validate::{Field, StepError, ValidationError, ValidationErrors};

/// The only scenario `schema_version` this crate reads.
pub const SCHEMA_VERSION: u32 = 1;
/// The narrowest supported initial terminal.
pub const MIN_TERMINAL_COLS: u16 = 32;
/// The shortest supported initial terminal.
pub const MIN_TERMINAL_ROWS: u16 = 8;
/// The terminal width used when a scenario has no `[terminal]` table.
pub const DEFAULT_TERMINAL_COLS: u16 = 120;
/// The terminal height used when a scenario has no `[terminal]` table.
pub const DEFAULT_TERMINAL_ROWS: u16 = 40;
/// The bound applied to a waiting step that does not set `timeout_ms`.
pub const DEFAULT_TIMEOUT_MS: u64 = 10_000;
/// The largest `timeout_ms` a step may set: thirty minutes. Every wait is bounded, and a bound
/// beyond this is a scenario bug rather than a patient test.
pub const MAX_TIMEOUT_MS: u64 = 1_800_000;
/// The longest scenario, fixture, and metric identifier.
pub const MAX_IDENTIFIER_LEN: usize = 64;

string_enum! {
    /// A named runner configuration that a scenario must pass under.
    pub enum Profile {
        /// The user defaults.
        Default => "default",
        /// Reduced motion and a single scan thread.
        Deterministic => "deterministic",
        /// Monochrome output with ASCII symbols and borders.
        MonochromeAscii => "monochrome-ascii",
        /// A narrow terminal.
        Narrow => "narrow",
        /// Mouse input and the alternative movement keymaps.
        MouseKeymaps => "mouse-keymaps",
    }
}

string_enum! {
    /// How often a scenario runs: the cadence `cargo xtask e2e` and the in-process runner select
    /// by. Distinct from [`crate::report::Tier`], which is how much of a *run* covered, not how
    /// often one scenario runs.
    #[derive(Default)]
    pub enum Tier {
        /// Runs under `cargo xtask e2e --quick`, `--full`, and `--nightly`, and under the
        /// in-process runner. The default.
        #[default]
        Quick => "quick",
        /// Runs under `cargo xtask e2e --full` and `--nightly`; never in-process.
        Full => "full",
        /// Runs under `cargo xtask e2e --nightly` only; never in-process.
        Nightly => "nightly",
    }
}

string_enum! {
    /// Whether a scenario is expected to pass or to fail.
    #[derive(Default)]
    pub enum Expect {
        /// The scenario must pass.
        #[default]
        Pass => "pass",
        /// The scenario documents a known defect: it runs and must fail (strict xfail).
        Fail => "fail",
    }
}

string_enum! {
    /// A named budget that a scenario can check with `expect_budget` or override in `[budgets]`.
    ///
    /// The variant documentation gives the unit of the limit.
    pub enum Budget {
        /// Ratio of headless scan time to `du -sk` time on the same fixture.
        HeadlessScanRatio => "headless_scan_ratio",
        /// Ratio of interactive time-to-COMPLETE to headless scan time.
        TuiCompleteRatio => "tui_complete_ratio",
        /// Ratio of default-motion time-to-COMPLETE to deterministic time-to-COMPLETE.
        MotionCompleteRatio => "motion_complete_ratio",
        /// The 99th percentile of input-to-frame latency, in milliseconds.
        InputToFrameP99Ms => "input_to_frame_p99_ms",
        /// The longest gap without a frame while work is active, in milliseconds.
        MaxStallMs => "max_stall_ms",
        /// Time to the first frame, in milliseconds.
        FirstFrameMs => "first_frame_ms",
        /// Time from confirming the quit to process exit, in milliseconds.
        QuitMs => "quit_ms",
        /// Peak resident memory (peak footprint where the platform reports it), in bytes.
        PeakRssBytes => "peak_rss_bytes",
        /// The allowed relative change in peak memory between builds, as a fraction (`0.05`).
        MemoryAbTolerance => "memory_ab_tolerance",
        /// The scan-store quota as a fraction of free scratch space (`0.75`).
        ScanStoreQuotaFraction => "scan_store_quota_fraction",
        /// Scan-store bytes per indexed entry.
        ScanStoreBytesPerEntry => "scan_store_bytes_per_entry",
        /// The number of threads in the process.
        Threads => "threads",
        /// The number of open file descriptors (handles on Windows).
        Fds => "fds",
        /// Terminal output bytes while the program is idle.
        IdleOutputBytes => "idle_output_bytes",
        /// CPU time used while the program is idle, in milliseconds.
        IdleCpuMs => "idle_cpu_ms",
        /// Files left in the scenario scratch directory.
        ResidueFiles => "residue_files",
        /// The tolerated timing regression between builds, as a fraction (`0.20`).
        TimingAbRegression => "timing_ab_regression",
    }
}

/// The size of the pseudo-terminal a scenario starts in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Terminal {
    /// Width in columns.
    pub cols: u16,
    /// Height in rows.
    pub rows: u16,
}

impl Default for Terminal {
    fn default() -> Self {
        Self {
            cols: DEFAULT_TERMINAL_COLS,
            rows: DEFAULT_TERMINAL_ROWS,
        }
    }
}

/// One scenario file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    /// The format version; must be [`SCHEMA_VERSION`].
    pub schema_version: u32,
    /// The scenario's identifier.
    pub name: String,
    /// What the scenario demonstrates.
    pub description: String,
    /// The identifier of the fixture specification to generate.
    pub fixture: String,
    /// Fixture-relative paths that must survive the scenario.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sentinels: Vec<String>,
    /// The profiles the scenario runs under.
    pub profiles: Vec<Profile>,
    /// How often the scenario runs. `quick` (the default) runs under every tier and in-process;
    /// `full` needs `--full` or `--nightly`; `nightly` needs `--nightly`.
    #[serde(default)]
    pub tier: Tier,
    /// The platforms the scenario runs on, as `std::env::consts::OS` spells them. Absent means
    /// every platform the harness knows. Elsewhere every runner skips the scenario, with the
    /// reason, even when it is named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platforms: Option<Vec<String>>,
    /// The initial terminal size.
    #[serde(default)]
    pub terminal: Terminal,
    /// Whether the scenario must pass or must fail.
    #[serde(default)]
    pub expect: Expect,
    /// The platforms `expect = "fail"` applies to, as `std::env::consts::OS` spells them. Absent
    /// means every platform in `platforms`. Elsewhere, on a platform the scenario runs on, it is
    /// an ordinary `expect = "pass"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fails_on: Option<Vec<String>>,
    /// The id of the work slice that will fix the defect; required when `expect` is `fail`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slice: Option<String>,
    /// Overrides for the limits of named budgets.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub budgets: BTreeMap<Budget, f64>,
    /// The ordered steps.
    pub steps: Vec<Step>,
}

/// A scenario file that could not be read or parsed.
#[derive(Debug, Error)]
pub enum LoadError {
    /// The file could not be read.
    #[error("cannot read scenario `{}`: {source}", path.display())]
    Read {
        /// The path that was read.
        path: PathBuf,
        /// The underlying error.
        source: io::Error,
    },
    /// The file is not a valid scenario document.
    #[error("invalid scenario `{}`: {source}", path.display())]
    Parse {
        /// The path that was read.
        path: PathBuf,
        /// The underlying error, with line and column.
        source: toml::de::Error,
    },
}

impl Scenario {
    /// Parses a scenario from TOML text.
    ///
    /// Parsing rejects unknown fields, unknown enum values, and wrongly typed values. It does not
    /// apply the semantic rules of [`Scenario::validate`], which a runner must also apply.
    ///
    /// # Errors
    ///
    /// Returns the TOML error, which carries the line and column of the problem.
    pub fn from_toml_str(source: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(source)
    }

    /// Reads and parses a scenario file.
    ///
    /// # Errors
    ///
    /// Returns [`LoadError::Read`] when the file cannot be read and [`LoadError::Parse`] when it
    /// is not a valid scenario document.
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, LoadError> {
        let path = path.as_ref();
        let source = fs::read_to_string(path).map_err(|source| LoadError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_toml_str(&source).map_err(|source| LoadError::Parse {
            path: path.to_path_buf(),
            source,
        })
    }
}

impl Scenario {
    /// The platforms this scenario runs on, as `std::env::consts::OS` spells them: `platforms`
    /// verbatim, or every platform the harness knows when it is absent.
    #[must_use]
    pub fn effective_platforms(&self) -> Vec<&str> {
        self.platforms.as_deref().map_or_else(
            || PLATFORMS.to_vec(),
            |platforms| platforms.iter().map(String::as_str).collect(),
        )
    }

    /// Whether the scenario runs on `os`, as `std::env::consts::OS` spells it.
    #[must_use]
    pub fn runs_on(&self, os: &str) -> bool {
        match &self.platforms {
            Some(platforms) => platforms.iter().any(|platform| platform == os),
            None => PLATFORMS.contains(&os),
        }
    }

    /// Whether `expect = "fail"` applies on `os`, as `std::env::consts::OS` spells it:
    /// `fails_on` verbatim, or every platform in `platforms` when it is absent.
    fn fails_on_platform(&self, os: &str) -> bool {
        match &self.fails_on {
            Some(fails_on) => fails_on.iter().any(|platform| platform == os),
            None => self.runs_on(os),
        }
    }

    /// The effective expectation on `os`, as `std::env::consts::OS` spells it.
    ///
    /// `expect = "fail"` applies only on the platforms [`Self::fails_on_platform`] names;
    /// elsewhere the scenario is an ordinary `expect = "pass"`. Both runners' verdicts call this
    /// instead of reading the raw `expect` field, so a scenario the harness runs at all is held
    /// to the expectation that applies on the platform it ran on. Takes `os` as a parameter, not
    /// `std::env::consts::OS` directly, so a test can check every platform from one host.
    #[must_use]
    pub fn expect_on(&self, os: &str) -> Expect {
        if self.expect == Expect::Fail && self.fails_on_platform(os) {
            Expect::Fail
        } else {
            Expect::Pass
        }
    }
}
