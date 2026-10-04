//! Budget limits for `expect_budget`.
//!
//! A scenario can override any limit in its `[budgets]` table. Without an override, the runner
//! applies the default below, which is the strict local and nightly limit of the validation
//! program. Every budget is an upper bound: a metric passes when it is at most the limit.
//!
//! A run can also carry a [`LatencyScale`], which multiplies the limit of every latency budget
//! (`input_to_frame_p99_ms`, `max_stall_ms`, `first_frame_ms`, `quit_ms`), whether the limit is
//! the default or a scenario's override. Pull-request runs use it to hold the latency budgets at
//! twice their strict value on a hosted runner. No other budget is ever scaled: memory, counts,
//! residue, idle output and CPU, and the ratios and fractions compare against a contract, not
//! against how busy the machine was.
//!
//! Three budgets have no default because their limit is a measured baseline that the validation
//! program calibrates later: `scan_store_bytes_per_entry`, `threads`, and `fds`. A scenario that
//! checks one must set the limit.

use thiserror::Error;

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

/// Whether `budget` is a latency budget, which a [`LatencyScale`] relaxes.
///
/// Every budget is named here, so a new one cannot be added without deciding whether a loaded
/// machine may excuse it.
#[must_use]
pub const fn is_latency(budget: Budget) -> bool {
    match budget {
        Budget::InputToFrameP99Ms | Budget::MaxStallMs | Budget::FirstFrameMs | Budget::QuitMs => {
            true
        }
        Budget::HeadlessScanRatio
        | Budget::TuiCompleteRatio
        | Budget::MotionCompleteRatio
        | Budget::PeakRssBytes
        | Budget::MemoryAbTolerance
        | Budget::ScanStoreQuotaFraction
        | Budget::ScanStoreBytesPerEntry
        | Budget::Threads
        | Budget::Fds
        | Budget::IdleOutputBytes
        | Budget::IdleCpuMs
        | Budget::ResidueFiles
        | Budget::TimingAbRegression => false,
    }
}

/// The factor a run multiplies every latency limit by: 1 for the strict limits, 2 for a
/// pull-request run on a hosted runner.
///
/// A scale is never below 1, so it can relax the latency budgets and never tighten them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LatencyScale(f64);

/// A latency scale that is not a finite number of at least 1.
#[derive(Debug, Clone, Copy, PartialEq, Error)]
#[error("a latency scale is a finite number of at least 1, not {0}")]
pub struct InvalidLatencyScale(pub f64);

impl LatencyScale {
    /// No scaling: the latency budgets keep their strict limits.
    pub const STRICT: Self = Self(1.0);

    /// A scale of `factor` times the strict limits.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidLatencyScale`] when `factor` is not finite or is below 1.
    pub fn new(factor: f64) -> Result<Self, InvalidLatencyScale> {
        if factor.is_finite() && factor >= 1.0 {
            Ok(Self(factor))
        } else {
            Err(InvalidLatencyScale(factor))
        }
    }

    /// The factor.
    #[must_use]
    pub const fn factor(self) -> f64 {
        self.0
    }

    /// Whether this scale leaves every limit as it is.
    #[must_use]
    pub fn is_strict(self) -> bool {
        self.0 <= 1.0
    }
}

impl Default for LatencyScale {
    fn default() -> Self {
        Self::STRICT
    }
}

/// The limit that applies to `budget` in `scenario` before any scale: its override, else the
/// default.
fn strict_limit(scenario: &Scenario, budget: Budget) -> Option<f64> {
    scenario
        .budgets
        .get(&budget)
        .copied()
        .or_else(|| default_limit(budget))
}

/// The limit that applies to `budget` in `scenario` under `scale`: its override, else the
/// default, multiplied by the scale when `budget` is a latency budget.
#[must_use]
pub fn limit_for(scenario: &Scenario, budget: Budget, scale: LatencyScale) -> Option<f64> {
    let strict = strict_limit(scenario, budget)?;
    Some(if is_latency(budget) {
        strict * scale.factor()
    } else {
        strict
    })
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

    fn strict(scenario: &Scenario, budget: Budget) -> Option<f64> {
        limit_for(scenario, budget, LatencyScale::STRICT)
    }

    fn doubled() -> LatencyScale {
        LatencyScale::new(2.0).expect("2 is a valid scale")
    }

    #[test]
    fn the_defaults_are_the_latency_and_memory_budgets_of_the_validation_program() {
        let scenario = scenario("");

        assert_eq!(strict(&scenario, Budget::InputToFrameP99Ms), Some(50.0));
        assert_eq!(strict(&scenario, Budget::MaxStallMs), Some(250.0));
        assert_eq!(strict(&scenario, Budget::FirstFrameMs), Some(250.0));
        assert_eq!(strict(&scenario, Budget::QuitMs), Some(250.0));
        assert_eq!(strict(&scenario, Budget::PeakRssBytes), Some(536_870_912.0));
    }

    #[test]
    fn a_scenario_override_wins_over_the_default() {
        let scenario = scenario("[budgets]\nmax_stall_ms = 400\nthreads = 12\n");

        assert_eq!(strict(&scenario, Budget::MaxStallMs), Some(400.0));
        assert_eq!(strict(&scenario, Budget::Threads), Some(12.0));
        assert_eq!(strict(&scenario, Budget::QuitMs), Some(250.0));
    }

    #[test]
    fn baseline_budgets_have_no_default() {
        let scenario = scenario("");

        for budget in [Budget::Threads, Budget::Fds, Budget::ScanStoreBytesPerEntry] {
            assert_eq!(strict(&scenario, budget), None, "{budget}");
            assert_eq!(
                limit_for(&scenario, budget, doubled()),
                None,
                "a scale does not invent a limit for {budget}"
            );
        }
    }

    #[test]
    fn a_latency_scale_multiplies_every_latency_limit_default_or_override() {
        let scenario = scenario("[budgets]\nmax_stall_ms = 400\n");
        let scale = LatencyScale::new(2.5).expect("2.5 is a valid scale");

        assert_eq!(
            limit_for(&scenario, Budget::InputToFrameP99Ms, scale),
            Some(125.0)
        );
        assert_eq!(
            limit_for(&scenario, Budget::MaxStallMs, scale),
            Some(1_000.0),
            "an override is scaled too: it is the scenario's strict limit"
        );
        assert_eq!(
            limit_for(&scenario, Budget::FirstFrameMs, scale),
            Some(625.0)
        );
        assert_eq!(limit_for(&scenario, Budget::QuitMs, scale), Some(625.0));
    }

    #[test]
    fn only_the_four_latency_budgets_are_ever_scaled() {
        let latency: Vec<Budget> = Budget::ALL
            .iter()
            .copied()
            .filter(|budget| is_latency(*budget))
            .collect();
        assert_eq!(
            latency,
            [
                Budget::InputToFrameP99Ms,
                Budget::MaxStallMs,
                Budget::FirstFrameMs,
                Budget::QuitMs
            ]
        );

        // Overrides for the budgets that have no default, so that every budget has a limit to
        // compare.
        let scenario =
            scenario("[budgets]\nthreads = 12\nfds = 48\nscan_store_bytes_per_entry = 1665\n");
        let scale = LatencyScale::new(10.0).expect("10 is a valid scale");
        for budget in Budget::ALL.iter().copied().filter(|b| !is_latency(*b)) {
            let limit = strict(&scenario, budget);
            assert!(limit.is_some(), "{budget} has a limit to compare");
            assert_eq!(
                limit_for(&scenario, budget, scale),
                limit,
                "{budget} is not a latency budget and ignores the scale"
            );
        }
    }

    #[test]
    fn a_scale_below_one_or_not_a_finite_number_is_refused() {
        for factor in [
            0.0,
            0.5,
            0.999,
            -2.0,
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
        ] {
            assert!(LatencyScale::new(factor).is_err(), "{factor}");
        }
        for factor in [1.0, 1.5, 2.0, 100.0] {
            assert_eq!(
                LatencyScale::new(factor).map(LatencyScale::factor),
                Ok(factor)
            );
        }
        assert!(LatencyScale::STRICT.is_strict());
        assert!(LatencyScale::default().is_strict());
        assert!(LatencyScale::new(1.0).is_ok_and(LatencyScale::is_strict));
        assert!(!doubled().is_strict());
    }
}
