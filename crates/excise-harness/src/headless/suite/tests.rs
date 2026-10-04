//! Tests of the ratio and peak-memory budgets' verdict resolution: pure functions, exercised
//! directly so that the gating threshold and the combination of the oracle diff with each budget
//! are each covered without a real scan or a real `du` (both are noisy and slow; see the harness
//! README).

use std::collections::BTreeSet;

use super::{
    Class, DuMeasure, Ended, FixtureReport, MIN_GATED_ENTRIES, RATIO_BUDGET, Round, ScanMeasure,
    Verdict, combine_verdicts, judge_ratio, resolve_budget_check,
};
use crate::{
    headless::expectations::RatioExpectedFailure,
    scenario::{Budget, Profile},
};

fn millis(value: f64) -> std::time::Duration {
    std::time::Duration::from_secs_f64(value / 1000.0)
}

/// A measured round pairing a scan of `scan_ms` with a `du` of `du_ms`.
fn round(scan_ms: f64, du_ms: f64) -> Round {
    Round {
        warm_up: false,
        scan: ScanMeasure {
            wall: millis(scan_ms),
            cpu: None,
            peak_memory_bytes: None,
            cgroup_memory_peak_bytes: None,
            ended: Ended::Exited(0),
            timed_out: false,
            report_bytes: None,
        },
        du: Some(DuMeasure {
            wall: millis(du_ms),
            kib: None,
            ended: Ended::Exited(0),
        }),
        diffed: false,
    }
}

/// A fixture report with `entries` oracle entries and no rounds yet.
fn with_entries(entries: u64) -> FixtureReport {
    FixtureReport::new(
        "fixture",
        &BTreeSet::<Class>::new(),
        Profile::Default,
        entries,
    )
}

// -------------------------------------------------------------------------------------------
// `FixtureReport::ratio_is_gated`: a deterministic entry count, fixed by the fixture's spec and
// seed, never a measured time, so a fixture's gating decision never depends on machine load.

#[test]
fn an_entry_count_at_the_threshold_is_gated_and_below_it_is_not() {
    assert!(
        with_entries(MIN_GATED_ENTRIES).ratio_is_gated(),
        "an oracle entry count at the threshold must be gated"
    );
    assert!(
        !with_entries(MIN_GATED_ENTRIES - 1).ratio_is_gated(),
        "an oracle entry count just below the threshold must not be gated"
    );
}

#[test]
fn ratio_over_budget_reads_the_spread_median_against_the_budget() {
    let mut over = with_entries(1);
    over.rounds.push(round(RATIO_BUDGET * 100.0 + 1.0, 100.0));
    assert_eq!(over.ratio_over_budget(), Some(true));

    let mut under = with_entries(1);
    under.rounds.push(round(RATIO_BUDGET * 100.0 - 1.0, 100.0));
    assert_eq!(under.ratio_over_budget(), Some(false));

    let never_measured = with_entries(1);
    assert_eq!(never_measured.ratio_over_budget(), None);
}

/// A gated fixture with one measured pair whose scan took `ratio` times as long as its `du`, with
/// an `[[expect_ratio_fail]]` entry for this platform when `documented`.
fn gated(ratio: f64, documented: bool) -> FixtureReport {
    let mut report = with_entries(MIN_GATED_ENTRIES);
    report.rounds.push(round(ratio * 100.0, 100.0));
    if documented {
        report.ratio_expectation = Some(RatioExpectedFailure {
            fixture: "fixture".to_owned(),
            platforms: vec![std::env::consts::OS.to_owned()],
            findings: Vec::new(),
            reason: "a documented defect".to_owned(),
        });
    }
    report
}

// -------------------------------------------------------------------------------------------
// `judge_ratio`: what the ratio budget makes of a gated fixture, strictly and when timing is
// informational.

#[test]
fn a_ratio_over_the_budget_fails_a_strict_run_and_is_a_warning_in_an_informational_one() {
    let mut strict = gated(RATIO_BUDGET + 5.0, false);
    assert_eq!(judge_ratio(&mut strict, false), Verdict::Fail);
    assert!(strict.timing_warnings.is_empty());

    let mut informational = gated(RATIO_BUDGET + 5.0, false);
    assert_eq!(judge_ratio(&mut informational, true), Verdict::Pass);
    let [warning] = informational.timing_warnings.as_slice() else {
        panic!("one warning: {:?}", informational.timing_warnings);
    };
    assert_eq!(warning.budget, Budget::HeadlessScanRatio);
    assert_eq!(warning.metric, "headless_scan_ratio");
    assert!(
        (warning.value - (RATIO_BUDGET + 5.0)).abs() < 1e-6,
        "{warning:?}"
    );
    assert!(
        (warning.limit - RATIO_BUDGET).abs() < f64::EPSILON,
        "{warning:?}"
    );
}

#[test]
fn informational_timing_records_nothing_for_a_ratio_within_the_budget() {
    let mut report = gated(RATIO_BUDGET - 1.0, false);

    assert_eq!(judge_ratio(&mut report, true), Verdict::Pass);
    assert!(report.timing_warnings.is_empty());
}

#[test]
fn informational_timing_keeps_a_documented_ratio_failure_strict() {
    let mut over = gated(RATIO_BUDGET + 5.0, true);
    assert_eq!(judge_ratio(&mut over, true), Verdict::Xfail);
    assert!(
        over.timing_warnings.is_empty(),
        "{:?}",
        over.timing_warnings
    );

    // Within the budget, the documented defect is fixed, and the run still has to say so.
    let mut within = gated(RATIO_BUDGET - 1.0, true);
    let verdict = judge_ratio(&mut within, true);
    assert_eq!(verdict, Verdict::Xpass);
    assert!(verdict.blocks_run());
}

/// `gated(ratio, false)` with the oracle's `du` total `expected_kib` and its one `du` changed by
/// `change`.
fn gated_with_du(
    ratio: f64,
    expected_kib: Option<u64>,
    change: impl FnOnce(&mut DuMeasure),
) -> FixtureReport {
    let mut report = gated(ratio, false);
    report.du_expected_kib = expected_kib;
    change(report.rounds[0].du.as_mut().expect("the round has a du"));
    report
}

#[test]
fn informational_timing_excuses_a_ratio_only_against_a_valid_du() {
    // A `du` that failed, was killed, or walked only part of the tree times something other than
    // the scan's work: the ratio keeps its strict verdict, and nothing is recorded as a warning.
    let over = RATIO_BUDGET + 5.0;
    let invalid = [
        (
            "a du that exited with code 1",
            gated_with_du(over, None, |du| du.ended = Ended::Exited(1)),
        ),
        (
            "a du killed by a signal",
            gated_with_du(over, None, |du| du.ended = Ended::Signaled(9)),
        ),
        (
            "a du whose total the oracle contradicts",
            gated_with_du(over, Some(100), |du| du.kib = Some(60)),
        ),
    ];
    for (what, mut report) in invalid {
        assert_eq!(judge_ratio(&mut report, true), Verdict::Fail, "{what}");
        assert!(
            report.timing_warnings.is_empty(),
            "{what}: {:?}",
            report.timing_warnings
        );
    }

    // The same miss against a `du` that exited 0 and printed the oracle's total is a warning.
    let mut valid = gated_with_du(over, Some(100), |du| du.kib = Some(100));
    assert_eq!(judge_ratio(&mut valid, true), Verdict::Pass);
    assert_eq!(
        valid.timing_warnings.len(),
        1,
        "{:?}",
        valid.timing_warnings
    );
}

// -------------------------------------------------------------------------------------------
// `resolve_budget_check`: the four combinations of (expected, over_budget), shared by the ratio
// budget and the memory budget (both reduce to one over/under-budget bit).

#[test]
fn an_expected_failure_over_budget_is_xfail() {
    assert_eq!(resolve_budget_check(true, true), Verdict::Xfail);
}

#[test]
fn an_expected_failure_within_budget_is_xpass() {
    assert_eq!(resolve_budget_check(true, false), Verdict::Xpass);
}

#[test]
fn an_unexpected_over_budget_is_fail_like_an_undocumented_diff() {
    assert_eq!(resolve_budget_check(false, true), Verdict::Fail);
}

#[test]
fn an_unexpected_within_budget_is_pass() {
    assert_eq!(resolve_budget_check(false, false), Verdict::Pass);
}

// -------------------------------------------------------------------------------------------
// `combine_verdicts`: the more severe of the oracle diff and a budget check.

#[test]
fn combining_keeps_the_more_severe_verdict_either_way_round() {
    let cases = [
        (Verdict::Pass, Verdict::Pass, Verdict::Pass),
        (Verdict::Pass, Verdict::Xfail, Verdict::Xfail),
        (Verdict::Xfail, Verdict::Pass, Verdict::Xfail),
        (Verdict::Pass, Verdict::Fail, Verdict::Fail),
        (Verdict::Xfail, Verdict::Fail, Verdict::Fail),
        (Verdict::Fail, Verdict::Xfail, Verdict::Fail),
        (Verdict::Xfail, Verdict::Xpass, Verdict::Xpass),
        (Verdict::Xpass, Verdict::Xfail, Verdict::Xpass),
        (Verdict::Fail, Verdict::Error, Verdict::Error),
        (Verdict::Pass, Verdict::Error, Verdict::Error),
    ];
    for (first, second, expected) in cases {
        assert_eq!(
            combine_verdicts(first, second),
            expected,
            "combining {first} with {second}"
        );
    }
}
