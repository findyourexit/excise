//! Verifying the target of a deletion before the confirmation key is sent.
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

use crate::{
    pty::ui::{Confirmation, DeleteDialog, DeleteKind},
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
    match fixture.lstat(&relative) {
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
        if sentinel == &relative || sentinel.starts_with(&format!("{relative}/")) {
            return Err(format!(
                "the target `{relative}` contains the sentinel `{sentinel}`, which must survive"
            ));
        }
    }
    Ok(Verified { relative })
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
}
