//! Running one scenario under one profile.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use crate::{
    metrics::milliseconds,
    pty::{PtySession, SpawnSpec},
    report::{FixtureIdentity, Rusage, Verdict},
    safety::{FixtureRoot, FixtureSnapshot, ProfileSettings, Scratch, isolated_env},
    scenario::{Profile, Scenario},
};

use super::{
    bundle::{self, Bundle, Invocation},
    exec::Executor,
    outcome::{RunError, StepFailure, Stop},
    plan::prepare,
    verdict::{Outcome, verdict},
};

/// How long the output of a killed program is read for the failure bundle.
const POST_MORTEM_QUIET: Duration = Duration::from_millis(20);
const POST_MORTEM_LIMIT: Duration = Duration::from_millis(500);

/// What one run needs.
#[derive(Debug, Clone, Copy)]
pub struct RunRequest<'a> {
    /// The scenario. The runner validates it before doing anything else.
    pub scenario: &'a Scenario,
    /// The profile to run it under.
    pub profile: Profile,
    /// The `excise` binary under test.
    pub binary: &'a Path,
    /// An already-materialized fixture root. The runner refuses a root without the ownership
    /// marker.
    pub fixture_root: &'a Path,
    /// The existing directory the scratch area and the recording are created in.
    pub work_dir: &'a Path,
    /// Where to write a failure bundle if the scenario fails. `None` writes none.
    pub bundle_dir: Option<&'a Path>,
    /// The command that reruns this scenario, for the failure document.
    pub repro_command: &'a str,
    /// The seed the fixture was generated from, for the failure document.
    pub fixture_seed: u64,
    /// Keeps the scratch area on disk instead of deleting it.
    pub keep_scratch: bool,
}

/// What a run produced.
#[derive(Debug)]
pub struct RunReport {
    /// The scenario name.
    pub scenario: String,
    /// The profile it ran under.
    pub profile: Profile,
    /// The verdict, with strict xfail applied.
    pub verdict: Verdict,
    /// Wall time from the start of the run to the end of the cleanup.
    pub duration: Duration,
    /// The named measurements. Every value is finite.
    pub metrics: BTreeMap<String, f64>,
    /// The failed step, when the scenario failed.
    pub failure: Option<StepFailure>,
    /// Why the harness could not run the scenario, when it could not.
    pub error: Option<String>,
    /// The failure bundle directory, when one was written.
    pub bundle: Option<PathBuf>,
    /// The kept scratch area, when the request asked for it.
    pub kept_scratch: Option<PathBuf>,
    /// The digest of the fixture the run started from.
    pub fixture_digest: String,
}

impl RunReport {
    /// What happened, without the strict-xfail interpretation.
    #[must_use]
    pub const fn outcome(&self) -> Outcome {
        if self.error.is_some() {
            Outcome::Errored
        } else if self.failure.is_some() {
            Outcome::Failed
        } else {
            Outcome::Passed
        }
    }
}

/// Runs `request.scenario` under `request.profile` in a pseudo-terminal.
///
/// The program is spawned with an isolated environment against the fixture root, driven through
/// the scenario's steps, and always ended before this function returns: the whole process group is
/// killed on any failure or timeout. The scratch area and the recording are deleted unless the
/// request keeps them.
///
/// This function never panics on a scenario or harness problem. A run that could not happen has the
/// verdict `error` and the reason in [`RunReport::error`].
#[must_use]
pub fn run_scenario(request: &RunRequest<'_>) -> RunReport {
    let started = Instant::now();
    let mut report = RunReport {
        scenario: request.scenario.name.clone(),
        profile: request.profile,
        verdict: Verdict::Error,
        duration: Duration::ZERO,
        metrics: BTreeMap::new(),
        failure: None,
        error: None,
        bundle: None,
        kept_scratch: None,
        fixture_digest: String::new(),
    };
    if let Err(error) = execute(request, &mut report) {
        report.error = Some(error.to_string());
    }
    report.verdict = verdict(
        request.scenario.expect_on(std::env::consts::OS),
        report.outcome(),
    );
    report.duration = started.elapsed();
    report
}

fn execute(request: &RunRequest<'_>, report: &mut RunReport) -> Result<(), RunError> {
    let scenario = request.scenario;
    scenario.validate().map_err(RunError::InvalidScenario)?;
    let prepared = prepare(scenario)?;
    let fixture = FixtureRoot::open(request.fixture_root)?;
    let binary = resolve_binary(request.binary)?;
    let baseline = FixtureSnapshot::take(fixture.path())?;
    report.fixture_digest = baseline.digest();

    let mut scratch = Scratch::create(request.work_dir)?;
    let recording = tempfile::Builder::new()
        .prefix("xh-cast-")
        .suffix(".cast")
        .tempfile_in(request.work_dir)
        .map_err(|source| RunError::Io {
            context: "cannot create the recording file",
            source,
        })?
        .into_temp_path();

    let settings = ProfileSettings::for_profile(request.profile);
    let spec = SpawnSpec {
        program: binary,
        args: vec![fixture.path().as_os_str().to_owned()],
        env: isolated_env(&scratch, request.profile, true),
        cwd: scratch.cwd(),
        cols: settings.cols.unwrap_or(scenario.terminal.cols),
        rows: scenario.terminal.rows,
        recording: Some(recording.to_path_buf()),
        title: Some(format!("{} ({})", scenario.name, request.profile)),
    };
    let session = PtySession::spawn(&spec)?;
    let mut executor = Executor::new(scenario, &prepared, &fixture, &scratch, &baseline, session);

    let result = executor.run();
    match result {
        Ok(()) => {
            report.metrics = executor.metrics();
            executor.session.kill();
        }
        Err(Stop::Error(error)) => {
            executor.session.kill();
            return Err(error);
        }
        Err(Stop::Fail(failure)) => {
            // Kill the whole process group first, then collect the evidence.
            executor.session.kill();
            let _ = executor.session.drain(POST_MORTEM_QUIET, POST_MORTEM_LIMIT);
            let _ = executor.pump();
            report.metrics = executor.metrics();
            if let Some(dir) = request.bundle_dir {
                write_bundle(
                    dir,
                    request,
                    &spec,
                    &mut executor,
                    &failure,
                    &recording,
                    &scratch,
                    &baseline,
                )?;
                report.bundle = Some(dir.to_path_buf());
            }
            report.failure = Some(*failure);
        }
    }
    drop(executor);
    if request.keep_scratch {
        report.kept_scratch = Some(scratch.keep());
        let _ = recording.keep();
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn write_bundle(
    dir: &Path,
    request: &RunRequest<'_>,
    spec: &SpawnSpec,
    executor: &mut Executor<'_>,
    failure: &StepFailure,
    recording: &Path,
    scratch: &Scratch,
    baseline: &FixtureSnapshot,
) -> Result<(), RunError> {
    executor.session.finish_recording().map_err(RunError::Pty)?;
    let screen = executor.session.screen();
    let cpu = executor.session.cpu_time().unwrap_or_default();
    let invocation = Invocation {
        program: spec.program.clone(),
        args: spec
            .args
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect(),
        env: spec.env.iter().map(lossy_pair).collect(),
        cwd: spec.cwd.clone(),
    };
    let (rows, cols) = screen.size();
    bundle::write(
        dir,
        &Bundle {
            scenario: &request.scenario.name,
            profile: request.profile,
            failure,
            screen_text: &screen.text(),
            screen_size: (rows, cols),
            cursor: screen.cursor(),
            modes: executor.session.modes(),
            recording,
            events: &scratch.events(),
            rusage: Rusage {
                max_rss_bytes: executor.session.sampler().peak_memory_bytes().unwrap_or(0),
                user_ms: whole_milliseconds(cpu.user),
                sys_ms: whole_milliseconds(cpu.system),
            },
            fixture: FixtureIdentity {
                hash: baseline.digest(),
                seed: request.fixture_seed,
            },
            repro_command: request.repro_command,
            invocation: &invocation,
        },
    )
    .map_err(|source| RunError::Io {
        context: "cannot write the failure bundle",
        source,
    })
}

fn lossy_pair((name, value): &(OsString, OsString)) -> (String, String) {
    (
        name.to_string_lossy().into_owned(),
        value.to_string_lossy().into_owned(),
    )
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn whole_milliseconds(duration: Duration) -> u64 {
    milliseconds(duration).round() as u64
}

/// The absolute path of the binary, which must be an executable file. A cleared environment has no
/// `PATH`, so the pseudo-terminal library needs the absolute path.
pub(crate) fn resolve_binary(path: &Path) -> Result<PathBuf, RunError> {
    let binary_error = |reason: String| RunError::Binary {
        path: path.to_path_buf(),
        reason,
    };
    let absolute =
        fs::canonicalize(path).map_err(|error: io::Error| binary_error(error.to_string()))?;
    let metadata = fs::metadata(&absolute).map_err(|error| binary_error(error.to_string()))?;
    if !metadata.is_file() {
        return Err(binary_error("it is not a file".to_owned()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(binary_error("it is not executable".to_owned()));
        }
    }
    Ok(absolute)
}
