//! The expansion of each part kind into manifest entries.
//!
//! Every generator draws from its own stream, `derive_seed(seed, root)`, so adding a part never
//! changes the names and sizes of the others. Entries are pushed in any order; the plan sorts
//! them.

use crate::fixture::{
    caps::Capability,
    names,
    path::RelPath,
    rng::{SplitMix64, derive_seed},
    spec::{
        DeepPart, FilePart, FilePlacement, HostileFeature, HostilePart, IdentityPart, LoopKind,
        Part, TreePart, VolumePart,
    },
};

use super::{GroupIds, ManifestEntry, VolumePlan};

/// Appends the entries of `part` to `entries` (and its volume, if it is one, to `volumes`). The
/// hard-link groups of a part are numbered from `groups`, which the whole plan shares.
pub(super) fn expand(
    part: &Part,
    seed: u64,
    entries: &mut Vec<ManifestEntry>,
    volumes: &mut Vec<VolumePlan>,
    groups: &mut GroupIds,
) {
    let stream = derive_seed(seed, part.root().as_bytes());
    match part {
        Part::Tree(part) => tree(part, stream, entries),
        Part::Deep(part) => deep(part, stream, entries),
        Part::File(part) => file(part, stream, entries),
        Part::Identity(part) => identity(part, stream, entries, groups),
        Part::Hostile(part) => hostile(part, stream, entries),
        Part::Shaped(part) => {
            super::shaped::expand(part, stream, &top_level(&part.root), entries, groups);
        }
        Part::Volume(part) => volume(part, entries, volumes),
    }
}

fn top_level(root: &str) -> RelPath {
    RelPath::root().join(root.as_bytes())
}

fn tree(part: &TreePart, stream: u64, entries: &mut Vec<ManifestEntry>) {
    let root = top_level(&part.root);
    entries.push(ManifestEntry::directory(root.clone()));
    let mut sizes = SplitMix64::new(stream);
    grow(part, &root, 0, stream, &mut sizes, entries);
}

fn grow(
    part: &TreePart,
    directory: &RelPath,
    level: u32,
    stream: u64,
    sizes: &mut SplitMix64,
    entries: &mut Vec<ManifestEntry>,
) {
    let deepest = level == part.depth;
    // Names are keyed per directory, so every directory of a hex-named tree has different names.
    let key = derive_seed(stream, directory.as_bytes());
    if part.file_placement == FilePlacement::All || deepest {
        for index in 0..u64::from(part.files_per_dir) {
            let name = part.file_names.name(index, key);
            entries.push(ManifestEntry::file(
                directory.join(&name),
                part.file_size.draw(sizes),
            ));
        }
    }
    if !deepest {
        for index in 0..u64::from(part.fanout) {
            let child = directory.join(&part.dir_names.name(index, derive_seed(key, b"dirs")));
            entries.push(ManifestEntry::directory(child.clone()));
            grow(part, &child, level + 1, stream, sizes, entries);
        }
    }
}

/// The name of the directory at `level` of a deep chain: a zero-padded level number, a dash,
/// then filler letters up to `length` bytes.
fn deep_name(level: u32, length: usize) -> Vec<u8> {
    let mut name = format!("{level:04}-").into_bytes();
    let filler = b"abcdefghijklmnopqrstuvwxyz";
    let mut next = 0;
    while name.len() < length {
        name.push(filler[next % filler.len()]);
        next += 1;
    }
    name.truncate(length);
    name
}

fn deep(part: &DeepPart, stream: u64, entries: &mut Vec<ManifestEntry>) {
    let mut sizes = SplitMix64::new(stream);
    let mut current = top_level(&part.root);
    entries.push(ManifestEntry::directory(current.clone()));
    let length = usize::try_from(part.name_len).unwrap_or(usize::MAX);
    for level in 0..=part.depth {
        if level > 0 {
            current = current.join(&deep_name(level, length));
            entries.push(ManifestEntry::directory(current.clone()));
        }
        for index in 0..part.files_per_level {
            entries.push(ManifestEntry::file(
                current.join(format!("f{index}.dat").as_bytes()),
                part.file_size.draw(&mut sizes),
            ));
        }
    }
}

fn file(part: &FilePart, stream: u64, entries: &mut Vec<ManifestEntry>) {
    let mut sizes = SplitMix64::new(stream);
    entries.push(ManifestEntry::file(
        top_level(&part.root),
        part.size.draw(&mut sizes),
    ));
}

fn identity(
    part: &IdentityPart,
    stream: u64,
    entries: &mut Vec<ManifestEntry>,
    groups: &mut GroupIds,
) {
    let mut sizes = SplitMix64::new(stream);
    let root = top_level(&part.root);
    entries.push(ManifestEntry::directory(root.clone()));

    if part.hard_link_groups > 0 {
        hard_links(part, &root, &mut sizes, entries, groups);
    }

    if part.dangling_symlinks > 0 {
        let directory = root.join(b"dangling");
        entries.push(ManifestEntry::directory(directory.clone()).requiring(Capability::Symlinks));
        for index in 0..part.dangling_symlinks {
            entries.push(ManifestEntry::symlink(
                directory.join(format!("dangling{index}").as_bytes()),
                format!("../missing/nothing{index}").as_bytes(),
            ));
        }
    }

    if part.valid_symlinks {
        let directory = root.join(b"symlinks");
        entries.push(ManifestEntry::directory(directory.clone()).requiring(Capability::Symlinks));
        entries.push(
            ManifestEntry::file(directory.join(b"target.dat"), 10).requiring(Capability::Symlinks),
        );
        entries.push(
            ManifestEntry::directory(directory.join(b"real-dir")).requiring(Capability::Symlinks),
        );
        entries.push(ManifestEntry::symlink(
            directory.join(b"file-link"),
            b"target.dat",
        ));
        entries.push(ManifestEntry::symlink(
            directory.join(b"dir-link"),
            b"real-dir",
        ));
    }

    if !part.symlink_loops.is_empty() {
        let directory = root.join(b"loops");
        entries.push(ManifestEntry::directory(directory.clone()).requiring(Capability::Symlinks));
        for kind in &part.symlink_loops {
            match kind {
                LoopKind::SelfLink => {
                    entries.push(ManifestEntry::symlink(directory.join(b"self"), b"self"));
                }
                LoopKind::Pair => {
                    entries.push(ManifestEntry::symlink(directory.join(b"pair-a"), b"pair-b"));
                    entries.push(ManifestEntry::symlink(directory.join(b"pair-b"), b"pair-a"));
                }
                LoopKind::Directory => {
                    let cycle = directory.join(b"cycle");
                    entries.push(
                        ManifestEntry::directory(cycle.clone()).requiring(Capability::Symlinks),
                    );
                    entries.push(ManifestEntry::symlink(cycle.join(b"again"), b"../cycle"));
                }
            }
        }
    }

    if !part.sparse_files.is_empty() {
        let directory = root.join(b"sparse");
        entries
            .push(ManifestEntry::directory(directory.clone()).requiring(Capability::SparseFiles));
        for (index, sparse) in part.sparse_files.iter().enumerate() {
            let mut entry = ManifestEntry::file(
                directory.join(format!("s{index}.bin").as_bytes()),
                sparse.apparent_mib * 1024 * 1024,
            )
            .requiring(Capability::SparseFiles);
            entry.sparse_data = Some(u64::from(sparse.data_kib) * 1024);
            entries.push(entry);
        }
    }

    if part.clones > 0 {
        let directory = root.join(b"clones");
        entries.push(ManifestEntry::directory(directory.clone()).requiring(Capability::Clones));
        let original = directory.join(b"original.bin");
        entries.push(
            ManifestEntry::file(original.clone(), u64::from(part.clone_kib) * 1024)
                .requiring(Capability::Clones),
        );
        for index in 0..part.clones {
            let mut entry = ManifestEntry::file(
                directory.join(format!("clone{index}.bin").as_bytes()),
                u64::from(part.clone_kib) * 1024,
            )
            .requiring(Capability::Clones);
            entry.clone_of = Some(original.clone());
            entries.push(entry);
        }
    }
}

/// The `links/` directory of an identity part: groups of names for one file, spread over several
/// directories so that the names of one file live in different places.
fn hard_links(
    part: &IdentityPart,
    root: &RelPath,
    sizes: &mut SplitMix64,
    entries: &mut Vec<ManifestEntry>,
    groups: &mut GroupIds,
) {
    let links = root.join(b"links");
    entries.push(ManifestEntry::directory(links.clone()));
    let spread = u64::from(part.link_spread);
    let directories: Vec<RelPath> = (0..spread)
        .map(|index| links.join(format!("d{index}").as_bytes()))
        .collect();
    for directory in &directories {
        entries.push(ManifestEntry::directory(directory.clone()));
    }
    for group in 0..u64::from(part.hard_link_groups) {
        let id = groups.next();
        let size = part.link_file_size.draw(sizes);
        for name in 0..u64::from(part.links_per_group) {
            // Consecutive names of a group land in different directories, so the names of one
            // file live in several places.
            let directory = &directories[usize::try_from((group + name) % spread).unwrap_or(0)];
            let mut entry = ManifestEntry::file(
                directory.join(format!("g{group}-n{name}.dat").as_bytes()),
                size,
            );
            // A temporary group id, which no other group of the plan has; the plan renumbers
            // groups in canonical order.
            entry.link_group = Some(id);
            entries.push(entry);
        }
    }
}

/// The names of one hostile feature, and the capability its names need.
fn catalog(feature: HostileFeature) -> Option<(Vec<Vec<u8>>, Option<Capability>)> {
    match feature {
        HostileFeature::ControlNames => Some((
            names::control_character_names(),
            Some(Capability::ControlCharacterNames),
        )),
        HostileFeature::BidiNames => Some((names::bidi_names(), None)),
        HostileFeature::EscapeNames => Some((
            names::escape_sequence_names(),
            Some(Capability::ControlCharacterNames),
        )),
        HostileFeature::NewlineNames => Some((
            names::newline_names(),
            Some(Capability::ControlCharacterNames),
        )),
        HostileFeature::InvalidUtf8Names => Some((
            names::invalid_utf8_names(),
            Some(Capability::InvalidUtf8Names),
        )),
        HostileFeature::LongNames => Some((names::maximum_length_names(), None)),
        HostileFeature::UnreadableDirs
        | HostileFeature::UnreadableFiles
        | HostileFeature::UnwritableDirs => None,
    }
}

fn hostile(part: &HostilePart, stream: u64, entries: &mut Vec<ManifestEntry>) {
    let mut sizes = SplitMix64::new(stream);
    let root = top_level(&part.root);
    entries.push(ManifestEntry::directory(root.clone()));
    let restricted = |entry: ManifestEntry| entry.requiring(Capability::RestrictedModes);
    let mut unreadable_added = false;

    for feature in &part.features {
        let directory = root.join(feature.directory().as_bytes());
        if let Some((catalog, requirement)) = catalog(*feature) {
            let needing = |entry: ManifestEntry| match requirement {
                Some(capability) => entry.requiring(capability),
                None => entry,
            };
            entries.push(needing(ManifestEntry::directory(directory.clone())));
            for name in catalog {
                let subdirectory = directory.join(&name);
                entries.push(needing(ManifestEntry::directory(subdirectory.clone())));
                entries.push(needing(ManifestEntry::file(
                    subdirectory.join(&name),
                    sizes.range_inclusive(1, 64),
                )));
            }
            continue;
        }
        if *feature == HostileFeature::UnwritableDirs {
            // A directory that can be listed and entered but not changed, so nothing in it can be
            // removed. The mode is applied once the file is in it.
            entries.push(restricted(
                ManifestEntry::directory(directory.clone()).with_mode(0o555),
            ));
            entries.push(restricted(ManifestEntry::file(
                directory.join(b"stuck.txt"),
                4096,
            )));
            continue;
        }
        if !unreadable_added {
            unreadable_added = true;
            entries.push(restricted(ManifestEntry::directory(directory.clone())));
        }
        if *feature == HostileFeature::UnreadableDirs {
            // A directory that can be neither listed nor entered, with contents behind it.
            let locked = directory.join(b"locked-000");
            entries.push(restricted(
                ManifestEntry::directory(locked.clone()).with_mode(0o000),
            ));
            entries.push(restricted(ManifestEntry::file(
                locked.join(b"inner.txt"),
                5,
            )));
            let nested = locked.join(b"nested");
            entries.push(restricted(ManifestEntry::directory(nested.clone())));
            entries.push(restricted(ManifestEntry::file(nested.join(b"deep.txt"), 7)));
            // A directory that can be entered but not listed.
            let search_only = directory.join(b"locked-100");
            entries.push(restricted(
                ManifestEntry::directory(search_only.clone()).with_mode(0o100),
            ));
            entries.push(restricted(ManifestEntry::file(
                search_only.join(b"inner.txt"),
                5,
            )));
        } else {
            entries.push(restricted(
                ManifestEntry::file(directory.join(b"locked-file-000.bin"), 100).with_mode(0o000),
            ));
            entries.push(restricted(
                ManifestEntry::file(directory.join(b"write-only.bin"), 16).with_mode(0o200),
            ));
        }
    }
}

fn volume(part: &VolumePart, entries: &mut Vec<ManifestEntry>, volumes: &mut Vec<VolumePlan>) {
    let mount = top_level(&part.root);
    entries.push(ManifestEntry::directory(mount.clone()));
    volumes.push(VolumePlan {
        mount,
        size_mib: part.size_mib,
        files: part.files,
        file_bytes: part.file_bytes,
    });
}
