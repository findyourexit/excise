//! Fixture snapshots: proof that a run changed nothing but its intended deletions.
//!
//! The fixture generator's oracle compares a tree with the manifest it was generated from. This
//! module answers a different question: what did one run change, for any fixture, including the
//! deletions and mutations the scenario asked for? It fingerprints a fixture by walking it: each
//! entry's kind, its length for a regular file, and its target for a symbolic link. Modification
//! times are ignored, because removing an entry legitimately changes its parent directory's. The
//! walk holds every path in memory and addresses entries by path, so it is meant for the small
//! fixtures of the lifecycle scenarios, not for million-entry or deeper-than-`PATH_MAX` trees.

use std::{
    collections::BTreeMap,
    fmt::Write as _,
    fs, io,
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};
use thiserror::Error;

/// A fixture could not be walked.
#[derive(Debug, Error)]
#[error("cannot walk the fixture at `{}`: {source}", path.display())]
pub struct SnapshotError {
    path: PathBuf,
    source: io::Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Fingerprint {
    File {
        len: u64,
    },
    Directory,
    /// A directory the hostile fixture class made unreadable on purpose. It contributes no
    /// descendants, the same rule the fixture generator's oracle applies to one it cannot list.
    UnreadableDirectory,
    Symlink {
        target: String,
    },
    Other,
}

impl Fingerprint {
    fn describe(&self) -> String {
        match self {
            Self::File { len } => format!("file, {len} bytes"),
            Self::Directory => "directory".to_owned(),
            Self::UnreadableDirectory => "unreadable directory".to_owned(),
            Self::Symlink { target } => format!("symbolic link to {target}"),
            Self::Other => "special file".to_owned(),
        }
    }
}

/// The fingerprint of every entry below a fixture root, keyed by `/`-separated relative path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixtureSnapshot {
    entries: BTreeMap<String, Fingerprint>,
}

/// What changed between two snapshots of one fixture.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FixtureDiff {
    /// Entries present before and gone now.
    pub removed: Vec<String>,
    /// Entries that were not there before.
    pub added: Vec<String>,
    /// Entries whose kind, length, or link target differs.
    pub changed: Vec<Change>,
}

/// An entry whose kind, length, or link target differs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// The `/`-separated path below the fixture root.
    pub path: String,
    /// What the entry was.
    pub before: String,
    /// What it is now.
    pub after: String,
}

impl std::fmt::Display for Change {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} ({} became {})",
            self.path, self.before, self.after
        )
    }
}

impl FixtureDiff {
    /// Whether nothing changed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.removed.is_empty() && self.added.is_empty() && self.changed.is_empty()
    }

    /// The changes that no intended deletion or mutation explains, one message each.
    ///
    /// A removal is expected when the entry is one of `deletions` or lies below one of them. A
    /// mutation explains what it did at its path, below it, and in the directories it created above
    /// it: additions, removals, and changes alike. Anything else that was removed, added, or
    /// changed is unexpected.
    #[must_use]
    pub fn unexpected(&self, deletions: &[String], mutations: &[String]) -> Vec<String> {
        let below = |path: &str, base: &str| {
            path == base
                || path
                    .strip_prefix(base)
                    .is_some_and(|rest| rest.starts_with('/'))
        };
        let deleted = |path: &str| deletions.iter().any(|base| below(path, base));
        let mutated = |path: &str| {
            mutations
                .iter()
                .any(|mutation| below(path, mutation) || below(mutation, path))
        };
        let mut messages = Vec::new();
        messages.extend(
            self.removed
                .iter()
                .filter(|path| !deleted(path) && !mutated(path))
                .map(|path| format!("removed without a confirmed deletion: {path}")),
        );
        messages.extend(
            self.added
                .iter()
                .filter(|path| !mutated(path))
                .map(|path| format!("appeared: {path}")),
        );
        messages.extend(
            self.changed
                .iter()
                .filter(|change| !mutated(&change.path))
                .map(|change| format!("changed: {change}")),
        );
        messages
    }
}

impl FixtureSnapshot {
    /// Walks `root` without following symbolic links.
    ///
    /// A directory discovered below the root that cannot be listed (the hostile fixture class
    /// makes some on purpose) is recorded as [`Fingerprint::UnreadableDirectory`] and contributes
    /// no descendants, the same rule the fixture generator's oracle applies. Only the root itself
    /// must be listable.
    ///
    /// # Errors
    ///
    /// Returns an error if the root cannot be read or an entry cannot be inspected.
    pub fn take(root: &Path) -> Result<Self, SnapshotError> {
        let error = |path: &Path, source: io::Error| SnapshotError {
            path: path.to_path_buf(),
            source,
        };
        let mut entries = BTreeMap::new();
        let mut pending = vec![(root.to_path_buf(), String::new())];
        while let Some((directory, prefix)) = pending.pop() {
            let listing = match fs::read_dir(&directory) {
                Ok(listing) => listing,
                Err(_source) if !prefix.is_empty() => {
                    entries.insert(prefix, Fingerprint::UnreadableDirectory);
                    continue;
                }
                Err(source) => return Err(error(&directory, source)),
            };
            for entry in listing {
                let entry = entry.map_err(|source| error(&directory, source))?;
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().into_owned();
                let relative = if prefix.is_empty() {
                    name
                } else {
                    format!("{prefix}/{name}")
                };
                let kind = entry.file_type().map_err(|source| error(&path, source))?;
                let fingerprint = if kind.is_symlink() {
                    let target = fs::read_link(&path).map_err(|source| error(&path, source))?;
                    Fingerprint::Symlink {
                        target: target.to_string_lossy().into_owned(),
                    }
                } else if kind.is_dir() {
                    pending.push((path, relative.clone()));
                    Fingerprint::Directory
                } else if kind.is_file() {
                    let metadata = entry.metadata().map_err(|source| error(&path, source))?;
                    Fingerprint::File {
                        len: metadata.len(),
                    }
                } else {
                    Fingerprint::Other
                };
                entries.insert(relative, fingerprint);
            }
        }
        Ok(Self { entries })
    }

    /// The number of entries, the root not included.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the fixture has no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether `relative` is an entry of the fixture.
    #[must_use]
    pub fn contains(&self, relative: &str) -> bool {
        self.entries.contains_key(relative)
    }

    /// Every path in the snapshot, `/`-separated, in path order.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }

    /// A stable identity of the fixture's shape: the lowercase hexadecimal SHA-256 of every path
    /// with its fingerprint, in path order.
    #[must_use]
    pub fn digest(&self) -> String {
        let mut hasher = Sha256::new();
        for (path, fingerprint) in &self.entries {
            hasher.update(path.as_bytes());
            hasher.update([0]);
            hasher.update(fingerprint.describe().as_bytes());
            hasher.update(b"\n");
        }
        let mut hex = String::with_capacity(64);
        for byte in hasher.finalize() {
            let _ = write!(hex, "{byte:02x}");
        }
        hex
    }

    /// What differs in `later` compared with this snapshot.
    #[must_use]
    pub fn diff(&self, later: &Self) -> FixtureDiff {
        let mut diff = FixtureDiff::default();
        for (path, before) in &self.entries {
            match later.entries.get(path) {
                None => diff.removed.push(path.clone()),
                Some(after) if after != before => diff.changed.push(Change {
                    path: path.clone(),
                    before: before.describe(),
                    after: after.describe(),
                }),
                Some(_) => {}
            }
        }
        diff.added = later
            .entries
            .keys()
            .filter(|path| !self.entries.contains_key(*path))
            .cloned()
            .collect();
        diff
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> tempfile::TempDir {
        let root = tempfile::tempdir().expect("a temporary directory");
        fs::create_dir_all(root.path().join("victim/nested")).expect("directories");
        fs::write(root.path().join("victim/a.bin"), vec![0; 10]).expect("a file");
        fs::write(root.path().join("victim/nested/b.bin"), vec![0; 20]).expect("a file");
        fs::write(root.path().join("keep.txt"), b"keep").expect("a file");
        root
    }

    #[test]
    fn an_untouched_fixture_has_an_empty_diff_and_a_stable_digest() {
        let root = fixture();

        let before = FixtureSnapshot::take(root.path()).expect("a snapshot");
        let after = FixtureSnapshot::take(root.path()).expect("a snapshot");

        assert_eq!(before.len(), 5);
        assert!(before.diff(&after).is_empty());
        assert_eq!(before.digest(), after.digest());
        assert_eq!(before.digest().len(), 64);
    }

    #[test]
    fn a_confirmed_deletion_of_a_subtree_is_expected() {
        let root = fixture();
        let before = FixtureSnapshot::take(root.path()).expect("a snapshot");
        fs::remove_dir_all(root.path().join("victim")).expect("delete the victim");
        let after = FixtureSnapshot::take(root.path()).expect("a snapshot");

        let diff = before.diff(&after);

        assert_eq!(
            diff.removed,
            [
                "victim",
                "victim/a.bin",
                "victim/nested",
                "victim/nested/b.bin"
            ]
        );
        assert!(diff.unexpected(&["victim".to_owned()], &[]).is_empty());
        assert_ne!(before.digest(), after.digest());
    }

    #[test]
    fn a_removal_outside_the_confirmed_deletions_is_unexpected() {
        let root = fixture();
        let before = FixtureSnapshot::take(root.path()).expect("a snapshot");
        fs::remove_file(root.path().join("keep.txt")).expect("remove a sentinel");
        let after = FixtureSnapshot::take(root.path()).expect("a snapshot");

        let unexpected = before.diff(&after).unexpected(&["victim".to_owned()], &[]);

        assert_eq!(
            unexpected,
            ["removed without a confirmed deletion: keep.txt"]
        );
    }

    #[test]
    fn a_name_that_merely_starts_like_a_deleted_folder_is_not_covered() {
        let root = fixture();
        fs::write(root.path().join("victim.txt"), b"neighbour").expect("a neighbour");
        let before = FixtureSnapshot::take(root.path()).expect("a snapshot");
        fs::remove_file(root.path().join("victim.txt")).expect("remove the neighbour");
        let after = FixtureSnapshot::take(root.path()).expect("a snapshot");

        let unexpected = before.diff(&after).unexpected(&["victim".to_owned()], &[]);

        assert_eq!(
            unexpected,
            ["removed without a confirmed deletion: victim.txt"]
        );
    }

    #[test]
    fn a_mutation_explains_its_own_path_and_the_directories_it_created_but_nothing_else() {
        let root = fixture();
        let before = FixtureSnapshot::take(root.path()).expect("a snapshot");
        fs::create_dir(root.path().join("added")).expect("a new directory");
        fs::write(root.path().join("added/note.bin"), b"x").expect("a new file");
        fs::write(root.path().join("keep.txt"), b"longer content").expect("a rewritten file");
        fs::write(root.path().join("stray.txt"), b"x").expect("an unrelated file");
        let after = FixtureSnapshot::take(root.path()).expect("a snapshot");
        let mutations = ["added/note.bin".to_owned(), "keep.txt".to_owned()];

        let unexpected = before.diff(&after).unexpected(&[], &mutations);

        assert_eq!(unexpected, ["appeared: stray.txt"]);
    }

    #[test]
    fn additions_and_changes_are_always_unexpected() {
        let root = fixture();
        let before = FixtureSnapshot::take(root.path()).expect("a snapshot");
        fs::write(root.path().join("new.txt"), b"x").expect("a new file");
        fs::write(root.path().join("keep.txt"), b"longer content").expect("a rewritten file");
        let after = FixtureSnapshot::take(root.path()).expect("a snapshot");

        let unexpected = before.diff(&after).unexpected(&["victim".to_owned()], &[]);

        assert_eq!(
            unexpected,
            [
                "appeared: new.txt",
                "changed: keep.txt (file, 4 bytes became file, 14 bytes)",
            ]
        );
    }

    /// The invariant an interrupted deletion (a signal mid-flight, before `X3`) relies on: a
    /// confirmed target excuses a removal anywhere below it, but never an addition or a change,
    /// even below that same target. This is what proves a deletion stopped at an entry boundary:
    /// every entry of the target is either untouched or gone, never a changed or a freshly
    /// created one, such as a private placeholder name a half-finished cleanup might leave.
    #[test]
    fn a_new_or_changed_entry_below_a_confirmed_deletion_target_is_still_unexpected() {
        let root = fixture();
        let before = FixtureSnapshot::take(root.path()).expect("a snapshot");
        // The deletion stopped partway through `victim`: one entry is gone (excused), but it also
        // left one entry changed in place and one brand new entry, both still below the target.
        fs::remove_file(root.path().join("victim/nested/b.bin")).expect("one entry is gone");
        fs::write(root.path().join("victim/a.bin"), vec![0; 999])
            .expect("one entry changed in place");
        fs::write(root.path().join("victim/nested/placeholder"), b"x")
            .expect("one new entry appeared");
        let after = FixtureSnapshot::take(root.path()).expect("a snapshot");

        let unexpected = before.diff(&after).unexpected(&["victim".to_owned()], &[]);

        assert_eq!(
            unexpected,
            [
                "appeared: victim/nested/placeholder",
                "changed: victim/a.bin (file, 10 bytes became file, 999 bytes)",
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn symbolic_links_are_fingerprinted_by_target_and_never_followed() {
        let root = fixture();
        let outside = tempfile::tempdir().expect("an outside directory");
        fs::write(outside.path().join("secret"), b"x").expect("a file outside");
        std::os::unix::fs::symlink(outside.path(), root.path().join("link")).expect("a link");

        let snapshot = FixtureSnapshot::take(root.path()).expect("a snapshot");

        assert!(snapshot.contains("link"));
        assert!(!snapshot.contains("link/secret"));
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_nested_directory_contributes_no_descendants_and_does_not_fail_the_walk() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = fixture();
        let locked = root.path().join("victim/locked");
        fs::create_dir(&locked).expect("a directory to lock");
        fs::write(locked.join("hidden.bin"), b"x").expect("a file that will become unreadable");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000))
            .expect("a restrictive mode");

        let first = FixtureSnapshot::take(root.path());
        let second = FixtureSnapshot::take(root.path());
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o700))
            .expect("the probe directory must stay removable");
        let first = first.expect("an unreadable nested directory does not fail the walk");
        let second = second.expect("an unreadable nested directory does not fail the walk");
        assert!(first.contains("victim/locked"));
        assert!(!first.contains("victim/locked/hidden.bin"));

        assert!(
            first.diff(&second).is_empty(),
            "two snapshots of the same unreadable directory produce the same fingerprint"
        );
    }
}
