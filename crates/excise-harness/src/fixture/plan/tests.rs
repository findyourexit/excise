//! Plan tests: the manifest is deterministic, canonical, and consistent with the spec.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
};

use super::{ManifestEntry, Plan};
use crate::fixture::{
    GENERATOR_VERSION, NodeKind,
    caps::{Capabilities, Capability},
    path::RelPath,
    spec::{FixtureSpec, HostileFeature, MAX_PORTABLE_PATH_BYTES, Part, SpecError},
};

fn spec(id: &str) -> FixtureSpec {
    FixtureSpec::load_bundled(id).unwrap_or_else(|error| panic!("{id}: {error}"))
}

fn plan(id: &str) -> Plan {
    Plan::new(&spec(id)).unwrap_or_else(|error| panic!("{id}: {error}"))
}

fn count(plan: &Plan, kind: NodeKind) -> usize {
    plan.entries()
        .iter()
        .filter(|entry| entry.kind == kind)
        .count()
}

/// The specs small enough to expand in a unit test; the million-file spec is checked by
/// arithmetic only.
const EXPANDABLE: &[&str] = &[
    "all-classes-small",
    "deep-past-path-max",
    "delete-folder",
    "hostile-small",
    "identity-small",
    "mount-boundary",
    "node-modules-2k",
    "tiny-files-50k",
    "wide-1k",
];

#[test]
fn node_modules_2k_is_the_repro_shape() {
    let plan = plan("node-modules-2k");
    assert_eq!(plan.entries().len(), 2_389);
    assert_eq!(count(&plan, NodeKind::Directory), 341);
    assert_eq!(count(&plan, NodeKind::File), 2_048);

    // The deepest package holds exactly m0.js..m7.js of one byte each.
    let leaf = "node_modules/pkg3/pkg3/pkg3/pkg3";
    let files: Vec<&ManifestEntry> = plan
        .entries()
        .iter()
        .filter(|entry| {
            entry.kind == NodeKind::File && entry.path.parent_bytes() == leaf.as_bytes()
        })
        .collect();
    let mut names: Vec<String> = files
        .iter()
        .map(|entry| {
            String::from_utf8_lossy(entry.path.file_name().unwrap_or_default()).into_owned()
        })
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "m0.js", "m1.js", "m2.js", "m3.js", "m4.js", "m5.js", "m6.js", "m7.js"
        ]
    );
    assert!(files.iter().all(|entry| entry.size == 1));
    // Files live only in the leaves.
    assert!(
        plan.entries()
            .iter()
            .filter(|entry| entry.kind == NodeKind::File)
            .all(|entry| entry.path.depth() == 6)
    );
}

#[test]
fn every_spec_plans_exactly_the_entries_its_parameters_predict() {
    for id in EXPANDABLE {
        let plan = plan(id);
        assert_eq!(
            plan.entries().len() as u64,
            plan.spec().planned_entry_count(),
            "{id}: the plan and the spec's arithmetic disagree"
        );
    }
    // The large specs are validated and counted without being expanded.
    let million = spec("tiny-files-1m");
    assert_eq!(million.planned_entry_count(), 1_010_101);
    let ten_million = spec("tiny-files-10m");
    assert_eq!(ten_million.planned_entry_count(), 10_010_101);
    let quarter_million = spec("tiny-files-250k");
    assert_eq!(quarter_million.planned_entry_count(), 249_250);
    assert!(
        quarter_million.planned_entry_count() <= 250_000,
        "the 250k spec must fit the full tier's 250,000-entry cap"
    );
    let fifty = spec("tiny-files-50k");
    assert_eq!(fifty.planned_entry_count(), 49_050);
    assert!(
        fifty.planned_entry_count() <= 50_000,
        "the 50k spec must respect the disk cap"
    );
}

#[test]
fn a_spec_that_plans_more_entries_than_the_limit_is_refused() {
    // The weekly tier's spec with 1,010 files in each of its 10,000 leaf directories plans
    // 10,110,101 entries, which is past the limit that the largest shipped spec fits under.
    let text = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/tiny-files-10m.toml"),
    )
    .expect("the weekly spec ships")
    .replace("files_per_dir = 1000", "files_per_dir = 1010");
    let error = FixtureSpec::from_toml_str(&text).expect_err("loading refuses the spec");

    assert!(
        error.to_string().contains("the limit is") && error.to_string().contains("10110101"),
        "{error}"
    );
}

#[test]
fn entries_are_unique_sorted_and_rooted_in_planned_directories() {
    for id in EXPANDABLE {
        let plan = plan(id);
        let entries = plan.entries();
        assert!(
            entries.windows(2).all(|pair| pair[0].path < pair[1].path),
            "{id}: entries must be strictly increasing in canonical order"
        );
        let directories: BTreeSet<&RelPath> = entries
            .iter()
            .filter(|entry| entry.kind == NodeKind::Directory)
            .map(|entry| &entry.path)
            .collect();
        for entry in entries {
            assert!(!entry.path.is_root(), "{id}: the root is not an entry");
            if let Some(parent) = entry.path.parent() {
                assert!(
                    parent.is_root() || directories.contains(&parent),
                    "{id}: `{}` has no planned parent directory",
                    entry.path
                );
            }
        }
    }
}

#[test]
fn a_skipped_directory_never_leaves_planned_children_behind() {
    // If a directory needs a capability, everything below it needs one too, or the generator
    // would create the child and silently recreate the skipped directory around it.
    for id in EXPANDABLE {
        let plan = plan(id);
        let required: Vec<(&RelPath, Capability)> = plan
            .entries()
            .iter()
            .filter(|entry| entry.kind == NodeKind::Directory)
            .filter_map(|entry| entry.requires.map(|capability| (&entry.path, capability)))
            .collect();
        for entry in plan.entries() {
            for (directory, capability) in &required {
                if entry.path.starts_with(directory) && entry.path != **directory {
                    assert_eq!(
                        entry.requires,
                        Some(*capability),
                        "{id}: `{}` lies below `{directory}`, which needs {capability}",
                        entry.path
                    );
                }
            }
        }
    }
}

#[test]
fn the_same_spec_and_seed_give_the_same_manifest_and_another_seed_a_different_one() {
    let spec = spec("all-classes-small");
    let first = Plan::new(&spec).unwrap_or_else(|error| panic!("{error}"));
    let second = Plan::new(&spec).unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(first.manifest_sha256(), second.manifest_sha256());
    assert_eq!(first.entries(), second.entries());
    assert_eq!(first.spec_hash(), second.spec_hash());

    let other = Plan::new(&spec.with_seed(spec.seed + 1)).unwrap_or_else(|error| panic!("{error}"));
    assert_ne!(
        first.spec_hash(),
        other.spec_hash(),
        "the seed is part of the spec hash"
    );
    assert_ne!(
        first.manifest_sha256(),
        other.manifest_sha256(),
        "seed-driven names and sizes change"
    );
}

#[test]
fn a_seed_changes_only_what_the_seed_drives() {
    // Sequential names and fixed sizes do not depend on the seed: the manifest is the same, and
    // only the spec hash tells the two apart.
    let spec = spec("node-modules-2k");
    let one = Plan::new(&spec.with_seed(1)).unwrap_or_else(|error| panic!("{error}"));
    let two = Plan::new(&spec.with_seed(2)).unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(one.manifest_sha256(), two.manifest_sha256());
    assert_ne!(one.spec_hash(), two.spec_hash());
}

#[test]
fn parts_do_not_disturb_each_others_streams() {
    // Removing one part must not change what the others plan.
    let full = spec("all-classes-small");
    let mut without_wide = full.clone();
    without_wide.parts.retain(|part| part.root() != "wide");
    let with = Plan::new(&full).unwrap_or_else(|error| panic!("{error}"));
    let without = Plan::new(&without_wide).unwrap_or_else(|error| panic!("{error}"));
    let nested = |plan: &Plan| -> Vec<ManifestEntry> {
        plan.entries()
            .iter()
            .filter(|entry| entry.path.components().next() == Some(&b"nested"[..]))
            .cloned()
            .collect()
    };
    assert_eq!(nested(&with), nested(&without));
    assert!(without.entries().len() < with.entries().len());
}

#[test]
fn hard_link_groups_are_numbered_in_canonical_order_and_only_the_first_name_is_the_file() {
    let plan = plan("identity-small");
    let members: Vec<&ManifestEntry> = plan
        .entries()
        .iter()
        .filter(|entry| entry.link_group.is_some())
        .collect();
    assert_eq!(members.len(), 9, "3 groups of 3 names");

    let mut seen: Vec<u32> = Vec::new();
    for member in &members {
        let group = member.link_group.unwrap_or(0);
        if seen.contains(&group) {
            assert_eq!(
                member.requires,
                Some(Capability::HardLinks),
                "{}",
                member.path
            );
        } else {
            assert_eq!(
                member.requires, None,
                "the first name of a group is the file itself"
            );
            seen.push(group);
        }
    }
    assert_eq!(
        seen,
        [1, 2, 3],
        "groups are numbered from 1 in order of first appearance"
    );

    // Names of one group are spread across directories, and all have the same size.
    for group in 1..=3 {
        let of_group: Vec<&&ManifestEntry> = members
            .iter()
            .filter(|member| member.link_group == Some(group))
            .collect();
        let directories: BTreeSet<Vec<u8>> = of_group
            .iter()
            .map(|member| member.path.parent_bytes().to_vec())
            .collect();
        assert!(
            directories.len() >= 2,
            "group {group} must have names in different directories"
        );
        assert!(
            of_group
                .iter()
                .all(|member| member.size == of_group[0].size)
        );
    }
}

#[test]
fn the_realized_hash_differs_from_the_plan_hash_exactly_by_the_skipped_entries() {
    let plan = plan("identity-small");
    let everything = Capabilities::all_supported();
    assert_eq!(plan.realized_sha256(&everything), plan.manifest_sha256());
    assert_eq!(plan.realized(&everything).count(), plan.entries().len());

    let without_clones = everything.without(Capability::Clones, "test");
    let realized: Vec<&ManifestEntry> = plan.realized(&without_clones).collect();
    let skipped = plan.entries().len() - realized.len();
    // The `clones/` directory, the original, and two clones.
    assert_eq!(skipped, 4);
    assert!(
        realized
            .iter()
            .all(|entry| entry.requires != Some(Capability::Clones))
    );
    assert_ne!(
        plan.realized_sha256(&without_clones),
        plan.manifest_sha256()
    );

    // Without hard links only the extra names disappear, never the file itself.
    let without_links = Capabilities::all_supported().without(Capability::HardLinks, "test");
    assert_eq!(
        plan.entries().len() - plan.realized(&without_links).count(),
        6
    );
}

#[test]
fn colliding_names_are_reported_instead_of_planned_twice() {
    // A directory style and a file style that both produce `x0`.
    let text = r#"
        schema_version = 1
        id = "collide"
        description = "names collide"
        seed = 1

        [[parts]]
        kind = "tree"
        root = "t"
        depth = 1
        fanout = 2
        files_per_dir = 2
        dir_names = { style = "sequential", prefix = "x" }
        file_names = { style = "sequential", prefix = "x" }
    "#;
    let spec = FixtureSpec::from_toml_str(text).unwrap_or_else(|error| panic!("{error}"));
    match Plan::new(&spec) {
        Err(SpecError::Invalid { problems, .. }) => {
            assert!(problems.to_string().contains("planned twice"), "{problems}");
        }
        other => panic!("expected a collision error, got {other:?}"),
    }
}

#[test]
fn the_manifest_hash_encoding_is_pinned() {
    // The hash is an interface: a cached fixture is keyed by it, and machines compare it. It runs
    // on every operating system, so a pass everywhere is the cross-machine determinism proof. If
    // this fails, the generator's output changed: bump `GENERATOR_VERSION` and update the values.
    //
    // The first value was also computed by an independent implementation of the encoding
    // documented on the module, written in another language.
    assert_eq!(
        plan("node-modules-2k").manifest_sha256(),
        "3215c2c1f155bd1a4f6c250b219ad0aba9df4bb1071c402dc773d2184460d698"
    );
    // Covers every part kind, hex names, and seed-drawn sizes: the PRNG and the permutation.
    assert_eq!(
        plan("all-classes-small").manifest_sha256(),
        "4251871f4822a5b78e9f68f8bc3d7a68662bf98ec015f049834eecea0c96d2df"
    );
}

/// Two identity parts, each with groups of hard links of its own. The groups of a plan are
/// numbered once for the whole plan, so that the groups of two parts are never taken for one
/// (two parts that each numbered theirs from 1 would have made the first group of each one
/// group, of the names of both): a spec with two identity parts therefore plans another
/// manifest than the generator did before it, under the same spec hash, and the version of the
/// generator is what keeps a cached tree of one from being taken for the other.
#[test]
fn the_groups_of_two_identity_parts_are_numbered_apart_and_the_generator_version_says_so() {
    let text = r#"
        schema_version = 1
        id = "two-identities"
        description = "Two identity parts, each with groups of hard links."
        seed = 3

        [[parts]]
        kind = "identity"
        root = "first"
        hard_link_groups = 2
        links_per_group = 2

        [[parts]]
        kind = "identity"
        root = "second"
        hard_link_groups = 2
        links_per_group = 3
    "#;
    let plan = Plan::new(&FixtureSpec::from_toml_str(text).expect("a spec")).expect("a plan");

    let mut names_in_group: BTreeMap<u32, usize> = BTreeMap::new();
    for group in plan.entries().iter().filter_map(|entry| entry.link_group) {
        *names_in_group.entry(group).or_default() += 1;
    }
    assert_eq!(
        names_in_group,
        BTreeMap::from([(1, 2), (2, 2), (3, 3), (4, 3)]),
        "four groups, two of two names and two of three"
    );
    assert_eq!(
        GENERATOR_VERSION, 2,
        "the plan-wide numbering of groups is the output of version 2: a cached tree of version 1 \
         is never reused"
    );
    // The numbering is part of the manifest, and so of its hash: a change to it changes this, and
    // that is a change of the generator's output, which is a new `GENERATOR_VERSION`.
    assert_eq!(
        plan.manifest_sha256(),
        "5b7b27c70569dd1623d55cd229ffaf291c962032d79cd5319a71a60cf9d41118"
    );
}

// ---------------------------------------------------------------------------------------------
// The shaped part.

/// A shaped part with three depths, links of both kinds, and a histogram of every kind.
const SHAPED: &str = r#"
    schema_version = 1
    id = "shaped-plan"
    description = "A shaped tree."
    seed = 4

    [[parts]]
    kind = "shaped"
    root = "home"
    max_file_bytes = 100
    dangling_symlinks = 2

    [[parts.levels]]
    directories = 3
    files = 4

    [[parts.levels]]
    directories = 2
    files = 20
    symlinks = 5

    [[parts.levels]]
    files = 30
    symlinks = 1

    [parts.subdirectories_per_directory]
    0 = 2
    1 = 2
    2 = 1

    [parts.files_per_directory]
    0 = 1
    1 = 2
    4 = 2
    16 = 1

    [parts.file_sizes]
    0 = 1
    1 = 2
    64 = 4
    4096 = 1

    [parts.directory_name_lengths]
    4 = 1
    9 = 1

    [parts.file_name_lengths]
    6 = 3
    11 = 1

    [parts.symlink_name_lengths]
    7 = 1

    [[parts.hard_links]]
    class = 1
    names = 2
    [parts.hard_links.group_sizes]
    2 = 1

    [[parts.hard_links]]
    class = 64
    names = 9
    [parts.hard_links.group_sizes]
    2 = 2
    4 = 1
"#;

fn shaped_spec() -> FixtureSpec {
    FixtureSpec::from_toml_str(SHAPED).unwrap_or_else(|error| panic!("{error}"))
}

/// The entries of `kind` at `depth`, counting the fixture root's child `home` as depth 1.
fn at_depth(plan: &Plan, kind: NodeKind, depth: usize) -> Vec<&ManifestEntry> {
    plan.entries()
        .iter()
        .filter(|entry| entry.kind == kind && entry.path.depth() == depth)
        .collect()
}

#[test]
fn a_shaped_part_plans_exactly_the_counts_of_its_levels() {
    let spec = shaped_spec();
    let plan = Plan::new(&spec).expect("a plan");

    assert_eq!(spec.planned_entry_count(), 1 + 5 + 54 + 6);
    assert_eq!(plan.entries().len() as u64, spec.planned_entry_count());
    assert_eq!(at_depth(&plan, NodeKind::Directory, 1).len(), 1, "the root");
    for (level, (directories, files, symlinks)) in
        [(3, 4, 0), (2, 20, 5), (0, 30, 1)].into_iter().enumerate()
    {
        let depth = level + 2;
        let found = |kind| at_depth(&plan, kind, depth).len();
        assert_eq!(
            found(NodeKind::Directory),
            directories,
            "directories at {depth}"
        );
        assert_eq!(found(NodeKind::File), files, "files at {depth}");
        assert_eq!(found(NodeKind::Symlink), symlinks, "links at {depth}");
    }
    // Everything is in a directory the plan has, and nothing is deeper than the levels.
    let directories: BTreeSet<&RelPath> = plan
        .entries()
        .iter()
        .filter(|entry| entry.kind == NodeKind::Directory)
        .map(|entry| &entry.path)
        .collect();
    for entry in plan.entries().iter().skip(1) {
        let parent = entry.path.parent().expect("a parent");
        assert!(
            directories.contains(&parent),
            "{} has no parent",
            entry.path
        );
        assert!(entry.path.depth() <= 4);
    }
}

#[test]
fn a_shaped_part_names_every_entry_distinctly_in_hexadecimal_and_draws_sizes_within_the_cap() {
    let plan = Plan::new(&shaped_spec()).expect("a plan");

    let mut sizes = Vec::new();
    for entry in plan.entries().iter().skip(1) {
        let name = entry.path.file_name().expect("a name");
        assert!(
            name.iter()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte)),
            "{} is not hexadecimal",
            entry.path
        );
        assert!(name.len() <= 255);
        match entry.kind {
            NodeKind::File => sizes.push(entry.size),
            NodeKind::Symlink => assert_eq!(entry.requires, Some(Capability::Symlinks)),
            _ => {}
        }
    }
    assert!(
        sizes.iter().all(|size| *size <= 100),
        "no file is above max_file_bytes"
    );
    assert!(
        sizes.contains(&0) && sizes.contains(&100),
        "the smallest and the cap are both drawn"
    );
    // Directory names are 4 or 9 bytes long and file names 6 or 11, unless a directory has so
    // many children that it needs more digits to tell them apart.
    let lengths = |kind| -> BTreeSet<usize> {
        plan.entries()
            .iter()
            .filter(|entry| entry.kind == kind && entry.path.depth() > 1)
            .map(|entry| entry.path.file_name().expect("a name").len())
            .collect()
    };
    assert_eq!(lengths(NodeKind::Directory), BTreeSet::from([4, 9]));
    assert_eq!(lengths(NodeKind::File), BTreeSet::from([6, 11]));
    assert_eq!(lengths(NodeKind::Symlink), BTreeSet::from([7]));
}

#[test]
fn a_shaped_part_spreads_entries_unevenly_over_directories_as_its_histogram_says() {
    let plan = Plan::new(&shaped_spec()).expect("a plan");

    // The 20 files of depth 3 go to the 3 directories of depth 2 in proportion to weights drawn
    // from {0, 1, 1, 4, 4, 16}: a spread that is not even.
    let mut per_directory: BTreeMap<Vec<u8>, usize> = BTreeMap::new();
    for entry in at_depth(&plan, NodeKind::File, 3) {
        *per_directory
            .entry(entry.path.parent_bytes().to_vec())
            .or_default() += 1;
    }
    let counts: Vec<usize> = per_directory.values().copied().collect();
    assert_eq!(counts.iter().sum::<usize>(), 20);
    assert!(
        counts.iter().max() != counts.iter().min() || counts.len() == 1,
        "the spread follows weights that are not equal: {counts:?}"
    );
}

#[test]
fn a_shaped_part_makes_groups_of_hard_links_and_links_that_resolve_or_dangle_as_asked() {
    let plan = Plan::new(&shaped_spec()).expect("a plan");

    // Four groups, each of at least two names and all of one size; the first name in canonical
    // order is the file and the others need hard links.
    let mut groups: BTreeMap<u32, Vec<&ManifestEntry>> = BTreeMap::new();
    for entry in plan
        .entries()
        .iter()
        .filter(|entry| entry.link_group.is_some())
    {
        groups
            .entry(entry.link_group.expect("a group"))
            .or_default()
            .push(entry);
    }
    assert_eq!(groups.keys().copied().collect::<Vec<_>>(), [1, 2, 3, 4]);
    let mut names = 0;
    for (group, members) in &groups {
        assert!(
            members.len() >= 2,
            "group {group} has {} names",
            members.len()
        );
        assert!(members.iter().all(|member| member.size == members[0].size));
        assert_eq!(members[0].requires, None);
        assert!(
            members[1..]
                .iter()
                .all(|member| member.requires == Some(Capability::HardLinks))
        );
        names += members.len();
    }
    assert_eq!(
        names, 11,
        "a group of 2 names among the files of 1 byte, and groups of 2, 2, and 5 names among \
         the files of 64 to 100 bytes"
    );
    let mut by_size: Vec<(u64, usize)> = groups
        .values()
        .map(|members| (members[0].size, members.len()))
        .collect();
    by_size.sort_unstable();
    assert_eq!(by_size[0], (1, 2), "the class of 1 byte");
    assert!(
        by_size[1..]
            .iter()
            .all(|(size, _)| (64..=100).contains(size))
    );
    let mut counts: Vec<usize> = by_size[1..].iter().map(|(_, names)| *names).collect();
    counts.sort_unstable();
    assert_eq!(counts, [2, 2, 5], "the class of 64 to 127, cut at 100");

    // Two links dangle, and the four others resolve to a file of the part, which the target says
    // by a relative path.
    let files: BTreeSet<&RelPath> = plan
        .entries()
        .iter()
        .filter(|entry| entry.kind == NodeKind::File)
        .map(|entry| &entry.path)
        .collect();
    let mut dangling = 0;
    for link in plan
        .entries()
        .iter()
        .filter(|entry| entry.kind == NodeKind::Symlink)
    {
        let target = link.target.as_ref().expect("a target").as_bytes();
        assert_eq!(link.size, target.len() as u64);
        let Some(path) = target_path(link) else {
            dangling += 1;
            continue;
        };
        assert!(
            files.contains(&path),
            "{} points at {path}, which is no file",
            link.path
        );
    }
    assert_eq!(dangling, 2);
}

/// Where a link of the plan points, as a path from the root of the fixture: its target read from
/// the folder that holds it, as a file system reads it. `None` for the target `missing`, which
/// points at nothing.
fn target_path(link: &ManifestEntry) -> Option<RelPath> {
    let target = link.target.as_ref().expect("a target").as_bytes();
    if target == b"missing" {
        return None;
    }
    let mut resolved: Vec<&[u8]> = link.path.components().collect();
    resolved.pop();
    for part in target.split(|byte| *byte == b'/') {
        if part == b".." {
            resolved.pop();
        } else {
            resolved.push(part);
        }
    }
    Some(RelPath::from_bytes(resolved.join(&b'/')).expect("a path"))
}

/// A shaped part in which every file has more than one name, and every link points at a file.
const ALL_GROUPED: &str = r#"
    schema_version = 1
    id = "shaped-all-grouped"
    description = "A shaped tree in which no file has one name only."
    seed = 1

    [[parts]]
    kind = "shaped"
    root = "home"
    max_file_bytes = 100

    [[parts.levels]]
    directories = 2
    files = 2

    [[parts.levels]]
    directories = 1
    files = 6
    symlinks = 4

    [[parts.levels]]
    files = 4
    symlinks = 2

    [parts.subdirectories_per_directory]
    0 = 1
    1 = 1
    2 = 1

    [parts.files_per_directory]
    0 = 1
    1 = 1
    4 = 1

    [parts.file_sizes]
    8 = 1

    [parts.directory_name_lengths]
    4 = 1

    [parts.file_name_lengths]
    6 = 1

    [parts.symlink_name_lengths]
    5 = 1

    [[parts.hard_links]]
    class = 8
    names = 12
    [parts.hard_links.group_sizes]
    2 = 2
    4 = 1
"#;

/// Where hard links cannot be made, every name of a group but its first, in canonical order, is
/// not created, and a link that pointed at one of those would point at nothing. When every file
/// has more than one name there is no file that is no group's to point at, so a link points at the
/// first name of a group: the one that is the file, wherever it stands and whatever the seed.
#[test]
fn a_link_never_points_at_a_name_that_needs_hard_links() {
    let spec = FixtureSpec::from_toml_str(ALL_GROUPED).expect("a spec");
    let without_hard_links = Capabilities::all_supported().without(Capability::HardLinks, "test");

    for seed in 0..40 {
        let plan = Plan::new(&spec.with_seed(seed)).expect("a plan");
        assert!(
            plan.entries()
                .iter()
                .filter(|entry| entry.kind == NodeKind::File)
                .all(|entry| entry.link_group.is_some()),
            "every file has more than one name"
        );
        let files: BTreeSet<&RelPath> = plan
            .realized(&without_hard_links)
            .filter(|entry| entry.kind == NodeKind::File)
            .map(|entry| &entry.path)
            .collect();
        let mut links = 0;
        for link in plan
            .realized(&without_hard_links)
            .filter(|entry| entry.kind == NodeKind::Symlink)
        {
            links += 1;
            let path = target_path(link).expect("every link of the part points at a file");
            assert!(
                files.contains(&path),
                "seed {seed}: {} points at {path}, which is not there where hard links cannot be \
                 made",
                link.path
            );
        }
        assert_eq!(links, 6, "seed {seed}");
    }
}

/// The same on a file system: generated where hard links cannot be made, the part has every link
/// it was asked for, and each resolves to a file.
#[cfg(unix)]
#[test]
fn the_links_of_a_tree_generated_without_hard_links_all_resolve() {
    use crate::fixture::{GenerateOptions, generate};

    let spec = FixtureSpec::from_toml_str(ALL_GROUPED).expect("a spec");
    let without_hard_links = Capabilities::all_supported().without(Capability::HardLinks, "test");

    for seed in 0..8 {
        let scratch = tempfile::tempdir().expect("a directory");
        // The generator makes the root itself, and refuses one that exists.
        let root = scratch.path().join("fixture");
        let plan = Plan::new(&spec.with_seed(seed)).expect("a plan");
        generate(
            &plan,
            &root,
            &without_hard_links,
            GenerateOptions::default(),
        )
        .expect("the tree is generated");

        let mut links = 0;
        for link in plan
            .entries()
            .iter()
            .filter(|entry| entry.kind == NodeKind::Symlink)
        {
            links += 1;
            if let Err(error) = std::fs::metadata(link.path.to_path_buf(&root)) {
                panic!("seed {seed}: {} does not resolve: {error}", link.path);
            }
        }
        assert_eq!(links, 6, "seed {seed}");
    }
}

/// A shaped part that is a chain of `folders` folders with names of `folder_name` bytes, and one
/// file at the bottom with a name of `file_name` bytes: the longest path it plans is
/// `4 + folders * (folder_name + 1) + 1 + file_name` bytes, `home` being the root.
fn chain(folders: u32, folder_name: u32, file_name: u32) -> FixtureSpec {
    let mut text = String::from(
        "schema_version = 1\nid = \"chain\"\ndescription = \"A chain.\"\nseed = 3\n\n\
         [[parts]]\nkind = \"shaped\"\nroot = \"home\"\nmax_file_bytes = 100\n",
    );
    for _ in 0..folders {
        text.push_str("\n[[parts.levels]]\ndirectories = 1\n");
    }
    text.push_str("\n[[parts.levels]]\nfiles = 1\n");
    write!(
        text,
        "\n[parts.subdirectories_per_directory]\n1 = 1\n\n\
         [parts.files_per_directory]\n1 = 1\n\n\
         [parts.file_sizes]\n1 = 1\n\n\
         [parts.directory_name_lengths]\n{folder_name} = 1\n\n\
         [parts.file_name_lengths]\n{file_name} = 1\n"
    )
    .expect("text");
    FixtureSpec::from_toml_str(&text).unwrap_or_else(|error| panic!("{error}"))
}

/// A shaped part with a few folders whose names are long among many that are short.
const LONG_NAMES_ARE_RARE: &str = r#"
    schema_version = 1
    id = "shaped-rare-long-names"
    description = "A shaped tree in which a tenth of the folders have names of 255 bytes."
    seed = 6

    [[parts]]
    kind = "shaped"
    root = "home"
    max_file_bytes = 100

    [[parts.levels]]
    directories = 6
    files = 4

    [[parts.levels]]
    directories = 6
    files = 30
    symlinks = 3

    [[parts.levels]]
    files = 40
    symlinks = 2

    [parts.subdirectories_per_directory]
    0 = 1
    1 = 2
    2 = 1

    [parts.files_per_directory]
    0 = 1
    4 = 3
    16 = 1

    [parts.file_sizes]
    1 = 1
    64 = 1

    [parts.directory_name_lengths]
    4 = 90
    255 = 10

    [parts.file_name_lengths]
    6 = 3
    40 = 1

    [parts.symlink_name_lengths]
    9 = 1
"#;

/// The longest path a shaped part can plan, worked out from its fields, is never shorter than the
/// longest path of any plan of it, whatever the seed; and where nothing is left to the seed, a
/// chain, it is the longest path exactly.
#[test]
fn the_longest_path_a_shaped_part_can_plan_is_never_below_one_the_plan_has() {
    let longest_of = |spec: &FixtureSpec| -> u64 {
        Plan::new(spec)
            .expect("a plan")
            .entries()
            .iter()
            .map(|entry| entry.path.as_bytes().len() as u64)
            .max()
            .expect("entries")
    };
    let bound_of = |spec: &FixtureSpec| -> u64 {
        let Part::Shaped(part) = &spec.parts[0] else {
            panic!("a shaped part");
        };
        part.longest_path_bytes()
    };

    for (label, spec) in [
        ("the shaped example", shaped_spec()),
        (
            "every file has more than one name",
            FixtureSpec::from_toml_str(ALL_GROUPED).expect("a spec"),
        ),
        (
            "long names are rare",
            FixtureSpec::from_toml_str(LONG_NAMES_ARE_RARE).expect("a spec"),
        ),
        ("a chain of 255-byte names", chain(31, 255, 255)),
    ] {
        let bound = bound_of(&spec);
        for seed in 0..12 {
            let longest = longest_of(&spec.with_seed(seed));
            assert!(
                longest <= bound,
                "{label}, seed {seed}: a path of {longest} bytes, and the bound is {bound}"
            );
        }
    }

    // A chain leaves nothing to the seed, so the bound is the longest path itself.
    for (folders, folder_name, file_name) in
        [(1, 4, 6), (4, 100, 103), (5, 100, 100), (31, 255, 255)]
    {
        let spec = chain(folders, folder_name, file_name);
        let expected =
            4 + u64::from(folders) * u64::from(folder_name + 1) + 1 + u64::from(file_name);
        assert_eq!(
            longest_of(&spec),
            expected,
            "{folders} folders of {folder_name}"
        );
        assert_eq!(
            bound_of(&spec),
            expected,
            "{folders} folders of {folder_name}"
        );
    }
}

/// A shaped part is removable by a path-based removal, and so cacheable, when its root, a
/// separator, and the longest path it can plan fit in `PATH_MAX` together, and not when they are
/// one byte more; the longer the root, the less room; and a part whose long names are rare is
/// judged by the longest chain they can make, not by the longest name alone.
#[test]
fn a_shaped_part_is_removable_by_path_exactly_when_its_longest_path_fits_below_its_root() {
    // `PATH_MAX` is 1,024 on macOS and counts the terminating NUL: 1,023 bytes of path.
    assert_eq!(MAX_PORTABLE_PATH_BYTES, 1_023);

    // Four folders of 100 bytes and a file of 103: 4 + 4 * 101 + 1 + 103 = 512 bytes. A root of
    // 510, a separator, and that make 1,023: the limit exactly.
    assert!(chain(4, 100, 103).removable_by_path(510), "1,023 bytes");
    assert!(!chain(4, 100, 103).removable_by_path(511), "1,024 bytes");
    // One byte more in the name of the file, and one byte less of room for the root.
    assert!(chain(4, 100, 104).removable_by_path(509), "1,023 bytes");
    assert!(!chain(4, 100, 104).removable_by_path(510), "1,024 bytes");
    // The room a root of 100 bytes leaves, and one of nothing.
    assert!(chain(5, 50, 50).removable_by_path(100), "100 + 1 + 310");
    assert!(chain(4, 100, 103).removable_by_path(0), "1 + 512");
    assert!(shaped_spec().removable_by_path(100));
    // A chain of names of 255 bytes is far past the limit, however short the root.
    assert!(
        !chain(31, 255, 255).removable_by_path(0),
        "over 8,000 bytes"
    );
    // A root that leaves no room refuses even the shortest path.
    assert!(!shaped_spec().removable_by_path(MAX_PORTABLE_PATH_BYTES));

    // A tenth of the folders have names of 255 bytes: one of the twelve. A path passes through two
    // folders and ends in one more entry, so the longest it can be is the long folder, a short one
    // (4 bytes), and the longest file (40 bytes), and not three folders of 255.
    let rare = FixtureSpec::from_toml_str(LONG_NAMES_ARE_RARE).expect("a spec");
    let Part::Shaped(part) = &rare.parts[0] else {
        panic!("a shaped part");
    };
    assert_eq!(
        part.longest_path_bytes(),
        4 + (1 + 255) + (1 + 4) + (1 + 40)
    );
    assert!(rare.removable_by_path(100));
    assert!(rare.removable_by_path(1_023 - 1 - part.longest_path_bytes()));
    assert!(!rare.removable_by_path(1_023 - part.longest_path_bytes()));
}

/// A spec with one part of `kind`, whose fields are the TOML lines `fields`.
fn one_part(kind: &str, fields: &str) -> FixtureSpec {
    FixtureSpec::from_toml_str(&format!(
        "schema_version = 1\nid = \"one-part\"\ndescription = \"One part.\"\nseed = 7\n\n\
         [[parts]]\nkind = \"{kind}\"\n{fields}"
    ))
    .unwrap_or_else(|error| panic!("{kind} {fields}: {error}"))
}

/// Specs at the edges of what each kind of part can plan, each with a name.
#[allow(clippy::too_many_lines)]
fn edge_specs() -> Vec<(String, FixtureSpec)> {
    let literal = |letter: &str, count: usize| {
        format!(
            "{{ style = \"literal\", name = \"{}\" }}",
            letter.repeat(count)
        )
    };
    let sparse = ["{ apparent_mib = 1 }"; 11].join(", ");
    let mut specs = vec![
        (
            "a wide folder under a root of 255 bytes",
            one_part(
                "tree",
                &format!(
                    "root = \"{}\"\ndepth = 0\nfiles_per_dir = 12\n",
                    "r".repeat(255)
                ),
            ),
        ),
        (
            "folders and files whose names need more digits",
            one_part(
                "tree",
                "root = \"t\"\ndepth = 2\nfanout = 11\nfiles_per_dir = 101\n\
                 file_placement = \"leaves\"\n",
            ),
        ),
        (
            "padded names with a prefix and a suffix",
            one_part(
                "tree",
                "root = \"t\"\ndepth = 3\nfanout = 3\nfiles_per_dir = 2\n\
                 dir_names = { style = \"sequential\", prefix = \"dir-\", suffix = \".d\", width = 6 }\n\
                 file_names = { style = \"sequential\", prefix = \"file-\", suffix = \".txt\", width = 3 }\n",
            ),
        ),
        (
            "hex names",
            one_part(
                "tree",
                "root = \"t\"\ndepth = 2\nfanout = 4\nfiles_per_dir = 5\n\
                 dir_names = { style = \"hex\", prefix = \"h-\", length = 5 }\n\
                 file_names = { style = \"hex\", suffix = \".bin\", length = 3 }\n",
            ),
        ),
        (
            "four folders named with 255 bytes",
            one_part(
                "tree",
                &format!(
                    "root = \"tree\"\ndepth = 4\nfanout = 1\ndir_names = {}\n",
                    literal("n", 255)
                ),
            ),
        ),
        (
            "four folders of 255 bytes and a file of 200",
            one_part(
                "tree",
                &format!(
                    "root = \"tree\"\ndepth = 4\nfanout = 1\nfiles_per_dir = 1\n\
                     file_placement = \"leaves\"\ndir_names = {}\nfile_names = {}\n",
                    literal("n", 255),
                    literal("m", 200)
                ),
            ),
        ),
        (
            "a file with a root of 255 bytes",
            one_part("file", &format!("root = \"{}\"\n", "f".repeat(255))),
        ),
        (
            "a file with a root of one byte",
            one_part("file", "root = \"x\"\n"),
        ),
        (
            "a volume with a root of 255 bytes",
            one_part(
                "volume",
                &format!("root = \"{}\"\nsize_mib = 8\n", "v".repeat(255)),
            ),
        ),
        (
            "a deep chain with files",
            one_part(
                "deep",
                "root = \"deep\"\ndepth = 3\nname_len = 255\nfiles_per_level = 12\n",
            ),
        ),
        (
            "the deepest chain",
            one_part("deep", "root = \"d\"\ndepth = 512\nname_len = 255\n"),
        ),
        (
            "every identity feature, links spread over 3 folders",
            one_part(
                "identity",
                &format!(
                    "root = \"id\"\nhard_link_groups = 1234\nlinks_per_group = 11\nlink_spread = 3\n\
                     dangling_symlinks = 1001\nvalid_symlinks = true\n\
                     symlink_loops = [\"self\", \"pair\", \"directory\"]\n\
                     sparse_files = [{sparse}]\nclones = 101\n"
                ),
            ),
        ),
        (
            "hard links spread over 11 folders",
            one_part(
                "identity",
                "root = \"id\"\nhard_link_groups = 30\nlinks_per_group = 11\nlink_spread = 11\n",
            ),
        ),
        (
            "an identity part with nothing but its root",
            one_part("identity", &format!("root = \"{}\"\n", "i".repeat(255))),
        ),
    ];
    for (label, fields) in [
        (
            "hard links",
            "hard_link_groups = 2\nlinks_per_group = 2\nlink_spread = 1\n",
        ),
        ("dangling links", "dangling_symlinks = 12\n"),
        ("working links", "valid_symlinks = true\n"),
        ("a self loop", "symlink_loops = [\"self\"]\n"),
        ("a pair loop", "symlink_loops = [\"pair\"]\n"),
        ("a directory loop", "symlink_loops = [\"directory\"]\n"),
        (
            "sparse files",
            "sparse_files = [{ apparent_mib = 1 }, { apparent_mib = 2 }]\n",
        ),
        ("clones", "clones = 2\n"),
    ] {
        specs.push((
            label,
            one_part("identity", &format!("root = \"identity\"\n{fields}")),
        ));
    }
    let mut specs: Vec<(String, FixtureSpec)> = specs
        .into_iter()
        .map(|(label, spec)| (label.to_owned(), spec))
        .collect();
    let features = [
        "control_names",
        "bidi_names",
        "escape_names",
        "newline_names",
        "invalid_utf8_names",
        "long_names",
        "unreadable_dirs",
        "unreadable_files",
        "unwritable_dirs",
    ];
    for feature in features {
        specs.push((
            format!("the hostile feature {feature}"),
            one_part(
                "hostile",
                &format!("root = \"hostile\"\nfeatures = [\"{feature}\"]\n"),
            ),
        ));
    }
    specs.push((
        "every hostile feature under a root of 255 bytes".to_owned(),
        one_part(
            "hostile",
            &format!(
                "root = \"{}\"\nfeatures = [\"{}\"]\n",
                "h".repeat(255),
                features.join("\", \"")
            ),
        ),
    ));
    specs
}

/// The longest path in bytes among the entries of the part whose root is `root`.
fn longest_planned(plan: &Plan, root: &str) -> u64 {
    plan.entries()
        .iter()
        .filter(|entry| {
            let path = entry.path.as_bytes();
            path.starts_with(root.as_bytes()) && matches!(path.get(root.len()), None | Some(b'/'))
        })
        .map(|entry| u64::try_from(entry.path.as_bytes().len()).expect("a length"))
        .max()
        .expect("a part plans its root")
}

/// The longest path a part can plan, worked out from its fields, is never shorter than the longest
/// path of any plan of it, whatever the seed, for every kind of part, over the bundled specs and
/// specs at the edges of each kind. It is the longest path exactly for every kind but `shaped`,
/// and `identity` when the groups of hard links can place their longest names in folders whose
/// names are shorter than the longest folder name.
#[test]
fn the_longest_path_every_part_kind_can_plan_is_never_below_one_the_plan_has() {
    let mut specs: Vec<(String, FixtureSpec)> = EXPANDABLE
        .iter()
        .chain(&["node-modules-50k", "delete-folder-65k"])
        .map(|id| ((*id).to_owned(), spec(id)))
        .collect();
    specs.extend(edge_specs());
    specs.push(("the shaped example".to_owned(), shaped_spec()));

    for (label, spec) in &specs {
        // The seed moves names and sizes, and no length that a spec of one of these kinds fixes.
        let seeds = if spec.planned_entry_count() <= 20_000 {
            2
        } else {
            1
        };
        for seed in 0..seeds {
            let spec = spec.with_seed(spec.seed + seed);
            let plan = Plan::new(&spec).unwrap_or_else(|error| panic!("{label}: {error}"));
            for part in &spec.parts {
                let kind = part.kind();
                let longest = longest_planned(&plan, part.root());
                let bound = part.longest_path_bytes();
                assert!(
                    longest <= bound,
                    "{label}, seed {seed}, the {kind} part: a path of {longest} bytes, and the bound is {bound}"
                );
                match part {
                    Part::Shaped(_) => {}
                    Part::Identity(_) if label.contains("11 folders") => {
                        assert!(bound <= longest + 1, "{label}: {longest} and {bound} bytes");
                    }
                    _ => assert_eq!(
                        bound, longest,
                        "{label}, seed {seed}, the {kind} part: the bound is the longest path"
                    ),
                }
            }
            // The spec's bound is the longest of its parts, or the marker of the fixture root.
            let parts = spec
                .parts
                .iter()
                .map(Part::longest_path_bytes)
                .max()
                .expect("a part");
            assert_eq!(spec.longest_path_bytes(), parts.max(25), "{label}");
        }
    }
}

/// Every kind of part is removable by a path-based removal exactly when the root, a separator,
/// and its longest path fit in `PATH_MAX` together, and not when they are a byte more; the marker
/// the cache writes at the fixture root, under a name of 25 bytes before its own of 21, is a path
/// too; `deep` parts and the hostile features that defeat a removal are never removable.
#[test]
fn every_part_kind_is_removable_by_path_exactly_when_its_longest_path_fits_below_its_root() {
    // Four folders named with 255 bytes: 4 + 4 * 256 = 1,028 bytes, past the limit below any
    // root, though no other kind of part has a name or a level to count it with.
    let tree = one_part(
        "tree",
        &format!(
            "root = \"tree\"\ndepth = 4\nfanout = 1\n\
             dir_names = {{ style = \"literal\", name = \"{}\" }}\n",
            "n".repeat(255)
        ),
    );
    assert_eq!(tree.longest_path_bytes(), 1_028);
    assert!(!tree.removable_by_path(0));

    // A part with one name of one byte still has the marker, which the cache writes under a name
    // of 25 bytes (`.excise-harness-owned.tmp`) and renames to its own, of 21.
    assert_eq!(crate::fixture::marker::TEMPORARY_NAME.len(), 25);
    assert_eq!(crate::fixture::MARKER_FILE_NAME.len(), 21);
    let small = one_part("file", "root = \"x\"\n");
    assert_eq!(small.longest_path_bytes(), 25);
    assert!(small.removable_by_path(1_023 - 1 - 25));
    assert!(!small.removable_by_path(1_023 - 25));

    for (label, spec) in edge_specs() {
        let defeated = spec.parts.iter().any(|part| match part {
            Part::Deep(_) => true,
            Part::Hostile(hostile) => hostile.features.iter().any(|feature| {
                matches!(
                    feature,
                    HostileFeature::UnreadableDirs | HostileFeature::UnwritableDirs
                )
            }),
            _ => false,
        });
        let bound = spec.longest_path_bytes();
        if defeated || bound >= MAX_PORTABLE_PATH_BYTES {
            assert!(!spec.removable_by_path(0), "{label}: {bound} bytes");
            continue;
        }
        assert!(
            spec.removable_by_path(MAX_PORTABLE_PATH_BYTES - 1 - bound),
            "{label}: {bound} bytes below a root that leaves exactly that"
        );
        assert!(
            !spec.removable_by_path(MAX_PORTABLE_PATH_BYTES - bound),
            "{label}: {bound} bytes below a root that leaves one byte less"
        );
    }
}

#[test]
fn a_shaped_part_is_a_pure_function_of_its_fields_and_the_seed_only_moves_details() {
    let spec = shaped_spec();
    let first = Plan::new(&spec).expect("a plan");
    let again = Plan::new(&spec).expect("a plan");
    let reseeded = Plan::new(&spec.with_seed(5)).expect("a plan");

    assert_eq!(first.manifest_sha256(), again.manifest_sha256());
    assert_eq!(first.entries(), again.entries());
    assert_ne!(first.manifest_sha256(), reseeded.manifest_sha256());
    // The shape is the seed's to leave alone: every depth holds the same of everything.
    for depth in 1..=4 {
        for kind in [NodeKind::Directory, NodeKind::File, NodeKind::Symlink] {
            assert_eq!(
                at_depth(&first, kind, depth).len(),
                at_depth(&reseeded, kind, depth).len()
            );
        }
    }
    // Another part beside it changes nothing about it.
    let beside =
        format!("{SHAPED}\n[[parts]]\nkind = \"file\"\nroot = \"sentinel.bin\"\nsize = 8\n");
    let beside = Plan::new(&FixtureSpec::from_toml_str(&beside).expect("a spec")).expect("a plan");
    let home: Vec<&ManifestEntry> = beside
        .entries()
        .iter()
        .filter(|entry| entry.path.components().next() == Some(b"home".as_slice()))
        .collect();
    assert_eq!(home.len(), first.entries().len());
    assert!(
        home.iter()
            .zip(first.entries())
            .all(|(left, right)| *left == right)
    );
}

#[test]
fn the_manifest_hash_of_a_shaped_part_is_pinned() {
    // Like the hashes pinned above: the shaped part's names, sizes, spread, and links are an
    // interface that a cached fixture is keyed by, and a machine that disagrees is a machine whose
    // fixture is not the one the maintainer's profile described. The groups of hard links are
    // placed class of file size by class since this part took `hard_links` in place of
    // `hard_link_groups`, and that is why the hash is not the one the part had before.
    assert_eq!(
        Plan::new(&shaped_spec()).expect("a plan").manifest_sha256(),
        "69302ec4c9f7b8b1f211217724f524fd25f5a7b8c4a642caf7ff050be317b5c1"
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn a_shaped_part_that_breaks_a_rule_is_refused_with_the_field() {
    type Edit<'a> = &'a dyn Fn(&str) -> String;
    let with = |edit: Edit| FixtureSpec::from_toml_str(&edit(SHAPED));
    let cases: [(&str, Edit, &str); 26] = [
        (
            "no levels",
            &|text| {
                text.split("[[parts.levels]]")
                    .next()
                    .expect("a head")
                    .to_owned()
            },
            "levels",
        ),
        (
            "an unknown field",
            &|text| {
                text.replace(
                    "dangling_symlinks = 2",
                    "dangling_symlinks = 2\n    bytes = 1",
                )
            },
            "unknown field",
        ),
        (
            "a cap above what the generator writes",
            &|text| text.replace("max_file_bytes = 100", "max_file_bytes = 1073741825"),
            "max_file_bytes",
        ),
        (
            "a level that holds nothing",
            &|text| text.replace("files = 30\n    symlinks = 1", ""),
            "holds nothing",
        ),
        (
            "a level below a depth with no directory",
            &|text| {
                text.replace(
                    "directories = 2\n    files = 20",
                    "directories = 0\n    files = 20",
                )
            },
            "lies below a depth with no directory",
        ),
        (
            "more links dangling than there are",
            &|text| text.replace("dangling_symlinks = 2", "dangling_symlinks = 7"),
            "more links dangle than the levels hold",
        ),
        (
            "more names in a class than it has files",
            &|text| text.replace("names = 9", "names = 90"),
            "the class 64 has 34 files, and its groups have 90 names",
        ),
        (
            "a total of names that groups of those sizes cannot have",
            &|text| text.replace("names = 9", "names = 14"),
            "cannot have 14 names: they have 8 to 13",
        ),
        (
            "a class that no file is in",
            &|text| text.replace("class = 1\n    names = 2", "class = 8\n    names = 2"),
            "the class 8 has 0 files",
        ),
        (
            "classes out of order",
            &|text| text.replace("class = 1\n    names = 2", "class = 128\n    names = 2"),
            "list the classes in ascending order",
        ),
        (
            "a class of groups that is not a power of two",
            &|text| text.replace("class = 1\n    names = 2", "class = 3\n    names = 2"),
            "3 is not a class",
        ),
        (
            "no histogram of what a directory holds",
            &|text| {
                text.replace(
                    "[parts.subdirectories_per_directory]\n    0 = 2\n    1 = 2\n    2 = 1",
                    "",
                )
            },
            "needs this histogram",
        ),
        (
            "no histogram of file sizes",
            &|text| {
                text.replace(
                    "[parts.file_sizes]\n    0 = 1\n    1 = 2\n    64 = 4\n    4096 = 1",
                    "",
                )
            },
            "needs this histogram",
        ),
        (
            "no histogram of the names of files",
            &|text| text.replace("[parts.file_name_lengths]\n    6 = 3\n    11 = 1", ""),
            "needs this histogram",
        ),
        (
            "no histogram of the names of links",
            &|text| text.replace("[parts.symlink_name_lengths]\n    7 = 1", ""),
            "needs this histogram",
        ),
        (
            "no sizes of groups",
            &|text| text.replace("[parts.hard_links.group_sizes]\n    2 = 1\n", ""),
            "group_sizes",
        ),
        (
            "a class that is not a power of two",
            &|text| text.replace("64 = 4", "65 = 4"),
            "not a class",
        ),
        (
            "a name length of 0",
            &|text| text.replace("4 = 1\n    9 = 1", "0 = 1\n    9 = 1"),
            "not a name length",
        ),
        (
            "a name length past NAME_MAX",
            &|text| text.replace("7 = 1", "256 = 1"),
            "not a name length",
        ),
        (
            "a bucket that counts nothing",
            &|text| text.replace("0 = 1\n    1 = 2\n    64", "0 = 0\n    1 = 2\n    64"),
            "leave it out",
        ),
        (
            "a group of one",
            &|text| text.replace("2 = 2\n    4 = 1", "1 = 2\n    4 = 1"),
            "use classes from 2",
        ),
        (
            "a bucket named by a word",
            &|text| text.replace("0 = 2\n    1 = 2", "zero = 2\n    1 = 2"),
            "is not a bucket number",
        ),
        (
            "a bucket named twice",
            &|text| text.replace("4 = 1\n    9 = 1", "4 = 1\n    9 = 1\n    9 = 2"),
            "duplicate key",
        ),
        (
            "a root with a slash",
            &|text| text.replace("root = \"home\"", "root = \"home/shaped\""),
            "without `/`",
        ),
        (
            "two parts with one root",
            &|text| format!("{text}\n[[parts]]\nkind = \"file\"\nroot = \"home\"\n"),
            "already the root of another part",
        ),
        (
            "a link that does not dangle and no file",
            &|text| {
                text.replace("files = 4", "files = 0")
                    .replace("files = 20", "files = 0")
                    .replace("files = 30", "files = 0")
                    .replace("dangling_symlinks = 2", "dangling_symlinks = 1")
            },
            "needs a file to point at",
        ),
    ];
    for (what, edit, expected) in cases {
        let error = with(edit).expect_err(&format!("{what} must be refused"));
        assert!(
            error.to_string().contains(expected),
            "{what}: `{expected}` is not in `{error}`"
        );
    }
}

#[test]
fn a_shaped_part_has_a_size_limit_like_every_part_and_levels_are_a_tree_limit() {
    // 33 levels, each with a directory, is one more than a tree part may be deep.
    let mut levels = String::new();
    for _ in 0..33 {
        levels.push_str("[[parts.levels]]\ndirectories = 1\n");
    }
    let text = format!(
        "schema_version = 1\nid = \"deep\"\ndescription = \"x\"\nseed = 1\n\n[[parts]]\nkind = \"shaped\"\nroot = \"r\"\n{levels}\n[parts.subdirectories_per_directory]\n0 = 1\n\n[parts.directory_name_lengths]\n3 = 1\n"
    );
    let error = FixtureSpec::from_toml_str(&text).expect_err("33 levels are too many");
    assert!(error.to_string().contains("use 1 to 32 levels"), "{error}");
    let thirty_two = text.replacen("[[parts.levels]]\ndirectories = 1\n", "", 1);
    assert!(FixtureSpec::from_toml_str(&thirty_two).is_ok());

    // Ten million entries is the limit of every spec, the shaped part's included.
    let big = text
        .replacen("[[parts.levels]]\ndirectories = 1\n", "", 1)
        .replacen("directories = 1\n", "directories = 10100000\n", 1);
    let error = FixtureSpec::from_toml_str(&big).expect_err("more than the limit");
    assert!(
        error.to_string().contains("the limit is 10100000"),
        "{error}"
    );
}

/// A shaped part `levels` deep with names of 255 bytes, in `branches` chains of folders side by
/// side. At the last level it has one file and one link that resolves; with one chain they are
/// beside each other, and with two they are in the same chain or in different ones, whichever
/// the seed shuffles them to.
fn long_names_spec(levels: usize, branches: u32) -> String {
    let mut text = String::from(
        "schema_version = 1\nid = \"long-names\"\ndescription = \"Names of 255 bytes.\"\nseed = 1\n\n\
         [[parts]]\nkind = \"shaped\"\nroot = \"home\"\nmax_file_bytes = 100\n",
    );
    for _ in 1..levels {
        let _ = writeln!(text, "\n[[parts.levels]]\ndirectories = {branches}");
    }
    text.push_str("\n[[parts.levels]]\nfiles = 1\nsymlinks = 1\n");
    text.push_str(
        "\n[parts.subdirectories_per_directory]\n1 = 1\n\n\
         [parts.files_per_directory]\n0 = 1\n1 = 1\n\n\
         [parts.file_sizes]\n1 = 1\n\n\
         [parts.directory_name_lengths]\n255 = 1\n\n\
         [parts.file_name_lengths]\n8 = 1\n\n\
         [parts.symlink_name_lengths]\n8 = 1\n",
    );
    text
}

/// The longest target of a symbolic link in a plan, in bytes.
fn longest_target(plan: &Plan) -> usize {
    plan.entries()
        .iter()
        .filter_map(|entry| entry.target.as_ref())
        .map(|target| target.as_bytes().len())
        .max()
        .unwrap_or(0)
}

#[test]
fn a_link_beside_its_anchor_needs_only_the_anchors_name_however_deep_and_long_the_names_are() {
    // 32 levels of folders with names of 255 bytes: the file and the link are in the same
    // folder, 8 KiB of names from the top of the part.
    let spec = FixtureSpec::from_toml_str(&long_names_spec(32, 1)).expect("a spec");
    let plan = Plan::new(&spec).expect("a plan");

    let deepest = plan
        .entries()
        .iter()
        .map(|entry| entry.path.as_bytes().len())
        .max()
        .expect("entries");
    assert!(deepest > 7_900, "the part is {deepest} bytes deep");
    assert_eq!(
        longest_target(&plan),
        8,
        "the name of the file, and nothing of the 31 folders above it"
    );
}

#[test]
fn a_link_that_would_need_a_longer_target_than_a_link_holds_is_refused_by_the_plan() {
    // Two chains side by side, so that for some seeds the link is in one and its anchor is in the
    // other: up 31 levels and down 31 more, with names of 255 bytes.
    let spec = FixtureSpec::from_toml_str(&long_names_spec(32, 2)).expect("a spec");
    let (mut built, mut refused) = (0, 0);
    for seed in 1..=24 {
        match Plan::new(&spec.with_seed(seed)) {
            Ok(plan) => {
                built += 1;
                assert!(
                    longest_target(&plan) <= super::MAX_LINK_TARGET_BYTES,
                    "seed {seed}: a target of {} bytes",
                    longest_target(&plan)
                );
            }
            Err(error) => {
                refused += 1;
                let text = error.to_string();
                assert!(
                    text.contains("would need a target of")
                        && text.contains("a link holds at most 1023 on every platform"),
                    "seed {seed}: {text}"
                );
            }
        }
    }
    assert!(built > 0, "a link beside its file needs only its name");
    assert!(refused > 0, "a link in the other chain would need 8 KiB");
}

// ---------------------------------------------------------------------------------------------
// Groups of hard links in a shaped part, class of file size by class: every group is made, and
// the histograms of file sizes and of group sizes are exact.

/// The head of a spec of shaped parts that hold files and hard links alone.
const GROUPED_HEAD: &str =
    "schema_version = 1\nid = \"grouped\"\ndescription = \"Hard links.\"\nseed = 1\n\n";

/// A shaped part whose root `root` holds `files` files and nothing else, with the histogram of
/// file sizes `file_sizes` (lines of TOML) and the `hard_links` (see [`hard_link_classes`]).
fn grouped_part(root: &str, files: u32, file_sizes: &str, hard_links: &str) -> String {
    format!(
        "[[parts]]\nkind = \"shaped\"\nroot = \"{root}\"\nmax_file_bytes = 1000000\n\n\
         [[parts.levels]]\nfiles = {files}\n\n\
         [parts.files_per_directory]\n1 = 1\n\n\
         [parts.file_name_lengths]\n8 = 1\n\n\
         [parts.file_sizes]\n{file_sizes}\n\n{hard_links}\n"
    )
}

/// A spec of one shaped part, `home`.
fn grouped_spec(files: u32, file_sizes: &str, hard_links: &str) -> String {
    format!(
        "{GROUPED_HEAD}{}",
        grouped_part("home", files, file_sizes, hard_links)
    )
}

/// A hard-link class as a sample: its class, its names, and its sizes of group with how many
/// groups have each.
type ClassSample<'a> = (u64, u64, &'a [(u64, u64)]);

/// The TOML of hard-link classes: each as its class, its names, and its sizes of group.
fn hard_link_classes(classes: &[ClassSample]) -> String {
    let mut text = String::new();
    for (class, names, sizes) in classes {
        writeln!(
            text,
            "[[parts.hard_links]]\nclass = {class}\nnames = {names}\n[parts.hard_links.group_sizes]"
        )
        .expect("a string");
        for (size, count) in *sizes {
            writeln!(text, "{size} = {count}").expect("a string");
        }
        text.push('\n');
    }
    text
}

/// The names of each group of hard links in `plan`, as their sizes, by group.
fn groups_of(plan: &Plan) -> BTreeMap<u32, Vec<u64>> {
    let mut groups: BTreeMap<u32, Vec<u64>> = BTreeMap::new();
    for entry in plan.entries() {
        if let Some(group) = entry.link_group {
            groups.entry(group).or_default().push(entry.size);
        }
    }
    groups
}

/// Four files cannot hold two groups of four names. A plan that gave the first group the four
/// files and dropped the second would hold one group where the spec asks for two, so the spec is
/// refused, and what cannot be met is named.
#[test]
fn groups_that_the_files_cannot_hold_are_refused_and_not_dropped() {
    let error = FixtureSpec::from_toml_str(&grouped_spec(
        4,
        "1 = 4",
        &hard_link_classes(&[(1, 8, &[(4, 2)])]),
    ))
    .expect_err("two groups of four names need eight files");
    assert!(
        error
            .to_string()
            .contains("the class 1 has 4 files, and its groups have 8 names"),
        "{error}"
    );

    // The same four files hold two groups of two names, and the plan has both.
    let spec = FixtureSpec::from_toml_str(&grouped_spec(
        4,
        "1 = 4",
        &hard_link_classes(&[(1, 4, &[(2, 2)])]),
    ))
    .expect("a spec");
    for seed in 1..=4 {
        let plan = Plan::new(&spec.with_seed(seed)).expect("a plan");
        let sizes: Vec<usize> = groups_of(&plan).values().map(Vec::len).collect();
        assert_eq!(sizes, [2, 2], "seed {seed}");
    }
}

/// The names of a hard-linked file are one file, so they share a size. Two files whose sizes are
/// in two classes cannot be the names of one group: the groups of a class take their names from
/// the files of that class, so the spec is refused. With both files in one class it is built, and
/// both names are in it, whatever the seed.
#[test]
fn a_group_cannot_join_names_whose_sizes_are_in_two_classes() {
    let error = FixtureSpec::from_toml_str(&grouped_spec(
        2,
        "1 = 1\n2 = 1",
        &hard_link_classes(&[(1, 2, &[(2, 1)])]),
    ))
    .expect_err("one name in each class");
    assert!(
        error
            .to_string()
            .contains("the class 1 has 1 files, and its groups have 2 names"),
        "{error}"
    );

    let spec = FixtureSpec::from_toml_str(&grouped_spec(
        2,
        "2 = 2",
        &hard_link_classes(&[(2, 2, &[(2, 1)])]),
    ))
    .expect("a spec");
    for seed in 1..=8 {
        let plan = Plan::new(&spec.with_seed(seed)).expect("a plan");
        let made = groups_of(&plan).into_values().collect::<Vec<_>>();
        // One group of two names, one size, in the class of 2 and 3 where both files were.
        assert_eq!(made.len(), 1, "seed {seed}");
        assert_eq!(made[0].len(), 2, "seed {seed}");
        assert_eq!(made[0][0], made[0][1], "seed {seed}");
        assert!((2..=3).contains(&made[0][0]), "seed {seed}: {made:?}");
    }
}

/// Three classes of ten files each, with eight groups of 2 or 3 names and two of 4 to 7 among
/// them: ten groups, each in one class, that take every one of the thirty files. A layout that
/// puts each group wherever it fits by a rule of thumb can fail to find the places (the groups
/// 6, 4, four of 3, and four of 2 do not pack into three bins of ten by taking them largest
/// first), and a part that says which class each group is in has nothing to find: it builds
/// exactly what it says.
#[test]
fn three_classes_of_ten_files_with_ten_groups_in_them_build_exactly() {
    let text = grouped_spec(
        30,
        "1 = 10\n64 = 10\n4096 = 10",
        &hard_link_classes(&[
            (1, 10, &[(2, 2), (4, 1)]),
            (64, 10, &[(2, 2), (4, 1)]),
            (4096, 10, &[(2, 4)]),
        ]),
    );
    let spec = FixtureSpec::from_toml_str(&text).expect("a spec the files can hold");
    for seed in 1..=8 {
        let plan = Plan::new(&spec.with_seed(seed)).expect("a plan");
        let made = groups_of(&plan);
        assert_eq!(made.len(), 10, "seed {seed}: the groups made");
        assert_eq!(
            made.values().map(Vec::len).sum::<usize>(),
            30,
            "seed {seed}: every file is a name of a group"
        );
        let mut by_class: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
        for names in made.values() {
            assert!(names.iter().all(|size| *size == names[0]), "one size");
            by_class
                .entry(crate::histogram::class_floor(names[0]))
                .or_default()
                .push(names.len());
        }
        for sizes in by_class.values_mut() {
            sizes.sort_unstable();
        }
        assert_eq!(
            by_class,
            BTreeMap::from([
                (1, vec![2, 3, 5]),
                (64, vec![2, 3, 5]),
                (4096, vec![2, 2, 3, 3]),
            ]),
            "seed {seed}"
        );
    }
}

/// For every seed, a plan holds, in each class of file size, exactly the groups of hard links the
/// spec asks for there: the files of a class are the same number as the histogram of sizes says,
/// the groups of a class have the sizes `HardLinkClass::sizes` gives, which add up to its names
/// and fall in the buckets of its histogram, and the names of a group share a size. Grouping moves
/// no name to another class. The cut at `max_file_bytes` is part of the histogram (the shaped
/// spec cuts at 100, which puts two of its classes in one).
#[test]
fn the_groups_of_hard_links_keep_the_histograms_of_file_sizes_and_of_group_sizes_whatever_the_seed()
{
    use crate::{fixture::spec::Part, histogram::class_floor};

    let specs = [
        grouped_spec(
            30,
            "1 = 10\n64 = 10\n4096 = 10",
            &hard_link_classes(&[
                (1, 10, &[(2, 2), (4, 1)]),
                (64, 10, &[(2, 3), (4, 1)]),
                (4096, 6, &[(2, 2)]),
            ]),
        ),
        grouped_spec(
            12,
            "0 = 4\n8 = 8",
            &hard_link_classes(&[(0, 4, &[(2, 2)]), (8, 5, &[(2, 2)])]),
        ),
        grouped_spec(
            40,
            "2 = 1\n16 = 1\n256 = 2",
            &hard_link_classes(&[
                (2, 3, &[(2, 1)]),
                (16, 10, &[(2, 3), (4, 1)]),
                (256, 11, &[(4, 2), (2, 1)]),
            ]),
        ),
        SHAPED.to_owned(),
    ];
    for text in &specs {
        let spec = FixtureSpec::from_toml_str(text).expect("a spec");
        let Part::Shaped(part) = &spec.parts[0] else {
            panic!("a shaped part");
        };
        let want_classes: BTreeMap<u64, u64> = part.size_classes().into_iter().collect();

        for seed in 1..=12 {
            let plan = Plan::new(&spec.with_seed(seed)).expect("a plan");
            let label = format!("{} files, seed {seed}", part.file_count());

            let mut classes: BTreeMap<u64, u64> = BTreeMap::new();
            for entry in plan
                .entries()
                .iter()
                .filter(|entry| entry.kind == NodeKind::File)
            {
                *classes.entry(class_floor(entry.size)).or_default() += 1;
            }
            assert_eq!(
                classes, want_classes,
                "{label}: the histogram of sizes moved"
            );

            let made = groups_of(&plan);
            assert_eq!(
                made.len() as u64,
                part.hard_link_group_count(),
                "{label}: the groups made"
            );
            let mut by_class: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
            for (group, names) in &made {
                assert!(
                    names.iter().all(|size| *size == names[0]),
                    "{label}: group {group} has names of different sizes: {names:?}"
                );
                by_class
                    .entry(class_floor(names[0]))
                    .or_default()
                    .push(names.len() as u64);
            }
            for entry in &part.hard_links {
                let mut got = by_class.remove(&entry.class).unwrap_or_default();
                got.sort_unstable();
                assert_eq!(
                    got,
                    entry.sizes(),
                    "{label}: the groups of the class {}",
                    entry.class
                );
                assert_eq!(
                    got.iter().sum::<u64>(),
                    entry.names,
                    "{label}: the names of the class {}",
                    entry.class
                );
                let mut buckets = crate::histogram::Buckets::new();
                for size in &got {
                    buckets.add(class_floor(*size), 1);
                }
                assert_eq!(
                    buckets, entry.group_sizes,
                    "{label}: the histogram of group sizes of the class {}",
                    entry.class
                );
            }
            assert!(
                by_class.is_empty(),
                "{label}: groups in a class the spec gave none: {by_class:?}"
            );
        }
    }
}

/// The sizes of the groups of a class are exact numbers within the classes of its histogram that
/// add up to its names: for any histogram and any total the groups can have, over many of both.
#[test]
fn the_sizes_of_a_class_of_groups_add_up_to_its_names_and_stay_in_their_classes() {
    use crate::{
        fixture::spec::HardLinkClass,
        histogram::{BucketKind, Buckets},
    };

    let mut state: u64 = 0xb10c;
    let mut below = |limit: u64| {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (state >> 33) % limit
    };
    for round in 0..400 {
        let mut group_sizes = Buckets::new();
        for class in [2_u64, 4, 8, 16, 128, 4096] {
            if below(3) != 0 {
                group_sizes.add(class, 1 + below(if round % 9 == 0 { 400 } else { 6 }));
            }
        }
        if group_sizes.is_empty() {
            group_sizes.add(2, 1);
        }
        let mut entry = HardLinkClass {
            class: 1,
            names: 0,
            group_sizes,
        };
        let (least, most) = entry.names_range();
        let span = u64::try_from(most - least).expect("fits");
        entry.names = u64::try_from(least).expect("fits") + below(span + 1);

        let sizes = entry.sizes();
        assert_eq!(sizes.len() as u64, entry.groups(), "{entry:?}");
        assert_eq!(sizes.iter().sum::<u64>(), entry.names, "{entry:?}");
        assert!(sizes.windows(2).all(|pair| pair[0] <= pair[1]), "ascending");
        let mut found = Buckets::new();
        for size in &sizes {
            let (floor, ceiling) = BucketKind::Class.bounds(crate::histogram::class_floor(*size));
            assert!((floor..=ceiling).contains(size));
            found.add(floor, 1);
        }
        assert_eq!(found, entry.group_sizes, "{entry:?}");
        assert_eq!(sizes, entry.sizes(), "the same entry gives the same sizes");
    }
}

/// The groups of two shaped parts are never one group: each part has its own group, and the plan
/// has two, one in each part, however the parts number their groups.
#[test]
fn the_groups_of_two_shaped_parts_are_two_groups_and_never_one() {
    let home = grouped_part("home", 4, "1 = 4", &hard_link_classes(&[(1, 2, &[(2, 1)])]));
    let work = grouped_part(
        "work",
        4,
        "64 = 4",
        &hard_link_classes(&[(64, 2, &[(2, 1)])]),
    );
    let spec =
        FixtureSpec::from_toml_str(&format!("{GROUPED_HEAD}{home}\n{work}")).expect("a spec");
    for seed in 1..=6 {
        let plan = Plan::new(&spec.with_seed(seed)).expect("a plan");
        let made = groups_of(&plan);
        assert_eq!(
            made.keys().copied().collect::<Vec<_>>(),
            [1, 2],
            "seed {seed}: two groups"
        );
        for (group, names) in &made {
            assert_eq!(names.len(), 2, "seed {seed}: group {group}");
        }
        // Each group is whole in one part.
        for group in made.keys() {
            let roots: BTreeSet<&[u8]> = plan
                .entries()
                .iter()
                .filter(|entry| entry.link_group == Some(*group))
                .filter_map(|entry| entry.path.components().next())
                .collect();
            assert_eq!(roots.len(), 1, "seed {seed}: group {group} spans {roots:?}");
        }
    }
}
