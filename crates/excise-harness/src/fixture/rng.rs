//! The generator's only source of variation: a small, documented PRNG.
//!
//! Fixture names and sizes are a pure function of the spec and its seed, on every machine, so the
//! generator uses `SplitMix64` (Steele, Lea, and Flood; the seeder recommended for the xoshiro
//! family) instead of an external crate. Its output for a given seed is fixed by the algorithm,
//! and the tests pin the published reference values.

/// `SplitMix64`.
#[derive(Debug, Clone)]
pub(crate) struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    pub(crate) const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    pub(crate) fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A uniform value in `0..bound`, without modulo bias (Lemire's widening multiply with
    /// rejection). `bound` must not be zero.
    pub(crate) fn below(&mut self, bound: u64) -> u64 {
        debug_assert!(bound > 0, "an empty range has no values");
        let bound_wide = u128::from(bound);
        let mut product = u128::from(self.next_u64()) * bound_wide;
        let mut low = low_word(product);
        if low < bound {
            let threshold = bound.wrapping_neg() % bound;
            while low < threshold {
                product = u128::from(self.next_u64()) * bound_wide;
                low = low_word(product);
            }
        }
        high_word(product)
    }

    /// A uniform value in `low..=high`. `low` must not exceed `high`.
    pub(crate) fn range_inclusive(&mut self, low: u64, high: u64) -> u64 {
        debug_assert!(low <= high, "the range runs backwards");
        match (high - low).checked_add(1) {
            Some(width) => low + self.below(width),
            None => self.next_u64(),
        }
    }

    /// Fills `buffer` with the stream, eight bytes at a time.
    pub(crate) fn fill(&mut self, buffer: &mut [u8]) {
        for chunk in buffer.chunks_mut(8) {
            let word = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&word[..chunk.len()]);
        }
    }
}

/// The low 64 bits of a product.
fn low_word(value: u128) -> u64 {
    u64::try_from(value & u128::from(u64::MAX)).unwrap_or(u64::MAX)
}

/// The high 64 bits of a product.
fn high_word(value: u128) -> u64 {
    u64::try_from(value >> 64).unwrap_or(u64::MAX)
}

/// Derives an independent stream seed from `seed` and a label, so that each part of a fixture,
/// and each file's content, draws from its own stream and adding a part never shifts the others.
///
/// The label is folded with FNV-1a (64-bit) and then mixed with the seed through one `SplitMix64`
/// step.
pub(crate) fn derive_seed(seed: u64, label: &[u8]) -> u64 {
    const FNV_OFFSET: u64 = 0xCBF2_9CE4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01B3;
    let folded = label.iter().fold(FNV_OFFSET, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(FNV_PRIME)
    });
    SplitMix64::new(seed ^ folded).next_u64()
}

/// A keyed bijection on `0..2^bits` (`1 <= bits <= 64`): distinct indexes map to distinct values,
/// so names built from the value are unique within a directory whatever the seed.
pub(crate) fn permute(index: u64, key: u64, bits: u32) -> u64 {
    debug_assert!((1..=64).contains(&bits), "bits must be between 1 and 64");
    let mask = if bits >= 64 {
        u64::MAX
    } else {
        (1_u64 << bits) - 1
    };
    let shift = (bits / 2).max(1);
    let mut stream = SplitMix64::new(key);
    let mut value = index & mask;
    for _ in 0..3 {
        // Each step is invertible modulo 2^bits: xor with a constant, multiplication by an odd
        // constant, and a right xor-shift.
        value ^= stream.next_u64() & mask;
        value = value.wrapping_mul(stream.next_u64() | 1) & mask;
        value ^= value >> shift;
    }
    value
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{SplitMix64, derive_seed, permute};

    #[test]
    fn splitmix64_matches_the_published_reference_stream() {
        let mut zero = SplitMix64::new(0);
        assert_eq!(zero.next_u64(), 0xE220_A839_7B1D_CDAF);
        assert_eq!(zero.next_u64(), 0x6E78_9E6A_A1B9_65F4);
        assert_eq!(zero.next_u64(), 0x06C4_5D18_8009_454F);

        let mut other = SplitMix64::new(1_234_567);
        assert_eq!(other.next_u64(), 0x599E_D017_FB08_FC85);
        assert_eq!(other.next_u64(), 0x2C73_F084_5854_0FA5);
        assert_eq!(other.next_u64(), 0x883E_BCE5_A3F2_7C77);
    }

    #[test]
    fn bounded_draws_stay_in_range_and_cover_it() {
        let mut rng = SplitMix64::new(7);
        let mut seen = BTreeSet::new();
        for _ in 0..2_000 {
            let value = rng.below(6);
            assert!(value < 6);
            seen.insert(value);
        }
        assert_eq!(seen.len(), 6, "every face of the die should appear");

        for _ in 0..500 {
            let value = rng.range_inclusive(10, 12);
            assert!((10..=12).contains(&value));
        }
        assert_eq!(rng.range_inclusive(5, 5), 5);
        // The full range must not overflow while computing its width.
        let _ = rng.range_inclusive(0, u64::MAX);
    }

    #[test]
    fn fill_is_the_little_endian_stream_and_handles_partial_tails() {
        let mut expected = SplitMix64::new(9);
        let first = expected.next_u64().to_le_bytes();
        let second = expected.next_u64().to_le_bytes();

        let mut buffer = [0_u8; 11];
        SplitMix64::new(9).fill(&mut buffer);
        assert_eq!(&buffer[..8], &first);
        assert_eq!(&buffer[8..], &second[..3]);
    }

    #[test]
    fn derived_seeds_depend_on_seed_and_label() {
        let base = derive_seed(1, b"victim");
        assert_eq!(base, derive_seed(1, b"victim"));
        assert_ne!(base, derive_seed(2, b"victim"));
        assert_ne!(base, derive_seed(1, b"keeper"));
    }

    #[test]
    fn permutation_is_a_bijection_for_every_width_used() {
        for (bits, key) in [(4_u32, 3_u64), (8, 99), (12, 5), (16, 0xDEAD_BEEF)] {
            let count = 1_u64 << bits;
            let images: BTreeSet<u64> = (0..count).map(|index| permute(index, key, bits)).collect();
            assert_eq!(
                images.len(),
                usize::try_from(count).unwrap_or(usize::MAX),
                "{bits}-bit permutation lost values"
            );
            assert!(images.iter().all(|value| *value < count));
        }
        // A different key gives a different arrangement.
        assert_ne!(
            (0..16)
                .map(|index| permute(index, 1, 4))
                .collect::<Vec<_>>(),
            (0..16)
                .map(|index| permute(index, 2, 4))
                .collect::<Vec<_>>()
        );
    }
}
