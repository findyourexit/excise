//! Per-run copies: fresh, marked, independent of the master, and removed on drop.

use std::{collections::BTreeMap, fs, path::Path};

use super::support::{Scratch, master, master_of, spec, tiny_spec, victim_spec};
use crate::fixture::{
    FixtureError, Fixtures, MARKER_FILE_NAME, MaterializeOptions, NodeKind, Oracle, OracleOptions,
    RelPath, Role, SpecError, remove_tree, verify_owned,
};

/// One entry of a listing.
type Entry = (RelPath, NodeKind, u64);

/// The oracle without the readability probe, which opens every file: these tests do not compare
/// readability, and a large tree pays for every open.
fn oracle_of(root: &Path) -> Oracle {
    let options = OracleOptions {
        probe_file_readability: false,
    };
    Oracle::collect_with(root, options).expect("oracle")
}

/// `(path, kind, size)` of everything below the root except the marker. A directory's own size is
/// left out, as 0: the plan does not specify it, and what a file system reports for a directory
/// can depend on how the directory grew, so on the order its entries were created in.
fn listing(root: &Path) -> Vec<Entry> {
    oracle_of(root)
        .descendants()
        .filter(|entry| entry.path.as_bytes() != MARKER_FILE_NAME.as_bytes())
        .map(|entry| {
            let size = if entry.kind == NodeKind::Directory {
                0
            } else {
                entry.size
            };
            (entry.path.clone(), entry.kind, size)
        })
        .collect()
}

/// A few of `lines`, and how many there are.
fn sample(lines: &[String]) -> String {
    let shown: Vec<&str> = lines.iter().take(5).map(String::as_str).collect();
    format!("{} ({} in all)", shown.join("; "), lines.len())
}

/// Asserts that two listings hold the same entries, in any order. A difference is reported by
/// name, a few entries of each kind, because a whole tree is too long to read in a failure.
#[track_caller]
fn assert_same(actual: &[Entry], expected: &[Entry], what: &str) {
    let facts = |entries: &[Entry]| -> BTreeMap<RelPath, (NodeKind, u64)> {
        entries
            .iter()
            .map(|(path, kind, size)| (path.clone(), (*kind, *size)))
            .collect()
    };
    let (actual, expected) = (facts(actual), facts(expected));
    if actual == expected {
        return;
    }
    let missing: Vec<String> = expected
        .iter()
        .filter(|(path, _)| !actual.contains_key(*path))
        .map(|(path, wanted)| format!("{path} {wanted:?}"))
        .collect();
    let unexpected: Vec<String> = actual
        .iter()
        .filter(|(path, _)| !expected.contains_key(*path))
        .map(|(path, found)| format!("{path} {found:?}"))
        .collect();
    let changed: Vec<String> = expected
        .iter()
        .filter_map(|(path, wanted)| {
            let found = actual.get(path)?;
            (found != wanted).then(|| format!("{path}: expected {wanted:?}, found {found:?}"))
        })
        .collect();
    panic!(
        "{what}\n  missing: {}\n  unexpected: {}\n  changed: {}",
        sample(&missing),
        sample(&unexpected),
        sample(&changed)
    );
}

#[test]
fn a_run_copy_is_a_fresh_marked_tree_and_never_touches_the_master() {
    let scratch = Scratch::new();
    let master = master_of(&scratch, &victim_spec());
    let before = listing(&master.root);
    let runs = scratch.join("runs");
    fs::create_dir(&runs).expect("mkdir");

    let first = master.run_copy(&runs).expect("first copy");
    let second = master.run_copy(&runs).expect("second copy");
    assert_ne!(first.root(), second.root());
    assert_ne!(first.root(), master.root);
    assert!(first.root().starts_with(&runs));

    for copy in [&first, &second] {
        verify_owned(copy.root()).expect("a run copy is a marked fixture");
        assert_eq!(copy.marker().role, Role::RunCopy);
        assert_eq!(copy.marker().manifest_sha256, master.manifest_sha256());
        let comparison = copy
            .oracle()
            .expect("oracle")
            .compare(copy.plan(), &copy.marker().capabilities);
        assert!(comparison.is_clean(), "{:?}", comparison.discrepancies);
        assert_same(
            &listing(copy.root()),
            &before,
            "the copy holds the same tree as the master",
        );
    }

    // Deleting and changing one copy affects neither the other copy nor the master.
    remove_tree(&first.root().join("victim")).expect("delete the victim");
    fs::write(first.root().join("keep-a.bin"), "changed").expect("change a sentinel");
    assert_same(&listing(&master.root), &before, "the master is untouched");
    assert_same(
        &listing(second.root()),
        &before,
        "the other copy is untouched",
    );
    assert!(second.root().join("victim").is_dir());
}

#[cfg(unix)]
#[test]
fn a_run_copy_is_new_files_not_new_names_for_the_masters_files() {
    use std::os::unix::fs::MetadataExt as _;

    let scratch = Scratch::new();
    let master = master(&scratch, "node-modules-2k");
    let copy = master.run_copy(scratch.path()).expect("copy");
    let leaf = "node_modules/pkg0/pkg0/pkg0/pkg0/m0.js";
    let original = fs::metadata(master.root.join(leaf)).expect("stat");
    let duplicate = fs::metadata(copy.root().join(leaf)).expect("stat");
    assert_ne!(original.ino(), duplicate.ino());
    assert_eq!(
        original.nlink(),
        1,
        "a hard link would let a deletion reach the master"
    );
    assert_eq!(duplicate.nlink(), 1);
    // Writing through the copy does not change the master's bytes.
    let master_bytes = fs::read(master.root.join(leaf)).expect("read");
    fs::write(copy.root().join(leaf), "changed").expect("write");
    assert_eq!(
        fs::read(master.root.join(leaf)).expect("read"),
        master_bytes
    );
}

#[test]
fn a_deletion_scenario_gets_a_fresh_victim_without_a_new_copy() {
    let scratch = Scratch::new();
    let master = master(&scratch, "delete-folder");
    let copy = master.run_copy(scratch.path()).expect("copy");
    let victim = copy.root().join("victim");
    let expected = listing(copy.root());
    #[cfg(unix)]
    let sentinel_inode = std::os::unix::fs::MetadataExt::ino(
        &fs::metadata(copy.root().join("keep-a.bin")).expect("stat"),
    );

    remove_tree(&victim).expect("the run deletes the victim");
    assert!(!victim.exists());

    let report = copy.regenerate("victim").expect("regenerate");
    assert_eq!(report.created, 5_011);
    // First against the plan, which names what is wrong, then against what was there before.
    let comparison = oracle_of(copy.root()).compare(copy.plan(), &copy.marker().capabilities);
    assert!(
        comparison.is_clean(),
        "the regenerated victim differs from the plan: {:?}",
        &comparison.discrepancies[..comparison.discrepancies.len().min(5)]
    );
    assert_same(
        &listing(copy.root()),
        &expected,
        "the victim is back, exactly as planned",
    );
    #[cfg(unix)]
    assert_eq!(
        std::os::unix::fs::MetadataExt::ino(
            &fs::metadata(copy.root().join("keep-a.bin")).expect("stat")
        ),
        sentinel_inode,
        "the sentinels were not regenerated"
    );

    // Regenerating over a partly deleted victim also works, and unknown parts are refused.
    let some_file = expected
        .iter()
        .find(|(path, kind, _)| {
            *kind == crate::fixture::NodeKind::File
                && path.starts_with(&RelPath::from_bytes("victim").expect("valid"))
        })
        .map(|(path, _, _)| path.to_path_buf(copy.root()))
        .expect("the victim holds files");
    fs::remove_file(&some_file).expect("remove one file");
    copy.regenerate("victim").expect("regenerate again");
    assert_same(
        &listing(copy.root()),
        &expected,
        "regenerating over a partly deleted victim",
    );
    assert!(matches!(
        copy.regenerate("no-such-part"),
        Err(FixtureError::UnknownPart { .. })
    ));
    assert!(
        matches!(
            copy.regenerate("../keep-a.bin"),
            Err(FixtureError::UnknownPart { .. })
        ),
        "a part name is never a path"
    );
}

#[cfg(unix)]
#[test]
fn a_run_copy_removes_itself_on_drop_even_when_it_holds_unreadable_directories() {
    let scratch = Scratch::new();
    let master = master(&scratch, "hostile-small");
    let runs = scratch.join("runs");
    fs::create_dir(&runs).expect("mkdir");

    let copy = master.run_copy(&runs).expect("copy");
    let root = copy.root().to_path_buf();
    assert!(root.join("hostile/unreadable/locked-000").exists());
    drop(copy);
    assert!(!root.exists(), "the copy is gone: {}", root.display());
    assert_eq!(
        fs::read_dir(&runs).expect("list").count(),
        0,
        "no residue in the run directory"
    );

    // `keep` opts out; `remove` reports.
    let kept = master.run_copy(&runs).expect("copy").keep();
    assert!(kept.exists());
    remove_tree(&kept).expect("clean up");
    let explicit = master.run_copy(&runs).expect("copy");
    let path = explicit.root().to_path_buf();
    explicit.remove().expect("explicit removal");
    assert!(!path.exists());
}

#[test]
fn the_facade_turns_a_fixture_id_into_a_marked_fresh_root() {
    let scratch = Scratch::new();
    let specs = scratch.join("specs");
    fs::create_dir(&specs).expect("mkdir");
    let tiny = tiny_spec();
    // The file is named for the id, and the spec carries the same id.
    let text = format!(
        "schema_version = 1\nid = \"{}\"\ndescription = \"x\"\nseed = 5\n\n\
         [[parts]]\nkind = \"tree\"\nroot = \"alpha\"\ndepth = 1\nfanout = 2\nfiles_per_dir = 1\n",
        tiny.id
    );
    fs::write(specs.join("tiny.toml"), text).expect("write the spec");
    let runs = scratch.join("runs");
    fs::create_dir(&runs).expect("mkdir");

    let fixtures = Fixtures::new(&specs, scratch.cache());
    let copy = fixtures.run_copy("tiny", &runs).expect("a run copy by id");
    verify_owned(copy.root()).expect("marked");
    assert_eq!(copy.marker().spec_id, "tiny");
    assert!(copy.root().join("alpha").is_dir());

    let master = fixtures.master("tiny").expect("the master");
    assert!(!master.cache_hit);
    assert!(fixtures.master("tiny").expect("again").cache_hit);

    // A seed option reaches both.
    let seeded = fixtures.clone().with_options(MaterializeOptions {
        seed: Some(77),
        ..MaterializeOptions::default()
    });
    assert_eq!(
        seeded.run_copy("tiny", &runs).expect("copy").marker().seed,
        77
    );
    assert_eq!(seeded.master("tiny").expect("master").marker.seed, 77);

    // Unknown and malformed ids are refused before anything is generated.
    assert!(matches!(
        fixtures.run_copy("absent", &runs),
        Err(FixtureError::Spec(SpecError::NotFound { .. }))
    ));
    assert!(matches!(
        fixtures.run_copy("../specs/tiny", &runs),
        Err(FixtureError::Spec(SpecError::Invalid { .. }))
    ));
}

#[test]
fn a_run_copy_needs_a_run_directory_that_exists() {
    let scratch = Scratch::new();
    let master = master(&scratch, "wide-1k");
    assert!(matches!(
        master.run_copy(&scratch.join("no-such-directory")),
        Err(FixtureError::Io { .. })
    ));
    // The bundled specs all load through the facade.
    let fixtures = Fixtures::bundled();
    assert_eq!(
        spec("wide-1k").id,
        fixtures.spec("wide-1k").expect("bundled").id
    );
}
