//! Per-run copies: fresh, marked, independent of the master, and removed on drop.

#[cfg(unix)]
use std::path::PathBuf;
use std::{collections::BTreeMap, fs, path::Path};

use super::support::{
    Scratch, chain_spec, deep_names_spec, master, master_of, path_of_len, spec, tiny_spec,
    tree_chain_spec, victim_spec,
};
#[cfg(unix)]
use super::support::{link_to, long_way_round};
use crate::fixture::{
    FixtureCache, FixtureError, FixtureSpec, Fixtures, MARKER_FILE_NAME, MaterializeOptions,
    NodeKind, Oracle, OracleOptions, RelPath, Role, SpecError, remove_tree, verify_owned,
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
fn the_facade_never_caches_a_fixture_that_cargo_clean_could_not_remove() {
    // A directory that cannot be listed, one that nothing can be removed from, and a path longer
    // than `PATH_MAX` each defeat a path-based removal, and the shared cache lives below the target
    // directory, which `cargo clean` and `git worktree remove` must always be able to remove.
    let scratch = Scratch::new();
    let fixtures = Fixtures::new(FixtureSpec::bundled_dir(), scratch.cache());
    for id in ["hostile-small", "refused", "deep-past-path-max"] {
        assert!(
            matches!(fixtures.master(id), Err(FixtureError::NotCacheable { .. })),
            "{id} is refused"
        );
    }
    assert!(
        !scratch.join("cache").exists(),
        "nothing was generated in the cache"
    );
}

/// A shaped part can plan paths as long as its levels and the names of its histograms allow: 32
/// levels of folders with names of 255 bytes plan paths of over 7,900 bytes, which a path-based
/// removal cannot take, as it cannot take the chain of a `deep` part. Such a part is never cached.
/// A part that is shallow, or whose names are short, is, as it was.
#[test]
fn the_facade_never_caches_a_shaped_part_whose_paths_can_be_too_long_to_remove_by_path() {
    let scratch = Scratch::new();
    let specs = scratch.join("specs");
    fs::create_dir(&specs).expect("mkdir");
    fs::write(specs.join("shaped-deep-names.toml"), deep_names_spec()).expect("write");
    fs::write(
        specs.join("shaped-short.toml"),
        "schema_version = 1\nid = \"shaped-short\"\ndescription = \"A shallow tree.\"\nseed = 2\n\n\
         [[parts]]\nkind = \"shaped\"\nroot = \"home\"\n\n\
         [[parts.levels]]\ndirectories = 2\nfiles = 3\n\n\
         [[parts.levels]]\nfiles = 5\n\n\
         [parts.subdirectories_per_directory]\n0 = 1\n1 = 1\n\n\
         [parts.files_per_directory]\n1 = 1\n4 = 1\n\n\
         [parts.file_sizes]\n1 = 1\n\n\
         [parts.directory_name_lengths]\n6 = 1\n\n\
         [parts.file_name_lengths]\n8 = 1\n",
    )
    .expect("write");
    let fixtures = Fixtures::new(&specs, scratch.cache());

    let long = fixtures.spec("shaped-deep-names").expect("a spec");
    assert!(!fixtures.is_cacheable(&long), "a path of over 7,900 bytes");
    assert!(matches!(
        fixtures.master("shaped-deep-names"),
        Err(FixtureError::NotCacheable { .. })
    ));
    assert!(
        !scratch.join("cache").exists(),
        "nothing was generated in the cache"
    );

    let short = fixtures.spec("shaped-short").expect("a spec");
    assert!(fixtures.is_cacheable(&short), "a path of under 40 bytes");
    let master = fixtures.master("shaped-short").expect("a master");
    assert!(!master.cache_hit);
    assert!(
        fixtures.master("shaped-short").expect("again").cache_hit,
        "it is cached"
    );
}

/// Whether a path-based removal can take a fixture depends on where it is: the directory of the
/// cache is above its root, and `CARGO_TARGET_DIR` can put that anywhere. The same spec, one whose
/// longest path is 512 bytes, is cached under a short cache root and refused under one that
/// leaves its paths less room, and nothing is generated in the cache that refuses it.
#[test]
fn the_same_shaped_spec_is_cached_under_a_short_cache_root_and_refused_under_a_long_one() {
    let scratch = Scratch::new();
    let specs = scratch.join("specs");
    fs::create_dir(&specs).expect("mkdir");
    // Four folders of 100 bytes and a file of 103: 4 + 4 * 101 + 1 + 103 = 512 bytes.
    fs::write(specs.join("chain.toml"), chain_spec("chain", 4, 100, 103)).expect("write");

    let short = Fixtures::new(&specs, scratch.cache());
    let master = short.master("chain").expect("cached under a short root");
    assert!(!master.cache_hit);
    assert!(
        short.master("chain").expect("again").cache_hit,
        "it is cached"
    );

    // A root of 500 bytes, a separator, the longest name the cache gives a directory (57), a
    // separator, and 512 bytes: 1,071, past the 1,023 of `PATH_MAX` on macOS.
    let long_root = path_of_len(scratch.path(), 500);
    let long = Fixtures::new(&specs, FixtureCache::at(&long_root));
    assert!(
        matches!(long.master("chain"), Err(FixtureError::NotCacheable { .. })),
        "the same spec is refused where its paths would not fit"
    );
    assert!(
        !long_root.exists(),
        "nothing was generated in the cache that refused it"
    );
    let run_parent = scratch.join("runs");
    fs::create_dir(&run_parent).expect("mkdir");
    let copy = long
        .run_copy("chain", &run_parent)
        .expect("a run copy is made wherever the cache is");
    assert!(copy.root().is_dir());
}

/// The rule to the byte, for the spec `text` (its id is `chain`), whose longest path is `longest`
/// bytes below the fixture root. The path of the cache directory as it resolves, a separator, the
/// longest name the cache gives a directory below it, a separator, and the longest path the spec
/// can plan must fit in 1,023 bytes: `PATH_MAX` on macOS is 1,024 and counts the terminating NUL.
/// The longest name is that of a fixture still being generated: `.partial-`, 16 hexadecimal
/// digits, `-`, the number of the process (at most 10 digits), `-`, and a count (at most 20
/// digits). The roots are built below the scratch directory as it resolves, because the cache
/// measures them that way: below `/var` on macOS, a link to `/private/var`, a root is 8 bytes
/// longer than it is written.
fn assert_cached_exactly_when_its_longest_path_fits_below_the_cache(text: &str, longest: usize) {
    const LONGEST_NAME: usize = ".partial-".len() + 16 + 1 + 10 + 1 + 20;

    let scratch = Scratch::new();
    let base = scratch.resolved_path();
    let specs = scratch.join("specs");
    fs::create_dir(&specs).expect("mkdir");
    fs::write(specs.join("chain.toml"), text).expect("write");
    let probe = Fixtures::new(&specs, scratch.cache());
    assert_eq!(
        probe.spec("chain").expect("a spec").longest_path_bytes(),
        u64::try_from(longest).expect("a length")
    );
    let fitting_root = 1_023 - 1 - LONGEST_NAME - 1 - longest;

    // At the limit: cached, reused, and the longest path of the tree can be named whole.
    let fits_root = path_of_len(&base, fitting_root);
    let fits = Fixtures::new(&specs, FixtureCache::at(&fits_root));
    let master = fits.master("chain").expect("cached at the limit");
    assert!(!master.cache_hit);
    assert!(fits.master("chain").expect("again").cache_hit);
    let deepest = master
        .plan()
        .entries()
        .iter()
        .map(|entry| entry.path.to_path_buf(&master.root))
        .max_by_key(|path| path.as_os_str().len())
        .expect("the plan has entries");
    assert!(deepest.as_os_str().len() <= 1_023);
    fs::symlink_metadata(&deepest).expect("the deepest path can be named whole");

    // One byte more: refused, and nothing is generated.
    let over_root = path_of_len(&base, fitting_root + 1);
    let over = Fixtures::new(&specs, FixtureCache::at(&over_root));
    assert!(
        matches!(over.master("chain"), Err(FixtureError::NotCacheable { .. })),
        "1,024 bytes is refused"
    );
    assert!(!over_root.exists(), "nothing was generated in the cache");
}

#[test]
fn a_shaped_part_is_cached_exactly_when_its_longest_path_fits_in_path_max_below_the_cache() {
    // Four folders of 100 bytes and a file of 103: 4 + 4 * 101 + 1 + 103 = 512 bytes.
    assert_cached_exactly_when_its_longest_path_fits_below_the_cache(
        &chain_spec("chain", 4, 100, 103),
        512,
    );
}

/// The same for a `tree` part, whose names a spec of one's own can make as long as a name can be:
/// the cache holds it when the root leaves its longest path exactly the limit, and refuses it one
/// byte below.
#[test]
fn a_tree_is_cached_exactly_when_its_longest_path_fits_in_path_max_below_the_cache() {
    assert_cached_exactly_when_its_longest_path_fits_below_the_cache(
        &tree_chain_spec("chain", 4, 100, 103),
        512,
    );
}

/// A `tree` part can have names of 255 bytes down 32 levels, which a spec of one's own can ask
/// for, and paths no cache root leaves room for: four folders named with 255 bytes below `tree`
/// are 4 + 4 * 256 = 1,028 bytes, over the limit before any root is counted. It is never cached,
/// wherever the cache is: the facade refuses it, nothing is generated in the cache, and a run copy
/// is made instead.
#[test]
fn the_facade_never_caches_a_tree_whose_names_make_a_path_too_long_to_remove_by_path() {
    let scratch = Scratch::new();
    let specs = scratch.join("specs");
    fs::create_dir(&specs).expect("mkdir");
    fs::write(
        specs.join("long-tree.toml"),
        format!(
            "schema_version = 1\nid = \"long-tree\"\ndescription = \"A tree of long names.\"\n\
             seed = 1\n\n[[parts]]\nkind = \"tree\"\nroot = \"tree\"\ndepth = 4\nfanout = 1\n\
             dir_names = {{ style = \"literal\", name = \"{}\" }}\n",
            "n".repeat(255)
        ),
    )
    .expect("write");
    let fixtures = Fixtures::new(&specs, scratch.cache());

    let long = fixtures.spec("long-tree").expect("a spec");
    assert_eq!(long.longest_path_bytes(), 1_028);
    assert!(!fixtures.is_cacheable(&long), "a path of 1,028 bytes");
    assert!(
        matches!(
            fixtures.master("long-tree"),
            Err(FixtureError::NotCacheable { .. })
        ),
        "the tree is refused below a short root"
    );
    assert!(
        !scratch.join("cache").exists(),
        "nothing was generated in the cache"
    );

    let run_parent = scratch.join("runs");
    fs::create_dir(&run_parent).expect("mkdir");
    let copy = fixtures
        .run_copy("long-tree", &run_parent)
        .expect("a run copy is made instead");
    assert!(copy.root().join("tree").is_dir());
}

/// A bundled fixture is held to the same rule as any other, whatever its kinds of parts. Below a
/// root that leaves it exactly its longest path, a fixture is cacheable, and below one byte more
/// it is refused; and the bundled specs that no path-based removal could remove at all (a `deep`
/// part, and a hostile one with directories that cannot be listed or changed) are refused below
/// every root. A fixture at the limit is cached and below the next byte refused, through the
/// facade, for one of them.
#[test]
fn a_bundled_fixture_is_refused_below_a_root_that_overruns_it() {
    // The longest name the cache gives a directory below its root, 57 bytes.
    const LONGEST_NAME: u64 = 9 + 16 + 1 + 10 + 1 + 20;

    let scratch = Scratch::new();
    let base = scratch.resolved_path();
    let bundled = Fixtures::bundled();
    let ids = bundled.ids().expect("the bundled specs");
    assert!(ids.len() >= 20, "{ids:?}");
    let at = |bytes: u64| {
        let root = path_of_len(&base, usize::try_from(bytes).expect("a length"));
        Fixtures::new(FixtureSpec::bundled_dir(), FixtureCache::at(root))
    };

    let mut never = Vec::new();
    for id in &ids {
        let spec = bundled.spec(id).expect("a spec");
        if !spec.removable_by_path(0) {
            never.push(id.as_str());
            continue;
        }
        // A root of `1,023 - 1 - 57 - 1 - longest` bytes: with the longest name and the
        // separators, the limit exactly.
        let fitting = 1_023 - 1 - LONGEST_NAME - 1 - spec.longest_path_bytes();
        assert!(at(fitting).is_cacheable(&spec), "{id} fits below {fitting}");
        assert!(
            !at(fitting + 1).is_cacheable(&spec),
            "{id} is refused below {}",
            fitting + 1
        );
    }
    assert_eq!(
        never,
        [
            "all-classes-small",
            "deep-past-path-max",
            "hostile-small",
            "refused"
        ]
    );

    // Through the facade: the same fixture is cached at the limit and refused a byte over.
    let fitting = 1_023
        - 1
        - LONGEST_NAME
        - 1
        - bundled
            .spec("delete-file")
            .expect("a spec")
            .longest_path_bytes();
    assert!(
        at(fitting).master("delete-file").is_ok(),
        "cached at the limit"
    );
    assert!(
        matches!(
            at(fitting + 1).master("delete-file"),
            Err(FixtureError::NotCacheable { .. })
        ),
        "refused a byte over"
    );
}

/// A path that goes through a symbolic link is counted against `PATH_MAX` as it resolves: on macOS
/// the system counts the content of the link and the rest of the path together, so a root that
/// is a few dozen bytes as it is written can lead 500 bytes down. The cache root is measured where
/// it leads. The same spec, one whose longest path is 512 bytes, is cached under a short real
/// path and refused below a short link to a deep directory, with nothing generated, and so is a
/// root that does not exist yet below the link; a link is no reason in itself, and below a link to
/// a short directory the cache is held. Unix only: on Windows a link to a directory needs a
/// privilege, and a junction, which needs none, can only be made through `cmd /C mklink /J`, which
/// was not tried with a target 520 bytes long.
#[cfg(unix)]
#[test]
fn a_cache_root_is_measured_where_a_link_in_it_leads() {
    let scratch = Scratch::new();
    let specs = scratch.join("specs");
    fs::create_dir(&specs).expect("mkdir");
    // Four folders of 100 bytes and a file of 103: 4 + 4 * 101 + 1 + 103 = 512 bytes.
    fs::write(specs.join("chain.toml"), chain_spec("chain", 4, 100, 103)).expect("write");

    // A real directory 520 bytes deep, and a short link to it.
    let deep = path_of_len(scratch.path(), 520);
    fs::create_dir_all(&deep).expect("a deep directory");
    let link = scratch.join("link");
    link_to(&link, &deep);

    let short = Fixtures::new(&specs, scratch.cache());
    assert!(
        short.master("chain").is_ok(),
        "the same spec is cached under a short real path"
    );

    let through = Fixtures::new(&specs, FixtureCache::at(link.join("cache")));
    let result = through.master("chain");
    assert!(
        matches!(result, Err(FixtureError::NotCacheable { .. })),
        "the same spec is refused below a link to a deep directory, and master gave {:?}",
        result.map(|master| master.root)
    );
    assert!(
        !deep.join("cache").exists(),
        "nothing was generated where the link leads"
    );

    // The same when the root does not exist yet, and the link is its longest ancestor that does.
    let not_yet = Fixtures::new(&specs, FixtureCache::at(link.join("not").join("yet")));
    let result = not_yet.master("chain");
    assert!(
        matches!(result, Err(FixtureError::NotCacheable { .. })),
        "a root that does not exist yet, below the link, is refused too, and master gave {:?}",
        result.map(|master| master.root)
    );
    assert!(
        !deep.join("not").exists(),
        "nothing was generated where the link leads"
    );

    // A link is no reason in itself: below one to a short directory, the cache is held, in the
    // directory the link leads to.
    let near = scratch.join("near");
    fs::create_dir(&near).expect("mkdir");
    let near_link = scratch.join("near-link");
    link_to(&near_link, &near);
    let held = Fixtures::new(&specs, FixtureCache::at(near_link.join("cache")));
    assert!(
        held.master("chain").is_ok(),
        "a link to a short directory holds the cache"
    );
    assert!(near.join("cache").is_dir());
}

/// The system works on the content of a link and what is left of the path after it, so a cache
/// whose root is below a short link to a long path that ends in a link back to a short directory
/// is measured by that pathname, though the root is short as it is written and as it resolves.
/// The rule to the byte: the content of the link (a deep folder and `/back`), `/cache`, a
/// separator, the longest name the cache gives a directory (57 bytes), a separator, and the
/// longest path of the part (512 bytes) are 1,023 bytes when the deep folder is 441 bytes, and
/// 1,024 when it is 442. Unix only, like the other tests that make a link.
#[cfg(unix)]
#[test]
fn a_cache_root_is_measured_by_the_longest_pathname_a_link_in_it_makes_the_system_work_on() {
    const LONGEST_NAME: usize = ".partial-".len() + 16 + 1 + 10 + 1 + 20;

    let scratch = Scratch::new();
    let base = scratch.resolved_path();
    let specs = scratch.join("specs");
    fs::create_dir(&specs).expect("mkdir");
    // Four folders of 100 bytes and a file of 103: 4 + 4 * 101 + 1 + 103 = 512 bytes.
    fs::write(specs.join("chain.toml"), chain_spec("chain", 4, 100, 103)).expect("write");
    let fitting = 1_023 - 1 - LONGEST_NAME - 1 - 512 - "/cache".len() - "/back".len();
    assert_eq!(fitting, 441);

    // At the limit: cached, reused, and the longest path of the tree can be named whole, by way
    // of the link.
    let at_limit = base.join("at-limit");
    fs::create_dir(&at_limit).expect("mkdir");
    let way = long_way_round(&at_limit, fitting);
    let fits = Fixtures::new(&specs, FixtureCache::at(way.start.join("cache")));
    let master = fits.master("chain").expect("cached at the limit");
    assert!(!master.cache_hit);
    assert!(fits.master("chain").expect("again").cache_hit);
    assert!(
        way.near.join("cache").is_dir(),
        "the cache is where the link leads"
    );
    let deepest = master
        .plan()
        .entries()
        .iter()
        .map(|entry| entry.path.to_path_buf(&master.root))
        .max_by_key(|path| path.as_os_str().len())
        .expect("the plan has entries");
    fs::symlink_metadata(&deepest).expect("the deepest path can be named whole");

    // One byte more: refused, and nothing is generated.
    let over_limit = base.join("over-limit");
    fs::create_dir(&over_limit).expect("mkdir");
    let way = long_way_round(&over_limit, fitting + 1);
    let over = Fixtures::new(&specs, FixtureCache::at(way.start.join("cache")));
    assert!(
        matches!(over.master("chain"), Err(FixtureError::NotCacheable { .. })),
        "1,024 bytes is refused"
    );
    assert!(
        !way.near.join("cache").exists(),
        "nothing was generated where the link leads"
    );
}

/// A cache whose path goes through a symbolic link that leads to a name that is not there holds
/// no fixture, whether the link is the cache directory or a directory above it. The link exists,
/// so the cache cannot make it, and nothing can be made where it leads from here: the resolver
/// takes a name that a link led to and that is not there as an error, and a name the root was
/// written with as one the cache makes. The fixture is never cached, and a run copy is made
/// instead. Once the link leads somewhere, the cache holds the fixture where it leads. Unix only:
/// on Windows a link needs a privilege.
#[cfg(unix)]
#[test]
fn a_cache_below_a_link_that_leads_nowhere_holds_no_fixture_and_a_run_copy_is_made() {
    let scratch = Scratch::new();
    let missing = scratch.join("missing");
    let link = scratch.join("link");
    link_to(&link, &missing);
    let run_parent = scratch.join("runs");
    fs::create_dir(&run_parent).expect("mkdir");

    for root in [link.clone(), link.join("cache")] {
        let fixtures = Fixtures::new(FixtureSpec::bundled_dir(), FixtureCache::at(&root));
        let wide = fixtures.spec("wide-1k").expect("a spec");
        assert!(!fixtures.is_cacheable(&wide), "{}", root.display());
        let result = fixtures.master("wide-1k");
        assert!(
            matches!(result, Err(FixtureError::NotCacheable { .. })),
            "master gave {:?}",
            result.map(|master| master.root)
        );
        assert!(!missing.exists(), "nothing was made where the link leads");
        let copy = fixtures
            .run_copy("wide-1k", &run_parent)
            .expect("a run copy is made instead");
        assert!(copy.root().is_dir());
    }

    // Where the link leads, the cache holds the fixture.
    fs::create_dir(&missing).expect("mkdir");
    let fixtures = Fixtures::new(
        FixtureSpec::bundled_dir(),
        FixtureCache::at(link.join("cache")),
    );
    assert!(
        fixtures.master("wide-1k").is_ok(),
        "the cache holds the fixture once the link leads somewhere"
    );
    assert!(missing.join("cache").is_dir());
}

/// A cache that is not a folder holds no fixture, whatever the fixture: a root that is a file, a
/// link to a file, a link whose content goes on after a file with a separator or a `.`, and a root
/// written so. The system refuses a path that goes on after a file, and `create_dir_all` fails on
/// a root that is one, so the fixture is never cached and a run copy is made instead; the file is
/// left as it was and nothing is made. Unix only: on Windows a link needs a privilege.
#[cfg(unix)]
#[test]
fn a_cache_that_is_not_a_folder_holds_no_fixture_and_a_run_copy_is_made() {
    let scratch = Scratch::new();
    let file = scratch.join("a-file");
    fs::write(&file, b"not a folder").expect("a file");
    fs::create_dir(scratch.join("a-folder")).expect("a folder");
    link_to(&scratch.join("to-file"), &file);
    link_to(&scratch.join("slash"), Path::new("a-file/"));
    link_to(&scratch.join("dot"), Path::new("a-file/."));
    link_to(&scratch.join("up"), Path::new("a-file/../a-folder"));
    let run_parent = scratch.join("runs");
    fs::create_dir(&run_parent).expect("mkdir");
    let listing = |dir: &Path| {
        let mut names: Vec<_> = fs::read_dir(dir)
            .expect("a listing")
            .map(|entry| entry.expect("an entry").file_name())
            .collect();
        names.sort();
        names
    };
    let before = listing(scratch.path());

    let dir = scratch.path().display().to_string();
    for root in [
        file.clone(),
        scratch.join("to-file"),
        scratch.join("slash"),
        scratch.join("dot"),
        scratch.join("up"),
        PathBuf::from(format!("{dir}/a-file/")),
        PathBuf::from(format!("{dir}/a-file/.")),
    ] {
        let fixtures = Fixtures::new(FixtureSpec::bundled_dir(), FixtureCache::at(&root));
        let wide = fixtures.spec("wide-1k").expect("a spec");
        assert!(!fixtures.is_cacheable(&wide), "{}", root.display());
        let result = fixtures.master("wide-1k");
        assert!(
            matches!(result, Err(FixtureError::NotCacheable { .. })),
            "{}: master gave {:?}",
            root.display(),
            result.map(|master| master.root)
        );
        let copy = fixtures
            .run_copy("wide-1k", &run_parent)
            .expect("a run copy is made instead");
        assert!(copy.root().is_dir());
    }

    assert_eq!(fs::read(&file).expect("the file"), b"not a folder");
    assert_eq!(listing(scratch.path()), before, "nothing was made");
    assert!(listing(&run_parent).is_empty(), "the copies are gone");
}

/// The root of the cache counts as it is spelled, which is what the system is handed: a spec that
/// fits to the byte below the root as `std::path::absolute` would spell it, without its repeated
/// separators and `.` names, is refused below the root as it is written when that is a byte longer,
/// with `//` in it, or two, with `/./`. Nothing is generated in the cache that refuses it. Unix
/// only: where `absolute` gives the spelling the system sees (Windows), it is the normalized one.
#[cfg(unix)]
#[test]
fn a_spec_that_fits_below_a_root_as_normalized_is_refused_below_it_as_it_is_spelled() {
    const LONGEST_NAME: usize = ".partial-".len() + 16 + 1 + 10 + 1 + 20;

    let scratch = Scratch::new();
    let base = scratch.resolved_path();
    let specs = scratch.join("specs");
    fs::create_dir(&specs).expect("mkdir");
    // Four folders of 100 bytes and a file of 103: 4 + 4 * 101 + 1 + 103 = 512 bytes.
    fs::write(specs.join("chain.toml"), chain_spec("chain", 4, 100, 103)).expect("write");
    let fitting_root = 1_023 - 1 - LONGEST_NAME - 1 - 512;

    // As `absolute` spells it, the root fits: cached.
    let normalized = path_of_len(&base.join("fits"), fitting_root);
    let fits = Fixtures::new(&specs, FixtureCache::at(&normalized));
    assert!(fits.master("chain").is_ok(), "cached at the limit");

    // The same length of root in other folders, with the separators and the dots that `absolute`
    // drops, are 1 and 2 bytes more: refused, and nothing is generated.
    for (name, separator) in [("doubled", "//"), ("dotted", "/./")] {
        let plain = path_of_len(&base.join(name), fitting_root);
        let tail = plain.strip_prefix(&base).expect("below the base");
        let spelled = PathBuf::from(format!("{}{separator}{}", base.display(), tail.display()));
        let refused = Fixtures::new(&specs, FixtureCache::at(&spelled));
        assert!(
            matches!(
                refused.master("chain"),
                Err(FixtureError::NotCacheable { .. })
            ),
            "{} is refused",
            spelled.display()
        );
        assert!(!plain.exists(), "nothing was generated below {name}");
    }
}

/// A cache whose path cannot be resolved holds no fixture, whatever the fixture: not a name that
/// is missing, which the cache makes, but one on the way that is not a folder. Unix only: Windows
/// reports such a path as one that is not there.
#[cfg(unix)]
#[test]
fn a_cache_root_that_cannot_be_resolved_holds_no_fixture() {
    let scratch = Scratch::new();
    let file = scratch.join("a-file");
    fs::write(&file, b"not a folder").expect("a file");
    let fixtures = Fixtures::new(
        FixtureSpec::bundled_dir(),
        FixtureCache::at(file.join("cache")),
    );

    let result = fixtures.master("wide-1k");
    assert!(
        matches!(result, Err(FixtureError::NotCacheable { .. })),
        "a cache below a file holds nothing, not even a tree, and master gave {:?}",
        result.map(|master| master.root)
    );
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
