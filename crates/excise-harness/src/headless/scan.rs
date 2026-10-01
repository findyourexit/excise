//! One headless scan: `excise --format json --output <scratch>/scan-report.json <fixture-root>`.
//!
//! The scan runs under the isolation of the pseudo-terminal runner: an empty environment rebuilt
//! from `TERM`, `COLORTERM`, and `LANG`, a scratch `HOME`, configuration, working directory,
//! temporary directory, and scan-store directory, and a fixture root that carries the ownership
//! marker. It is bounded by a deadline and kills its whole process group when that passes. When it
//! ends, the scratch area is checked for residue: everything the scan was allowed to write is the
//! report, and nothing else may be left.

use std::{
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use thiserror::Error;

use crate::{
    runner::resolve_binary,
    safety::{FixtureRoot, Scratch, ScratchError, isolated_env},
    scenario::Profile,
};

use super::process::{self, Finished, ProcessError};

/// What one scan needs.
#[derive(Debug, Clone, Copy)]
pub struct ScanRequest<'a> {
    /// The `excise` binary under test.
    pub binary: &'a Path,
    /// The fixture to scan: a directory that carries the ownership marker.
    pub fixture: &'a FixtureRoot,
    /// The existing directory the scratch area is created in.
    pub work_dir: &'a Path,
    /// The profile to scan under: `default` or `deterministic`, the two that change how a scan
    /// runs. The others only change how the terminal looks.
    pub profile: Profile,
    /// How long the scan may take before its process group is killed.
    pub timeout: Duration,
}

/// A scan could not be run.
#[derive(Debug, Error)]
pub enum ScanError {
    /// The binary cannot be used.
    #[error("{0}")]
    Binary(String),
    /// The profile only changes how the terminal looks, so it means nothing to a scan.
    #[error(
        "the `{0}` profile only changes how the terminal looks and does not apply to a headless scan"
    )]
    Profile(Profile),
    /// The scratch area could not be created or read.
    #[error(transparent)]
    Scratch(#[from] ScratchError),
    /// The process could not be run.
    #[error(transparent)]
    Process(#[from] ProcessError),
}

/// A scan that has ended. It owns its scratch area, and with it the report: dropping the run
/// deletes both.
#[derive(Debug)]
pub struct ScanRun {
    /// How the process ended, and what was measured.
    pub finished: Finished,
    /// What the scan left in its scratch area besides the report: nothing, when the scan is clean.
    pub residue: Vec<String>,
    scratch: Scratch,
}

impl ScanRun {
    /// Where the report is, or would be: the file `--output` named.
    #[must_use]
    pub fn report_path(&self) -> PathBuf {
        self.scratch.report()
    }

    /// Keeps the scratch area on disk when the run is dropped, and returns where it is.
    pub fn keep(&mut self) -> PathBuf {
        self.scratch.keep()
    }
}

/// Runs one scan.
///
/// # Errors
///
/// Returns [`ScanError`] when the binary or the profile cannot be used, the scratch area cannot be
/// made, or the process cannot be run. A scan that fails, times out, or writes nothing is a
/// successful call: what it did is in the [`ScanRun`].
pub fn run_scan(request: &ScanRequest<'_>) -> Result<ScanRun, ScanError> {
    if !matches!(request.profile, Profile::Default | Profile::Deterministic) {
        return Err(ScanError::Profile(request.profile));
    }
    let program =
        resolve_binary(request.binary).map_err(|error| ScanError::Binary(error.to_string()))?;
    let scratch = Scratch::create(request.work_dir)?;
    let mut command = Command::new(&program);
    command
        .args(["--format", "json", "--output"])
        .arg(scratch.report())
        .arg(request.fixture.path())
        .env_clear()
        .envs(isolated_env(&scratch, request.profile, false))
        .current_dir(scratch.cwd());
    let finished = process::run(&mut command, request.timeout, true)?;
    let residue = scratch.residue()?;
    Ok(ScanRun {
        finished,
        residue,
        scratch,
    })
}
