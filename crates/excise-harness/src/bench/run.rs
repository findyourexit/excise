//! Building the warm-up and measured pairs of every case, and assembling the `harness-ab`
//! document.

use std::{
    collections::BTreeMap,
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
    /// Where fixtures come from.
    pub fixtures: Fixtures,
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
/// [`BenchError::ConcurrentExcise`] when `--strict` was given and another `excise` process is
/// running, [`BenchError::Case`] when a case cannot be run or measured, and an I/O or rendering
/// error when the output cannot be written.
pub fn run_bench_e2e(options: &BenchOptions) -> Result<BenchReport, BenchError> {
    if options.cases.is_empty() {
        return Err(BenchError::NothingToCompare);
    }
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
                    &options.fixtures,
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
