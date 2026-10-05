//! The expansion of a `shaped` part: a tree that has exactly the counts of its levels and the
//! spread of its histograms.
//!
//! The tree is built a depth at a time. The directories of depth `n - 1` are the parents of the
//! entries of depth `n`; each kind of entry is split among them in proportion to weights that
//! are the quantiles of a histogram (see [`crate::histogram::spread`]), shuffled so that which
//! parent gets which weight depends on the seed. Nothing is sampled, so the totals of a level and
//! the shape of the whole are the same for every seed, and everything is integer arithmetic.
//!
//! Every directory names its children with a number that is unique among them, in hexadecimal
//! and as many digits as the directory needs for its children, then pads the name with more
//! digits up to the length drawn for it. A hexadecimal name is lowercase ASCII digits and letters
//! that no file system or operating system treats as special, and no entry is named `missing`,
//! which is what a dangling link points at.

use std::collections::BTreeMap;

use crate::{
    fixture::{
        names::{MAX_NAME_BYTES, hex_digits_for},
        path::{LinkTarget, RelPath},
        rng::{SplitMix64, derive_seed, permute},
        spec::ShapedPart,
    },
    histogram::{BucketKind, Buckets, apportion, class_floor, spread},
};

use super::{GroupIds, ManifestEntry};

/// The target of a symbolic link that points at nothing.
const MISSING: &[u8] = b"missing";

const HEX: &[u8; 16] = b"0123456789abcdef";

/// Appends the entries of `part`, whose top-level directory is `root`, to `entries`. Its groups
/// of hard links are numbered from `ids`, which the whole plan shares.
pub(super) fn expand(
    part: &ShapedPart,
    stream: u64,
    root: &RelPath,
    entries: &mut Vec<ManifestEntry>,
    ids: &mut GroupIds,
) {
    let mut rng = SplitMix64::new(stream);
    entries.push(ManifestEntry::directory(root.clone()));
    let mut draws = Draws::new(part, &mut rng);

    let mut parents = vec![root.clone()];
    let mut files: Vec<usize> = Vec::new();
    let mut links: Vec<usize> = Vec::new();
    for level in &part.levels {
        let width = parents.len();
        let directories = shares(
            u64::from(level.directories),
            &part.subdirectories_per_directory,
            width,
            &mut rng,
        );
        let regular = shares(
            u64::from(level.files),
            &part.files_per_directory,
            width,
            &mut rng,
        );
        let symbolic = shares(
            u64::from(level.symlinks),
            &part.files_per_directory,
            width,
            &mut rng,
        );
        let mut next = Vec::with_capacity(usize::try_from(level.directories).unwrap_or(0));
        for (index, parent) in parents.iter().enumerate() {
            let held = directories[index] + regular[index] + symbolic[index];
            if held == 0 {
                continue;
            }
            let key = derive_seed(stream, parent.as_bytes());
            let digits = hex_digits_for(held);
            let mut number = 0;
            let mut next_name = |length: u64, rng: &mut SplitMix64| {
                let name = hex_name(number, key, digits, length, rng);
                number += 1;
                parent.join(&name)
            };
            for _ in 0..directories[index] {
                let path = next_name(draws.directory_length(), &mut rng);
                entries.push(ManifestEntry::directory(path.clone()));
                next.push(path);
            }
            for _ in 0..regular[index] {
                let path = next_name(draws.file_length(), &mut rng);
                files.push(entries.len());
                entries.push(ManifestEntry::file(path, draws.size()));
            }
            for _ in 0..symbolic[index] {
                let path = next_name(draws.symlink_length(), &mut rng);
                links.push(entries.len());
                entries.push(ManifestEntry::symlink(path, b""));
            }
        }
        parents = next;
    }
    group_hard_links(part, &files, entries, &mut rng, ids);
    point_symlinks(part, &files, &links, entries, &mut rng);
}

/// The values drawn for the entries of a part, in a random order each: every size and every name
/// length is taken from the shuffled quantiles of its histogram, one for each entry.
struct Draws {
    sizes: Vec<u64>,
    directory_lengths: Vec<u64>,
    file_lengths: Vec<u64>,
    symlink_lengths: Vec<u64>,
}

impl Draws {
    fn new(part: &ShapedPart, rng: &mut SplitMix64) -> Self {
        let drawn = |buckets: &Buckets, count: u64, kind, rng: &mut SplitMix64| {
            let count = usize::try_from(count).unwrap_or(0);
            shuffled(spread(buckets, count, kind), rng)
        };
        let mut sizes = drawn(&part.file_sizes, part.file_count(), BucketKind::Class, rng);
        for size in &mut sizes {
            *size = (*size).min(part.max_file_bytes);
        }
        Self {
            sizes,
            directory_lengths: drawn(
                &part.directory_name_lengths,
                part.directory_count(),
                BucketKind::Length,
                rng,
            ),
            file_lengths: drawn(
                &part.file_name_lengths,
                part.file_count(),
                BucketKind::Length,
                rng,
            ),
            symlink_lengths: drawn(
                &part.symlink_name_lengths,
                part.symlink_count(),
                BucketKind::Length,
                rng,
            ),
        }
    }

    fn size(&mut self) -> u64 {
        self.sizes.pop().unwrap_or(0)
    }

    fn directory_length(&mut self) -> u64 {
        self.directory_lengths.pop().unwrap_or(1)
    }

    fn file_length(&mut self) -> u64 {
        self.file_lengths.pop().unwrap_or(1)
    }

    fn symlink_length(&mut self) -> u64 {
        self.symlink_lengths.pop().unwrap_or(1)
    }
}

/// `values` in a random order: Fisher-Yates.
fn shuffled<T>(mut values: Vec<T>, rng: &mut SplitMix64) -> Vec<T> {
    for end in (1..values.len()).rev() {
        let pick = usize::try_from(rng.below(end as u64 + 1)).unwrap_or(0);
        values.swap(end, pick);
    }
    values
}

/// Splits `total` among `parents` directories: in proportion to as many weights from `buckets`,
/// in a random order.
fn shares(total: u64, buckets: &Buckets, parents: usize, rng: &mut SplitMix64) -> Vec<u64> {
    if total == 0 {
        return vec![0; parents];
    }
    let weights = shuffled(spread(buckets, parents, BucketKind::Class), rng);
    apportion(total, &weights)
}

/// The name of the entry numbered `number` in a directory whose names are keyed by `key`: `digits`
/// hexadecimal digits that are a keyed permutation of the number, so that no two names in the
/// directory are alike, then random digits up to `length` bytes.
fn hex_name(number: u64, key: u64, digits: u32, length: u64, rng: &mut SplitMix64) -> Vec<u8> {
    let unique = permute(number, key, digits * 4);
    let mut name = format!("{unique:0width$x}", width = digits as usize).into_bytes();
    let wanted = usize::try_from(length)
        .unwrap_or(MAX_NAME_BYTES)
        .min(MAX_NAME_BYTES);
    while name.len() < wanted {
        let mut word = rng.next_u64();
        for _ in 0..16 {
            if name.len() == wanted {
                break;
            }
            name.push(HEX[usize::try_from(word & 0xf).unwrap_or(0)]);
            word >>= 4;
        }
    }
    name
}

/// Makes the groups of regular files that are names of one file. For each class of size that
/// `part.hard_links` has, the groups of the class (see
/// [`HardLinkClass::sizes`](crate::fixture::spec::HardLinkClass::sizes)) take their names from the
/// files in that class, chosen in a random order, and every name of a group is given the size of
/// the first. The names of a group are of one class, so no file leaves its class and the
/// histogram of sizes stays what the part says. The groups are numbered from `ids`.
fn group_hard_links(
    part: &ShapedPart,
    files: &[usize],
    entries: &mut [ManifestEntry],
    rng: &mut SplitMix64,
    ids: &mut GroupIds,
) {
    if part.hard_links.is_empty() {
        return;
    }
    // The files of each class that holds groups, as positions in `files`.
    let mut in_class: BTreeMap<u64, Vec<usize>> = part
        .hard_links
        .iter()
        .map(|entry| (entry.class, Vec::new()))
        .collect();
    for (position, index) in files.iter().enumerate() {
        if let Some(positions) = in_class.get_mut(&class_floor(entries[*index].size)) {
            positions.push(position);
        }
    }
    for entry in &part.hard_links {
        let Some(positions) = in_class.get_mut(&entry.class) else {
            continue;
        };
        let sizes = entry.sizes();
        // The first `names` positions of a shuffle of the files of the class.
        let names = sizes
            .iter()
            .fold(0_usize, |sum, size| {
                sum.saturating_add(usize::try_from(*size).unwrap_or(usize::MAX))
            })
            .min(positions.len());
        for start in 0..names {
            let remaining = (positions.len() - start) as u64;
            let pick = start + usize::try_from(rng.below(remaining)).unwrap_or(0);
            positions.swap(start, pick);
        }
        let mut taken = 0_usize;
        for size in sizes {
            let size = usize::try_from(size).unwrap_or(usize::MAX);
            // A part that was validated has the files; a group that cannot be made is left out.
            let Some(members) = positions.get(taken..taken.saturating_add(size)) else {
                break;
            };
            taken += size;
            let id = ids.next();
            let shared = entries[files[members[0]]].size;
            for position in members {
                let name = &mut entries[files[*position]];
                // A temporary number, which no other group of the plan has; the plan renumbers
                // the groups in canonical order.
                name.link_group = Some(id);
                name.size = shared;
            }
        }
    }
}

/// Gives every symbolic link its target: `dangling_symlinks` of them, chosen at random, point at
/// nothing, and the others at one file of the part that exists wherever any file does, by the
/// shortest relative path (see [`relative_target`]): a file that is nobody's hard link, or, when
/// every file has more than one name, the file of a group (see [`surviving_path`]).
fn point_symlinks(
    part: &ShapedPart,
    files: &[usize],
    links: &[usize],
    entries: &mut [ManifestEntry],
    rng: &mut SplitMix64,
) {
    if links.is_empty() {
        return;
    }
    let dangling = usize::try_from(part.dangling_symlinks)
        .unwrap_or(usize::MAX)
        .min(links.len());
    let mut order: Vec<usize> = (0..links.len()).collect();
    for start in 0..dangling {
        let remaining = (order.len() - start) as u64;
        let pick = start + usize::try_from(rng.below(remaining)).unwrap_or(0);
        order.swap(start, pick);
    }
    // A file that exists wherever any file does: one that is nobody's hard link, or the file of a
    // group, and not whichever name of a group the part happened to plan first.
    let anchor = files
        .iter()
        .find(|index| entries[**index].link_group.is_none())
        .or_else(|| files.first())
        .map(|index| surviving_path(entries, *index));
    for (rank, position) in order.iter().enumerate() {
        let index = links[*position];
        let target = match &anchor {
            Some(anchor) if rank >= dangling => relative_target(&entries[index].path, anchor),
            _ => MISSING.to_vec(),
        };
        entries[index].size = target.len() as u64;
        entries[index].target = Some(LinkTarget::new(target));
    }
}

/// The path of the file that stays where hard links cannot be made, for the name `entries[index]`:
/// its own when the file has no other name, and otherwise the first name of its group in canonical
/// order. The plan keeps that name as the file and makes every other name of the group a hard link
/// to it (see `finish_link_groups`), so where hard links cannot be made the other names are not
/// created, and a link to one of them would point at nothing.
fn surviving_path(entries: &[ManifestEntry], index: usize) -> RelPath {
    let entry = &entries[index];
    let Some(group) = entry.link_group else {
        return entry.path.clone();
    };
    entries
        .iter()
        .filter(|other| other.link_group == Some(group))
        .map(|other| &other.path)
        .min()
        .unwrap_or(&entry.path)
        .clone()
}

/// The shortest relative path from the folder that holds the link `link` to the file `anchor`:
/// the anchor's own name when the two are side by side, and otherwise `..` for each folder the
/// link is below the nearest folder the two share, then the way down from there to the anchor.
/// Names are at most 255 bytes, so a target is at most `3 * up + 256 * down` bytes long, which
/// is short whenever the anchor is near the link or near the top of the part; the plan refuses a
/// part in which one would still be longer than a link can hold (see
/// [`super::MAX_LINK_TARGET_BYTES`]).
fn relative_target(link: &RelPath, anchor: &RelPath) -> Vec<u8> {
    let link_parts: Vec<&[u8]> = link.components().collect();
    let anchor_parts: Vec<&[u8]> = anchor.components().collect();
    // Only folders are shared: the last component of each path is the entry itself.
    let link_folders = &link_parts[..link_parts.len().saturating_sub(1)];
    let anchor_folders = &anchor_parts[..anchor_parts.len().saturating_sub(1)];
    let shared = link_folders
        .iter()
        .zip(anchor_folders)
        .take_while(|(from_link, from_anchor)| from_link == from_anchor)
        .count();
    let mut target = b"../".repeat(link_folders.len() - shared);
    for (index, part) in anchor_parts[shared..].iter().enumerate() {
        if index > 0 {
            target.push(b'/');
        }
        target.extend_from_slice(part);
    }
    target
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(text: &str) -> RelPath {
        RelPath::from_bytes(text.as_bytes()).expect("a path")
    }

    #[test]
    fn a_link_beside_its_anchor_gets_just_the_anchors_name() {
        assert_eq!(
            relative_target(&path("shaped/a/link"), &path("shaped/a/file")),
            b"file"
        );
        assert_eq!(
            relative_target(&path("shaped/link"), &path("shaped/file")),
            b"file"
        );
    }

    #[test]
    fn a_link_climbs_to_the_nearest_folder_it_shares_with_its_anchor_and_goes_down() {
        // Below the anchor's folder: up, and no way down.
        assert_eq!(
            relative_target(&path("shaped/a/b/c/link"), &path("shaped/file")),
            b"../../../file"
        );
        // Above the anchor's folder: no way up, and down.
        assert_eq!(
            relative_target(&path("shaped/link"), &path("shaped/a/b/file")),
            b"a/b/file"
        );
        // Sideways: up to the folder they share, and down the other branch.
        assert_eq!(
            relative_target(&path("shaped/a/x/link"), &path("shaped/a/y/z/file")),
            b"../y/z/file"
        );
        // Folders that only share a prefix of a name are not shared.
        assert_eq!(
            relative_target(&path("shaped/ab/link"), &path("shaped/a/file")),
            b"../a/file"
        );
    }
}
