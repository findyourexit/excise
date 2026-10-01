//! The timing of paired, interleaved runs: the headless scan against `du -sk`.
//!
//! Timings are only evidence when they come from one session on one warm fixture, run
//! interleaved so that drift in the machine's state hits both sides alike (decision D5.1). The
//! suite runs the scan and `du` alternately, scan first, and every scan is paired with the `du`
//! that followed it. The ratio of a pair is its scan time over its `du` time; the headline is the
//! median ratio, and the spread says how far to trust it.

use std::time::Duration;

use crate::run_support::median;

/// How a set of values is spread: its extremes, its quartiles, and its median.
///
/// The quartiles are the medians of the lower and upper halves of the sorted values, the middle
/// value left out of both for an odd count.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spread {
    /// How many values.
    pub count: usize,
    /// The smallest value.
    pub min: f64,
    /// The median of the lower half.
    pub q1: f64,
    /// The median. For an even count, the midpoint of the two middle values.
    pub median: f64,
    /// The median of the upper half.
    pub q3: f64,
    /// The largest value.
    pub max: f64,
}

impl Spread {
    /// The spread of `values`, or `None` for none. Values that are not finite are ignored.
    #[must_use]
    pub fn of(values: &[f64]) -> Option<Self> {
        let mut sorted: Vec<f64> = values.iter().copied().filter(|v| v.is_finite()).collect();
        sorted.sort_by(f64::total_cmp);
        let (&min, &max) = (sorted.first()?, sorted.last()?);
        let middle = median(&sorted)?;
        let half = sorted.len() / 2;
        let lower = median(&sorted[..half]).unwrap_or(middle);
        let upper = median(&sorted[sorted.len() - half..]).unwrap_or(middle);
        Some(Self {
            count: sorted.len(),
            min,
            q1: lower,
            median: middle,
            q3: upper,
            max,
        })
    }

    /// The interquartile range.
    #[must_use]
    pub fn iqr(&self) -> f64 {
        self.q3 - self.q1
    }
}

/// The ratio of each pair: the scan time over the `du` time that followed it. A pair whose `du`
/// took no measurable time has no ratio and is left out.
#[must_use]
pub fn ratios(scans: &[Duration], references: &[Duration]) -> Vec<f64> {
    scans
        .iter()
        .zip(references)
        .filter(|(_, reference)| !reference.is_zero())
        .map(|(scan, reference)| scan.as_secs_f64() / reference.as_secs_f64())
        .collect()
}

/// A duration in milliseconds.
#[must_use]
pub fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_spread_of_an_odd_count_leaves_the_middle_out_of_both_halves() {
        let spread = Spread::of(&[5.0, 1.0, 3.0, 2.0, 4.0]).expect("a spread");

        assert_eq!(spread.count, 5);
        assert!((spread.min - 1.0).abs() < f64::EPSILON);
        assert!((spread.q1 - 1.5).abs() < f64::EPSILON);
        assert!((spread.median - 3.0).abs() < f64::EPSILON);
        assert!((spread.q3 - 4.5).abs() < f64::EPSILON);
        assert!((spread.max - 5.0).abs() < f64::EPSILON);
        assert!((spread.iqr() - 3.0).abs() < f64::EPSILON);
    }

    #[test]
    fn the_spread_of_an_even_count_splits_it_in_two() {
        let spread = Spread::of(&[1.0, 2.0, 3.0, 4.0]).expect("a spread");

        assert!((spread.q1 - 1.5).abs() < f64::EPSILON);
        assert!((spread.median - 2.5).abs() < f64::EPSILON);
        assert!((spread.q3 - 3.5).abs() < f64::EPSILON);
    }

    #[test]
    fn one_value_is_its_own_spread_and_nothing_is_no_spread() {
        let one = Spread::of(&[2.5]).expect("a spread");

        assert!((one.min - 2.5).abs() < f64::EPSILON);
        assert!((one.q1 - 2.5).abs() < f64::EPSILON);
        assert!((one.median - 2.5).abs() < f64::EPSILON);
        assert!((one.q3 - 2.5).abs() < f64::EPSILON);
        assert!((one.max - 2.5).abs() < f64::EPSILON);
        assert_eq!(Spread::of(&[]), None);
        assert_eq!(Spread::of(&[f64::NAN, f64::INFINITY]), None);
    }

    #[test]
    fn ratios_pair_each_scan_with_the_du_that_followed_it() {
        let ms = Duration::from_millis;

        let paired = ratios(&[ms(30), ms(40), ms(50)], &[ms(10), ms(0), ms(20)]);

        assert_eq!(paired.len(), 2, "a pair with no du time has no ratio");
        assert!((paired[0] - 3.0).abs() < 1e-9);
        assert!((paired[1] - 2.5).abs() < 1e-9);
    }
}
