//! The time budget of the quick tier.
//!
//! Agents and developers run `cargo xtask e2e --quick` on every iteration, and the tier grows one
//! scenario at a time, so a run of the whole tier times itself: from just before the first launch
//! of the binary under test to the end of its last run. The verdict table's last line prints that
//! time against the budget, and the summary records it (`quick_tier_ms`).
//!
//! A tier over its budget fails the run on the reference machine, the one that sets
//! [`REFERENCE_ENV`] to `1` and whose speed the budget was set on. Everywhere else it is a warning
//! in the verdict table and the run still passes: a slower machine is not a defect of the tier.
//! Either way the message names the five slowest runs, so that the change that made the tier slow
//! knows what to trim. A run that already failed for another reason keeps that failure, and still
//! reports its time.
//!
//! The budget is a constant beside the other tier constants (`QUICK_BUDGET` in `e2e`). Nothing here
//! reads it: every function takes the budget as a parameter, so that a test can hold a run to any
//! budget without waiting for it.

use std::{cmp::Reverse, time::Duration};

use crate::run_support::format_ms;

use super::e2e::RunRecord;

/// Set to exactly `1` on the reference machine: the one whose speed the time budget of the quick
/// tier was set on. A quick tier over its budget fails the run there. Unset, or any other value,
/// is any other machine, where the same run only warns.
pub(super) const REFERENCE_ENV: &str = "EXCISE_HARNESS_REFERENCE";

/// How many of the slowest runs the message about a tier over its budget names.
const SLOWEST_RUNS: usize = 5;

/// Whether this process runs on the reference machine: [`REFERENCE_ENV`] is exactly `1`.
pub(super) fn is_reference_machine() -> bool {
    reference_from_value(std::env::var(REFERENCE_ENV).ok().as_deref())
}

/// The pure parser behind [`is_reference_machine`]. Separated out because tests cannot call
/// `std::env::set_var` (unsafe in edition 2024) and so cannot exercise the environment itself.
fn reference_from_value(value: Option<&str>) -> bool {
    value == Some("1")
}

/// What the whole quick tier may take, and whether a run that takes longer fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuickBudget {
    /// The most the whole tier may take.
    pub limit: Duration,
    /// Whether the budget is enforced: a tier over `limit` fails the run, as it does on the
    /// reference machine. Everywhere else it only warns.
    pub enforced: bool,
}

/// How long the whole quick tier took, held to its budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuickTierTime {
    /// From just before the first launch to the end of the last run.
    pub elapsed: Duration,
    /// The budget it is held to.
    pub budget: QuickBudget,
}

impl QuickTierTime {
    /// Whether the tier took longer than its budget. Exactly the budget is within it.
    #[must_use]
    pub fn is_over(&self) -> bool {
        self.elapsed > self.budget.limit
    }

    /// Whether the run fails for its time: the tier is over its budget, on a machine where that
    /// fails the run.
    #[must_use]
    pub fn fails_run(&self) -> bool {
        self.is_over() && self.budget.enforced
    }

    /// `elapsed` in whole milliseconds, as the summary records it.
    #[must_use]
    pub fn millis(&self) -> u64 {
        u64::try_from(self.elapsed.as_millis()).unwrap_or(u64::MAX)
    }

    /// The time against the budget, for the last line of the verdict table: `quick tier: 22.6 s of
    /// 120 s`, or `quick tier: 131.2 s, over its 120 s budget`.
    pub(super) fn against_budget(&self) -> String {
        let (elapsed, limit) = (self.elapsed_text(), seconds(self.budget.limit, false));
        if self.is_over() {
            format!("quick tier: {elapsed} s, over its {limit} s budget")
        } else {
            format!("quick tier: {elapsed} s of {limit} s")
        }
    }

    /// What went wrong when the tier took longer than its budget, naming the slowest of `records`
    /// (the runs of the tier); `None` when it did not.
    pub(super) fn overrun(&self, records: &[RunRecord]) -> Option<String> {
        if !self.is_over() {
            return None;
        }
        let mut message = format!(
            "the quick tier took {} s, over its {} s budget",
            self.elapsed_text(),
            seconds(self.budget.limit, false)
        );
        let slowest = slowest_runs(records);
        if !slowest.is_empty() {
            let runs: Vec<String> = slowest.into_iter().map(describe_run).collect();
            message.push_str("; slowest runs: ");
            message.push_str(&runs.join(", "));
        }
        Some(message)
    }

    /// The elapsed time in seconds, rounded up when it is over the budget, so that the message
    /// never says a tier took "120 s" against a budget of "120 s" and was over it.
    fn elapsed_text(&self) -> String {
        seconds(self.elapsed, self.is_over())
    }
}

/// The runs that took the longest, longest first, at most [`SLOWEST_RUNS`] of them. Runs that took
/// the same time keep the order they ran in.
fn slowest_runs(records: &[RunRecord]) -> Vec<&RunRecord> {
    let mut slowest: Vec<&RunRecord> = records.iter().collect();
    slowest.sort_by_key(|record| Reverse(record.report.duration));
    slowest.truncate(SLOWEST_RUNS);
    slowest
}

/// One run as the message names it, with the time the verdict table shows for it:
/// `delete-folder-lifecycle [default] (2.44 s)`. The tier is one pass over the matrix, so there is
/// no repetition to say.
fn describe_run(record: &RunRecord) -> String {
    let report = &record.report;
    format!(
        "{} [{}] ({})",
        report.scenario,
        report.profile,
        format_ms(Some(report.duration.as_secs_f64() * 1000.0))
    )
}

/// `duration` in seconds to a tenth, without the tenth when it is a whole number of seconds:
/// `22.6`, `120`. It is rounded to the nearest tenth, or up when `round_up`.
fn seconds(duration: Duration, round_up: bool) -> String {
    const TENTH: u128 = 100_000_000;
    let nanos = duration.as_nanos();
    let tenths = if round_up {
        nanos.div_ceil(TENTH)
    } else {
        (nanos + TENTH / 2) / TENTH
    };
    if tenths.is_multiple_of(10) {
        (tenths / 10).to_string()
    } else {
        format!("{}.{}", tenths / 10, tenths % 10)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{super::run::RunReport, *};
    use crate::{report::Verdict, scenario::Profile};

    /// A run of `scenario` under `profile` that passed after `millis` milliseconds.
    fn run(scenario: &str, profile: Profile, millis: u64) -> RunRecord {
        RunRecord {
            repetition: 1,
            report: RunReport {
                scenario: scenario.to_owned(),
                profile,
                verdict: Verdict::Pass,
                duration: Duration::from_millis(millis),
                metrics: BTreeMap::new(),
                failure: None,
                error: None,
                bundle: None,
                kept_scratch: None,
                fixture_digest: String::new(),
                timing_warnings: Vec::new(),
            },
            kept_workspace: None,
        }
    }

    fn time(elapsed: Duration, limit: Duration, enforced: bool) -> QuickTierTime {
        QuickTierTime {
            elapsed,
            budget: QuickBudget { limit, enforced },
        }
    }

    #[test]
    fn only_the_exact_value_one_names_the_reference_machine() {
        assert!(reference_from_value(Some("1")));
        for value in [
            None,
            Some(""),
            Some("0"),
            Some("11"),
            Some("true"),
            Some("yes"),
            Some(" 1"),
            Some("1 "),
        ] {
            assert!(!reference_from_value(value), "{value:?}");
        }
    }

    #[test]
    fn the_message_names_the_budget_and_the_five_slowest_runs_longest_first() {
        let runs = [
            run("a", Profile::Default, 900),
            run("b", Profile::Default, 4_200),
            run("c", Profile::Default, 150),
            run("d", Profile::Deterministic, 3_100),
            run("e", Profile::Default, 4_200),
            run("f", Profile::Deterministic, 60),
            run("g", Profile::Deterministic, 2_500),
        ];
        let over = time(
            Duration::from_millis(131_204),
            Duration::from_secs(120),
            true,
        );

        assert_eq!(
            over.overrun(&runs).as_deref(),
            Some(
                "the quick tier took 131.3 s, over its 120 s budget; slowest runs: \
                 b [default] (4.20 s), e [default] (4.20 s), d [deterministic] (3.10 s), \
                 g [deterministic] (2.50 s), a [default] (900 ms)"
            ),
            "the 150 ms and the 60 ms runs are not among the five slowest, and runs of equal \
             time keep the order they ran in"
        );
    }

    #[test]
    fn a_tier_within_its_budget_has_nothing_to_say_but_its_time() {
        let runs = [run("a", Profile::Default, 900)];
        let within = time(
            Duration::from_millis(22_634),
            Duration::from_secs(120),
            true,
        );
        let exactly = time(Duration::from_secs(120), Duration::from_secs(120), true);

        for time in [within, exactly] {
            assert!(!time.is_over() && !time.fails_run(), "{time:?}");
            assert_eq!(time.overrun(&runs), None, "{time:?}");
        }
        assert_eq!(within.against_budget(), "quick tier: 22.6 s of 120 s");
        assert_eq!(exactly.against_budget(), "quick tier: 120 s of 120 s");
    }

    #[test]
    fn a_tier_over_its_budget_fails_only_where_the_budget_is_held() {
        let elapsed = Duration::from_millis(131_204);
        let held = time(elapsed, Duration::from_secs(120), true);
        let advisory = time(elapsed, Duration::from_secs(120), false);

        assert!(held.is_over() && held.fails_run());
        assert!(advisory.is_over() && !advisory.fails_run());
        assert_eq!(held.overrun(&[]), advisory.overrun(&[]));
        assert_eq!(
            held.against_budget(),
            "quick tier: 131.3 s, over its 120 s budget"
        );
    }

    #[test]
    fn a_time_over_the_budget_never_reads_as_the_budget() {
        // 120.0004 s is over 120 s, though neither a whole millisecond nor a nearest tenth can say so.
        let over = time(
            Duration::from_secs(120) + Duration::from_micros(400),
            Duration::from_secs(120),
            true,
        );

        assert_eq!(
            over.overrun(&[]).as_deref(),
            Some("the quick tier took 120.1 s, over its 120 s budget")
        );
        assert_eq!(
            over.millis(),
            120_000,
            "the summary records whole milliseconds"
        );
    }
}
