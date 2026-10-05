//! The scenario × profile matrix behind `cargo xtask e2e`.
//!
//! [`run_e2e`] runs every selected scenario under every selected profile, `repeat` times each. Each
//! run gets a fresh fixture (a disposable copy from the fixture generator) and a fresh scratch area
//! under a short work directory, and both are deleted afterwards unless the options keep them.
//! The result is a [`E2eReport`]: one [`RunRecord`] per run, the `harness-summary` document that
//! was written to `<out_root>/<run-id>/summary.json`, and a verdict table.
//!
//! # Tiers
//!
//! `--quick` runs scenarios tagged `quick` (the default), `--full` adds `full`, and `--nightly`
//! adds `nightly` too; a scenario named with `--scenario` runs whatever its tier, but a scenario
//! outside its `platforms` is always skipped, with the reason, even when it is named. The quick
//! tier also limits the *profiles* that run: the two profiles every lifecycle scenario must pass
//! under (`default` and `deterministic`); `--full` and `--nightly` run every profile a scenario
//! declares. A scenario is selected for a profile only if it declares that profile.
//!
//! # Latency scale
//!
//! [`E2eOptions::latency_scale`] multiplies the limits of the latency budgets in every run of the
//! matrix (see [`LatencyScale`]): pull-request CI holds them to twice their strict value. A scaled
//! matrix says so: the summary carries `latency_budget_scale`, and the verdict table's last line
//! names the factor. A strict matrix writes neither.
//!
//! # Informational timing
//!
//! [`E2eOptions::timing_informational`] makes the matrix report a latency budget that a scenario
//! misses instead of failing on it, for a hosted machine that is slower than the one the budgets
//! were set on. The miss is a warning: the run's [`RunReport::timing_warnings`] and its result in
//! the summary hold it, the summary says `timing_informational`, and the verdict table lists it
//! and counts it on its last line. It is not a blocking verdict. Every other check still fails the
//! scenario (the other budgets, a wait that times out, a step that fails, residue), and a scenario
//! that is expected to fail on the platform keeps its strict verdict.
//!
//! # Quick-tier time
//!
//! A run of the whole quick tier (`quick`, with no scenario named, no profile chosen, and no
//! repetition) times itself, from just before the warm-up launch to the end of its last run, and
//! is held to [`QUICK_BUDGET`]. The summary records the time (`quick_tier_ms`), the verdict
//! table's last line prints it against the budget, and [`E2eReport::quick_tier`] holds it. A tier
//! over its budget fails the run on the reference machine, where `EXCISE_HARNESS_REFERENCE=1`, and
//! is a warning in the table elsewhere; the message names the five slowest runs, and a run that
//! failed for another reason keeps that failure. [`run_e2e`] reads the environment; the tests reach
//! the budget and the machine through the parameters of `run_matrix` instead, without waiting.

use std::{
    collections::BTreeMap,
    fmt::Write as _,
    fs, io,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime},
};

use thiserror::Error;

use crate::{
    fixture::{Fixtures, PRIVILEGED_ENV, PrivilegedOptIn, RunCopy},
    report::{
        BinaryIdentity, Document, HarnessSummary, ScenarioResult, SchemaVersion, SummaryKind, Tier,
        TimingWarning, Verdict,
    },
    run_support::{
        compact_utc, format_ms, host_name, median, point_latest_at, render_rows, rfc3339,
        sha256_file, worst,
    },
    safety::{Scratch, cgroup, isolated_env},
    scenario::{LoadError, Profile, Scenario, Tier as ScenarioTier, ValidationErrors},
};

use super::{
    budget::LatencyScale,
    run::{RunReport, RunRequest, resolve_binary, run_scenario},
    tier_time::{QuickBudget, QuickTierTime, REFERENCE_ENV, is_reference_machine},
    work::work_base,
};

/// How long the warm-up launch may take. A cold first launch was measured at under half a second.
const WARM_UP_TIMEOUT: Duration = Duration::from_secs(10);

/// The profiles of the quick tier.
const QUICK_PROFILES: [Profile; 2] = [Profile::Default, Profile::Deterministic];

/// How long the whole quick tier may take: two minutes, so that running it on every iteration
/// stays cheap. A tier over it fails the run on the reference machine and warns elsewhere (see
/// [`QuickTierTime`]).
const QUICK_BUDGET: Duration = Duration::from_secs(120);

/// Whether a scenario tagged `scenario_tier` runs when the matrix is asked for `tier`.
const fn tier_includes(tier: Tier, scenario_tier: ScenarioTier) -> bool {
    match scenario_tier {
        ScenarioTier::Quick => true,
        ScenarioTier::Full => !matches!(tier, Tier::Quick),
        ScenarioTier::Nightly => matches!(tier, Tier::Nightly | Tier::Weekly),
    }
}

/// A scenario this matrix did not attempt, and why.
#[derive(Debug, Clone)]
pub struct SkippedScenario {
    /// The scenario's name.
    pub name: String,
    /// Why it was not run.
    pub reason: String,
}

/// What to run and where to put the results.
#[derive(Debug, Clone)]
pub struct E2eOptions {
    /// The `excise` binary under test.
    pub binary: PathBuf,
    /// The tier, which selects scenarios whose own `tier` is no higher and, for `quick`, limits
    /// the profiles that run too.
    pub tier: Tier,
    /// The scenarios to consider, already loaded.
    pub scenarios: Vec<Scenario>,
    /// Whether `scenarios` was narrowed to scenarios named with `--scenario`: when true, each one
    /// runs whatever its tier. Platform selection always applies.
    pub named: bool,
    /// Limits the run to these profiles. Empty means every profile the tier allows.
    pub profiles: Vec<Profile>,
    /// How many times each scenario runs under each profile.
    pub repeat: u32,
    /// Keeps each run's fixture and scratch area instead of deleting them.
    pub keep_fixture: bool,
    /// The output root, normally `target/excise-e2e`.
    pub out_root: PathBuf,
    /// The directory fixtures and scratch areas are built in. `None` uses [`work_base`].
    pub work_dir: Option<PathBuf>,
    /// The 40-character commit the binary was built from.
    pub git_sha: String,
    /// The factor every scenario's latency budgets are multiplied by (see [`LatencyScale`]). A
    /// scale other than [`LatencyScale::STRICT`] is recorded in the summary.
    pub latency_scale: LatencyScale,
    /// Whether a latency budget that a scenario misses is reported as a warning and does not fail
    /// the run (see [`RunRequest::timing_informational`]). The summary says so.
    pub timing_informational: bool,
}

/// The matrix could not be run.
#[derive(Debug, Error)]
pub enum E2eError {
    /// Nothing matched the scenario and profile selection. The message lists every scenario that
    /// was skipped, with the reason, so a named scenario that cannot run here says why.
    #[error("no scenario runs under the selected profiles ({selected}){}", skipped_lines(.skipped))]
    NothingToRun {
        /// The tier, or the profiles named on the command line.
        selected: String,
        /// The scenarios that were skipped, with the reason.
        skipped: Vec<SkippedScenario>,
    },
    /// A scenario file could not be loaded.
    #[error(transparent)]
    Load(#[from] LoadError),
    /// A scenario file is not valid.
    #[error("scenario `{name}` is invalid: {errors}")]
    Invalid {
        /// The scenario.
        name: String,
        /// Every broken rule.
        errors: ValidationErrors,
    },
    /// A scenario file is named differently from the scenario inside it.
    #[error("`{}` holds the scenario `{name}`; a scenario file is named after its scenario", path.display())]
    Misnamed {
        /// The file.
        path: PathBuf,
        /// The scenario name inside it.
        name: String,
    },
    /// A file or directory could not be read or written.
    #[error("{context}: {source}")]
    Io {
        /// What was being done.
        context: String,
        /// The underlying error.
        source: io::Error,
    },
    /// The summary could not be rendered.
    #[error("cannot render the run summary: {0}")]
    Summary(#[from] serde_json::Error),
    /// The warm-up launch of the binary failed, so nothing was run.
    #[error("the warm-up launch of `{} --version` failed: {reason}", binary.display())]
    WarmUp {
        /// The binary under test.
        binary: PathBuf,
        /// What went wrong.
        reason: String,
    },
}

/// The skipped scenarios as `SKIP <name>: <reason>` lines, each after a line break, as the verdict
/// table prints them.
fn skipped_lines(skipped: &[SkippedScenario]) -> String {
    let mut lines = String::new();
    for skipped in skipped {
        let _ = write!(lines, "\nSKIP {}: {}", skipped.name, skipped.reason);
    }
    lines
}

fn io_error(context: impl Into<String>) -> impl FnOnce(io::Error) -> E2eError {
    let context = context.into();
    move |source| E2eError::Io { context, source }
}

/// One run of the matrix.
#[derive(Debug)]
pub struct RunRecord {
    /// The one-based repetition of this scenario under this profile.
    pub repetition: u32,
    /// What the run produced.
    pub report: RunReport,
    /// The kept work directory (fixture and scratch), when the options keep it.
    pub kept_workspace: Option<PathBuf>,
}

/// The outcome of a whole matrix.
#[derive(Debug)]
pub struct E2eReport {
    /// The `harness-summary` document that was written.
    pub summary: HarnessSummary,
    /// Where it was written.
    pub summary_path: PathBuf,
    /// The output directory of this run.
    pub run_dir: PathBuf,
    /// Every run, in the order they ran.
    pub records: Vec<RunRecord>,
    /// Scenarios this matrix did not attempt, with the reason.
    pub skipped: Vec<SkippedScenario>,
    /// How long the whole quick tier took, held to its budget. `None` unless this run was the
    /// whole quick tier: `--scenario`, `--profile`, and `--repeat` make it something else.
    pub quick_tier: Option<QuickTierTime>,
}

impl E2eReport {
    /// Whether the run passed: no run has a blocking verdict, and the quick tier did not fail for
    /// its time (see [`E2eReport::failure`]).
    #[must_use]
    pub fn is_success(&self) -> bool {
        self.failure().is_none()
    }

    /// Why the run failed, in one line, or `None` when it passed.
    ///
    /// A `fail`, `xpass`, or `error` fails the run, and a run that already failed keeps that
    /// failure whatever its time was: the verdict table still reports the time. Otherwise the run
    /// fails when the whole quick tier took longer than its budget on the reference machine, and
    /// the reason names the budget and the five slowest runs.
    #[must_use]
    pub fn failure(&self) -> Option<String> {
        if self
            .records
            .iter()
            .any(|record| record.report.verdict.blocks_run())
        {
            return Some("the e2e run has blocking verdicts".to_owned());
        }
        self.quick_tier
            .filter(QuickTierTime::fails_run)?
            .overrun(&self.records)
    }
}

/// Loads every scenario in `dir`, which holds one `<name>.toml` file per scenario.
///
/// Scenarios are returned in name order. Each is parsed strictly, validated, and checked to be
/// named after its file.
///
/// # Errors
///
/// Returns the first file that cannot be read, parsed, or validated, or that is misnamed.
pub fn load_scenarios(dir: &Path) -> Result<Vec<Scenario>, E2eError> {
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)
        .map_err(io_error(format!(
            "cannot read the scenario directory `{}`",
            dir.display()
        )))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "toml")
        })
        .collect();
    paths.sort();
    let mut scenarios = Vec::with_capacity(paths.len());
    for path in paths {
        let scenario = Scenario::from_path(&path)?;
        if path.file_stem().and_then(|stem| stem.to_str()) != Some(scenario.name.as_str()) {
            return Err(E2eError::Misnamed {
                path,
                name: scenario.name,
            });
        }
        if let Err(errors) = scenario.validate() {
            return Err(E2eError::Invalid {
                name: scenario.name,
                errors,
            });
        }
        scenarios.push(scenario);
    }
    Ok(scenarios)
}

/// The profiles `scenario` runs under with these options.
fn selected_profiles(options: &E2eOptions, scenario: &Scenario) -> Vec<Profile> {
    scenario
        .profiles
        .iter()
        .copied()
        .filter(|profile| options.tier != Tier::Quick || QUICK_PROFILES.contains(profile))
        .filter(|profile| options.profiles.is_empty() || options.profiles.contains(profile))
        .collect()
}

/// Whether `options` ask for the whole quick tier, once: `quick`, with no scenario named, no
/// profile chosen, and no repetition. Anything else is a part of the tier, or more than it, so its
/// time is not the tier's and is not held to its budget.
fn is_whole_quick_tier(options: &E2eOptions) -> bool {
    options.tier == Tier::Quick
        && !options.named
        && options.profiles.is_empty()
        && options.repeat <= 1
}

/// Plans which of `options.scenarios` run on `os`, and under which profiles, and which are
/// skipped, with the reason.
///
/// A scenario outside its `platforms` is always skipped, even when `options.named`. One outside
/// `options.tier` is skipped too, unless `options.named`: a scenario named with `--scenario` runs
/// whatever its tier. One whose fixture needs a scratch volume (`fixture::spec::FixtureSpec::has_volumes`)
/// is skipped too, even when named, unless `EXCISE_HARNESS_PRIVILEGED=1` opts in: a volume is
/// privileged-adjacent on every OS (see `fixture::volume`).
fn select<'a>(
    options: &'a E2eOptions,
    os: &str,
) -> (Vec<(&'a Scenario, Profile)>, Vec<SkippedScenario>) {
    let fixtures = Fixtures::bundled();
    let mut plan = Vec::new();
    let mut skipped = Vec::new();
    for scenario in &options.scenarios {
        if !scenario.runs_on(os) {
            skipped.push(SkippedScenario {
                name: scenario.name.clone(),
                reason: format!(
                    "`platforms` is {:?}, which does not include `{os}`",
                    scenario.effective_platforms()
                ),
            });
            continue;
        }
        if !options.named && !tier_includes(options.tier, scenario.tier) {
            skipped.push(SkippedScenario {
                name: scenario.name.clone(),
                reason: format!(
                    "its tier is `{}`, outside the `{}` tier",
                    scenario.tier, options.tier
                ),
            });
            continue;
        }
        if fixtures
            .spec(&scenario.fixture)
            .is_ok_and(|spec| spec.has_volumes())
            && PrivilegedOptIn::from_env().is_none()
        {
            skipped.push(SkippedScenario {
                name: scenario.name.clone(),
                reason: format!(
                    "its fixture `{}` needs a scratch volume, which needs `{PRIVILEGED_ENV}=1`",
                    scenario.fixture
                ),
            });
            continue;
        }
        if scenario.cgroup_memory_cap {
            if cgroup::CgroupOptIn::from_env().is_none() {
                skipped.push(SkippedScenario {
                    name: scenario.name.clone(),
                    reason: format!(
                        "needs the Linux cgroup memory cap, which needs `{}=1`",
                        cgroup::OPT_IN_ENV
                    ),
                });
                continue;
            }
            if let Err(reason) = cgroup::detect() {
                skipped.push(SkippedScenario {
                    name: scenario.name.clone(),
                    reason: format!("needs the Linux cgroup memory cap: {reason}"),
                });
                continue;
            }
        }
        for profile in selected_profiles(options, scenario) {
            plan.push((scenario, profile));
        }
    }
    (plan, skipped)
}

/// Launches `binary --version` once, in an isolated environment, and waits for it to end.
///
/// The first launch of a new binary costs more than a later one: macOS assesses its code signature
/// once, which was measured at over 300 ms against 5 to 8 ms afterwards. Without a warm-up the
/// first frame of a run would depend on whether an earlier launch had already paid that cost, so
/// the matrix pays it before its first measured session.
///
/// # Errors
///
/// Returns [`E2eError::WarmUp`] if the binary is not an absolute path to an executable file, cannot
/// be started, exits unsuccessfully, or does not end within `timeout`. A failed warm-up is never
/// skipped: a binary that cannot print its version cannot be measured.
fn warm_up(binary: &Path, work_dir: &Path, timeout: Duration) -> Result<(), E2eError> {
    let fail = |reason: String| E2eError::WarmUp {
        binary: binary.to_path_buf(),
        reason,
    };
    let program = resolve_binary(binary).map_err(|error| fail(error.to_string()))?;
    let scratch = Scratch::create(work_dir)
        .map_err(|error| fail(format!("its scratch area could not be created: {error}")))?;
    let mut child = Command::new(&program)
        .arg("--version")
        .env_clear()
        .envs(isolated_env(&scratch, Profile::Default, false, None))
        .current_dir(scratch.cwd())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| fail(format!("it could not be started: {error}")))?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => return Err(fail(format!("it ended with {status}"))),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(fail(format!(
                    "it did not end within {} ms",
                    timeout.as_millis()
                )));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(fail(format!("it could not be waited for: {error}")));
            }
        }
    }
}

/// Runs the matrix and writes the summary.
///
/// `progress` is called with each run as it finishes, so a caller can print it.
///
/// The binary is launched once with `--version` before the first run (see [`warm_up`]), so that no
/// measured session pays the one-time cost of a new binary's first launch.
///
/// A run of the whole quick tier is timed from just before that launch to the end of its last run,
/// and held to [`QUICK_BUDGET`]: over it, the run fails on the reference machine
/// (`EXCISE_HARNESS_REFERENCE=1`) and only warns elsewhere. See [`E2eReport::failure`].
///
/// # Errors
///
/// Returns an error if nothing was selected, if the warm-up launch fails, or if the output
/// directory or the summary cannot be written. A scenario that fails or cannot run is not an error of the matrix: it is a run with a
/// blocking verdict in the report.
pub fn run_e2e(
    options: &E2eOptions,
    progress: impl FnMut(&RunRecord),
) -> Result<E2eReport, E2eError> {
    let budget = QuickBudget {
        limit: QUICK_BUDGET,
        enforced: is_reference_machine(),
    };
    run_matrix(options, budget, progress)
}

/// [`run_e2e`] with the quick tier's budget given, so that a test can hold a run to any budget
/// without waiting for it: [`warm_up`] takes its timeout the same way.
fn run_matrix(
    options: &E2eOptions,
    budget: QuickBudget,
    mut progress: impl FnMut(&RunRecord),
) -> Result<E2eReport, E2eError> {
    let (plan, skipped) = select(options, std::env::consts::OS);
    if plan.is_empty() {
        let selected = if options.profiles.is_empty() {
            format!("the {} tier", options.tier)
        } else {
            options
                .profiles
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        };
        return Err(E2eError::NothingToRun { selected, skipped });
    }

    let work_dir = options.work_dir.clone().unwrap_or_else(work_base);
    fs::create_dir_all(&work_dir).map_err(io_error(format!(
        "cannot create the work directory `{}`",
        work_dir.display()
    )))?;
    let began = Instant::now();
    warm_up(&options.binary, &work_dir, WARM_UP_TIMEOUT)?;

    let started_at = SystemTime::now();
    let run_id = format!("{}-{}", compact_utc(started_at), std::process::id());
    let run_dir = options.out_root.join(&run_id);
    fs::create_dir_all(&run_dir)
        .map_err(io_error(format!("cannot create `{}`", run_dir.display())))?;

    let mut records = Vec::new();
    for (scenario, profile) in plan {
        for repetition in 1..=options.repeat.max(1) {
            let record = run_one(options, scenario, profile, repetition, &run_dir, &work_dir);
            progress(&record);
            records.push(record);
        }
    }

    let elapsed = began.elapsed();
    let quick_tier = is_whole_quick_tier(options).then_some(QuickTierTime { elapsed, budget });

    let summary = HarnessSummary {
        document_kind: SummaryKind::default(),
        schema_version: SchemaVersion,
        run_id,
        tier: options.tier,
        started_at: rfc3339(started_at),
        finished_at: rfc3339(SystemTime::now()),
        host: host_name(),
        os: std::env::consts::OS.to_owned(),
        arch: std::env::consts::ARCH.to_owned(),
        excise_binary: BinaryIdentity {
            path: options.binary.display().to_string(),
            sha256: sha256_file(&options.binary).map_err(io_error(format!(
                "cannot hash `{}`",
                options.binary.display()
            )))?,
        },
        git_sha: options.git_sha.clone(),
        latency_budget_scale: (!options.latency_scale.is_strict())
            .then(|| options.latency_scale.factor()),
        timing_informational: options.timing_informational,
        quick_tier_ms: quick_tier.as_ref().map(QuickTierTime::millis),
        scenarios: records.iter().map(result_of).collect(),
    };
    let summary_path = run_dir.join("summary.json");
    fs::write(&summary_path, summary.to_json_pretty()?).map_err(io_error(format!(
        "cannot write `{}`",
        summary_path.display()
    )))?;
    point_latest_at(&options.out_root, &summary.run_id)
        .map_err(io_error("cannot update the `latest` pointer"))?;
    Ok(E2eReport {
        summary,
        summary_path,
        run_dir,
        records,
        skipped,
        quick_tier,
    })
}

fn run_one(
    options: &E2eOptions,
    scenario: &Scenario,
    profile: Profile,
    repetition: u32,
    run_dir: &Path,
    work_dir: &Path,
) -> RunRecord {
    let workspace = match tempfile::Builder::new().prefix("xh-").tempdir_in(work_dir) {
        Ok(workspace) => workspace,
        Err(error) => {
            return failed_to_start(
                scenario,
                profile,
                repetition,
                format!("cannot create a work directory: {error}"),
            );
        }
    };
    let mut fixture = match Fixtures::bundled().run_copy(&scenario.fixture, workspace.path()) {
        Ok(fixture) => fixture,
        Err(error) => {
            return failed_to_start(scenario, profile, repetition, error.to_string());
        }
    };
    let scan_store_dir = match attach_volumes_if_needed(&mut fixture, scenario) {
        Ok(scan_store_dir) => scan_store_dir,
        Err(reason) => return failed_to_start(scenario, profile, repetition, reason),
    };
    let bundle_dir = run_dir.join(format!("{}-{profile}-{repetition}", scenario.name));
    let repro = format!(
        "cargo xtask e2e --scenario {} --profile {profile} --keep-fixture",
        scenario.name
    );
    let report = run_scenario(&RunRequest {
        scenario,
        profile,
        binary: &options.binary,
        fixture_root: fixture.root(),
        scan_store_dir: scan_store_dir.as_deref(),
        work_dir: workspace.path(),
        bundle_dir: Some(&bundle_dir),
        repro_command: &repro,
        fixture_seed: fixture.plan().spec().seed,
        keep_scratch: options.keep_fixture,
        latency_scale: options.latency_scale,
        timing_informational: options.timing_informational,
    });
    let kept_workspace = if options.keep_fixture {
        let _ = fixture.keep();
        Some(workspace.keep())
    } else {
        None
    };
    RunRecord {
        repetition,
        report,
        kept_workspace,
    }
}

/// Attaches every volume part of `fixture`'s plan, and returns the directory
/// `scenario.scan_store_on_volume` asks `EXCISE_SCAN_STORE_DIR` to use instead of the scratch
/// area's own `store` directory.
///
/// # Errors
///
/// Returns the reason the run cannot start: the fixture has no volume part but
/// `scan_store_on_volume` is set; the opt-in is missing (`select` already filters this out, but
/// this function is defensive on its own: a fixture can plan volumes without `select` loading
/// its spec successfully, for example a spec that fails to parse); attaching failed; or
/// `scan_store_on_volume` is set and the fixture declares other than exactly one volume part.
fn attach_volumes_if_needed(
    fixture: &mut RunCopy,
    scenario: &Scenario,
) -> Result<Option<PathBuf>, String> {
    if fixture.plan().volumes().is_empty() {
        return if scenario.scan_store_on_volume {
            Err(format!(
                "`{}` sets `scan_store_on_volume`, but its fixture `{}` has no volume part",
                scenario.name, scenario.fixture
            ))
        } else {
            Ok(None)
        };
    }
    let opt_in = PrivilegedOptIn::from_env().ok_or_else(|| {
        format!(
            "its fixture `{}` needs a scratch volume, which needs `{PRIVILEGED_ENV}=1`",
            scenario.fixture
        )
    })?;
    fixture
        .attach_volumes(opt_in)
        .map_err(|error| error.to_string())?;
    if !scenario.scan_store_on_volume {
        return Ok(None);
    }
    match fixture.attached_mount_points().as_slice() {
        [mount] => Ok(Some(mount.join("store"))),
        mounts => Err(format!(
            "`{}` sets `scan_store_on_volume`, which needs exactly one volume part; its \
             fixture `{}` declares {}",
            scenario.name,
            scenario.fixture,
            mounts.len()
        )),
    }
}

/// A run that never started because its fixture could not be built.
fn failed_to_start(
    scenario: &Scenario,
    profile: Profile,
    repetition: u32,
    reason: String,
) -> RunRecord {
    RunRecord {
        repetition,
        report: RunReport {
            scenario: scenario.name.clone(),
            profile,
            verdict: Verdict::Error,
            duration: Duration::ZERO,
            metrics: BTreeMap::new(),
            failure: None,
            error: Some(reason),
            bundle: None,
            kept_scratch: None,
            fixture_digest: String::new(),
            timing_warnings: Vec::new(),
        },
        kept_workspace: None,
    }
}

fn result_of(record: &RunRecord) -> ScenarioResult {
    let report = &record.report;
    ScenarioResult {
        name: report.scenario.clone(),
        profile: report.profile,
        verdict: report.verdict,
        duration_ms: u64::try_from(report.duration.as_millis()).unwrap_or(u64::MAX),
        metrics: report.metrics.clone(),
        failure_bundle: report
            .bundle
            .as_ref()
            .map(|bundle| bundle.display().to_string()),
        timing_warnings: report.timing_warnings.clone(),
    }
}

// ---------------------------------------------------------------------------------------------
// The verdict table.

impl E2eReport {
    /// The verdict table: one line per scenario and profile, then one block per blocking run, one
    /// line per timing warning, and a block for a quick tier over its budget, then the overall
    /// verdict, whose line also carries the quick tier's time against its budget.
    #[must_use]
    pub fn table(&self) -> String {
        let mut table = render_rows(&self.summary_rows());
        self.write_skipped(&mut table);
        self.write_blocking_runs(&mut table);
        self.write_timing_warnings(&mut table);
        self.write_tier_overrun(&mut table);
        let blocking = self
            .records
            .iter()
            .filter(|record| record.report.verdict.blocks_run())
            .count();
        let scale = self
            .summary
            .latency_budget_scale
            .map_or_else(String::new, |factor| {
                format!("; latency budgets at {factor}x their strict limits")
            });
        let timing = if self.summary.timing_informational {
            format!(
                "; timing informational: {} warning(s)",
                self.timing_warnings().count()
            )
        } else {
            String::new()
        };
        let tier_time = self
            .quick_tier
            .map_or_else(String::new, |time| format!("; {}", time.against_budget()));
        let _ = writeln!(
            table,
            "\ne2e {}: {} run(s), {blocking} blocking{scale}{timing}{tier_time}; summary: {}",
            if self.is_success() { "ok" } else { "FAILED" },
            self.records.len(),
            self.summary_path.display()
        );
        table
    }

    /// One line per scenario this matrix did not attempt, with the reason.
    fn write_skipped(&self, table: &mut String) {
        for skipped in &self.skipped {
            let _ = writeln!(table, "\nSKIP {}: {}", skipped.name, skipped.reason);
        }
    }

    /// Every timing warning of every run, with the run it belongs to.
    fn timing_warnings(&self) -> impl Iterator<Item = (&RunRecord, &TimingWarning)> {
        self.records.iter().flat_map(|record| {
            record
                .report
                .timing_warnings
                .iter()
                .map(move |warning| (record, warning))
        })
    }

    /// One line per timing budget a run missed without failing for it.
    fn write_timing_warnings(&self, table: &mut String) {
        for (record, warning) in self.timing_warnings() {
            let report = &record.report;
            let _ = writeln!(
                table,
                "\nWARN {} [{}] run {}: {warning}",
                report.scenario, report.profile, record.repetition
            );
        }
    }

    /// A block for a quick tier that took longer than its budget: a failure where that fails the
    /// run, a warning with the reason it does not elsewhere.
    fn write_tier_overrun(&self, table: &mut String) {
        let Some(time) = self.quick_tier else {
            return;
        };
        let Some(overrun) = time.overrun(&self.records) else {
            return;
        };
        if time.fails_run() {
            let _ = writeln!(table, "\nFAIL {overrun}");
        } else {
            let _ = writeln!(
                table,
                "\nWARN {overrun}\n  not a failure here: only the reference machine \
                 ({REFERENCE_ENV}=1) fails on it"
            );
        }
    }

    /// The header row and one row per scenario and profile.
    fn summary_rows(&self) -> Vec<[String; 12]> {
        let mut groups: BTreeMap<(String, Profile), Vec<&RunRecord>> = BTreeMap::new();
        for record in &self.records {
            groups
                .entry((record.report.scenario.clone(), record.report.profile))
                .or_default()
                .push(record);
        }
        let mut rows = vec![
            [
                "scenario",
                "profile",
                "runs",
                "pass",
                "fail",
                "xfail",
                "xpass",
                "error",
                "wall (median)",
                "first frame (median)",
                "input p99 (worst)",
                "max stall (worst)",
            ]
            .map(str::to_owned),
        ];
        for ((scenario, profile), records) in &groups {
            let count = |verdict: Verdict| {
                records
                    .iter()
                    .filter(|record| record.report.verdict == verdict)
                    .count()
                    .to_string()
            };
            let metric = |name: &str| -> Vec<f64> {
                records
                    .iter()
                    .filter_map(|record| record.report.metrics.get(name).copied())
                    .collect()
            };
            let wall: Vec<f64> = records
                .iter()
                .map(|record| record.report.duration.as_secs_f64() * 1000.0)
                .collect();
            rows.push([
                scenario.clone(),
                profile.to_string(),
                records.len().to_string(),
                count(Verdict::Pass),
                count(Verdict::Fail),
                count(Verdict::Xfail),
                count(Verdict::Xpass),
                count(Verdict::Error),
                format_ms(median(&wall)),
                format_ms(median(&metric("first_frame_ms"))),
                format_ms(worst(&metric("input_to_frame_p99_ms"))),
                format_ms(worst(&metric("max_stall_ms"))),
            ]);
        }
        rows
    }

    /// One block per run whose verdict fails the run.
    fn write_blocking_runs(&self, table: &mut String) {
        for record in &self.records {
            let report = &record.report;
            if !report.verdict.blocks_run() {
                continue;
            }
            let _ = write!(
                table,
                "\n{} {} [{}] run {}: ",
                report.verdict.as_str().to_uppercase(),
                report.scenario,
                report.profile,
                record.repetition
            );
            match (&report.failure, &report.error) {
                (_, Some(error)) => {
                    let _ = writeln!(table, "the harness could not run it: {error}");
                }
                (Some(failure), None) => {
                    let _ = writeln!(table, "{failure}");
                }
                (None, None) => {
                    let _ = writeln!(
                        table,
                        "the scenario passed although it was expected to fail; flip it to \
                         `expect = \"pass\"`"
                    );
                }
            }
            if let Some(bundle) = &report.bundle {
                let _ = writeln!(table, "  failure bundle: {}", bundle.display());
            }
            if let Some(kept) = &record.kept_workspace {
                let _ = writeln!(table, "  kept fixture and scratch: {}", kept.display());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    use super::super::outcome::FailureCause;
    #[cfg(unix)]
    use crate::scenario::Budget;

    fn scenario(profiles: &str) -> Scenario {
        Scenario::from_toml_str(&format!(
            "schema_version = 1\nname = \"s\"\ndescription = \"d\"\nfixture = \"f\"\nprofiles = {profiles}\n[[steps]]\nstep = \"settle\"\n"
        ))
        .expect("a scenario")
    }

    fn options(tier: Tier, profiles: Vec<Profile>) -> E2eOptions {
        E2eOptions {
            binary: PathBuf::from("/bin/true"),
            tier,
            scenarios: Vec::new(),
            named: false,
            profiles,
            repeat: 1,
            keep_fixture: false,
            out_root: PathBuf::from("target/excise-e2e"),
            work_dir: None,
            git_sha: "0".repeat(40),
            latency_scale: LatencyScale::STRICT,
            timing_informational: false,
        }
    }

    #[test]
    fn the_quick_tier_runs_only_the_two_lifecycle_profiles() {
        let scenario = scenario(r#"["default", "deterministic", "narrow", "mouse-keymaps"]"#);

        assert_eq!(
            selected_profiles(&options(Tier::Quick, Vec::new()), &scenario),
            [Profile::Default, Profile::Deterministic]
        );
        assert_eq!(
            selected_profiles(&options(Tier::Full, Vec::new()), &scenario),
            [
                Profile::Default,
                Profile::Deterministic,
                Profile::Narrow,
                Profile::MouseKeymaps
            ]
        );
    }

    #[test]
    fn a_profile_selection_only_narrows_what_the_scenario_declares() {
        let scenario = scenario(r#"["default", "narrow"]"#);

        assert_eq!(
            selected_profiles(
                &options(Tier::Full, vec![Profile::Narrow, Profile::MonochromeAscii]),
                &scenario
            ),
            [Profile::Narrow],
            "a profile the scenario does not declare is never run"
        );
        assert!(
            selected_profiles(&options(Tier::Quick, vec![Profile::Narrow]), &scenario).is_empty(),
            "the quick tier does not run the narrow profile even when asked"
        );
    }

    #[test]
    fn tier_includes_every_scenario_tier_up_to_the_requested_one() {
        let table = [
            (Tier::Quick, ScenarioTier::Quick, true),
            (Tier::Quick, ScenarioTier::Full, false),
            (Tier::Quick, ScenarioTier::Nightly, false),
            (Tier::Full, ScenarioTier::Quick, true),
            (Tier::Full, ScenarioTier::Full, true),
            (Tier::Full, ScenarioTier::Nightly, false),
            (Tier::Nightly, ScenarioTier::Quick, true),
            (Tier::Nightly, ScenarioTier::Full, true),
            (Tier::Nightly, ScenarioTier::Nightly, true),
        ];
        for (tier, scenario_tier, included) in table {
            assert_eq!(
                tier_includes(tier, scenario_tier),
                included,
                "{tier} includes {scenario_tier}?"
            );
        }
    }

    #[test]
    fn a_scenario_whose_fixture_needs_a_volume_is_skipped_without_the_privileged_opt_in() {
        // `mount-boundary` is a real bundled fixture with a `volume` part. The test process never
        // sets `EXCISE_HARNESS_PRIVILEGED` (setting it would be unsafe and race every other test
        // in this process), so `select` must skip it here exactly as it would in an ordinary,
        // unprivileged run.
        assert!(
            PrivilegedOptIn::from_env().is_none(),
            "this test assumes the opt-in is not set in the test process"
        );
        let mut needs_a_volume = scenario(r#"["default"]"#);
        needs_a_volume.fixture = "mount-boundary".to_owned();
        let mut opts = options(Tier::Quick, Vec::new());
        opts.scenarios = vec![needs_a_volume];

        let (plan, skipped) = select(&opts, "linux");

        assert!(plan.is_empty(), "{plan:?}");
        assert_eq!(skipped.len(), 1, "{skipped:?}");
        assert_eq!(skipped[0].name, "s");
        assert!(
            skipped[0].reason.contains("scratch volume")
                && skipped[0].reason.contains(PRIVILEGED_ENV),
            "{}",
            skipped[0].reason
        );

        // Named with `--scenario`, the same scenario is still skipped: platform and tier
        // selection are bypassed by `named`, but the volume opt-in never is.
        opts.named = true;
        let (plan, skipped) = select(&opts, "linux");
        assert!(plan.is_empty(), "{plan:?}");
        assert_eq!(skipped.len(), 1, "{skipped:?}");
    }

    #[test]
    fn a_scenario_that_asks_for_the_cgroup_memory_cap_is_skipped_without_the_opt_in() {
        // The test process never sets `EXCISE_HARNESS_CGROUP` (setting it would be unsafe and race
        // every other test in this process), so `select` must skip it here exactly as it would in
        // an ordinary run without the opt-in.
        assert!(
            crate::safety::cgroup::CgroupOptIn::from_env().is_none(),
            "this test assumes the opt-in is not set in the test process"
        );
        let mut wants_the_cap = scenario(r#"["deterministic"]"#);
        wants_the_cap.cgroup_memory_cap = true;
        let mut opts = options(Tier::Nightly, Vec::new());
        opts.scenarios = vec![wants_the_cap];

        let (plan, skipped) = select(&opts, "linux");

        assert!(plan.is_empty(), "{plan:?}");
        assert_eq!(skipped.len(), 1, "{skipped:?}");
        assert_eq!(skipped[0].name, "s");
        assert!(
            skipped[0].reason.contains("cgroup")
                && skipped[0]
                    .reason
                    .contains(crate::safety::cgroup::OPT_IN_ENV),
            "{}",
            skipped[0].reason
        );

        // Named with `--scenario`, the same scenario is still skipped: platform and tier
        // selection are bypassed by `named`, but the cgroup opt-in never is.
        opts.named = true;
        let (plan, skipped) = select(&opts, "linux");
        assert!(plan.is_empty(), "{plan:?}");
        assert_eq!(skipped.len(), 1, "{skipped:?}");
    }

    #[test]
    fn a_scenario_whose_fixture_has_no_volume_part_is_never_skipped_for_one() {
        let ordinary = scenario(r#"["default"]"#);
        let mut opts = options(Tier::Quick, Vec::new());
        opts.scenarios = vec![ordinary];

        let (plan, skipped) = select(&opts, "linux");

        assert_eq!(plan.len(), 1);
        assert!(skipped.is_empty(), "{skipped:?}");
    }

    #[test]
    fn a_scenario_above_the_requested_tier_is_skipped_with_the_reason_unless_named() {
        let mut full_tier = scenario(r#"["default"]"#);
        full_tier.tier = ScenarioTier::Full;
        let mut opts = options(Tier::Quick, Vec::new());
        opts.scenarios = vec![full_tier];

        let (plan, skipped) = select(&opts, "linux");
        assert!(plan.is_empty(), "{plan:?}");
        assert_eq!(skipped.len(), 1, "{skipped:?}");
        assert_eq!(skipped[0].name, "s");
        assert!(skipped[0].reason.contains("full"), "{}", skipped[0].reason);
        assert!(skipped[0].reason.contains("quick"), "{}", skipped[0].reason);

        // Named with `--scenario`, the same scenario runs whatever its tier.
        opts.named = true;
        let (plan, skipped) = select(&opts, "linux");
        assert!(skipped.is_empty(), "{skipped:?}");
        assert_eq!(plan.len(), 1);
    }

    #[test]
    fn a_scenario_outside_its_platforms_is_skipped_with_the_reason_even_when_named() {
        let mut only_linux = scenario(r#"["default"]"#);
        only_linux.platforms = Some(vec!["linux".to_owned()]);
        let mut opts = options(Tier::Quick, Vec::new());
        opts.scenarios = vec![only_linux];
        opts.named = true;

        let (plan, skipped) = select(&opts, "macos");
        assert!(plan.is_empty(), "{plan:?}");
        assert_eq!(skipped.len(), 1, "{skipped:?}");
        assert_eq!(skipped[0].name, "s");
        assert!(skipped[0].reason.contains("macos"), "{}", skipped[0].reason);

        let (plan, skipped) = select(&opts, "linux");
        assert!(skipped.is_empty(), "{skipped:?}");
        assert_eq!(plan.len(), 1);
    }

    #[test]
    fn a_named_scenario_that_cannot_run_here_says_why() {
        let elsewhere = crate::platform::PLATFORMS
            .iter()
            .copied()
            .find(|os| *os != std::env::consts::OS)
            .expect("another platform");
        let mut scenario = scenario(r#"["default"]"#);
        scenario.platforms = Some(vec![elsewhere.to_owned()]);
        let mut opts = options(Tier::Full, Vec::new());
        opts.scenarios = vec![scenario];
        opts.named = true;

        let error = run_e2e(&opts, |_| {}).expect_err("nothing can run on this platform");
        let message = error.to_string();
        assert!(message.contains("SKIP s: "), "{message}");
        assert!(message.contains(elsewhere), "{message}");
    }

    #[test]
    fn scenario_files_are_loaded_in_name_order_and_checked_against_their_names() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let text = |name: &str| {
            format!(
                "schema_version = 1\nname = \"{name}\"\ndescription = \"d\"\nfixture = \"f\"\nprofiles = [\"default\"]\n[[steps]]\nstep = \"settle\"\n"
            )
        };
        fs::write(dir.path().join("b-second.toml"), text("b-second")).expect("write");
        fs::write(dir.path().join("a-first.toml"), text("a-first")).expect("write");
        fs::write(dir.path().join("notes.md"), "not a scenario").expect("write");

        let names: Vec<String> = load_scenarios(dir.path())
            .expect("scenarios")
            .into_iter()
            .map(|scenario| scenario.name)
            .collect();
        assert_eq!(names, ["a-first", "b-second"]);

        fs::write(dir.path().join("c-third.toml"), text("other-name")).expect("write");
        assert!(matches!(
            load_scenarios(dir.path()),
            Err(E2eError::Misnamed { .. })
        ));
    }

    #[test]
    fn an_invalid_scenario_file_stops_the_load() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        fs::write(
            dir.path().join("bad.toml"),
            "schema_version = 1\nname = \"bad\"\ndescription = \"d\"\nfixture = \"f\"\nprofiles = []\n[[steps]]\nstep = \"settle\"\n",
        )
        .expect("write");

        assert!(matches!(
            load_scenarios(dir.path()),
            Err(E2eError::Invalid { .. })
        ));
    }

    /// An executable shell script named `name` in `dir`.
    #[cfg(unix)]
    fn stub(dir: &Path, name: &str, script: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;

        let path = dir.join(name);
        fs::write(&path, script).expect("a stub program");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("executable");
        path
    }

    /// Options that run the shipped lifecycle scenario once, under `deterministic`, in `work`.
    #[cfg(unix)]
    fn lifecycle_options(binary: PathBuf, work: &Path) -> E2eOptions {
        let scenario = load_scenarios(&Path::new(env!("CARGO_MANIFEST_DIR")).join("scenarios"))
            .expect("the shipped scenarios load")
            .into_iter()
            .find(|scenario| scenario.name == "delete-folder-lifecycle")
            .expect("the lifecycle scenario ships");
        E2eOptions {
            binary,
            tier: Tier::Quick,
            scenarios: vec![scenario],
            named: false,
            profiles: vec![Profile::Deterministic],
            repeat: 1,
            keep_fixture: false,
            out_root: work.join("out"),
            work_dir: Some(work.to_path_buf()),
            git_sha: "0".repeat(40),
            latency_scale: LatencyScale::STRICT,
            timing_informational: false,
        }
    }

    /// Validates the document at `path` against `schema`.
    #[cfg(unix)]
    fn validate_against_schema(schema: &str, path: &Path) {
        let schema = serde_json::from_str(schema).expect("a schema");
        let validator = jsonschema::draft202012::options()
            .should_validate_formats(true)
            .build(&schema)
            .expect("the schema compiles");
        let text = fs::read_to_string(path).expect("a document");
        let document: serde_json::Value = serde_json::from_str(&text).expect("JSON");
        let violations: Vec<String> = validator
            .iter_errors(&document)
            .map(|error| error.to_string())
            .collect();
        assert!(violations.is_empty(), "{}: {violations:?}", path.display());
    }

    /// A stand-in for `excise` that prints its version and then exits at once: the run fails, and
    /// the matrix still reports it.
    #[cfg(unix)]
    #[test]
    fn a_failed_run_leaves_a_summary_and_a_bundle_that_match_their_schemas() {
        use crate::report::HarnessFailure;

        let work = tempfile::tempdir().expect("a temporary directory");
        let program = stub(
            work.path(),
            "excise",
            "#!/bin/sh\n[ \"$1\" = \"--version\" ] && exit 0\nexit 3\n",
        );

        let report =
            run_e2e(&lifecycle_options(program, work.path()), |_| {}).expect("the matrix runs");

        assert!(!report.is_success());
        let record = &report.records[0];
        assert_eq!(
            record.report.verdict,
            Verdict::Fail,
            "{:?}",
            record.report.error
        );
        validate_against_schema(HarnessSummary::SCHEMA_JSON, &report.summary_path);
        let bundle = record
            .report
            .bundle
            .as_ref()
            .expect("a failed run leaves a bundle");
        validate_against_schema(HarnessFailure::SCHEMA_JSON, &bundle.join("failure.json"));
    }

    /// A stand-in for `excise` that exits at once with status 3, whatever it is asked: every run
    /// fails, and the matrix still writes its summary.
    #[cfg(unix)]
    fn failing_stub(work: &Path) -> PathBuf {
        stub(
            work,
            "excise",
            "#!/bin/sh\n[ \"$1\" = \"--version\" ] && exit 0\nexit 3\n",
        )
    }

    #[cfg(unix)]
    #[test]
    fn a_scaled_matrix_records_its_scale_and_a_strict_one_records_none() {
        let work = tempfile::tempdir().expect("a temporary directory");
        let mut options = lifecycle_options(failing_stub(work.path()), work.path());
        let strict = run_e2e(&options, |_| {}).expect("a strict matrix runs");
        options.latency_scale = LatencyScale::new(2.0).expect("a valid scale");
        options.out_root = work.path().join("scaled");
        let scaled = run_e2e(&options, |_| {}).expect("a scaled matrix runs");

        assert_eq!(strict.summary.latency_budget_scale, None);
        assert_eq!(scaled.summary.latency_budget_scale, Some(2.0));
        for report in [&strict, &scaled] {
            validate_against_schema(HarnessSummary::SCHEMA_JSON, &report.summary_path);
        }
        let text =
            |report: &E2eReport| fs::read_to_string(&report.summary_path).expect("a summary");
        assert!(
            !text(&strict).contains("latency_budget_scale"),
            "a strict summary has no scale to record: {}",
            text(&strict)
        );
        assert!(
            text(&scaled).contains("\"latency_budget_scale\": 2.0"),
            "{}",
            text(&scaled)
        );
        assert!(
            !strict.table().contains("latency budgets at"),
            "{}",
            strict.table()
        );
        assert!(
            scaled
                .table()
                .contains("latency budgets at 2x their strict limits"),
            "{}",
            scaled.table()
        );
    }

    /// A stand-in for `excise` that writes exactly 300 bytes of output and waits: a metric
    /// (`output_bytes`) that no scheduler can change, for a latency budget to judge. The strict
    /// first-frame limit is 250 and twice it is 500.
    #[cfg(unix)]
    fn three_hundred_bytes(work: &Path) -> PathBuf {
        stub(
            work,
            "excise",
            "#!/bin/sh\n[ \"$1\" = \"--version\" ] && exit 0\nprintf '%0299dX' 0\nsleep 30\n",
        )
    }

    /// A scenario that waits for the 300 bytes and holds them to the first-frame budget.
    /// `expectation` is the scenario's `expect` lines, which come before its steps.
    #[cfg(unix)]
    fn output_against_the_first_frame_budget(name: &str, expectation: &str) -> Scenario {
        let scenario = Scenario::from_toml_str(&format!(
            "schema_version = 1\nname = \"{name}\"\ndescription = \"d\"\n\
             fixture = \"delete-file\"\nprofiles = [\"deterministic\"]\n{expectation}\
             [[steps]]\nstep = \"wait_text\"\ntext = \"0X\"\n\
             [[steps]]\nstep = \"expect_budget\"\nbudget = \"first_frame_ms\"\n\
             metric = \"output_bytes\"\n"
        ))
        .expect("a scenario");
        scenario.validate().expect("a valid scenario");
        scenario
    }

    #[cfg(unix)]
    #[test]
    fn a_scale_lets_a_scenario_meet_a_latency_limit_but_never_an_expected_failure() {
        let work = tempfile::tempdir().expect("a temporary directory");
        let program = three_hundred_bytes(work.path());
        let verdicts = |scale: LatencyScale, out: &str| -> Vec<(String, Verdict)> {
            let mut options = lifecycle_options(program.clone(), work.path());
            options.scenarios = vec![
                output_against_the_first_frame_budget("meets-a-scaled-limit", ""),
                output_against_the_first_frame_budget(
                    "expected-to-fail",
                    "expect = \"fail\"\nslice = \"SLICE\"\n",
                ),
            ];
            options.latency_scale = scale;
            options.out_root = work.path().join(out);
            let report = run_e2e(&options, |_| {}).expect("the matrix runs");
            report
                .records
                .iter()
                .map(|record| (record.report.scenario.clone(), record.report.verdict))
                .collect()
        };

        let strict = verdicts(LatencyScale::STRICT, "strict");
        let scaled = verdicts(LatencyScale::new(2.0).expect("a valid scale"), "scaled");

        // 300 bytes are over the strict limit of 250 and under twice it. The scenario that
        // expects to fail must keep failing: under the scale it would pass, and read as fixed.
        assert_eq!(
            strict,
            [
                ("meets-a-scaled-limit".to_owned(), Verdict::Fail),
                ("expected-to-fail".to_owned(), Verdict::Xfail)
            ]
        );
        assert_eq!(
            scaled,
            [
                ("meets-a-scaled-limit".to_owned(), Verdict::Pass),
                ("expected-to-fail".to_owned(), Verdict::Xfail)
            ]
        );
    }

    /// The verdicts of `report`'s runs, in the order they ran, with the name of each scenario.
    #[cfg(unix)]
    fn verdicts_of(report: &E2eReport) -> Vec<(String, Verdict)> {
        report
            .records
            .iter()
            .map(|record| (record.report.scenario.clone(), record.report.verdict))
            .collect()
    }

    #[cfg(unix)]
    #[test]
    fn informational_timing_reports_a_missed_latency_budget_without_failing_the_run() {
        let work = tempfile::tempdir().expect("a temporary directory");
        let program = three_hundred_bytes(work.path());
        let matrix = |informational: bool, out: &str| -> E2eReport {
            let mut options = lifecycle_options(program.clone(), work.path());
            options.scenarios = vec![
                output_against_the_first_frame_budget("misses-the-limit", ""),
                output_against_the_first_frame_budget(
                    "expected-to-fail",
                    "expect = \"fail\"\nslice = \"SLICE\"\n",
                ),
            ];
            options.timing_informational = informational;
            options.out_root = work.path().join(out);
            run_e2e(&options, |_| {}).expect("the matrix runs")
        };

        let strict = matrix(false, "strict");
        let informational = matrix(true, "informational");
        let summary_text =
            |report: &E2eReport| fs::read_to_string(&report.summary_path).expect("a summary");

        // 300 bytes are over the strict first-frame limit of 250. A strict matrix is as it was:
        // the miss fails the run, and nothing in the summary or the table mentions timing.
        assert_eq!(
            verdicts_of(&strict),
            [
                ("misses-the-limit".to_owned(), Verdict::Fail),
                ("expected-to-fail".to_owned(), Verdict::Xfail)
            ]
        );
        assert!(!strict.is_success());
        assert!(!strict.summary.timing_informational);
        assert!(
            !summary_text(&strict).contains("timing_"),
            "{}",
            summary_text(&strict)
        );
        assert!(!strict.table().contains("timing informational"));
        assert!(!strict.table().contains("WARN"), "{}", strict.table());

        // An informational matrix passes the miss and records it. The scenario that expects to
        // fail keeps failing, strictly: excused, it would pass and read as fixed.
        assert_eq!(
            verdicts_of(&informational),
            [
                ("misses-the-limit".to_owned(), Verdict::Pass),
                ("expected-to-fail".to_owned(), Verdict::Xfail)
            ]
        );
        assert!(informational.is_success(), "{}", informational.table());
        let [missed, documented] = informational.records.as_slice() else {
            panic!("two runs: {:?}", informational.records);
        };
        let [warning] = missed.report.timing_warnings.as_slice() else {
            panic!("one warning: {:?}", missed.report.timing_warnings);
        };
        assert_eq!(warning.budget, Budget::FirstFrameMs);
        assert_eq!(warning.metric, "output_bytes");
        assert!(warning.value > 250.0, "{warning:?}");
        assert!((warning.limit - 250.0).abs() < f64::EPSILON, "{warning:?}");
        assert!(documented.report.timing_warnings.is_empty());

        let summary = &informational.summary;
        assert!(summary.timing_informational);
        assert_eq!(
            summary.scenarios[0].timing_warnings,
            std::slice::from_ref(warning)
        );
        assert!(summary.scenarios[1].timing_warnings.is_empty());
        validate_against_schema(HarnessSummary::SCHEMA_JSON, &informational.summary_path);
        assert!(
            summary_text(&informational).contains("\"timing_informational\": true"),
            "{}",
            summary_text(&informational)
        );
        let table = informational.table();
        assert!(
            table.contains(&format!(
                "WARN misses-the-limit [deterministic] run 1: {warning}"
            )),
            "{table}"
        );
        assert!(
            table.contains("timing informational: 1 warning(s)"),
            "{table}"
        );
    }

    /// A scenario that misses the first-frame budget (300 bytes against 250) and then reaches
    /// `last_step`, a step in TOML that is not a timing check.
    #[cfg(unix)]
    fn misses_the_budget_then(name: &str, last_step: &str) -> Scenario {
        let scenario = Scenario::from_toml_str(&format!(
            "schema_version = 1\nname = \"{name}\"\ndescription = \"d\"\n\
             fixture = \"delete-file\"\nprofiles = [\"deterministic\"]\n\
             [[steps]]\nstep = \"wait_text\"\ntext = \"0X\"\n\
             [[steps]]\nstep = \"expect_budget\"\nbudget = \"first_frame_ms\"\n\
             metric = \"output_bytes\"\n{last_step}"
        ))
        .expect("a scenario");
        scenario.validate().expect("a valid scenario");
        scenario
    }

    #[cfg(unix)]
    #[test]
    fn a_failure_that_is_not_timing_still_fails_an_informational_matrix() {
        let work = tempfile::tempdir().expect("a temporary directory");
        let mut options = lifecycle_options(three_hundred_bytes(work.path()), work.path());
        options.scenarios = vec![
            // 300 bytes of output against a limit of none: a resource budget, never timing.
            misses_the_budget_then(
                "then-a-resource-budget",
                "[[steps]]\nstep = \"expect_budget\"\nbudget = \"idle_output_bytes\"\n\
                 metric = \"output_bytes\"\n",
            ),
            misses_the_budget_then(
                "then-a-wait-that-times-out",
                "[[steps]]\nstep = \"wait_text\"\ntext = \"text that is never printed\"\n\
                 timeout_ms = 300\n",
            ),
        ];
        options.timing_informational = true;

        let report = run_e2e(&options, |_| {}).expect("the matrix runs");

        assert!(!report.is_success(), "{}", report.table());
        assert_eq!(
            verdicts_of(&report),
            [
                ("then-a-resource-budget".to_owned(), Verdict::Fail),
                ("then-a-wait-that-times-out".to_owned(), Verdict::Fail)
            ]
        );
        let causes: Vec<FailureCause> = report
            .records
            .iter()
            .map(|record| record.report.failure.as_ref().expect("a failed step").cause)
            .collect();
        assert_eq!(causes, [FailureCause::Mismatch, FailureCause::Timeout]);
        for record in &report.records {
            assert_eq!(
                record.report.timing_warnings.len(),
                1,
                "the miss before the failure is still recorded: {:?}",
                record.report.timing_warnings
            );
        }
        validate_against_schema(HarnessSummary::SCHEMA_JSON, &report.summary_path);
        let table = report.table();
        assert!(table.contains("FAIL then-a-resource-budget"), "{table}");
        assert!(table.contains("e2e FAILED"), "{table}");
        assert!(
            table.contains("timing informational: 2 warning(s)"),
            "{table}"
        );
    }

    /// A harness error after an informational miss still reports the miss. Here the step after
    /// the miss fails, and its failure bundle cannot be written because its directory would be
    /// under a regular file: the run errors, and keeps the warning it measured.
    #[cfg(unix)]
    #[test]
    fn a_harness_error_after_an_informational_miss_keeps_the_warning() {
        let work = tempfile::tempdir().expect("a temporary directory");
        let program = three_hundred_bytes(work.path());
        let scenario = misses_the_budget_then(
            "then-a-bundle-that-cannot-be-written",
            "[[steps]]\nstep = \"expect_budget\"\nbudget = \"idle_output_bytes\"\n\
             metric = \"output_bytes\"\n",
        );
        let fixture = Fixtures::bundled()
            .run_copy(&scenario.fixture, work.path())
            .expect("a copy of the fixture");
        let not_a_directory = work.path().join("not-a-directory");
        fs::write(&not_a_directory, "").expect("a regular file");
        let bundle_dir = not_a_directory.join("bundle");

        let report = run_scenario(&RunRequest {
            scenario: &scenario,
            profile: Profile::Deterministic,
            binary: &program,
            fixture_root: fixture.root(),
            scan_store_dir: None,
            work_dir: work.path(),
            bundle_dir: Some(&bundle_dir),
            repro_command: "cargo xtask e2e",
            fixture_seed: fixture.plan().spec().seed,
            keep_scratch: false,
            latency_scale: LatencyScale::STRICT,
            timing_informational: true,
        });

        assert_eq!(report.verdict, Verdict::Error, "{:?}", report.error);
        let [warning] = report.timing_warnings.as_slice() else {
            panic!(
                "the miss before the error is kept: {:?}",
                report.timing_warnings
            );
        };
        assert_eq!(warning.budget, Budget::FirstFrameMs);
    }

    /// A scenario that waits for the 300 bytes `three_hundred_bytes` prints and asks nothing else
    /// of the program, under both quick-tier profiles. Against that stand-in it passes in a
    /// moment; against `failing_stub`, which exits at once, it fails.
    #[cfg(unix)]
    fn waits_for_the_output() -> Scenario {
        let scenario = Scenario::from_toml_str(
            "schema_version = 1\nname = \"waits-for-the-output\"\ndescription = \"d\"\n\
             fixture = \"delete-file\"\nprofiles = [\"default\", \"deterministic\"]\n\
             [[steps]]\nstep = \"wait_text\"\ntext = \"0X\"\n",
        )
        .expect("a scenario");
        scenario.validate().expect("a valid scenario");
        scenario
    }

    /// Options for the whole quick tier against `binary`: one scenario under both of the tier's
    /// profiles, with nothing named, nothing chosen, and no repetition.
    #[cfg(unix)]
    fn the_whole_quick_tier(binary: PathBuf, work: &Path) -> E2eOptions {
        E2eOptions {
            scenarios: vec![waits_for_the_output()],
            profiles: Vec::new(),
            ..lifecycle_options(binary, work)
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_quick_tier_over_its_budget_fails_the_reference_machine_and_only_warns_elsewhere() {
        let work = tempfile::tempdir().expect("a temporary directory");
        let program = three_hundred_bytes(work.path());
        let matrix = |limit: Duration, enforced: bool, out: &str| -> E2eReport {
            let mut options = the_whole_quick_tier(program.clone(), work.path());
            options.out_root = work.path().join(out);
            run_matrix(&options, QuickBudget { limit, enforced }, |_| {}).expect("the matrix runs")
        };
        let passing = |report: &E2eReport| {
            report
                .records
                .iter()
                .all(|record| record.report.verdict == Verdict::Pass)
        };

        // A budget of no time is always over: every run passed, and the time is all that is wrong.
        let reference = matrix(Duration::ZERO, true, "reference");
        assert!(passing(&reference), "{}", reference.table());
        assert!(!reference.is_success());
        let reason = reference
            .failure()
            .expect("a tier over its budget fails the reference machine");
        assert!(
            reason.starts_with("the quick tier took ")
                && reason.contains(" s, over its 0 s budget; slowest runs: ")
                && reason.contains("waits-for-the-output [default] (")
                && reason.contains("waits-for-the-output [deterministic] ("),
            "the failure names the budget and the runs: {reason}"
        );
        let table = reference.table();
        assert!(table.contains(&format!("\nFAIL {reason}\n")), "{table}");
        assert!(
            table.contains("e2e FAILED: 2 run(s), 0 blocking; quick tier: ")
                && table.contains(" s, over its 0 s budget; summary: "),
            "{table}"
        );

        // The same run elsewhere passes, and the table warns.
        let elsewhere = matrix(Duration::ZERO, false, "elsewhere");
        assert!(
            passing(&elsewhere) && elsewhere.is_success(),
            "{}",
            elsewhere.table()
        );
        assert_eq!(elsewhere.failure(), None);
        let table = elsewhere.table();
        assert!(table.contains("\nWARN the quick tier took "), "{table}");
        assert!(
            table.contains("e2e ok: 2 run(s), 0 blocking; quick tier: ") && !table.contains("FAIL"),
            "{table}"
        );

        // Within its budget a tier says nothing but its time, on either machine.
        for (enforced, out) in [(true, "within-reference"), (false, "within-elsewhere")] {
            let within = matrix(Duration::from_secs(3600), enforced, out);
            assert!(
                passing(&within) && within.is_success(),
                "{}",
                within.table()
            );
            assert_eq!(within.failure(), None);
            let table = within.table();
            assert!(
                !table.contains("WARN") && !table.contains("FAIL"),
                "{table}"
            );
            assert!(
                table.contains("e2e ok: 2 run(s), 0 blocking; quick tier: ")
                    && table.contains(" s of 3600 s; summary: "),
                "{table}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_run_that_already_failed_keeps_its_failure_and_still_reports_its_time() {
        let work = tempfile::tempdir().expect("a temporary directory");
        let options = the_whole_quick_tier(failing_stub(work.path()), work.path());

        for (limit, over, out) in [
            (Duration::from_secs(3600), false, "within"),
            (Duration::ZERO, true, "over"),
        ] {
            let mut options = options.clone();
            options.out_root = work.path().join(out);
            let report = run_matrix(
                &options,
                QuickBudget {
                    limit,
                    enforced: true,
                },
                |_| {},
            )
            .expect("the matrix runs");

            assert_eq!(
                verdicts_of(&report),
                [
                    ("waits-for-the-output".to_owned(), Verdict::Fail),
                    ("waits-for-the-output".to_owned(), Verdict::Fail)
                ]
            );
            assert_eq!(
                report.failure().as_deref(),
                Some("the e2e run has blocking verdicts"),
                "the failed runs are the failure, over the budget or not"
            );
            let time = report.quick_tier.expect("a failed run is still timed");
            assert_eq!(report.summary.quick_tier_ms, Some(time.millis()));
            validate_against_schema(HarnessSummary::SCHEMA_JSON, &report.summary_path);
            let table = report.table();
            assert!(
                table.contains("e2e FAILED: 2 run(s), 2 blocking; quick tier: "),
                "{table}"
            );
            assert_eq!(
                table.contains(" s, over its 0 s budget; summary: "),
                over,
                "{table}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_summary_records_the_time_of_the_whole_quick_tier_and_of_no_other_run() {
        let work = tempfile::tempdir().expect("a temporary directory");
        let whole = the_whole_quick_tier(three_hundred_bytes(work.path()), work.path());
        let run = |out: &str, change: &dyn Fn(&mut E2eOptions)| -> E2eReport {
            let mut options = whole.clone();
            options.out_root = work.path().join(out);
            change(&mut options);
            run_e2e(&options, |_| {}).expect("the matrix runs")
        };
        let text =
            |report: &E2eReport| fs::read_to_string(&report.summary_path).expect("a summary");

        let tier = run("tier", &|_| {});
        let time = tier.quick_tier.expect("the whole quick tier is timed");
        let runs: Duration = tier
            .records
            .iter()
            .map(|record| record.report.duration)
            .sum();
        assert_eq!(
            time.budget.limit,
            Duration::from_secs(120),
            "the quick tier is held to two minutes"
        );
        assert!(
            time.elapsed >= runs,
            "the time covers every run: {time:?} against {runs:?}"
        );
        assert_eq!(tier.summary.quick_tier_ms, Some(time.millis()));
        assert!(
            text(&tier).contains(&format!("\"quick_tier_ms\": {}", time.millis())),
            "{}",
            text(&tier)
        );
        assert!(
            tier.table().contains(" s of 120 s; summary: "),
            "{}",
            tier.table()
        );

        // A run that is not the tier has no time to hold to its budget: a scenario named with
        // `--scenario`, a profile chosen, a repetition, or another tier.
        let others = [
            ("named", run("named", &|options| options.named = true)),
            (
                "narrowed",
                run("narrowed", &|options| {
                    options.profiles = vec![Profile::Default];
                }),
            ),
            ("repeated", run("repeated", &|options| options.repeat = 2)),
            ("full", run("full", &|options| options.tier = Tier::Full)),
        ];
        for (what, report) in &others {
            assert_eq!(report.quick_tier, None, "{what}");
            assert_eq!(report.summary.quick_tier_ms, None, "{what}");
            assert!(
                !text(report).contains("quick_tier_ms"),
                "{what}: {}",
                text(report)
            );
            assert!(
                !report.table().contains("quick tier"),
                "{what}: {}",
                report.table()
            );
        }

        // The real writer's documents validate, and read back as the report says.
        for report in std::iter::once(&tier).chain(others.iter().map(|(_, report)| report)) {
            validate_against_schema(HarnessSummary::SCHEMA_JSON, &report.summary_path);
            assert_eq!(
                HarnessSummary::from_json_str(&text(report)).expect("the summary reads back"),
                report.summary
            );
        }
    }

    /// A scenario whose one step waits for text `excise` never prints, bounded by a short
    /// timeout: the shape needed to force a step to time out with the child still running,
    /// rather than exiting first.
    #[cfg(unix)]
    fn timeout_probe_scenario() -> Scenario {
        Scenario::from_toml_str(
            "schema_version = 1\n\
             name = \"timeout-probe\"\n\
             description = \"A step that can never be satisfied, so it always times out.\"\n\
             fixture = \"wide-1k\"\n\
             profiles = [\"default\"]\n\
             [[steps]]\n\
             step = \"wait_text\"\n\
             text = \"text that excise never prints\"\n\
             timeout_ms = 300\n",
        )
        .expect("a valid scenario")
    }

    /// A stand-in for `excise` that writes a little output and then hangs well past the
    /// scenario's 300 ms step timeout, so the step times out with the child still alive instead
    /// of exiting first: the shape `PtySession::diagnostics` is captured for.
    #[cfg(unix)]
    #[test]
    fn a_timed_out_step_leaves_a_bundle_whose_session_diagnostics_match_the_schema() {
        use crate::report::HarnessFailure;

        let work = tempfile::tempdir().expect("a temporary directory");
        let program = stub(
            work.path(),
            "excise",
            "#!/bin/sh\n[ \"$1\" = \"--version\" ] && exit 0\nprintf 'booting up\\n'\nsleep 5\nexit 0\n",
        );
        let options = E2eOptions {
            binary: program,
            tier: Tier::Quick,
            scenarios: vec![timeout_probe_scenario()],
            named: false,
            profiles: vec![Profile::Default],
            repeat: 1,
            keep_fixture: false,
            out_root: work.path().join("out"),
            work_dir: Some(work.path().to_path_buf()),
            git_sha: "0".repeat(40),
            latency_scale: LatencyScale::STRICT,
            timing_informational: false,
        };

        let report = run_e2e(&options, |_| {}).expect("the matrix runs");

        assert!(!report.is_success());
        let record = &report.records[0];
        assert_eq!(
            record.report.verdict,
            Verdict::Fail,
            "{:?}",
            record.report.error
        );
        let failure = record.report.failure.as_ref().expect("a failed step");
        assert_eq!(
            failure.cause,
            crate::runner::FailureCause::Timeout,
            "{failure}"
        );
        let diagnostics = failure
            .session_diagnostics
            .as_ref()
            .expect("a timed-out step records session diagnostics");
        assert!(diagnostics.output_bytes > 0, "{diagnostics:?}");
        assert!(diagnostics.first_byte_after.is_some(), "{diagnostics:?}");
        assert!(
            diagnostics.exit.is_none(),
            "the child was still running when the step timed out"
        );

        let bundle = record
            .report
            .bundle
            .as_ref()
            .expect("a failed run leaves a bundle");
        let failure_path = bundle.join("failure.json");
        validate_against_schema(HarnessFailure::SCHEMA_JSON, &failure_path);

        let text = fs::read_to_string(&failure_path).expect("a document");
        let document = HarnessFailure::from_json_str(&text).expect("a harness-failure document");
        let recorded = document
            .session_diagnostics
            .expect("the document records session diagnostics");
        assert!(recorded.output_bytes > 0, "{recorded:?}");
        assert!(recorded.first_byte_after_ms.is_some(), "{recorded:?}");
        assert!(recorded.child_running, "{recorded:?}");
        assert!(recorded.head.contains("booting up"), "{recorded:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_binary_that_cannot_warm_up_stops_the_matrix_before_any_run() {
        let work = tempfile::tempdir().expect("a temporary directory");
        // Only the hang needs a short bound, to fire the timeout. A stub that must finish gets the
        // real one: on a loaded machine, starting `/bin/sh` alone can take longer than 300 ms.
        let quick = Duration::from_millis(300);

        // A healthy binary is launched once and leaves nothing behind.
        let healthy = stub(work.path(), "healthy", "#!/bin/sh\nexit 0\n");
        warm_up(&healthy, work.path(), WARM_UP_TIMEOUT).expect("a healthy warm-up");
        let entries: Vec<_> = fs::read_dir(work.path())
            .expect("the work directory")
            .filter_map(Result::ok)
            .collect();
        assert_eq!(
            entries.len(),
            1,
            "the warm-up left its scratch area: {entries:?}"
        );

        // An unsuccessful exit, a hang, and a missing binary are errors, never skipped.
        let failing = stub(work.path(), "failing", "#!/bin/sh\nexit 3\n");
        let hanging = stub(work.path(), "hanging", "#!/bin/sh\nexec sleep 30\n");
        for (binary, timeout, reason) in [
            (&failing, WARM_UP_TIMEOUT, "exit status: 3"),
            (&hanging, quick, "did not end within 300 ms"),
        ] {
            let error = warm_up(binary, work.path(), timeout).expect_err("a failed warm-up");
            assert!(matches!(error, E2eError::WarmUp { .. }), "{error}");
            assert!(error.to_string().contains(reason), "{error}");
        }
        let missing = work.path().join("absent");
        assert!(matches!(
            warm_up(&missing, work.path(), WARM_UP_TIMEOUT),
            Err(E2eError::WarmUp { .. })
        ));

        // The matrix does not start, and writes nothing.
        let options = lifecycle_options(failing, work.path());
        let error = run_e2e(&options, |_| {}).expect_err("no matrix without a warm-up");
        assert!(matches!(error, E2eError::WarmUp { .. }), "{error}");
        assert!(
            !options.out_root.exists(),
            "a failed warm-up wrote a run directory"
        );
    }
}
