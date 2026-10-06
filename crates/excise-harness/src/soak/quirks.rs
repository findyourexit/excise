//! The quirk log: anything unexpected that a soak saw.
//!
//! A soak gates nothing, so a number that looks wrong is not a failure. What it does is write it
//! down, with the circumstances, in `quirks.txt` beside `summary.json`. The log is **local only**:
//! its text comes from the screen and from the scan report, and a real tree's names are in both (a
//! dialog that names a folder, the last path a scan could not read). `summary.json`, which is made
//! to be shared, only counts the quirks by kind ([`QuirkLog::counts`]).

use std::fmt::Write as _;

use crate::{
    report::{QuirkKind, SoakQuirkCount},
    run_support::is_deceptive,
};

/// The most characters of a quirk's detail that are kept, the separators between its words
/// included. A detail that is longer is cut there and ends with [`ELLIPSIS`], so that it is never
/// longer than `DETAIL_LIMIT` plus the ellipsis.
const DETAIL_LIMIT: usize = 600;

/// What a detail that was cut ends with: a separator and three dots, which are not counted against
/// [`DETAIL_LIMIT`].
const ELLIPSIS: &str = " ...";

/// One unexpected thing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Quirk {
    /// What kind of thing.
    pub kind: QuirkKind,
    /// Where it happened: `round 1, tui default`. Never a path.
    pub scope: String,
    /// What was seen, on one line. It can name things in the tree.
    pub detail: String,
}

/// The quirks of one soak, in the order they were seen.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QuirkLog {
    quirks: Vec<Quirk>,
}

impl QuirkLog {
    /// An empty log.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Notes a quirk. The detail is put on one line (a screen's rows are joined with ` | `) and
    /// cut at a bounded length.
    pub fn note(&mut self, kind: QuirkKind, scope: &str, detail: impl AsRef<str>) {
        self.quirks.push(Quirk {
            kind,
            scope: scope.to_owned(),
            detail: one_line(detail.as_ref()),
        });
    }

    /// Takes every quirk of `other`, after the ones this log has.
    pub fn append(&mut self, other: Self) {
        self.quirks.extend(other.quirks);
    }

    /// The quirks, in the order they were noted.
    #[must_use]
    pub fn quirks(&self) -> &[Quirk] {
        &self.quirks
    }

    /// How many quirks there are.
    #[must_use]
    pub fn len(&self) -> usize {
        self.quirks.len()
    }

    /// Whether the log is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.quirks.is_empty()
    }

    /// Whether a quirk of `kind` was noted.
    #[must_use]
    pub fn has(&self, kind: QuirkKind) -> bool {
        self.quirks.iter().any(|quirk| quirk.kind == kind)
    }

    /// How many quirks of each kind, in the order the kinds are declared, for the kinds that have
    /// any. This is all of the log that `summary.json` carries.
    #[must_use]
    pub fn counts(&self) -> Vec<SoakQuirkCount> {
        QuirkKind::ALL
            .iter()
            .filter_map(|&kind| {
                let count = self
                    .quirks
                    .iter()
                    .filter(|quirk| quirk.kind == kind)
                    .count();
                (count > 0).then_some(SoakQuirkCount {
                    kind,
                    count: count as u64,
                })
            })
            .collect()
    }

    /// The text of `quirks.txt`.
    #[must_use]
    pub fn render(&self, run_id: &str) -> String {
        let mut text = format!(
            "# excise soak {run_id}: quirks\n\
             #\n\
             # LOCAL ONLY. The lines below can name things in the tree that was soaked: the text of a\n\
             # dialog, the last folder a scan could not read. Do not share this file. summary.json,\n\
             # which holds metrics and counts and no name, is the one to share.\n\
             #\n"
        );
        if self.quirks.is_empty() {
            text.push_str("# none\n");
            return text;
        }
        let _ = writeln!(text, "# {} noted\n", self.quirks.len());
        for quirk in &self.quirks {
            let _ = writeln!(text, "[{}] {}: {}", quirk.kind, quirk.scope, quirk.detail);
        }
        text
    }
}

/// `text` on one line: line breaks, other control characters, and the bidirectional controls that
/// reorder what a terminal shows become separators, runs of spaces collapse, and a long text is
/// cut with an ellipsis.
///
/// At most [`DETAIL_LIMIT`] characters of the text are kept, the separators between words
/// included, and the ellipsis comes after them. The limit is checked before each character is
/// added together with the separator that goes in front of it, so that no run of white space can
/// carry the count over the limit: a count that is only tested for being equal to the limit is
/// never met again once a separator has stepped past it.
fn one_line(text: &str) -> String {
    let mut line = String::with_capacity(text.len().min(DETAIL_LIMIT + ELLIPSIS.len()));
    let mut pending_space = false;
    let mut characters = 0;
    for character in text.chars() {
        if is_deceptive(character) || character.is_whitespace() {
            pending_space = !line.is_empty();
            continue;
        }
        let added = if pending_space { 2 } else { 1 };
        if characters + added > DETAIL_LIMIT {
            line.push_str(ELLIPSIS);
            return line;
        }
        if pending_space {
            line.push(' ');
            pending_space = false;
            characters += 1;
        }
        line.push(character);
        characters += 1;
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_are_by_kind_in_declaration_order_and_only_for_kinds_that_have_any() {
        let mut log = QuirkLog::new();
        log.note(QuirkKind::Timeout, "round 1, headless default", "slow");
        log.note(QuirkKind::ErrorDialog, "round 1, tui default", "a dialog");
        log.note(
            QuirkKind::Timeout,
            "round 2, tui deterministic",
            "slow again",
        );

        assert_eq!(
            log.counts(),
            [
                SoakQuirkCount {
                    kind: QuirkKind::ErrorDialog,
                    count: 1
                },
                SoakQuirkCount {
                    kind: QuirkKind::Timeout,
                    count: 2
                },
            ]
        );
        assert!(log.has(QuirkKind::Timeout) && !log.has(QuirkKind::Stall));
        assert_eq!(log.len(), 3);
    }

    #[test]
    fn a_detail_is_one_line_and_bounded() {
        let mut log = QuirkLog::new();

        log.note(
            QuirkKind::ErrorDialog,
            "round 1, tui default",
            "  ERROR\n  Could not read /a/b\t\u{1b}[31mred\u{1b}[0m \n\n  [Esc] close ",
        );
        log.note(QuirkKind::Stall, "x", "y".repeat(10_000));

        let first = &log.quirks()[0].detail;
        assert!(
            !first.contains('\n') && !first.contains('\u{1b}'),
            "{first:?}"
        );
        assert!(first.starts_with("ERROR Could not read /a/b"), "{first:?}");
        assert!(first.ends_with("[Esc] close"), "{first:?}");
        let second = &log.quirks()[1].detail;
        assert!(
            second.chars().count() <= DETAIL_LIMIT + 4,
            "{}",
            second.len()
        );
        assert!(second.ends_with(" ..."));
    }

    /// What a detail should be, said the long way round: the words of `text` (white space and the
    /// controls that reorder a line set them apart) joined by one space each, and, when that is
    /// more than the limit, its first `DETAIL_LIMIT` characters without a space at their end,
    /// followed by the ellipsis.
    fn expected_detail(text: &str) -> String {
        let words: Vec<&str> = text
            .split(|character: char| character.is_whitespace() || is_deceptive(character))
            .filter(|word| !word.is_empty())
            .collect();
        let collapsed = words.join(" ");
        if collapsed.chars().count() <= DETAIL_LIMIT {
            return collapsed;
        }
        let kept: String = collapsed.chars().take(DETAIL_LIMIT).collect();
        format!("{}{ELLIPSIS}", kept.trim_end())
    }

    #[test]
    fn a_separator_cannot_carry_a_detail_past_its_limit() {
        // 599 characters, white space, and more. The separator is the 600th character and the
        // next word's first the 601st: a limit that is only tested for being equal to the count
        // is passed over, and what follows is kept without bound.
        let text = format!("{} {}", "a".repeat(DETAIL_LIMIT - 1), "b".repeat(1_000));

        let detail = one_line(&text);

        assert_eq!(detail, format!("{} ...", "a".repeat(DETAIL_LIMIT - 1)));
        assert!(detail.chars().count() <= DETAIL_LIMIT + ELLIPSIS.len());
    }

    #[test]
    fn however_the_white_space_falls_a_detail_is_never_longer_than_its_limit_and_the_ellipsis() {
        let separators = [
            " ",
            "  ",
            "\n",
            "\t \n",
            "\r\n",
            "\u{202e}",
            " \u{200f} ",
            "\u{1b}",
        ];
        // Words of one-, two-, and three-byte characters, so that the limit is in characters.
        for letter in ['a', 'é', '日'] {
            // The first word ends in front of the limit, at it, and behind it.
            for first in DETAIL_LIMIT - 5..=DETAIL_LIMIT + 2 {
                for separator in separators {
                    // Nothing after the separator, a character, two, a word, and a long tail.
                    for second in [0, 1, 2, 5, 1_000] {
                        for third in [0, 3] {
                            let mut text = letter.to_string().repeat(first);
                            text.push_str(separator);
                            text.push_str(&"b".repeat(second));
                            if third > 0 {
                                text.push_str(separator);
                                text.push_str(&"c".repeat(third));
                            }
                            let detail = one_line(&text);

                            assert!(
                                detail.chars().count() <= DETAIL_LIMIT + ELLIPSIS.len(),
                                "{} characters from a first word of {first} {letter:?}, then \
                                 {separator:?}, {second}, {third}",
                                detail.chars().count()
                            );
                            assert_eq!(
                                detail,
                                expected_detail(&text),
                                "a first word of {first} {letter:?}, then {separator:?}, \
                                 {second}, {third}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn a_detail_that_fits_exactly_is_not_cut_and_one_more_character_is() {
        let exactly = "x".repeat(DETAIL_LIMIT);

        assert_eq!(one_line(&exactly), exactly);
        assert_eq!(
            one_line(&format!("{exactly}   \n  ")),
            exactly,
            "white space after the last character is not a character to keep"
        );
        assert_eq!(one_line(&format!("{exactly}y")), format!("{exactly} ..."));
    }

    #[test]
    fn a_detail_holds_no_character_that_reorders_what_a_terminal_shows() {
        let mut log = QuirkLog::new();

        // A name from the tree with a right-to-left override, an isolate, and a mark in it.
        log.note(
            QuirkKind::ErrorDialog,
            "round 1, tui default",
            "Could not read evil\u{202e}gpj.exe and \u{2066}more\u{200f} text",
        );

        let detail = &log.quirks()[0].detail;
        assert!(
            !detail.chars().any(is_deceptive),
            "a control that reorders the line is still in it: {detail:?}"
        );
        assert_eq!(detail, "Could not read evil gpj.exe and more text");
    }

    #[test]
    fn the_text_names_its_audience_and_lists_every_quirk() {
        let mut log = QuirkLog::new();
        let empty = log.render("20261006T000000Z-1");
        assert!(empty.contains("LOCAL ONLY") && empty.contains("# none"));

        log.note(
            QuirkKind::UncertainScan,
            "round 1, headless default",
            "3 folders",
        );
        let text = log.render("20261006T000000Z-1");

        assert!(text.contains("# 1 noted"), "{text}");
        assert!(
            text.contains("[uncertain_scan] round 1, headless default: 3 folders\n"),
            "{text}"
        );
    }

    #[test]
    fn appending_keeps_the_order() {
        let mut first = QuirkLog::new();
        first.note(QuirkKind::Stall, "a", "1");
        let mut second = QuirkLog::new();
        second.note(QuirkKind::Residue, "b", "2");

        first.append(second);

        let kinds: Vec<QuirkKind> = first.quirks().iter().map(|quirk| quirk.kind).collect();
        assert_eq!(kinds, [QuirkKind::Stall, QuirkKind::Residue]);
    }
}
