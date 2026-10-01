//! Tests of the ratio budget's verdict resolution: pure functions, exercised directly so that the
//! gating threshold and the combination of the oracle diff with the ratio budget are each covered
//! without a real scan or a real `du` (both are noisy and slow; see the harness README).

use std::collections::BTreeSet;

use super::{
    Class, DuMeasure, Ended, FixtureReport, MIN_GATED_ENTRIES, RATIO_BUDGET, RatioExpectedFailure,
    Round, ScanMeasure, Verdict, combine_verdicts, resolve_ratio,
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

fn expectation(reason: &str) -> RatioExpectedFailure {
    RatioExpectedFailure {
        fixture: "fixture".to_owned(),
        platforms: vec![std::env::consts::OS.to_owned()],
        findings: vec!["F1".to_owned()],
        reason: reason.to_owned(),
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
// `resolve_ratio`: the four combinations of (expectation, over_budget).

#[test]
fn an_expected_failure_over_budget_is_xfail() {
    let expected = expectation("reason");
    assert_eq!(resolve_ratio(Some(&expected), true), Verdict::Xfail);
}

#[test]
fn an_expected_failure_within_budget_is_xpass() {
    let expected = expectation("reason");
    assert_eq!(resolve_ratio(Some(&expected), false), Verdict::Xpass);
}

#[test]
fn an_unexpected_over_budget_ratio_is_fail_like_an_undocumented_diff() {
    assert_eq!(resolve_ratio(None, true), Verdict::Fail);
}

#[test]
fn an_unexpected_within_budget_ratio_is_pass() {
    assert_eq!(resolve_ratio(None, false), Verdict::Pass);
}

// -------------------------------------------------------------------------------------------
// `combine_verdicts`: the more severe of the oracle diff and the ratio budget.

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
