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
//! Scenarios carry no tier of their own yet, so the tier limits the *profiles*: the quick tier runs
//! the two profiles that every lifecycle scenario must pass under (`default` and `deterministic`),
//! and the full tier runs every profile a scenario declares. A scenario is selected for a profile
//! only if it declares that profile.

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
    fixture::Fixtures,
    report::{
        BinaryIdentity, Document, HarnessSummary, ScenarioResult, SchemaVersion, SummaryKind, Tier,
        Verdict,
    },
    run_support::{
        compact_utc, format_ms, host_name, median, point_latest_at, render_rows, rfc3339,
        sha256_file, worst,
    },
    safety::{Scratch, isolated_env},
    scenario::{LoadError, Profile, Scenario, ValidationErrors},
};

use super::{
    run::{RunReport, RunRequest, resolve_binary, run_scenario},
    work::work_base,
};

/// How long the warm-up launch may take. A cold first launch was measured at under half a second.
const WARM_UP_TIMEOUT: Duration = Duration::from_secs(10);

/// The profiles of the quick tier.
const QUICK_PROFILES: [Profile; 2] = [Profile::Default, Profile::Deterministic];

/// What to run and where to put the results.
#[derive(Debug, Clone)]
pub struct E2eOptions {
    /// The `excise` binary under test.
    pub binary: PathBuf,
    /// The tier, which limits the profiles that run.
    pub tier: Tier,
    /// The scenarios to consider, already loaded.
    pub scenarios: Vec<Scenario>,
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
    /// Nothing matched the scenario and profile selection.
    #[error("no scenario runs under the selected profiles ({0})")]
    NothingToRun(String),
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
        .envs(isolated_env(&scratch, Profile::Default, false))
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
    let mut plan: Vec<(&Scenario, Profile)> = Vec::new();
    for scenario in &options.scenarios {
        for profile in selected_profiles(options, scenario) {
            plan.push((scenario, profile));
        }
    }
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
        return Err(E2eError::NothingToRun(selected));
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
    let fixture = match Fixtures::bundled().run_copy(&scenario.fixture, workspace.path()) {
        Ok(fixture) => fixture,
        Err(error) => {
            return failed_to_start(scenario, profile, repetition, error.to_string());
        }
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
        let quick = Duration::from_millis(300);

        // A healthy binary is launched once and leaves nothing behind.
        let healthy = stub(work.path(), "healthy", "#!/bin/sh\nexit 0\n");
        warm_up(&healthy, work.path(), quick).expect("a healthy warm-up");
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
        for (binary, reason) in [
            (&failing, "exit status: 3"),
            (&hanging, "did not end within 300 ms"),
        ] {
            let error = warm_up(binary, work.path(), quick).expect_err("a failed warm-up");
            assert!(matches!(error, E2eError::WarmUp { .. }), "{error}");
            assert!(error.to_string().contains(reason), "{error}");
        }
        let missing = work.path().join("absent");
        assert!(matches!(
            warm_up(&missing, work.path(), quick),
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
