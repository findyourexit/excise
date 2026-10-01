//! Verdict policy for one metric's comparison: a block, a warning, or a pass.

use crate::report::{AbVerdict, ConfidenceInterval};

/// Whether a metric is judged as elapsed time (or a count that moves like one) or as memory,
/// which has its own, tighter, symmetric tolerance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricKind {
    /// A timing metric: a regression is a large, confident increase.
    Timing,
    /// A memory metric: a regression is a large, confident move in either direction.
    Memory,
}

impl MetricKind {
    /// Classifies a metric by its bare name: the part after the case prefix (see
    /// [`crate::bench::cases::bare_metric_name`]).
    #[must_use]
    pub fn of(bare_metric_name: &str) -> Self {
        match bare_metric_name {
            "peak_memory_bytes" | "peak_rss_bytes" => Self::Memory,
            _ => Self::Timing,
        }
    }
}

/// Whether `ci` excludes 1.0 (no change), the confidence the budget tables call "excludes 0" for
/// a regression stated as a difference; this tool states a regression as a ratio, so "no change"
/// is 1.0 rather than 0.
#[must_use]
fn excludes_no_change(ci: ConfidenceInterval) -> bool {
    ci.lower > 1.0 || ci.upper < 1.0
}

/// The verdict for one metric.
///
/// * **Timing**: a median at or below the baseline passes. A worse median blocks when it
///   is more than `timing_threshold` worse (for example `0.20` for 20%) and the interval excludes
///   1.0; any other worse median warns.
/// * **Memory**: a median within `memory_tolerance` of 1.0, in either direction, passes.
///   Beyond tolerance blocks when the interval excludes 1.0; beyond tolerance without that
///   confidence warns.
#[must_use]
pub fn classify(
    kind: MetricKind,
    median_ratio: f64,
    bootstrap_ci: ConfidenceInterval,
    timing_threshold: f64,
    memory_tolerance: f64,
) -> AbVerdict {
    let confident = excludes_no_change(bootstrap_ci);
    match kind {
        MetricKind::Timing if median_ratio <= 1.0 => AbVerdict::Pass,
        MetricKind::Timing => {
            if median_ratio > 1.0 + timing_threshold && confident {
                AbVerdict::Block
            } else {
                AbVerdict::Warn
            }
        }
        MetricKind::Memory => {
            if (median_ratio - 1.0).abs() <= memory_tolerance {
                AbVerdict::Pass
            } else if confident {
                AbVerdict::Block
            } else {
                AbVerdict::Warn
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NARROW_ABOVE_ONE: ConfidenceInterval = ConfidenceInterval {
        lower: 1.05,
        upper: 1.5,
        confidence: 0.95,
    };
    const WIDE_INCLUDING_ONE: ConfidenceInterval = ConfidenceInterval {
        lower: 0.9,
        upper: 1.5,
        confidence: 0.95,
    };

    #[test]
    fn a_metric_is_classified_by_its_bare_name_only() {
        assert_eq!(MetricKind::of("peak_memory_bytes"), MetricKind::Memory);
        assert_eq!(MetricKind::of("peak_rss_bytes"), MetricKind::Memory);
        assert_eq!(MetricKind::of("wall_time_ms"), MetricKind::Timing);
        assert_eq!(MetricKind::of("scan_complete_ms"), MetricKind::Timing);
    }

    #[test]
    fn timing_at_or_below_baseline_always_passes() {
        for ratio in [0.0, 0.5, 1.0] {
            assert_eq!(
                classify(MetricKind::Timing, ratio, NARROW_ABOVE_ONE, 0.20, 0.05),
                AbVerdict::Pass,
                "ratio {ratio}"
            );
        }
    }

    #[test]
    fn timing_blocks_only_past_the_threshold_with_a_confident_interval() {
        // Exactly at the 20% threshold is not yet "more than" it: warn, not block.
        assert_eq!(
            classify(MetricKind::Timing, 1.20, NARROW_ABOVE_ONE, 0.20, 0.05),
            AbVerdict::Warn
        );
        // Just past the threshold, with a confident interval: block.
        assert_eq!(
            classify(MetricKind::Timing, 1.200_001, NARROW_ABOVE_ONE, 0.20, 0.05),
            AbVerdict::Block
        );
        // Clearly past the threshold, but the interval still includes no change: warn.
        assert_eq!(
            classify(MetricKind::Timing, 1.30, WIDE_INCLUDING_ONE, 0.20, 0.05),
            AbVerdict::Warn
        );
        // A mild regression under the threshold warns regardless of confidence.
        assert_eq!(
            classify(MetricKind::Timing, 1.05, NARROW_ABOVE_ONE, 0.20, 0.05),
            AbVerdict::Warn
        );
    }

    #[test]
    fn memory_within_tolerance_always_passes() {
        for ratio in [0.96, 1.0, 1.04] {
            assert_eq!(
                classify(MetricKind::Memory, ratio, NARROW_ABOVE_ONE, 0.20, 0.05),
                AbVerdict::Pass,
                "ratio {ratio}"
            );
        }
    }

    #[test]
    fn memory_blocks_only_past_tolerance_with_a_confident_interval() {
        // Just past the 5% tolerance, with a confident interval: block.
        assert_eq!(
            classify(MetricKind::Memory, 1.06, NARROW_ABOVE_ONE, 0.20, 0.05),
            AbVerdict::Block
        );
        // Same regression, but the interval still includes no change: warn.
        assert_eq!(
            classify(MetricKind::Memory, 1.06, WIDE_INCLUDING_ONE, 0.20, 0.05),
            AbVerdict::Warn
        );
        // The tolerance is symmetric: a confident move the other way also blocks.
        assert_eq!(
            classify(
                MetricKind::Memory,
                0.90,
                ConfidenceInterval {
                    lower: 0.85,
                    upper: 0.95,
                    confidence: 0.95
                },
                0.20,
                0.05
            ),
            AbVerdict::Block
        );
    }
}
