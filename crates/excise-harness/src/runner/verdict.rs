//! The verdict of a run: how strict xfail combines with what happened.

use crate::{report::Verdict, scenario::Expect};

/// What happened when the harness ran a scenario.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Every step and every final check held.
    Passed,
    /// A step failed.
    Failed,
    /// The harness could not run the scenario: a fixture, spawn, isolation, or protocol failure.
    Errored,
}

/// The verdict for a scenario that expected `expect` and ended in `outcome`.
///
/// This is strict xfail. An expected failure that fails is `xfail`; one that passes is `xpass`,
/// which fails the run so that the change that fixed the defect must flip the scenario to
/// `expect = "pass"`. A scenario the harness could not run is `error` whatever it expected: an
/// expected failure must never absorb a harness problem, or a broken harness would look like a
/// documented defect.
#[must_use]
pub const fn verdict(expect: Expect, outcome: Outcome) -> Verdict {
    match outcome {
        Outcome::Passed => Verdict::resolve(expect, true),
        Outcome::Failed => Verdict::resolve(expect, false),
        Outcome::Errored => Verdict::Error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_xfail_maps_every_combination() {
        let table = [
            (Expect::Pass, Outcome::Passed, Verdict::Pass, false),
            (Expect::Pass, Outcome::Failed, Verdict::Fail, true),
            (Expect::Pass, Outcome::Errored, Verdict::Error, true),
            (Expect::Fail, Outcome::Failed, Verdict::Xfail, false),
            (Expect::Fail, Outcome::Passed, Verdict::Xpass, true),
            (Expect::Fail, Outcome::Errored, Verdict::Error, true),
        ];

        for (expect, outcome, expected, blocks) in table {
            let got = verdict(expect, outcome);
            assert_eq!(got, expected, "{expect} + {outcome:?}");
            assert_eq!(got.blocks_run(), blocks, "{expect} + {outcome:?}");
        }
    }
}
