//! Holding the fixtures that builds only scan to the plans they were generated from.
//!
//! The sweep runs published releases, some of which predate the guards of today's program, on
//! fixtures it shares between versions. A release that deleted or rewrote what it was only to read
//! would hand the next one another tree, and every cell after it would measure that. So once the
//! builds have scanned a fixture, the fixture is compared with the plan it was generated from, and
//! a difference stops the whole sweep ([`Fatal`]).
//!
//! A cached master is held to its plan the way the cache holds it before it reuses it, but in full
//! ([`verify_master`] with [`Verify::Full`]): its marker, its top-level names, and a walk of every
//! entry. A run copy (a fixture that is not cached, because a path-based removal cannot remove it)
//! has no cache entry to verify, so it is held to the plan by the oracle's comparison of a walk with
//! the plan, and its ownership marker is checked on top.
//!
//! What this cannot see: the contents of a file (the walk compares kind, size, link target,
//! permission bits, and which entries there are), and what lies below a folder the walk cannot
//! list. The unreadable folders of `hostile-small` are made that way on purpose, and no comparison
//! can look into them.

use std::fmt::Write as _;

use crate::{
    bench::cases::SharedFixture,
    fixture::{Oracle, RunCopy, Verify, integrity::describe, verify_master, verify_owned},
};

use super::checks::Fatal;

/// How many differences a message lists.
const LISTED: usize = 5;

/// The `context` of a difference found before the first build ran on the fixture.
pub(crate) const BEFORE_ANY_BUILD: &str =
    "differs from the plan it was generated from before any build has run on it";

/// The `context` of a difference found right after `version` scanned the fixture.
pub(crate) fn after_scan_by(version: &str) -> String {
    format!("changed while {version} scanned it")
}

/// Stops the sweep when `shared`, the fixture `id`, is not exactly what its plan says it is.
///
/// `context` finishes the sentence that names the fixture and says when the difference was found:
/// [`BEFORE_ANY_BUILD`], or [`after_scan_by`] a version.
///
/// # Errors
///
/// Returns [`Fatal`] when the fixture differs from its plan, or cannot be looked at.
pub(crate) fn verify(shared: &SharedFixture, id: &str, context: &str) -> Result<(), Fatal> {
    let problem = match shared {
        SharedFixture::Master(master) => verify_master(&master.root, master.plan(), Verify::Full)
            .err()
            .map(|failure| failure.to_string()),
        SharedFixture::RunCopy(copy) => copy_problem(copy, id)?,
    };
    match problem {
        None => Ok(()),
        Some(problem) => Err(Fatal(format!(
            "fixture `{id}` {context}: {problem}; the sweep stops here"
        ))),
    }
}

/// How a run copy differs from its plan, or `None` when it does not.
fn copy_problem(copy: &RunCopy, id: &str) -> Result<Option<String>, Fatal> {
    if let Err(error) = verify_owned(copy.root()) {
        return Ok(Some(error.to_string()));
    }
    let oracle = Oracle::collect(copy.root())
        .map_err(|error| Fatal(format!("cannot look at fixture `{id}` again: {error}")))?;
    let found = oracle
        .compare(copy.plan(), &copy.marker().capabilities)
        .discrepancies;
    if found.is_empty() {
        return Ok(None);
    }
    let count = found.len();
    let listed = found
        .iter()
        .take(LISTED)
        .map(describe)
        .collect::<Vec<_>>()
        .join("; ");
    let mut problem = format!("{count} entries differ from the plan: {listed}");
    if count > LISTED {
        let _ = write!(problem, "; and {} more", count - LISTED);
    }
    Ok(Some(problem))
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use tempfile::TempDir;

    use super::*;
    use crate::fixture::{FixtureCache, FixtureSpec, Fixtures, MARKER_FILE_NAME};

    /// A small fixture the harness can cache: `photos/` with two files, `music/` with one, and
    /// `readme.txt`.
    const FIXTURE: &str = "navigate-folders";
    const CONTEXT: &str = "changed while v1.3.0 scanned it";

    /// Makes a fixture of one kind in a directory of the test's own.
    type Make = fn(&TempDir) -> SharedFixture;

    /// The bundled specs, with a cache in the test's own directory.
    fn fixtures(dir: &TempDir) -> Fixtures {
        Fixtures::new(
            FixtureSpec::bundled_dir(),
            FixtureCache::at(dir.path().join("cache")),
        )
    }

    /// A cached master.
    fn master(dir: &TempDir) -> SharedFixture {
        let shared =
            SharedFixture::acquire(&fixtures(dir), FIXTURE, dir.path()).expect("a cached master");
        assert!(matches!(shared, SharedFixture::Master(_)));
        shared
    }

    /// A run copy, the kind of fixture that is generated for the run and never cached.
    fn copy(dir: &TempDir) -> SharedFixture {
        let runs = dir.path().join("runs");
        fs::create_dir(&runs).expect("a directory for the copy");
        let specs = fixtures(dir);
        let run = specs.run_copy(FIXTURE, &runs).expect("a run copy");
        SharedFixture::RunCopy(run)
    }

    fn add_in_a_folder(root: &Path) {
        fs::write(root.join("music/extra.bin"), b"x").expect("add a file");
    }

    fn add_at_the_top(root: &Path) {
        fs::write(root.join("extra.bin"), b"x").expect("add a file");
    }

    fn change_a_size(root: &Path) {
        fs::write(root.join("readme.txt"), b"short").expect("rewrite a file");
    }

    fn remove_a_file(root: &Path) {
        fs::remove_file(root.join("music/track.mp3")).expect("remove a file");
    }

    fn remove_a_folder(root: &Path) {
        fs::remove_dir_all(root.join("photos")).expect("remove a folder");
    }

    /// One change a build could have made, what it is called, and a word the message must name.
    type Change = (&'static str, fn(&Path), &'static str);

    /// What a build that was only to read could have done to the fixture.
    const CHANGES: [Change; 5] = [
        ("a file added in a folder", add_in_a_folder, "extra.bin"),
        ("a file added at the top", add_at_the_top, "extra.bin"),
        ("a file changed in size", change_a_size, "readme.txt"),
        ("a file removed", remove_a_file, "track.mp3"),
        ("a folder removed", remove_a_folder, "photos"),
    ];

    /// Checks that `make`'s fixture stops the sweep after every change in [`CHANGES`], with a
    /// message that says which fixture, when, and what.
    fn assert_every_change_stops_the_sweep(make: Make) {
        for (what, change, named) in CHANGES {
            let dir = tempfile::tempdir().expect("a directory");
            let shared = make(&dir);
            change(shared.root());

            let stopped = verify(&shared, FIXTURE, CONTEXT).expect_err(what);

            let message = stopped.to_string();
            let start = format!("fixture `{FIXTURE}` {CONTEXT}: ");
            assert!(message.starts_with(&start), "{what}: {message}");
            assert!(message.contains(named), "{what}: {message}");
            assert!(
                message.ends_with("; the sweep stops here"),
                "{what}: {message}"
            );
        }
    }

    #[test]
    fn an_untouched_master_and_an_untouched_run_copy_are_intact() {
        let dir = tempfile::tempdir().expect("a directory");
        let cached = master(&dir);
        let generated = copy(&dir);

        verify(&cached, FIXTURE, CONTEXT).expect("an untouched master is intact");
        verify(&generated, FIXTURE, CONTEXT).expect("an untouched run copy is intact");
    }

    #[test]
    fn a_fixture_that_was_only_read_is_still_intact() {
        let makers: [Make; 2] = [master, copy];
        for make in makers {
            let dir = tempfile::tempdir().expect("a directory");
            let shared = make(&dir);
            for name in ["readme.txt", "music/track.mp3"] {
                fs::read(shared.root().join(name)).expect("read a file");
            }

            verify(&shared, FIXTURE, CONTEXT).expect("reading changes nothing");
        }
    }

    #[test]
    fn a_master_that_a_build_changed_stops_the_sweep() {
        assert_every_change_stops_the_sweep(master);
    }

    #[test]
    fn a_run_copy_that_a_build_changed_stops_the_sweep() {
        assert_every_change_stops_the_sweep(copy);
    }

    #[test]
    fn a_fixture_that_lost_its_ownership_marker_stops_the_sweep() {
        let makers: [Make; 2] = [master, copy];
        for make in makers {
            let dir = tempfile::tempdir().expect("a directory");
            let shared = make(&dir);
            fs::remove_file(shared.root().join(MARKER_FILE_NAME)).expect("remove the marker");

            let stopped = verify(&shared, FIXTURE, CONTEXT).expect_err("a fixture with no marker");

            assert!(stopped.to_string().contains("marker"), "{stopped}");
        }
    }

    #[test]
    fn a_fixture_that_is_gone_altogether_stops_the_sweep() {
        let makers: [Make; 2] = [master, copy];
        for make in makers {
            let dir = tempfile::tempdir().expect("a directory");
            let shared = make(&dir);
            fs::remove_dir_all(shared.root()).expect("remove the fixture");

            let stopped = verify(&shared, FIXTURE, CONTEXT).expect_err("a fixture that is gone");

            let start = format!("fixture `{FIXTURE}` {CONTEXT}: ");
            assert!(stopped.to_string().starts_with(&start), "{stopped}");
        }
    }

    #[test]
    fn a_run_copy_lists_a_few_differences_and_counts_the_rest() {
        let dir = tempfile::tempdir().expect("a directory");
        let shared = copy(&dir);
        for number in 0..8 {
            let name = format!("extra-{number}.bin");
            fs::write(shared.root().join(name), b"x").expect("add a file");
        }

        let stopped = verify(&shared, FIXTURE, CONTEXT).expect_err("eight files added");

        let message = stopped.to_string();
        assert!(
            message.contains("8 entries differ from the plan: "),
            "{message}"
        );
        assert!(
            message.contains("; and 3 more; the sweep stops here"),
            "{message}"
        );
    }

    #[test]
    fn a_difference_is_named_with_the_fixture_when_it_was_found_and_what_differs() {
        let dir = tempfile::tempdir().expect("a directory");
        let shared = copy(&dir);
        add_at_the_top(shared.root());

        let after = verify(&shared, FIXTURE, &after_scan_by("v1.3.0")).expect_err("after a scan");
        let before = verify(&shared, FIXTURE, BEFORE_ANY_BUILD).expect_err("before any build");

        assert_eq!(after_scan_by("v1.3.0"), CONTEXT);
        assert_eq!(
            after.to_string(),
            "fixture `navigate-folders` changed while v1.3.0 scanned it: 1 entries differ from \
             the plan: `extra.bin` is not in the plan; the sweep stops here"
        );
        assert_eq!(
            before.to_string(),
            "fixture `navigate-folders` differs from the plan it was generated from before any \
             build has run on it: 1 entries differ from the plan: `extra.bin` is not in the \
             plan; the sweep stops here"
        );
    }
}
