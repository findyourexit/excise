//! The oracle reads facts from the file system, and those facts agree with an independent walk
//! and with themselves.

use std::path::Path;

use serde_json::Value;

use super::support::{Scratch, master};
use crate::fixture::{Capability, Oracle};
#[cfg(unix)]
use crate::fixture::{MARKER_FILE_NAME, NodeKind, OracleEntry, RelPath};

fn oracle_of(root: &Path) -> Oracle {
    Oracle::collect(root).expect("the oracle walk should succeed")
}

/// A digest of the names, kinds, and file contents of a tree, marker excluded: two trees have the
/// same digest exactly when they hold the same bytes under the same names.
#[cfg(unix)]
pub(super) fn content_digest(root: &Path) -> String {
    use std::io::Read as _;

    use sha2::{Digest as _, Sha256};

    use crate::fixture::{generate::open_existing, spec::hex, sys::Dir};

    let oracle = oracle_of(root);
    let directory = Dir::open_root(root).expect("open the root");
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    for entry in oracle.descendants() {
        if entry.path.as_bytes() == MARKER_FILE_NAME.as_bytes() {
            continue;
        }
        hasher.update(entry.path.as_bytes());
        hasher.update([0, entry.kind as u8]);
        if entry.kind == NodeKind::File && entry.readable {
            let parent =
                open_existing(&directory, entry.path.parent_bytes()).expect("open the parent");
            let mut file = parent
                .open_regular_for_read(entry.path.file_name().unwrap_or_default())
                .expect("open the file");
            loop {
                let read = file.read(&mut buffer).expect("read the file");
                if read == 0 {
                    break;
                }
                hasher.update(&buffer[..read]);
            }
        }
    }
    hex(&hasher.finalize())
}

/// An independent walk with `std`, which names every path in full and so cannot reach a tree
/// deeper than `PATH_MAX`: `(path, kind, size, blocks * 512, dev, ino, nlink, mode)`.
#[cfg(unix)]
type Facts = (RelPath, NodeKind, u64, u64, u64, u64, u64, u32);

#[cfg(unix)]
fn independent_facts(root: &Path) -> Vec<Facts> {
    use std::os::unix::{ffi::OsStrExt as _, fs::MetadataExt as _};

    fn walk(root: &Path, relative: &RelPath, out: &mut Vec<Facts>) {
        let directory = relative.to_path_buf(root);
        let Ok(listing) = std::fs::read_dir(&directory) else {
            return; // unreadable: the contents cannot be listed
        };
        let mut names: Vec<Vec<u8>> = listing
            .map(|entry| {
                entry
                    .expect("a directory entry")
                    .file_name()
                    .as_bytes()
                    .to_vec()
            })
            .collect();
        names.sort();
        for name in names {
            let path = relative.try_join(&name).expect("a valid name");
            let meta = std::fs::symlink_metadata(path.to_path_buf(root)).expect("lstat");
            let kind = if meta.file_type().is_dir() {
                NodeKind::Directory
            } else if meta.file_type().is_symlink() {
                NodeKind::Symlink
            } else if meta.file_type().is_file() {
                NodeKind::File
            } else {
                NodeKind::Other
            };
            out.push((
                path.clone(),
                kind,
                meta.size(),
                meta.blocks() * 512,
                meta.dev(),
                meta.ino(),
                meta.nlink(),
                meta.mode() & 0o7777,
            ));
            if kind == NodeKind::Directory {
                walk(root, &path, out);
            }
        }
    }

    let mut out = Vec::new();
    walk(root, &RelPath::root(), &mut out);
    out
}

#[cfg(unix)]
#[test]
fn the_oracle_agrees_with_an_independent_lstat_walk_on_every_reachable_class() {
    let scratch = Scratch::new();
    // Everything except the deep chain, which `std` cannot reach.
    for id in [
        "node-modules-2k",
        "wide-1k",
        "delete-folder",
        "identity-small",
        "hostile-small",
        "mount-boundary",
    ] {
        let master = master(&scratch, id);
        let oracle = oracle_of(&master.root);
        let independent = independent_facts(&master.root);

        let from_oracle: Vec<Facts> = oracle
            .descendants()
            .filter(|entry| {
                // The independent walk cannot enter directories the oracle also found unreadable.
                independent.iter().any(|facts| facts.0 == entry.path)
            })
            .map(|entry| {
                (
                    entry.path.clone(),
                    entry.kind,
                    entry.size,
                    entry.allocated.expect("allocated on Unix"),
                    entry.dev.expect("dev on Unix"),
                    entry.ino.expect("ino on Unix"),
                    entry.nlink.expect("nlink on Unix"),
                    entry.mode.expect("mode on Unix"),
                )
            })
            .collect();
        assert_eq!(
            from_oracle, independent,
            "{id}: the oracle and an independent lstat walk disagree"
        );
        // And the oracle found nothing the independent walk could have seen but did not.
        let seen: std::collections::BTreeSet<&RelPath> =
            independent.iter().map(|facts| &facts.0).collect();
        for entry in oracle.descendants() {
            let reachable = entry
                .path
                .parent()
                .is_none_or(|parent| parent.is_root() || seen.contains(&parent));
            if reachable {
                assert!(
                    seen.contains(&entry.path),
                    "{id}: the walk missed `{}`",
                    entry.path
                );
            }
        }
    }
}

/// Recomputes the totals below `directory` from the raw per-entry facts, the slow way.
#[cfg(unix)]
fn naive_totals(oracle: &Oracle, directory: &RelPath) -> [u64; 9] {
    let mut identities = std::collections::BTreeSet::new();
    let mut totals = [0_u64; 9];
    for entry in oracle.descendants() {
        if entry.path == *directory || !entry.path.starts_with(directory) {
            continue;
        }
        totals[0] += 1;
        match entry.kind {
            NodeKind::Directory => {
                totals[1] += 1;
                totals[6] += entry.size;
                totals[7] += entry.allocated.unwrap_or(0);
                if !entry.readable {
                    totals[8] += 1;
                }
            }
            kind => {
                totals[if kind == NodeKind::File { 2 } else { 3 }] += 1;
                totals[4] += entry.size;
                let first_time = match (entry.dev, entry.ino, entry.nlink) {
                    (Some(dev), Some(ino), Some(nlink)) if nlink > 1 => {
                        identities.insert((dev, ino))
                    }
                    _ => true,
                };
                if first_time {
                    totals[5] += entry.allocated.unwrap_or(0);
                }
            }
        }
    }
    totals
}

#[cfg(unix)]
#[test]
fn directory_totals_equal_a_slow_recount_of_the_entries_below_them() {
    let scratch = Scratch::new();
    for id in ["all-classes-small", "identity-small"] {
        let master = master(&scratch, id);
        let oracle = oracle_of(&master.root);
        let mut checked = 0;
        for entry in oracle
            .entries
            .iter()
            .filter(|entry| entry.kind == NodeKind::Directory)
        {
            let subtree = entry.subtree.expect("directories carry totals");
            let [
                entries,
                directories,
                files,
                others,
                apparent,
                allocated,
                directory_apparent,
                directory_allocated,
                unreadable,
            ] = naive_totals(&oracle, &entry.path);
            let context = format!("{id}: `{}`", entry.path);
            assert_eq!(subtree.entries, entries, "{context}");
            assert_eq!(subtree.directories, directories, "{context}");
            assert_eq!(subtree.files, files, "{context}");
            assert_eq!(subtree.symlinks + subtree.others, others, "{context}");
            assert_eq!(subtree.apparent_bytes, apparent, "{context}");
            assert_eq!(
                subtree.allocated_bytes,
                Some(allocated),
                "{context}: hard links counted once"
            );
            assert_eq!(
                subtree.directory_apparent_bytes, directory_apparent,
                "{context}"
            );
            assert_eq!(
                subtree.directory_allocated_bytes,
                Some(directory_allocated),
                "{context}"
            );
            assert_eq!(subtree.unreadable_directories, unreadable, "{context}");
            checked += 1;
        }
        assert!(
            checked > 5,
            "{id}: only {checked} directories were compared"
        );
    }
}

#[cfg(unix)]
#[test]
fn hard_links_are_counted_once_in_the_allocated_total_of_a_subtree_that_holds_them() {
    let scratch = Scratch::new();
    let master = master(&scratch, "identity-small");
    let oracle = oracle_of(&master.root);
    let links = RelPath::from_bytes("identity/links").expect("valid");
    let subtree = oracle
        .find(&links)
        .and_then(|entry| entry.subtree)
        .expect("links/ has totals");
    // Nine names, three files: names are counted in full, allocation once per file.
    assert_eq!(subtree.files, 9);
    let per_group: Vec<u64> = oracle
        .hard_links
        .iter()
        .map(|group| {
            let entry = oracle.find(&group.paths[0]).expect("a linked name");
            entry.allocated.expect("allocated")
        })
        .collect();
    let sizes: Vec<u64> = oracle
        .hard_links
        .iter()
        .map(|group| oracle.find(&group.paths[0]).expect("a linked name").size)
        .collect();
    assert_eq!(subtree.allocated_bytes, Some(per_group.iter().sum()));
    assert_eq!(
        subtree.apparent_bytes,
        sizes.iter().map(|size| size * 3).sum::<u64>()
    );
    assert_eq!(subtree.apparent_unique_bytes, sizes.iter().sum::<u64>());
}

fn validator() -> jsonschema::Validator {
    let schema: Value = serde_json::from_str(include_str!(
        "../../../../../docs/schemas/native-path.schema.json"
    ))
    .expect("the product's native-path schema is JSON");
    jsonschema::draft202012::options()
        .build(&schema)
        .expect("the native-path schema compiles")
}

#[test]
fn every_path_in_the_oracle_validates_against_the_products_native_path_schema() {
    let scratch = Scratch::new();
    let validator = validator();
    for id in ["hostile-small", "identity-small"] {
        let master = master(&scratch, id);
        let oracle = oracle_of(&master.root);
        let document = serde_json::to_value(&oracle).expect("the oracle serializes");
        let entries = document["entries"].as_array().expect("entries");
        assert_eq!(entries.len(), oracle.entries.len());
        for entry in entries {
            let errors: Vec<String> = validator
                .iter_errors(&entry["path"])
                .map(|error| error.to_string())
                .collect();
            assert!(
                errors.is_empty(),
                "{id}: {} is not a native path: {errors:?}",
                entry["path"]
            );
        }
        for group in document["hard_links"].as_array().expect("hard_links") {
            for path in group["paths"].as_array().expect("paths") {
                assert!(validator.is_valid(path), "{id}: {path}");
            }
        }
    }
}

#[cfg(unix)]
#[test]
fn oracle_paths_round_trip_losslessly_including_names_that_are_not_utf8() {
    let scratch = Scratch::new();
    let master = master(&scratch, "hostile-small");
    let oracle = oracle_of(&master.root);
    let text = serde_json::to_string(&oracle).expect("serialize");
    let back: Oracle = serde_json::from_str(&text).expect("deserialize");
    assert_eq!(back, oracle, "the JSON form loses nothing");

    // Where the file system allows a name that is not valid UTF-8, it is in the oracle and it
    // survived the round trip byte for byte.
    if master
        .marker
        .capabilities
        .supports(crate::fixture::Capability::InvalidUtf8Names)
    {
        let invalid: Vec<&OracleEntry> = back
            .descendants()
            .filter(|entry| std::str::from_utf8(entry.path.as_bytes()).is_err())
            .collect();
        assert!(
            !invalid.is_empty(),
            "the file system accepts invalid UTF-8 names"
        );
        assert!(
            invalid
                .iter()
                .any(|entry| entry.path.file_name() == Some(&b"bad-\xff-byte"[..]))
        );
    }
}

#[test]
fn the_oracle_document_has_a_stable_shape() {
    let scratch = Scratch::new();
    let master = master(&scratch, "identity-small");
    let oracle = oracle_of(&master.root);
    let document = serde_json::to_value(&oracle).expect("serialize");

    let keys = |value: &Value| -> Vec<String> {
        value
            .as_object()
            .expect("an object")
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect()
    };
    assert_eq!(
        keys(&document),
        [
            "document_kind",
            "entries",
            "hard_links",
            "platform",
            "schema_version"
        ]
    );
    assert_eq!(document["document_kind"], "harness-fixture-oracle");
    assert_eq!(document["schema_version"], 1);
    assert_eq!(
        keys(&document["platform"]),
        ["allocation", "identity", "os"]
    );

    let entries = document["entries"].as_array().expect("entries");
    let by_kind = |kind: &str| -> &Value {
        entries
            .iter()
            .find(|entry| entry["kind"] == kind && entry["path"]["data"] != "")
            .unwrap_or_else(|| panic!("no {kind} entry"))
    };
    let common = [
        "allocated",
        "device_boundary",
        "dev",
        "ino",
        "kind",
        "mode",
        "nlink",
        "path",
        "readable",
        "size",
    ];
    let with = |extra: &[&str]| -> Vec<String> {
        let mut all: Vec<String> = common
            .iter()
            .chain(extra)
            .map(|key| (*key).to_owned())
            .collect();
        all.sort();
        all
    };
    assert_eq!(keys(by_kind("file")), with(&[]));
    // Windows cannot create a symbolic link, so its fixture has none to show the shape of.
    if master.marker.capabilities.supports(Capability::Symlinks) {
        assert_eq!(keys(by_kind("symlink")), with(&["symlink_target"]));
    }
    assert_eq!(keys(by_kind("directory")), with(&["subtree"]));
    assert_eq!(
        keys(&by_kind("directory")["subtree"]),
        [
            "allocated_bytes",
            "apparent_bytes",
            "apparent_unique_bytes",
            "directories",
            "directory_allocated_bytes",
            "directory_apparent_bytes",
            "entries",
            "files",
            "others",
            "symlinks",
            "unreadable_directories",
        ]
    );
    // The root has the empty path and comes first.
    assert_eq!(entries[0]["path"]["data"], "");
    assert_eq!(entries[0]["kind"], "directory");
}
