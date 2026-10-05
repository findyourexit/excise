//! Building the warm-up and measured pairs of every case, and assembling the `harness-ab`
//! document.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use thiserror::Error;

use crate::{
    fixture::Fixtures,
    report::{
        AbContext, AbFixture, AbKind, AbVerdict, BuildIdentity, Document, HarnessAb,
        MetricComparison, Samples, SchemaVersion, Side,
    },
    run_support::{compact_utc, host_name, point_latest_at, render_rows, sha256_file},
};

use super::{
    bootstrap::{bootstrap_ci, median_ratio},
    cases::{self, Case, CaseError, SharedFixture, bare_metric_name, qualify},
    context::{Snapshot, load_average_one_minute, os_description, power_state, toolchain},
    pairing::{align, interleaving},
    verdict::{MetricKind, classify},
};

/// What to compare and how.
#[derive(Debug, Clone)]
pub struct BenchOptions {
    /// The baseline binary, already built or supplied by `--baseline-binary`.
    pub baseline_binary: PathBuf,
    /// The git reference the baseline was built from, or a label when it was supplied as a
    /// binary without one. Recorded as-is in `context.baseline.git_ref`.
    pub baseline_ref: String,
    /// The candidate binary: the current checkout's release build, or `--candidate-binary`.
    pub candidate_binary: PathBuf,
    /// The git reference the candidate was built from (normally the current checkout's `HEAD`).
    pub candidate_ref: String,
    /// Where the fixtures of the `Case::Fixture` cases come from: the bundled specs, or the specs
    /// of a directory of the operator's own (`cargo xtask bench-e2e --fixture-dir`).
    pub fixtures: Fixtures,
    /// Where the fixtures of the `Case::Scenario` cases come from. A scenario names a bundled
    /// fixture, so this is the bundled specs whatever `fixtures` is. A fixture id that a
    /// `Case::Fixture` and a `Case::Scenario` both run on has to name one tree in both, because
    /// the document names a fixture by its id alone: see [`BenchError::FixtureIdCollision`].
    pub scenario_fixtures: Fixtures,
    /// The cases to compare, in the order they run. Never empty.
    pub cases: Vec<Case>,
    /// The number of measured pairs, after one untimed warm-up pair.
    pub pairs: u32,
    /// The seed for the deterministic bootstrap.
    pub seed: u64,
    /// The fraction a timing metric may be worse by before it blocks (the `timing_ab_regression`
    /// budget; default `0.20`).
    pub timing_threshold: f64,
    /// The fraction a memory metric may move by, either direction, before it blocks (the
    /// `memory_ab_tolerance` budget; default `0.05`).
    pub memory_tolerance: f64,
    /// Aborts instead of warning when other `excise` processes are found.
    pub strict: bool,
    /// How long one scan or one scenario run may take.
    pub timeout: Duration,
    /// The directory fixtures' scratch areas and run copies are made in.
    pub work_dir: PathBuf,
    /// The output root, normally `target/excise-bench-e2e`.
    pub out_root: PathBuf,
}

/// The comparison could not be completed.
#[derive(Debug, Error)]
pub enum BenchError {
    /// No case was given.
    #[error("nothing to compare: pass --fixture or --scenario at least once")]
    NothingToCompare,
    /// One fixture id names two different trees: the spec of `--fixture-dir` and the bundled spec
    /// that a selected scenario runs on.
    #[error(
        "the fixture id `{id}` names two different fixtures in one comparison: the spec of \
         `--fixture-dir` and the bundled spec that scenario `{scenario}` runs on; `ab.json` names \
         a fixture by its id alone, so rename the spec in the directory"
    )]
    FixtureIdCollision {
        /// The fixture id.
        id: String,
        /// The first selected scenario that runs on the bundled fixture with that id.
        scenario: String,
    },
    /// `--strict` and another `excise` process is running.
    #[error(
        "{count} other `excise` process(es) are running and --strict was given; pass without \
         --strict to proceed with a warning instead"
    )]
    ConcurrentExcise {
        /// How many were found.
        count: u32,
    },
    /// A case could not be run or measured.
    #[error(transparent)]
    Case(#[from] CaseError),
    /// A binary could not be hashed.
    #[error("cannot hash `{}`: {source}", path.display())]
    Hash {
        /// The binary.
        path: PathBuf,
        /// The underlying error.
        source: io::Error,
    },
    /// A file or directory could not be created or written.
    #[error("{context}: {source}")]
    Io {
        /// What was being done.
        context: String,
        /// The underlying error.
        source: io::Error,
    },
    /// The document could not be rendered.
    #[error("cannot render the comparison: {0}")]
    Json(#[from] serde_json::Error),
}

fn io_error(context: impl Into<String>) -> impl FnOnce(io::Error) -> BenchError {
    let context = context.into();
    move |source| BenchError::Io { context, source }
}

/// What one comparison produced.
#[derive(Debug)]
pub struct BenchReport {
    /// The `harness-ab` document.
    pub document: HarnessAb,
    /// Where it was written.
    pub document_path: PathBuf,
    /// The run's output directory.
    pub run_dir: PathBuf,
}

impl BenchReport {
    /// Whether no metric blocks.
    #[must_use]
    pub fn is_success(&self) -> bool {
        !self
            .document
            .metrics
            .iter()
            .any(|metric| metric.verdict == AbVerdict::Block)
    }

    /// The verdict table: one row per metric.
    #[must_use]
    pub fn table(&self) -> String {
        let mut rows = vec![[
            "metric".to_owned(),
            "median ratio".to_owned(),
            "95% ci".to_owned(),
            "verdict".to_owned(),
        ]];
        for metric in &self.document.metrics {
            let ci = metric.bootstrap_ci;
            rows.push([
                metric.name.clone(),
                format!("{:.3}", metric.median_ratio),
                format!("[{:.3}, {:.3}]", ci.lower, ci.upper),
                metric.verdict.to_string(),
            ]);
        }
        render_rows(&rows)
    }
}

/// Runs every case's warm-up pair and measured pairs, and assembles and writes the `harness-ab`
/// document.
///
/// # Errors
///
/// Returns [`BenchError::NothingToCompare`] when `options.cases` is empty,
/// [`BenchError::FixtureIdCollision`] when a fixture id that a `Case::Fixture` and a
/// `Case::Scenario` both run on names one spec in `options.fixtures` and another in
/// `options.scenario_fixtures` (checked before anything is created or run),
/// [`BenchError::ConcurrentExcise`] when `--strict` was given and another `excise` process is
/// running, [`BenchError::Case`] when a case cannot be run or measured, and an I/O or rendering
/// error when the output cannot be written.
pub fn run_bench_e2e(options: &BenchOptions) -> Result<BenchReport, BenchError> {
    if options.cases.is_empty() {
        return Err(BenchError::NothingToCompare);
    }
    refuse_ambiguous_fixture_ids(options)?;
    fs::create_dir_all(&options.work_dir).map_err(io_error(format!(
        "cannot create `{}`",
        options.work_dir.display()
    )))?;
    fs::create_dir_all(&options.out_root).map_err(io_error(format!(
        "cannot create `{}`",
        options.out_root.display()
    )))?;

    let snapshot = Snapshot::take();
    let concurrent = snapshot.concurrent_excise_processes();
    if concurrent > 0 {
        if options.strict {
            return Err(BenchError::ConcurrentExcise { count: concurrent });
        }
        eprintln!(
            "warning: {concurrent} other `excise` process(es) are running; timings may be \
             noisy (pass --strict to abort instead of warning)"
        );
    }
    let load_average_start = load_average_one_minute();

    let baseline_sha256 =
        sha256_file(&options.baseline_binary).map_err(|source| BenchError::Hash {
            path: options.baseline_binary.clone(),
            source,
        })?;
    let candidate_sha256 =
        sha256_file(&options.candidate_binary).map_err(|source| BenchError::Hash {
            path: options.candidate_binary.clone(),
            source,
        })?;

    let order = interleaving(options.pairs);
    let mut raw: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    let mut fixtures_seen: BTreeMap<String, AbFixture> = BTreeMap::new();

    for case in &options.cases {
        run_case(options, case, &order, &mut raw, &mut fixtures_seen)?;
    }

    let load_average_end = load_average_one_minute();

    let metrics = compare_metrics(raw, &order, options);

    let document = HarnessAb {
        document_kind: AbKind::HarnessAb,
        schema_version: SchemaVersion,
        baseline: BuildIdentity {
            git_ref: options.baseline_ref.clone(),
            binary_sha256: baseline_sha256,
        },
        candidate: BuildIdentity {
            git_ref: options.candidate_ref.clone(),
            binary_sha256: candidate_sha256,
        },
        trials: options.pairs,
        interleaving: order,
        metrics,
        context: AbContext {
            host: host_name(),
            cpu: snapshot.cpu_model(),
            os: os_description(),
            arch: std::env::consts::ARCH.to_owned(),
            logical_cpus: snapshot.logical_cpus(),
            toolchain: toolchain(),
            power: power_state(),
            load_average_start,
            load_average_end,
            concurrent_excise_processes: concurrent,
            fixtures: fixtures_seen.into_values().collect(),
        },
    };

    let run_id = format!("{}-{}", compact_utc(SystemTime::now()), std::process::id());
    let run_dir = options.out_root.join(&run_id);
    fs::create_dir_all(&run_dir)
        .map_err(io_error(format!("cannot create `{}`", run_dir.display())))?;
    let document_path = run_dir.join("ab.json");
    fs::write(&document_path, document.to_json_pretty()?).map_err(io_error(format!(
        "cannot write `{}`",
        document_path.display()
    )))?;
    point_latest_at(&options.out_root, &run_id)
        .map_err(io_error("cannot update the `latest` pointer"))?;

    Ok(BenchReport {
        document,
        document_path,
        run_dir,
    })
}

/// Refuses a comparison in which one fixture id would name two different trees.
///
/// The `Case::Fixture` cases take their specs from `options.fixtures` (the bundled specs, or
/// those of `--fixture-dir`) and the `Case::Scenario` cases from `options.scenario_fixtures`
/// (the bundled specs: a scenario names a bundled fixture). The document names a fixture by its
/// id alone, and the metric names of a scenario do not say which fixture it ran on, so an id
/// that cases of both kinds run on has to name one tree. Two specs are one tree when their
/// canonical text, the text the cache key is made from, is the same: a description, or the way
/// the file is laid out, does not make them two. A spec that cannot be loaded is left to its
/// case, which reports why.
fn refuse_ambiguous_fixture_ids(options: &BenchOptions) -> Result<(), BenchError> {
    let mut fixture_ids: BTreeSet<&str> = BTreeSet::new();
    // Each fixture id a scenario runs on, with the first scenario that does.
    let mut scenario_ids: BTreeMap<&str, &str> = BTreeMap::new();
    for case in &options.cases {
        match case {
            Case::Fixture { id } => {
                fixture_ids.insert(id);
            }
            Case::Scenario { scenario, .. } => {
                scenario_ids
                    .entry(&scenario.fixture)
                    .or_insert(&scenario.name);
            }
        }
    }
    for (id, scenario) in scenario_ids {
        if !fixture_ids.contains(id) {
            continue;
        }
        let own = canonical_spec(&options.fixtures, id);
        let bundled = canonical_spec(&options.scenario_fixtures, id);
        let (Some(own), Some(bundled)) = (own, bundled) else {
            continue;
        };
        if own != bundled {
            return Err(BenchError::FixtureIdCollision {
                id: id.to_owned(),
                scenario: scenario.to_owned(),
            });
        }
    }
    Ok(())
}

/// The canonical text of the spec `id` of `fixtures`, the text its cache key is made from, or
/// `None` when the spec cannot be loaded.
fn canonical_spec(fixtures: &Fixtures, id: &str) -> Option<String> {
    let spec = fixtures.spec(id).ok()?;
    Some(spec.canonical_json())
}

/// Turns every qualified metric's flat, run-ordered samples into a [`MetricComparison`]: splits
/// them back into the two sides, computes the median ratio and its bootstrap interval, and
/// classifies the verdict.
fn compare_metrics(
    raw: BTreeMap<String, Vec<f64>>,
    order: &[Side],
    options: &BenchOptions,
) -> Vec<MetricComparison> {
    raw.into_iter()
        .map(|(name, values)| {
            let (baseline, candidate) = align(order, &values);
            let median_ratio = median_ratio(&baseline, &candidate);
            let bootstrap_ci = bootstrap_ci(&baseline, &candidate, options.seed);
            let kind = MetricKind::of(bare_metric_name(&name));
            let verdict = classify(
                kind,
                median_ratio,
                bootstrap_ci,
                options.timing_threshold,
                options.memory_tolerance,
            );
            MetricComparison {
                name,
                samples: Samples {
                    baseline,
                    candidate,
                },
                median_ratio,
                bootstrap_ci,
                verdict,
            }
        })
        .collect()
}

/// Runs one case's warm-up pair and measured pairs: baseline then candidate, `options.pairs + 1`
/// times, the first discarded. Records each metric the case produced on every measured run, in
/// run order, under its qualified name; a metric missing from any one run is dropped from the
/// comparison entirely, so every recorded metric stays index-aligned with `order`.
fn run_case(
    options: &BenchOptions,
    case: &Case,
    order: &[Side],
    raw: &mut BTreeMap<String, Vec<f64>>,
    fixtures_seen: &mut BTreeMap<String, AbFixture>,
) -> Result<(), CaseError> {
    const WARM_UP: usize = 2;

    let key = case.key();
    let wanted = case.candidate_metrics();
    let full_order: Vec<Side> = [Side::Baseline, Side::Candidate]
        .into_iter()
        .chain(order.iter().copied())
        .collect();

    let mut by_metric: BTreeMap<String, Vec<f64>> = wanted
        .iter()
        .map(|name| (name.clone(), Vec::with_capacity(order.len())))
        .collect();

    match case {
        Case::Fixture { id } => {
            let shared = SharedFixture::acquire(&options.fixtures, id, &options.work_dir)?;
            fixtures_seen
                .entry(id.clone())
                .or_insert_with(|| shared.identity(id));
            let root = shared.root().to_path_buf();
            for (index, side) in full_order.iter().enumerate() {
                let binary = binary_for(options, *side);
                let produced =
                    cases::run_fixture_once(binary, &root, &options.work_dir, options.timeout)?;
                if index >= WARM_UP {
                    record(&produced, &mut by_metric);
                }
            }
        }
        Case::Scenario { scenario, profile } => {
            for (index, side) in full_order.iter().enumerate() {
                let binary = binary_for(options, *side);
                let (produced, identity) = cases::run_scenario_once(
                    scenario,
                    *profile,
                    binary,
                    &options.scenario_fixtures,
                    &options.work_dir,
                )?;
                fixtures_seen
                    .entry(scenario.fixture.clone())
                    .or_insert(identity);
                if index >= WARM_UP {
                    record(&produced, &mut by_metric);
                }
            }
        }
    }

    for (metric, values) in by_metric {
        if values.len() == order.len() {
            raw.entry(qualify(&key, &metric))
                .or_default()
                .extend(values);
        } else {
            eprintln!(
                "note: `{key}` did not produce `{metric}` on every run; it is dropped from the \
                 comparison ({} of {} runs had it)",
                values.len(),
                order.len()
            );
        }
    }
    Ok(())
}

/// The binary for `side`.
fn binary_for(options: &BenchOptions, side: Side) -> &Path {
    match side {
        Side::Baseline => &options.baseline_binary,
        Side::Candidate => &options.candidate_binary,
    }
}

/// Appends every wanted metric `produced` has to `by_metric`.
fn record(produced: &BTreeMap<String, f64>, by_metric: &mut BTreeMap<String, Vec<f64>>) {
    for (metric, values) in by_metric.iter_mut() {
        if let Some(value) = produced.get(metric) {
            values.push(*value);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        fixture::FixtureCache,
        scenario::{Profile, Scenario},
    };

    use super::*;

    /// A directory of specs that holds one file, `<id>.toml`, with the text `text`, and the facade
    /// that reads it. Nothing is generated: the check reads specs and nothing else.
    fn specs_with(id: &str, text: &str) -> (tempfile::TempDir, Fixtures) {
        let dir = tempfile::tempdir().expect("a directory of specs");
        fs::write(dir.path().join(format!("{id}.toml")), text).expect("a spec is written");
        let cache = FixtureCache::at(dir.path().join("cache"));
        let fixtures = Fixtures::new(dir.path(), cache);
        (dir, fixtures)
    }

    /// The same, for the spec of a tree of one file below `root`.
    fn specs(id: &str, description: &str, root: &str) -> (tempfile::TempDir, Fixtures) {
        let spec = format!(
            r#"
schema_version = 1
id = "{id}"
description = "{description}"
seed = 1

[[parts]]
kind = "tree"
root = "{root}"
depth = 0
files_per_dir = 1
"#
        );
        specs_with(id, &spec)
    }

    fn fixture_case(id: &str) -> Case {
        Case::Fixture { id: id.into() }
    }

    /// A scenario case that runs on the fixture `fixture`.
    fn scenario_case(name: &str, fixture: &str) -> Case {
        let scenario = Scenario::from_toml_str(&format!(
            r#"
schema_version = 1
name = "{name}"
description = "d"
fixture = "{fixture}"
profiles = ["default"]

[[steps]]
step = "quit"
"#
        ))
        .expect("a scenario");
        Case::Scenario {
            scenario: Box::new(scenario),
            profile: Profile::Default,
        }
    }

    /// The check on `cases`, with `own` for the fixture cases and `bundled` for the scenarios.
    fn check(cases: Vec<Case>, own: Fixtures, bundled: Fixtures) -> Result<(), BenchError> {
        refuse_ambiguous_fixture_ids(&BenchOptions {
            baseline_binary: PathBuf::from("baseline"),
            baseline_ref: "baseline".to_owned(),
            candidate_binary: PathBuf::from("candidate"),
            candidate_ref: "candidate".to_owned(),
            fixtures: own,
            scenario_fixtures: bundled,
            cases,
            pairs: 1,
            seed: 0,
            timing_threshold: 0.2,
            memory_tolerance: 0.05,
            strict: false,
            timeout: Duration::from_secs(1),
            work_dir: PathBuf::from("work"),
            out_root: PathBuf::from("out"),
        })
    }

    #[test]
    fn one_id_with_two_different_specs_is_refused_and_names_the_first_scenario() {
        let (own_dir, own) = specs("shared", "the tree of the directory", "mine");
        let (bundled_dir, bundled) = specs("shared", "the bundled tree", "theirs");
        let cases = vec![
            fixture_case("shared"),
            scenario_case("first", "shared"),
            scenario_case("second", "shared"),
        ];

        let error = check(cases, own, bundled).expect_err("one id cannot name two trees");

        let message = error.to_string();
        let BenchError::FixtureIdCollision { id, scenario } = error else {
            panic!("expected a fixture id collision, got: {message}");
        };
        assert_eq!(id, "shared");
        assert_eq!(scenario, "first");
        assert!(
            message.contains("`shared`") && message.contains("`first`"),
            "the refusal names the id and the scenario: {message}"
        );
        for dir in [own_dir.path(), bundled_dir.path()] {
            assert!(
                !message.contains(&dir.display().to_string()),
                "the refusal names a directory of specs: {message}"
            );
        }
    }

    #[test]
    fn two_copies_of_one_spec_are_one_tree_whatever_their_description() {
        let (_own_dir, own) = specs("shared", "as the directory has it", "tree");
        let (_bundled_dir, bundled) = specs("shared", "as the bundled spec has it", "tree");
        let cases = vec![fixture_case("shared"), scenario_case("s", "shared")];

        check(cases, own, bundled).expect("one tree under one id");
    }

    #[test]
    fn the_bundled_specs_in_both_places_are_never_a_collision() {
        let cases = vec![fixture_case("wide-1k"), scenario_case("s", "wide-1k")];

        check(cases, Fixtures::bundled(), Fixtures::bundled()).expect("the same specs");
    }

    #[test]
    fn an_id_that_only_one_kind_of_case_runs_on_is_never_compared() {
        // `shared` is two different trees, but in each list of cases below only one kind of case
        // runs on it.
        let (_own_dir, own) = specs("shared", "d", "mine");
        let (_bundled_dir, bundled) = specs("shared", "d", "theirs");

        for cases in [
            vec![fixture_case("shared"), scenario_case("s", "other")],
            vec![fixture_case("other"), scenario_case("s", "shared")],
            vec![scenario_case("s", "shared")],
            vec![fixture_case("shared")],
            Vec::new(),
        ] {
            check(cases, own.clone(), bundled.clone()).expect("no id is named by both");
        }
    }

    #[test]
    fn a_spec_that_cannot_be_loaded_is_left_to_the_case_that_needs_it() {
        let (_broken_dir, broken) = specs_with("shared", "this is not a spec");
        let (_absent_dir, absent) = specs_with("another", "this is not a spec");
        let (_bundled_dir, bundled) = specs("shared", "d", "theirs");
        let cases = || vec![fixture_case("shared"), scenario_case("s", "shared")];

        check(cases(), broken.clone(), bundled.clone()).expect("the case reports it");
        check(cases(), absent, bundled.clone()).expect("the case reports it");
        check(cases(), bundled, broken).expect("the case reports it");
    }
}
