//! Verifying the target of a deletion before the key that starts it is sent.
//!
//! The deletion dialog is the ground truth of what `excise` will delete: it captures its target
//! when it opens and confirming acts on that target, whatever the selection does afterwards. So
//! the `delete` step reads the dialog and confirms only when everything below holds. Any single
//! failure refuses, and `y` is never sent.
//!
//! 1. The dialog's title names the requested kind: `! DELETE FOLDER` for a folder, `! DELETE FILE`
//!    for a file. `! DELETE ITEM` is never a match.
//! 2. The dialog is confirmed by one key. A dialog that asks for a typed phrase is refused, because
//!    this runner would have to guess how to answer it.
//! 3. The dialog shows the entry's whole path. `excise` cuts a long path with an ellipsis at the
//!    end, which removes the name, so a truncated path cannot prove anything and is refused. Place
//!    the fixture at a shorter path.
//! 4. The path is the canonical fixture root, a separator, and any directories, ending in exactly
//!    the requested name. A name that merely starts like the requested one, or a root that merely
//!    starts like the fixture root, does not match. The comparison uses `excise`'s display
//!    encoding, which doubles every backslash, including the separators of a Windows path. A name
//!    that `excise` shows escaped (a backslash, a control character, or a deceptive character) is
//!    refused; no fixture-relative path holds one.
//! 5. The entry exists on disk and has the requested kind, looked up without following links.
//! 6. Every sentinel exists, and no sentinel is the target or lies inside it.
//!
//! A scenario that sets `disable_delete_confirmation` has no dialog, and Backspace alone starts the
//! deletion. [`verify_selection`] then checks what it can before Backspace: the selected-item panel
//! shows an entry of the requested name and kind, the entry the step names exists on disk with
//! that kind (rules 5 and 6), and no sentinel is it or inside it. Nothing shows where the selected
//! entry lives, so that is taken from the scenario's `path`, and the panel's name and kind prove
//! it only when they point at one entry in the whole fixture:
//!
//! 7. The entry is the only one of its name and kind in the fixture. With two, the panel cannot
//!    say which is selected, and Backspace would delete that one, whatever `path` says.

use crate::{
    pty::ui::{Confirmation, DeleteDialog, DeleteKind, SelectedItem},
    safety::FixtureRoot,
    scenario::EntryKind,
};

/// The character `excise` ends a cut path with.
const ELLIPSIS: char = '…';

/// The entry a verified dialog names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Verified {
    /// The entry's fixture-relative path, `/`-separated.
    pub relative: String,
}

/// Why a deletion was refused.
pub(crate) type Refusal = String;

/// Checks the dialog against the request. `Ok` means it is safe to press the confirmation key.
pub(crate) fn verify(
    dialog: &DeleteDialog,
    name: &str,
    kind: EntryKind,
    fixture: &FixtureRoot,
    sentinels: &[String],
) -> Result<Verified, Refusal> {
    match (kind, dialog.kind) {
        (EntryKind::Folder, DeleteKind::Folder) | (EntryKind::File, DeleteKind::File) => {}
        (_, DeleteKind::Item) => {
            return Err(format!(
                "the dialog is titled `{}`, a summary item, but the step deletes a {kind}",
                dialog.view.title
            ));
        }
        _ => {
            return Err(format!(
                "the dialog is titled `{}`, but the step deletes a {kind}",
                dialog.view.title
            ));
        }
    }
    match &dialog.confirmation {
        Confirmation::SingleKey => {}
        Confirmation::TypedPhrase(phrase) => {
            return Err(format!(
                "the dialog asks for the typed phrase {phrase:?}, which this runner does not type"
            ));
        }
        Confirmation::Unknown => {
            return Err("the dialog offers no confirmation this runner recognises".to_owned());
        }
    }

    let relative = relative_path(&dialog.path, fixture, name)?;
    verify_entry(&relative, kind, fixture, sentinels)?;
    Ok(Verified { relative })
}

/// Checks the selection against the request, for a deletion that no dialog confirms. `relative`
/// is where the scenario says the entry is. `Ok` means it is safe to press Backspace.
pub(crate) fn verify_selection(
    selected: &SelectedItem,
    name: &str,
    kind: EntryKind,
    relative: &str,
    fixture: &FixtureRoot,
    sentinels: &[String],
) -> Result<Verified, Refusal> {
    if selected.name != name {
        return Err(format!(
            "the selected-item panel shows `{}`, but the step deletes `{name}`",
            selected.name
        ));
    }
    if selected.kind != kind.to_string() {
        return Err(format!(
            "the selected-item panel shows a {} named `{name}`, but the step deletes a {kind}",
            selected.kind
        ));
    }
    verify_entry(relative, kind, fixture, sentinels)?;
    // Rule 7: the panel's name and kind can only mean the entry the step names.
    fixture.require_only_entry(name, kind, relative)?;
    Ok(Verified {
        relative: relative.to_owned(),
    })
}

/// Rules 5 and 6: the entry is on disk with the requested kind, every sentinel is there, and none
/// is the entry or inside it.
fn verify_entry(
    relative: &str,
    kind: EntryKind,
    fixture: &FixtureRoot,
    sentinels: &[String],
) -> Result<(), Refusal> {
    match fixture.lstat(relative) {
        Ok(Some(metadata)) => {
            let is_folder = metadata.is_dir();
            if is_folder != (kind == EntryKind::Folder) {
                return Err(format!(
                    "`{relative}` is a {} on disk, but the step deletes a {kind}",
                    if is_folder { "folder" } else { "file" }
                ));
            }
        }
        Ok(None) => return Err(format!("`{relative}` does not exist in the fixture")),
        Err(error) => return Err(format!("`{relative}` cannot be looked up safely: {error}")),
    }

    for sentinel in sentinels {
        match fixture.exists(sentinel) {
            Ok(true) => {}
            Ok(false) => return Err(format!("the sentinel `{sentinel}` is already missing")),
            Err(error) => {
                return Err(format!(
                    "the sentinel `{sentinel}` cannot be checked: {error}"
                ));
            }
        }
        if sentinel == relative || sentinel.starts_with(&format!("{relative}/")) {
            return Err(format!(
                "the target `{relative}` contains the sentinel `{sentinel}`, which must survive"
            ));
        }
    }
    Ok(())
}

/// The fixture-relative path the dialog's path line names, if it names an entry called `name`
/// under the fixture root.
fn relative_path(shown: &str, fixture: &FixtureRoot, name: &str) -> Result<String, Refusal> {
    if shown.ends_with(ELLIPSIS) {
        return Err(format!(
            "the dialog cuts the path short (`{shown}`), so it cannot show which entry is \
             deleted; place the fixture at a shorter path, for example with `EXCISE_E2E_TMPDIR`"
        ));
    }
    let separator = displayed(std::path::MAIN_SEPARATOR_STR);
    let root = fixture.path().to_string_lossy().into_owned();
    // On Windows a canonical path starts with `\\?\`; accept the path with or without it.
    let roots = [
        displayed(&root),
        displayed(root.trim_start_matches(r"\\?\")),
    ];
    let rest = roots
        .iter()
        .find_map(|root| shown.strip_prefix(root.as_str()))
        .and_then(|rest| rest.strip_prefix(separator.as_str()))
        .ok_or_else(|| {
            format!("the dialog's path `{shown}` is not below the fixture root `{root}`")
        })?;
    let components: Vec<&str> = rest.split(separator.as_str()).collect();
    if let Some(escaped) = components.iter().find(|component| component.contains('\\')) {
        return Err(format!(
            "the dialog's path `{shown}` escapes a character in `{escaped}`, so it cannot show \
             which entry is deleted"
        ));
    }
    if components.iter().any(|component| component.is_empty()) {
        return Err(format!(
            "the dialog's path `{shown}` has an empty component"
        ));
    }
    if components.last() != Some(&name) {
        return Err(format!(
            "the dialog deletes `{}`, but the step deletes `{name}`",
            components.last().copied().unwrap_or_default()
        ));
    }
    Ok(components.join("/"))
}

/// `text` as `excise` displays text that holds no control or deceptive character: every
/// backslash doubled (`escape_valid_text` in `src/native_path.rs`).
fn displayed(text: &str) -> String {
    text.replace('\\', r"\\")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::{
        fixture::MARKER_FILE_NAME,
        pty::{BoxRect, BoxView},
    };

    struct Fixture {
        _dir: tempfile::TempDir,
        root: FixtureRoot,
        sentinels: Vec<String>,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().expect("a temporary directory");
        fs::write(dir.path().join(MARKER_FILE_NAME), b"owned").expect("marker");
        fs::create_dir_all(dir.path().join("victim/nested")).expect("victim");
        fs::write(dir.path().join("victim/a.bin"), b"x").expect("file");
        fs::create_dir_all(dir.path().join("sub/victim")).expect("nested victim");
        fs::write(dir.path().join("victim.txt"), b"neighbour").expect("neighbour");
        fs::write(dir.path().join("other.bin"), b"other").expect("a plain file");
        fs::write(dir.path().join("keep-a.bin"), b"keep").expect("sentinel");
        fs::create_dir(dir.path().join("keep-b")).expect("sentinel folder");
        fs::write(dir.path().join("keep-b/keep.txt"), b"keep").expect("sentinel");
        let root = FixtureRoot::open(dir.path()).expect("a marked root");
        Fixture {
            _dir: dir,
            root,
            sentinels: vec![
                "keep-a.bin".into(),
                "keep-b/keep.txt".into(),
                "victim.txt".into(),
            ],
        }
    }

    fn dialog(kind: DeleteKind, path: &str, confirmation: Confirmation) -> DeleteDialog {
        let title = match kind {
            DeleteKind::Folder => "! DELETE FOLDER",
            DeleteKind::File => "! DELETE FILE",
            DeleteKind::Item => "! DELETE ITEM",
        };
        DeleteDialog {
            kind,
            path: path.to_owned(),
            confirmation,
            view: BoxView {
                title: title.to_owned(),
                rect: BoxRect {
                    left: 0,
                    top: 0,
                    right: 0,
                    bottom: 0,
                },
                interior: Vec::new(),
                text: String::new(),
            },
        }
    }

    /// `relative`, `/`-separated, under the fixture root, as `excise` displays it.
    fn shown(fixture: &Fixture, relative: &str) -> String {
        let path = relative
            .split('/')
            .fold(fixture.root.path().to_path_buf(), |path, component| {
                path.join(component)
            });
        displayed(&path.to_string_lossy())
    }

    fn check(
        fixture: &Fixture,
        dialog: &DeleteDialog,
        name: &str,
        kind: EntryKind,
    ) -> Result<Verified, Refusal> {
        verify(dialog, name, kind, &fixture.root, &fixture.sentinels)
    }

    #[test]
    fn the_requested_folder_is_confirmed() {
        let fixture = fixture();
        let dialog = dialog(
            DeleteKind::Folder,
            &shown(&fixture, "victim"),
            Confirmation::SingleKey,
        );

        let verified = check(&fixture, &dialog, "victim", EntryKind::Folder).expect("a match");

        assert_eq!(verified.relative, "victim");
    }

    #[test]
    fn a_target_below_other_folders_is_still_under_the_fixture_root() {
        let fixture = fixture();
        let dialog = dialog(
            DeleteKind::Folder,
            &shown(&fixture, "sub/victim"),
            Confirmation::SingleKey,
        );

        let verified = check(&fixture, &dialog, "victim", EntryKind::Folder).expect("a match");

        assert_eq!(verified.relative, "sub/victim");
    }

    #[test]
    fn a_file_target_is_confirmed_when_the_dialog_says_file() {
        let fixture = fixture();
        let dialog = dialog(
            DeleteKind::File,
            &shown(&fixture, "other.bin"),
            Confirmation::SingleKey,
        );

        let verified = check(&fixture, &dialog, "other.bin", EntryKind::File).expect("a match");

        assert_eq!(verified.relative, "other.bin");
    }

    #[test]
    fn the_neighbouring_sentinel_can_be_seen_but_never_deleted() {
        let fixture = fixture();
        let dialog = dialog(
            DeleteKind::File,
            &shown(&fixture, "victim.txt"),
            Confirmation::SingleKey,
        );

        let refusal =
            check(&fixture, &dialog, "victim.txt", EntryKind::File).expect_err("a sentinel");

        assert!(
            refusal.contains("victim.txt") && refusal.contains("must survive"),
            "{refusal}"
        );
    }

    #[test]
    fn the_wrong_name_is_refused_even_when_it_sorts_next_to_the_right_one() {
        let fixture = fixture();
        for wrong in ["victim.txt", "victim-2", "victi", "vic"] {
            let dialog = dialog(
                DeleteKind::Folder,
                &shown(&fixture, wrong),
                Confirmation::SingleKey,
            );

            let refusal = check(&fixture, &dialog, "victim", EntryKind::Folder).expect_err(wrong);

            assert!(
                refusal.contains("deletes `") || refusal.contains("victim"),
                "{wrong}: {refusal}"
            );
        }
    }

    #[test]
    fn the_wrong_kind_is_refused_from_the_title_and_from_the_disk() {
        let fixture = fixture();

        let by_title = dialog(
            DeleteKind::File,
            &shown(&fixture, "victim"),
            Confirmation::SingleKey,
        );
        assert!(
            check(&fixture, &by_title, "victim", EntryKind::Folder)
                .expect_err("wrong title")
                .contains("titled")
        );

        let item = dialog(
            DeleteKind::Item,
            &shown(&fixture, "victim"),
            Confirmation::SingleKey,
        );
        assert!(
            check(&fixture, &item, "victim", EntryKind::Folder)
                .expect_err("a summary item")
                .contains("summary item")
        );

        // The dialog and the request agree that this is a file, but on disk it is a folder.
        let by_disk = dialog(
            DeleteKind::File,
            &shown(&fixture, "victim"),
            Confirmation::SingleKey,
        );
        assert!(
            check(&fixture, &by_disk, "victim", EntryKind::File)
                .expect_err("wrong kind on disk")
                .contains("is a folder on disk")
        );
    }

    #[test]
    fn a_dialog_that_needs_a_typed_phrase_or_an_unknown_answer_is_refused() {
        let fixture = fixture();
        let path = shown(&fixture, "victim");

        let typed = dialog(
            DeleteKind::Folder,
            &path,
            Confirmation::TypedPhrase("DELETE 7K3M9P".into()),
        );
        assert!(
            check(&fixture, &typed, "victim", EntryKind::Folder)
                .expect_err("typed")
                .contains("typed phrase")
        );

        let unknown = dialog(DeleteKind::Folder, &path, Confirmation::Unknown);
        assert!(
            check(&fixture, &unknown, "victim", EntryKind::Folder)
                .expect_err("unknown")
                .contains("recognises")
        );
    }

    #[test]
    fn a_path_cut_short_proves_nothing_and_is_refused() {
        let fixture = fixture();
        let full = shown(&fixture, "victim");
        let cut = format!("{}…", &full[..full.len() - 4]);
        let dialog = dialog(DeleteKind::Folder, &cut, Confirmation::SingleKey);

        let refusal = check(&fixture, &dialog, "victim", EntryKind::Folder).expect_err("cut path");

        assert!(refusal.contains("shorter path"), "{refusal}");
    }

    #[test]
    fn a_path_outside_the_fixture_root_is_refused() {
        let fixture = fixture();
        let root = displayed(&fixture.root.path().to_string_lossy());
        let separator = displayed(std::path::MAIN_SEPARATOR_STR);

        for path in [
            "/etc/victim".to_owned(),
            format!("{root}-elsewhere{separator}victim"),
            format!("{root}victim"),
            root.clone(),
            format!("{root}{separator}{separator}victim"),
        ] {
            let dialog = dialog(DeleteKind::Folder, &path, Confirmation::SingleKey);

            assert!(
                check(&fixture, &dialog, "victim", EntryKind::Folder).is_err(),
                "{path}"
            );
        }
    }

    #[test]
    fn a_name_that_excise_shows_escaped_is_refused() {
        let fixture = fixture();
        let root = displayed(&fixture.root.path().to_string_lossy());
        let separator = displayed(std::path::MAIN_SEPARATOR_STR);
        let mut names = vec![r"vic\ntim", r"vic\u{202e}tim", r"victim\"];
        // On Unix a backslash can be part of a name, and `excise` shows it doubled.
        if cfg!(unix) {
            names.push(r"vic\\tim");
        }

        for name in names {
            let path = format!("{root}{separator}{name}");
            let dialog = dialog(DeleteKind::Folder, &path, Confirmation::SingleKey);

            let refusal =
                check(&fixture, &dialog, "victim", EntryKind::Folder).expect_err("an escape");

            assert!(refusal.contains("escapes a character"), "{refusal}");
        }
    }

    #[test]
    fn an_entry_that_is_not_on_disk_is_refused() {
        let fixture = fixture();
        let dialog = dialog(
            DeleteKind::Folder,
            &shown(&fixture, "gone/victim"),
            Confirmation::SingleKey,
        );

        assert!(
            check(&fixture, &dialog, "victim", EntryKind::Folder)
                .expect_err("missing")
                .contains("does not exist")
        );
    }

    #[test]
    fn a_missing_sentinel_is_refused_before_confirming() {
        let mut fixture = fixture();
        fixture.sentinels.push("keep-c.bin".to_owned());
        let dialog = dialog(
            DeleteKind::Folder,
            &shown(&fixture, "victim"),
            Confirmation::SingleKey,
        );

        let refusal =
            check(&fixture, &dialog, "victim", EntryKind::Folder).expect_err("missing sentinel");

        assert!(
            refusal.contains("keep-c.bin") && refusal.contains("missing"),
            "{refusal}"
        );
    }

    #[test]
    fn a_target_that_contains_a_sentinel_is_refused() {
        let fixture = fixture();
        let dialog = dialog(
            DeleteKind::Folder,
            &shown(&fixture, "keep-b"),
            Confirmation::SingleKey,
        );

        let refusal =
            check(&fixture, &dialog, "keep-b", EntryKind::Folder).expect_err("contains a sentinel");

        assert!(refusal.contains("keep-b/keep.txt"), "{refusal}");
    }

    fn selected(name: &str, kind: &str) -> SelectedItem {
        SelectedItem {
            name: name.to_owned(),
            state: "◆ COMPLETE".to_owned(),
            kind: kind.to_owned(),
        }
    }

    fn check_selection(
        fixture: &Fixture,
        selected: &SelectedItem,
        name: &str,
        kind: EntryKind,
        relative: &str,
    ) -> Result<Verified, Refusal> {
        verify_selection(
            selected,
            name,
            kind,
            relative,
            &fixture.root,
            &fixture.sentinels,
        )
    }

    #[test]
    fn a_selection_that_matches_the_request_and_the_disk_is_cleared_for_backspace() {
        let fixture = fixture();

        let verified = check_selection(
            &fixture,
            &selected("other.bin", "file"),
            "other.bin",
            EntryKind::File,
            "other.bin",
        )
        .expect("a match");
        assert_eq!(verified.relative, "other.bin");

        // The panel never shows where an entry is: the step's path says, and the disk is checked.
        let nested = check_selection(
            &fixture,
            &selected("nested", "folder"),
            "nested",
            EntryKind::Folder,
            "victim/nested",
        )
        .expect("a nested match");
        assert_eq!(nested.relative, "victim/nested");
    }

    /// The fixture has a `victim` folder at its root and another below `sub`. The panel shows a
    /// name and a kind, so it cannot say which one is selected, and Backspace deletes that one:
    /// naming either as the entry cannot be confirmed.
    #[test]
    fn a_selection_whose_name_and_kind_fit_two_entries_is_refused_whichever_the_step_names() {
        let fixture = fixture();
        let item = selected("victim", "folder");

        for relative in ["victim", "sub/victim"] {
            let refusal = check_selection(&fixture, &item, "victim", EntryKind::Folder, relative)
                .expect_err("two entries fit the panel");
            assert!(
                refusal.contains("2 folders called `victim` (`sub/victim`, `victim`)"),
                "{relative}: {refusal}"
            );
        }
    }

    #[test]
    fn a_same_named_entry_of_the_other_kind_does_not_make_a_selection_ambiguous() {
        let fixture = fixture();
        // A file called `nested` far below the folder called `nested`: the panel says which kind.
        fs::write(fixture.root.path().join("sub/victim/nested"), b"x").expect("a file");

        let folder = check_selection(
            &fixture,
            &selected("nested", "folder"),
            "nested",
            EntryKind::Folder,
            "victim/nested",
        )
        .expect("the only folder called nested");
        assert_eq!(folder.relative, "victim/nested");

        let file = check_selection(
            &fixture,
            &selected("nested", "file"),
            "nested",
            EntryKind::File,
            "sub/victim/nested",
        )
        .expect("the only file called nested");
        assert_eq!(file.relative, "sub/victim/nested");
    }

    #[test]
    fn a_path_that_is_not_where_the_only_entry_of_that_name_is_is_refused() {
        let fixture = fixture();
        fs::create_dir_all(fixture.root.path().join("sub/elsewhere")).expect("a folder");
        fs::write(fixture.root.path().join("sub/elsewhere/lonely.bin"), b"x").expect("a file");
        fs::write(fixture.root.path().join("lonely"), b"x").expect("a file");

        let refusal = check_selection(
            &fixture,
            &selected("lonely.bin", "file"),
            "lonely.bin",
            EntryKind::File,
            "lonely",
        )
        .expect_err("the step's path is another entry");

        assert!(
            refusal.contains("the only file called `lonely.bin` is `sub/elsewhere/lonely.bin`"),
            "{refusal}"
        );
    }

    #[test]
    fn a_selection_that_is_another_entry_or_another_kind_is_refused() {
        let fixture = fixture();

        let other = check_selection(
            &fixture,
            &selected("other.bin", "file"),
            "victim",
            EntryKind::Folder,
            "victim",
        )
        .expect_err("another entry");
        assert!(
            other.contains("shows `other.bin`, but the step deletes `victim`"),
            "{other}"
        );

        let kind = check_selection(
            &fixture,
            &selected("victim", "file"),
            "victim",
            EntryKind::Folder,
            "victim",
        )
        .expect_err("another kind");
        assert!(
            kind.contains("shows a file named `victim`, but the step deletes a folder"),
            "{kind}"
        );
    }

    #[test]
    fn a_selection_is_held_to_the_disk_and_the_sentinels_as_a_dialog_is() {
        let fixture = fixture();

        // The panel and the step agree on a file, but the entry on disk is a folder.
        let by_disk = check_selection(
            &fixture,
            &selected("victim", "file"),
            "victim",
            EntryKind::File,
            "victim",
        )
        .expect_err("wrong kind on disk");
        assert!(by_disk.contains("is a folder on disk"), "{by_disk}");

        let missing = check_selection(
            &fixture,
            &selected("victim", "folder"),
            "victim",
            EntryKind::Folder,
            "gone/victim",
        )
        .expect_err("missing");
        assert!(missing.contains("does not exist"), "{missing}");

        let holds_sentinel = check_selection(
            &fixture,
            &selected("keep-b", "folder"),
            "keep-b",
            EntryKind::Folder,
            "keep-b",
        )
        .expect_err("holds a sentinel");
        assert!(
            holds_sentinel.contains("keep-b/keep.txt"),
            "{holds_sentinel}"
        );
    }

    #[test]
    fn a_selection_is_refused_while_a_sentinel_is_missing() {
        let mut fixture = fixture();
        fixture.sentinels.push("keep-c.bin".to_owned());

        let refusal = check_selection(
            &fixture,
            &selected("victim", "folder"),
            "victim",
            EntryKind::Folder,
            "victim",
        )
        .expect_err("missing sentinel");

        assert!(
            refusal.contains("keep-c.bin") && refusal.contains("missing"),
            "{refusal}"
        );
    }
}
