//! The pull-request comment: a pull request's counts against its base, as Markdown.
//!
//! Everything here is rendered from a document that was read as untrusted input, so the rule is
//! strict. Text that came from the document is shown only inside an inline code span, which
//! renders it literally (see [`super::text::code`]); numbers are formatted here from whole
//! numbers; commits are shown as links built from 40 hexadecimal digits that were checked. The
//! words around them are this module's own. The first line is the hidden marker by which a
//! comment is found again, and nothing a document holds can add another.

use std::fmt::Write as _;

use crate::{report::HarnessCounts, scenario::Profile};

use super::{
    PROFILE,
    compare::{FLAG_PERCENT, Flag, KNOWN, NotCompared, Row, Skipped, compare},
    history::{BaseSearch, is_commit},
    text::{code, grouped},
};

/// The hidden first line of the comment, by which an earlier one is found and updated.
pub const MARKER: &str = "<!-- excise-counts -->";

/// The longest comment, in bytes. GitHub refuses one of 65,536 characters. [`render`] holds to it
/// whatever the document holds: the rows that do not fit are left out, and the comment says how
/// many.
pub const MAX_COMMENT_BYTES: usize = 60_000;

/// How many counts the table lists. A document is bounded well below this by its schema.
const MAX_ROWS: usize = 200;

/// How many notes below the table are listed. A document is bounded well below this by its
/// schema, and without the bound a document that never met it could make the notes alone longer
/// than the comment may be.
const MAX_NOTES: usize = 32;

/// The room kept for the line that says how many rows were left out.
const OMISSION_RESERVE: usize = 64;

/// The longest piece of a document's text that is shown.
const SHOWN: usize = 100;

const REPOSITORY_URL: &str = "https://github.com/findyourexit/excise";

/// Where a reader learns how to read the comment.
const GUIDE_URL: &str =
    "https://github.com/findyourexit/excise/blob/main/docs/development.md#counts-and-count-history";

/// A rendered comment, and what the comparison found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    /// The Markdown, beginning with [`MARKER`].
    pub body: String,
    /// How many counts were compared.
    pub compared: usize,
    /// How many of them differ.
    pub changed: usize,
    /// How many are flagged.
    pub flagged: usize,
}

/// A commit as a link to it, in a code span. `commit` must be 40 hexadecimal digits: anything
/// else is shown as plain code, never as a link.
fn commit_link(commit: &str) -> String {
    if is_commit(commit) {
        format!("[`{}`]({REPOSITORY_URL}/commit/{commit})", &commit[..7])
    } else {
        code(commit, 12)
    }
}

/// A commit that is not linked, in a code span.
fn commit_code(commit: &str) -> String {
    code(commit.get(..7).unwrap_or(commit), 7)
}

fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// The move from `base` to `head` as a percentage of `base`, to one decimal place and with its
/// sign, worked out in whole numbers.
fn percent_text(base: u64, head: u64) -> String {
    if base == 0 {
        return "from 0".to_owned();
    }
    let difference = u128::from(base.abs_diff(head));
    let denominator = u128::from(base);
    // Tenths of a percent, rounded half up.
    let tenths = (difference * 2000 + denominator) / (2 * denominator);
    let sign = if head >= base { '+' } else { '-' };
    format!("{sign}{}.{}%", tenths / 10, tenths % 10)
}

/// The signed difference from `base` to `head`.
fn delta_text(base: u64, head: u64) -> String {
    if head >= base {
        format!("+{}", grouped(head - base))
    } else {
        format!("-{}", grouped(base - head))
    }
}

/// The name of a case in a table: its fixture, and its profile where it is not the usual one.
fn case_label(fixture: &str, profile: Profile) -> String {
    if profile == PROFILE {
        code(fixture, SHOWN)
    } else {
        code(&format!("{fixture} ({profile})"), SHOWN)
    }
}

/// The name of a count in a table.
fn metric_label(name: &str) -> String {
    KNOWN
        .iter()
        .find(|known| known.name == name)
        .map_or_else(|| code(name, SHOWN), |known| known.label.to_owned())
}

fn number_cell(count: Option<u64>) -> String {
    count.map_or_else(|| "-".to_owned(), grouped)
}

fn change_cell(row: &Row, has_base: bool) -> String {
    match (row.base, row.head) {
        (Some(base), Some(head)) if base == head => "none".to_owned(),
        (Some(base), Some(head)) => {
            let change = format!("{} ({})", delta_text(base, head), percent_text(base, head));
            match row.flag {
                Flag::Worse => format!("**{change}, worse**"),
                Flag::Better => format!("**{change}, better**"),
                Flag::Unexpected => format!("**{change}, unexpected**"),
                Flag::None => change,
            }
        }
        (None, Some(_)) if has_base => "new".to_owned(),
        (None, Some(_)) => "no base".to_owned(),
        (Some(_), None) => "dropped".to_owned(),
        (None, None) => "-".to_owned(),
    }
}

fn skipped_note(skipped: &NotCompared) -> String {
    let case = case_label(&skipped.fixture, skipped.profile);
    match skipped.reason {
        Skipped::FixtureChanged => format!(
            "- {case} is not compared: its fixture changed in this pull request, so its counts \
             are of a different tree."
        ),
        Skipped::NewCase => {
            format!("- {case} is new in this pull request, so there is nothing to compare it with.")
        }
        Skipped::DroppedCase => {
            format!("- {case} was counted at the base and is not counted by this pull request.")
        }
    }
}

/// What the comment says about the commit the comparison is against.
fn basis(head: &HarnessCounts, search: &BaseSearch) -> String {
    let counted = format!(
        "Counts of this pull request's merge commit {}",
        commit_code(&head.context.git_sha)
    );
    let base_commit = head
        .context
        .pull_request
        .as_ref()
        .map(|pull_request| pull_request.base_sha.as_str());
    match (&search.found, base_commit) {
        (Some(found), Some(base)) if found.distance == 0 => format!(
            "{counted}, compared with the counts recorded for its base commit {}.",
            commit_link(base)
        ),
        (Some(found), Some(base)) => format!(
            "{counted}, compared with the counts recorded for {}, the nearest recorded ancestor of \
             its base commit {}, {} earlier.",
            commit_link(&found.commit),
            commit_link(base),
            plural(found.distance, "commit", "commits")
        ),
        (Some(found), None) => format!(
            "{counted}, compared with the counts recorded for {}.",
            commit_link(&found.commit)
        ),
        (None, Some(base)) => match search.searched {
            0 => format!(
                "{counted}. The history of its base commit {} could not be read, so there is \
                 nothing to compare them with.",
                commit_link(base)
            ),
            1 => format!(
                "{counted}. No counts are recorded for its base commit {}, which has no earlier \
                 commit to look at, so there is nothing to compare them with.",
                commit_link(base)
            ),
            searched => format!(
                "{counted}. No counts are recorded for its base commit {} or for any of its {} \
                 nearest ancestors, so there is nothing to compare them with.",
                commit_link(base),
                searched - 1
            ),
        },
        (None, None) => format!("{counted}. There is nothing to compare them with."),
    }
}

/// What differs between the machines that counted the two sides, which a reader should weigh.
fn environment_notes(head: &HarnessCounts, search: &BaseSearch) -> Vec<String> {
    let Some(found) = &search.found else {
        return Vec::new();
    };
    let base = &found.document.context;
    let mut notes = Vec::new();
    if base.toolchain != head.context.toolchain {
        notes.push(format!(
            "- The toolchain differs: the base was counted with {}, this pull request with {}.",
            code(&base.toolchain, SHOWN),
            code(&head.context.toolchain, SHOWN)
        ));
    }
    if base.runner != head.context.runner {
        notes.push(format!(
            "- The runner differs: the base was counted on {}, this pull request on {}.",
            code(
                &format!(
                    "{} {} {}",
                    base.runner.os, base.runner.arch, base.runner.os_version
                ),
                SHOWN
            ),
            code(
                &format!(
                    "{} {} {}",
                    head.context.runner.os,
                    head.context.runner.arch,
                    head.context.runner.os_version
                ),
                SHOWN
            )
        ));
    }
    notes
}

/// Renders the comment for `head`, compared with the nearest record in `search`.
///
/// The body is at most [`MAX_COMMENT_BYTES`].
#[must_use]
pub fn render(head: &HarnessCounts, search: &BaseSearch) -> Comment {
    render_within(head, search, MAX_COMMENT_BYTES)
}

/// [`render`], for a body of at most `limit` bytes.
fn render_within(head: &HarnessCounts, search: &BaseSearch, limit: usize) -> Comment {
    let comparison = compare(search.found.as_ref().map(|found| &found.document), head);
    let (compared, changed, flagged) = (
        comparison.compared(),
        comparison.changed(),
        comparison.flagged(),
    );

    let mut top = String::new();
    let _ = writeln!(top, "{MARKER}");
    let _ = writeln!(top, "### Deterministic counts\n");
    let _ = writeln!(top, "{}\n", basis(head, search));
    if comparison.has_base {
        let _ = writeln!(
            top,
            "{} compared: {} unchanged, {} changed, {} flagged.\n",
            plural(compared, "count", "counts"),
            compared - changed,
            changed,
            flagged
        );
    }
    let _ = writeln!(top, "| Fixture | Count | Base | Head | Change |");
    let _ = writeln!(top, "|---|---|---:|---:|---|");

    let rows: Vec<String> = comparison
        .rows
        .iter()
        .take(MAX_ROWS)
        .map(|row| {
            format!(
                "| {} | {} | {} | {} | {} |\n",
                case_label(&row.fixture, row.profile),
                metric_label(&row.metric),
                number_cell(row.base),
                number_cell(row.head),
                change_cell(row, comparison.has_base)
            )
        })
        .collect();

    let mut notes: Vec<String> = comparison.not_compared.iter().map(skipped_note).collect();
    notes.extend(environment_notes(head, search));
    let more_notes = notes.len().saturating_sub(MAX_NOTES);
    notes.truncate(MAX_NOTES);
    if more_notes > 0 {
        notes.push(format!(
            "- {} are not listed.",
            plural(more_notes, "more note", "more notes")
        ));
    }
    let mut tail = String::new();
    if !notes.is_empty() {
        let _ = writeln!(tail, "\n{}", notes.join("\n"));
    }
    let _ = writeln!(
        tail,
        "\nCounts do not depend on timing or load, so every difference is real. A cost that moves \
         by more than {FLAG_PERCENT}% is flagged, and so is any change in a fixture's entries, \
         which is no cost but a sign that the fixture or the accounting changed. These are not \
         timings: for those, attach `cargo xtask bench-e2e` evidence under \"Performance \
         evidence\" in the description. [How to read this comment]({GUIDE_URL})."
    );

    // The rows are what grows with the document: they get what is left of the limit once the
    // rest, and the line that says how many were left out, are in.
    let room = limit.saturating_sub(top.len() + tail.len() + OMISSION_RESERVE);
    let (mut shown, mut used) = (0, 0);
    for row in &rows {
        if used + row.len() > room {
            break;
        }
        used += row.len();
        shown += 1;
    }
    let left_out = comparison.rows.len() - shown;

    let mut body = top;
    for row in &rows[..shown] {
        body.push_str(row);
    }
    if left_out > 0 {
        let _ = writeln!(
            body,
            "\n{} are not shown.",
            plural(left_out, "more count", "more counts")
        );
    }
    body.push_str(&tail);
    Comment {
        body,
        compared,
        changed,
        flagged,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        counts::{
            artifact::parse_untrusted,
            history::{ANCESTOR_LIMIT, BaseRecord},
            test_support::{HASH, OTHER_HASH, case, commit, for_pull_request, record, suite},
        },
        report::{Document, MAX_COUNT},
    };

    const NUMBER: u64 = 123;

    /// The counts of a pull request whose base is commit `0xb0` and head `0xaa`.
    fn head(store: u64) -> HarnessCounts {
        for_pull_request(
            record(&commit(0xee), suite(store)),
            NUMBER,
            &commit(0xb0),
            &commit(0xaa),
        )
    }

    /// A search that found the record of `at`, `distance` commits before the base.
    fn found(store: u64, at: u8, distance: usize) -> BaseSearch {
        BaseSearch {
            found: Some(BaseRecord {
                document: record(&commit(at), suite(store)),
                commit: commit(at),
                distance,
            }),
            searched: distance + 1,
            rejected: Vec::new(),
        }
    }

    fn nothing_found(searched: usize) -> BaseSearch {
        BaseSearch {
            found: None,
            searched,
            rejected: Vec::new(),
        }
    }

    /// `body` with every inline code span replaced by `CODE`: what a reader sees as prose.
    fn without_code_spans(body: &str) -> String {
        let mut prose = String::new();
        let mut in_code = false;
        for character in body.chars() {
            if character == '`' {
                if in_code {
                    prose.push_str("CODE");
                }
                in_code = !in_code;
            } else if !in_code {
                prose.push(character);
            }
        }
        prose
    }

    /// The unescaped pipes of a table row.
    fn pipes(line: &str) -> usize {
        line.matches('|').count() - line.matches("\\|").count()
    }

    #[test]
    fn the_comment_begins_with_the_marker_and_says_what_it_was_compared_with() {
        let comment = render(&head(350), &found(350, 0xb0, 0));

        assert!(
            comment.body.starts_with("<!-- excise-counts -->\n"),
            "{}",
            comment.body
        );
        assert_eq!(comment.body.matches(MARKER).count(), 1);
        assert!(
            comment.body.contains(
                "compared with the counts recorded for its base commit \
                 [`b0b0b0b`](https://github.com/findyourexit/excise/commit/"
            ),
            "{}",
            comment.body
        );
        assert!(
            comment.body.contains("merge commit `eeeeeee`"),
            "{}",
            comment.body
        );
    }

    #[test]
    fn a_nearest_ancestor_is_named_with_how_far_back_it_is() {
        let three = render(&head(350), &found(350, 0x77, 3)).body;
        let one = render(&head(350), &found(350, 0x77, 1)).body;

        assert!(
            three.contains("the nearest recorded ancestor of its base commit"),
            "{three}"
        );
        assert!(three.contains("`7777777`"), "{three}");
        assert!(three.contains("3 commits earlier"), "{three}");
        assert!(one.contains("1 commit earlier"), "{one}");
    }

    #[test]
    fn a_missing_base_record_says_so_and_still_shows_the_counts() {
        let comment = render(&head(350), &nothing_found(ANCESTOR_LIMIT + 1));

        assert!(
            comment
                .body
                .contains("No counts are recorded for its base commit [`b0b0b0b`]")
                && comment.body.contains("any of its 200 nearest ancestors"),
            "{}",
            comment.body
        );
        assert!(
            comment
                .body
                .contains("| `wide-1k` | Scan-store bytes | - | 350 | no base |")
        );
        assert_eq!(
            (comment.compared, comment.changed, comment.flagged),
            (0, 0, 0)
        );
    }

    #[test]
    fn a_missing_base_record_says_how_far_back_it_looked() {
        let alone = render(&head(350), &nothing_found(1)).body;
        let unread = render(&head(350), &nothing_found(0)).body;
        let two = render(&head(350), &nothing_found(2)).body;

        assert!(
            alone.contains("which has no earlier commit to look at"),
            "a base commit with no ancestry is not said to have had ancestors searched: {alone}"
        );
        assert!(
            unread.contains("The history of its base commit [`b0b0b0b`]")
                && unread.contains("could not be read"),
            "{unread}"
        );
        assert!(two.contains("any of its 1 nearest ancestors"), "{two}");
    }

    #[test]
    fn the_table_shows_both_sides_the_delta_and_flags_what_passes_the_threshold() {
        let mut moved = head(350);
        moved.cases[0]
            .metrics
            .insert("scan_store_bytes".to_owned(), 380); // +8.6%
        moved.cases[0].metrics.insert("residue_files".to_owned(), 3); // -25.0%
        moved.cases[1]
            .metrics
            .insert("scan_store_bytes".to_owned(), 17_150 + 100); // +0.6%
        let mut base = found(350, 0xb0, 0);
        base.found.as_mut().expect("a base record").document.cases[0]
            .metrics
            .insert("residue_files".to_owned(), 4);

        let comment = render(&moved, &base);

        assert!(
            comment
                .body
                .contains("| `wide-1k` | Scan-store bytes | 350 | 380 | **+30 (+8.6%), worse** |"),
            "{}",
            comment.body
        );
        assert!(
            comment
                .body
                .contains("| `wide-1k` | Residue files | 4 | 3 | **-1 (-25.0%), better** |"),
            "{}",
            comment.body
        );
        assert!(
            comment.body.contains(
                "| `tiny-files-50k` | Scan-store bytes | 17,150 | 17,250 | +100 (+0.6%) |"
            ),
            "a change within the threshold is shown and not flagged: {}",
            comment.body
        );
        assert!(
            comment
                .body
                .contains("| `wide-1k` | Entries | 1,002 | 1,002 | none |")
        );
        assert_eq!(
            (comment.compared, comment.changed, comment.flagged),
            (6, 3, 2)
        );
        assert!(
            comment.body.contains("more than 5% is flagged"),
            "the threshold is stated: {}",
            comment.body
        );
    }

    #[test]
    fn a_change_in_a_fixtures_entries_is_called_unexpected() {
        let mut moved = head(350);
        moved.cases[0].metrics.insert("entries".to_owned(), 1_003);

        let body = render(&moved, &found(350, 0xb0, 0)).body;

        assert!(
            body.contains("| `wide-1k` | Entries | 1,002 | 1,003 | **+1 (+0.1%), unexpected** |"),
            "{body}"
        );
    }

    #[test]
    fn a_move_from_zero_has_no_percentage() {
        assert_eq!(percent_text(0, 3), "from 0");
        assert_eq!(delta_text(0, 3), "+3");
        assert_eq!(delta_text(1_000_000, 0), "-1,000,000");
        assert_eq!(percent_text(1_000, 1_005), "+0.5%");
        assert_eq!(percent_text(1_000, 929), "-7.1%");
        assert_eq!(percent_text(1, 3), "+200.0%");
        assert_eq!(percent_text(1_000, 1_000), "+0.0%");
    }

    #[test]
    fn cases_that_cannot_be_compared_are_explained_below_the_table() {
        let mut changed = head(350);
        changed.cases[0].fixture.hash = OTHER_HASH.to_owned();
        changed
            .cases
            .push(case("brand-new", HASH, &[("entries", 3)]));
        let mut base = found(350, 0xb0, 0);
        base.found
            .as_mut()
            .expect("a record")
            .document
            .cases
            .push(case("retired", HASH, &[("entries", 3)]));

        let body = render(&changed, &base).body;

        assert!(
            body.contains("- `wide-1k` is not compared: its fixture changed"),
            "{body}"
        );
        assert!(
            body.contains("- `brand-new` is new in this pull request"),
            "{body}"
        );
        assert!(
            body.contains(
                "- `retired` was counted at the base and is not counted by this pull request"
            ),
            "{body}"
        );
        assert!(
            !body.contains("| `wide-1k` |"),
            "no count of a changed fixture is compared: {body}"
        );
    }

    #[test]
    fn a_different_toolchain_or_runner_is_pointed_out() {
        let mut base = found(350, 0xb0, 0);
        let record = &mut base.found.as_mut().expect("a record").document;
        record.context.toolchain = "rustc 1.97.0 (aaaaaaaaa 2026-06-01)".to_owned();
        record.context.runner.os_version = "Ubuntu 22.04".to_owned();

        let body = render(&head(350), &base).body;

        assert!(
            body.contains(
                "The toolchain differs: the base was counted with `rustc 1.97.0 (aaaaaaaaa 2026-06-01)`, \
                 this pull request with `rustc 1.98.0 (88d9e12ae 2026-08-18)`."
            ),
            "{body}"
        );
        assert!(
            body.contains(
                "The runner differs: the base was counted on `linux x86_64 Ubuntu 22.04`"
            ),
            "{body}"
        );
        let same = render(&head(350), &found(350, 0xb0, 0)).body;
        assert!(!same.contains("differs"), "{same}");
    }

    /// The document of a pull request whose free-text fields hold everything hostile that the
    /// schema lets through: printable ASCII of at most 120 characters.
    fn hostile() -> (HarnessCounts, String) {
        let toolchain = "](https://evil.example/t) <img src=x onerror=alert(1)> @everyone #7 `tick` | ::set-output name=pwn::1";
        assert!(toolchain.len() <= 120);
        let mut document = head(350);
        document.context.toolchain = toolchain.to_owned();
        document.context.runner.os_version = "x]( <!-- excise-counts --> ``` \\| @here".to_owned();
        // It is a realistic artifact: what the schema accepts, and nothing else.
        let text = document.to_json_pretty().expect("a document renders");
        let parsed = parse_untrusted(text.as_bytes())
            .expect("the schema accepts hostile text that is printable ASCII");
        assert_eq!(parsed, document);
        (parsed, text)
    }

    #[test]
    fn hostile_text_in_an_artifact_is_shown_only_inside_code_spans() {
        let (document, _) = hostile();

        let body = render(&document, &found(350, 0xb0, 0)).body;

        let prose = without_code_spans(&body);
        let prose = prose
            .strip_prefix(MARKER)
            .expect("the marker begins the comment");
        for token in [
            "evil.example",
            "<img",
            "onerror",
            "@everyone",
            "@here",
            "#7",
            "pwn",
            "set-output",
            "<!--",
        ] {
            assert!(
                !prose.contains(token),
                "`{token}` escaped its code span:\n{body}"
            );
        }
        assert!(
            body.contains("evil.example") && body.contains("@everyone"),
            "the text is still there to read: {body}"
        );
        assert_eq!(
            body.matches(MARKER).count(),
            1,
            "a marker inside hostile text must not become a second marker: {body}"
        );
        assert_eq!(
            body.matches('<').count(),
            1,
            "the only `<` in the comment is the marker's: {body}"
        );
    }

    #[test]
    fn hostile_text_cannot_start_a_line_or_leave_its_table_cell() {
        let (document, _) = hostile();

        let body = render(&document, &found(350, 0xb0, 0)).body;

        assert!(
            body.lines()
                .all(|line| !line.trim_start().starts_with("::")),
            "text from outside must not begin a line: {body}"
        );
        for line in body.lines().filter(|line| line.starts_with('|')) {
            assert_eq!(pipes(line), 6, "{line}");
        }
    }

    #[test]
    fn text_that_the_schema_would_have_refused_is_still_inert() {
        // The renderer does not rely on the schema having been applied.
        let mut document = head(350);
        document.cases[0].fixture.id = "a|b\n| x | y |\n[click](http://evil.example)".to_owned();
        document.cases[0].metrics.insert("m|n`x".to_owned(), 5);
        document.context.pull_request = Some(crate::report::PullRequestOrigin {
            number: NUMBER,
            base_sha: "not-a-commit](http://evil.example)".to_owned(),
            head_sha: commit(0xaa),
        });

        let body = render(&document, &nothing_found(0)).body;

        assert!(
            !without_code_spans(&body).contains("evil.example"),
            "{body}"
        );
        assert!(
            !body.contains("/commit/not-a-commit"),
            "a link is built only from 40 hexadecimal digits: {body}"
        );
        for line in body.lines().filter(|line| line.starts_with('|')) {
            assert_eq!(pipes(line), 6, "{line}");
        }
    }

    #[test]
    fn a_commit_is_linked_only_when_it_is_forty_hexadecimal_digits() {
        let linked = commit_link(&commit(0xab));

        assert_eq!(
            linked,
            format!(
                "[`abababa`](https://github.com/findyourexit/excise/commit/{})",
                commit(0xab)
            )
        );
        for not_a_commit in ["main", "ABABABAB", &"g".repeat(40), &commit(0xab)[..39]] {
            assert!(!commit_link(not_a_commit).contains("]("), "{not_a_commit}");
        }
    }

    /// A document with many short counts: 16 cases of 32.
    fn many_counts() -> HarnessCounts {
        let mut document = head(350);
        let metrics: Vec<(String, u64)> = (0..32)
            .map(|index| (format!("count_{index:02}"), index))
            .collect();
        document.cases = (0..16)
            .map(|index| {
                let mut case = case(&format!("fixture-{index:02}"), HASH, &[]);
                case.metrics = metrics.iter().cloned().collect();
                case
            })
            .collect();
        document
    }

    #[test]
    fn many_counts_are_cut_at_the_row_limit_and_the_comment_says_what_it_left_out() {
        let body = render(&many_counts(), &nothing_found(10)).body;

        assert!(body.len() <= MAX_COMMENT_BYTES, "{} bytes", body.len());
        assert!(body.contains("312 more counts are not shown"), "{body}");
        assert_eq!(
            body.lines()
                .filter(|line| line.starts_with("| `fixture-"))
                .count(),
            200
        );
    }

    /// The widest document the schema allows: 16 cases under the profile with the longest name,
    /// each with 32 counts of the longest names and the largest values, against a base of the
    /// smallest values, so that every cell is as wide as it gets.
    fn widest() -> (HarnessCounts, BaseSearch) {
        let longest_profile = Profile::ALL
            .iter()
            .copied()
            .max_by_key(|profile| profile.as_str().len())
            .expect("a profile");
        let names: Vec<String> = (0..32)
            .map(|index| format!("c{index:02}{}", "x".repeat(45)))
            .collect();
        let cases = |value: u64| -> Vec<_> {
            (0..16)
                .map(|index| {
                    let mut one = case(&format!("f{index:02}{}", "y".repeat(61)), HASH, &[]);
                    one.profile = longest_profile;
                    one.metrics = names.iter().map(|name| (name.clone(), value)).collect();
                    one
                })
                .collect()
        };
        let mut document = head(350);
        document.cases = cases(MAX_COUNT);
        let search = BaseSearch {
            found: Some(BaseRecord {
                document: record(&commit(0xb0), cases(1)),
                commit: commit(0xb0),
                distance: 0,
            }),
            searched: 1,
            rejected: Vec::new(),
        };
        (document, search)
    }

    #[test]
    fn the_widest_document_the_schema_allows_makes_a_comment_that_fits() {
        let (document, search) = widest();
        let text = document.to_json_pretty().expect("it renders");
        assert!(
            parse_untrusted(text.as_bytes()).is_ok(),
            "the widest document is one that the schema and the reader allow"
        );

        let body = render(&document, &search).body;

        assert!(
            body.len() <= MAX_COMMENT_BYTES,
            "{} bytes, of at most {MAX_COMMENT_BYTES}",
            body.len()
        );
    }

    /// No valid document reaches the limit today, so the limit itself is tested by lowering it:
    /// the rows that do not fit are the ones left out, and the comment says how many.
    #[test]
    fn rows_that_do_not_fit_the_limit_are_left_out_and_counted() {
        let limit = 5_000;

        let body = render_within(&many_counts(), &nothing_found(10), limit).body;

        let shown = body
            .lines()
            .filter(|line| line.starts_with("| `fixture-"))
            .count();
        assert!(
            body.len() <= limit,
            "{} bytes, of at most {limit}",
            body.len()
        );
        assert!(shown > 0 && shown < MAX_ROWS, "{shown} rows are shown");
        assert!(
            body.contains(&format!("{} more counts are not shown", 512 - shown)),
            "{body}"
        );
        assert!(
            body.starts_with(MARKER) && body.contains("[How to read this comment]("),
            "everything that is not a row is still there"
        );
        assert!(
            body.contains("| `fixture-00` | `count_00` |"),
            "the rows that are shown are the first ones"
        );
    }

    /// The renderer does not rely on the schema, so neither does its limit: a document of a
    /// thousand cases, which the schema stops at sixteen, has a note for each that changed.
    #[test]
    fn the_limit_holds_for_a_document_that_never_met_the_schema() {
        let mut document = head(350);
        document.cases = (0..1_000)
            .map(|index| case(&format!("fixture-{index}"), OTHER_HASH, &[("entries", 1)]))
            .collect();

        let body = render(&document, &found(350, 0xb0, 0)).body;

        assert!(
            body.len() <= MAX_COMMENT_BYTES,
            "{} bytes, of at most {MAX_COMMENT_BYTES}",
            body.len()
        );
        assert!(body.contains("more notes are not listed"), "{body}");
    }
}
