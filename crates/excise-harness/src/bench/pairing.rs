//! Building the interleaved run order, and pairing its measurements back up by trial.

use crate::report::Side;

/// The run order for `pairs` trials: baseline then candidate, `pairs` times, so that the two
/// builds alternate one-for-one: paired and interleaved. Its length is always
/// `2 * pairs`.
#[must_use]
pub fn interleaving(pairs: u32) -> Vec<Side> {
    (0..pairs)
        .flat_map(|_| [Side::Baseline, Side::Candidate])
        .collect()
}

/// Splits `values`, recorded in the same order as `order`, back into the two sides, index-aligned
/// by trial: `baseline[i]` and `candidate[i]` are the measurements of the same trial.
///
/// # Panics
///
/// Panics if `values.len() != order.len()`, or if `order` does not pair one baseline with one
/// candidate (a caller bug: `order` must be the output of [`interleaving`]).
#[must_use]
pub fn align(order: &[Side], values: &[f64]) -> (Vec<f64>, Vec<f64>) {
    assert_eq!(
        order.len(),
        values.len(),
        "one recorded value per run in the order"
    );
    let mut baseline = Vec::with_capacity(order.len().div_ceil(2));
    let mut candidate = Vec::with_capacity(order.len() / 2);
    for (side, value) in order.iter().zip(values) {
        match side {
            Side::Baseline => baseline.push(*value),
            Side::Candidate => candidate.push(*value),
        }
    }
    assert_eq!(
        baseline.len(),
        candidate.len(),
        "the order must pair one baseline run with one candidate run"
    );
    (baseline, candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_interleaving_alternates_baseline_and_candidate_for_every_pair() {
        assert_eq!(interleaving(0), []);
        assert_eq!(interleaving(1), [Side::Baseline, Side::Candidate]);
        assert_eq!(
            interleaving(3),
            [
                Side::Baseline,
                Side::Candidate,
                Side::Baseline,
                Side::Candidate,
                Side::Baseline,
                Side::Candidate,
            ]
        );
        assert_eq!(interleaving(5).len(), 10);
    }

    #[test]
    fn aligning_recovers_index_aligned_pairs_from_the_run_order() {
        let order = interleaving(3);
        // Recorded in run order: baseline, candidate, baseline, candidate, baseline, candidate.
        let values = vec![10.0, 20.0, 11.0, 19.0, 12.0, 18.0];

        let (baseline, candidate) = align(&order, &values);

        assert_eq!(baseline, [10.0, 11.0, 12.0]);
        assert_eq!(candidate, [20.0, 19.0, 18.0]);
    }

    #[test]
    fn aligning_zero_pairs_gives_two_empty_sides() {
        let (baseline, candidate) = align(&[], &[]);

        assert!(baseline.is_empty());
        assert!(candidate.is_empty());
    }

    #[test]
    #[should_panic(expected = "one recorded value per run in the order")]
    fn aligning_a_mismatched_length_panics() {
        let _ = align(&interleaving(2), &[1.0, 2.0]);
    }
}
