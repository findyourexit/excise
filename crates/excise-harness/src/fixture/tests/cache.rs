//! The cache: a hit skips generation, a damaged entry is regenerated, entries are marked, and
//! publishing is safe.

use std::{fs, path::Path};

use super::support::{Scratch, master, spec, tiny_spec};
use crate::fixture::{
    FixtureCache, GENERATOR_VERSION, MARKER_FILE_NAME, MaterializeOptions, Materialized, Oracle,
    OwnershipError, Role, Verify, read_marker, remove_tree, verify_owned,
};

fn materialize(cache: &FixtureCache, id: &str, options: &MaterializeOptions) -> Materialized {
    cache
        .materialize(&spec(id), options)
        .unwrap_or_else(|error| panic!("{id}: {error}"))
}

fn assert_clean(materialized: &Materialized) {
    let comparison = Oracle::collect(&materialized.root)
        .expect("oracle")
        .compare(materialized.plan(), &materialized.marker.capabilities);
    assert!(comparison.is_clean(), "{:?}", comparison.discrepancies);
}

#[test]
fn a_cache_hit_skips_generation_and_returns_the_same_tree() {
    let scratch = Scratch::new();
    let cache = scratch.cache();
    let first = materialize(&cache, "node-modules-2k", &MaterializeOptions::default());
    assert!(!first.cache_hit);
    assert!(first.generation.is_some());
    assert!(
        first.verified.is_none(),
        "a tree that was just generated is not re-verified"
    );

    // A change the plan does not describe (a file's permission bits) survives a hit and would not
    // survive regeneration: it proves the second call did not generate.
    #[cfg(unix)]
    let canary = {
        use std::os::unix::fs::PermissionsExt as _;

        let file = first.root.join("node_modules/pkg0/pkg0/pkg0/pkg0/m0.js");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).expect("chmod");
        file
    };

    let second = materialize(&cache, "node-modules-2k", &MaterializeOptions::default());
    assert!(second.cache_hit);
    assert!(second.generation.is_none());
    assert!(second.generation_time().is_none());
    assert!(second.regenerated_because.is_none());
    assert_eq!(second.root, first.root);
    assert_eq!(second.manifest_sha256(), first.manifest_sha256());
    let verified = second
        .verified
        .expect("a hit is verified before it is reused");
    assert!(verified.full, "a small plan is verified in full");
    assert_eq!(verified.checked, 2_389);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        let mode = fs::metadata(&canary).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the tree was not touched by the second call");
    }
    assert_clean(&second);
}

#[test]
fn the_cache_entry_is_named_by_spec_hash_and_generator_version() {
    let scratch = Scratch::new();
    let master = master(&scratch, "wide-1k");
    let name = master
        .root
        .file_name()
        .and_then(|name| name.to_str())
        .expect("a UTF-8 entry name")
        .to_owned();
    let (hash, version) = name.split_once('-').expect("`<hash>-<version>`");
    assert_eq!(hash.len(), 16);
    assert!(master.marker.spec_hash.starts_with(hash));
    assert_eq!(version, GENERATOR_VERSION.to_string());
    assert_eq!(master.root.parent(), Some(scratch.join("cache").as_path()));
}

#[test]
fn each_seed_has_its_own_entry_and_neither_disturbs_the_other() {
    let scratch = Scratch::new();
    let cache = scratch.cache();
    let default = materialize(&cache, "wide-1k", &MaterializeOptions::default());
    let other = materialize(
        &cache,
        "wide-1k",
        &MaterializeOptions {
            seed: Some(99),
            ..MaterializeOptions::default()
        },
    );
    assert_ne!(default.root, other.root);
    assert_eq!(other.marker.seed, 99);
    assert_ne!(default.manifest_sha256(), other.manifest_sha256());
    assert!(materialize(&cache, "wide-1k", &MaterializeOptions::default()).cache_hit);
    assert!(
        materialize(
            &cache,
            "wide-1k",
            &MaterializeOptions {
                seed: Some(99),
                ..MaterializeOptions::default()
            }
        )
        .cache_hit
    );
}

#[test]
fn the_marker_is_a_regular_file_that_records_what_was_generated() {
    let scratch = Scratch::new();
    let master = master(&scratch, "delete-folder");
    verify_owned(&master.root).expect("a generated fixture is owned");

    let marker_path = master.root.join(MARKER_FILE_NAME);
    let meta = fs::symlink_metadata(&marker_path).expect("stat the marker");
    assert!(
        meta.file_type().is_file(),
        "the marker is a regular file, not a link"
    );

    let marker = read_marker(&master.root).expect("the marker parses");
    assert_eq!(marker, master.marker);
    assert_eq!(marker.role, Role::Master);
    assert_eq!(marker.generator_version, GENERATOR_VERSION);
    assert_eq!(marker.spec_id, "delete-folder");
    assert_eq!(marker.spec_hash.len(), 64);
    assert_eq!(marker.seed, 20_260_930);
    assert_eq!(marker.manifest_sha256, master.plan().manifest_sha256());
    assert_eq!(
        marker.entries,
        5_011 + 1 + 2,
        "victim, keep-a.bin, keep-b and its file"
    );
    assert_eq!(marker.created, marker.entries);
    assert!(marker.skipped.is_empty());
    // The marker is the last thing written: no temporary is left beside it.
    assert!(!master.root.join(".excise-harness-owned.tmp").exists());
}

#[test]
fn a_root_without_a_marker_that_is_a_regular_file_is_not_owned() {
    let scratch = Scratch::new();
    let root = scratch.join("plain");
    fs::create_dir(&root).expect("mkdir");
    assert!(matches!(
        verify_owned(&root),
        Err(OwnershipError::Missing { .. })
    ));

    // Interim fixtures from other tools may put anything in the marker; a regular file suffices.
    fs::write(root.join(MARKER_FILE_NAME), "interim").expect("write");
    assert!(verify_owned(&root).is_ok());

    assert!(matches!(
        verify_owned(&scratch.join("absent")),
        Err(OwnershipError::Root { .. })
    ));
    let file = scratch.join("a-file");
    fs::write(&file, "x").expect("write");
    assert!(matches!(
        verify_owned(&file),
        Err(OwnershipError::Root { .. })
    ));
}

#[cfg(unix)]
#[test]
fn a_marker_or_root_that_is_a_link_or_a_directory_is_refused() {
    use std::os::unix::fs::symlink;

    let scratch = Scratch::new();

    // A marker that is a link could point at any file that exists.
    let real = scratch.join("real");
    fs::create_dir(&real).expect("mkdir");
    fs::write(scratch.join("elsewhere"), "x").expect("write");
    symlink(scratch.join("elsewhere"), real.join(MARKER_FILE_NAME)).expect("symlink");
    assert!(matches!(
        verify_owned(&real),
        Err(OwnershipError::NotRegular { kind, .. }) if kind == crate::fixture::NodeKind::Symlink
    ));

    // A marker that is a directory is not a marker.
    let odd = scratch.join("odd");
    fs::create_dir_all(odd.join(MARKER_FILE_NAME)).expect("mkdir");
    assert!(matches!(
        verify_owned(&odd),
        Err(OwnershipError::NotRegular { kind, .. }) if kind == crate::fixture::NodeKind::Directory
    ));

    // A root that is itself a link is refused, even when the link points at a marked fixture.
    let marked = scratch.join("marked");
    fs::create_dir(&marked).expect("mkdir");
    fs::write(marked.join(MARKER_FILE_NAME), "m").expect("write");
    let via_link = scratch.join("via-link");
    symlink(&marked, &via_link).expect("symlink");
    assert!(verify_owned(&marked).is_ok());
    assert!(matches!(
        verify_owned(&via_link),
        Err(OwnershipError::Root { .. })
    ));
}

/// Materializes the tiny spec, damages the entry with `damage`, materializes again, and checks
/// that the entry was regenerated because of `reason` and is whole and reusable again.
fn assert_regenerated(options: &MaterializeOptions, reason: &str, damage: impl Fn(&Path)) {
    let scratch = Scratch::new();
    let cache = scratch.cache();
    let spec = tiny_spec();
    let materialize = || {
        cache
            .materialize(&spec, options)
            .unwrap_or_else(|error| panic!("{reason}: {error}"))
    };
    let first = materialize();
    damage(&first.root);

    let second = materialize();
    assert!(
        !second.cache_hit,
        "{reason}: a damaged entry must not be reused"
    );
    assert!(
        second.generation.is_some(),
        "{reason}: it must be generated again"
    );
    let because = second
        .regenerated_because
        .as_deref()
        .unwrap_or_else(|| panic!("{reason}: no reason recorded"));
    assert!(
        because.contains(reason),
        "expected the reason to mention `{reason}`, got: {because}"
    );
    assert_eq!(second.root, first.root);
    assert_clean(&second);
    assert!(
        materialize().cache_hit,
        "{reason}: the regenerated entry is reusable"
    );
}

#[test]
fn a_corrupted_entry_is_regenerated() {
    let options = MaterializeOptions::default();
    assert_regenerated(&options, "is missing", |root| {
        fs::remove_file(root.join("alpha/d1/d2/f1.dat")).expect("remove");
    });
    assert_regenerated(&options, "bytes", |root| {
        // Same name, one wrong size.
        fs::write(root.join("alpha/d0/d0/f0.dat"), vec![b'x'; 200]).expect("write");
    });
    assert_regenerated(&options, "not in the plan", |root| {
        fs::write(root.join("alpha/d0/stray.txt"), "x").expect("write");
    });
    assert_regenerated(&options, "top-level", |root| {
        fs::write(root.join("intruder"), "x").expect("write");
    });
    assert_regenerated(&options, "unusable", |root| {
        fs::write(root.join(MARKER_FILE_NAME), "{ not json").expect("write");
    });
    assert_regenerated(&options, "no ownership marker", |root| {
        fs::remove_file(root.join(MARKER_FILE_NAME)).expect("remove");
    });
    assert_regenerated(&options, "generator version", |root| {
        let text = fs::read_to_string(root.join(MARKER_FILE_NAME)).expect("read");
        let stale = text.replace(
            &format!("\"generator_version\": {GENERATOR_VERSION}"),
            "\"generator_version\": 0",
        );
        assert_ne!(text, stale, "the marker names the generator version");
        fs::write(root.join(MARKER_FILE_NAME), stale).expect("write");
    });
    assert_regenerated(&options, "spec hash", |root| {
        let text = fs::read_to_string(root.join(MARKER_FILE_NAME)).expect("read");
        let marker = read_marker(root).expect("marker");
        let forged = text.replace(&marker.spec_hash, &"0".repeat(64));
        fs::write(root.join(MARKER_FILE_NAME), forged).expect("write");
    });
}

#[test]
fn an_entry_that_is_a_file_where_a_directory_belongs_is_replaced() {
    let scratch = Scratch::new();
    let cache = scratch.cache();
    let spec = tiny_spec();
    let first = cache
        .materialize(&spec, &MaterializeOptions::default())
        .expect("materialize");
    remove_tree(&first.root).expect("remove");
    fs::write(&first.root, "not a directory").expect("write");
    let second = cache
        .materialize(&spec, &MaterializeOptions::default())
        .expect("materialize again");
    assert!(!second.cache_hit);
    assert!(second.root.is_dir());
    assert_clean(&second);
}

#[test]
fn the_cheap_check_samples_and_the_full_check_sees_everything() {
    use crate::fixture::integrity::{CHEAP_SAMPLE, sample_indices};

    let scratch = Scratch::new();
    let cache = scratch.cache();
    let first = materialize(&cache, "node-modules-2k", &MaterializeOptions::default());

    // Damage one file that the sample is certain not to visit.
    let realized: Vec<_> = first.plan().realized(&first.marker.capabilities).collect();
    let sampled = sample_indices(first.plan().manifest_sha256(), realized.len());
    assert_eq!(sampled.len(), CHEAP_SAMPLE);
    let unsampled = realized
        .iter()
        .enumerate()
        .find(|(index, entry)| {
            !sampled.contains(index) && entry.kind == crate::fixture::NodeKind::File
        })
        .map(|(_, entry)| entry.path.to_path_buf(&first.root))
        .expect("most files are not sampled");
    fs::write(&unsampled, "damaged").expect("write");

    let cheap = MaterializeOptions {
        verify: Verify::Cheap,
        ..MaterializeOptions::default()
    };
    let hit = materialize(&cache, "node-modules-2k", &cheap);
    assert!(
        hit.cache_hit,
        "the sample cannot see a file it does not look at"
    );
    let verified = hit.verified.expect("verified");
    assert!(!verified.full);
    assert!(verified.checked <= CHEAP_SAMPLE as u64);

    let full = MaterializeOptions {
        verify: Verify::Full,
        ..MaterializeOptions::default()
    };
    let regenerated = materialize(&cache, "node-modules-2k", &full);
    assert!(!regenerated.cache_hit, "the full check sees every file");
    assert_clean(&regenerated);
}

#[test]
fn the_cheap_check_still_catches_a_missing_top_level_entry_and_a_bad_marker() {
    let scratch = Scratch::new();
    let cache = scratch.cache();
    let cheap = MaterializeOptions {
        verify: Verify::Cheap,
        ..MaterializeOptions::default()
    };
    let spec = tiny_spec();
    let first = cache.materialize(&spec, &cheap).expect("materialize");
    remove_tree(&first.root.join("beta")).expect("remove");
    let second = cache.materialize(&spec, &cheap).expect("materialize again");
    assert!(!second.cache_hit);
    assert!(
        second
            .regenerated_because
            .as_deref()
            .is_some_and(|reason| reason.contains("top-level")),
        "{:?}",
        second.regenerated_because
    );
    assert_clean(&second);
}

#[test]
fn a_partial_directory_left_by_a_killed_generation_is_never_used_as_an_entry() {
    let scratch = Scratch::new();
    let cache = scratch.cache();
    let leftover = cache.root().join(".partial-0123456789abcdef-1-0");
    fs::create_dir_all(leftover.join("half")).expect("mkdir");
    fs::write(leftover.join("half/file"), "x").expect("write");

    let master = materialize(&cache, "wide-1k", &MaterializeOptions::default());
    assert!(!master.cache_hit);
    assert!(
        leftover.exists(),
        "another process's partial directory is not ours to remove"
    );
    assert_ne!(master.root, leftover);
    assert_clean(&master);
    // Nothing of ours was left behind.
    let ours: Vec<String> = fs::read_dir(cache.root())
        .expect("list")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .filter(|name| {
            name.starts_with(".partial-") && !name.starts_with(".partial-0123456789abcdef")
        })
        .collect();
    assert!(ours.is_empty(), "leftover partial directories: {ours:?}");
}

#[test]
fn two_racing_materializations_agree_and_leave_one_sealed_entry() {
    let scratch = Scratch::new();
    let cache = scratch.cache();
    let spec = tiny_spec();
    let results: Vec<Materialized> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..3)
            .map(|_| {
                scope.spawn(|| {
                    cache
                        .materialize(&spec, &MaterializeOptions::default())
                        .expect("materialize")
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|worker| worker.join().expect("worker"))
            .collect()
    });
    assert!(results.iter().all(|result| result.root == results[0].root));
    assert!(
        results
            .iter()
            .all(|result| result.manifest_sha256() == results[0].manifest_sha256())
    );
    assert_clean(&results[0]);

    let entries: Vec<String> = fs::read_dir(cache.root())
        .expect("list")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(
        entries.len(),
        1,
        "one sealed entry and no partial leftovers: {entries:?}"
    );
}

#[test]
fn the_default_cache_lives_in_the_target_directory() {
    use std::ffi::OsString;

    use crate::fixture::cache::default_root;

    assert_eq!(
        default_root(Some(OsString::from("/somewhere/target"))),
        Path::new("/somewhere/target/excise-fixtures.noindex")
    );
    let workspace = default_root(None);
    assert!(
        workspace.ends_with("target/excise-fixtures.noindex"),
        "{}",
        workspace.display()
    );
    let workspace_root = workspace
        .parent()
        .and_then(Path::parent)
        .expect("workspace root");
    assert!(
        workspace_root
            .join("crates/excise-harness/Cargo.toml")
            .is_file(),
        "{} is not this workspace",
        workspace_root.display()
    );
}
