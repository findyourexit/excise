//! Each generator class produces the tree its plan describes, and the oracle, which reads the
//! tree and not the plan, agrees.

#[cfg(unix)]
use std::collections::BTreeSet;

#[cfg(unix)]
use super::support::deep_names_spec;
use super::support::{Scratch, master, spec};
#[cfg(unix)]
use crate::fixture::{Capability, RelPath};
use crate::fixture::{
    GenerateOptions, MARKER_FILE_NAME, MaterializeOptions, NodeKind, Oracle, remove_tree,
};

/// The specs that are small enough to generate in a unit test. `tiny-files-50k` and
/// `tiny-files-1m` are excluded on purpose.
const SMALL: &[&str] = &[
    "all-classes-small",
    "deep-past-path-max",
    "delete-folder",
    "hostile-small",
    "identity-small",
    "mount-boundary",
    "nested",
    "node-modules-2k",
    "refused",
    "twins",
    "wide-1k",
];

fn oracle_of(root: &std::path::Path) -> Oracle {
    Oracle::collect(root).unwrap_or_else(|error| panic!("oracle walk failed: {error}"))
}

#[test]
fn every_class_generates_a_tree_that_the_oracle_confirms() {
    let scratch = Scratch::new();
    for id in SMALL {
        let master = master(&scratch, id);
        assert!(!master.cache_hit, "{id}: a fresh cache cannot hit");
        let report = master
            .generation
            .as_ref()
            .expect("a fresh entry has a report");
        eprintln!(
            "{id}: {} planned, {} created, {} skipped, {} threads, {:?}",
            master.plan().entries().len(),
            report.created,
            report.skipped.values().sum::<u64>(),
            report.threads,
            report.elapsed
        );
        let oracle = oracle_of(&master.root);
        let comparison = oracle.compare(master.plan(), &master.marker.capabilities);
        assert!(
            comparison.is_clean(),
            "{id}: the tree differs from its plan: {:?}",
            comparison.discrepancies.iter().take(5).collect::<Vec<_>>()
        );
        assert_eq!(
            comparison.checked + comparison.unverifiable,
            master.plan().realized(&master.marker.capabilities).count() as u64,
            "{id}: every realized entry is checked or accounted for as unobservable"
        );
        assert_eq!(
            master.marker.created + master.marker.skipped.values().sum::<u64>(),
            master.plan().entries().len() as u64,
            "{id}: every planned entry is created or skipped"
        );
    }
}

#[test]
fn node_modules_2k_has_2389_planned_entries_plus_the_marker() {
    let scratch = Scratch::new();
    let master = master(&scratch, "node-modules-2k");
    let oracle = oracle_of(&master.root);
    let subtree = oracle
        .root()
        .subtree
        .unwrap_or_else(|| panic!("the root has no totals"));
    // 2,389 planned entries (341 directories, 2,048 files) and the ownership marker.
    assert_eq!(subtree.entries, 2_390);
    assert_eq!(subtree.directories, 341);
    assert_eq!(subtree.files, 2_049);
    assert_eq!(subtree.symlinks, 0);
    // Counting the root itself, the marker included: 2,391. Without the marker, the repro's 2,390.
    assert_eq!(oracle.entries.len(), 2_391);
}

#[test]
fn the_deep_class_goes_past_path_max_and_the_oracle_still_walks_it() {
    let scratch = Scratch::new();
    let master = master(&scratch, "deep-past-path-max");
    let deepest = master
        .plan()
        .entries()
        .iter()
        .filter(|entry| entry.kind == NodeKind::Directory)
        .map(|entry| &entry.path)
        .max_by_key(|path| path.as_bytes().len())
        .unwrap_or_else(|| panic!("no directories"));
    assert!(
        deepest.as_bytes().len() > 4096,
        "the deepest path is {} bytes; it must exceed PATH_MAX everywhere",
        deepest.as_bytes().len()
    );

    let oracle = oracle_of(&master.root);
    let found = oracle
        .find(deepest)
        .unwrap_or_else(|| panic!("the oracle did not reach the deepest directory"));
    assert_eq!(found.kind, NodeKind::Directory);
    assert!(
        oracle
            .compare(master.plan(), &master.marker.capabilities)
            .is_clean()
    );

    // Path-based calls cannot reach it: that is why the generator works directory by directory.
    #[cfg(unix)]
    {
        let path = deepest.to_path_buf(&master.root);
        assert!(
            std::fs::symlink_metadata(&path).is_err(),
            "a path of {} bytes should exceed PATH_MAX for path-based calls",
            path.as_os_str().len()
        );
    }
}

#[test]
fn different_thread_counts_generate_identical_trees() {
    let scratch = Scratch::new();
    let spec = spec("all-classes-small");
    let listing = |threads: usize, name: &str| {
        let cache = crate::fixture::FixtureCache::at(scratch.join(name));
        let master = cache
            .materialize(
                &spec,
                &MaterializeOptions {
                    generate: GenerateOptions {
                        threads: Some(threads),
                    },
                    ..MaterializeOptions::default()
                },
            )
            .unwrap_or_else(|error| panic!("{error}"));
        let oracle = oracle_of(&master.root);
        assert_eq!(
            master.generation.as_ref().map(|report| report.threads),
            Some(threads)
        );
        // The marker records this run's timing, so its size differs between runs.
        oracle
            .descendants()
            .filter(|entry| entry.path.as_bytes() != MARKER_FILE_NAME.as_bytes())
            .map(|entry| (entry.path.clone(), entry.kind, entry.size))
            .collect::<Vec<_>>()
    };
    assert_eq!(listing(1, "one"), listing(4, "four"));
}

#[test]
fn the_same_spec_and_seed_give_an_identical_tree_in_two_different_directories() {
    let spec = spec("all-classes-small");
    let first = Scratch::new();
    let second = Scratch::new();
    let a = first
        .cache()
        .materialize(&spec, &MaterializeOptions::default())
        .unwrap_or_else(|error| panic!("{error}"));
    let b = second
        .cache()
        .materialize(&spec, &MaterializeOptions::default())
        .unwrap_or_else(|error| panic!("{error}"));
    assert_ne!(a.root, b.root);
    assert_eq!(a.manifest_sha256(), b.manifest_sha256());
    assert_eq!(a.marker.spec_hash, b.marker.spec_hash);
    assert_eq!(a.marker.realized_sha256, b.marker.realized_sha256);

    // The trees agree entry by entry, including the bytes of every regular file.
    #[cfg(unix)]
    assert_eq!(
        super::oracle::content_digest(&a.root),
        super::oracle::content_digest(&b.root),
        "file contents are a function of the spec and seed only"
    );

    // Another seed gives another tree.
    let third = Scratch::new();
    let c = third
        .cache()
        .materialize(
            &spec,
            &MaterializeOptions {
                seed: Some(spec.seed + 1),
                ..MaterializeOptions::default()
            },
        )
        .unwrap_or_else(|error| panic!("{error}"));
    assert_ne!(a.manifest_sha256(), c.manifest_sha256());
    assert_ne!(a.marker.spec_hash, c.marker.spec_hash);
}

#[cfg(unix)]
#[test]
fn identity_class_hard_links_share_a_file_across_directories() {
    let scratch = Scratch::new();
    let master = master(&scratch, "identity-small");
    let oracle = oracle_of(&master.root);
    let supported = master.marker.capabilities.supports(Capability::HardLinks);
    assert!(
        supported,
        "every Unix file system the harness targets can hard-link"
    );

    // Three groups of three names each, and nothing else has more than one name.
    assert_eq!(oracle.hard_links.len(), 3);
    for group in &oracle.hard_links {
        assert_eq!(group.nlink, 3);
        assert_eq!(group.paths.len(), 3, "every name is inside the walked tree");
        let directories: BTreeSet<Vec<u8>> = group
            .paths
            .iter()
            .map(|path| path.parent_bytes().to_vec())
            .collect();
        assert!(
            directories.len() >= 2,
            "names of one file lie in several directories"
        );
        let identity = |path: &RelPath| {
            let entry = oracle
                .find(path)
                .unwrap_or_else(|| panic!("{path} missing"));
            (entry.dev, entry.ino, entry.size)
        };
        assert!(
            group
                .paths
                .iter()
                .all(|path| identity(path) == identity(&group.paths[0]))
        );
    }
}

#[cfg(unix)]
#[test]
fn identity_class_symlinks_are_dangling_looping_or_valid_and_never_followed() {
    let scratch = Scratch::new();
    let master = master(&scratch, "identity-small");
    let oracle = oracle_of(&master.root);
    let target_of = |path: &str| -> String {
        let path = RelPath::from_bytes(path).unwrap_or_else(|error| panic!("{error}"));
        let entry = oracle
            .find(&path)
            .unwrap_or_else(|| panic!("{path} missing"));
        assert_eq!(entry.kind, NodeKind::Symlink, "{path}");
        entry
            .symlink_target
            .as_ref()
            .map(|target| String::from_utf8_lossy(target.as_bytes()).into_owned())
            .unwrap_or_default()
    };
    assert_eq!(target_of("identity/loops/self"), "self");
    assert_eq!(target_of("identity/loops/pair-a"), "pair-b");
    assert_eq!(target_of("identity/loops/pair-b"), "pair-a");
    assert_eq!(target_of("identity/loops/cycle/again"), "../cycle");
    assert_eq!(
        target_of("identity/dangling/dangling0"),
        "../missing/nothing0"
    );
    assert_eq!(target_of("identity/symlinks/file-link"), "target.dat");
    assert_eq!(target_of("identity/symlinks/dir-link"), "real-dir");

    // The links behave as their names say when a path-based call does follow them.
    let follow = |path: &str| std::fs::metadata(master.root.join(path));
    assert!(follow("identity/dangling/dangling0").is_err(), "dangling");
    assert!(
        follow("identity/loops/self").is_err(),
        "a self-loop cannot be resolved"
    );
    assert!(
        follow("identity/loops/pair-a").is_err(),
        "a two-link cycle cannot be resolved"
    );
    assert!(follow("identity/symlinks/file-link").is_ok_and(|meta| meta.is_file()));
    assert!(follow("identity/symlinks/dir-link").is_ok_and(|meta| meta.is_dir()));
    // The directory loop resolves for a while, then runs into the kernel's link limit.
    let mut looping = String::from("identity/loops/cycle");
    for _ in 0..64 {
        looping.push_str("/again");
    }
    assert!(
        follow(&looping).is_err(),
        "a directory loop must exhaust the link limit"
    );

    // The oracle counts each link as a link, and the walk was finite.
    let subtree = oracle.root().subtree.unwrap_or_default();
    assert_eq!(
        subtree.symlinks,
        2 + 2 + 1 + 2 + 1,
        "dangling, valid, self, pair, cycle"
    );
}

#[cfg(unix)]
#[test]
fn identity_class_sparse_files_allocate_far_less_than_their_size() {
    let scratch = Scratch::new();
    let master = master(&scratch, "identity-small");
    assert!(
        master.marker.capabilities.supports(Capability::SparseFiles),
        "APFS and ext4 both keep holes: {:?}",
        master.marker.capabilities.status(Capability::SparseFiles)
    );
    let oracle = oracle_of(&master.root);
    for (path, apparent) in [
        ("identity/sparse/s0.bin", 64 * 1024 * 1024),
        ("identity/sparse/s1.bin", 32 * 1024 * 1024),
    ] {
        let entry = oracle
            .find(&RelPath::from_bytes(path).unwrap_or_else(|error| panic!("{error}")))
            .unwrap_or_else(|| panic!("{path} missing"));
        assert_eq!(entry.size, apparent);
        let allocated = entry.allocated.unwrap_or(u64::MAX);
        assert!(
            allocated < 1024 * 1024,
            "{path}: {allocated} bytes allocated for {apparent} apparent bytes"
        );
    }
}

#[cfg(unix)]
#[test]
fn identity_class_clones_are_created_only_where_the_file_system_can_clone() {
    let scratch = Scratch::new();
    let master = master(&scratch, "identity-small");
    let oracle = oracle_of(&master.root);
    let original = RelPath::from_bytes("identity/clones/original.bin")
        .unwrap_or_else(|error| panic!("{error}"));
    let clone =
        RelPath::from_bytes("identity/clones/clone0.bin").unwrap_or_else(|error| panic!("{error}"));

    if master.marker.capabilities.supports(Capability::Clones) {
        let original = oracle
            .find(&original)
            .unwrap_or_else(|| panic!("original missing"));
        let clone = oracle
            .find(&clone)
            .unwrap_or_else(|| panic!("clone missing"));
        assert_eq!(original.size, clone.size);
        assert_ne!(
            original.ino, clone.ino,
            "a clone is its own file, not another name"
        );
        assert_eq!(original.nlink, Some(1));
        assert!(!master.marker.skipped.contains_key(&Capability::Clones));
    } else {
        // Skipped, and the reason is on record.
        assert!(oracle.find(&original).is_none());
        assert_eq!(master.marker.skipped.get(&Capability::Clones), Some(&4));
        let status = master
            .marker
            .capabilities
            .status(Capability::Clones)
            .unwrap_or_else(|| panic!("no clone status"));
        assert!(!status.supported && !status.detail.is_empty());
        assert_ne!(master.marker.realized_sha256, master.marker.manifest_sha256);
    }
}

#[cfg(unix)]
#[test]
fn hostile_class_names_survive_creation_and_unreadable_entries_are_marked() {
    let scratch = Scratch::new();
    let master = master(&scratch, "hostile-small");
    let capabilities = &master.marker.capabilities;
    let oracle = oracle_of(&master.root);
    assert!(oracle.compare(master.plan(), capabilities).is_clean());

    // Every planned hostile name that the file system accepts is there, byte for byte.
    let present = |name: &[u8]| {
        oracle
            .descendants()
            .any(|entry| entry.path.file_name() == Some(name))
    };
    assert_eq!(
        present(b"ctl-\x01-soh"),
        capabilities.supports(Capability::ControlCharacterNames)
    );
    assert!(
        present("bidi-\u{202e}fdp.exe".as_bytes()),
        "bidi overrides are ordinary names"
    );
    assert!(present(&[b'a'; 255]), "a 255-byte name");
    assert!(
        present(b"bad-\xff-byte") == capabilities.supports(Capability::InvalidUtf8Names),
        "invalid UTF-8 exists exactly where the file system allows it"
    );
    assert!(
        capabilities.supports(Capability::ControlCharacterNames),
        "Unix file systems accept control characters"
    );

    let locked = oracle
        .find(
            &RelPath::from_bytes("hostile/unreadable/locked-000")
                .unwrap_or_else(|error| panic!("{error}")),
        )
        .unwrap_or_else(|| panic!("locked-000 missing"));
    assert_eq!(locked.mode, Some(0o000));
    let restricted_file = oracle
        .find(
            &RelPath::from_bytes("hostile/unreadable/locked-file-000.bin")
                .unwrap_or_else(|error| panic!("{error}")),
        )
        .unwrap_or_else(|| panic!("locked file missing"));
    assert_eq!(restricted_file.mode, Some(0o000));
    if capabilities.running_as_root() == Some(false) {
        assert!(!locked.readable);
        assert_eq!(locked.subtree.map(|subtree| subtree.entries), Some(0));
        assert!(!restricted_file.readable);
        let unreadable = oracle
            .find(
                &RelPath::from_bytes("hostile/unreadable")
                    .unwrap_or_else(|error| panic!("{error}")),
            )
            .and_then(|entry| entry.subtree)
            .unwrap_or_default();
        assert_eq!(
            unreadable.unreadable_directories, 2,
            "mode 000 and mode 100"
        );
        assert!(
            oracle.compare(master.plan(), capabilities).unverifiable > 0,
            "the contents of unreadable directories cannot be observed"
        );
    }
}

#[cfg(unix)]
#[test]
fn hostile_fixtures_clean_up_completely_even_with_unreadable_directories() {
    let scratch = Scratch::new();
    let path = scratch.path().to_path_buf();
    let root = master(&scratch, "hostile-small").root;
    assert!(root.exists());
    // Plain `remove_dir_all` cannot enter a mode 000 directory; this is why the harness has its
    // own removal.
    let restricted = root.join("hostile/unreadable/locked-000");
    if std::fs::read_dir(&restricted).is_err() {
        assert!(std::fs::remove_dir_all(&root).is_err());
    }
    drop(scratch);
    assert!(
        !path.exists(),
        "the scratch directory must be gone: {}",
        path.display()
    );
}

#[cfg(unix)]
#[test]
fn the_unwritable_class_refuses_every_change_and_still_cleans_up() {
    let scratch = Scratch::new();
    let path = scratch.path().to_path_buf();
    let master = master(&scratch, "refused");
    let capabilities = &master.marker.capabilities;
    let oracle = oracle_of(&master.root);
    assert!(oracle.compare(master.plan(), capabilities).is_clean());
    assert_eq!(spec("refused").planned_entry_count(), 4);
    let root_bytes = u64::try_from(master.root.as_os_str().len()).expect("a length");
    assert!(
        !spec("refused").removable_by_path(root_bytes),
        "a path-based removal cannot remove files from a mode 555 directory, wherever it is"
    );

    let directory = oracle
        .find(&RelPath::from_bytes("hostile/unwritable").unwrap_or_else(|error| panic!("{error}")))
        .unwrap_or_else(|| panic!("the unwritable directory is missing"));
    assert_eq!(directory.mode, Some(0o555));

    // A user that is not root cannot remove what is inside: the entry is listed and readable, and
    // that is all. Root ignores the mode, which the marker records.
    let stuck = master.root.join("hostile/unwritable/stuck.txt");
    assert!(stuck.is_file(), "the file is there to be listed");
    if capabilities.running_as_root() == Some(false) {
        assert!(std::fs::remove_file(&stuck).is_err());
        assert!(std::fs::remove_dir_all(&master.root).is_err());
        assert!(stuck.is_file(), "the refused removal changed nothing");
    }
    drop(scratch);
    assert!(
        !path.exists(),
        "the harness's own removal restores access first: {}",
        path.display()
    );
}

#[test]
fn a_fixture_that_refuses_only_a_user_who_is_not_root_is_refused_to_root() {
    use crate::fixture::{Capabilities, FixtureError, run::require_unprivileged_user};

    let unwritable = spec("refused");
    assert!(unwritable.needs_unprivileged_user());
    // The unreadable directories of the hostile class are marked, not relied on: root reads them.
    assert!(!spec("hostile-small").needs_unprivileged_user());
    assert!(!spec("delete-folder").needs_unprivileged_user());

    let capabilities = Capabilities::all_supported();
    let as_root = capabilities.clone().with_running_as_root(Some(true));
    let refusal = require_unprivileged_user(&unwritable, &as_root)
        .expect_err("root is not refused by a mode");
    assert!(
        matches!(&refusal, FixtureError::NeedsUnprivilegedUser { id } if id == "refused"),
        "{refusal}"
    );
    assert!(refusal.to_string().contains("not root"), "{refusal}");

    for answer in [Some(false), None] {
        let capabilities = capabilities.clone().with_running_as_root(answer);
        assert!(
            require_unprivileged_user(&unwritable, &capabilities).is_ok(),
            "{answer:?}"
        );
    }
    // A fixture that needs nothing of the user is never refused, root or not.
    assert!(require_unprivileged_user(&spec("delete-file"), &as_root).is_ok());
}

#[test]
fn removing_a_tree_that_is_missing_is_not_an_error() {
    let scratch = Scratch::new();
    assert!(remove_tree(&scratch.join("never-existed")).is_ok());
}

#[test]
fn generation_time_is_recorded() {
    let scratch = Scratch::new();
    let master = master(&scratch, "node-modules-2k");
    let report = master
        .generation
        .as_ref()
        .unwrap_or_else(|| panic!("a fresh entry has a generation report"));
    assert_eq!(report.created, 2_389);
    assert!(report.threads >= 1);
    assert_eq!(master.generation_time(), Some(report.elapsed));
    assert_eq!(
        u64::try_from(report.elapsed.as_millis()).unwrap_or(u64::MAX),
        master.marker.generation_ms
    );
}

/// A shaped part with every feature: three depths, links that resolve and links that do not, and
/// files with several names.
const SHAPED: &str = r#"
    schema_version = 1
    id = "shaped-small"
    description = "A shaped tree."
    seed = 11

    [[parts]]
    kind = "shaped"
    root = "home"
    max_file_bytes = 5000
    dangling_symlinks = 3

    [[parts.levels]]
    directories = 4
    files = 6
    symlinks = 1

    [[parts.levels]]
    directories = 3
    files = 40
    symlinks = 6

    [[parts.levels]]
    files = 60
    symlinks = 2

    [parts.subdirectories_per_directory]
    0 = 3
    1 = 2
    2 = 1

    [parts.files_per_directory]
    0 = 2
    1 = 3
    8 = 2
    32 = 1

    [parts.file_sizes]
    0 = 1
    1 = 2
    64 = 4
    4096 = 2

    [parts.directory_name_lengths]
    3 = 2
    12 = 1

    [parts.file_name_lengths]
    5 = 3
    20 = 1

    [parts.symlink_name_lengths]
    8 = 1

    [[parts.hard_links]]
    class = 1
    names = 5
    [parts.hard_links.group_sizes]
    2 = 2

    [[parts.hard_links]]
    class = 64
    names = 6
    [parts.hard_links.group_sizes]
    2 = 1
    4 = 1
"#;

#[test]
fn a_shaped_part_generates_the_tree_its_plan_describes_and_the_oracle_confirms() {
    let scratch = Scratch::new();
    let spec = crate::fixture::FixtureSpec::from_toml_str(SHAPED).expect("a spec");
    let master = super::support::master_of(&scratch, &spec);

    let oracle = oracle_of(&master.root);
    let comparison = oracle.compare(master.plan(), &master.marker.capabilities);

    assert!(
        comparison.is_clean(),
        "the tree differs from its plan: {:?}",
        comparison.discrepancies.iter().take(5).collect::<Vec<_>>()
    );
    assert_eq!(
        master.plan().entries().len() as u64,
        spec.planned_entry_count()
    );
    assert!(
        master
            .plan()
            .entries()
            .iter()
            .all(|entry| entry.kind != NodeKind::File || entry.size <= 5000),
        "no file is larger than the cap"
    );
    #[cfg(unix)]
    {
        assert!(
            master.marker.skipped.is_empty(),
            "every capability is there on Unix"
        );
        // Four groups of hard links, by the oracle's own count of files with several names.
        assert_eq!(oracle.hard_links.len(), 4);
        assert!(oracle.hard_links.iter().all(|group| {
            group.paths.len() >= 2 && usize::try_from(group.nlink) == Ok(group.paths.len())
        }));
        // Three of the nine links dangle, and the six others lead to a file that exists.
        let mut dangling = 0;
        for entry in oracle
            .descendants()
            .filter(|entry| entry.kind == NodeKind::Symlink)
        {
            if std::fs::metadata(entry.path.to_path_buf(&master.root)).is_err() {
                dangling += 1;
            }
        }
        assert_eq!(dangling, 3);
    }
}

/// Two shaped parts, `home` and `work`, with one group of two names each, among files of one size
/// each. Each part numbers its groups from its own start, so a plan that let the numbers meet
/// would make one group of four names of two sizes, which the generator would link into one inode
/// and the manifest would not describe: the plan has two groups, and the tree two files with two
/// names each.
#[cfg(unix)]
#[test]
fn two_shaped_parts_with_one_group_each_make_two_groups_and_two_inodes() {
    let part = |root: &str, size: u64| {
        format!(
            "[[parts]]\nkind = \"shaped\"\nroot = \"{root}\"\nmax_file_bytes = 100000\n\n\
             [[parts.levels]]\nfiles = 4\n\n\
             [parts.files_per_directory]\n4 = 1\n\n\
             [parts.file_name_lengths]\n8 = 1\n\n\
             [parts.file_sizes]\n{size} = 4\n\n\
             [[parts.hard_links]]\nclass = {size}\nnames = 2\n\
             [parts.hard_links.group_sizes]\n2 = 1\n\n"
        )
    };
    let text = format!(
        "schema_version = 1\nid = \"two-shaped\"\ndescription = \"Two shaped parts.\"\nseed = 3\n\n{}{}",
        part("home", 1),
        part("work", 64)
    );
    let scratch = Scratch::new();
    let spec = crate::fixture::FixtureSpec::from_toml_str(&text).expect("a spec");
    let master = super::support::master_of(&scratch, &spec);

    let oracle = oracle_of(&master.root);
    let comparison = oracle.compare(master.plan(), &master.marker.capabilities);
    assert!(
        comparison.is_clean(),
        "the tree differs from its plan: {:?}",
        comparison.discrepancies.iter().take(5).collect::<Vec<_>>()
    );
    let numbers: BTreeSet<u32> = master
        .plan()
        .entries()
        .iter()
        .filter_map(|entry| entry.link_group)
        .collect();
    assert_eq!(numbers.len(), 2, "two groups in the plan");
    assert_eq!(oracle.hard_links.len(), 2, "two files with several names");
    assert!(
        oracle
            .hard_links
            .iter()
            .all(|group| group.paths.len() == 2 && group.nlink == 2),
        "{:?}",
        oracle.hard_links
    );
    let inodes: BTreeSet<(u64, u64)> = oracle
        .hard_links
        .iter()
        .map(|group| (group.dev, group.ino))
        .collect();
    assert_eq!(inodes.len(), 2, "two inodes");
}

/// The links of a shaped part point at their anchor by the shortest relative path. A link that
/// climbed to the top of the part and came back down through 31 folders with names of 255 bytes
/// would hold 8 KiB, which no file system takes as the text of a link: macOS refuses past 1,023
/// bytes and the common Linux file systems past 4,095.
#[cfg(unix)]
#[test]
fn a_shaped_part_32_levels_deep_with_long_names_is_built_with_short_link_targets() {
    let scratch = Scratch::new();
    let spec = crate::fixture::FixtureSpec::from_toml_str(&deep_names_spec()).expect("a spec");
    let master = super::support::master_of(&scratch, &spec);

    let comparison = oracle_of(&master.root).compare(master.plan(), &master.marker.capabilities);
    assert!(
        comparison.is_clean(),
        "the tree differs from its plan: {:?}",
        comparison.discrepancies.iter().take(5).collect::<Vec<_>>()
    );
    let deepest = master
        .plan()
        .entries()
        .iter()
        .map(|entry| entry.path.as_bytes().len())
        .max()
        .expect("entries");
    assert!(deepest > 7_900, "the deepest path is {deepest} bytes");
    let targets: Vec<usize> = master
        .plan()
        .entries()
        .iter()
        .filter_map(|entry| entry.target.as_ref())
        .map(|target| target.as_bytes().len())
        .collect();
    assert_eq!(targets.len(), 2);
    assert!(
        targets.iter().all(|length| *length <= 8),
        "the targets are {targets:?} bytes long"
    );

    // One of the two links is asked to dangle and the other points at the file, 8 KiB down: the
    // oracle, which reads the tree, agrees with the plan about both targets.
    assert_eq!(
        master
            .plan()
            .entries()
            .iter()
            .filter_map(|entry| entry.target.as_ref())
            .filter(|target| target.as_bytes() == b"missing")
            .count(),
        1,
        "one link dangles"
    );

    // The walk reaches the bottom through the handle of each folder, which no path could, and
    // counts the two links without asking where they point.
    let shape = crate::shape::profile(&master.root, crate::shape::WalkOptions::default())
        .expect("the walk");
    assert_eq!(shape.symbolic_links.count, 2);
    assert_eq!(shape.unreadable.errors, 0);
}
