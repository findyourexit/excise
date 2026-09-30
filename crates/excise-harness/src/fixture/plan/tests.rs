//! Plan tests: the manifest is deterministic, canonical, and consistent with the spec.

use std::collections::BTreeSet;

use super::{ManifestEntry, Plan};
use crate::fixture::{
    NodeKind,
    caps::{Capabilities, Capability},
    path::RelPath,
    spec::{FixtureSpec, SpecError},
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
    let fifty = spec("tiny-files-50k");
    assert_eq!(fifty.planned_entry_count(), 49_050);
    assert!(
        fifty.planned_entry_count() <= 50_000,
        "the 50k spec must respect the disk cap"
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
