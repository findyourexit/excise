//! Running the sweep: the phases and the documents. What a sweep leaves behind is described in the
//! module documentation of [`sweep`](super).

use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use thiserror::Error;

use crate::{
    bench::{
        cases::{CaseError, SharedFixture},
        context::{Snapshot, load_average_one_minute, os_description, power_state},
    },
    fixture::{Fixtures, Oracle, OracleError},
    headless::du::Du,
    report::{
        AbFixture, BuildStatus, CheckStatus, Document, HarnessSweep, SchemaVersion, SweepBuild,
        SweepContext, SweepInvalid, SweepKind, SweepMeasurement, SweepTier, SweepToolchain,
        SweepVersion,
    },
    run_support::{compact_utc, host_name, point_latest_at, rfc3339, sha256_file},
    safety::{FixtureRoot, FixtureSnapshot, Scratch},
    scenario::Profile,
};

use super::{
    checks::{self, Checked, Context, Fatal, IdleWindow, Recorded},
    classify::Classifier,
    headless::{
        HEADLESS_SCAN, HEADLESS_WALL, REPORT_WRITE, ReportRead, oracle_check_name, oracle_run,
        timed_du, timed_scan,
    },
    intact,
    model::{MeasuredSet, Placed, Plan, VersionFacts, roles},
    output::Output,
    probe::{Probe, ProbeSpec, Wait},
    signals::{limit_core_dumps, signals_for, skipped_note},
    table,
    timing::{Against, Paired, Reading, RunValue, paired_rounds, series_of},
    traits::{self, Traits},
};

/// The fixtures a full sweep holds every version's report to the oracle on besides the quick
/// tier's: the identity class, every class in one tree, and a flat tree.
const FULL_EXTRA_ORACLE: [&str; 3] = ["identity-small", "all-classes-small", "wide-1k"];
/// How many measured runs in a row a leg of a version may run out of time in before its remaining
/// rounds are skipped (recorded as skipped, and never as samples): a version that cannot finish
/// twice in a row will not finish the third time, and a minute spent waiting for it is a minute not
/// spent on the others. The warm-up round does not count, so that every version that is given up on
/// has this many runs that ran out of time in the table.
const GIVE_UP_AFTER: usize = 2;
/// How many attempts the selection-drift reproduction makes on each version.
const DRIFT_ATTEMPTS: u32 = 2;
/// The least a scan of a large fixture may take before a check gives up on it, whatever the
/// command line allows a scan of a small one: the 49,050-entry tree takes minutes to scan in a
/// build that syncs to disk for every batch.
const LARGE_FIXTURE_BOUND: Duration = Duration::from_secs(600);

/// A sweep's directory below the output root, made before the builds so that their logs can live
/// in it.
#[derive(Debug, Clone)]
pub struct RunDir {
    id: String,
    path: PathBuf,
    out_root: PathBuf,
    started: SystemTime,
}

impl RunDir {
    /// Makes the directory of a run below `out_root`, named by the time it started and the process
    /// id.
    ///
    /// # Errors
    ///
    /// Returns [`SweepError::Io`] when the directory cannot be made.
    pub fn create(out_root: &Path) -> Result<Self, SweepError> {
        let started = SystemTime::now();
        let id = format!("{}-{}", compact_utc(started), std::process::id());
        let path = out_root.join(&id);
        fs::create_dir_all(&path)
            .map_err(io_error(format!("cannot create `{}`", path.display())))?;
        Ok(Self {
            id,
            path,
            out_root: out_root.to_path_buf(),
            started,
        })
    }

    /// The run's id, which names its directory.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The run's directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// The build of one version, as the command line made it.
#[derive(Debug, Clone)]
pub enum BuildInput {
    /// There is a binary.
    Built {
        /// Where it is.
        binary: PathBuf,
        /// Whether it came from the cache of an earlier build of the same commit.
        cached: bool,
        /// The build's output, as a path below the run's directory, when it was kept.
        log: Option<String>,
    },
    /// The build failed.
    Failed {
        /// Why, with the end of the build's output.
        reason: String,
        /// The build's output, as a path below the run's directory, when it was kept.
        log: Option<String>,
    },
}

/// One version to sweep: a git ref and its build.
#[derive(Debug, Clone)]
pub struct VersionInput {
    /// The ref as it was given. The files the sweep keeps for the version are named by the
    /// [`label`](fn@super::label) of it and of `sha`, never by the ref itself.
    pub reference: String,
    /// The commit it resolved to.
    pub sha: String,
    /// The toolchain the build used, when it got as far as one.
    pub toolchain: Option<SweepToolchain>,
    /// How the build went.
    pub build: BuildInput,
}

/// What to sweep and how.
#[derive(Debug, Clone)]
pub struct SweepOptions {
    /// The versions, oldest first. The last is the candidate: every ratio is taken to it.
    pub versions: Vec<VersionInput>,
    /// How much of the sweep runs.
    pub tier: SweepTier,
    /// The fixtures the headless checks run on, when the command line named them; the tier's own
    /// otherwise. The interface checks keep the fixtures they play their roles on.
    pub fixture_ids: Vec<String>,
    /// How many measured rounds each paired timing runs.
    pub rounds: u32,
    /// The seed of the bootstrap.
    pub seed: u64,
    /// How long one scan, or one interface run to COMPLETE, may take.
    pub timeout: Duration,
    /// The cap on how fast the slow terminal's output is read, in bytes per second.
    pub drain_bytes_per_sec: u64,
    /// How long the idle check waits and looks. The default is the validation program's own.
    pub idle: IdleWindow,
    /// Where fixtures come from.
    pub fixtures: Fixtures,
    /// The run's directory, made before the builds.
    pub run: RunDir,
    /// The directory scratch areas and fixture copies are made in.
    pub work_dir: PathBuf,
    /// The commit of the checkout that runs the sweep.
    pub checkout_sha: String,
}

/// The sweep could not be run.
#[derive(Debug, Error)]
pub enum SweepError {
    /// There is nothing to sweep, or the options contradict themselves.
    #[error("{0}")]
    Options(String),
    /// A fixture was named that does not exist.
    #[error("unknown fixture `{id}`; the fixtures are {}", known.join(", "))]
    UnknownFixture {
        /// The id that was asked for.
        id: String,
        /// The ids there are.
        known: Vec<String>,
    },
    /// A fixture could not be made or walked.
    #[error("fixture `{id}`: {reason}")]
    Fixture {
        /// The fixture.
        id: String,
        /// Why.
        reason: String,
    },
    /// A fixture is not what it should be: a build that ran on it deleted or rewrote what it was
    /// only to read, or it was not what was generated before the first build ran.
    #[error(transparent)]
    Fatal(#[from] Fatal),
    /// A file or directory could not be read or written.
    #[error("{context}: {source}")]
    Io {
        /// What was being done.
        context: String,
        /// The underlying error.
        source: io::Error,
    },
    /// The document could not be rendered.
    #[error("cannot render the sweep: {0}")]
    Json(#[from] serde_json::Error),
    /// The document breaks a rule of its own: a defect in the sweep.
    #[error("the sweep built a document that breaks its own rules: {0}")]
    Invalid(#[from] SweepInvalid),
}

fn io_error(context: impl Into<String>) -> impl FnOnce(io::Error) -> SweepError {
    let context = context.into();
    move |source| SweepError::Io { context, source }
}

/// What a sweep produced.
#[derive(Debug)]
pub struct SweepReport {
    /// The `harness-sweep` document that was written.
    pub document: HarnessSweep,
    /// Where it was written.
    pub document_path: PathBuf,
    /// Where the table's text, with its detail, was written.
    pub table_path: PathBuf,
    /// The run's directory.
    pub run_dir: PathBuf,
}

impl SweepReport {
    /// What went wrong that the table does not show: builds that failed and checks that the harness
    /// could not carry out. A sweep with a problem still has its table, and exits non-zero.
    #[must_use]
    pub fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        for version in &self.document.versions {
            if let Some(reason) = &version.build.reason {
                problems.push(format!("{}: the build failed: {reason}", version.reference));
            }
        }
        for check in &self.document.checks {
            if check.status == CheckStatus::Errored {
                problems.push(format!(
                    "{}: the `{}` check could not be carried out: {}",
                    check.reference,
                    check.check,
                    check.reason.as_deref().unwrap_or("no reason given")
                ));
            }
        }
        problems
    }

    /// The grid of the table, for the terminal.
    #[must_use]
    pub fn grid(&self) -> String {
        table::grid(&self.document)
    }
}

/// The plan of a tier, or of the fixtures the command line named.
fn plan_of(options: &SweepOptions) -> Plan {
    let (oracle, timing) = if options.fixture_ids.is_empty() {
        let mut oracle: Vec<String> = [roles::TIMING, roles::HOSTILE, roles::DEEP]
            .iter()
            .map(|id| (*id).to_owned())
            .collect();
        if options.tier == SweepTier::Full {
            oracle.extend(FULL_EXTRA_ORACLE.iter().map(|id| (*id).to_owned()));
        }
        (oracle, vec![roles::TIMING.to_owned()])
    } else {
        (options.fixture_ids.clone(), options.fixture_ids.clone())
    };
    Plan {
        tier: options.tier,
        oracle_fixtures: oracle,
        timing_fixtures: timing,
    }
}

/// The ids of every fixture the sweep touches.
fn fixture_ids_of(plan: &Plan) -> Vec<String> {
    let mut ids: Vec<String> = plan
        .oracle_fixtures
        .iter()
        .chain(&plan.timing_fixtures)
        .cloned()
        .collect();
    ids.extend([roles::QUICK.to_owned(), roles::FILTER.to_owned()]);
    if plan.is_full() {
        ids.extend([
            roles::DRIFT.to_owned(),
            roles::DESCRIPTORS_SMALL.to_owned(),
            roles::DESCRIPTORS_LARGE.to_owned(),
        ]);
    }
    ids.sort();
    ids.dedup();
    ids
}

/// A fixture the sweep scans or times: its root, kept for the whole sweep, and the shared fixture
/// behind it, which is held to its plan before the first build and after the scans of each version
/// (see [`verify_oracle_fixtures`]).
struct Prepared {
    shared: SharedFixture,
    root: FixtureRoot,
    oracle: Option<Oracle>,
    identity: AbFixture,
}

/// Holds every fixture the headless scans run on to the plan it was generated from, and stops the
/// sweep when one differs. `context` says when the difference was found.
fn verify_oracle_fixtures(
    plan: &Plan,
    prepared: &BTreeMap<String, Prepared>,
    context: &str,
) -> Result<(), SweepError> {
    for id in &plan.oracle_fixtures {
        if let Some(fixture) = prepared.get(id) {
            intact::verify(&fixture.shared, id, context)?;
        }
    }
    Ok(())
}

fn validate(options: &SweepOptions) -> Result<(), SweepError> {
    if options.versions.is_empty() {
        return Err(SweepError::Options(
            "nothing to sweep: no version was given".to_owned(),
        ));
    }
    for (index, version) in options.versions.iter().enumerate() {
        if options.versions[..index]
            .iter()
            .any(|earlier| earlier.reference == version.reference)
        {
            return Err(SweepError::Options(format!(
                "the version `{}` is given twice",
                version.reference
            )));
        }
    }
    if options.rounds == 0 {
        return Err(SweepError::Options(
            "a paired timing needs at least one round".to_owned(),
        ));
    }
    if options.drain_bytes_per_sec == 0 {
        return Err(SweepError::Options(
            "the slow terminal must read at least one byte a second".to_owned(),
        ));
    }
    let known = options
        .fixtures
        .ids()
        .map_err(io_error("cannot read the fixture specifications"))?;
    let plan = plan_of(options);
    for id in fixture_ids_of(&plan) {
        if !known.contains(&id) {
            return Err(SweepError::UnknownFixture { id, known });
        }
    }
    Ok(())
}

fn prepare(options: &SweepOptions, plan: &Plan) -> Result<BTreeMap<String, Prepared>, SweepError> {
    let mut prepared = BTreeMap::new();
    for id in fixture_ids_of(plan) {
        let fixture_error = |reason: String| SweepError::Fixture {
            id: id.clone(),
            reason,
        };
        let shared = SharedFixture::acquire(&options.fixtures, &id, &options.work_dir)
            .map_err(|error: CaseError| fixture_error(error.to_string()))?;
        let root =
            FixtureRoot::open(shared.root()).map_err(|error| fixture_error(error.to_string()))?;
        let wants_oracle = plan.oracle_fixtures.contains(&id) || id == roles::FILTER;
        let oracle = if wants_oracle {
            Some(
                Oracle::collect(root.path())
                    .map_err(|error: OracleError| fixture_error(error.to_string()))?,
            )
        } else {
            None
        };
        let identity = shared.identity(&id);
        prepared.insert(
            id,
            Prepared {
                shared,
                root,
                oracle,
                identity,
            },
        );
    }
    Ok(prepared)
}

/// The largest direct child of the root of `oracle`, by the bytes its subtree allocates: what an
/// untouched cursor must land on at COMPLETE.
fn largest_child(oracle: &Oracle) -> Option<String> {
    oracle
        .entries
        .iter()
        .filter(|entry| entry.path.depth() == 1)
        .max_by_key(|entry| {
            let own = entry.allocated.unwrap_or(0);
            let below = entry.subtree.map_or(0, |subtree| {
                subtree.allocated_bytes.unwrap_or(0)
                    + subtree.directory_allocated_bytes.unwrap_or(0)
            });
            own + below
        })
        .and_then(|entry| entry.path.file_name())
        .map(|name| String::from_utf8_lossy(name).into_owned())
}

/// Runs the sweep and writes the document and the table.
///
/// A version whose build failed is a column of `not-measurable` cells that say so, and the sweep
/// goes on to the others; a check that cannot run says why and is not a finding.
///
/// # Errors
///
/// Returns [`SweepError`] when the options are wrong, a fixture cannot be made, a check changed a
/// fixture (the sweep stops at once), or the output cannot be written.
#[allow(clippy::too_many_lines)]
pub fn run_sweep(
    options: &SweepOptions,
    mut progress: impl FnMut(&str),
) -> Result<SweepReport, SweepError> {
    validate(options)?;
    limit_core_dumps();
    fs::create_dir_all(&options.work_dir).map_err(io_error(format!(
        "cannot create `{}`",
        options.work_dir.display()
    )))?;
    // The harness opens a fixture's root without following links, so the directory fixtures are
    // made in must have none in its path, and `/tmp` is one on macOS.
    let options = &SweepOptions {
        work_dir: fs::canonicalize(&options.work_dir).map_err(io_error(format!(
            "cannot resolve `{}`",
            options.work_dir.display()
        )))?,
        ..options.clone()
    };
    let started = options.run.started;
    let run_id = options.run.id.clone();
    let run_dir = options.run.path.clone();
    let mut output = Output::new(&run_dir, &options.versions)?;
    let snapshot = Snapshot::take();
    let concurrent = snapshot.concurrent_excise_processes();
    if concurrent > 0 {
        progress(&format!(
            "warning: {concurrent} other `excise` process(es) are running; timings may be noisy"
        ));
    }
    let load_start = load_average_one_minute();
    let du = Du::find();
    let plan = plan_of(options);
    let work_dir = options.work_dir.as_path();

    progress(&format!(
        "preparing {} fixtures",
        fixture_ids_of(&plan).len()
    ));
    let prepared = prepare(options, &plan)?;
    let filter_largest = prepared
        .get(roles::FILTER)
        .and_then(|fixture| fixture.oracle.as_ref())
        .and_then(largest_child);

    let mut facts: Vec<VersionFacts> = options
        .versions
        .iter()
        .map(|version| VersionFacts {
            reference: version.reference.clone(),
            unbuilt: match &version.build {
                BuildInput::Built { .. } => None,
                BuildInput::Failed { reason, .. } => Some(format!("the build failed: {reason}")),
            },
            ..VersionFacts::default()
        })
        .collect();
    let mut binaries: Vec<Option<PathBuf>> = options
        .versions
        .iter()
        .map(|version| match &version.build {
            BuildInput::Built { binary, .. } => Some(binary.clone()),
            BuildInput::Failed { .. } => None,
        })
        .collect();

    // What each build takes.
    progress("asking each build what it takes");
    let mut report_versions: Vec<Option<u32>> = vec![None; options.versions.len()];
    for (index, version) in options.versions.iter().enumerate() {
        let Some(binary) = binaries[index].clone() else {
            continue;
        };
        match traits::probe(&binary, work_dir) {
            Ok(found) => facts[index].traits = Some(found),
            Err(why) => {
                facts[index].unbuilt = Some(format!("the binary is unusable: {why}"));
                binaries[index] = None;
                progress(&format!("  {}: unusable: {why}", version.reference));
            }
        }
    }

    // The headless scans, held to the oracle.
    progress("scanning the fixtures and holding the reports to the oracle");
    // The scans only read, so every fixture must be what was generated before the first one runs:
    // a difference found afterwards is then the doing of the version that ran in between.
    verify_oracle_fixtures(&plan, &prepared, intact::BEFORE_ANY_BUILD)?;
    for (index, version) in options.versions.iter().enumerate() {
        let Some(binary) = binaries[index].clone() else {
            continue;
        };
        for id in &plan.oracle_fixtures {
            let Some(fixture) = prepared.get(id) else {
                continue;
            };
            let checked = if facts[index].traits.is_some_and(Traits::headless) {
                match &fixture.oracle {
                    Some(oracle) => oracle_run(
                        &binary,
                        id,
                        &fixture.root,
                        oracle,
                        work_dir,
                        options.timeout,
                    ),
                    None => Checked {
                        observation: None,
                        record: Recorded::errored(
                            oracle_check_name(),
                            Some(id),
                            Some(Profile::Default),
                            "the harness has no oracle for the fixture",
                        ),
                    },
                }
            } else {
                Checked {
                    observation: None,
                    record: Recorded::not_run(
                        oracle_check_name(),
                        Some(id),
                        Some(Profile::Default),
                        "the build's `--help` lists no `--format` and `--output`, so it has no headless scan",
                    ),
                }
            };
            let placed = output.place(&version.reference, checked)?;
            progress(&format!(
                "  {} {id}: {}",
                version.reference,
                summarize(&placed)
            ));
            facts[index].oracle.insert(id.clone(), placed);
        }
        // A scan only reads. A build that changed a fixture stops the sweep here, before another
        // build is run on it.
        if facts[index].traits.is_some_and(Traits::headless) {
            let context = intact::after_scan_by(&version.reference);
            verify_oracle_fixtures(&plan, &prepared, &context)?;
        }
        report_versions[index] = facts[index].oracle.values().find_map(|placed| {
            match placed.checked.observation.as_ref()?.report {
                ReportRead::Read { version, .. } => Some(version),
                ReportRead::Missing(_) | ReportRead::Invalid(_) => None,
            }
        });
    }

    // The interface checks.
    progress("driving each build in a terminal");
    let signals = signals_for(std::env::consts::OS);
    if let Some(note) = skipped_note(std::env::consts::OS) {
        progress(&format!("  {note}"));
    }
    for (index, version) in options.versions.iter().enumerate() {
        let Some(binary) = binaries[index].clone() else {
            continue;
        };
        let context = Context {
            binary: &binary,
            fixtures: &options.fixtures,
            work_dir,
            bound: options.timeout,
        };
        let name = version.reference.as_str();
        for signal in signals {
            let checked = checks::signal(&context, roles::QUICK, name, *signal)?;
            let placed = output.place(name, checked)?;
            progress(&format!("  {name} {signal}: {}", summarize(&placed)));
            facts[index]
                .signals
                .insert(signal.as_str().to_owned(), placed);
        }
        let checked = checks::kill_restart(&context, roles::QUICK, name)?;
        let placed = output.place(name, checked)?;
        progress(&format!("  {name} kill-restart: {}", summarize(&placed)));
        facts[index].kill_restart = Some(placed);

        let checked = checks::idle(&context, roles::QUICK, name, options.idle)?;
        let placed = output.place(name, checked)?;
        progress(&format!("  {name} idle: {}", summarize(&placed)));
        facts[index].idle = Some(placed);

        let checked = checks::filter(&context, roles::FILTER, filter_largest.clone(), name)?;
        let placed = output.place(name, checked)?;
        progress(&format!("  {name} filter: {}", summarize(&placed)));
        facts[index].filter = Some(placed);

        if plan.is_full() {
            let large = Context {
                bound: options.timeout.max(LARGE_FIXTURE_BOUND),
                ..context
            };
            let checked = checks::drift(&large, roles::DRIFT, DRIFT_ATTEMPTS, name)?;
            let placed = output.place(name, checked)?;
            progress(&format!("  {name} selection-drift: {}", summarize(&placed)));
            facts[index].drift = Some(placed);

            let checked = checks::descriptors(
                &large,
                roles::DESCRIPTORS_SMALL,
                roles::DESCRIPTORS_LARGE,
                name,
            )?;
            let placed = output.place(name, checked)?;
            progress(&format!("  {name} descriptors: {}", summarize(&placed)));
            facts[index].descriptors = Some(placed);
        }
    }

    // The paired timings.
    let timed: Vec<usize> = (0..options.versions.len())
        .filter(|index| {
            binaries[*index].is_some() && facts[*index].traits.is_some_and(Traits::headless)
        })
        .collect();
    let names: Vec<String> = timed
        .iter()
        .map(|index| options.versions[*index].reference.clone())
        .collect();
    let mut measurements: Vec<MeasuredSet> = Vec::new();
    if names.is_empty() {
        progress("no build can be timed");
    } else {
        for id in &plan.timing_fixtures {
            let Some(fixture) = prepared.get(id) else {
                continue;
            };
            let baseline = FixtureSnapshot::take(fixture.root.path()).map_err(|error| {
                SweepError::Fixture {
                    id: id.clone(),
                    reason: error.to_string(),
                }
            })?;
            progress(&format!(
                "timing `{id}`: {} versions, {} rounds",
                names.len(),
                options.rounds
            ));
            let on = TimedFixture {
                options,
                versions: &timed,
                names: &names,
                binaries: &binaries,
                fixture,
                id,
            };
            time_unthrottled(
                &on,
                du.as_ref(),
                &mut output,
                &mut measurements,
                &mut progress,
            )?;
            time_drained(&on, &mut output, &mut measurements, &mut progress)?;
            let after = FixtureSnapshot::take(fixture.root.path()).map_err(|error| {
                SweepError::Fixture {
                    id: id.clone(),
                    reason: error.to_string(),
                }
            })?;
            let changes = baseline.diff(&after).unexpected(&[], &[]);
            if !changes.is_empty() {
                return Err(Fatal(format!(
                    "fixture `{id}` changed while the versions were timed on it: {}; the sweep \
                     stops here",
                    changes.join("; ")
                ))
                .into());
            }
        }
    }

    // The table.
    let os = std::env::consts::OS;
    let classifier = Classifier {
        versions: &facts,
        measurements: &measurements,
        plan: &plan,
        os,
        drain: options.drain_bytes_per_sec,
    };
    let rows = classifier.rows();

    let versions: Vec<SweepVersion> = options
        .versions
        .iter()
        .enumerate()
        .map(|(index, version)| {
            sweep_version(
                version,
                binaries[index].as_deref(),
                facts[index].traits,
                report_versions[index],
            )
        })
        .collect::<Result<_, SweepError>>()?;
    let candidate = options
        .versions
        .last()
        .map(|version| version.reference.clone())
        .unwrap_or_default();
    let load_end = load_average_one_minute();
    let finished = SystemTime::now();
    let document = HarnessSweep {
        document_kind: SweepKind::default(),
        schema_version: SchemaVersion,
        run_id: run_id.clone(),
        tier: options.tier,
        started_at: rfc3339(started),
        finished_at: rfc3339(finished),
        candidate,
        context: SweepContext {
            host: host_name(),
            cpu: snapshot.cpu_model(),
            os: os_description(),
            arch: std::env::consts::ARCH.to_owned(),
            logical_cpus: snapshot.logical_cpus(),
            power: power_state(),
            load_average_start: load_start,
            load_average_end: load_end,
            concurrent_excise_processes: concurrent,
            checkout_sha: options.checkout_sha.clone(),
            rounds: options.rounds,
            seed: options.seed,
            drain_bytes_per_sec: options.drain_bytes_per_sec,
            timeout_ms: u64::try_from(options.timeout.as_millis()).unwrap_or(u64::MAX),
            du: du.as_ref().map(|du| du.flavor().as_str().to_owned()),
            fixtures: prepared
                .values()
                .map(|fixture| fixture.identity.clone())
                .collect(),
        },
        versions,
        measurements: measurements
            .iter()
            .map(|set| set.measurement.clone())
            .collect(),
        checks: output.checks,
        rows,
    };
    document.check()?;

    let document_path = run_dir.join("sweep.json");
    fs::write(&document_path, document.to_json_pretty()?).map_err(io_error(format!(
        "cannot write `{}`",
        document_path.display()
    )))?;
    let table_path = run_dir.join("table.txt");
    fs::write(
        &table_path,
        format!("{}\n{}", table::grid(&document), table::detail(&document)),
    )
    .map_err(io_error(format!("cannot write `{}`", table_path.display())))?;
    point_latest_at(&options.run.out_root, &run_id)
        .map_err(io_error("cannot update the `latest` pointer"))?;
    drop(prepared);
    Ok(SweepReport {
        document,
        document_path,
        table_path,
        run_dir,
    })
}

fn sweep_version(
    version: &VersionInput,
    binary: Option<&Path>,
    traits: Option<Traits>,
    report_version: Option<u32>,
) -> Result<SweepVersion, SweepError> {
    let (cached, log, failed) = match &version.build {
        BuildInput::Built { cached, log, .. } => (*cached, log.clone(), None),
        BuildInput::Failed { reason, log } => (false, log.clone(), Some(reason.clone())),
    };
    let binary_sha256 = match binary {
        Some(path) => {
            Some(sha256_file(path).map_err(io_error(format!("cannot hash `{}`", path.display())))?)
        }
        None => None,
    };
    // A binary that was found unusable is a failed build in the document.
    let build = match (&failed, binary) {
        (Some(reason), _) => SweepBuild {
            status: BuildStatus::Failed,
            cached: false,
            reason: Some(reason.clone()),
            log,
        },
        (None, None) => SweepBuild {
            status: BuildStatus::Failed,
            cached: false,
            reason: Some("the binary could not be used".to_owned()),
            log,
        },
        (None, Some(_)) => SweepBuild {
            status: BuildStatus::Built,
            cached,
            reason: None,
            log,
        },
    };
    Ok(SweepVersion {
        reference: version.reference.clone(),
        sha: version.sha.clone(),
        toolchain: version.toolchain.clone(),
        binary_sha256,
        build,
        traits: traits.map(|traits| traits.recorded(report_version)),
    })
}

/// One line about a check, for the progress.
fn summarize<T>(placed: &Placed<T>) -> String {
    let record = &placed.checked.record;
    match (&record.reason, record.notes.first()) {
        (Some(reason), _) => format!("{}: {reason}", record.status),
        (None, Some(note)) => note.clone(),
        (None, None) => record.status.to_string(),
    }
}

/// The series of the unthrottled phase: a headless scan's wall time, the time its report took to
/// write, and its time without the report (what [`TimedScan::readings`] makes of a scan), and the
/// time an interface run under the default profile takes to reach COMPLETE. A key names a series
/// inside one phase; the document names a measurement by its metric, profile, and drain.
const TUI_DEFAULT: &str = "tui_default";
/// The series of the phase against a slow terminal: an interface run under each of two profiles.
const TUI_DEFAULT_DRAINED: &str = "tui_default_drained";
const TUI_REDUCED_DRAINED: &str = "tui_reduced_drained";
/// What a version does in its turn of the unthrottled phase: a headless scan and then, back to
/// back, an interface run to COMPLETE. Both are measured in the same round, so that the interface
/// time of a version is held to the time of the same version's headless scan without the writing
/// of its report, in the same round.
const UNTHROTTLED_LEGS: [&[&str]; 2] = [
    &[HEADLESS_WALL, REPORT_WRITE, HEADLESS_SCAN],
    &[TUI_DEFAULT],
];
/// What a version does in its turn of the phase against a slow terminal: an interface run under
/// the default profile and then one under reduced motion, in the same round, so that default
/// motion is held to reduced motion of the same version in the same round.
const DRAINED_LEGS: [&[&str]; 2] = [&[TUI_DEFAULT_DRAINED], &[TUI_REDUCED_DRAINED]];

/// What the timing phases of one fixture need: the options, the versions that are timed, and the
/// fixture they are timed on.
struct TimedFixture<'a> {
    options: &'a SweepOptions,
    /// The position in `options.versions` of each version that is timed, in the order of `names`.
    versions: &'a [usize],
    /// The refs of the versions that are timed.
    names: &'a [String],
    /// The binary of every version, by its position in `options.versions`.
    binaries: &'a [Option<PathBuf>],
    /// The fixture they are timed on.
    fixture: &'a Prepared,
    /// Its id.
    id: &'a str,
}

impl TimedFixture<'_> {
    /// The binary of the version at `position` among those that are timed.
    fn binary(&self, position: usize) -> Result<&Path, String> {
        self.binaries[self.versions[position]]
            .as_deref()
            .ok_or_else(|| "there is no binary".to_owned())
    }

    /// One headless scan: its wall time, how long its report took to write, and its time without
    /// the report, which [`TimedScan::readings`] makes of the run. A scan that did not finish is
    /// flagged on each, and each is then the least it took.
    fn scan(&self, binary: &Path) -> Result<RunValue, String> {
        let options = self.options;
        let scan = timed_scan(
            binary,
            &self.fixture.root,
            options.work_dir.as_path(),
            options.timeout,
        )?;
        Ok(scan.readings())
    }

    /// One interface run to COMPLETE under `profile`, against a terminal that reads `drain` bytes a
    /// second when that is set, measured as the series `key`. A run that does not reach COMPLETE
    /// is flagged, and recorded at how long it ran: the bound, when it ran out of time.
    fn interface(
        &self,
        binary: &Path,
        profile: Profile,
        drain: Option<u64>,
        key: &'static str,
    ) -> Result<RunValue, String> {
        let options = self.options;
        let work_dir = options.work_dir.as_path();
        let scratch = Scratch::create(work_dir).map_err(|error| error.to_string())?;
        let started = std::time::Instant::now();
        let mut probe = Probe::start(&ProbeSpec {
            binary,
            fixture: &self.fixture.root,
            scratch: &scratch,
            profile,
            drain_bytes_per_sec: drain,
        })
        .map_err(|error| error.to_string())?;
        let waited = probe
            .wait_complete(options.timeout)
            .map_err(|error| error.to_string())?;
        let ran = started.elapsed();
        probe.kill();
        let reading = match waited {
            Wait::Ready(after) => Reading::new(after.as_secs_f64() * 1000.0, true),
            Wait::TimedOut | Wait::Exited => Reading::new(ran.as_secs_f64() * 1000.0, false),
        };
        Ok(RunValue::from([(key, Some(reading))]))
    }
}

/// One measurement the rounds of a phase make.
struct Metric {
    /// The series it is made of.
    key: &'static str,
    /// What the document calls it.
    name: &'static str,
    /// The profile it ran under, when it is an interface metric.
    profile: Option<Profile>,
    /// How fast the terminal read, in bytes per second, when it was slow.
    drain: Option<u64>,
    /// What each series is held against besides the candidate: the name of the ratio, and what it
    /// is taken against.
    also: &'static [(&'static str, Against)],
}

/// The unthrottled phase: in each round every version runs a headless scan and then an interface
/// run to COMPLETE, so that the two are measured in the same round. Makes the measurements of the
/// scan's wall time (against `du -sk` as well), of the writing of its report, and of the interface.
fn time_unthrottled(
    on: &TimedFixture<'_>,
    du: Option<&Du>,
    output: &mut Output,
    measurements: &mut Vec<MeasuredSet>,
    progress: &mut impl FnMut(&str),
) -> Result<(), SweepError> {
    let options = on.options;
    let (root, work_dir) = (&on.fixture.root, options.work_dir.as_path());
    let paired = paired_rounds(
        on.names,
        &UNTHROTTLED_LEGS,
        options.rounds,
        Some(GIVE_UP_AFTER),
        || du.and_then(|du| timed_du(du, root, work_dir, options.timeout)),
        |position, leg| {
            let binary = on.binary(position)?;
            if leg == 0 {
                on.scan(binary)
            } else {
                on.interface(binary, Profile::Default, None, TUI_DEFAULT)
            }
        },
    );
    let metrics = [
        Metric {
            key: HEADLESS_WALL,
            name: "headless_wall_ms",
            profile: None,
            drain: None,
            also: &[("du", Against::Du)],
        },
        Metric {
            key: REPORT_WRITE,
            name: "report_write_ms",
            profile: None,
            drain: None,
            also: &[],
        },
        Metric {
            key: HEADLESS_SCAN,
            name: "headless_scan_ms",
            profile: None,
            drain: None,
            also: &[],
        },
        Metric {
            key: TUI_DEFAULT,
            name: "tui_complete_ms",
            profile: Some(Profile::Default),
            drain: None,
            also: &[("headless-scan", Against::Own(HEADLESS_SCAN))],
        },
    ];
    for metric in &metrics {
        keep(on, output, measurements, &paired, metric, progress)?;
    }
    Ok(())
}

/// The phase against a slow terminal: in each round every version runs an interface run to
/// COMPLETE under the default profile and then one under reduced motion, so that the two are
/// measured in the same round.
fn time_drained(
    on: &TimedFixture<'_>,
    output: &mut Output,
    measurements: &mut Vec<MeasuredSet>,
    progress: &mut impl FnMut(&str),
) -> Result<(), SweepError> {
    let options = on.options;
    let drain = Some(options.drain_bytes_per_sec);
    let paired = paired_rounds(
        on.names,
        &DRAINED_LEGS,
        options.rounds,
        Some(GIVE_UP_AFTER),
        || None,
        |position, leg| {
            let binary = on.binary(position)?;
            if leg == 0 {
                on.interface(binary, Profile::Default, drain, TUI_DEFAULT_DRAINED)
            } else {
                on.interface(binary, Profile::ReducedMotion, drain, TUI_REDUCED_DRAINED)
            }
        },
    );
    let metrics = [
        Metric {
            key: TUI_DEFAULT_DRAINED,
            name: "tui_complete_ms",
            profile: Some(Profile::Default),
            drain,
            also: &[("reduced-motion", Against::Own(TUI_REDUCED_DRAINED))],
        },
        Metric {
            key: TUI_REDUCED_DRAINED,
            name: "tui_complete_ms",
            profile: Some(Profile::ReducedMotion),
            drain,
            also: &[],
        },
    ];
    for metric in &metrics {
        keep(on, output, measurements, &paired, metric, progress)?;
    }
    Ok(())
}

/// Keeps the measurement `metric` makes of the rounds of `paired`, and records an errored check for
/// every version whose runs of it could not be carried out.
fn keep(
    on: &TimedFixture<'_>,
    output: &mut Output,
    measurements: &mut Vec<MeasuredSet>,
    paired: &Paired,
    metric: &Metric,
    progress: &mut impl FnMut(&str),
) -> Result<(), SweepError> {
    let options = on.options;
    let against_du = metric
        .also
        .iter()
        .any(|(_, against)| *against == Against::Du);
    let measurement = SweepMeasurement {
        metric: metric.name.to_owned(),
        fixture: on.id.to_owned(),
        profile: metric.profile,
        drain_bytes_per_sec: metric.drain,
        rounds: options.rounds,
        order: paired.order.clone(),
        du_samples: if against_du && paired.du.iter().any(Option::is_some) {
            paired.du.clone()
        } else {
            Vec::new()
        },
        series: series_of(metric.key, on.names, paired, options.seed, metric.also),
    };
    let mut errors = BTreeMap::new();
    for (name, runs) in &paired.runs {
        if let Some(error) = runs.errors.get(metric.key) {
            errors.insert(name.clone(), error.clone());
            let mut record = Recorded::errored(
                &format!("timing-{}", measurement.metric),
                Some(&measurement.fixture),
                measurement.profile,
                error.clone(),
            );
            if let Some(drain) = measurement.drain_bytes_per_sec {
                record.notes.push(format!(
                    "against a terminal that reads {drain} bytes a second"
                ));
            }
            output.record(name, record)?;
        }
    }
    let describe = |series: &crate::report::SweepSeries| {
        let median = series
            .median
            .map_or_else(|| "none".to_owned(), |ms| format!("{ms:.0} ms"));
        let mut text = format!("{} median {median}", series.reference);
        let unfinished = series.completed.iter().filter(|done| !**done).count();
        if unfinished > 0 {
            text = format!("{text}, {unfinished} did not finish");
        }
        if series.skipped > 0 {
            text = format!("{text}, {} skipped", series.skipped);
        }
        text
    };
    progress(&format!(
        "  {} on `{}`{}{}: {}",
        measurement.metric,
        measurement.fixture,
        measurement
            .profile
            .map_or_else(String::new, |profile| format!(" ({profile})")),
        measurement
            .drain_bytes_per_sec
            .map_or_else(String::new, |drain| format!(" at {drain} B/s")),
        measurement
            .series
            .iter()
            .map(describe)
            .collect::<Vec<_>>()
            .join(", ")
    ));
    let index = measurements.len();
    measurements.push(MeasuredSet {
        index,
        measurement,
        errors,
    });
    Ok(())
}
