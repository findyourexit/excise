//! The root of a soak: a real directory that the harness did not generate.
//!
//! A [`SoakRoot`] is *not* a [`FixtureRoot`](crate::safety::FixtureRoot), and that is the point.
//! Every step and protocol of this crate that deletes or changes a fixture takes a `FixtureRoot`,
//! which only a directory that carries the harness's ownership marker can become: the scenario
//! runner's `delete` and `fs_mutate` steps (`Executor`), the deletion protocol
//! (`DeletionRequest`), and the interactive driver. A `SoakRoot` converts into none of that, so
//! the compiler refuses to hand a real tree to any of it. That is what the type does, and it is
//! all it does: it keeps a `SoakRoot` out of every API that takes a `FixtureRoot`. It is not a
//! seal around the path. A `SoakRoot` prints its path (`Display`, `Debug`,
//! [`SoakRoot::canonical_text`]) because a person must be able to type it, and two helpers of
//! `crate::fixture` delete and write by *path* (`remove_tree`, `write_marker`) and are public.
//! What keeps the soak's own code away from the APIs that take a path is a test that reads the
//! soak's source (`soak/tests.rs`): the modules it may use, and the names it may never mention.
//! The three checks below are the type's, and each is a doctest that the compiler runs.
//!
//! * It is a different type. A function that wants a `FixtureRoot` does not take it:
//!
//!   ```compile_fail,E0308
//!   use excise_harness::{safety::FixtureRoot, soak::SoakRoot};
//!
//!   fn something_that_deletes(_: &FixtureRoot) {}
//!
//!   let root = SoakRoot::open("/").expect("a directory");
//!   something_that_deletes(&root);
//!   ```
//!
//!   The same call with a `FixtureRoot` compiles, so the error above is the type and nothing else:
//!
//!   ```no_run
//!   use excise_harness::safety::FixtureRoot;
//!
//!   fn something_that_deletes(_: &FixtureRoot) {}
//!
//!   let root = FixtureRoot::open("/tmp/a-generated-fixture").expect("a fixture");
//!   something_that_deletes(&root);
//!   ```
//!
//! * It is not a path. It implements neither `AsRef<Path>` nor `Deref`, so it cannot stand in for
//!   the `impl AsRef<Path>` that `FixtureRoot::open` takes either:
//!
//!   ```compile_fail,E0277
//!   use excise_harness::{safety::FixtureRoot, soak::SoakRoot};
//!
//!   let root = SoakRoot::open("/").expect("a directory");
//!   let _ = FixtureRoot::open(root);
//!   ```
//!
//! * The `&Path` is not available outside the soak. The accessor is visible inside `crate::soak`
//!   only (the printed text is public, and is only text), and the code there is the soak's own:
//!   the source test fails if it reaches for a helper that takes a path, a mutator, the deletion
//!   protocol, or Backspace.
//!
//!   ```compile_fail,E0624
//!   use excise_harness::soak::SoakRoot;
//!
//!   let root = SoakRoot::open("/").expect("a directory");
//!   let _ = root.path();
//!   ```
//!
//! What a `SoakRoot` does give is what a person and a command need: its canonical spelling, as
//! text, so that they can type it ([`SoakRoot::is_named_by`]), and whether a path is inside it
//! ([`SoakRoot::contains`]), so that the command can say what it writes in the tree.
//!
//! The spelling is shown to the person as it is and typed back, so it must be something a line can
//! hold and a terminal shows as it is: a directory whose canonical path holds a control character
//! (a line break could never be typed on one line, an escape rewrites the prompt) or a
//! bidirectional control (it reorders what is shown) is refused ([`RootError::Deceptive`]), by the
//! rule `excise` shows paths by. Every other path a command shows goes through
//! [`safe_path_text`], which escapes by the same rule.

use std::{
    fmt, fs, io,
    path::{Component, Path, PathBuf},
};

use thiserror::Error;

use crate::run_support::{is_deceptive, safe_path_text};

/// Why a directory cannot be the root of a soak.
///
/// Every path in a message is shown through [`safe_path_text`], so that a name in the path cannot
/// rewrite the terminal the message is printed on.
#[derive(Debug, Error)]
pub enum RootError {
    /// The directory cannot be inspected: it does not exist, or it cannot be read.
    #[error("cannot inspect `{}`: {source}", safe_path_text(path))]
    Inspect {
        /// The path as it was given.
        path: PathBuf,
        /// The underlying error.
        source: io::Error,
    },
    /// The path is a symbolic link. A soak scans the directory a person names, not wherever a link
    /// happens to point today.
    #[error(
        "`{}` is a symbolic link; name the directory it points to",
        safe_path_text(path)
    )]
    Symlink {
        /// The path as it was given.
        path: PathBuf,
    },
    /// The path is not a directory.
    #[error("`{}` is not a directory", safe_path_text(path))]
    NotADirectory {
        /// The path as it was given.
        path: PathBuf,
    },
    /// The canonical path is not valid text, so a person could not type it to confirm it.
    #[error(
        "the canonical path of `{}` is not valid UTF-8 text, so it cannot be confirmed by typing it",
        safe_path_text(path)
    )]
    NotText {
        /// The path as it was given.
        path: PathBuf,
    },
    /// The canonical path holds a control character (a line break, an escape) or text that changes
    /// the direction a terminal shows it in. The prompt could not show it as it is, and a person
    /// could not type it to confirm it.
    #[error(
        "the canonical path of the directory holds a control character or text that changes how \
         it is shown, so a prompt could not show it as it is and a person could not type it to \
         confirm it: {escaped}"
    )]
    Deceptive {
        /// The canonical path, escaped by [`safe_path_text`].
        escaped: String,
    },
}

/// A directory to soak: canonical, a directory, and not a symbolic link.
///
/// See the [module documentation](self) for what the type is for. It carries no ownership marker
/// and needs none: the soak never changes the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoakRoot {
    path: PathBuf,
    text: String,
}

impl SoakRoot {
    /// Accepts `path` if it is a directory that is not a symbolic link, and spells it canonically.
    ///
    /// The path as given must not be a link (`lstat`), and neither may what it resolves to. A link
    /// further up the path is resolved, as every program does: `/tmp/x` is `/private/tmp/x` on
    /// macOS.
    ///
    /// # Errors
    ///
    /// Returns [`RootError`] for a path that cannot be inspected, is a link, is not a directory,
    /// or whose canonical spelling is not valid text.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, RootError> {
        let given = path.as_ref();
        let inspect = |path: &Path| {
            fs::symlink_metadata(path).map_err(|source| RootError::Inspect {
                path: given.to_path_buf(),
                source,
            })
        };
        let metadata = inspect(given)?;
        if metadata.file_type().is_symlink() {
            return Err(RootError::Symlink {
                path: given.to_path_buf(),
            });
        }
        if !metadata.is_dir() {
            return Err(RootError::NotADirectory {
                path: given.to_path_buf(),
            });
        }
        let canonical = fs::canonicalize(given).map_err(|source| RootError::Inspect {
            path: given.to_path_buf(),
            source,
        })?;
        // What it resolves to must be a directory and not a link either: the check is cheap, and
        // it keeps the type's promise from resting on `canonicalize` alone.
        let resolved = inspect(&canonical)?;
        if resolved.file_type().is_symlink() || !resolved.is_dir() {
            return Err(RootError::NotADirectory {
                path: given.to_path_buf(),
            });
        }
        let text = canonical
            .to_str()
            .ok_or_else(|| RootError::NotText {
                path: given.to_path_buf(),
            })?
            .to_owned();
        if text.chars().any(is_deceptive) {
            return Err(RootError::Deceptive {
                escaped: safe_path_text(&canonical),
            });
        }
        Ok(Self {
            path: canonical,
            text,
        })
    }

    /// The canonical spelling of the root, as the text a person types to confirm it.
    #[must_use]
    pub fn canonical_text(&self) -> &str {
        &self.text
    }

    /// Whether `typed` is exactly the canonical spelling of the root. Nothing is trimmed or
    /// forgiven: a person who types a different path has not confirmed this one. Only the line
    /// ending a terminal adds is the caller's to remove.
    #[must_use]
    pub fn is_named_by(&self, typed: &str) -> bool {
        typed == self.text
    }

    /// The canonical path. Visible inside the soak only: see the [module documentation](self).
    pub(in crate::soak) fn path(&self) -> &Path {
        &self.path
    }

    /// Whether `other` is the root or lies inside it, comparing canonical spellings. A path that
    /// cannot be resolved (it does not exist yet) is resolved through its nearest ancestor that
    /// does, so that a directory the soak is about to make is judged by where it would be, and
    /// a `..` in the part that is not there is judged by where it leads
    /// ([`resolve_through_ancestors`]).
    #[must_use]
    pub fn contains(&self, other: &Path) -> bool {
        resolve_through_ancestors(other).starts_with(&self.path)
    }

    /// Whether the root lies strictly inside `place`: `place` holds the root and is not the root.
    /// `place` is judged where it resolves, as [`SoakRoot::contains`] judges a path. A program that
    /// writes in a directory that holds the root, as a build does in its target directory, may
    /// rewrite what is inside the root, and nothing says which files.
    #[must_use]
    pub fn lies_inside(&self, place: &Path) -> bool {
        let place = resolve_through_ancestors(place);
        self.path != place && self.path.starts_with(&place)
    }
}

impl fmt::Display for SoakRoot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.text)
    }
}

/// The canonical form of `path`, or of its nearest ancestor that exists with the rest appended.
///
/// This is where a path *would* be written: a command that is about to make a directory says what
/// it will write by this spelling, which follows every link that exists, and not by the spelling
/// it was given.
///
/// The path is walked from its start, one component at a time, so that a `..` means what it means
/// to the system. In the part that exists, a `..` is the parent of the directory the walk has
/// reached, every link before it followed: the parent of a link's target, not of the link. Below
/// that, the directories are the ones that making the path would make, so a `..` takes away the
/// component before it, and one that has none left goes up from the part that exists.
/// `missing/../../tree/new` below a directory `repo`, with no `missing` there yet, is `tree/new`
/// beside `repo`: the first thing that writing it does is make `missing`, and the last is a write
/// beside `repo`, which a spelling that kept the `..` would never judge to be so.
///
/// A path that cannot be anchored (an empty path, or a relative one when the working directory
/// cannot be found) is returned as it is.
#[must_use]
pub fn resolve_through_ancestors(path: &Path) -> PathBuf {
    if path.as_os_str().is_empty() {
        return path.to_path_buf();
    }
    let mut components = path.components().peekable();
    // The start: a root or a prefix, and for a relative path the working directory.
    let mut anchor = PathBuf::new();
    while let Some(start @ (Component::Prefix(_) | Component::RootDir)) = components.peek().copied()
    {
        anchor.push(start.as_os_str());
        components.next();
    }
    if anchor.as_os_str().is_empty() {
        anchor.push(".");
    }
    // What the walk has reached: canonical, so that every link in it is followed.
    let Ok(mut resolved) = fs::canonicalize(&anchor) else {
        return path.to_path_buf();
    };
    // What is below that and does not exist: the names that making the path would make.
    let mut missing = Vec::new();
    for component in components {
        match component {
            Component::Normal(name) if missing.is_empty() => {
                match fs::canonicalize(resolved.join(name)) {
                    Ok(canonical) => resolved = canonical,
                    Err(_) => missing.push(name),
                }
            }
            Component::Normal(name) => missing.push(name),
            Component::ParentDir => {
                // Nothing left of the part that is not there: this goes up from the part that
                // is, and a root is its own parent.
                if missing.pop().is_none() {
                    resolved.pop();
                }
            }
            Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
        }
    }
    resolved.extend(missing);
    resolved
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_directory_is_accepted_and_spelled_canonically() {
        let parent = tempfile::tempdir().expect("a temporary directory");
        let root = parent.path().join("tree");
        fs::create_dir(&root).expect("a directory");

        let soak = SoakRoot::open(&root).expect("a directory is a root");

        let canonical = fs::canonicalize(&root).expect("canonical");
        assert_eq!(soak.canonical_text(), canonical.to_str().expect("text"));
        assert_eq!(soak.path(), canonical);
        assert_eq!(soak.to_string(), soak.canonical_text());
    }

    #[test]
    fn a_path_that_is_not_there_or_not_a_directory_is_refused() {
        let parent = tempfile::tempdir().expect("a temporary directory");
        let file = parent.path().join("file");
        fs::write(&file, b"x").expect("a file");

        assert!(matches!(
            SoakRoot::open(parent.path().join("absent")),
            Err(RootError::Inspect { .. })
        ));
        assert!(matches!(
            SoakRoot::open(&file),
            Err(RootError::NotADirectory { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_symbolic_link_is_refused_even_when_it_points_at_a_directory() {
        let parent = tempfile::tempdir().expect("a temporary directory");
        let real = parent.path().join("real");
        let link = parent.path().join("link");
        fs::create_dir_all(real.join("inner")).expect("directories");
        std::os::unix::fs::symlink(&real, &link).expect("a link");

        assert!(matches!(
            SoakRoot::open(&link),
            Err(RootError::Symlink { .. })
        ));
        // What the link points at is a root, and a link further up the path is resolved, as every
        // program resolves it.
        assert!(SoakRoot::open(&real).is_ok());
        let through = SoakRoot::open(link.join("inner")).expect("a directory behind a link");
        assert_eq!(
            through,
            SoakRoot::open(real.join("inner")).expect("the same directory")
        );
    }

    #[test]
    fn only_the_exact_canonical_spelling_confirms_the_root() {
        let parent = tempfile::tempdir().expect("a temporary directory");
        let soak = SoakRoot::open(parent.path()).expect("a root");
        let text = soak.canonical_text().to_owned();

        assert!(soak.is_named_by(&text));
        for typed in [
            String::new(),
            format!("{text}/"),
            format!(" {text}"),
            format!("{text} "),
            format!("{text}\n"),
            text.to_uppercase(),
            text.trim_start_matches('/').to_owned(),
            parent.path().join("..").to_string_lossy().into_owned(),
        ] {
            assert!(
                typed == text || !soak.is_named_by(&typed),
                "`{typed}` must not confirm `{text}`"
            );
        }
    }

    #[test]
    fn a_directory_inside_the_root_is_inside_it_and_one_beside_it_is_not() {
        let parent = tempfile::tempdir().expect("a temporary directory");
        let root = parent.path().join("tree");
        let beside = parent.path().join("beside");
        fs::create_dir_all(root.join("deep")).expect("directories");
        fs::create_dir(&beside).expect("a directory");
        let soak = SoakRoot::open(&root).expect("a root");

        assert!(soak.contains(&root));
        assert!(soak.contains(&root.join("deep")));
        assert!(
            soak.contains(&root.join("deep").join("not").join("yet")),
            "a path that does not exist yet is judged by where it would be"
        );
        assert!(!soak.contains(&beside));
        assert!(!soak.contains(&parent.path().join("treehouse")));
    }

    #[test]
    fn a_root_lies_inside_a_directory_that_holds_it_and_is_not_inside_itself_or_what_it_holds() {
        let parent = tempfile::tempdir().expect("a temporary directory");
        let root = parent.path().join("tree");
        let beside = parent.path().join("beside");
        fs::create_dir_all(root.join("deep")).expect("directories");
        fs::create_dir(&beside).expect("a directory");
        let soak = SoakRoot::open(&root).expect("a root");

        assert!(soak.lies_inside(parent.path()));
        assert!(
            soak.lies_inside(&beside.join("..")),
            "a place is judged where it resolves, not by how it is spelled"
        );
        assert!(!soak.lies_inside(&root), "the root is not inside itself");
        assert!(
            !soak.lies_inside(&root.join("deep")),
            "nor inside what it holds"
        );
        assert!(!soak.lies_inside(&beside));
        assert!(!soak.lies_inside(&parent.path().join("tree-house")));
    }

    /// Names that a terminal would act on rather than show: a line break, a tab, an escape that
    /// clears the screen, the delete and next-line controls, and the bidirectional controls that
    /// reorder what follows.
    #[cfg(unix)]
    const HOSTILE: [&str; 9] = [
        "line\nbreak",
        "return\rhome",
        "tab\there",
        "esc\u{1b}[2Jcleared",
        "del\u{7f}ete",
        "next\u{85}line",
        "evil\u{202e}gpj.exe",
        "isolate\u{2066}x",
        "mark\u{200f}x",
    ];

    #[cfg(unix)]
    #[test]
    fn a_root_whose_canonical_path_holds_a_control_or_a_bidirectional_control_is_refused() {
        let parent = tempfile::tempdir().expect("a temporary directory");
        for name in HOSTILE {
            let hostile = parent.path().join(name);
            fs::create_dir_all(hostile.join("inner")).expect("a hostile directory");

            for given in [hostile.clone(), hostile.join("inner")] {
                let refusal = SoakRoot::open(&given).expect_err(name);

                let RootError::Deceptive { escaped } = &refusal else {
                    panic!("{name:?}: {refusal:?}");
                };
                assert!(escaped.starts_with("[deceptive] "), "{name:?}: {escaped}");
                let shown = refusal.to_string();
                assert!(
                    !shown.chars().any(is_deceptive),
                    "{name:?}: the message would act on a terminal: {shown:?}"
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_rule_is_about_the_canonical_path_that_is_shown_and_typed() {
        // A link whose own name is hostile, to a directory whose path is plain: the root is the
        // directory, spelled plainly, and that is what the prompt shows and the person types.
        let parent = tempfile::tempdir().expect("a temporary directory");
        let real = parent.path().join("real");
        fs::create_dir_all(real.join("inner")).expect("a directory");
        let link = parent.path().join("link\nname");
        std::os::unix::fs::symlink(&real, &link).expect("a link");

        let through = SoakRoot::open(link.join("inner")).expect("a plain canonical path");

        assert!(!through.canonical_text().chars().any(is_deceptive));
        assert_eq!(
            through,
            SoakRoot::open(real.join("inner")).expect("the same directory")
        );
    }

    #[cfg(unix)]
    #[test]
    fn every_message_about_a_path_shows_it_escaped() {
        let parent = tempfile::tempdir().expect("a temporary directory");
        let file = parent.path().join("file\u{1b}[2J\nname");
        fs::write(&file, b"x").expect("a file");
        let link = parent.path().join("link\u{202e}name");
        std::os::unix::fs::symlink(parent.path(), &link).expect("a link");

        for (refusal, what) in [
            (
                SoakRoot::open(parent.path().join("absent\u{1b}[2J\nname")),
                "absent",
            ),
            (SoakRoot::open(&file), "not a directory"),
            (SoakRoot::open(&link), "a link"),
        ] {
            let shown = refusal.expect_err(what).to_string();
            assert!(
                shown.contains("[deceptive]") || shown.contains("\\u{202e}"),
                "{what}: {shown}"
            );
            assert!(
                !shown.chars().any(is_deceptive),
                "{what}: the message would act on a terminal: {shown:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_path_that_does_not_exist_yet_is_resolved_through_the_links_that_do() {
        let parent = tempfile::tempdir().expect("a temporary directory");
        let real = parent.path().join("real");
        fs::create_dir(&real).expect("a directory");
        let link = parent.path().join("link");
        std::os::unix::fs::symlink(&real, &link).expect("a link");

        let resolved = resolve_through_ancestors(&link.join("not").join("yet"));

        assert_eq!(
            resolved,
            fs::canonicalize(&real)
                .expect("canonical")
                .join("not")
                .join("yet")
        );
        assert_eq!(
            resolve_through_ancestors(&link),
            fs::canonicalize(&real).expect("canonical"),
            "a link that exists is followed"
        );
    }

    /// A temporary directory that holds two directories, `repo` and `tree`, and its canonical
    /// spelling.
    fn repo_and_tree() -> (tempfile::TempDir, PathBuf) {
        let parent = tempfile::tempdir().expect("a temporary directory");
        fs::create_dir(parent.path().join("repo")).expect("a checkout");
        fs::create_dir(parent.path().join("tree")).expect("a tree");
        let canonical = fs::canonicalize(parent.path()).expect("canonical");
        (parent, canonical)
    }

    #[test]
    fn a_path_that_climbs_out_of_a_directory_that_is_not_there_yet_is_judged_by_where_it_lands() {
        let (parent, canonical) = repo_and_tree();
        let soak = SoakRoot::open(parent.path().join("tree")).expect("a root");
        // `CARGO_TARGET_DIR=missing/../../tree/target`, joined to the checkout as the command
        // joins it: the build makes `missing` first, and then writes in the tree beside the
        // checkout.
        let target = parent.path().join("repo").join("missing/../../tree/target");

        let resolved = resolve_through_ancestors(&target);

        assert_eq!(resolved, canonical.join("tree").join("target"));
        assert!(
            soak.contains(&target),
            "a build in it writes in the tree: {resolved:?}"
        );
        let elsewhere = parent
            .path()
            .join("repo")
            .join("missing/../../beside/target");
        assert_eq!(
            resolve_through_ancestors(&elsewhere),
            canonical.join("beside").join("target")
        );
        assert!(!soak.contains(&elsewhere));
        assert!(
            !parent.path().join("repo").join("missing").exists(),
            "judging a path makes nothing"
        );
    }

    #[test]
    fn a_place_that_climbs_back_to_the_directory_that_holds_the_root_holds_it() {
        let (parent, _) = repo_and_tree();
        let tree = parent.path().join("tree");
        let soak = SoakRoot::open(&tree).expect("a root");

        assert!(
            soak.lies_inside(&tree.join("missing/../..")),
            "the directory that holds the tree holds it"
        );
        assert!(
            !soak.lies_inside(&tree.join("missing/..")),
            "the tree is not inside itself"
        );
    }

    #[test]
    fn a_parent_component_that_leaves_the_root_leaves_it_and_one_that_stays_in_it_stays() {
        let (parent, _) = repo_and_tree();
        let tree = parent.path().join("tree");
        let soak = SoakRoot::open(&tree).expect("a root");

        assert!(soak.contains(&tree.join("a/b/../../c")));
        assert!(soak.contains(&tree.join("a/../b")));
        assert!(
            soak.contains(&tree.join("a/../../tree/x")),
            "and back in again"
        );
        assert!(
            !soak.contains(&tree.join("a/../..")),
            "the directory that holds the tree is not inside it"
        );
        assert!(!soak.contains(&tree.join("a/../../repo/x")));
    }

    #[test]
    fn parent_components_below_the_part_that_exists_take_away_what_they_follow_and_then_climb() {
        let (parent, canonical) = repo_and_tree();
        let repo = parent.path().join("repo");

        for (given, lands) in [
            ("missing/..", "repo"),
            ("a/b/..", "repo/a"),
            ("a/b/../..", "repo"),
            ("a/b/../../..", ""),
            ("a/b/../../../tree/x", "tree/x"),
            ("a/../b/../c", "repo/c"),
            ("./a/./b", "repo/a/b"),
            ("../tree/new", "tree/new"),
            ("../tree", "tree"),
        ] {
            let expected = if lands.is_empty() {
                canonical.clone()
            } else {
                canonical.join(lands)
            };

            assert_eq!(
                resolve_through_ancestors(&repo.join(given)),
                expected,
                "{given}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_parent_component_after_a_link_that_exists_is_the_parent_of_what_the_link_leads_to() {
        let parent = tempfile::tempdir().expect("a temporary directory");
        let real = parent.path().join("real");
        fs::create_dir_all(real.join("inner")).expect("directories");
        let link = parent.path().join("link");
        std::os::unix::fs::symlink(real.join("inner"), &link).expect("a link");
        let beside_the_target = fs::canonicalize(&real).expect("canonical");

        // `link/..` is the directory that holds `inner`, which is not the one that holds `link`.
        assert_eq!(
            resolve_through_ancestors(&link.join("..")),
            beside_the_target
        );
        assert_eq!(
            resolve_through_ancestors(&link.join("../new")),
            beside_the_target.join("new")
        );
        assert_eq!(
            resolve_through_ancestors(&link.join("missing/../../new")),
            beside_the_target.join("new"),
            "a `..` that has nothing of the missing part left goes up from the target of the link"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_root_of_the_file_system_is_its_own_parent() {
        assert_eq!(resolve_through_ancestors(Path::new("/..")), Path::new("/"));
        assert_eq!(
            resolve_through_ancestors(Path::new("/../../no-such-directory-xh/..")),
            Path::new("/")
        );
    }

    #[test]
    fn a_relative_path_is_judged_from_the_working_directory_and_an_empty_one_is_left_as_it_is() {
        let here = fs::canonicalize(".").expect("the working directory");

        assert_eq!(resolve_through_ancestors(Path::new("")), Path::new(""));
        assert_eq!(resolve_through_ancestors(Path::new(".")), here);
        assert_eq!(
            resolve_through_ancestors(Path::new("no-such-directory-xh/..")),
            here
        );
        assert_eq!(
            resolve_through_ancestors(Path::new("no-such-directory-xh/x")),
            here.join("no-such-directory-xh").join("x")
        );
    }
}
