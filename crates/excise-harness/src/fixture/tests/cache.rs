//! The cache: a hit skips generation, a damaged entry is regenerated, entries are marked, and
//! publishing is safe.

#[cfg(unix)]
use std::path::PathBuf;
use std::{fs, path::Path};

use super::support::{Scratch, master, spec, tiny_spec};
#[cfg(unix)]
use super::support::{link_to, long_way_round, path_of_len};
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

/// The longest name the cache gives a directory directly below its root, which is that of a
/// fixture still being generated: `.partial-` (9 bytes), 16 hexadecimal digits, `-`, a process
/// number of at most 10 digits, `-`, and a count of at most 20.
const LONGEST_NAME: u64 = 9 + 16 + 1 + 10 + 1 + 20;

/// What a fixture root in a cache measures at the most: the path of the cache directory, as it
/// resolves, a separator, and the longest name the cache gives a directory below it. The cache
/// directory need not exist, and however many of its names are missing, it counts below the
/// longest ancestor that does; a relative root counts as the absolute path it names.
#[test]
fn the_longest_entry_path_of_a_cache_is_its_resolved_root_a_separator_and_the_longest_name() {
    let scratch = Scratch::new();
    let base = scratch.resolved_path();
    for root in [
        base.clone(),
        base.join("cache"),
        base.join("below").join("a").join("cache"),
    ] {
        let bytes = u64::try_from(root.as_os_str().len()).expect("a length");
        assert_eq!(
            FixtureCache::at(&root)
                .longest_entry_path_bytes()
                .expect("the path resolves"),
            bytes + 1 + LONGEST_NAME,
            "{}",
            root.display()
        );
    }

    let relative = Path::new("a-cache-root-below-the-current-directory");
    let named = std::env::current_dir()
        .expect("a current directory")
        .join(relative);
    assert_eq!(
        FixtureCache::at(relative)
            .longest_entry_path_bytes()
            .expect("a relative root resolves"),
        FixtureCache::at(&named)
            .longest_entry_path_bytes()
            .expect("the absolute root resolves"),
        "a relative root counts as the absolute path it names"
    );
}

/// A root below a symbolic link counts as the longer of where it is written and where it leads,
/// the names that do not exist yet included. Unix only, like the other tests that make a link.
#[cfg(unix)]
#[test]
fn a_root_below_a_link_counts_the_longer_of_where_it_is_written_and_where_it_leads() {
    let scratch = Scratch::new();
    let base = scratch.resolved_path();
    let bytes = |path: &Path| u64::try_from(path.as_os_str().len()).expect("a length");
    let measured = |root: &Path| {
        FixtureCache::at(root)
            .longest_entry_path_bytes()
            .expect("the path resolves")
    };

    // A short link to a deep directory leads far: the root counts as the deep directory and what
    // is below it, whether the names below the link exist or not.
    let deep = path_of_len(&base, 300);
    fs::create_dir_all(&deep).expect("a deep directory");
    let link = base.join("link");
    link_to(&link, &deep);
    assert_eq!(
        measured(&link),
        bytes(&deep) + 1 + LONGEST_NAME,
        "the link itself"
    );
    for below in [Path::new("cache"), Path::new("not/yet/cache")] {
        assert_eq!(
            measured(&link.join(below)),
            bytes(&deep.join(below)) + 1 + LONGEST_NAME,
            "below the link: `{}`",
            below.display()
        );
    }

    // A long link to a short directory is longer where it is written than where it leads, and
    // what is written counts.
    let near = base.join("near");
    fs::create_dir(&near).expect("a short directory");
    let long_link = base.join("l".repeat(200));
    link_to(&long_link, &near);
    let written = long_link.join("cache");
    assert_eq!(measured(&written), bytes(&written) + 1 + LONGEST_NAME);
}

/// The system works on the content of a link and what is left of the path after it, and that can
/// be longer than the root as it is written and as it resolves: below a short link to a long path
/// that ends in a link back to a short directory, the root counts as that pathname. Unix only.
#[cfg(unix)]
#[test]
fn a_root_counts_the_pathname_the_system_works_on_at_a_link_in_it() {
    let scratch = Scratch::new();
    let base = scratch.resolved_path();
    let way = long_way_round(&base, 700);
    let bytes = |path: &Path| u64::try_from(path.as_os_str().len()).expect("a length");
    let measured = |root: &Path| {
        FixtureCache::at(root)
            .longest_entry_path_bytes()
            .expect("the path resolves")
    };

    // Where the way leads, the root is short.
    let near = way.near.join("cache");
    assert_eq!(measured(&near), bytes(&near) + 1 + LONGEST_NAME);

    // Below the link that begins the way, it is the 705 bytes of the content of the link and
    // `/cache`, short as the root is written and as it resolves.
    let start = way.start.join("cache");
    assert_eq!(
        measured(&start),
        bytes(&way.back.join("cache")) + 1 + LONGEST_NAME
    );
    assert!(bytes(&start) + 500 < bytes(&way.back.join("cache")));
}

/// The path of a root counts as it is spelled, `.` names and repeated separators included: the
/// system is handed the text of the root and counts all of it against `PATH_MAX`, and
/// `std::path::absolute`, which drops them, is not what the cache hands it. So `<dir>//cache` is a
/// byte longer than `<dir>/cache`, `<dir>/./cache` two, and a relative root is the current
/// directory and the text written after it. Unix only: where `absolute` gives the spelling the
/// system sees (Windows), that is the normalized one.
#[cfg(unix)]
#[test]
fn a_root_counts_as_it_is_spelled() {
    let scratch = Scratch::new();
    let base = scratch.resolved_path();
    let measured = |root: &str| {
        FixtureCache::at(root)
            .longest_entry_path_bytes()
            .expect("the path resolves")
    };
    let dir = base.display().to_string();
    let plain = measured(&format!("{dir}/cache"));
    for (root, extra) in [
        (format!("{dir}//cache"), 1),
        (format!("{dir}/./cache"), 2),
        (format!("{dir}/cache/"), 1),
        (format!("{dir}/cache/."), 2),
        (format!("{dir}///cache"), 2),
        (format!("{dir}/./././cache"), 6),
    ] {
        assert_eq!(measured(&root), plain + extra, "{root}");
    }

    // A relative root counts as the current directory and the text after it.
    let current = std::env::current_dir().expect("a current directory");
    let plain = measured("a-cache-below-the-current-directory");
    assert_eq!(
        plain,
        measured(&format!(
            "{}/a-cache-below-the-current-directory",
            current.display()
        ))
    );
    assert_eq!(measured("./a-cache-below-the-current-directory"), plain + 2);
    assert_eq!(measured("a-cache-below-the-current-directory//"), plain + 2);
}

/// A root that cannot be resolved has no length: the empty root; one that exists and is not a
/// folder, a file, however it is spelled, and a link to one; and (on Unix, where they are not
/// reported as one that is not there) one below a name that is not a folder, and one that goes
/// through a link that leads to a name that is not there, whether the link is the root or a
/// directory above it, absolute or relative.
#[test]
fn a_root_that_cannot_be_resolved_has_no_length() {
    assert!(
        FixtureCache::at("").longest_entry_path_bytes().is_err(),
        "the empty root"
    );

    let scratch = Scratch::new();
    let file = scratch.join("a-file");
    fs::write(&file, b"not a folder").expect("a file");
    assert!(
        FixtureCache::at(&file).longest_entry_path_bytes().is_err(),
        "a root that is a file"
    );

    #[cfg(unix)]
    {
        assert!(
            FixtureCache::at(file.join("cache"))
                .longest_entry_path_bytes()
                .is_err(),
            "below a file"
        );

        // A name that exists and is not a folder ends the path, whatever follows it: a link to a
        // file, a separator or a `.` after the file, and a link whose content goes on after it.
        link_to(&scratch.join("to-file"), &file);
        link_to(&scratch.join("slash"), Path::new("a-file/"));
        link_to(&scratch.join("dot"), Path::new("a-file/."));
        fs::create_dir(scratch.join("a-folder")).expect("a folder");
        link_to(&scratch.join("up"), Path::new("a-file/../a-folder"));
        let dir = scratch.path().display().to_string();
        for root in [
            scratch.join("to-file"),
            scratch.join("slash"),
            scratch.join("dot"),
            scratch.join("up"),
            PathBuf::from(format!("{dir}/a-file/")),
            PathBuf::from(format!("{dir}/a-file/.")),
        ] {
            assert!(
                FixtureCache::at(&root).longest_entry_path_bytes().is_err(),
                "not a folder: {}",
                root.display()
            );
        }

        // A link that leads to a name that is not there exists, so it is not a name the cache
        // can make: the root cannot be resolved, as the link itself or below it.
        link_to(&scratch.join("relative"), Path::new("missing"));
        link_to(&scratch.join("absolute"), &scratch.join("missing"));
        for link in ["relative", "absolute"] {
            let link = scratch.join(link);
            for root in [link.clone(), link.join("cache")] {
                assert!(
                    FixtureCache::at(&root).longest_entry_path_bytes().is_err(),
                    "a link that leads nowhere: {}",
                    root.display()
                );
            }
        }
    }
}
