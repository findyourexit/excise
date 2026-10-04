//! Running one [`Comparison`]: paired, interleaved trials of its two sides.
//!
//! [`run_comparison`] drives `comparison.pairs` interleaved trials in [`crate::bench::pairing`]
//! order, running each side with the same primitives `cargo xtask bench-e2e` uses to run one case
//! ([`crate::bench::cases::run_scenario_once`] for an interactive profile,
//! [`crate::bench::cases::run_fixture_once`] for a headless scan) and summarizing the per-trial
//! ratio with [`crate::bench::bootstrap`]. It adds only the one rule those tools do not need: a
//! run that never reaches `COMPLETE` (or never finishes its scan) within [`Comparison::timeout_ms`]
//! is a failed ratio, not a harness error, because that non-completion is F2's own symptom.
//! [`run_compare`] runs every comparison a tier and platform select, mirroring
//! [`crate::runner::run_e2e`]'s selection.
//!
//! With [`CompareOptions::timing_informational`], a median ratio over its limit is reported as a
//! warning and does not fail the comparison, for a hosted machine that is slower than the one the
//! limits were set on. A run that never completes within the bound is a timeout, not a ratio, and
//! still fails it, and so does a comparison that is expected to fail on the platform: it keeps its
//! strict verdict.

use std::{
    collections::BTreeMap,
    fmt::Write as _,
    path::Path,
    time::{Duration, Instant},
};

use thiserror::Error;

use crate::{
    bench::{
        bootstrap::{bootstrap_ci, median_ratio},
        cases::{CaseError, SharedFixture, run_fixture_once, run_scenario_once},
        pairing::interleaving,
    },
    fixture::Fixtures,
    headless::pairs::millis,
    report::{self, ConfidenceInterval, Side, TimingWarning, Verdict},
    run_support::render_rows,
    runner::{self, Outcome},
    scenario::{self, Budget, EventKind, Expect, Profile, Scenario, Step, WaitEvent},
};

use super::Comparison;

/// What one side of one trial measured.
#[derive(Debug, Clone, Copy, PartialEq)]
struct SideRun {
    /// Milliseconds to `COMPLETE` (interactive) or to the end of the scan (headless), or the
    /// comparison's bound when the run never got there.
    ms: f64,
    /// Whether the run reached `COMPLETE`, or finished its scan, within the bound.
    completed: bool,
}

/// A comparison could not be computed at all: a genuine harness problem (fixture, spawn, or
/// isolation failure), not a failed ratio.
#[derive(Debug, Error)]
pub enum ComparisonError {
    /// A side's run could not be measured for a reason other than failing to complete in time.
    #[error(transparent)]
    Case(#[from] CaseError),
    /// A run reported as complete recorded none of the metric its side needs.
    #[error("the {0} run completed but recorded no `{1}` metric")]
    MissingMetric(&'static str, &'static str),
}

/// What running one [`Comparison`] produced.
#[derive(Debug)]
pub struct ComparisonReport {
    /// The comparison's name.
    pub name: String,
    /// The budget it checked.
    pub budget: Budget,
    /// How many interleaved pairs were asked for.
    pub pairs: u32,
    /// The baseline side's milliseconds, one per trial (capped at the bound for a trial that
    /// never completed).
    pub baseline: Vec<f64>,
    /// The candidate side's milliseconds, one per trial, index-aligned with `baseline`.
    pub candidate: Vec<f64>,
    /// How many baseline runs reached `COMPLETE` (or finished their scan) within the bound.
    pub baseline_completed: usize,
    /// How many candidate runs reached `COMPLETE` within the bound.
    pub candidate_completed: usize,
    /// Whether any recorded run, on either side, failed to reach `COMPLETE` (or finish its scan)
    /// within the bound.
    pub any_incomplete: bool,
    /// The median of the per-trial candidate/baseline ratios.
    pub median_ratio: f64,
    /// A bootstrap confidence interval for the median ratio.
    pub ci: ConfidenceInterval,
    /// The limit the ratio was checked against.
    pub limit: f64,
    /// What happened, without the strict-xfail interpretation. A median ratio over its limit that
    /// informational timing excused (see `timing_warnings`) counts as `Passed`.
    pub outcome: Outcome,
    /// The verdict, with strict xfail applied.
    pub verdict: Verdict,
    /// Wall time of the whole comparison.
    pub duration: Duration,
    /// Why the comparison could not be computed, when it could not.
    pub error: Option<String>,
    /// The ratio checks the comparison missed without failing for it, because the options held
    /// timing informational: its median ratio over its limit, at most once.
    pub timing_warnings: Vec<TimingWarning>,
}

/// What every call needs to run a comparison.
#[derive(Debug, Clone, Copy)]
pub struct CompareOptions<'a> {
    /// The `excise` binary under test.
    pub binary: &'a Path,
    /// Where fixture specs live and fixtures are cached.
    pub fixtures: &'a Fixtures,
    /// The existing directory fresh copies and scratch areas are created in.
    pub work_dir: &'a Path,
    /// Seeds the bootstrap confidence interval, so the same seed always gives the same interval.
    pub seed: u64,
    /// Whether a median ratio over its limit is reported as a warning and does not fail the
    /// comparison: for a hosted machine whose speed the limits were not set on. A comparison that
    /// is expected to fail on this platform keeps its strict verdict whatever this says.
    pub timing_informational: bool,
}

/// Runs `comparison`'s interleaved pairs and checks its ratio against its budget.
///
/// This function never panics on a comparison or harness problem: a comparison that could not be
/// computed has the verdict `error` and the reason in [`ComparisonReport::error`], mirroring
/// [`crate::runner::run_scenario`].
#[must_use]
pub fn run_comparison(comparison: &Comparison, options: &CompareOptions<'_>) -> ComparisonReport {
    let started = Instant::now();
    let limit = comparison.limit();
    let mut report = ComparisonReport {
        name: comparison.name.clone(),
        budget: comparison.budget,
        pairs: comparison.pairs,
        baseline: Vec::new(),
        candidate: Vec::new(),
        baseline_completed: 0,
        candidate_completed: 0,
        any_incomplete: false,
        median_ratio: 0.0,
        ci: ConfidenceInterval {
            lower: 0.0,
            upper: 0.0,
            confidence: 0.95,
        },
        limit,
        outcome: Outcome::Errored,
        verdict: Verdict::Error,
        duration: Duration::ZERO,
        error: None,
        timing_warnings: Vec::new(),
    };
    match execute(comparison, options) {
        Ok((baseline, candidate)) => {
            report.baseline_completed = baseline.iter().filter(|run| run.completed).count();
            report.candidate_completed = candidate.iter().filter(|run| run.completed).count();
            report.any_incomplete = any_run_incomplete(&baseline, &candidate);
            report.baseline = baseline.iter().map(|run| run.ms).collect();
            report.candidate = candidate.iter().map(|run| run.ms).collect();
            report.median_ratio = median_ratio(&report.baseline, &report.candidate);
            report.ci = bootstrap_ci(&report.baseline, &report.candidate, options.seed);
            judge(
                &mut report,
                comparison.expect_on(std::env::consts::OS),
                options.timing_informational,
            );
        }
        Err(error) => report.error = Some(error.to_string()),
    }
    report.duration = started.elapsed();
    report
}

/// Runs every pair of `comparison`, interleaved baseline/candidate, and returns the two sides'
/// runs index-aligned by trial.
fn execute(
    comparison: &Comparison,
    options: &CompareOptions<'_>,
) -> Result<(Vec<SideRun>, Vec<SideRun>), ComparisonError> {
    let bound = Duration::from_millis(comparison.timeout_ms);
    let probe = probe_scenario(comparison);
    let baseline_profile = comparison.baseline_profile();
    // A headless baseline reuses one shared, warm root for every pair, exactly as the headless
    // runner and `bench-e2e` do: the scan never writes to it.
    let headless_root = match baseline_profile {
        Some(_) => None,
        None => Some(SharedFixture::acquire(
            options.fixtures,
            &comparison.fixture,
            options.work_dir,
        )?),
    };
    run_pairs(
        comparison.pairs,
        || {
            if let Some(profile) = baseline_profile {
                run_interactive(&probe, profile, options, bound)
            } else {
                let root = headless_root
                    .as_ref()
                    .expect("acquired above whenever the baseline is headless");
                run_headless(root.root(), options, bound)
            }
        },
        || run_interactive(&probe, comparison.candidate_profile(), options, bound),
    )
}

/// Runs `pairs` interleaved trials, in [`interleaving`] order, calling `run_baseline` for every
/// baseline turn and `run_candidate` for every candidate turn. Returns the two sides' runs,
/// index-aligned by trial: `baseline[i]` and `candidate[i]` are the same trial.
///
/// Generic and side-effect-free in its own right (the closures do the real work), so the ordering
/// is directly testable without spawning a process.
fn run_pairs<T, E>(
    pairs: u32,
    mut run_baseline: impl FnMut() -> Result<T, E>,
    mut run_candidate: impl FnMut() -> Result<T, E>,
) -> Result<(Vec<T>, Vec<T>), E> {
    let order = interleaving(pairs);
    let mut baseline = Vec::with_capacity(order.len().div_ceil(2));
    let mut candidate = Vec::with_capacity(order.len() / 2);
    for side in &order {
        match side {
            Side::Baseline => baseline.push(run_baseline()?),
            Side::Candidate => candidate.push(run_candidate()?),
        }
    }
    Ok((baseline, candidate))
}

/// A minimal interactive scenario: launch, then wait for the `scan_complete` event within
/// `comparison`'s bound. The event channel, not the screen: a drain cap (`comparison.
/// drain_bytes_per_sec`) paces the reader, so a screen-based wait (`wait_header`) would lag the
/// program and could time out long after the event the comparison actually cares about already
/// fired. No `quit` step: whether the wait holds or times out, the runner kills the whole process
/// group once the step ends (see `crate::runner::run_scenario`), and a step that only waits never
/// changes the fixture, so nothing is lost by never asking the program to exit on its own.
fn probe_scenario(comparison: &Comparison) -> Scenario {
    Scenario {
        schema_version: scenario::SCHEMA_VERSION,
        name: comparison.name.clone(),
        description: comparison.description.clone(),
        fixture: comparison.fixture.clone(),
        sentinels: Vec::new(),
        scan_store_on_volume: false,
        cgroup_memory_cap: false,
        disable_delete_confirmation: false,
        profiles: vec![comparison.candidate_profile()],
        tier: scenario::Tier::default(),
        platforms: None,
        terminal: scenario::Terminal {
            drain_bytes_per_sec: comparison.drain_bytes_per_sec,
            ..scenario::Terminal::default()
        },
        expect: Expect::default(),
        fails_on: None,
        slice: None,
        budgets: BTreeMap::new(),
        steps: vec![Step::WaitEvent(WaitEvent {
            event: EventKind::ScanComplete,
            fields: BTreeMap::new(),
            timeout_ms: comparison.timeout_ms,
        })],
    }
}

/// Runs `probe` under `profile`, interactively, and returns its time-to-`COMPLETE`.
///
/// A step failure (the wait timed out, or the program exited before `COMPLETE`) is the F2 symptom
/// this comparison exists to catch, not a harness error: it is reported as an incomplete run,
/// capped at the bound. Any other error (the fixture could not be built, the root failed the
/// ownership check, the process could not be spawned) propagates, because nothing was learned.
fn run_interactive(
    probe: &Scenario,
    profile: Profile,
    options: &CompareOptions<'_>,
    bound: Duration,
) -> Result<SideRun, ComparisonError> {
    match run_scenario_once(
        probe,
        profile,
        options.binary,
        options.fixtures,
        options.work_dir,
    ) {
        Ok((metrics, _identity)) => {
            let ms =
                metrics
                    .get("scan_complete_ms")
                    .copied()
                    .ok_or(ComparisonError::MissingMetric(
                        "interactive",
                        "scan_complete_ms",
                    ))?;
            Ok(SideRun {
                ms,
                completed: true,
            })
        }
        Err(CaseError::ScenarioFailed { .. }) => Ok(SideRun {
            ms: millis(bound),
            completed: false,
        }),
        Err(other) => Err(other.into()),
    }
}

/// Runs a headless scan of `shared_root` and returns its wall time.
///
/// A scan that ran past its deadline is an incomplete run, capped at the bound, for the same
/// reason as [`run_interactive`]'s step failure: the comparison's rule is about reaching the end
/// within the bound, on either side, not about why a side did not.
fn run_headless(
    shared_root: &Path,
    options: &CompareOptions<'_>,
    bound: Duration,
) -> Result<SideRun, ComparisonError> {
    match run_fixture_once(options.binary, shared_root, options.work_dir, bound) {
        Ok(metrics) => {
            let ms = metrics
                .get("wall_time_ms")
                .copied()
                .ok_or(ComparisonError::MissingMetric("headless", "wall_time_ms"))?;
            Ok(SideRun {
                ms,
                completed: true,
            })
        }
        Err(CaseError::ScanTimedOut { .. }) => Ok(SideRun {
            ms: millis(bound),
            completed: false,
        }),
        Err(other) => Err(other.into()),
    }
}

/// Whether any recorded run, on either side, failed to reach `COMPLETE` (or finish its scan)
/// within the comparison's bound. On its own, this is a failed ratio: F2's symptom under default
/// motion is exactly a run that never gets there, and a comparison must not wait out a hang to
/// judge it, nor read non-completion as a tolerable, merely-large ratio.
fn any_run_incomplete(baseline: &[SideRun], candidate: &[SideRun]) -> bool {
    baseline.iter().chain(candidate).any(|run| !run.completed)
}

/// The outcome of the budget check: a failed ratio whenever a run never completed (regardless of
/// the computed number, which the capped value makes large but which is not itself the reason),
/// else the ordinary upper-bound check every budget uses: a metric passes when it is at most the
/// limit.
fn classify(any_incomplete: bool, median_ratio: f64, limit: f64) -> Outcome {
    if any_incomplete || median_ratio > limit {
        Outcome::Failed
    } else {
        Outcome::Passed
    }
}

/// Judges a finished comparison: the outcome of its budget check, and the verdict that follows
/// from it and from `expect`.
///
/// With `timing_informational`, a median ratio over its limit is not a failure: it is recorded in
/// `report.timing_warnings` and the comparison counts as passed. A run that never completed within
/// the bound stays a failure, because that is a timeout and not a ratio. A comparison that is
/// expected to fail keeps its strict verdict (`xfail`, or `xpass` when its ratio is within the
/// limit): it documents a defect measured against the strict limit, and excusing the miss would
/// turn it into an `xpass`, the signal that the defect is fixed.
fn judge(report: &mut ComparisonReport, expect: Expect, timing_informational: bool) {
    report.outcome = classify(report.any_incomplete, report.median_ratio, report.limit);
    if timing_informational
        && expect == Expect::Pass
        && report.outcome == Outcome::Failed
        && !report.any_incomplete
    {
        report.timing_warnings.push(TimingWarning {
            budget: report.budget,
            metric: report.budget.to_string(),
            value: report.median_ratio,
            limit: report.limit,
        });
        report.outcome = Outcome::Passed;
    }
    report.verdict = runner::verdict(expect, report.outcome);
}

/// A comparison this run did not attempt, and why.
#[derive(Debug, Clone)]
pub struct Skipped {
    /// The comparison's name.
    pub name: String,
    /// Why it was not run.
    pub reason: String,
}

/// What to run and under which tier.
#[derive(Debug, Clone, Copy)]
pub struct CompareRunOptions<'a> {
    /// What every comparison run needs.
    pub compare: CompareOptions<'a>,
    /// The tier: selects comparisons whose own `tier` is no higher.
    pub tier: report::Tier,
    /// Whether the comparisons given to [`run_compare`] were narrowed by name: when true, each one
    /// runs whatever its tier. Platform selection always applies.
    pub named: bool,
    /// Overrides every comparison's own `pairs`, when set.
    pub pairs: Option<u32>,
}

/// The outcome of running a set of comparisons.
#[derive(Debug)]
pub struct CompareReport {
    /// One record per comparison that ran, in selection order.
    pub records: Vec<ComparisonReport>,
    /// Comparisons this run did not attempt, with the reason.
    pub skipped: Vec<Skipped>,
    /// Whether the run held timing informational (see [`CompareOptions::timing_informational`]),
    /// which its table says.
    pub timing_informational: bool,
}

impl CompareReport {
    /// Whether no record has a blocking verdict: a `fail`, `xpass`, or `error` fails the run.
    #[must_use]
    pub fn is_success(&self) -> bool {
        !self
            .records
            .iter()
            .any(|record| record.verdict.blocks_run())
    }

    /// The verdict table: one row per comparison, the cause of any `error` verdict, the skipped
    /// comparisons, one line per timing warning, and the overall verdict.
    #[must_use]
    pub fn table(&self) -> String {
        let mut table = render_rows(&self.rows());
        for record in &self.records {
            if let Some(reason) = &record.error {
                let _ = writeln!(table, "\nERROR {}: {reason}", record.name);
            }
        }
        for skipped in &self.skipped {
            let _ = writeln!(table, "\nSKIP {}: {}", skipped.name, skipped.reason);
        }
        for record in &self.records {
            for warning in &record.timing_warnings {
                let _ = writeln!(table, "\nWARN {}: {warning}", record.name);
            }
        }
        let blocking = self
            .records
            .iter()
            .filter(|record| record.verdict.blocks_run())
            .count();
        let timing = if self.timing_informational {
            let warnings: usize = self
                .records
                .iter()
                .map(|record| record.timing_warnings.len())
                .sum();
            format!("; timing informational: {warnings} warning(s)")
        } else {
            String::new()
        };
        let _ = writeln!(
            table,
            "\ncompare {}: {} comparison(s), {blocking} blocking{timing}",
            if blocking == 0 { "ok" } else { "FAILED" },
            self.records.len(),
        );
        table
    }

    fn rows(&self) -> Vec<[String; 8]> {
        let mut rows = vec![
            [
                "comparison",
                "budget",
                "verdict",
                "pairs",
                "completed (base/cand)",
                "median ratio",
                "95% CI",
                "limit",
            ]
            .map(str::to_owned),
        ];
        for record in &self.records {
            rows.push([
                record.name.clone(),
                record.budget.to_string(),
                record.verdict.to_string(),
                record.pairs.to_string(),
                format!(
                    "{}/{}, {}/{}",
                    record.baseline_completed,
                    record.baseline.len(),
                    record.candidate_completed,
                    record.candidate.len(),
                ),
                format!("{:.3}", record.median_ratio),
                format!("[{:.3}, {:.3}]", record.ci.lower, record.ci.upper),
                format!("{:.3}", record.limit),
            ]);
        }
        rows
    }
}

/// Whether a comparison tagged `comparison_tier` runs when the matrix is asked for `tier`. Mirrors
/// `crate::runner::e2e`'s own `tier_includes`.
const fn tier_includes(tier: report::Tier, comparison_tier: scenario::Tier) -> bool {
    match comparison_tier {
        scenario::Tier::Quick => true,
        scenario::Tier::Full => !matches!(tier, report::Tier::Quick),
        scenario::Tier::Nightly => matches!(tier, report::Tier::Nightly | report::Tier::Weekly),
    }
}

/// Runs every comparison in `comparisons` that `options` selects: in `platforms` always, and
/// within `options.tier` unless `options.named`.
#[must_use]
pub fn run_compare(options: &CompareRunOptions<'_>, comparisons: &[Comparison]) -> CompareReport {
    let os = std::env::consts::OS;
    let mut records = Vec::new();
    let mut skipped = Vec::new();
    for comparison in comparisons {
        if !comparison.runs_on(os) {
            skipped.push(Skipped {
                name: comparison.name.clone(),
                reason: format!(
                    "`{os}` is not among its platforms ({})",
                    comparison.effective_platforms().join(", ")
                ),
            });
            continue;
        }
        if !options.named && !tier_includes(options.tier, comparison.tier) {
            skipped.push(Skipped {
                name: comparison.name.clone(),
                reason: format!(
                    "tier `{}` is above the selected `{}`",
                    comparison.tier, options.tier
                ),
            });
            continue;
        }
        let comparison = match options.pairs {
            Some(pairs) => Comparison {
                pairs,
                ..comparison.clone()
            },
            None => comparison.clone(),
        };
        records.push(run_comparison(&comparison, &options.compare));
    }
    CompareReport {
        records,
        skipped,
        timing_informational: options.compare.timing_informational,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairs_run_in_baseline_candidate_order_and_stay_index_aligned() {
        use std::cell::RefCell;

        let log: RefCell<Vec<&'static str>> = RefCell::new(Vec::new());
        let mut next_baseline = 0u32;
        let mut next_candidate = 0u32;
        let (baseline, candidate) = run_pairs::<u32, ()>(
            3,
            || {
                log.borrow_mut().push("baseline");
                next_baseline += 1;
                Ok(next_baseline)
            },
            || {
                log.borrow_mut().push("candidate");
                next_candidate += 1;
                Ok(next_candidate)
            },
        )
        .expect("infallible closures");

        assert_eq!(
            *log.borrow(),
            [
                "baseline",
                "candidate",
                "baseline",
                "candidate",
                "baseline",
                "candidate"
            ]
        );
        assert_eq!(baseline, [1, 2, 3]);
        assert_eq!(candidate, [1, 2, 3]);
    }

    #[test]
    fn zero_pairs_runs_neither_side() {
        let (baseline, candidate) = run_pairs::<u32, ()>(0, || Ok(1), || Ok(1)).expect("ok");
        assert!(baseline.is_empty());
        assert!(candidate.is_empty());
    }

    #[test]
    fn a_run_baseline_error_stops_before_any_candidate_call() {
        let candidate_calls = std::cell::Cell::new(0u32);
        let result = run_pairs::<u32, &'static str>(
            2,
            || Err("baseline cannot run"),
            || {
                candidate_calls.set(candidate_calls.get() + 1);
                Ok(1)
            },
        );
        assert_eq!(result, Err("baseline cannot run"));
        assert_eq!(candidate_calls.get(), 0);
    }

    #[test]
    fn an_incomplete_run_fails_the_ratio_even_when_the_number_would_pass() {
        let baseline = [SideRun {
            ms: 1000.0,
            completed: true,
        }];
        let candidate = [SideRun {
            ms: 1000.0,
            completed: false,
        }];
        assert!(any_run_incomplete(&baseline, &candidate));

        let ratio = median_ratio(&[1000.0], &[1000.0]);
        assert!(
            (ratio - 1.0).abs() < f64::EPSILON,
            "the capped values are numerically equal, got {ratio}"
        );
        assert_eq!(
            classify(any_run_incomplete(&baseline, &candidate), ratio, 1.25),
            Outcome::Failed,
            "a run that never completed must fail the ratio even though the number alone would pass"
        );
    }

    #[test]
    fn a_complete_baseline_and_candidate_never_trip_the_timeout_rule() {
        let baseline = [SideRun {
            ms: 1000.0,
            completed: true,
        }];
        let candidate = [SideRun {
            ms: 1100.0,
            completed: true,
        }];
        assert!(!any_run_incomplete(&baseline, &candidate));
    }

    #[test]
    fn the_budget_check_passes_exactly_at_the_limit_and_fails_just_past_it() {
        assert_eq!(classify(false, 1.25, 1.25), Outcome::Passed);
        assert_eq!(classify(false, 1.0, 1.25), Outcome::Passed);
        assert_eq!(classify(false, 1.250_000_1, 1.25), Outcome::Failed);
    }

    #[test]
    fn the_probe_scenario_is_valid_and_waits_on_the_scan_complete_event() {
        let comparison = Comparison::from_toml_str(
            "schema_version = 1\nname = \"motion-complete-probe\"\ndescription = \"d\"\n\
             fixture = \"node-modules-2k\"\nbudget = \"motion_complete_ratio\"\n\
             timeout_ms = 60000\n",
        )
        .expect("a comparison");
        comparison.validate().expect("valid");

        let probe = probe_scenario(&comparison);
        probe
            .validate()
            .expect("the probe scenario is itself valid");
        assert_eq!(probe.steps.len(), 1);
        assert_eq!(
            probe.steps[0],
            Step::WaitEvent(WaitEvent {
                event: EventKind::ScanComplete,
                fields: std::collections::BTreeMap::new(),
                timeout_ms: 60000,
            })
        );
        assert_eq!(probe.profiles, [Profile::Default]);
    }

    /// A finished comparison of default against reduced motion, one pair, measured at
    /// `median_ratio` against the limit of 1.25. Nothing has judged it yet.
    fn finished(median_ratio: f64, any_incomplete: bool) -> ComparisonReport {
        ComparisonReport {
            name: "motion-complete".to_owned(),
            budget: Budget::MotionCompleteRatio,
            pairs: 1,
            baseline: vec![1000.0],
            candidate: vec![1000.0 * median_ratio],
            baseline_completed: 1,
            candidate_completed: usize::from(!any_incomplete),
            any_incomplete,
            median_ratio,
            ci: ConfidenceInterval {
                lower: median_ratio,
                upper: median_ratio,
                confidence: 0.95,
            },
            limit: 1.25,
            outcome: Outcome::Errored,
            verdict: Verdict::Error,
            duration: Duration::ZERO,
            error: None,
            timing_warnings: Vec::new(),
        }
    }

    /// What `judge` makes of a comparison measured at `median_ratio`: its verdict and the warnings
    /// it records.
    fn judged(
        median_ratio: f64,
        any_incomplete: bool,
        expect: Expect,
        timing_informational: bool,
    ) -> (Verdict, Vec<TimingWarning>) {
        let mut report = finished(median_ratio, any_incomplete);
        judge(&mut report, expect, timing_informational);
        (report.verdict, report.timing_warnings)
    }

    #[test]
    fn a_ratio_over_its_limit_fails_a_strict_comparison_and_warns_in_an_informational_one() {
        assert_eq!(
            judged(2.19, false, Expect::Pass, false),
            (Verdict::Fail, Vec::new())
        );

        let (verdict, warnings) = judged(2.19, false, Expect::Pass, true);
        assert_eq!(verdict, Verdict::Pass);
        assert_eq!(
            warnings,
            [TimingWarning {
                budget: Budget::MotionCompleteRatio,
                metric: "motion_complete_ratio".to_owned(),
                value: 2.19,
                limit: 1.25,
            }]
        );
    }

    #[test]
    fn informational_timing_records_nothing_for_a_ratio_within_its_limit() {
        assert_eq!(
            judged(1.25, false, Expect::Pass, true),
            (Verdict::Pass, Vec::new())
        );
    }

    #[test]
    fn informational_timing_does_not_excuse_a_run_that_never_completed() {
        // A run that hit the bound is a timeout, whatever the capped numbers say.
        for ratio in [0.5, 2.19] {
            assert_eq!(
                judged(ratio, true, Expect::Pass, true),
                (Verdict::Fail, Vec::new()),
                "median ratio {ratio}"
            );
        }
    }

    #[test]
    fn informational_timing_keeps_an_expected_failure_strict() {
        assert_eq!(
            judged(2.19, false, Expect::Fail, true),
            (Verdict::Xfail, Vec::new()),
            "the documented defect still fails, and nothing is excused"
        );
        let (verdict, warnings) = judged(1.0, false, Expect::Fail, true);
        assert_eq!(verdict, Verdict::Xpass, "a fixed defect is still reported");
        assert!(verdict.blocks_run() && warnings.is_empty());
    }
}
