//! Each generator class produces the tree its plan describes, and the oracle, which reads the
//! tree and not the plan, agrees.

#[cfg(unix)]
use std::collections::BTreeSet;

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
    "node-modules-2k",
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
