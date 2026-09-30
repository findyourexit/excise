//! The oracle agrees with `du` on every class that can be built without privileges.
//!
//! `du` walks the tree with `lstat` and never follows a link, counts a directory's own blocks
//! and size as well as its contents' (the oracle keeps those facts per entry, so nothing is
//! hidden in an aggregate), and counts a file with several names once. The expected `du` values
//! are therefore computed from the raw per-entry facts of the oracle, every entry including the
//! root and every directory, with each `(dev, ino)` that has several names counted once. The
//! implementations differ in what an apparent size covers and in what they do with a directory
//! they cannot list, so each rule is pinned exactly for the `du` that is installed:
//!
//! | `du` | Command | Expected value |
//! |---|---|---|
//! | any | `du -sk` (allocated) | ceil(Σ `allocated` / 1024) KiB |
//! | GNU 9.2 and later | `du --apparent-size -sb` | Σ `size` of regular files and symbolic links, in bytes |
//! | GNU before 9.2 | `du --apparent-size -sb` | Σ `size` of every entry, in bytes |
//! | BSD (macOS) | `du -A -sk` | ceil(Σ ceil(`size` / 512) / 2) KiB over every entry: BSD rounds every entry up to a 512-byte block before it adds |
//!
//! GNU coreutils 9.2 stopped adding the `st_size` of directories and of every other file that is
//! not a regular file or a symbolic link ("`du --apparent` now counts apparent sizes only of
//! regular files and symbolic links", its NEWS). The tests do not read a version number: they run
//! `du` on a directory whose contents they know and see which rule it follows, see
//! [`gnu_counts_directory_sizes`].
//!
//! # Tolerances
//!
//! * **KiB rounding** is part of the expected values above; nothing else is tolerated.
//! * **Hard links** are counted once by `du` and by the oracle, so they agree exactly. `du -l`
//!   would count every name.
//! * **Directories `du` cannot read.** `du` prints a permission error and exits non-zero, but it
//!   still prints the total of what it could read, which is what the oracle also sees, so the exit
//!   status is not compared. The implementations differ on whether the unreadable directory's
//!   *own* size and blocks are added, and each is exact: GNU adds them (`du.c`: "even if this
//!   directory is unreadable ... do let its size contribute to the total"), and BSD `du` skips
//!   them (its error path `break`s before it counts). A directory reports 0 blocks on APFS but a
//!   whole block on ext4, so the difference shows in the allocated total only on Linux.
//! * **Paths beyond `PATH_MAX`.** BSD `du` cannot descend past the limit and reports less than the
//!   tree holds; a GNU `du` that opens directories by descriptor can. The oracle reaches the
//!   bottom either way, so for that class `du` may fall short, and never exceed.

use std::{collections::BTreeSet, ffi::OsStr, fs, path::Path, time::Duration};

use super::support::{Scratch, master, run_bounded, spec};
use crate::fixture::{FixtureCache, MaterializeOptions, NodeKind, Oracle};

const LIMIT: Duration = Duration::from_secs(60);

/// What an implementation of `du` adds up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rules {
    /// Whether the own size and blocks of a directory that `du` cannot list are added.
    counts_unreadable_directories: bool,
    /// Whether an apparent-size total includes directories, and every other entry that is not a
    /// regular file or a symbolic link.
    apparent_counts_directories: bool,
}

/// The installed `du`: which command measures apparent size, and the rules it follows.
#[derive(Debug, Clone, Copy)]
struct Flavor {
    gnu: bool,
    rules: Rules,
}

impl Flavor {
    /// The installed `du`, or `None` when there is none.
    fn detect() -> Option<Self> {
        let (success, output) = run_bounded("du", &[OsStr::new("--version")], LIMIT)?;
        Some(if success && output.contains("GNU") {
            Self {
                gnu: true,
                rules: Rules {
                    counts_unreadable_directories: true,
                    apparent_counts_directories: gnu_counts_directory_sizes(),
                },
            }
        } else {
            Self {
                gnu: false,
                rules: Rules {
                    counts_unreadable_directories: false,
                    apparent_counts_directories: true,
                },
            }
        })
    }
}

/// Whether this GNU `du --apparent-size` adds the size of directories, as releases before
/// coreutils 9.2 did. It runs `du` on a directory that holds a 10-byte file and an empty
/// subdirectory, and accepts only the two totals the two rules give.
fn gnu_counts_directory_sizes() -> bool {
    let scratch = tempfile::Builder::new()
        .prefix("excise-du-probe-")
        .tempdir()
        .expect("a directory for the probe");
    let root = scratch.path().join("root");
    fs::create_dir_all(root.join("sub")).expect("create the probe directories");
    fs::write(root.join("file"), [0_u8; 10]).expect("create the probe file");

    let directories: u64 = [root.clone(), root.join("sub")]
        .iter()
        .map(|directory| {
            fs::symlink_metadata(directory)
                .expect("stat a probe directory")
                .len()
        })
        .sum();
    let reported = du(&["--apparent-size", "-sb"], &root).expect("GNU du prints a total");
    if reported == 10 {
        return false;
    }
    assert_eq!(
        reported,
        10 + directories,
        "`du --apparent-size -sb` on the probe adds neither nothing nor the {directories} bytes \
         of its two directories to the 10-byte file"
    );
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Expected {
    allocated_kib: u64,
    apparent_bytes: u64,
    bsd_apparent_kib: u64,
}

/// The totals `du` reports for the tree under `rules`, computed from the raw facts.
fn expected(oracle: &Oracle, rules: Rules) -> Expected {
    let mut seen = BTreeSet::new();
    let (mut allocated, mut apparent, mut blocks) = (0_u64, 0_u64, 0_u64);
    for entry in &oracle.entries {
        if entry.kind == NodeKind::Directory
            && !entry.readable
            && !rules.counts_unreadable_directories
        {
            continue;
        }
        if entry.kind != NodeKind::Directory
            && let (Some(dev), Some(ino), Some(nlink)) = (entry.dev, entry.ino, entry.nlink)
            && nlink > 1
            && !seen.insert((dev, ino))
        {
            continue;
        }
        allocated += entry.allocated.expect("allocated on Unix");
        if rules.apparent_counts_directories
            || matches!(entry.kind, NodeKind::File | NodeKind::Symlink)
        {
            apparent += entry.size;
        }
        blocks += entry.size.div_ceil(512);
    }
    Expected {
        allocated_kib: allocated.div_ceil(1024),
        apparent_bytes: apparent,
        bsd_apparent_kib: blocks.div_ceil(2),
    }
}

/// The first number `du` printed, or `None` when `du` is not available.
fn du(args: &[&str], root: &Path) -> Option<u64> {
    let mut arguments: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
    arguments.push(root.as_os_str());
    let (_status, output) = run_bounded("du", &arguments, LIMIT)?;
    output.split_whitespace().next()?.parse().ok()
}

/// Compares the oracle of `root` with `du`.
fn compare_with_du(id: &str, root: &Path) {
    let Some(flavor) = Flavor::detect() else {
        eprintln!("skipped: `du` is not available");
        return;
    };
    let oracle = Oracle::collect(root).expect("the oracle walk should succeed");
    let want = expected(&oracle, flavor.rules);

    let allocated = du(&["-sk"], root).expect("`du -sk` prints a total");
    assert_eq!(
        allocated, want.allocated_kib,
        "{id}: `du -sk` (KiB) against the oracle, under {:?}",
        flavor.rules
    );

    if flavor.gnu {
        let apparent = du(&["--apparent-size", "-sb"], root).expect("GNU du prints a total");
        assert_eq!(
            apparent, want.apparent_bytes,
            "{id}: `du --apparent-size -sb` (bytes) against the oracle, under {:?}",
            flavor.rules
        );
    } else {
        let apparent = du(&["-A", "-sk"], root).expect("BSD du prints a total");
        assert_eq!(
            apparent, want.bsd_apparent_kib,
            "{id}: `du -A -sk` (KiB) against the oracle, under {:?}",
            flavor.rules
        );
    }
}

#[test]
fn the_oracle_matches_du_on_the_scale_classes() {
    let scratch = Scratch::new();
    for id in ["node-modules-2k", "wide-1k", "delete-folder"] {
        compare_with_du(id, &master(&scratch, id).root);
    }
}

#[test]
fn the_oracle_matches_du_on_the_identity_class() {
    // Hard links counted once, sparse files at their allocated size, links not followed.
    let scratch = Scratch::new();
    compare_with_du("identity-small", &master(&scratch, "identity-small").root);
}

#[test]
fn the_oracle_matches_du_on_the_hostile_class() {
    let scratch = Scratch::new();
    compare_with_du("hostile-small", &master(&scratch, "hostile-small").root);
}

#[test]
fn the_oracle_matches_du_across_every_class_that_du_can_walk() {
    // Everything but the deep chain, which BSD `du` cannot descend.
    let scratch = Scratch::new();
    let mut spec = spec("all-classes-small");
    spec.parts.retain(|part| part.kind() != "deep");
    let master = FixtureCache::at(scratch.join("cache"))
        .materialize(&spec, &MaterializeOptions::default())
        .expect("materialize");
    compare_with_du("all-classes-small without the deep chain", &master.root);
}

#[test]
fn du_reaches_a_tree_deeper_than_path_max_only_as_far_as_it_can_and_never_beyond() {
    let scratch = Scratch::new();
    let master = master(&scratch, "deep-past-path-max");
    let Some(flavor) = Flavor::detect() else {
        eprintln!("skipped: `du` is not available");
        return;
    };
    let oracle = Oracle::collect(&master.root).expect("the oracle walks it");
    let full = expected(&oracle, flavor.rules).allocated_kib;
    let reported = du(&["-sk"], &master.root).expect("du prints a total");
    eprintln!(
        "deep tree: du -sk = {reported} KiB, oracle = {full} KiB ({} du)",
        if flavor.gnu { "GNU" } else { "BSD" }
    );
    assert!(
        reported <= full,
        "du can fall short of the oracle where it cannot descend, never exceed it: {reported} vs {full}"
    );
}
