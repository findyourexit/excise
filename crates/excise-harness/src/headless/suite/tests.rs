//! Tests of the ratio and peak-memory budgets' verdict resolution: pure functions, exercised
//! directly so that the gating threshold and the combination of the oracle diff with each budget
//! are each covered without a real scan or a real `du` (both are noisy and slow; see the harness
//! README).

use std::collections::BTreeSet;

use super::{
    Class, DuMeasure, Ended, FixtureReport, MIN_GATED_ENTRIES, RATIO_BUDGET, Round, ScanMeasure,
    Verdict, combine_verdicts, resolve_budget_check,
};
use crate::scenario::Profile;

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
