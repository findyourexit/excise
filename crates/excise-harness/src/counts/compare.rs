//! Comparing the counts of a pull request with the record of its base commit.
//!
//! Counts do not depend on timing or load, so a difference between two documents is always real
//! and there is no noise to allow for. What a threshold decides is which differences a reviewer
//! is told about: a cost that moves by more than [`FLAG_PERCENT`] of its base value is flagged,
//! as is any change in a count of the fixture itself, which is not a cost at all but a sign that
//! the fixture or the accounting changed. The arithmetic is on whole numbers, so a value that is
//! exactly on the threshold is never flagged and never rounds across it.

use std::cmp::Ordering;

use crate::{report::HarnessCounts, scenario::Profile};

use super::metric;

/// A cost is flagged when it moves by more than this many percent of its base value.
pub const FLAG_PERCENT: u64 = 5;

/// What a difference in a count means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// What the build costs: less is better, and a large move is flagged.
    Cost,
    /// A property of the fixture and its accounting, not of the build's cost: it should not
    /// move, so any change is flagged.
    Fixture,
}

/// How a count is shown and read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Known {
    /// The name in a document.
    pub name: &'static str,
    /// The name in a table.
    pub label: &'static str,
    /// What a difference means.
    pub kind: Kind,
}

/// The counts the suite takes, in the order a table lists them.
pub const KNOWN: [Known; 3] = [
    Known {
        name: metric::ENTRIES,
        label: "Entries",
        kind: Kind::Fixture,
    },
    Known {
        name: metric::SCAN_STORE_BYTES,
        label: "Scan-store bytes",
        kind: Kind::Cost,
    },
    Known {
        name: metric::RESIDUE_FILES,
        label: "Residue files",
        kind: Kind::Cost,
    },
];

/// The kind of the count `name`: a count this build does not know is a cost.
#[must_use]
pub fn kind_of(name: &str) -> Kind {
    KNOWN
        .iter()
        .find(|known| known.name == name)
        .map_or(Kind::Cost, |known| known.kind)
}

/// Whether `head` differs from `base` by more than `percent` percent of `base`: any difference
/// from a base of zero is more than that.
#[must_use]
pub fn exceeds(base: u64, head: u64, percent: u64) -> bool {
    u128::from(base.abs_diff(head)) * 100 > u128::from(base) * u128::from(percent)
}

/// Why a count is flagged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flag {
    /// It is not.
    None,
    /// A cost grew by more than [`FLAG_PERCENT`].
    Worse,
    /// A cost shrank by more than [`FLAG_PERCENT`].
    Better,
    /// A count of the fixture changed.
    Unexpected,
}

/// What to say of the move from `base` to `head` in a count of `kind`.
#[must_use]
pub fn flag(kind: Kind, base: u64, head: u64) -> Flag {
    if base == head {
        return Flag::None;
    }
    match kind {
        Kind::Fixture => Flag::Unexpected,
        Kind::Cost if !exceeds(base, head, FLAG_PERCENT) => Flag::None,
        Kind::Cost => match head.cmp(&base) {
            Ordering::Greater => Flag::Worse,
            _ => Flag::Better,
        },
    }
}

/// One count of one case, on both sides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// The fixture.
    pub fixture: String,
    /// The profile it was counted under.
    pub profile: Profile,
    /// The metric name.
    pub metric: String,
    /// The count at the base, or `None` where there is no base or it did not count this.
    pub base: Option<u64>,
    /// The count at the head, or `None` where the head did not count this.
    pub head: Option<u64>,
    /// Whether the move is flagged. Only a count on both sides can be.
    pub flag: Flag,
}

/// Why a case has no rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skipped {
    /// The fixture is not the one the base counted (its hash differs), so no count of it can be
    /// compared.
    FixtureChanged,
    /// The head counts a case that the base did not.
    NewCase,
    /// The base counted a case that the head did not.
    DroppedCase,
}

/// A case that was not compared, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotCompared {
    /// The fixture.
    pub fixture: String,
    /// The profile.
    pub profile: Profile,
    /// Why.
    pub reason: Skipped,
}

/// The counts of a head set against a base set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comparison {
    /// One row per count, in the head's order of cases and the table's order of counts.
    pub rows: Vec<Row>,
    /// The cases that have no rows.
    pub not_compared: Vec<NotCompared>,
    /// Whether there was a base to compare with.
    pub has_base: bool,
}

impl Comparison {
    /// How many counts are on both sides.
    #[must_use]
    pub fn compared(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| row.base.is_some() && row.head.is_some())
            .count()
    }

    /// How many counts are on both sides and differ.
    #[must_use]
    pub fn changed(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| matches!((row.base, row.head), (Some(base), Some(head)) if base != head))
            .count()
    }

    /// How many counts are flagged.
    #[must_use]
    pub fn flagged(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| row.flag != Flag::None)
            .count()
    }
}

/// The position of `name` in a table: the known counts in their order, then the rest by name.
fn order(name: &str) -> (usize, &str) {
    (
        KNOWN
            .iter()
            .position(|known| known.name == name)
            .unwrap_or(KNOWN.len()),
        name,
    )
}

/// Compares `head` with `base`, or lists the counts of `head` alone where there is no base.
///
/// A case is compared only with the same fixture under the same profile, and only while the
/// fixture's hash is the same on both sides: a fixture that changed is a different thing to count.
#[must_use]
pub fn compare(base: Option<&HarnessCounts>, head: &HarnessCounts) -> Comparison {
    let mut rows = Vec::new();
    let mut not_compared = Vec::new();
    for case in &head.cases {
        let counterpart = base.and_then(|base| base.case(&case.fixture.id, case.profile));
        let compare_with = match (base, counterpart) {
            (Some(_), None) => {
                not_compared.push(NotCompared {
                    fixture: case.fixture.id.clone(),
                    profile: case.profile,
                    reason: Skipped::NewCase,
                });
                continue;
            }
            (_, Some(earlier)) if earlier.fixture.hash != case.fixture.hash => {
                not_compared.push(NotCompared {
                    fixture: case.fixture.id.clone(),
                    profile: case.profile,
                    reason: Skipped::FixtureChanged,
                });
                continue;
            }
            (_, earlier) => earlier,
        };
        let mut names: Vec<&String> = case
            .metrics
            .keys()
            .chain(
                compare_with
                    .iter()
                    .flat_map(|earlier| earlier.metrics.keys()),
            )
            .collect();
        names.sort_by(|left, right| order(left).cmp(&order(right)));
        names.dedup();
        for name in names {
            let head_count = case.metrics.get(name).copied();
            let base_count = compare_with.and_then(|earlier| earlier.metrics.get(name).copied());
            rows.push(Row {
                fixture: case.fixture.id.clone(),
                profile: case.profile,
                metric: name.clone(),
                base: base_count,
                head: head_count,
                flag: match (base_count, head_count) {
                    (Some(base), Some(head)) => flag(kind_of(name), base, head),
                    _ => Flag::None,
                },
            });
        }
    }
    if let Some(base) = base {
        for case in &base.cases {
            if head.case(&case.fixture.id, case.profile).is_none() {
                not_compared.push(NotCompared {
                    fixture: case.fixture.id.clone(),
                    profile: case.profile,
                    reason: Skipped::DroppedCase,
                });
            }
        }
    }
    Comparison {
        rows,
        not_compared,
        has_base: base.is_some(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::counts::test_support::{HASH, OTHER_HASH, case, record, suite};

    fn row<'a>(comparison: &'a Comparison, fixture: &str, metric: &str) -> &'a Row {
        comparison
            .rows
            .iter()
            .find(|row| row.fixture == fixture && row.metric == metric)
            .unwrap_or_else(|| panic!("no row for {fixture} {metric}: {comparison:#?}"))
    }

    #[test]
    fn a_count_is_over_the_threshold_only_beyond_five_percent_of_the_base() {
        // Exactly 5% is not beyond it, in either direction; one more is.
        assert!(!exceeds(1_000, 1_050, 5));
        assert!(exceeds(1_000, 1_051, 5));
        assert!(!exceeds(1_000, 950, 5));
        assert!(exceeds(1_000, 949, 5));
        assert!(!exceeds(1_000, 1_000, 5));
        // Any move from nothing is beyond it, and nothing to nothing is not.
        assert!(exceeds(0, 1, 5));
        assert!(!exceeds(0, 0, 5));
    }

    #[test]
    fn the_threshold_arithmetic_cannot_overflow_at_the_largest_counts() {
        let largest = crate::report::MAX_COUNT;

        assert!(!exceeds(largest, largest - 1, 5));
        assert!(exceeds(largest / 2, largest, 5));
        assert!(
            exceeds(u64::MAX, 0, 5),
            "even u64::MAX cannot overflow the products"
        );
    }

    #[test]
    fn a_cost_is_flagged_by_the_direction_it_moves_and_the_fixture_by_any_move() {
        assert_eq!(flag(Kind::Cost, 1_000, 1_000), Flag::None);
        assert_eq!(flag(Kind::Cost, 1_000, 1_050), Flag::None);
        assert_eq!(flag(Kind::Cost, 1_000, 1_051), Flag::Worse);
        assert_eq!(flag(Kind::Cost, 1_000, 949), Flag::Better);
        assert_eq!(flag(Kind::Cost, 1_000, 999), Flag::None);
        assert_eq!(
            flag(Kind::Cost, 0, 3),
            Flag::Worse,
            "a first file left behind is a regression"
        );
        assert_eq!(flag(Kind::Fixture, 1_002, 1_002), Flag::None);
        assert_eq!(flag(Kind::Fixture, 1_002, 1_003), Flag::Unexpected);
        assert_eq!(flag(Kind::Fixture, 1_002, 1), Flag::Unexpected);
    }

    #[test]
    fn the_known_counts_are_kinds_and_an_unknown_one_is_a_cost() {
        assert_eq!(kind_of("entries"), Kind::Fixture);
        assert_eq!(kind_of("scan_store_bytes"), Kind::Cost);
        assert_eq!(kind_of("residue_files"), Kind::Cost);
        assert_eq!(kind_of("a_count_this_build_does_not_know"), Kind::Cost);
    }

    #[test]
    fn identical_counts_have_no_changes_and_no_flags() {
        let base = record(&"a".repeat(40), suite(350));
        let head = record(&"b".repeat(40), suite(350));

        let comparison = compare(Some(&base), &head);

        assert!(comparison.has_base);
        assert_eq!(comparison.compared(), 6);
        assert_eq!(comparison.changed(), 0);
        assert_eq!(comparison.flagged(), 0);
        assert!(comparison.not_compared.is_empty());
    }

    #[test]
    fn deltas_are_computed_per_count_and_the_threshold_flags_only_the_large_ones() {
        let mut base = record(&"a".repeat(40), suite(1_000));
        base.cases[1].metrics.insert("residue_files".to_owned(), 10);
        let mut head = record(&"b".repeat(40), suite(1_000));
        // +4.9%: changed, not flagged. +12%: flagged worse. -40%: flagged better.
        head.cases[0]
            .metrics
            .insert("scan_store_bytes".to_owned(), 1_049);
        head.cases[1]
            .metrics
            .insert("scan_store_bytes".to_owned(), 49_000 * 112 / 100);
        head.cases[1].metrics.insert("residue_files".to_owned(), 6);

        let comparison = compare(Some(&base), &head);

        let small = row(&comparison, "wide-1k", "scan_store_bytes");
        assert_eq!(
            (small.base, small.head, small.flag),
            (Some(1_000), Some(1_049), Flag::None)
        );
        let worse = row(&comparison, "tiny-files-50k", "scan_store_bytes");
        assert_eq!(worse.flag, Flag::Worse);
        let better = row(&comparison, "tiny-files-50k", "residue_files");
        assert_eq!(
            (better.base, better.head, better.flag),
            (Some(10), Some(6), Flag::Better)
        );
        assert_eq!(comparison.changed(), 3);
        assert_eq!(comparison.flagged(), 2);
    }

    #[test]
    fn a_change_in_the_entries_of_a_fixture_is_flagged_whatever_its_size() {
        let base = record(&"a".repeat(40), suite(1_000));
        let mut head = record(&"b".repeat(40), suite(1_000));
        head.cases[0].metrics.insert("entries".to_owned(), 1_003);

        let comparison = compare(Some(&base), &head);

        assert_eq!(
            row(&comparison, "wide-1k", "entries").flag,
            Flag::Unexpected
        );
        assert_eq!(comparison.flagged(), 1);
    }

    #[test]
    fn rows_follow_the_head_cases_and_the_tables_order_of_counts() {
        let base = record(
            &"a".repeat(40),
            vec![case("wide-1k", HASH, &[("zzz_old", 1), ("entries", 5)])],
        );
        let head = record(
            &"b".repeat(40),
            vec![
                case(
                    "wide-1k",
                    HASH,
                    &[
                        ("scan_store_bytes", 9),
                        ("aaa_new", 2),
                        ("residue_files", 3),
                        ("entries", 5),
                    ],
                ),
                case("node-modules-2k", OTHER_HASH, &[("entries", 7)]),
            ],
        );

        let comparison = compare(Some(&base), &head);

        let order: Vec<(&str, &str)> = comparison
            .rows
            .iter()
            .map(|row| (row.fixture.as_str(), row.metric.as_str()))
            .collect();
        assert_eq!(
            order,
            [
                ("wide-1k", "entries"),
                ("wide-1k", "scan_store_bytes"),
                ("wide-1k", "residue_files"),
                ("wide-1k", "aaa_new"),
                ("wide-1k", "zzz_old"),
            ],
            "the known counts in the table's order, then the rest by name"
        );
    }

    #[test]
    fn a_count_on_one_side_only_is_listed_and_never_flagged() {
        let base = record(
            &"a".repeat(40),
            vec![case(
                "wide-1k",
                HASH,
                &[("entries", 5), ("residue_files", 9)],
            )],
        );
        let head = record(
            &"b".repeat(40),
            vec![case("wide-1k", HASH, &[("entries", 5), ("a_new_count", 9)])],
        );

        let comparison = compare(Some(&base), &head);

        let dropped = row(&comparison, "wide-1k", "residue_files");
        assert_eq!(
            (dropped.base, dropped.head, dropped.flag),
            (Some(9), None, Flag::None)
        );
        let added = row(&comparison, "wide-1k", "a_new_count");
        assert_eq!(
            (added.base, added.head, added.flag),
            (None, Some(9), Flag::None)
        );
        assert_eq!(comparison.compared(), 1);
    }

    #[test]
    fn a_fixture_that_changed_is_not_compared_and_has_no_rows() {
        let base = record(
            &"a".repeat(40),
            vec![case("wide-1k", HASH, &[("scan_store_bytes", 100)])],
        );
        let head = record(
            &"b".repeat(40),
            vec![case("wide-1k", OTHER_HASH, &[("scan_store_bytes", 900)])],
        );

        let comparison = compare(Some(&base), &head);

        assert!(
            comparison.rows.is_empty(),
            "nothing of a different tree is compared"
        );
        assert_eq!(
            comparison.not_compared,
            [NotCompared {
                fixture: "wide-1k".to_owned(),
                profile: Profile::Deterministic,
                reason: Skipped::FixtureChanged,
            }]
        );
    }

    #[test]
    fn a_case_on_one_side_only_is_reported_and_a_profile_is_part_of_the_case() {
        let base = record(&"a".repeat(40), vec![case("gone", HASH, &[("entries", 1)])]);
        let mut other_profile = case("wide-1k", HASH, &[("entries", 1)]);
        other_profile.profile = Profile::Default;
        let head = record(
            &"b".repeat(40),
            vec![case("wide-1k", HASH, &[("entries", 1)]), other_profile],
        );

        let comparison = compare(Some(&base), &head);

        assert!(comparison.rows.is_empty());
        let reasons: Vec<(&str, Skipped)> = comparison
            .not_compared
            .iter()
            .map(|skipped| (skipped.fixture.as_str(), skipped.reason))
            .collect();
        assert_eq!(
            reasons,
            [
                ("wide-1k", Skipped::NewCase),
                ("wide-1k", Skipped::NewCase),
                ("gone", Skipped::DroppedCase)
            ]
        );
    }

    #[test]
    fn without_a_base_every_count_of_the_head_is_listed_unflagged() {
        let head = record(&"b".repeat(40), suite(350));

        let comparison = compare(None, &head);

        assert!(!comparison.has_base);
        assert_eq!(comparison.rows.len(), 6);
        assert!(
            comparison
                .rows
                .iter()
                .all(|row| row.base.is_none() && row.flag == Flag::None)
        );
        assert_eq!(comparison.compared(), 0);
        assert!(comparison.not_compared.is_empty());
    }
}
