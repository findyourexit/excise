//! What `excise` shows: the grammar of its header, panes, and dialogs.
//!
//! [`Screen`] finds text and boxes and knows nothing about `excise`. This module reads the
//! specific things a scenario cares about: the scan state in the header badge, the selected item
//! in the inspector pane, the filter prompt, and the deletion and quit dialogs. If the interface
//! changes wording, this is the one file to update.

use crate::scenario::ScanState;

use super::screen::{BoxView, HEADER_ROWS, Screen};

/// The title of the inspector pane that describes the selected entry.
const INSPECTOR_TITLE: &str = "SELECTED ITEM";
/// What the inspector says when nothing is selected.
const NOTHING_SELECTED: &str = "Choose an item";
/// The first words of the deletion dialog's title.
const DELETE_TITLE_PREFIX: &str = "! DELETE ";
/// The confirmation line of a deletion that one key confirms.
const SINGLE_KEY_CONFIRMATION: &str = "[Enter/y] start";
/// The instruction of a deletion that must be confirmed by typing a phrase.
const TYPED_CONFIRMATION: &str = "Type this exactly: ";
/// The quit dialog's confirmation line.
const QUIT_CONFIRMATION: &str = "[y] Quit";

/// The scan state in the header row's badge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeaderState {
    /// A scan is running.
    Scanning,
    /// The scan finished.
    Complete,
    /// Any other state, such as `REBUILDING` or `STALE MAP`, with its text.
    Other(String),
}

impl HeaderState {
    /// The badge text.
    #[must_use]
    pub fn label(&self) -> &str {
        match self {
            Self::Scanning => "SCANNING",
            Self::Complete => "COMPLETE",
            Self::Other(label) => label,
        }
    }

    /// Whether this is the state a scenario waits for.
    #[must_use]
    pub const fn is(&self, wanted: ScanState) -> bool {
        matches!(
            (self, wanted),
            (Self::Scanning, ScanState::Scanning) | (Self::Complete, ScanState::Complete)
        )
    }
}

/// The scan state shown in the header band, or `None` before the header is drawn.
///
/// The first row reads ` EXCISE  <path>  <marker> <STATE>`: the path and the badge each carry a
/// space on both sides, so the badge is what follows the last double space. The badge is the only
/// valid completion signal; the word `COMPLETE` also appears in the inspector while a scan is
/// still running.
#[must_use]
pub fn header_state(screen: &Screen) -> Option<HeaderState> {
    let row = screen.row_text(0);
    if !row.trim_start().starts_with("EXCISE") {
        return None;
    }
    let (_, badge) = row.rsplit_once("  ")?;
    let (marker, state) = badge.split_once(' ')?;
    if marker.chars().count() != 1 {
        return None;
    }
    Some(match state.trim() {
        "SCANNING" => HeaderState::Scanning,
        "COMPLETE" => HeaderState::Complete,
        other => HeaderState::Other(other.to_owned()),
    })
}

/// The entry described by the inspector pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedItem {
    /// The entry's name, as the pane shows it.
    pub name: String,
    /// The state line, such as `◆ COMPLETE`.
    pub state: String,
    /// The entry kind: `folder`, `file`, `link`, or `shared item`.
    pub kind: String,
}

/// What the inspector pane shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inspector {
    /// The pane is not drawn.
    NotShown,
    /// The pane invites the user to choose an item.
    NothingSelected,
    /// The pane describes an entry.
    Item(SelectedItem),
}

/// Reads the inspector pane. The selected item is the one the pane shows, not the one a key press
/// meant to select.
#[must_use]
pub fn inspector(screen: &Screen) -> Inspector {
    let Some(pane) = screen.box_titled(INSPECTOR_TITLE) else {
        return Inspector::NotShown;
    };
    let mut lines = pane.interior.iter().map(|line| line.trim());
    let name = lines.next().unwrap_or_default();
    if name.is_empty() || name.starts_with(NOTHING_SELECTED) {
        return Inspector::NothingSelected;
    }
    let state_line = lines.next().unwrap_or_default();
    let (state, kind) = state_line
        .rsplit_once(" · ")
        .or_else(|| state_line.rsplit_once(" . "))
        .unwrap_or((state_line, ""));
    Inspector::Item(SelectedItem {
        name: name.to_owned(),
        state: state.to_owned(),
        kind: kind.to_owned(),
    })
}

/// The filter prompt in the header band.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterPrompt {
    /// The text typed so far.
    pub input: String,
    /// The reason the filter was rejected, if it was.
    pub error: Option<String>,
}

/// The open filter prompt, or `None` when no filter is being typed.
///
/// While the filter is open, the header's status row reads `/ <text>_  [Enter] apply  [Esc]
/// cancel`, or `/ <text>_  ERROR: <reason>` for a filter that does not compile, after any safety
/// labels (`! ELEVATED · `). The filter opens with the previous filter's text in place, so a runner
/// reads the prompt to know what to erase.
#[must_use]
pub fn filter_prompt(screen: &Screen) -> Option<FilterPrompt> {
    const APPLY: &str = "_  [Enter] apply";
    const ERROR: &str = "_  ERROR: ";
    (0..HEADER_ROWS).find_map(|row| {
        let text = screen.row_text(row);
        let rest = after_safety_labels(text.trim_start()).strip_prefix("/ ")?;
        if let Some(end) = rest.rfind(APPLY) {
            return Some(FilterPrompt {
                input: rest[..end].to_owned(),
                error: None,
            });
        }
        let end = rest.rfind(ERROR)?;
        Some(FilterPrompt {
            input: rest[..end].to_owned(),
            error: Some(rest[end + ERROR.len()..].trim().to_owned()),
        })
    })
}

/// The labels `excise` puts in front of the status row when it runs elevated, with reduced deletion
/// guardrails, or both (`safety_label` in `src/ui/display.rs`). A separator follows each: ` · `, or
/// ` . ` in ASCII mode.
const SAFETY_LABELS: [&str; 3] = ["! ELEVATED", "! REDUCED DELETE GUARD", "! REDUCED GUARD"];

/// `status` after the safety labels in front of it and their separators.
fn after_safety_labels(mut status: &str) -> &str {
    while let Some(rest) = SAFETY_LABELS.iter().find_map(|label| {
        let rest = status.strip_prefix(label)?;
        rest.strip_prefix(" · ")
            .or_else(|| rest.strip_prefix(" . "))
    }) {
        status = rest;
    }
    status
}

/// The kind of entry a deletion dialog says it deletes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteKind {
    /// `! DELETE FOLDER`.
    Folder,
    /// `! DELETE FILE`.
    File,
    /// `! DELETE ITEM`: a summary entry that is neither.
    Item,
}

/// How a deletion dialog asks to be confirmed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confirmation {
    /// One key confirms: `[Enter/y] start`.
    SingleKey,
    /// The user must type this phrase and press Enter.
    TypedPhrase(String),
    /// The dialog offers neither, so the runner does not know how to confirm it.
    Unknown,
}

/// A deletion confirmation dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteDialog {
    /// What the title says is deleted.
    pub kind: DeleteKind,
    /// The path line, as drawn. It is cut with an ellipsis when it does not fit.
    pub path: String,
    /// How the dialog is confirmed.
    pub confirmation: Confirmation,
    /// The whole dialog.
    pub view: BoxView,
}

/// The dialog on the screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DialogView {
    /// No dialog is open, or one is still being drawn.
    None,
    /// A deletion confirmation.
    Delete(DeleteDialog),
    /// Any other dialog.
    Other(BoxView),
}

/// Reads the dialog on the screen.
#[must_use]
pub fn dialog_view(screen: &Screen) -> DialogView {
    let Some(view) = screen.dialog() else {
        return DialogView::None;
    };
    let kind = match view.title.strip_prefix(DELETE_TITLE_PREFIX) {
        Some("FOLDER") => DeleteKind::Folder,
        Some("FILE") => DeleteKind::File,
        Some("ITEM") => DeleteKind::Item,
        _ => return DialogView::Other(view),
    };
    let lines: Vec<String> = view
        .interior
        .iter()
        .map(|line| line.trim().to_owned())
        .filter(|line| !line.is_empty())
        .collect();
    let path = lines.first().cloned().unwrap_or_default();
    let confirmation = if lines
        .iter()
        .any(|line| line.contains(SINGLE_KEY_CONFIRMATION))
    {
        Confirmation::SingleKey
    } else if let Some(phrase) = lines
        .iter()
        .find_map(|line| line.strip_prefix(TYPED_CONFIRMATION))
    {
        Confirmation::TypedPhrase(phrase.to_owned())
    } else {
        Confirmation::Unknown
    };
    DialogView::Delete(DeleteDialog {
        kind,
        path,
        confirmation,
        view,
    })
}

/// Whether a dialog offers the plain `[y] Quit` confirmation, as opposed to the variants shown
/// while background work is pending or running.
#[must_use]
pub fn offers_plain_quit(view: &BoxView) -> bool {
    view.interior
        .iter()
        .any(|line| line.trim() == QUIT_CONFIRMATION)
}

#[cfg(test)]
mod tests {
    use super::super::screen::tests::{ascii_box, drawn, place, unicode_box};
    use super::*;

    #[test]
    fn the_header_badge_is_what_follows_the_last_double_space() {
        for (row, expected) in [
            (" EXCISE  /tmp/x/r/  ◆ COMPLETE   ", HeaderState::Complete),
            (" EXCISE  /tmp/x/r/  ◌ SCANNING   ", HeaderState::Scanning),
            (" EXCISE  /tmp/x/r/  C COMPLETE", HeaderState::Complete),
            (" EXCISE  /tmp/x/r/  ~ SCANNING", HeaderState::Scanning),
            (
                " EXCISE  /tmp/x/r/  ◌ REBUILDING",
                HeaderState::Other("REBUILDING".to_owned()),
            ),
            (
                " EXCISE  /tmp/x/r/  ! STALE MAP",
                HeaderState::Other("STALE MAP".to_owned()),
            ),
            (
                " EXCISE  /tmp/x/r/  ? NEEDS REVIEW",
                HeaderState::Other("NEEDS REVIEW".to_owned()),
            ),
        ] {
            let screen = drawn(5, 80, &[(0, 0, row)]);

            assert_eq!(header_state(&screen), Some(expected), "{row:?}");
        }
    }

    #[test]
    fn a_path_that_says_complete_never_fools_the_header() {
        let screen = drawn(5, 80, &[(0, 0, " EXCISE  /data/COMPLETE  ◌ SCANNING")]);

        assert_eq!(header_state(&screen), Some(HeaderState::Scanning));
        assert!(
            !header_state(&screen)
                .expect("a header")
                .is(ScanState::Complete)
        );
    }

    #[test]
    fn a_path_with_double_spaces_still_leaves_the_badge_last() {
        let screen = drawn(5, 80, &[(0, 0, " EXCISE  /data/a  b  ◆ COMPLETE")]);

        assert_eq!(header_state(&screen), Some(HeaderState::Complete));
    }

    #[test]
    fn there_is_no_header_state_before_the_header_is_drawn() {
        assert_eq!(header_state(&drawn(5, 80, &[])), None);
        assert_eq!(
            header_state(&drawn(5, 80, &[(0, 0, "Loading...  ◆ COMPLETE")])),
            None
        );
        assert_eq!(
            header_state(&drawn(5, 80, &[(0, 0, " EXCISE  /tmp/x")])),
            None
        );
    }

    #[test]
    fn the_completed_word_in_the_inspector_is_not_a_header_state() {
        let screen = drawn(
            5,
            80,
            &[
                (0, 0, " EXCISE  /tmp/x  ◌ SCANNING"),
                (3, 0, "◆ COMPLETE · file"),
            ],
        );

        assert!(
            !header_state(&screen)
                .expect("a header")
                .is(ScanState::Complete)
        );
    }

    fn inspector_screen(lines: &[&str]) -> Screen {
        let mut screen = Screen::new(20, 60);
        place(&mut screen, 10, 0, &unicode_box(INSPECTOR_TITLE, 60, lines));
        screen
    }

    #[test]
    fn the_inspector_reports_the_selected_entry() {
        let screen = inspector_screen(&["victim", "◆ COMPLETE · folder", ""]);

        assert_eq!(
            inspector(&screen),
            Inspector::Item(SelectedItem {
                name: "victim".to_owned(),
                state: "◆ COMPLETE".to_owned(),
                kind: "folder".to_owned(),
            })
        );
    }

    #[test]
    fn the_inspector_reads_the_ascii_state_line() {
        let mut screen = Screen::new(20, 60);
        place(
            &mut screen,
            10,
            0,
            &ascii_box(INSPECTOR_TITLE, 60, &["keep-a.bin", "C COMPLETE . file"]),
        );

        let Inspector::Item(item) = inspector(&screen) else {
            panic!("an item was selected");
        };

        assert_eq!(
            (item.name.as_str(), item.kind.as_str()),
            ("keep-a.bin", "file")
        );
    }

    #[test]
    fn the_inspector_can_show_nothing_or_be_absent() {
        let nothing = inspector_screen(&["Choose an item to see its space and scan status."]);

        assert_eq!(inspector(&nothing), Inspector::NothingSelected);
        assert_eq!(inspector(&drawn(20, 60, &[])), Inspector::NotShown);
    }

    #[test]
    fn a_selection_named_like_a_pane_title_is_still_a_name() {
        let screen = inspector_screen(&["SELECTED ITEM", "◆ COMPLETE · file"]);

        assert!(
            matches!(inspector(&screen), Inspector::Item(item) if item.name == "SELECTED ITEM")
        );
    }

    #[test]
    fn the_filter_prompt_reports_the_text_typed_so_far() {
        let screen = drawn(
            5,
            80,
            &[
                (0, 0, " EXCISE  /tmp/x  ◆ COMPLETE"),
                (2, 0, "/ victim_  [Enter] apply  [Esc] cancel"),
            ],
        );

        assert_eq!(
            filter_prompt(&screen),
            Some(FilterPrompt {
                input: "victim".to_owned(),
                error: None
            })
        );
    }

    #[test]
    fn the_filter_prompt_can_be_empty_or_show_an_error() {
        let empty = drawn(5, 80, &[(2, 0, "/ _  [Enter] apply  [Esc] cancel")]);
        assert_eq!(
            filter_prompt(&empty).map(|prompt| prompt.input),
            Some(String::new())
        );

        let rejected = drawn(5, 80, &[(2, 0, "/ [a_  ERROR: invalid glob filter")]);
        assert_eq!(
            filter_prompt(&rejected),
            Some(FilterPrompt {
                input: "[a".to_owned(),
                error: Some("invalid glob filter".to_owned())
            })
        );
    }

    #[test]
    fn the_last_prompt_ending_wins_over_lookalike_text_in_the_filter() {
        let screen = drawn(
            5,
            80,
            &[(2, 0, "/ a_  [Enter] apply_  [Enter] apply  [Esc] cancel")],
        );

        assert_eq!(
            filter_prompt(&screen).map(|prompt| prompt.input),
            Some("a_  [Enter] apply".to_owned())
        );
    }

    #[test]
    fn the_filter_prompt_is_found_behind_the_safety_labels() {
        for (row, input, error) in [
            // An elevated Windows runner, as drawn in CI.
            (
                "! ELEVATED · / _  [Enter] apply  [Esc] cancel · store 2.3M/23.4G",
                "",
                None,
            ),
            (
                "! ELEVATED · ! REDUCED DELETE GUARD · / victim_  [Enter] apply  [Esc] cancel",
                "victim",
                None,
            ),
            (
                "! ELEVATED . ! REDUCED GUARD . / victim_  [Enter] apply  [Esc] cancel",
                "victim",
                None,
            ),
            (
                "! REDUCED DELETE GUARD · / [a_  ERROR: invalid glob filter",
                "[a",
                Some("invalid glob filter"),
            ),
        ] {
            assert_eq!(
                filter_prompt(&drawn(5, 100, &[(2, 0, row)])),
                Some(FilterPrompt {
                    input: input.to_owned(),
                    error: error.map(str::to_owned)
                }),
                "{row}"
            );
        }

        let scanning = drawn(5, 80, &[(2, 0, "! ELEVATED · ~ SCANNING /tmp/x/victim")]);
        assert_eq!(filter_prompt(&scanning), None);
    }

    #[test]
    fn no_prompt_is_reported_when_no_filter_is_open() {
        let screen = drawn(5, 80, &[(2, 0, "Excise · store 5.7K/11312.3G")]);

        assert_eq!(filter_prompt(&screen), None);
    }

    fn dialog_screen(title: &str, body: &[&str]) -> Screen {
        let mut screen = Screen::new(30, 100);
        place(&mut screen, 8, 10, &unicode_box(title, 78, body));
        screen
    }

    #[test]
    fn a_deletion_dialog_reports_its_kind_path_and_confirmation() {
        let screen = dialog_screen(
            "! DELETE FOLDER",
            &[
                "/private/tmp/xh-AbCd/fx/victim",
                "Deletion continues in the background.",
                "Contents are checked before each removal.",
                "[Enter/y] start · [Esc/n] cancel",
            ],
        );

        let DialogView::Delete(dialog) = dialog_view(&screen) else {
            panic!("a deletion dialog");
        };

        assert_eq!(dialog.kind, DeleteKind::Folder);
        assert_eq!(dialog.path, "/private/tmp/xh-AbCd/fx/victim");
        assert_eq!(dialog.confirmation, Confirmation::SingleKey);
    }

    #[test]
    fn the_title_decides_between_file_and_item() {
        for (title, kind) in [
            ("! DELETE FILE", DeleteKind::File),
            ("! DELETE ITEM", DeleteKind::Item),
        ] {
            let screen = dialog_screen(
                title,
                &["/tmp/x/keep-a.bin", "[Enter/y] start · [Esc/n] cancel"],
            );

            assert!(
                matches!(dialog_view(&screen), DialogView::Delete(dialog) if dialog.kind == kind),
                "{title}"
            );
        }
    }

    #[test]
    fn a_dialog_that_wants_a_typed_phrase_is_not_a_single_key_confirmation() {
        let screen = dialog_screen(
            "! DELETE FOLDER",
            &[
                "/tmp/x/evil\\nname",
                "Type this exactly: DELETE 7K3M9P",
                "> _",
                "[Enter] start when exact · [Esc/n] cancel",
            ],
        );

        let DialogView::Delete(dialog) = dialog_view(&screen) else {
            panic!("a deletion dialog");
        };

        assert_eq!(
            dialog.confirmation,
            Confirmation::TypedPhrase("DELETE 7K3M9P".to_owned())
        );
    }

    #[test]
    fn an_unrecognised_confirmation_is_reported_as_unknown() {
        let screen = dialog_screen("! DELETE FOLDER", &["/tmp/x/victim", "Press a key"]);

        assert!(
            matches!(dialog_view(&screen), DialogView::Delete(dialog) if dialog.confirmation == Confirmation::Unknown)
        );
    }

    #[test]
    fn other_dialogs_and_the_absence_of_one_are_told_apart() {
        let quit = dialog_screen(
            "QUIT",
            &["Quit Excise?", "", "[y] Quit", "[Esc/q/n] Keep working"],
        );
        let DialogView::Other(view) = dialog_view(&quit) else {
            panic!("another dialog");
        };
        assert!(offers_plain_quit(&view));

        let busy = dialog_screen(
            "QUIT",
            &[
                "1 deletion check(s) are waiting.",
                "[c] Cancel checks and quit",
            ],
        );
        let DialogView::Other(view) = dialog_view(&busy) else {
            panic!("another dialog");
        };
        assert!(!offers_plain_quit(&view));

        assert_eq!(dialog_view(&drawn(30, 100, &[])), DialogView::None);
    }
}
