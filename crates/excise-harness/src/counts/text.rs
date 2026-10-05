//! Text that is safe to show.
//!
//! A pull request's counts arrive as an artifact, which its author controls. Nothing in a
//! document is shown as it came: a field is held to the schema first, and what survives is shown
//! only through these functions, in a log or in a Markdown comment. This is the one place where
//! text from outside becomes text that can neither forge a workflow command, nor break out of a
//! table, nor link, mention, or embed anything.

/// `text` for a log line or an error message: at most `limit` characters, each one printable
/// ASCII, with every other character written as an escape (`\n`, `\u{202e}`). The result never
/// holds a line break, so text from outside can never begin a line of its own, and a line that
/// begins with `::` is how a workflow command is spelled.
pub(crate) fn log_safe(text: &str, limit: usize) -> String {
    let mut safe = String::new();
    for (index, character) in text.chars().enumerate() {
        if index == limit {
            safe.push_str("...");
            break;
        }
        safe.extend(character.escape_default());
    }
    safe
}

/// `text` as an inline Markdown code span, which renders it literally: a link, a mention, an
/// issue reference, an HTML tag, and a comment marker inside one are all plain text.
///
/// At most `limit` characters are kept, every character that is not printable ASCII becomes `?`,
/// a backtick (the only character that could end the span) becomes `'`, a pipe (which would end
/// a table cell, even inside a code span) is escaped, and `<` becomes `?`, so that no HTML tag
/// and no second comment marker from outside is anywhere in the comment's text.
pub(crate) fn code(text: &str, limit: usize) -> String {
    let mut span = String::from("`");
    for character in text.chars().take(limit) {
        match character {
            '`' => span.push('\''),
            '|' => span.push_str("\\|"),
            '<' => span.push('?'),
            ' '..='~' => span.push(character),
            _ => span.push('?'),
        }
    }
    if span.len() == 1 {
        span.push('-');
    }
    span.push('`');
    span
}

/// `value` with a comma between each group of three digits.
pub(crate) fn grouped(value: u64) -> String {
    let digits = value.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_log_line_never_holds_a_line_break_or_a_raw_control_character() {
        let hostile = "ok\n::set-output name=pwn::1\r\u{1b}[2J\u{202e}end";

        let safe = log_safe(hostile, 200);

        assert!(
            safe.is_ascii() && !safe.contains(['\n', '\r', '\u{1b}']),
            "{safe}"
        );
        assert!(
            safe.contains("\\n::set-output"),
            "the break is written out: {safe}"
        );
        assert!(safe.contains("\\u{202e}"), "{safe}");
    }

    #[test]
    fn a_log_line_is_cut_at_the_limit() {
        assert_eq!(log_safe("abcdef", 3), "abc...");
        assert_eq!(log_safe("abc", 3), "abc");
        assert_eq!(log_safe("", 3), "");
    }

    #[test]
    fn a_code_span_cannot_be_ended_and_shows_only_printable_ascii() {
        assert_eq!(code("a`b", 20), "`a'b`");
        assert_eq!(code("a|b", 20), "`a\\|b`");
        assert_eq!(
            code("<!-- excise-counts --> <img>", 40),
            "`?!-- excise-counts --> ?img>`",
            "no `<` from outside reaches the comment, so no HTML and no second marker"
        );
        assert_eq!(code("line\nbreak\u{202e}", 20), "`line?break?`");
        assert_eq!(code("", 20), "`-`");
        assert_eq!(code("abcdef", 3), "`abc`");
    }

    #[test]
    fn numbers_are_grouped_by_thousands() {
        for (value, text) in [
            (0, "0"),
            (999, "999"),
            (1_000, "1,000"),
            (17_435_521, "17,435,521"),
            (9_007_199_254_740_991, "9,007,199,254,740,991"),
        ] {
            assert_eq!(grouped(value), text);
        }
    }
}
