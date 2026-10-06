//! Paired, interleaved timing across every version.
//!
//! A single timing never transfers across sessions (the same binary and tree shape have measured
//! 9.46 s one day and 3.8 s the next), so a version is only ever compared with the candidate
//! through rounds that ran every version one after another in one session on the same warm
//! fixture, and the evidence is the median of the per-round ratios with a bootstrap confidence
//! interval ([`crate::bench::bootstrap`], the statistics `cargo xtask bench-e2e` uses).
//!
//! One untimed warm-up round runs every version first. Then each measured round runs them again,
//! starting from a different version every round, so that no version always runs right after the
//! same one and a drift of the machine over a round falls on every version in turn. In its turn a
//! version runs every *leg* of the phase back to back (a headless scan and then an interface run,
//! say), so that what two legs measure of one version is measured in the same round. The order of
//! every round is recorded.
//!
//! **Rounds are logical.** Every measurement is kept in the slot of the round it was taken in. A
//! round a version did not run in (it was given up on, or its runs could not be carried out) is a
//! slot with no sample: it is neither a sample nor a pair, and it never moves a later round
//! against another one. A ratio is made of the rounds in which both of its sides have a sample.
//!
//! **A run that does not finish is censored.** Whether a measurement finished within the bound is
//! kept for each metric of a run, and one that did not is recorded at how long it ran (the bound,
//! when it ran out of time), which is the least it took. A round in which both sides finished gives
//! a ratio, and the median and the interval rest on those rounds alone. A round in which only the
//! numerator did not finish gives a lower bound on its ratio, one in which only the denominator did
//! not finish gives an upper bound, and one in which neither finished says nothing, so it is
//! counted and never made a ratio.
//!
//! **Giving up.** A leg of a version whose runs have run out of time a given number of times in a
//! row is not run again, and the rounds it misses are recorded as skipped: that saves the whole
//! bound that a run which cannot finish would wait each time. The warm-up round does not count
//! towards it, so every version that is given up on has that many runs that ran out of time in the
//! record.

use std::collections::BTreeMap;

use crate::{
    bench::bootstrap::{bootstrap_ci, median_ratio, ratios},
    report::{SweepRatio, SweepSeries},
    run_support::median,
};

/// One measurement of one run.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Reading {
    /// Milliseconds. A run that did not finish within the bound is recorded at how long it ran (the
    /// bound, when it ran out of time), which is the least it took.
    pub ms: f64,
    /// Whether the measurement finished within the bound.
    pub completed: bool,
}

impl Reading {
    /// A reading of `ms` milliseconds that did, or did not, finish within the bound.
    pub(crate) const fn new(ms: f64, completed: bool) -> Self {
        Self { ms, completed }
    }
}

/// What one run measured: for each series key the run's leg feeds, a reading, or `None` when the
/// run has no usable reading of that series (a slot without a sample, like a round the version did
/// not run in, and never a reading of 0 ms).
pub(crate) type RunValue = BTreeMap<&'static str, Option<Reading>>;

/// What became of one round of one series of one version.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Round {
    /// The run was carried out.
    Measured(Reading),
    /// The version was not run in this round, because its runs had run out of time too many times
    /// in a row.
    Skipped,
    /// There is no usable reading for this round: the version's runs could not be carried out, or
    /// the run had no usable reading of this series.
    Failed,
}

/// Everything one version's rounds came to.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct VersionRuns {
    /// For each series key, one slot per round, in round order.
    pub rounds: BTreeMap<&'static str, Vec<Round>>,
    /// Why the runs of a series stopped, by series key, when they could not be carried out.
    pub errors: BTreeMap<&'static str, String>,
}

/// What the rounds of one phase came to.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Paired {
    /// The versions that ran in each round, in the order they ran. A version that was skipped in a
    /// round, or whose runs could not be carried out, is not in it.
    pub order: Vec<Vec<String>>,
    /// What each version's runs came to, by its name.
    pub runs: BTreeMap<String, VersionRuns>,
    /// The milliseconds `du` took in each round: `None` for a round it was not asked in, or could
    /// not be timed in.
    pub du: Vec<Option<f64>>,
}

/// What a series is also held against, besides the candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Against {
    /// `du -sk`, timed once per round.
    Du,
    /// Another series of the same version, in the same rounds.
    Own(&'static str),
}

/// The order version indices run in on round `round` of `count` versions: from a different one
/// each round, wrapping around.
pub(crate) fn rotation(count: usize, round: usize) -> Vec<usize> {
    if count == 0 {
        return Vec::new();
    }
    let start = round % count;
    (0..count).map(|offset| (start + offset) % count).collect()
}

/// What one leg of one version has come to: how many of its measured runs in a row ran out of
/// time, and whether it can be run at all.
#[derive(Debug, Clone, Copy, Default)]
struct LegState {
    out_of_time: usize,
    failed: bool,
}

/// Runs one leg of one version's turn, unless it is to be left out, and returns the slot of each
/// of the leg's `keys` for the round, and why the leg's runs stopped when this run stopped them.
///
/// A leg ran out of time when any of its readings did not finish.
fn leg_turn(
    state: &mut LegState,
    keys: &[&'static str],
    give_up_after: Option<usize>,
    run: impl FnOnce() -> Result<RunValue, String>,
) -> (Vec<Round>, Option<String>) {
    let all = |round: Round| vec![round; keys.len()];
    if state.failed {
        return (all(Round::Failed), None);
    }
    if give_up_after.is_some_and(|limit| state.out_of_time >= limit) {
        return (all(Round::Skipped), None);
    }
    let value = match run() {
        Ok(value) => value,
        Err(error) => {
            state.failed = true;
            return (all(Round::Failed), Some(error));
        }
    };
    let mut readings = Vec::with_capacity(keys.len());
    for key in keys {
        let Some(reading) = value.get(key) else {
            state.failed = true;
            return (
                all(Round::Failed),
                Some(format!("the run measured no `{key}`")),
            );
        };
        readings.push(*reading);
    }
    // A series the run has no usable reading of is a slot without a sample: it says nothing of
    // whether the run ran out of time.
    if readings.iter().flatten().all(|reading| reading.completed) {
        state.out_of_time = 0;
    } else {
        state.out_of_time += 1;
    }
    (
        readings
            .into_iter()
            .map(|reading| reading.map_or(Round::Failed, Round::Measured))
            .collect(),
        None,
    )
}

/// Keeps why the runs of the series `keys` of `name` stopped.
fn record_error(paired: &mut Paired, name: &str, keys: &[&'static str], error: Option<String>) {
    let Some(error) = error else {
        return;
    };
    if let Some(runs) = paired.runs.get_mut(name) {
        for key in keys {
            runs.errors.insert(*key, error.clone());
        }
    }
}

/// Runs `rounds` measured rounds after one warm-up round.
///
/// A round runs every version in turn, in the [`rotation`] of the round, and in its turn a version
/// runs each of the `legs` one after another: a leg is the list of series keys that one run of it
/// feeds. `run` is called with a version's index and a leg's index and returns what the run
/// measured (a reading for each key of the leg), or why it could not be carried out; a leg of a
/// version whose run could not be carried out is not run again, and its error is kept. `du` is
/// called once at the start of every round, warm-up included, and returns the milliseconds it
/// took, if there is a `du` to time against and it could be timed.
///
/// With `give_up_after`, a leg of a version whose measured runs have run out of time that many
/// times in a row is not run again: each round it misses is recorded as skipped, and is no sample.
/// A version that cannot finish twice in a row will not finish a third time, and the wait is the
/// whole bound each time. The warm-up round is not counted: every version that is given up on has
/// that many runs that ran out of time in the record.
pub(crate) fn paired_rounds(
    names: &[String],
    legs: &[&[&'static str]],
    rounds: u32,
    give_up_after: Option<usize>,
    mut du: impl FnMut() -> Option<f64>,
    mut run: impl FnMut(usize, usize) -> Result<RunValue, String>,
) -> Paired {
    let mut paired = Paired {
        order: Vec::new(),
        runs: names
            .iter()
            .map(|name| (name.clone(), VersionRuns::default()))
            .collect(),
        du: Vec::new(),
    };
    let mut states = vec![vec![LegState::default(); legs.len()]; names.len()];

    // The warm-up round: nothing it measures is kept, and a run of it that ran out of time does
    // not count towards giving a version up. A run that cannot be carried out stops its leg.
    let _ = du();
    for (version, name) in names.iter().enumerate() {
        for (leg, keys) in legs.iter().enumerate() {
            let state = &mut states[version][leg];
            let (_, error) = leg_turn(state, keys, None, || run(version, leg));
            state.out_of_time = 0;
            record_error(&mut paired, name, keys, error);
        }
    }

    for round in 0..rounds as usize {
        paired.du.push(du());
        let mut ran = Vec::new();
        for version in rotation(names.len(), round) {
            let mut carried_out = false;
            for (leg, keys) in legs.iter().enumerate() {
                let state = &mut states[version][leg];
                let (slots, error) = leg_turn(state, keys, give_up_after, || run(version, leg));
                carried_out |= slots.iter().any(|slot| matches!(slot, Round::Measured(_)));
                if let Some(runs) = paired.runs.get_mut(&names[version]) {
                    for (key, slot) in keys.iter().zip(slots) {
                        runs.rounds.entry(*key).or_default().push(slot);
                    }
                }
                record_error(&mut paired, &names[version], keys, error);
            }
            if carried_out {
                ran.push(names[version].clone());
            }
        }
        paired.order.push(ran);
    }
    paired
}

/// The ratio of `numerator` over `denominator`, taken over the rounds in which both have a sample,
/// or `None` when no round has one.
///
/// The slots are index-aligned by round, so a round that either side did not run in is left out
/// and never shifts a later round against another. A round in which both finished gives a ratio
/// (unless its denominator is zero), and the median and the interval rest on those. A round in
/// which only the numerator did not finish gives a lower bound on its ratio, one in which only
/// the denominator did not finish gives an upper bound, and one in which neither finished is
/// counted and says nothing.
pub(crate) fn ratio_between(
    numerator: &[Round],
    denominator: &[Round],
    seed: u64,
) -> Option<SweepRatio> {
    let (mut finished_numerators, mut finished_denominators) = (Vec::new(), Vec::new());
    let (mut lower_bounds, mut upper_bounds, mut both_censored) = (Vec::new(), Vec::new(), 0_u32);
    for (above, below) in numerator.iter().zip(denominator) {
        let (Round::Measured(above), Round::Measured(below)) = (above, below) else {
            continue;
        };
        match (above.completed, below.completed) {
            (true, true) => {
                finished_numerators.push(above.ms);
                finished_denominators.push(below.ms);
            }
            (false, true) if below.ms != 0.0 => lower_bounds.push(above.ms / below.ms),
            (true, false) if below.ms != 0.0 => upper_bounds.push(above.ms / below.ms),
            (false, false) => both_censored += 1,
            _ => {}
        }
    }
    let pairs = ratios(&finished_denominators, &finished_numerators).len();
    if pairs == 0 && lower_bounds.is_empty() && upper_bounds.is_empty() && both_censored == 0 {
        return None;
    }
    let finished = pairs > 0;
    Some(SweepRatio {
        median: finished.then(|| median_ratio(&finished_denominators, &finished_numerators)),
        bootstrap_ci: finished
            .then(|| bootstrap_ci(&finished_denominators, &finished_numerators, seed)),
        pairs: u32::try_from(pairs).unwrap_or(u32::MAX),
        lower_bounds,
        upper_bounds,
        both_censored,
    })
}

/// The slots of the rounds `du` was timed in: it has finished whenever it has a time.
fn du_slots(du: &[Option<f64>]) -> Vec<Round> {
    du.iter()
        .map(|ms| match ms {
            Some(ms) => Round::Measured(Reading::new(*ms, true)),
            None => Round::Failed,
        })
        .collect()
}

/// The series of the key `key`, one per version that has a sample of it, in the order of `names`:
/// the rounds the samples were taken in, the samples, whether each completed, how many rounds were
/// skipped, the median of those that completed, and the ratio to the candidate (the last name)
/// and to everything in `also`, each over the rounds in which both sides have a sample.
pub(crate) fn series_of(
    key: &str,
    names: &[String],
    paired: &Paired,
    seed: u64,
    also: &[(&str, Against)],
) -> Vec<SweepSeries> {
    let candidate = names
        .last()
        .and_then(|name| paired.runs.get(name))
        .and_then(|runs| runs.rounds.get(key));
    let du = du_slots(&paired.du);
    names
        .iter()
        .filter_map(|name| {
            let runs = paired.runs.get(name)?;
            let slots = runs.rounds.get(key)?;
            let measured: Vec<(usize, Reading)> = slots
                .iter()
                .enumerate()
                .filter_map(|(round, slot)| match slot {
                    Round::Measured(reading) => Some((round, *reading)),
                    Round::Skipped | Round::Failed => None,
                })
                .collect();
            if measured.is_empty() {
                return None;
            }
            let mut ratios_by_name = BTreeMap::new();
            if let Some(candidate) = candidate
                && let Some(ratio) = ratio_between(slots, candidate, seed)
            {
                ratios_by_name.insert("candidate".to_owned(), ratio);
            }
            for (label, against) in also {
                let denominator = match against {
                    Against::Du => Some(&du),
                    Against::Own(other) => runs.rounds.get(other),
                };
                if let Some(denominator) = denominator
                    && let Some(ratio) = ratio_between(slots, denominator, seed)
                {
                    ratios_by_name.insert((*label).to_owned(), ratio);
                }
            }
            let finished: Vec<f64> = measured
                .iter()
                .filter(|(_, reading)| reading.completed)
                .map(|(_, reading)| reading.ms)
                .collect();
            let skipped = slots
                .iter()
                .filter(|slot| matches!(slot, Round::Skipped))
                .count();
            Some(SweepSeries {
                reference: name.clone(),
                rounds: measured
                    .iter()
                    .map(|(round, _)| u32::try_from(*round).unwrap_or(u32::MAX))
                    .collect(),
                samples: measured.iter().map(|(_, reading)| reading.ms).collect(),
                completed: measured
                    .iter()
                    .map(|(_, reading)| reading.completed)
                    .collect(),
                skipped: u32::try_from(skipped).unwrap_or(u32::MAX),
                median: median(&finished),
                ratios: ratios_by_name,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One leg that feeds one series, `wall`.
    const ONE_LEG: &[&[&str]] = &[&["wall"]];

    fn labelled(labels: &[&str]) -> Vec<String> {
        labels.iter().map(|label| (*label).to_owned()).collect()
    }

    fn near(left: f64, right: f64) -> bool {
        (left - right).abs() < 1e-9
    }

    fn reading(ms: f64, completed: bool) -> Reading {
        Reading { ms, completed }
    }

    fn finished(ms: f64) -> RunValue {
        BTreeMap::from([("wall", Some(reading(ms, true)))])
    }

    fn out_of_time(ms: f64) -> RunValue {
        BTreeMap::from([("wall", Some(reading(ms, false)))])
    }

    fn done(ms: f64) -> Round {
        Round::Measured(reading(ms, true))
    }

    fn unfinished(ms: f64) -> Round {
        Round::Measured(reading(ms, false))
    }

    /// What `paired_rounds` makes of runs that each take one `wall` slot per round.
    fn paired_of(slots: &[(&str, Vec<Round>)], du: Vec<Option<f64>>) -> Paired {
        Paired {
            order: Vec::new(),
            runs: slots
                .iter()
                .map(|(name, rounds)| {
                    (
                        (*name).to_owned(),
                        VersionRuns {
                            rounds: BTreeMap::from([("wall", rounds.clone())]),
                            errors: BTreeMap::new(),
                        },
                    )
                })
                .collect(),
            du,
        }
    }

    #[test]
    fn every_round_starts_from_a_different_version_and_runs_each_once() {
        assert_eq!(rotation(0, 3), Vec::<usize>::new());
        assert_eq!(rotation(3, 0), [0, 1, 2]);
        assert_eq!(rotation(3, 1), [1, 2, 0]);
        assert_eq!(rotation(3, 2), [2, 0, 1]);
        assert_eq!(rotation(3, 3), [0, 1, 2], "it wraps");
        for round in 0..7 {
            let mut order = rotation(4, round);
            order.sort_unstable();
            assert_eq!(order, [0, 1, 2, 3], "round {round}");
        }
    }

    #[test]
    fn the_warm_up_round_runs_every_version_and_keeps_nothing() {
        let names = labelled(&["a", "b", "c"]);
        let mut calls = Vec::new();
        let paired = paired_rounds(
            &names,
            ONE_LEG,
            0,
            None,
            || Some(1.0),
            |version, _| {
                calls.push(version);
                Ok(finished(5.0))
            },
        );
        assert_eq!(calls, [0, 1, 2]);
        assert!(paired.order.is_empty());
        assert!(paired.du.is_empty());
        assert!(
            paired
                .runs
                .values()
                .all(|runs| runs.rounds.values().all(Vec::is_empty))
        );
    }

    #[test]
    fn each_version_runs_once_per_round_in_the_recorded_order_and_slots_line_up() {
        let names = labelled(&["a", "b", "c"]);
        let mut calls = Vec::new();
        let mut tick = 0.0;
        let paired = paired_rounds(
            &names,
            ONE_LEG,
            3,
            None,
            || {
                tick += 1.0;
                Some(tick)
            },
            |version, _| {
                calls.push(version);
                let ms = f64::from(u32::try_from(version).expect("a small index")) + 10.0;
                Ok(if version == 1 {
                    out_of_time(ms)
                } else {
                    finished(ms)
                })
            },
        );
        // Warm-up (0, 1, 2), then the three rotations.
        assert_eq!(calls, [0, 1, 2, 0, 1, 2, 1, 2, 0, 2, 0, 1]);
        assert_eq!(
            paired.order,
            [
                labelled(&["a", "b", "c"]),
                labelled(&["b", "c", "a"]),
                labelled(&["c", "a", "b"]),
            ]
        );
        assert_eq!(paired.runs["a"].rounds["wall"], [done(10.0); 3]);
        assert_eq!(
            paired.runs["b"].rounds["wall"],
            [unfinished(11.0); 3],
            "a run that ran out of time is a sample at the bound, flagged"
        );
        assert_eq!(paired.runs["c"].rounds["wall"], [done(12.0); 3]);
        assert_eq!(
            paired.du,
            [Some(2.0), Some(3.0), Some(4.0)],
            "du ran once per round, the warm-up round first and not kept"
        );
    }

    #[test]
    fn a_version_whose_run_cannot_be_carried_out_is_not_run_again_and_is_in_no_order() {
        let names = labelled(&["a", "b"]);
        let mut calls = Vec::new();
        let paired = paired_rounds(
            &names,
            ONE_LEG,
            2,
            None,
            || None,
            |version, _| {
                calls.push(version);
                if version == 0 {
                    Err("cannot spawn".to_owned())
                } else {
                    Ok(finished(1.0))
                }
            },
        );
        assert_eq!(calls.iter().filter(|version| **version == 0).count(), 1);
        assert_eq!(paired.runs["a"].errors["wall"], "cannot spawn");
        assert_eq!(paired.runs["a"].rounds["wall"], [Round::Failed; 2]);
        assert_eq!(paired.runs["b"].rounds["wall"], [done(1.0); 2]);
        assert_eq!(paired.order, [labelled(&["b"]), labelled(&["b"])]);
        let series = series_of("wall", &names, &paired, 1, &[]);
        assert_eq!(series.len(), 1, "a version with no sample has no series");
        assert_eq!(series[0].reference, "b");
    }

    #[test]
    fn a_version_that_runs_out_of_time_twice_in_a_row_is_given_up_on_and_its_rounds_are_skipped() {
        let names = labelled(&["slow", "fast"]);
        let mut calls = Vec::new();
        let paired = paired_rounds(
            &names,
            ONE_LEG,
            5,
            Some(2),
            || None,
            |version, _| {
                calls.push(version);
                Ok(if version == 0 {
                    out_of_time(90_000.0)
                } else {
                    finished(10.0)
                })
            },
        );
        // The warm-up and the first two rounds ran out of time; the other three did not run.
        assert_eq!(calls.iter().filter(|version| **version == 0).count(), 3);
        assert_eq!(calls.iter().filter(|version| **version == 1).count(), 6);
        assert_eq!(
            paired.runs["slow"].rounds["wall"],
            [
                unfinished(90_000.0),
                unfinished(90_000.0),
                Round::Skipped,
                Round::Skipped,
                Round::Skipped,
            ]
        );
        assert_eq!(
            paired.order[1],
            labelled(&["fast", "slow"]),
            "it ran in the second round"
        );
        for (round, order) in paired.order.iter().enumerate().skip(2) {
            assert_eq!(*order, labelled(&["fast"]), "round {round}");
        }

        let series = series_of("wall", &names, &paired, 3, &[]);
        let slow = &series[0];
        assert_eq!(slow.rounds, [0, 1], "a skipped round is not a sample");
        assert_eq!(slow.samples, [90_000.0; 2]);
        assert_eq!(slow.completed, [false; 2]);
        assert_eq!(slow.skipped, 3);
        assert_eq!(slow.median, None, "no run of it finished");
        let to_candidate = &slow.ratios["candidate"];
        assert_eq!(to_candidate.pairs, 0, "a skipped round is not a pair");
        assert_eq!(to_candidate.median, None);
        assert_eq!(to_candidate.bootstrap_ci, None);
        assert_eq!(to_candidate.lower_bounds, [9_000.0; 2]);
        assert!(to_candidate.upper_bounds.is_empty());
        assert_eq!(to_candidate.both_censored, 0);
        assert_eq!(series[1].rounds, [0, 1, 2, 3, 4]);
        assert_eq!(series[1].skipped, 0);
    }

    #[test]
    fn the_warm_up_runs_that_ran_out_of_time_do_not_count_towards_giving_up() {
        let names = labelled(&["a"]);
        let mut calls = 0;
        let paired = paired_rounds(
            &names,
            ONE_LEG,
            4,
            Some(2),
            || None,
            |_, _| {
                calls += 1;
                // The warm-up and the first round run out of time; the rest finish.
                Ok(if calls <= 2 {
                    out_of_time(1.0)
                } else {
                    finished(1.0)
                })
            },
        );
        assert_eq!(calls, 5, "the warm-up and four rounds all ran");
        assert_eq!(
            paired.runs["a"].rounds["wall"],
            [unfinished(1.0), done(1.0), done(1.0), done(1.0)],
            "one run out of time in the record is not two in a row"
        );
    }

    #[test]
    fn a_version_that_finishes_is_never_given_up_on() {
        let names = labelled(&["a"]);
        let mut calls = 0;
        let paired = paired_rounds(
            &names,
            ONE_LEG,
            4,
            Some(2),
            || None,
            |_, _| {
                calls += 1;
                // Finished, out of time, finished, out of time, finished: never twice in a row.
                Ok(if calls % 2 == 1 {
                    finished(1.0)
                } else {
                    out_of_time(1.0)
                })
            },
        );
        assert_eq!(calls, 5, "the warm-up and four rounds all ran");
        assert_eq!(
            paired.runs["a"].rounds["wall"],
            [unfinished(1.0), done(1.0), unfinished(1.0), done(1.0)]
        );
    }

    #[test]
    fn a_leg_that_is_given_up_on_does_not_stop_the_other_legs_of_the_version() {
        let names = labelled(&["a"]);
        let legs: &[&[&str]] = &[&["scan"], &["tui"]];
        let mut calls = Vec::new();
        let paired = paired_rounds(
            &names,
            legs,
            4,
            Some(2),
            || None,
            |_, leg| {
                calls.push(leg);
                Ok(if leg == 0 {
                    BTreeMap::from([("scan", Some(reading(5.0, true)))])
                } else {
                    BTreeMap::from([("tui", Some(reading(120_000.0, false)))])
                })
            },
        );
        assert_eq!(calls.iter().filter(|leg| **leg == 0).count(), 5);
        assert_eq!(
            calls.iter().filter(|leg| **leg == 1).count(),
            3,
            "the warm-up and two measured rounds"
        );
        assert_eq!(paired.runs["a"].rounds["scan"], [done(5.0); 4]);
        assert_eq!(
            paired.runs["a"].rounds["tui"],
            [
                unfinished(120_000.0),
                unfinished(120_000.0),
                Round::Skipped,
                Round::Skipped,
            ]
        );
    }

    #[test]
    fn the_legs_of_a_turn_run_back_to_back_so_that_both_are_measured_in_one_round() {
        let names = labelled(&["a", "b"]);
        let legs: &[&[&str]] = &[&["scan"], &["tui"]];
        let mut calls = Vec::new();
        let paired = paired_rounds(
            &names,
            legs,
            2,
            None,
            || None,
            |version, leg| {
                calls.push((version, leg));
                let ms = f64::from(u32::try_from(version * 10 + leg).expect("small"));
                Ok(BTreeMap::from([(legs[leg][0], Some(reading(ms, true)))]))
            },
        );
        assert_eq!(
            calls,
            [
                // The warm-up round.
                (0, 0),
                (0, 1),
                (1, 0),
                (1, 1),
                // Round 0, then round 1 with the other version first.
                (0, 0),
                (0, 1),
                (1, 0),
                (1, 1),
                (1, 0),
                (1, 1),
                (0, 0),
                (0, 1),
            ]
        );
        assert_eq!(paired.runs["a"].rounds["scan"], [done(0.0); 2]);
        assert_eq!(paired.runs["b"].rounds["tui"], [done(11.0); 2]);
    }

    #[test]
    fn a_leg_whose_run_cannot_be_carried_out_stops_alone() {
        let names = labelled(&["a"]);
        let legs: &[&[&str]] = &[&["scan", "write"], &["tui"]];
        let paired = paired_rounds(
            &names,
            legs,
            2,
            None,
            || None,
            |_, leg| {
                if leg == 0 {
                    Err("cannot scan".to_owned())
                } else {
                    Ok(BTreeMap::from([("tui", Some(reading(7.0, true)))]))
                }
            },
        );
        let runs = &paired.runs["a"];
        assert_eq!(runs.errors["scan"], "cannot scan");
        assert_eq!(runs.errors["write"], "cannot scan", "every key of the leg");
        assert!(!runs.errors.contains_key("tui"));
        assert_eq!(runs.rounds["scan"], [Round::Failed; 2]);
        assert_eq!(runs.rounds["write"], [Round::Failed; 2]);
        assert_eq!(runs.rounds["tui"], [done(7.0); 2]);
        assert_eq!(paired.order, [labelled(&["a"]), labelled(&["a"])]);
    }

    #[test]
    fn a_leg_whose_run_leaves_out_a_key_is_an_error_rather_than_a_shorter_series() {
        let names = labelled(&["a"]);
        let legs: &[&[&str]] = &[&["scan", "write"]];
        let paired = paired_rounds(
            &names,
            legs,
            1,
            None,
            || None,
            |_, _| Ok(BTreeMap::from([("scan", Some(reading(7.0, true)))])),
        );
        let runs = &paired.runs["a"];
        assert_eq!(runs.errors["scan"], "the run measured no `write`");
        assert_eq!(runs.rounds["write"], [Round::Failed]);
    }

    #[test]
    fn completion_is_kept_for_each_metric_of_a_run_and_any_unfinished_one_counts_as_out_of_time() {
        let names = labelled(&["a"]);
        let legs: &[&[&str]] = &[&["wall", "write"]];
        let paired = paired_rounds(
            &names,
            legs,
            3,
            Some(2),
            || None,
            |_, _| {
                Ok(BTreeMap::from([
                    ("wall", Some(reading(50.0, true))),
                    ("write", Some(reading(0.0, false))),
                ]))
            },
        );
        let runs = &paired.runs["a"];
        assert_eq!(runs.rounds["wall"][0], done(50.0), "finished on its own");
        assert_eq!(runs.rounds["write"][0], unfinished(0.0));
        assert_eq!(
            runs.rounds["wall"][2],
            Round::Skipped,
            "a run with any unfinished reading ran out of time: two in a row gives up the leg"
        );
        let wall = series_of("wall", &names, &paired, 1, &[]);
        let write = series_of("write", &names, &paired, 1, &[]);
        assert_eq!(wall[0].completed, [true; 2]);
        assert_eq!(write[0].completed, [false; 2]);
        assert_eq!(wall[0].median, Some(50.0));
        assert_eq!(write[0].median, None);
    }

    #[test]
    fn a_ratio_pairs_rounds_by_their_number_and_never_by_position() {
        // The numerator has no sample in round 1; positions 0, 1, 2 of its samples are rounds 0, 2,
        // 3, so pairing by position would divide round 2's 30 by round 1's 2.
        let numerator = [done(10.0), Round::Skipped, done(30.0), done(40.0)];
        let denominator = [done(1.0), done(2.0), done(3.0), done(4.0)];

        let ratio = ratio_between(&numerator, &denominator, 5).expect("three rounds have a ratio");

        assert_eq!(ratio.pairs, 3);
        assert!(near(ratio.median.expect("a median"), 10.0), "{ratio:?}");
        let interval = ratio.bootstrap_ci.expect("an interval");
        assert!(near(interval.lower, 10.0) && near(interval.upper, 10.0));
        assert!(ratio.lower_bounds.is_empty() && ratio.upper_bounds.is_empty());
        assert_eq!(ratio.both_censored, 0);
    }

    #[test]
    fn a_ratio_rests_on_the_rounds_both_sides_finished_and_keeps_the_others_as_bounds() {
        let numerator = [
            done(10.0),
            unfinished(1_000.0),
            done(20.0),
            unfinished(1_000.0),
            unfinished(1_000.0),
            Round::Failed,
        ];
        let denominator = [
            done(5.0),
            done(100.0),
            unfinished(1_000.0),
            unfinished(1_000.0),
            done(40.0),
            done(1.0),
        ];

        let ratio = ratio_between(&numerator, &denominator, 5).expect("a ratio");

        assert_eq!(ratio.pairs, 1, "only round 0 finished on both sides");
        assert!(near(ratio.median.expect("a median"), 2.0), "{ratio:?}");
        assert_eq!(
            ratio.lower_bounds.len(),
            2,
            "rounds 1 and 4: the numerator ran out of time, the denominator finished"
        );
        assert!(near(ratio.lower_bounds[0], 10.0), "{ratio:?}");
        assert!(near(ratio.lower_bounds[1], 25.0), "{ratio:?}");
        assert_eq!(
            ratio.upper_bounds.len(),
            1,
            "round 2: the denominator ran out of time, the numerator finished"
        );
        assert!(near(ratio.upper_bounds[0], 0.02), "{ratio:?}");
        assert_eq!(
            ratio.both_censored, 1,
            "round 3: neither finished, which says nothing"
        );
    }

    #[test]
    fn the_median_and_the_interval_do_not_move_when_a_round_does_not_finish() {
        let numerator = [done(10.0), done(12.0), done(11.0), done(13.0)];
        let denominator = [done(10.0), done(10.0), done(10.0), done(10.0)];
        let plain = ratio_between(&numerator, &denominator, 9).expect("a ratio");

        let with_more = ratio_between(
            &[numerator.to_vec(), vec![unfinished(9_000.0); 3]].concat(),
            &[denominator.to_vec(), vec![done(10.0); 3]].concat(),
            9,
        )
        .expect("a ratio");

        assert_eq!(with_more.median, plain.median);
        assert_eq!(with_more.bootstrap_ci, plain.bootstrap_ci);
        assert_eq!(with_more.pairs, plain.pairs);
        assert_eq!(with_more.lower_bounds, [900.0; 3]);
    }

    #[test]
    fn two_runs_that_ran_out_of_time_in_one_round_are_counted_and_never_made_a_ratio_of_one() {
        let numerator = [unfinished(120_000.0); 4];
        let denominator = [unfinished(120_000.0); 4];

        let ratio = ratio_between(&numerator, &denominator, 1).expect("a count");

        assert_eq!(ratio.pairs, 0);
        assert_eq!(ratio.median, None, "not 1.0");
        assert_eq!(ratio.bootstrap_ci, None);
        assert!(ratio.lower_bounds.is_empty() && ratio.upper_bounds.is_empty());
        assert_eq!(ratio.both_censored, 4);
    }

    #[test]
    fn a_ratio_needs_a_round_both_sides_have_a_sample_in_and_a_denominator_that_is_not_zero() {
        assert!(ratio_between(&[done(1.0)], &[Round::Skipped], 1).is_none());
        assert!(ratio_between(&[Round::Failed], &[done(1.0)], 1).is_none());
        assert!(ratio_between(&[], &[], 1).is_none());
        assert!(ratio_between(&[done(1.0), done(2.0)], &[done(0.0), done(0.0)], 1).is_none());
        assert!(
            ratio_between(&[unfinished(9.0)], &[done(0.0)], 1).is_none(),
            "no bound can be put on a ratio over zero"
        );
        let ratio = ratio_between(&[done(1.0), done(4.0)], &[done(0.0), done(2.0)], 1)
            .expect("one round has a ratio");
        assert_eq!(ratio.pairs, 1);
        assert!(near(ratio.median.expect("a median"), 2.0));
    }

    #[test]
    fn a_version_is_compared_with_the_candidate_and_with_du_round_by_round() {
        let names = labelled(&["old", "new"]);
        let paired = paired_of(
            &[
                ("old", vec![done(100.0), done(200.0), done(300.0)]),
                ("new", vec![done(10.0), done(20.0), done(30.0)]),
            ],
            vec![Some(10.0); 3],
        );

        let series = series_of("wall", &names, &paired, 7, &[("du", Against::Du)]);

        assert_eq!(series.len(), 2);
        let old = &series[0];
        assert_eq!(old.reference, "old");
        assert_eq!(old.rounds, [0, 1, 2]);
        assert_eq!(old.samples, [100.0, 200.0, 300.0]);
        assert_eq!(old.completed, [true; 3]);
        assert_eq!(old.skipped, 0);
        assert_eq!(old.median, Some(200.0));
        let to_candidate = &old.ratios["candidate"];
        assert!(near(to_candidate.median.expect("a median"), 10.0));
        let interval = to_candidate.bootstrap_ci.expect("an interval");
        assert!(interval.lower <= 10.0 && interval.upper >= 10.0);
        assert_eq!(to_candidate.pairs, 3);
        assert!(near(old.ratios["du"].median.expect("a median"), 20.0));
        let new = &series[1];
        assert!(near(new.ratios["candidate"].median.expect("a median"), 1.0));

        let without_du = series_of("wall", &names, &paired, 7, &[]);
        assert!(!without_du[0].ratios.contains_key("du"));
    }

    #[test]
    fn a_round_in_which_du_could_not_be_timed_is_left_out_of_the_du_ratio_not_shifted_over() {
        let names = labelled(&["only"]);
        let paired = paired_of(
            &[("only", vec![done(120.0), done(100.0), done(110.0)])],
            vec![Some(40.0), None, Some(50.0)],
        );

        let series = series_of("wall", &names, &paired, 7, &[("du", Against::Du)]);

        let to_du = &series[0].ratios["du"];
        assert_eq!(to_du.pairs, 2, "rounds 0 and 2");
        // 120/40 = 3.0 and 110/50 = 2.2; by position, 100 would have met 50 and given 2.5.
        assert!(near(to_du.median.expect("a median"), 2.6), "{to_du:?}");
    }

    #[test]
    fn a_series_is_held_against_another_of_the_same_version_in_the_same_rounds() {
        let names = labelled(&["a", "b"]);
        let mut paired = paired_of(
            &[
                ("a", vec![done(40.0), done(60.0), Round::Skipped]),
                ("b", vec![done(10.0), done(10.0), done(10.0)]),
            ],
            Vec::new(),
        );
        for (name, slots) in [
            ("a", vec![done(20.0), Round::Skipped, done(30.0)]),
            ("b", vec![done(5.0), done(5.0), done(5.0)]),
        ] {
            paired
                .runs
                .get_mut(name)
                .expect("a version")
                .rounds
                .insert("headless", slots);
        }

        let series = series_of(
            "wall",
            &names,
            &paired,
            7,
            &[("headless", Against::Own("headless"))],
        );

        let own = &series[0].ratios["headless"];
        assert_eq!(own.pairs, 1, "only round 0 has a sample on both sides");
        assert!(near(own.median.expect("a median"), 2.0), "{own:?}");
        assert!(near(
            series[1].ratios["headless"].median.expect("a median"),
            2.0
        ));
        assert_eq!(series[1].ratios["headless"].pairs, 3);
    }

    #[test]
    fn a_candidate_that_is_given_up_on_leaves_the_rounds_it_skipped_out_of_every_ratio() {
        let names = labelled(&["old", "new"]);
        let paired = paired_rounds(
            &names,
            ONE_LEG,
            5,
            Some(2),
            || None,
            |version, _| {
                Ok(if version == 1 {
                    out_of_time(500.0)
                } else {
                    finished(50.0)
                })
            },
        );

        let series = series_of("wall", &names, &paired, 7, &[]);

        let old = &series[0];
        assert_eq!(
            old.samples.len(),
            5,
            "the old version finished in every round"
        );
        let to_candidate = &old.ratios["candidate"];
        assert_eq!(to_candidate.pairs, 0);
        assert_eq!(
            to_candidate.upper_bounds.len(),
            2,
            "only the two rounds the candidate ran in count, not the three it skipped"
        );
        assert!(
            to_candidate
                .upper_bounds
                .iter()
                .all(|bound| near(*bound, 0.1))
        );
        assert_eq!(series[1].skipped, 3);
    }

    #[test]
    fn a_run_with_no_usable_reading_of_a_series_leaves_that_round_of_it_without_a_sample() {
        let names = labelled(&["a"]);
        let legs: &[&[&str]] = &[&["scan", "split"]];
        let mut calls = 0;
        let paired = paired_rounds(
            &names,
            legs,
            3,
            Some(2),
            || None,
            |_, _| {
                calls += 1;
                // The warm-up is the first call, so the second measured round is the third.
                Ok(BTreeMap::from([
                    ("scan", Some(reading(100.0, true))),
                    ("split", (calls != 3).then_some(reading(40.0, true))),
                ]))
            },
        );

        let runs = &paired.runs["a"];
        assert_eq!(runs.rounds["scan"], [done(100.0), done(100.0), done(100.0)]);
        assert_eq!(
            runs.rounds["split"],
            [done(40.0), Round::Failed, done(40.0)]
        );
        assert!(
            runs.errors.is_empty(),
            "a reading that is not usable is no error"
        );
        assert_eq!(paired.order, vec![vec!["a".to_owned()]; 3]);
        let split = series_of("split", &names, &paired, 1, &[]);
        assert_eq!(split[0].rounds, [0, 2]);
        assert_eq!(split[0].samples, [40.0, 40.0]);
        assert_eq!(split[0].skipped, 0);
    }

    #[test]
    fn a_series_without_a_usable_reading_is_not_a_run_that_ran_out_of_time() {
        let names = labelled(&["a"]);
        let legs: &[&[&str]] = &[&["scan", "split"]];
        let paired = paired_rounds(
            &names,
            legs,
            4,
            Some(1),
            || None,
            |_, _| {
                Ok(BTreeMap::from([
                    ("scan", Some(reading(100.0, true))),
                    ("split", None),
                ]))
            },
        );
        let runs = &paired.runs["a"];
        assert!(
            runs.rounds["scan"]
                .iter()
                .all(|slot| matches!(slot, Round::Measured(_))),
            "a leg whose runs finish is never given up on"
        );
        assert!(
            runs.rounds["split"]
                .iter()
                .all(|slot| *slot == Round::Failed)
        );

        let censored = paired_rounds(
            &names,
            legs,
            3,
            Some(1),
            || None,
            |_, _| {
                Ok(BTreeMap::from([
                    ("scan", Some(reading(9000.0, false))),
                    ("split", None),
                ]))
            },
        );
        assert_eq!(
            censored.runs["a"].rounds["scan"],
            [unfinished(9000.0), Round::Skipped, Round::Skipped],
            "an unfinished reading still counts, beside one that is not usable"
        );
    }
}
