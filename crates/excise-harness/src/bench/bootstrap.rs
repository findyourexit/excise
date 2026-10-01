//! Deterministic bootstrap statistics for paired A/B comparisons.
//!
//! A metric's evidence is the per-trial candidate/baseline ratio, index-aligned across the two
//! sides (see [`crate::bench::pairing`]). The headline is the median of those ratios; the
//! bootstrap resamples trial indices with replacement, recomputes the median ratio of each
//! resample, and takes a percentile interval of the resampled medians. Resampling draws from a
//! PRNG seeded from the caller's `--seed`, so the same seed always gives the same interval.
//! Timings are only evidence when they can be judged, and a confidence interval that moved
//! between two reads of the same samples could not be judged.

use crate::report::ConfidenceInterval;
use crate::run_support::median;

/// How many resamples the bootstrap draws. Large enough that the interval is stable between
/// seeds, small enough to stay instant at the pair counts this tool runs (tens, not thousands).
const RESAMPLES: u32 = 2_000;

/// The confidence level of [`bootstrap_ci`]'s interval.
const CONFIDENCE: f64 = 0.95;

/// A small, deterministic, non-cryptographic PRNG (`SplitMix64`), seeded explicitly so the
/// bootstrap is reproducible for a given seed.
struct SplitMix64(u64);

impl SplitMix64 {
    const fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// The next value in the sequence.
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A uniform index in `0..bound`. `bound` must be nonzero.
    fn index(&mut self, bound: usize) -> usize {
        let bound = u64::try_from(bound).unwrap_or(u64::MAX);
        let value = self.next_u64() % bound;
        usize::try_from(value).unwrap_or(usize::MAX)
    }
}

/// The per-trial candidate/baseline ratios, index-aligned. A pair whose baseline measurement is
/// zero has no ratio and is left out, as in [`crate::headless::pairs::ratios`].
#[must_use]
pub fn ratios(baseline: &[f64], candidate: &[f64]) -> Vec<f64> {
    baseline
        .iter()
        .zip(candidate)
        .filter(|(base, _)| **base != 0.0)
        .map(|(base, cand)| cand / base)
        .collect()
}

/// The median of the per-trial candidate/baseline ratios, or `0.0` when no pair has a ratio
/// (every baseline measurement was zero).
#[must_use]
pub fn median_ratio(baseline: &[f64], candidate: &[f64]) -> f64 {
    median(&ratios(baseline, candidate)).unwrap_or(0.0)
}

/// A deterministic bootstrap confidence interval for the median of the per-trial
/// candidate/baseline ratios.
///
/// Each of [`RESAMPLES`] resamples draws `ratios.len()` indices with replacement from a PRNG
/// seeded by `seed`, takes the median ratio of the resample, and the interval is the
/// `(1 - CONFIDENCE) / 2` and `1 - (1 - CONFIDENCE) / 2` nearest-rank percentiles of the resampled
/// medians. Returns a point interval at the sample median when there are fewer than two ratios to
/// resample.
#[must_use]
pub fn bootstrap_ci(baseline: &[f64], candidate: &[f64], seed: u64) -> ConfidenceInterval {
    let ratios = ratios(baseline, candidate);
    let point = median(&ratios).unwrap_or(0.0);
    if ratios.len() < 2 {
        return ConfidenceInterval {
            lower: point,
            upper: point,
            confidence: CONFIDENCE,
        };
    }
    let mut rng = SplitMix64::new(seed);
    let mut resampled: Vec<f64> = (0..RESAMPLES)
        .map(|_| {
            let draw: Vec<f64> = (0..ratios.len())
                .map(|_| ratios[rng.index(ratios.len())])
                .collect();
            median(&draw).unwrap_or(point)
        })
        .collect();
    resampled.sort_by(f64::total_cmp);
    let tail = (1.0 - CONFIDENCE) / 2.0;
    ConfidenceInterval {
        lower: percentile(&resampled, tail),
        upper: percentile(&resampled, 1.0 - tail),
        confidence: CONFIDENCE,
    }
}

/// The nearest-rank percentile of already-sorted `values` (not empty).
fn percentile(sorted: &[f64], fraction: f64) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let rank = (fraction * sorted.len() as f64).ceil();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let rank = (rank as usize).clamp(1, sorted.len());
    sorted[rank - 1]
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASELINE: [f64; 5] = [10.0, 10.0, 10.0, 10.0, 10.0];
    const CANDIDATE: [f64; 5] = [9.0, 10.0, 11.0, 9.5, 10.5];

    #[test]
    fn the_median_ratio_is_the_median_of_the_per_trial_ratios() {
        // Ratios: 0.9, 1.0, 1.1, 0.95, 1.05 -> sorted 0.9, 0.95, 1.0, 1.05, 1.1 -> median 1.0.
        let ratio = median_ratio(&BASELINE, &CANDIDATE);
        assert!((ratio - 1.0).abs() < 1e-9, "{ratio}");
    }

    #[test]
    fn a_zero_baseline_pair_has_no_ratio() {
        let ratio = median_ratio(&[0.0, 10.0], &[5.0, 11.0]);
        assert!((ratio - 1.1).abs() < 1e-9, "{ratio}");
        assert!(median_ratio(&[0.0], &[5.0]).abs() < f64::EPSILON);
    }

    #[test]
    fn the_bootstrap_interval_is_deterministic_for_a_seed() {
        let first = bootstrap_ci(&BASELINE, &CANDIDATE, 42);
        let second = bootstrap_ci(&BASELINE, &CANDIDATE, 42);

        assert_eq!(first, second, "the same seed must give the same interval");

        // A seeded PRNG, not wall-clock or process-state randomness: calling it again (even from
        // a different seed) never depends on anything outside its own state.
        let reseeded_same_seed = bootstrap_ci(&BASELINE, &CANDIDATE, 42);
        assert_eq!(first, reseeded_same_seed);
    }

    #[test]
    fn the_bootstrap_interval_brackets_the_median_ratio() {
        for seed in [0, 1, 42, 1_000, u64::MAX] {
            let point = median_ratio(&BASELINE, &CANDIDATE);
            let ci = bootstrap_ci(&BASELINE, &CANDIDATE, seed);

            assert!(ci.lower <= point, "seed {seed}: {ci:?} vs point {point}");
            assert!(ci.upper >= point, "seed {seed}: {ci:?} vs point {point}");
            assert!(ci.lower <= ci.upper, "seed {seed}: {ci:?}");
            assert!((ci.confidence - 0.95).abs() < 1e-12);
        }
    }

    #[test]
    fn identical_builds_give_a_point_interval_at_one() {
        let same = [1.0, 2.0, 3.0, 4.0];
        let ci = bootstrap_ci(&same, &same, 7);

        assert!((ci.lower - 1.0).abs() < 1e-9);
        assert!((ci.upper - 1.0).abs() < 1e-9);
    }

    #[test]
    fn fewer_than_two_ratios_gives_a_point_interval_at_the_median() {
        let ci = bootstrap_ci(&[10.0], &[11.0], 1);
        assert!((ci.lower - 1.1).abs() < 1e-9);
        assert!((ci.upper - 1.1).abs() < 1e-9);

        let ci = bootstrap_ci(&[], &[], 1);
        assert!(ci.lower.abs() < f64::EPSILON);
        assert!(ci.upper.abs() < f64::EPSILON);
    }
}
