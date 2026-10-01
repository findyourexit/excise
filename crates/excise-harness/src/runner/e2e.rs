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
        Verdict,
    },
    run_support::{
        compact_utc, format_ms, host_name, median, point_latest_at, render_rows, rfc3339,
        sha256_file, worst,
    },
    safety::{Scratch, cgroup, isolated_env},
    scenario::{LoadError, Profile, Scenario, Tier as ScenarioTier, ValidationErrors},
};

use super::{
    run::{RunReport, RunRequest, resolve_binary, run_scenario},
    work::work_base,
};

/// How long the warm-up launch may take. A cold first launch was measured at under half a second.
const WARM_UP_TIMEOUT: Duration = Duration::from_secs(10);

/// The profiles of the quick tier.
const QUICK_PROFILES: [Profile; 2] = [Profile::Default, Profile::Deterministic];

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
}

impl E2eReport {
    /// Whether no run has a blocking verdict: a `fail`, `xpass`, or `error` fails the run.
    #[must_use]
    pub fn is_success(&self) -> bool {
        !self
            .records
            .iter()
            .any(|record| record.report.verdict.blocks_run())
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
/// # Errors
///
/// Returns an error if nothing was selected, if the warm-up launch fails, or if the output
/// directory or the summary cannot be written. A scenario that fails or cannot run is not an error of the matrix: it is a run with a
/// blocking verdict in the report.
pub fn run_e2e(
    options: &E2eOptions,
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
    }
}

// ---------------------------------------------------------------------------------------------
// The verdict table.

impl E2eReport {
    /// The verdict table: one line per scenario and profile, then one block per blocking run, then
    /// the overall verdict.
    #[must_use]
    pub fn table(&self) -> String {
        let mut table = render_rows(&self.summary_rows());
        self.write_skipped(&mut table);
        self.write_blocking_runs(&mut table);
        let blocking = self
            .records
            .iter()
            .filter(|record| record.report.verdict.blocks_run())
            .count();
        let _ = writeln!(
            table,
            "\ne2e {}: {} run(s), {blocking} blocking; summary: {}",
            if blocking == 0 { "ok" } else { "FAILED" },
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
        }
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
        let validate = |schema: &str, path: &Path| {
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
        };
        validate(HarnessSummary::SCHEMA_JSON, &report.summary_path);
        let bundle = record
            .report
            .bundle
            .as_ref()
            .expect("a failed run leaves a bundle");
        validate(HarnessFailure::SCHEMA_JSON, &bundle.join("failure.json"));
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
