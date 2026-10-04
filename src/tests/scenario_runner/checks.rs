//! The assertions the runner makes, as functions of the screen and the fixture.

use std::path::Path;

use excise_harness::scenario::{ExpectFs, ExpectScreen, Region};
use regex::Regex;

use super::fixture::entry_exists;
use super::screen::{Panel, Screen};
use crate::native_path::safe_display_path;
use crate::tests::fixtures::snapshot_path;

/// The part of the screen an assertion looks at, in words.
pub fn region_name(region: Option<Region>) -> String {
    match region {
        None => "the screen".to_owned(),
        Some(Region::Header) => "the header".to_owned(),
        Some(Region::Dialog) => "the dialog".to_owned(),
        Some(Region::Rows([first, last])) => format!("rows {first} to {last}"),
    }
}

/// What `expect_screen` asserts, in words.
pub fn expectation(expect: &ExpectScreen) -> String {
    let mut parts = Vec::new();
    if !expect.contains.is_empty() {
        parts.push(format!("contains {:?}", expect.contains));
    }
    if !expect.not_contains.is_empty() {
        parts.push(format!("does not contain {:?}", expect.not_contains));
    }
    if !expect.regex.is_empty() {
        parts.push(format!("matches {:?}", expect.regex));
    }
    format!("{} {}", region_name(expect.region), parts.join(" and "))
}

/// What `expect_screen` finds wrong with the screen, one message per broken expectation.
///
/// A dialog region with no dialog open holds no text: nothing can be found in it, and nothing
/// unwanted is in it either.
pub fn screen_violations(screen: &Screen, expect: &ExpectScreen, regexes: &[Regex]) -> Vec<String> {
    let named = region_name(expect.region);
    let text = screen.region_text(expect.region);
    let mut violations = Vec::new();
    for needle in &expect.contains {
        match &text {
            Some(text) if text.contains(needle.as_str()) => {}
            Some(_) => violations.push(format!("{named} does not contain {needle:?}")),
            None => violations.push(format!("no dialog is open, so it lacks {needle:?}")),
        }
    }
    for needle in &expect.not_contains {
        if let Some(text) = &text
            && text.contains(needle.as_str())
        {
            violations.push(format!("{named} contains {needle:?}"));
        }
    }
    for (pattern, regex) in expect.regex.iter().zip(regexes) {
        match &text {
            Some(text) if regex.is_match(text) => {}
            Some(_) => violations.push(format!("{named} does not match /{pattern}/")),
            None => violations.push(format!("no dialog is open, so it lacks /{pattern}/")),
        }
    }
    violations
}

/// What `expect_fs` asserts, in words.
pub fn fs_expectation(expect: &ExpectFs) -> String {
    let mut parts = Vec::new();
    if !expect.present.is_empty() {
        parts.push(format!("has {:?}", expect.present));
    }
    if !expect.absent.is_empty() {
        parts.push(format!("lacks {:?}", expect.absent));
    }
    format!("the fixture {}", parts.join(" and "))
}

/// What `expect_fs` finds wrong with the fixture, one message per broken expectation.
pub fn fs_violations(root: &Path, expect: &ExpectFs) -> Vec<String> {
    let mut violations = Vec::new();
    for (paths, wanted) in [(&expect.present, true), (&expect.absent, false)] {
        for path in paths {
            match entry_exists(root, path) {
                Ok(exists) if exists == wanted => {}
                Ok(true) => violations.push(format!("`{path}` exists")),
                Ok(false) => violations.push(format!("`{path}` does not exist")),
                Err(message) => violations.push(message),
            }
        }
    }
    violations
}

pub fn is_delete_dialog(dialog: &Panel) -> bool {
    dialog.title().starts_with("! DELETE ")
}

/// How the program shows a path: the same text the dialog draws, for comparison.
fn displayed(path: &Path) -> String {
    let raw = safe_display_path(path);
    if raw.deceptive {
        return raw.text;
    }
    safe_display_path(Path::new(&snapshot_path(path))).text
}

/// The part of the dialog's path below the fixture root. A path the dialog had to cut short cannot
/// be checked, so it fails.
fn below_root<'a>(shown: &'a str, root: &Path) -> Result<&'a str, String> {
    if shown.ends_with('…') {
        return Err(format!(
            "the dialog cuts its path short ({shown:?}), so the target cannot be verified"
        ));
    }
    let root_text = displayed(root);
    shown
        .strip_prefix(root_text.as_str())
        .and_then(|rest| rest.strip_prefix('/'))
        .ok_or_else(|| {
            format!("the dialog path {shown:?} is not under the fixture root {root_text:?}")
        })
}

/// Checks that the path the dialog shows is an entry called `name` under the fixture root. A path
/// the dialog had to cut short cannot be checked, so it fails.
///
/// # Errors
///
/// Returns why the path is not the entry the step expects.
pub fn check_target(shown: &str, root: &Path, name: &str) -> Result<(), String> {
    let below = below_root(shown, root)?;
    let target = below.rsplit('/').next().unwrap_or(below);
    if target != name {
        return Err(format!(
            "the dialog names {target:?} ({shown:?}), but the step expects {name:?}"
        ));
    }
    Ok(())
}

/// Checks that the path the dialog shows is exactly `path` below the fixture root, where the step
/// says the entry is (`delete`'s `path`).
///
/// # Errors
///
/// Returns why the dialog's path is not that one.
pub fn check_target_path(shown: &str, root: &Path, path: &str) -> Result<(), String> {
    let below = below_root(shown, root)?;
    if below != path {
        return Err(format!(
            "the dialog deletes {below:?} ({shown:?}), but the step expects {path:?}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen_of(lines: &[&str]) -> Screen {
        Screen::from_lines(lines)
    }

    fn expect_screen(
        contains: &[&str],
        not_contains: &[&str],
        region: Option<Region>,
    ) -> ExpectScreen {
        ExpectScreen {
            contains: contains.iter().map(|text| (*text).to_owned()).collect(),
            not_contains: not_contains.iter().map(|text| (*text).to_owned()).collect(),
            regex: Vec::new(),
            region,
        }
    }

    #[test]
    fn the_dialog_target_must_be_exactly_the_requested_entry_under_the_root() {
        let root = Path::new("/nowhere/fixture-root");
        let shown_root = displayed(root);
        let below = |tail: &str| format!("{shown_root}/{tail}");

        assert_eq!(
            check_target(&below("victim.bin"), root, "victim.bin"),
            Ok(())
        );
        assert_eq!(
            check_target(&below("docs/victim.bin"), root, "victim.bin"),
            Ok(())
        );

        let other = check_target(&below("keep-a.bin"), root, "victim.bin")
            .expect_err("another entry must be refused");
        assert!(other.contains("names \"keep-a.bin\""), "{other}");
        // A name that merely ends the same way is another entry.
        for lookalike in ["old-victim.bin", "victim.bin.bak"] {
            check_target(&below(lookalike), root, "victim.bin")
                .expect_err("a lookalike must be refused");
        }
        // The entry's name must be the last component, not merely a component.
        check_target(&below("victim.bin/child"), root, "victim.bin")
            .expect_err("a child of the entry must be refused");
        let outside = check_target("/elsewhere/victim.bin", root, "victim.bin")
            .expect_err("a path outside the root must be refused");
        assert!(outside.contains("not under the fixture root"), "{outside}");
    }

    #[test]
    fn a_path_the_dialog_cut_short_cannot_be_verified() {
        let root = Path::new("/nowhere/fixture-root");
        let shown = format!("{}/vict…", displayed(root));
        let refused =
            check_target(&shown, root, "victim.bin").expect_err("a cut path must be refused");
        assert!(refused.contains("cuts its path short"), "{refused}");
    }

    #[test]
    fn a_dialog_region_without_a_dialog_lacks_text_but_holds_nothing_unwanted() {
        let screen = screen_of(&["EXCISE  /work  ◆ COMPLETE", "no dialog here"]);
        let violations = screen_violations(
            &screen,
            &expect_screen(&["Quit Excise?"], &["DELETE"], Some(Region::Dialog)),
            &[],
        );
        assert_eq!(
            violations,
            ["no dialog is open, so it lacks \"Quit Excise?\""]
        );
    }

    #[test]
    fn regions_limit_what_a_match_can_see() {
        let screen = screen_of(&["title row", "second row", "third row", "body with needle"]);
        let header = Some(Region::Header);
        let body = Some(Region::Rows([3, 9]));
        assert!(screen_violations(&screen, &expect_screen(&["needle"], &[], None), &[]).is_empty());
        assert!(screen_violations(&screen, &expect_screen(&["needle"], &[], body), &[]).is_empty());
        assert_eq!(
            screen_violations(&screen, &expect_screen(&["needle"], &[], header), &[]),
            ["the header does not contain \"needle\""]
        );
        assert_eq!(
            screen_violations(&screen, &expect_screen(&[], &["title"], header), &[]),
            ["the header contains \"title\""]
        );
    }
}
