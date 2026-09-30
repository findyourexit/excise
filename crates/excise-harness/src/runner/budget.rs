//! Budget limits for `expect_budget`.
//!
//! A scenario can override any limit in its `[budgets]` table. Without an override, the runner
//! applies the default below, which is the strict local and nightly limit of the validation
//! program. Pull-request runs apply looser limits to the latency budgets by overriding them; the
//! runner has no notion of a tier here. Every budget is an upper bound: a metric passes when it is
//! at most the limit.
//!
//! Three budgets have no default because their limit is a measured baseline that the validation
//! program calibrates later: `scan_store_bytes_per_entry`, `threads`, and `fds`. A scenario that
//! checks one must set the limit.

use crate::scenario::{Budget, Scenario};

/// One mebibyte.
const MIB: f64 = 1024.0 * 1024.0;

/// The default limit of `budget`, or `None` when the scenario has to supply one.
#[must_use]
pub const fn default_limit(budget: Budget) -> Option<f64> {
    match budget {
        Budget::HeadlessScanRatio => Some(3.0),
        Budget::TuiCompleteRatio | Budget::MotionCompleteRatio => Some(1.25),
        Budget::InputToFrameP99Ms | Budget::IdleCpuMs => Some(50.0),
        Budget::MaxStallMs | Budget::FirstFrameMs | Budget::QuitMs => Some(250.0),
        Budget::PeakRssBytes => Some(512.0 * MIB),
        Budget::MemoryAbTolerance => Some(0.05),
        Budget::ScanStoreQuotaFraction => Some(0.75),
        Budget::IdleOutputBytes | Budget::ResidueFiles => Some(0.0),
        Budget::TimingAbRegression => Some(0.20),
        Budget::ScanStoreBytesPerEntry | Budget::Threads | Budget::Fds => None,
    }
}

/// The limit that applies to `budget` in `scenario`: its override, else the default.
#[must_use]
pub fn limit_for(scenario: &Scenario, budget: Budget) -> Option<f64> {
    scenario
        .budgets
        .get(&budget)
        .copied()
        .or_else(|| default_limit(budget))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scenario(budgets: &str) -> Scenario {
        Scenario::from_toml_str(&format!(
            r#"
schema_version = 1
name = "budgets"
description = "d"
fixture = "f"
profiles = ["default"]
{budgets}
[[steps]]
step = "settle"
"#
        ))
        .expect("a scenario")
    }

    #[test]
    fn the_defaults_are_the_latency_and_memory_budgets_of_the_validation_program() {
        let scenario = scenario("");

        assert_eq!(limit_for(&scenario, Budget::InputToFrameP99Ms), Some(50.0));
        assert_eq!(limit_for(&scenario, Budget::MaxStallMs), Some(250.0));
        assert_eq!(limit_for(&scenario, Budget::FirstFrameMs), Some(250.0));
        assert_eq!(limit_for(&scenario, Budget::QuitMs), Some(250.0));
        assert_eq!(
            limit_for(&scenario, Budget::PeakRssBytes),
            Some(536_870_912.0)
        );
    }

    #[test]
    fn a_scenario_override_wins_over_the_default() {
        let scenario = scenario("[budgets]\nmax_stall_ms = 400\nthreads = 12\n");

        assert_eq!(limit_for(&scenario, Budget::MaxStallMs), Some(400.0));
        assert_eq!(limit_for(&scenario, Budget::Threads), Some(12.0));
        assert_eq!(limit_for(&scenario, Budget::QuitMs), Some(250.0));
    }

    #[test]
    fn baseline_budgets_have_no_default() {
        let scenario = scenario("");

        for budget in [Budget::Threads, Budget::Fds, Budget::ScanStoreBytesPerEntry] {
            assert_eq!(limit_for(&scenario, budget), None, "{budget}");
        }
    }
}
