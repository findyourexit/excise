//! Names for generated entries: seed-driven styles for the scale classes, and the fixed catalog of
//! hostile names.

use serde::{Deserialize, Serialize};

use crate::fixture::{path::is_valid_component, rng::permute};

/// The longest name any file system this harness targets accepts, in bytes (`NAME_MAX`).
pub const MAX_NAME_BYTES: usize = 255;

/// The fewest hexadecimal digits that tell `held` entries apart: how many a name needs to be
/// unique among the entries of a folder that holds that many.
pub(crate) fn hex_digits_for(held: u64) -> u32 {
    let mut digits = 1;
    let mut capacity: u128 = 16;
    while capacity < u128::from(held) {
        digits += 1;
        capacity *= 16;
    }
    digits
}

/// How many decimal digits `value` has: `0` has one.
pub(crate) fn decimal_digits(value: u64) -> u32 {
    value.checked_ilog10().map_or(1, |log| log + 1)
}

/// How the names of the generated directories or files are spelled.
///
/// Both styles give every index a distinct name, so names never collide within a directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "style", rename_all = "snake_case", deny_unknown_fields)]
pub enum NameStyle {
    /// `{prefix}{index}{suffix}`, with the index in decimal, zero-padded to `width` digits.
    /// Independent of the seed.
    Sequential {
        /// Text before the index.
        #[serde(default)]
        prefix: String,
        /// Text after the index, for example an extension.
        #[serde(default)]
        suffix: String,
        /// The minimum number of digits; `0` means no padding.
        #[serde(default)]
        width: u8,
    },
    /// `{prefix}{hex}{suffix}`: `length` lowercase hexadecimal digits that are a keyed
    /// permutation of the index. Names look random, are unique within a directory, and change
    /// with the seed.
    Hex {
        /// Text before the digits.
        #[serde(default)]
        prefix: String,
        /// Text after the digits.
        #[serde(default)]
        suffix: String,
        /// The number of hexadecimal digits, `1..=16`. Default `8`.
        #[serde(default = "default_hex_length")]
        length: u8,
    },
    /// Exactly `name`. Names a single entry, for a file or directory whose name a scenario
    /// refers to, such as `keep.txt`.
    Literal {
        /// The name.
        name: String,
    },
}

const fn default_hex_length() -> u8 {
    8
}

impl NameStyle {
    /// A sequential style with a prefix and suffix and no padding.
    #[must_use]
    pub fn sequential(prefix: &str, suffix: &str) -> Self {
        Self::Sequential {
            prefix: prefix.to_owned(),
            suffix: suffix.to_owned(),
            width: 0,
        }
    }

    /// The name for `index`. `key` seeds the [`NameStyle::Hex`] permutation and is ignored by
    /// [`NameStyle::Sequential`].
    pub(crate) fn name(&self, index: u64, key: u64) -> Vec<u8> {
        match self {
            Self::Sequential {
                prefix,
                suffix,
                width,
            } => format!(
                "{prefix}{index:0width$}{suffix}",
                width = usize::from(*width)
            )
            .into_bytes(),
            Self::Hex {
                prefix,
                suffix,
                length,
            } => {
                let digits = usize::from(*length);
                let value = permute(index, key, u32::from(*length) * 4);
                format!("{prefix}{value:0digits$x}{suffix}").into_bytes()
            }
            Self::Literal { name } => name.as_bytes().to_vec(),
        }
    }

    /// The length in bytes of the longest name this style gives to the entries `0..count` of one
    /// folder, whatever the key, and exactly: a sequential index has the most digits at the
    /// largest index, a hex name always has its `length` digits, and a literal name is the same
    /// for the one entry it names. With a `count` of 0 it is the length of the first name, which
    /// no entry has.
    pub(crate) fn longest_name_bytes(&self, count: u64) -> u64 {
        let bytes = |text: &str| u64::try_from(text.len()).unwrap_or(u64::MAX);
        match self {
            Self::Sequential {
                prefix,
                suffix,
                width,
            } => {
                let digits = u64::from(decimal_digits(count.saturating_sub(1)));
                bytes(prefix)
                    .saturating_add(bytes(suffix))
                    .saturating_add(digits.max(u64::from(*width)))
            }
            Self::Hex {
                prefix,
                suffix,
                length,
            } => bytes(prefix)
                .saturating_add(bytes(suffix))
                .saturating_add(u64::from(*length)),
            Self::Literal { name } => bytes(name),
        }
    }

    /// Checks that this style can name `count` entries, and returns a description of the problem
    /// otherwise.
    pub(crate) fn check(&self, count: u64) -> Result<(), String> {
        let (prefix, suffix) = match self {
            Self::Sequential { prefix, suffix, .. } | Self::Hex { prefix, suffix, .. } => {
                (prefix, suffix)
            }
            Self::Literal { name } => {
                return if count > 1 {
                    Err("a literal name can name only one entry".to_owned())
                } else if !is_valid_component(name.as_bytes()) || name.len() > MAX_NAME_BYTES {
                    Err(format!(
                        "{name:?} is not a usable name of at most {MAX_NAME_BYTES} bytes"
                    ))
                } else {
                    Ok(())
                };
            }
        };
        for text in [prefix, suffix] {
            if text.contains(['/', '\0']) {
                return Err("prefixes and suffixes must not contain `/` or NUL".to_owned());
            }
        }
        let digits = match self {
            Self::Literal { .. } => 0,
            Self::Sequential { width, .. } => {
                let needed = count.saturating_sub(1).to_string().len();
                needed.max(usize::from(*width))
            }
            Self::Hex { length, .. } => {
                if !(1..=16).contains(length) {
                    return Err("`length` must be between 1 and 16".to_owned());
                }
                let capacity = 1_u128 << (u32::from(*length) * 4);
                if u128::from(count) > capacity {
                    return Err(format!(
                        "{length} hex digits cannot name {count} entries distinctly"
                    ));
                }
                usize::from(*length)
            }
        };
        if prefix.len() + suffix.len() + digits > MAX_NAME_BYTES {
            return Err(format!("names would exceed {MAX_NAME_BYTES} bytes"));
        }
        Ok(())
    }
}

/// Names with control characters: C0 controls, DEL, and two C1 controls.
///
/// Every name in the catalog is a valid Unix file name (no `/`, no NUL) of at most
/// [`MAX_NAME_BYTES`] bytes and keeps a readable ASCII tail, so a failure message can say which
/// name it was.
pub(crate) fn control_character_names() -> Vec<Vec<u8>> {
    let mut names: Vec<Vec<u8>> = [
        &b"ctl-\x01-soh"[..],
        b"ctl-\x07-bel",
        b"ctl-\x08-bs",
        b"ctl-\t-tab",
        b"ctl-\x0b-vt",
        b"ctl-\x0c-ff",
        b"ctl-\x7f-del",
    ]
    .iter()
    .map(|name| name.to_vec())
    .collect();
    // C1 controls, spelled as valid UTF-8: NEXT LINE and the single-character CSI.
    names.push("ctl-\u{85}-nel".as_bytes().to_vec());
    names.push("ctl-\u{9b}-csi".as_bytes().to_vec());
    names
}

/// Names with bidirectional overrides, isolates, and marks: the classic `invoice-<RLO>fdp.exe`
/// spoof and its relatives.
pub(crate) fn bidi_names() -> Vec<Vec<u8>> {
    [
        "bidi-\u{202e}fdp.exe",
        "bidi-\u{202d}lro",
        "bidi-\u{2066}lri-\u{2069}",
        "bidi-\u{200f}rlm",
        "\u{202e}leading-rlo",
    ]
    .iter()
    .map(|name| name.as_bytes().to_vec())
    .collect()
}

/// Names that carry terminal escape sequences: colors, an OSC title change, a screen clear, the
/// alternate-screen switch, and a full reset.
pub(crate) fn escape_sequence_names() -> Vec<Vec<u8>> {
    [
        &b"esc-\x1b[31mred\x1b[0m"[..],
        b"esc-\x1b]0;pwned\x07",
        b"esc-\x1b[2J",
        b"esc-\x1b[?1049h",
        b"esc-\x1bc",
    ]
    .iter()
    .map(|name| name.to_vec())
    .collect()
}

/// Names with line breaks.
pub(crate) fn newline_names() -> Vec<Vec<u8>> {
    [
        &b"nl-line1\nline2"[..],
        b"nl-cr\rreturn",
        b"nl-crlf\r\nend",
        b"nl-trailing\n",
    ]
    .iter()
    .map(|name| name.to_vec())
    .collect()
}

/// Names that are not valid UTF-8. Only file systems that store names as bytes accept them:
/// Linux ext4 does, APFS answers `EILSEQ`.
pub(crate) fn invalid_utf8_names() -> Vec<Vec<u8>> {
    [
        &b"bad-\xff-byte"[..],
        b"bad-\xc0\xaf-overlong",
        b"bad-\x80-continuation",
        b"bad-\xe2\x82-truncated",
        b"bad-\xed\xa0\x80-surrogate",
        b"bad-\x9b31m-c1csi",
    ]
    .iter()
    .map(|name| name.to_vec())
    .collect()
}

/// Names of exactly [`MAX_NAME_BYTES`] bytes, in one, two, and four bytes per character.
pub(crate) fn maximum_length_names() -> Vec<Vec<u8>> {
    vec![
        "a".repeat(MAX_NAME_BYTES).into_bytes(),
        format!("{}a", "\u{e9}".repeat((MAX_NAME_BYTES - 1) / 2)).into_bytes(),
        format!("{}abc", "\u{1F600}".repeat((MAX_NAME_BYTES - 3) / 4)).into_bytes(),
    ]
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{
        MAX_NAME_BYTES, NameStyle, bidi_names, control_character_names, decimal_digits,
        escape_sequence_names, invalid_utf8_names, maximum_length_names, newline_names,
    };
    use crate::fixture::path::is_valid_component;

    fn text(name: Vec<u8>) -> String {
        String::from_utf8(name).unwrap_or_else(|error| panic!("not UTF-8: {error}"))
    }

    #[test]
    fn sequential_names_are_padded_and_ignore_the_key() {
        let plain = NameStyle::sequential("m", ".js");
        assert_eq!(text(plain.name(7, 1)), "m7.js");
        assert_eq!(plain.name(7, 1), plain.name(7, 999));

        let padded = NameStyle::Sequential {
            prefix: "f".to_owned(),
            suffix: String::new(),
            width: 6,
        };
        assert_eq!(text(padded.name(42, 0)), "f000042");
        assert_eq!(
            text(padded.name(1_234_567, 0)),
            "f1234567",
            "padding never truncates"
        );
    }

    #[test]
    fn hex_names_are_unique_per_key_and_change_with_it() {
        let style = NameStyle::Hex {
            prefix: "h-".to_owned(),
            suffix: ".bin".to_owned(),
            length: 4,
        };
        let names: BTreeSet<Vec<u8>> = (0..65_536).map(|index| style.name(index, 5)).collect();
        assert_eq!(
            names.len(),
            65_536,
            "4 hex digits must give 65,536 distinct names"
        );
        assert!(
            names
                .iter()
                .all(|name| name.len() == "h-".len() + 4 + ".bin".len())
        );
        assert_ne!(style.name(0, 5), style.name(0, 6));
    }

    #[test]
    fn check_rejects_styles_that_cannot_name_the_requested_count() {
        let hex = |length| NameStyle::Hex {
            prefix: String::new(),
            suffix: String::new(),
            length,
        };
        assert!(hex(2).check(256).is_ok());
        assert!(
            hex(2).check(257).is_err(),
            "two digits name only 256 entries"
        );
        assert!(hex(0).check(1).is_err());
        assert!(hex(17).check(1).is_err());

        let slash = NameStyle::sequential("a/", "");
        assert!(slash.check(1).is_err());
        let long = NameStyle::sequential(&"p".repeat(MAX_NAME_BYTES), "");
        assert!(long.check(1).is_err(), "prefix plus digits exceed NAME_MAX");
        assert!(NameStyle::sequential("", "").check(0).is_ok());
        assert!(NameStyle::sequential("d", "").check(10_000).is_ok());
    }

    #[test]
    fn the_hostile_catalog_is_made_of_valid_names_that_are_hostile() {
        let groups = [
            control_character_names(),
            bidi_names(),
            escape_sequence_names(),
            newline_names(),
            invalid_utf8_names(),
            maximum_length_names(),
        ];
        let mut all = BTreeSet::new();
        for group in &groups {
            assert!(!group.is_empty());
            for name in group {
                assert!(is_valid_component(name), "{name:?} is not a usable name");
                assert!(name.len() <= MAX_NAME_BYTES);
                assert!(all.insert(name.clone()), "{name:?} appears twice");
            }
        }
        assert!(
            control_character_names()
                .iter()
                .all(|name| { String::from_utf8_lossy(name).chars().any(char::is_control) })
        );
        assert!(
            newline_names()
                .iter()
                .all(|name| name.iter().any(|byte| matches!(byte, b'\n' | b'\r')))
        );
        assert!(
            escape_sequence_names()
                .iter()
                .all(|name| name.contains(&0x1b))
        );
        assert!(
            invalid_utf8_names()
                .iter()
                .all(|name| std::str::from_utf8(name).is_err())
        );
        assert!(
            bidi_names()
                .iter()
                .all(|name| std::str::from_utf8(name).is_ok())
        );
        assert!(
            maximum_length_names()
                .iter()
                .all(|name| name.len() == MAX_NAME_BYTES),
            "the long names are exactly at the limit"
        );
    }

    #[test]
    fn the_longest_name_of_a_style_is_the_longest_name_it_gives() {
        let padded = |width| NameStyle::Sequential {
            prefix: "p-".to_owned(),
            suffix: ".x".to_owned(),
            width,
        };
        let hex = |length| NameStyle::Hex {
            prefix: "h".to_owned(),
            suffix: ".bin".to_owned(),
            length,
        };
        let styles = [
            NameStyle::sequential("", ""),
            NameStyle::sequential("d", ""),
            NameStyle::sequential("f", ".dat"),
            padded(1),
            padded(4),
            padded(8),
            hex(1),
            hex(3),
            hex(8),
        ];
        // Counts on either side of every power of ten, and 1, which names one entry.
        for style in &styles {
            for count in [
                1_u64, 2, 9, 10, 11, 99, 100, 101, 1_000, 1_001, 10_000, 10_001,
            ] {
                if matches!(style, NameStyle::Hex { length: 1, .. }) && count > 16 {
                    continue;
                }
                if matches!(style, NameStyle::Hex { length: 3, .. }) && count > 4_096 {
                    continue;
                }
                let longest = (0..count)
                    .map(|index| u64::try_from(style.name(index, 7).len()).expect("a length"))
                    .max()
                    .expect("a name");
                assert_eq!(
                    style.longest_name_bytes(count),
                    longest,
                    "{style:?} naming {count} entries"
                );
            }
        }

        let literal = NameStyle::Literal {
            name: "keep.txt".to_owned(),
        };
        assert_eq!(literal.longest_name_bytes(1), 8);
        let longest = NameStyle::Literal {
            name: "n".repeat(MAX_NAME_BYTES),
        };
        assert_eq!(longest.longest_name_bytes(1), 255);
    }

    #[test]
    fn decimal_digits_counts_digits() {
        for (value, digits) in [
            (0_u64, 1),
            (9, 1),
            (10, 2),
            (99, 2),
            (100, 3),
            (999_999, 6),
            (1_000_000, 7),
            (u64::MAX, 20),
        ] {
            assert_eq!(decimal_digits(value), digits, "{value}");
        }
    }
}
