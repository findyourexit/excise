//! Histograms in sparse buckets, shared by the shape profile and the shaped fixture part.
//!
//! A histogram is a map from the smallest value of a bucket to the number of values in it. Two
//! kinds of bucket exist, and the field that holds a histogram says which one it is:
//!
//! * **Classes**: powers of two. The bucket `0` holds the value 0, the bucket `1` the value 1, the
//!   bucket `2` the values 2 and 3, the bucket `4` the values 4 to 7, and so on up to the bucket
//!   2^63, which holds everything from 2^63 to `u64::MAX`. A class says how large a value is, within
//!   a factor of two, and the histogram stays at most 65 buckets long whatever it counts: the
//!   children of a directory, the size of a file, the names of a hard-linked file.
//! * **Lengths**: one bucket per value, `1..=255`. The lengths of names in bytes, which are short
//!   and bounded by `NAME_MAX`, are kept exactly.
//!
//! [`spread`] turns a histogram back into values, and [`apportion`] splits a total in proportion
//! to weights. Both are integer arithmetic only, so what they return is the same on every machine.

use std::{collections::BTreeMap, fmt};

use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{self, MapAccess, Visitor},
};

/// The longest name, in bytes, a length histogram has a bucket for: `NAME_MAX` on every file
/// system the harness targets.
pub const MAX_NAME_LENGTH: u64 = 255;

/// The smallest value of the class that holds `value`: 0, or the largest power of two that is at
/// most `value`.
#[must_use]
pub const fn class_floor(value: u64) -> u64 {
    match value.checked_ilog2() {
        Some(bit) => 1 << bit,
        None => 0,
    }
}

/// The largest value of the class whose smallest value is `floor`.
#[must_use]
pub const fn class_ceiling(floor: u64) -> u64 {
    if floor == 0 {
        return 0;
    }
    match floor.checked_mul(2) {
        Some(next) => next - 1,
        None => u64::MAX,
    }
}

/// Whether `key` names a class: it is 0 or a power of two.
#[must_use]
pub const fn is_class_floor(key: u64) -> bool {
    key == 0 || key.is_power_of_two()
}

/// What the keys of a [`Buckets`] mean.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BucketKind {
    /// Powers of two: the key is the smallest value of its class.
    Class,
    /// One bucket per value, `1..=`[`MAX_NAME_LENGTH`].
    Length,
}

impl BucketKind {
    /// Whether `key` can name a bucket of this kind.
    #[must_use]
    pub const fn admits(self, key: u64) -> bool {
        match self {
            Self::Class => is_class_floor(key),
            Self::Length => key >= 1 && key <= MAX_NAME_LENGTH,
        }
    }

    /// The smallest and the largest value of the bucket named `key`.
    #[must_use]
    pub const fn bounds(self, key: u64) -> (u64, u64) {
        match self {
            Self::Class => (key, class_ceiling(key)),
            Self::Length => (key, key),
        }
    }
}

/// The buckets of a histogram: the smallest value of each bucket, and how many values fell in it.
///
/// In JSON and TOML the keys are written as strings of digits, the way both formats name the
/// members of a map, and read back from them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Buckets(BTreeMap<u64, u64>);

impl Buckets {
    /// A histogram with no values.
    #[must_use]
    pub const fn new() -> Self {
        Self(BTreeMap::new())
    }

    /// Counts `count` more values in the bucket `key`.
    pub fn add(&mut self, key: u64, count: u64) {
        let slot = self.0.entry(key).or_default();
        *slot = slot.saturating_add(count);
    }

    /// Takes `count` values out of the bucket `key`, and the bucket with them when none is left.
    pub fn take(&mut self, key: u64, count: u64) {
        if let Some(slot) = self.0.get_mut(&key) {
            *slot = slot.saturating_sub(count);
            if *slot == 0 {
                self.0.remove(&key);
            }
        }
    }

    /// The buckets in ascending order of their keys, with their counts.
    pub fn iter(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.0.iter().map(|(key, count)| (*key, *count))
    }

    /// Whether no value is counted anywhere.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The number of buckets.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// The count of the bucket `key`, 0 when there is none.
    #[must_use]
    pub fn get(&self, key: u64) -> u64 {
        self.0.get(&key).copied().unwrap_or(0)
    }

    /// The largest key.
    #[must_use]
    pub fn last_key(&self) -> Option<u64> {
        self.0.last_key_value().map(|(key, _)| *key)
    }

    /// How many values the histogram counts in all.
    #[must_use]
    pub fn count(&self) -> u128 {
        self.0.values().map(|count| u128::from(*count)).sum()
    }

    /// The buckets of `self` restricted to the keys that satisfy `keep`.
    #[must_use]
    pub fn filtered(&self, keep: impl Fn(u64) -> bool) -> Self {
        Self(
            self.0
                .iter()
                .filter(|(key, _)| keep(**key))
                .map(|(key, count)| (*key, *count))
                .collect(),
        )
    }
}

impl FromIterator<(u64, u64)> for Buckets {
    fn from_iter<T: IntoIterator<Item = (u64, u64)>>(pairs: T) -> Self {
        let mut buckets = Self::new();
        for (key, count) in pairs {
            buckets.add(key, count);
        }
        buckets
    }
}

impl Serialize for Buckets {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_map(self.0.iter())
    }
}

/// The number that the key of a map of buckets names. Keys are written the one way `to_string`
/// writes a number, so that a bucket cannot be named twice, by `7` and `07`.
///
/// # Errors
///
/// Returns what is wrong with `key`.
pub(crate) fn parse_bucket_key(key: &str) -> Result<u64, String> {
    key.parse()
        .ok()
        .filter(|number: &u64| number.to_string() == key)
        .ok_or_else(|| format!("`{key}` is not a bucket number"))
}

impl<'de> Deserialize<'de> for Buckets {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct BucketsVisitor;

        impl<'de> Visitor<'de> for BucketsVisitor {
            type Value = Buckets;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a map from bucket numbers to counts")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Buckets, A::Error> {
                let mut buckets = BTreeMap::new();
                while let Some((key, count)) = map.next_entry::<String, u64>()? {
                    let number = parse_bucket_key(&key).map_err(de::Error::custom)?;
                    if buckets.insert(number, count).is_some() {
                        return Err(de::Error::custom(format!(
                            "the bucket {number} appears twice"
                        )));
                    }
                }
                Ok(Buckets(buckets))
            }
        }

        deserializer.deserialize_map(BucketsVisitor)
    }
}

/// `count` values that follow `buckets`, in ascending order: the value at the middle of each of
/// `count` equal slices of the distribution. A histogram with no values gives zeros.
///
/// Within a bucket the values are spread evenly from its smallest to its largest value. The result
/// depends on nothing but its arguments, so a shape built from it is the same on every machine,
/// and it follows the histogram as closely as `count` values can, with no sampling noise.
#[must_use]
pub fn spread(buckets: &Buckets, count: usize, kind: BucketKind) -> Vec<u64> {
    let total = buckets.count();
    if total == 0 || count == 0 {
        return vec![0; count];
    }
    let steps: Vec<(u64, u64)> = buckets.iter().filter(|(_, size)| *size > 0).collect();
    let slices = count as u128;
    let mut values = Vec::with_capacity(count);
    let mut index = 0;
    let mut before: u128 = 0;
    for slice in 0..slices {
        let rank = (2 * slice + 1) * total / (2 * slices);
        while index + 1 < steps.len() && rank >= before + u128::from(steps[index].1) {
            before += u128::from(steps[index].1);
            index += 1;
        }
        let (key, size) = steps[index];
        let (low, high) = kind.bounds(key);
        let width = u128::from(high - low) + 1;
        let offset = (rank - before) * width / u128::from(size);
        values.push(low + u64::try_from(offset).unwrap_or(high - low));
    }
    values
}

/// How many of the `count` values [`spread`] returns lie in each bucket of `buckets`, as the key of
/// each bucket that holds any and how many, in ascending order of key: the counts of the values
/// without making them, which a spec of ten million files cannot afford to make to be checked. A
/// histogram with no values, or a `count` of 0, has no buckets.
#[must_use]
pub fn spread_counts(buckets: &Buckets, count: u64) -> Vec<(u64, u64)> {
    let total = buckets.count();
    if total == 0 || count == 0 {
        return Vec::new();
    }
    let slices = u128::from(count);
    let mut counts = Vec::new();
    let mut through: u128 = 0;
    let mut placed: u128 = 0;
    for (key, size) in buckets.iter().filter(|(_, size)| *size > 0) {
        through += u128::from(size);
        // `spread` puts slice `s` in this bucket or one before it when its rank, `(2s + 1) * total
        // / (2 * count)` rounded down, is less than `through`: when `(2s + 1) * total` is less than
        // `2 * through * count`, which is so for every odd number below the quotient of the two,
        // and there are half as many of those as the quotient rounded up.
        let upto = ((2 * through * slices).div_ceil(total) / 2).min(slices);
        if upto > placed {
            counts.push((key, u64::try_from(upto - placed).unwrap_or(u64::MAX)));
        }
        placed = upto;
    }
    counts
}

/// Splits `total` among `weights.len()` shares in proportion to the weights, by the method of
/// largest remainders: every share is its exact proportion rounded down, and the units left over
/// go to the largest fractional parts, the earlier share first among equals. The shares add up to
/// `total`. Weights that are all zero split it evenly.
#[must_use]
pub fn apportion(total: u64, weights: &[u64]) -> Vec<u64> {
    let slots = weights.len();
    if slots == 0 {
        return Vec::new();
    }
    let whole: u128 = weights.iter().map(|weight| u128::from(*weight)).sum();
    if whole == 0 {
        let each = total / slots as u64;
        let extra = usize::try_from(total % slots as u64).unwrap_or(0);
        return (0..slots)
            .map(|index| each + u64::from(index < extra))
            .collect();
    }
    let mut shares = Vec::with_capacity(slots);
    let mut remainders = Vec::with_capacity(slots);
    for weight in weights {
        let product = u128::from(total) * u128::from(*weight);
        shares.push(u64::try_from(product / whole).unwrap_or(u64::MAX));
        remainders.push(product % whole);
    }
    let given: u64 = shares.iter().sum();
    let mut leftover = usize::try_from(total - given).unwrap_or(0);
    if leftover > 0 {
        let mut order: Vec<usize> = (0..slots).collect();
        order.sort_unstable_by(|left, right| {
            remainders[*right]
                .cmp(&remainders[*left])
                .then(left.cmp(right))
        });
        for index in order {
            if leftover == 0 {
                break;
            }
            shares[index] += 1;
            leftover -= 1;
        }
    }
    shares
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::names::MAX_NAME_BYTES;

    #[test]
    fn a_class_runs_from_its_floor_to_just_below_the_next_power_of_two() {
        let cases = [
            (0, 0, 0),
            (1, 1, 1),
            (2, 2, 3),
            (3, 2, 3),
            (4, 4, 7),
            (1023, 512, 1023),
            (1024, 1024, 2047),
            (u64::MAX, 1 << 63, u64::MAX),
        ];
        for (value, floor, ceiling) in cases {
            assert_eq!(class_floor(value), floor, "the class of {value}");
            assert_eq!(
                class_ceiling(floor),
                ceiling,
                "the end of the class of {value}"
            );
            assert!(is_class_floor(floor));
        }
        assert!(!is_class_floor(3) && !is_class_floor(6) && !is_class_floor(u64::MAX));
    }

    #[test]
    fn the_longest_name_a_histogram_counts_is_name_max() {
        assert_eq!(MAX_NAME_LENGTH, MAX_NAME_BYTES as u64);
        assert!(BucketKind::Length.admits(1) && BucketKind::Length.admits(MAX_NAME_LENGTH));
        assert!(!BucketKind::Length.admits(0) && !BucketKind::Length.admits(MAX_NAME_LENGTH + 1));
        assert!(BucketKind::Class.admits(0) && BucketKind::Class.admits(64));
        assert!(!BucketKind::Class.admits(65));
    }

    /// A part of a spec: the one place the buckets are read through a tagged enum.
    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    #[serde(tag = "kind")]
    enum Tagged {
        Part { sizes: Buckets },
    }

    #[test]
    fn buckets_round_trip_through_json_and_toml_and_refuse_keys_that_are_not_numbers() {
        let buckets: Buckets = [(0, 3), (4, 5), (1 << 63, 1)].into_iter().collect();

        let json = serde_json::to_string(&buckets).expect("serializes");
        assert_eq!(json, r#"{"0":3,"4":5,"9223372036854775808":1}"#);
        assert_eq!(
            serde_json::from_str::<Buckets>(&json).expect("parses"),
            buckets
        );

        for bad in [
            r#"{"07":1}"#,
            r#"{"+7":1}"#,
            r#"{"x":1}"#,
            r#"{"-1":1}"#,
            r#"{"":1}"#,
        ] {
            assert!(serde_json::from_str::<Buckets>(bad).is_err(), "{bad}");
        }
        let twice = serde_json::from_str::<Buckets>(r#"{"7":1,"7":2}"#);
        assert!(twice.is_err(), "a bucket named twice is refused");

        let part = Tagged::Part { sizes: buckets };
        let text = toml::to_string(&part).expect("TOML writes integer keys as plain keys");
        assert_eq!(
            toml::from_str::<Tagged>(&text).expect("TOML reads them back"),
            part
        );
    }

    #[test]
    fn spread_follows_the_histogram_and_is_ascending() {
        let buckets: Buckets = [(0, 50), (1, 25), (4, 25)].into_iter().collect();

        let values = spread(&buckets, 100, BucketKind::Class);

        assert_eq!(values.len(), 100);
        assert!(values.windows(2).all(|pair| pair[0] <= pair[1]));
        assert_eq!(values.iter().filter(|value| **value == 0).count(), 50);
        assert_eq!(values.iter().filter(|value| **value == 1).count(), 25);
        let wide: Vec<u64> = values.iter().copied().filter(|value| *value >= 4).collect();
        assert_eq!(wide.len(), 25);
        assert!(wide.iter().all(|value| (4..=7).contains(value)));
        assert_eq!(wide.first(), Some(&4));
        assert_eq!(wide.last(), Some(&7));
    }

    #[test]
    fn spread_of_few_values_still_represents_every_heavy_bucket() {
        let buckets: Buckets = [(1, 90), (1024, 10)].into_iter().collect();

        let ten = spread(&buckets, 10, BucketKind::Class);

        assert_eq!(ten.iter().filter(|value| **value == 1).count(), 9);
        assert_eq!(ten.iter().filter(|value| **value >= 1024).count(), 1);
        assert_eq!(spread(&Buckets::new(), 3, BucketKind::Class), [0, 0, 0]);
        assert!(spread(&buckets, 0, BucketKind::Class).is_empty());
        let one = spread(&[(8, 1)].into_iter().collect(), 1, BucketKind::Class);
        assert!((8..=15).contains(&one[0]));
    }

    #[test]
    fn spread_keeps_lengths_exact() {
        let buckets: Buckets = [(3, 2), (9, 2)].into_iter().collect();

        assert_eq!(spread(&buckets, 4, BucketKind::Length), [3, 3, 9, 9]);
        assert_eq!(spread(&buckets, 2, BucketKind::Length), [3, 9]);
    }

    #[test]
    fn apportion_adds_up_follows_the_weights_and_breaks_ties_by_position() {
        assert_eq!(apportion(10, &[1, 1, 1]), [4, 3, 3]);
        assert_eq!(apportion(10, &[0, 0, 0]), [4, 3, 3]);
        assert_eq!(apportion(7, &[0, 3, 1]), [0, 5, 2]);
        assert_eq!(apportion(0, &[5, 5]), [0, 0]);
        assert_eq!(apportion(5, &[]), Vec::<u64>::new());
        let big = apportion(1_000_003, &[u64::MAX / 3, 2 * (u64::MAX / 3), 7]);
        assert_eq!(big.iter().sum::<u64>(), 1_000_003);
        assert!(big[1] > big[0]);
    }

    #[test]
    fn the_counts_of_a_spread_are_what_its_values_make_in_each_bucket() {
        // A repeatable stream of numbers below a limit.
        let mut state: u64 = 0x5eed;
        let mut below = |limit: u64| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 33) % limit
        };
        let classes = [0_u64, 1, 2, 4, 8, 64, 1024, 4096, 1 << 20, 1 << 40];
        for round in 0..600 {
            let mut buckets = Buckets::new();
            for class in classes {
                if below(3) != 0 {
                    buckets.add(class, 1 + below(if round % 7 == 0 { 100_000 } else { 40 }));
                }
            }
            if buckets.is_empty() {
                buckets.add(2, 1);
            }
            let count = 1 + below(if round % 5 == 0 { 3000 } else { 70 });

            let mut made: BTreeMap<u64, u64> = BTreeMap::new();
            for value in spread(
                &buckets,
                usize::try_from(count).expect("fits"),
                BucketKind::Class,
            ) {
                *made.entry(class_floor(value)).or_default() += 1;
            }

            assert_eq!(
                spread_counts(&buckets, count),
                made.into_iter().collect::<Vec<_>>(),
                "{buckets:?} spread into {count}"
            );
        }
        assert!(spread_counts(&Buckets::new(), 5).is_empty());
        assert!(spread_counts(&[(1, 3)].into_iter().collect(), 0).is_empty());
    }
}
