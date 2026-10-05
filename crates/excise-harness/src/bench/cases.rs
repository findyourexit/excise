//! What one `--fixture` or `--scenario --profile` compares, and the metrics it yields.
//!
//! A `--fixture` case shares one warm root (a cached master, or one run copy when the fixture
//! cannot be cached) across every measured run: the scan never writes to it, so reusing it keeps
//! the comparison on the same warm tree, exactly as
//! [`crate::headless::suite`] does. A `--scenario` case cannot share a root the same way: a
//! scenario may delete or mutate its fixture, so every run — baseline or candidate, warm-up or
//! measured — gets its own fresh copy, exactly as [`crate::runner::run_e2e`] does; what stays
//! "the same" across its pairs is the fixture's spec and seed, not one mutable tree.

use std::{
    collections::BTreeMap,
    collections::BTreeSet,
    path::{Path, PathBuf},
    time::Duration,
};

use thiserror::Error;

use crate::{
    fixture::{FixtureError, Fixtures, Materialized, Plan, RunCopy},
    headless::{
        pairs::millis,
        scan::{ScanError, ScanRequest, run_scan},
    },
    report::AbFixture,
    runner::{LatencyScale, RunRequest, run_scenario},
    safety::{FixtureRoot, SafetyError},
    scenario::{Profile, Scenario, Step},
};

/// The fixed headless-fixture metrics: wall time, user and system CPU, and peak memory.
pub const FIXTURE_METRICS: [&str; 4] = [
    "wall_time_ms",
    "user_cpu_ms",
    "sys_cpu_ms",
    "peak_memory_bytes",
];

/// The fixed scenario metrics a comparison always asks for when the run recorded them:
/// `scan_complete_ms`, `first_frame_ms`, `input_to_frame_p99_ms`, `max_stall_ms`, and
/// `peak_rss_bytes`. A scenario's own `measure` names are added on top of these.
pub const SCENARIO_METRICS: [&str; 5] = [
    "scan_complete_ms",
    "first_frame_ms",
    "input_to_frame_p99_ms",
    "max_stall_ms",
    "peak_rss_bytes",
];

/// One thing a `bench-e2e` run compares: a headless fixture scan, or a PTY scenario under a
/// profile.
#[derive(Debug, Clone)]
pub enum Case {
    /// `--fixture ID`.
    Fixture {
        /// The fixture id.
        id: String,
    },
    /// `--scenario NAME --profile P`.
    Scenario {
        /// The loaded, validated scenario, boxed: a scenario is far larger than a fixture id.
        scenario: Box<Scenario>,
        /// The profile to run it under.
        profile: Profile,
    },
}

impl Case {
    /// The key that qualifies this case's metric names: the fixture id, or
    /// `<scenario>-<profile>`.
    #[must_use]
    pub fn key(&self) -> String {
        match self {
            Self::Fixture { id } => id.clone(),
            Self::Scenario { scenario, profile } => format!("{}-{profile}", scenario.name),
        }
    }

    /// The underlying fixture id this case runs against.
    #[must_use]
    pub fn fixture_id(&self) -> &str {
        match self {
            Self::Fixture { id } => id,
            Self::Scenario { scenario, .. } => &scenario.fixture,
        }
    }

    /// The metric names this case asks for; a name is dropped from the comparison if either side
    /// of any pair did not produce it (see [`run_once`]'s caller in
    /// [`crate::bench::run::run_bench_e2e`]).
    #[must_use]
    pub fn candidate_metrics(&self) -> BTreeSet<String> {
        match self {
            Self::Fixture { .. } => FIXTURE_METRICS
                .iter()
                .map(|&name| name.to_owned())
                .collect(),
            Self::Scenario { scenario, .. } => {
                let mut names: BTreeSet<String> = SCENARIO_METRICS
                    .iter()
                    .map(|&name| name.to_owned())
                    .collect();
                names.extend(scenario.steps.iter().filter_map(|step| match step {
                    Step::Measure(measure) => Some(measure.name.clone()),
                    _ => None,
                }));
                names
            }
        }
    }
}

/// Qualifies `metric` with `case_key` so metrics from different cases in one comparison never
/// collide (see [`crate::report::MetricComparison::name`]).
#[must_use]
pub fn qualify(case_key: &str, metric: &str) -> String {
    format!("{case_key}__{metric}")
}

/// The bare metric name: the part of a `qualify`-d name after the last `__`.
#[must_use]
pub fn bare_metric_name(qualified: &str) -> &str {
    qualified.rsplit("__").next().unwrap_or(qualified)
}

/// One case could not be run or measured.
#[derive(Debug, Error)]
pub enum CaseError {
    /// The fixture could not be acquired.
    #[error(transparent)]
    Fixture(#[from] FixtureError),
    /// The fixture root failed the ownership check.
    #[error(transparent)]
    Safety(#[from] SafetyError),
    /// The scan process could not be run.
    #[error(transparent)]
    Scan(#[from] ScanError),
    /// A work directory could not be created.
    #[error("cannot create a work directory below `{}`: {source}", parent.display())]
    Workspace {
        /// The parent directory.
        parent: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
    /// The scan ran past its deadline, so its wall time is not a measurement of completion.
    #[error("the scan of fixture `{id}` did not finish within {timeout:?}")]
    ScanTimedOut {
        /// The fixture id.
        id: String,
        /// The bound that passed.
        timeout: Duration,
    },
    /// The harness could not run the scenario at all.
    #[error("cannot run scenario `{scenario}` under `{profile}`: {reason}")]
    ScenarioErrored {
        /// The scenario name.
        scenario: String,
        /// The profile.
        profile: Profile,
        /// Why.
        reason: String,
    },
    /// The scenario ran and failed a step.
    #[error("scenario `{scenario}` under `{profile}` failed: {failure}")]
    ScenarioFailed {
        /// The scenario name.
        scenario: String,
        /// The profile.
        profile: Profile,
        /// The failed step, rendered as text.
        failure: String,
    },
}

/// A fixture root shared by every measured run of a `--fixture` case: a cached master, read-only,
/// or (when the fixture cannot be cached where this cache is, [`Fixtures::is_cacheable`] is false)
/// one run copy reused for every scan. Dropping it removes the run copy; a master is left in the
/// cache.
pub enum SharedFixture {
    /// A cached master, read-only.
    Master(Materialized),
    /// One run copy, reused because the fixture cannot be cached.
    RunCopy(RunCopy),
}

impl SharedFixture {
    /// Acquires the shared root of fixture `id`: the cached master if it is cacheable, else one
    /// fresh run copy below `parent`.
    ///
    /// # Errors
    ///
    /// Returns why the spec cannot be loaded or the fixture cannot be generated.
    pub fn acquire(fixtures: &Fixtures, id: &str, parent: &Path) -> Result<Self, CaseError> {
        match fixtures.master(id) {
            Ok(master) => Ok(Self::Master(master)),
            Err(FixtureError::NotCacheable { .. }) => {
                Ok(Self::RunCopy(fixtures.run_copy(id, parent)?))
            }
            Err(error) => Err(error.into()),
        }
    }

    /// The fixture root.
    #[must_use]
    pub fn root(&self) -> &Path {
        match self {
            Self::Master(master) => &master.root,
            Self::RunCopy(copy) => copy.root(),
        }
    }

    /// The plan the fixture was generated from: its manifest hash and seed.
    #[must_use]
    pub fn plan(&self) -> &Plan {
        match self {
            Self::Master(master) => master.plan(),
            Self::RunCopy(copy) => copy.plan(),
        }
    }

    /// The context identity of this fixture: its id, manifest hash, and seed.
    #[must_use]
    pub fn identity(&self, id: &str) -> AbFixture {
        AbFixture {
            id: id.to_owned(),
            hash: self.plan().manifest_sha256().to_owned(),
            seed: self.plan().seed(),
        }
    }
}

/// Runs `binary` once against `shared_root` and returns the [`FIXTURE_METRICS`] it produced.
///
/// # Errors
///
/// Returns [`CaseError::Safety`] when the root fails the ownership check, [`CaseError::Scan`] when
/// the process cannot be run, and [`CaseError::ScanTimedOut`] when it does not finish in time.
pub fn run_fixture_once(
    binary: &Path,
    shared_root: &Path,
    work_dir: &Path,
    timeout: Duration,
) -> Result<BTreeMap<String, f64>, CaseError> {
    let fixture = FixtureRoot::open(shared_root)?;
    let run = run_scan(&ScanRequest {
        binary,
        fixture: &fixture,
        work_dir,
        profile: Profile::Default,
        timeout,
    })?;
    if run.finished.timed_out {
        return Err(CaseError::ScanTimedOut {
            id: shared_root.display().to_string(),
            timeout,
        });
    }
    let mut metrics = BTreeMap::new();
    metrics.insert("wall_time_ms".to_owned(), millis(run.finished.wall));
    if let Some(cpu) = run.finished.cpu {
        metrics.insert("user_cpu_ms".to_owned(), millis(cpu.user));
        metrics.insert("sys_cpu_ms".to_owned(), millis(cpu.system));
    }
    if let Some(bytes) = run.finished.peak_memory_bytes {
        #[allow(clippy::cast_precision_loss)]
        metrics.insert("peak_memory_bytes".to_owned(), bytes as f64);
    }
    Ok(metrics)
}

/// Runs `binary` once against a fresh copy of `scenario`'s fixture, generated below
/// `parent_work_dir`, and returns the metrics the run recorded together with that fixture's
/// identity (its id, manifest hash, and seed; the same for every run of this scenario, since
/// every copy shares the same spec and seed).
///
/// # Errors
///
/// Returns [`CaseError::Workspace`] when a work directory cannot be created,
/// [`CaseError::Fixture`] when the fresh copy cannot be generated, [`CaseError::ScenarioErrored`]
/// when the harness could not run the scenario at all, and [`CaseError::ScenarioFailed`] when it
/// ran and failed a step.
pub fn run_scenario_once(
    scenario: &Scenario,
    profile: Profile,
    binary: &Path,
    fixtures: &Fixtures,
    parent_work_dir: &Path,
) -> Result<(BTreeMap<String, f64>, AbFixture), CaseError> {
    let workspace = tempfile::Builder::new()
        .prefix("bench-")
        .tempdir_in(parent_work_dir)
        .map_err(|source| CaseError::Workspace {
            parent: parent_work_dir.to_path_buf(),
            source,
        })?;
    let fixture = fixtures.run_copy(&scenario.fixture, workspace.path())?;
    let identity = AbFixture {
        id: scenario.fixture.clone(),
        hash: fixture.plan().manifest_sha256().to_owned(),
        seed: fixture.plan().seed(),
    };
    let repro = format!(
        "cargo xtask bench-e2e --baseline <ref> --scenario {} --profile {profile}",
        scenario.name
    );
    let report = run_scenario(&RunRequest {
        scenario,
        profile,
        binary,
        fixture_root: fixture.root(),
        work_dir: workspace.path(),
        scan_store_dir: None,
        bundle_dir: None,
        repro_command: &repro,
        fixture_seed: fixture.plan().seed(),
        keep_scratch: false,
        // Timing evidence is measured against the strict budgets, never a loosened one, and a
        // missed budget fails the case, never a warning.
        latency_scale: LatencyScale::STRICT,
        timing_informational: false,
    });
    if let Some(reason) = report.error {
        return Err(CaseError::ScenarioErrored {
            scenario: scenario.name.clone(),
            profile,
            reason,
        });
    }
    if let Some(failure) = report.failure {
        return Err(CaseError::ScenarioFailed {
            scenario: scenario.name.clone(),
            profile,
            failure: failure.to_string(),
        });
    }
    Ok((report.metrics, identity))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scenario(steps: &str) -> Scenario {
        Scenario::from_toml_str(&format!(
            r#"
schema_version = 1
name = "s"
description = "d"
fixture = "f"
profiles = ["default"]
{steps}
"#
        ))
        .expect("a scenario")
    }

    #[test]
    fn a_fixture_case_asks_for_the_four_fixed_metrics() {
        let case = Case::Fixture {
            id: "wide-1k".to_owned(),
        };

        assert_eq!(case.key(), "wide-1k");
        assert_eq!(case.fixture_id(), "wide-1k");
        assert_eq!(
            case.candidate_metrics(),
            FIXTURE_METRICS.iter().map(|&n| n.to_owned()).collect()
        );
    }

    #[test]
    fn a_scenario_case_key_joins_the_name_and_profile() {
        let case = Case::Scenario {
            scenario: Box::new(scenario("[[steps]]\nstep = \"quit\"\n")),
            profile: Profile::Deterministic,
        };

        assert_eq!(case.key(), "s-deterministic");
        assert_eq!(case.fixture_id(), "f");
    }

    #[test]
    fn a_scenario_case_adds_its_own_measure_names_to_the_fixed_set() {
        let case = Case::Scenario {
            scenario: Box::new(scenario(
                "[[steps]]\nstep = \"measure\"\nname = \"delete-time\"\nmarker = \"start\"\n\n\
                 [[steps]]\nstep = \"measure\"\nname = \"delete-time\"\nmarker = \"stop\"\n",
            )),
            profile: Profile::Default,
        };

        let metrics = case.candidate_metrics();
        assert!(metrics.contains("delete-time"));
        assert!(metrics.contains("scan_complete_ms"));
        assert_eq!(metrics.len(), SCENARIO_METRICS.len() + 1);
    }

    #[test]
    fn qualified_names_round_trip_through_bare_metric_name() {
        let qualified = qualify("wide-1k", "wall_time_ms");
        assert_eq!(qualified, "wide-1k__wall_time_ms");
        assert_eq!(bare_metric_name(&qualified), "wall_time_ms");

        let qualified = qualify("delete-folder-lifecycle-default", "peak_rss_bytes");
        assert_eq!(bare_metric_name(&qualified), "peak_rss_bytes");
    }

    /// A `--fixture` case shares one root: the cached master, unless a path-based removal could
    /// not take the fixture's paths below the cache, and then one run copy. Where the cache is
    /// decides it, as for the headless suite.
    #[test]
    fn a_fixture_is_one_run_copy_when_the_cache_root_leaves_its_paths_no_room() {
        use crate::fixture::{
            FixtureCache,
            tests::support::{chain_spec, path_of_len},
        };

        let work = tempfile::tempdir().expect("a work directory");
        let specs = work.path().join("specs");
        std::fs::create_dir(&specs).expect("a directory of specs");
        // A chain whose longest path is 4 + 4 * 101 + 1 + 103 = 512 bytes.
        std::fs::write(specs.join("chain.toml"), chain_spec("chain", 4, 100, 103)).expect("a spec");

        let short = Fixtures::new(&specs, FixtureCache::at(work.path().join("cache")));
        let shared = SharedFixture::acquire(&short, "chain", work.path()).expect("acquired");
        assert!(
            matches!(shared, SharedFixture::Master(_)),
            "a short cache root holds it"
        );

        // 500 bytes, a separator, the longest name the cache gives a directory (57), a separator,
        // and 512 bytes is past the 1,023 that `PATH_MAX` leaves on macOS.
        let long_root = path_of_len(work.path(), 500);
        let long = Fixtures::new(&specs, FixtureCache::at(&long_root));
        let shared = SharedFixture::acquire(&long, "chain", work.path()).expect("acquired");
        assert!(
            matches!(shared, SharedFixture::RunCopy(_)),
            "a long cache root leaves it no room, and it is one run copy"
        );
        assert!(!long_root.exists(), "and nothing was cached in it");
    }
}
