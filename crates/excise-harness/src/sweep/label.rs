//! The name a version's files go by below the run's directory.
//!
//! A ref is whatever a person typed after `--refs`, and git accepts a good deal in it: `feature/x`
//! has a slash, `v1.4.0^{/fix}` and `main@{upstream}` have braces, `HEAD~3` a tilde, and `..` or a
//! name that starts with a dot is a way out of a directory or a hidden file. Every file the sweep
//! keeps for a version (the evidence of its checks, the log of its build) is named by [`label`]
//! instead, so that no ref can make a subdirectory, leave the run's directory, or hide a file.

/// The most characters of the ref that a label keeps.
const REFERENCE_CHARS: usize = 64;
/// How many characters of the commit a label ends in.
const SHA_CHARS: usize = 12;

/// The name of a version's files: the ref with every character outside `[A-Za-z0-9._-]` replaced
/// by `_` (a `.` or `-` that begins the name too, so that it is neither hidden nor an option), cut to
/// 64 characters, then `-` and the first 12 characters of the commit `sha`.
///
/// A label is never empty, never begins with `.` or `-`, and holds no path separator, so
/// `evidence/<label>/<name>.txt` and `builds/<label>.log` stay directly below the run's directory
/// whatever the ref says. Refs of different commits get different labels, short of two commits
/// that begin with the same twelve characters. Two refs of one commit share a label when they
/// differ only in characters the label replaces (`a/b` and `a_b`), or in the case of letters on a
/// file system that ignores it, so a sweep refuses such a pair.
///
/// ```
/// use excise_harness::sweep::label;
///
/// let sha = "0123456789abcdef0123456789abcdef01234567";
/// assert_eq!(label("v1.3.0", sha), "v1.3.0-0123456789ab");
/// assert_eq!(label("a/../b", sha), "a_.._b-0123456789ab");
/// ```
#[must_use]
pub fn label(reference: &str, sha: &str) -> String {
    let mut name = String::with_capacity(REFERENCE_CHARS + 1 + SHA_CHARS);
    let mut leading = true;
    for c in reference.chars().take(REFERENCE_CHARS) {
        leading &= matches!(c, '.' | '-');
        name.push(if is_plain(c) && !leading { c } else { '_' });
    }
    if name.is_empty() {
        name.push('_');
    }
    name.push('-');
    name.extend(
        sha.chars()
            .take(SHA_CHARS)
            .map(|c| if is_plain(c) { c } else { '_' }),
    );
    name
}

/// Whether a character may be part of a label as it is.
fn is_plain(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
    const OTHER_SHA: &str = "fedcba9876543210fedcba9876543210fedcba98";
    const SHORT_SHA: &str = "0123456789ab";

    fn is_plain_name(name: &str) -> bool {
        !name.is_empty() && name.chars().all(is_plain)
    }

    #[test]
    fn a_published_tag_keeps_its_name_and_gains_the_start_of_its_commit() {
        assert_eq!(label("v1.3.0", SHA), "v1.3.0-0123456789ab");
        assert_eq!(label("HEAD", SHA), "HEAD-0123456789ab");
        assert_eq!(
            label("release_1.4-rc.1", SHA),
            "release_1.4-rc.1-0123456789ab"
        );
    }

    #[test]
    fn whatever_the_ref_says_the_label_is_one_plain_name_that_ends_in_the_commit() {
        let hostile = [
            "feature/x",
            "a/../b",
            "../../etc/passwd",
            "/etc/passwd",
            "..",
            ".",
            "...",
            "v1.4.0^{/fix}",
            "main@{upstream}",
            "HEAD~3",
            "HEAD^^",
            "with space",
            "tab\there",
            "line\nbreak",
            "nul\0byte",
            "-rf",
            "--help",
            "-",
            ".hidden",
            "..hidden",
            "a\\b",
            "C:\\Windows",
            "naïve",
            "日本語/タグ",
            "emoji-🦀",
            "",
        ];
        for reference in hostile {
            let name = label(reference, SHA);

            assert!(is_plain_name(&name), "{reference:?} gave {name:?}");
            assert!(
                name.ends_with("-0123456789ab"),
                "{reference:?} gave {name:?}, which does not end in the commit"
            );
            assert!(
                !name.starts_with(['.', '-']),
                "{reference:?} gave {name:?}, which is hidden or an option"
            );
            assert_ne!(name, ".");
            assert_ne!(name, "..");
        }
    }

    #[test]
    fn a_slash_a_dot_dot_and_a_brace_each_become_a_single_underscore() {
        assert_eq!(label("a/../b", SHA), "a_.._b-0123456789ab");
        assert_eq!(label("v1.4.0^{/fix}", SHA), "v1.4.0___fix_-0123456789ab");
        assert_eq!(label("with space", SHA), "with_space-0123456789ab");
    }

    #[test]
    fn a_leading_dot_or_dash_is_replaced_so_that_the_name_is_neither_hidden_nor_an_option() {
        assert_eq!(label(".hidden", SHA), "_hidden-0123456789ab");
        assert_eq!(label("..", SHA), "__-0123456789ab");
        assert_eq!(label("../../etc", SHA), "___.._etc-0123456789ab");
        assert_eq!(label("-rf", SHA), "_rf-0123456789ab");
        assert_eq!(label("--help", SHA), "__help-0123456789ab");
        assert_eq!(label(".-.x", SHA), "___x-0123456789ab");
        // Only the run at the start: a dot or a dash further in is an ordinary character.
        assert_eq!(label("v1-.x", SHA), "v1-.x-0123456789ab");
    }

    #[test]
    fn every_character_that_is_not_ascii_is_replaced_one_for_one() {
        assert_eq!(label("naïve", SHA), "na_ve-0123456789ab");
        assert_eq!(label("日本語", SHA), "___-0123456789ab");
        assert_eq!(label("🦀", SHA), "_-0123456789ab");
    }

    #[test]
    fn a_ref_with_nothing_to_keep_still_gets_a_name() {
        assert_eq!(label("", SHA), "_-0123456789ab");
    }

    #[test]
    fn a_long_ref_is_cut_to_sixty_four_characters_before_the_commit() {
        let long = "x".repeat(200);

        let name = label(&long, SHA);

        assert_eq!(name, format!("{}-0123456789ab", "x".repeat(64)));
        assert_eq!(name.chars().count(), 64 + 1 + 12);
        // Cut by characters, never in the middle of one.
        let wide = "日".repeat(100);
        assert_eq!(
            label(&wide, SHA),
            format!("{}-0123456789ab", "_".repeat(64))
        );
    }

    #[test]
    fn only_the_first_twelve_characters_of_the_commit_are_kept_and_a_short_one_is_kept_whole() {
        assert_eq!(label("v1", SHA), "v1-0123456789ab");
        assert_eq!(label("v1", SHORT_SHA), "v1-0123456789ab");
        assert_eq!(label("v1", "abc"), "v1-abc");
        assert_eq!(label("v1", ""), "v1-");
    }

    #[test]
    fn a_commit_that_is_not_plain_text_cannot_take_the_label_out_of_its_directory() {
        let name = label("v1", "../../../etc/passwd");

        assert_eq!(name, "v1-.._.._.._etc");
        assert!(is_plain_name(&name));
    }

    #[test]
    fn refs_that_read_alike_once_replaced_are_told_apart_by_their_commits() {
        let slash = label("release/1.0", SHA);
        let underscore = label("release_1.0", OTHER_SHA);

        assert_ne!(slash, underscore);
        // The same commit under two names that differ only in what is replaced is the one case
        // where labels coincide, and the sweep refuses it before it writes anything.
        assert_eq!(label("release/1.0", SHA), label("release_1.0", SHA));
    }

    #[test]
    fn refs_of_different_commits_never_share_a_label() {
        for reference in ["HEAD", "v1.3.0", "a/b", "..", ""] {
            assert_ne!(label(reference, SHA), label(reference, OTHER_SHA));
        }
    }
}
