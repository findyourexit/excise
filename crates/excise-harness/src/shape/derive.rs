//! A fixture specification shaped like a profile.
//!
//! [`spec_from_profile`] turns the aggregates of a [`HarnessShapeProfile`] into a [`FixtureSpec`]
//! with one [`ShapedPart`], scaled to a number of entries. The scaling is exact where the part
//! can be: the spec plans precisely the entries asked for.
//!
//! # How the profile is scaled
//!
//! * **Counts.** The directories, files, and symbolic links at each depth are scaled by the same
//!   factor, `entries / profile entries`, with the method of largest remainders so that they add
//!   up to the entries asked for. The entries a spec plans are the part's root directory and the
//!   levels, so the levels get `entries - 1`. Every depth above the deepest keeps at least one
//!   directory and the deepest keeps at least one entry, so that a profile scaled far down stays
//!   as deep as it was, and those few entries are taken from the budget before the rest is shared.
//!   A tree deeper than the part allows ([`MAX_SHAPED_DEPTH`]) has the entries below that depth
//!   counted at the last one.
//! * **Shape.** What a directory holds, how large a file is, and how long a name is keep their
//!   histograms whatever the scale: scaling a tree down means fewer directories, not smaller
//!   ones. A tree scaled down far enough has too few directories to show the tail of a
//!   histogram, and shows only as much of it as there is room for.
//! * **Links.** The groups of hard links are scaled class by class: the files of a class of size
//!   and the groups the profile had in it are scaled by the same factor, with the same rounding
//!   as every other count, and the names of a class's groups stay among the files of the class.
//!   A group the files of its class have no room for is left out, the largest first, and the
//!   names of a class are carried as the profile had them (scaled), within what its groups can
//!   have. A file whose other names lay outside the tree that was profiled has no group to be
//!   in, so it is a plain file. Every symbolic link points at a file of the part, and none
//!   dangles: a profile says nothing of where a link pointed, because the walk never asks, so
//!   there is no share to keep. (A part with no file has nothing to point at, and its links point
//!   at nothing.)
//! * **What is left out.** Sockets, FIFOs, and devices cannot be generated, and neither can
//!   folders that cannot be listed or mount points, because a fixture with one could not be
//!   cached or removed the way every other fixture is.
//!
//! The result is a pure function of the profile and the request: the same inputs give the same
//! spec, and so the same manifest hash.

use std::collections::BTreeMap;

use thiserror::Error;

use crate::{
    fixture::{
        Plan,
        spec::{
            FixtureSpec, HardLinkClass, MAX_ENTRIES, MAX_SHAPED_DEPTH, Part, SPEC_SCHEMA_VERSION,
            ShapedLevel, ShapedPart, SpecError, SpecProblems,
        },
    },
    histogram::{Buckets, apportion, class_floor},
    report::{HarnessShapeProfile, ShapeProblems},
};

/// The name of the top-level directory of a derived spec.
pub const SHAPED_ROOT: &str = "shaped";

/// What to build from a profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecRequest {
    /// The id of the spec: the name of its file without `.toml`.
    pub id: String,
    /// How many entries the spec plans, the root directory of the part included. The fixture
    /// holds one more, the ownership marker.
    pub entries: u64,
    /// The default seed of the spec.
    pub seed: u64,
    /// The largest file the spec writes, in bytes.
    pub max_file_bytes: u64,
}

/// Why a spec could not be built from a profile.
#[derive(Debug, Error)]
pub enum DeriveError {
    /// The numbers of the profile do not agree with each other.
    #[error("the profile is not consistent: {0}")]
    Inconsistent(#[from] ShapeProblems),
    /// The profile has nothing to shape a tree from.
    #[error("the profile holds no folder, file, or symbolic link to shape a tree from")]
    Empty,
    /// The number of entries is not one a spec can plan.
    #[error("a spec plans at least 1 and at most {MAX_ENTRIES} entries, not {entries}")]
    Entries {
        /// What was asked for.
        entries: u64,
    },
    /// The levels the spec keeps, the profile's depth or the most a spec holds when the profile is
    /// deeper, need more entries than that.
    #[error("a spec that keeps {depth} levels needs at least {minimum} entries, not {entries}")]
    TooSmall {
        /// The depth the tree keeps.
        depth: u32,
        /// The fewest entries that reach it.
        minimum: u64,
        /// What was asked for.
        entries: u64,
    },
    /// The spec that was built breaks a rule of the generator.
    #[error("the spec built from the profile is not valid: {0}")]
    Invalid(#[from] SpecProblems),
    /// The spec that was built cannot be planned: a symbolic link in it would need a target
    /// longer than a link can hold.
    #[error("the spec built from the profile cannot be planned: {0}")]
    Plan(#[from] SpecError),
}

/// `value * numerator / denominator`, rounded to the nearest whole number; 0 when there is no
/// denominator.
fn scaled(value: u64, numerator: u64, denominator: u64) -> u64 {
    if denominator == 0 {
        return 0;
    }
    let wide = u128::from(value) * u128::from(numerator) + u128::from(denominator / 2);
    u64::try_from(wide / u128::from(denominator)).unwrap_or(u64::MAX)
}

/// Builds the spec of `request` from `profile`.
///
/// # Errors
///
/// Returns why the profile cannot be used or the request cannot be met: see [`DeriveError`].
pub fn spec_from_profile(
    profile: &HarnessShapeProfile,
    request: &SpecRequest,
) -> Result<FixtureSpec, DeriveError> {
    profile.check()?;
    if request.entries == 0 || request.entries > MAX_ENTRIES {
        return Err(DeriveError::Entries {
            entries: request.entries,
        });
    }

    // What the profile holds that the generator can build, depth by depth: directories, files,
    // and symbolic links. Entries deeper than the part allows are counted at its last depth.
    let kept = profile.max_depth.min(MAX_SHAPED_DEPTH);
    let depth = usize::try_from(kept).unwrap_or(0);
    let mut source = vec![[0_u64; 3]; depth];
    for level in &profile.depths {
        let index = usize::try_from(level.depth.min(kept)).unwrap_or(1) - 1;
        source[index][0] += level.directories;
        source[index][1] += level.files;
        source[index][2] += level.symlinks;
    }
    if source.iter().flatten().all(|value| *value == 0) {
        return Err(DeriveError::Empty);
    }

    // The entries every depth must keep: a directory at each depth above the deepest, so that
    // there is a parent for the depth below, and one entry at the deepest.
    let mut minimum = vec![[0_u64; 3]; depth];
    for level in minimum.iter_mut().take(depth - 1) {
        level[0] = 1;
    }
    let deepest = &source[depth - 1];
    let largest = (0..3).rev().max_by_key(|kind| deepest[*kind]).unwrap_or(0);
    minimum[depth - 1][largest] = 1;

    let below = request.entries - 1;
    let reserved = depth as u64;
    if below < reserved {
        return Err(DeriveError::TooSmall {
            depth: kept,
            minimum: reserved + 1,
            entries: request.entries,
        });
    }
    let weights: Vec<u64> = source.iter().flatten().copied().collect();
    let shares = apportion(below - reserved, &weights);
    let mut levels = Vec::with_capacity(depth);
    for (index, minimum) in minimum.iter().enumerate() {
        let at = |kind: usize| {
            u32::try_from(minimum[kind] + shares[index * 3 + kind]).unwrap_or(u32::MAX)
        };
        levels.push(ShapedLevel {
            directories: at(0),
            files: at(1),
            symlinks: at(2),
        });
    }

    let part = shaped_part(profile, request, levels);
    let spec = FixtureSpec {
        schema_version: SPEC_SCHEMA_VERSION,
        id: request.id.clone(),
        description: describe(profile, request, kept),
        seed: request.seed,
        parts: vec![Part::Shaped(part)],
    };
    spec.validate()?;
    // The plan is what refuses a link whose target no file system could hold.
    Plan::new(&spec)?;
    Ok(spec)
}

/// `buckets` without `leaves` directories that hold nothing, which are among its zeros. Left as it
/// is when taking them out would leave nothing to draw from.
fn without_leaves(buckets: &Buckets, leaves: u64) -> Buckets {
    let kept: Buckets = buckets
        .iter()
        .map(|(key, count)| {
            (
                key,
                if key == 0 {
                    count.saturating_sub(leaves)
                } else {
                    count
                },
            )
        })
        .filter(|(_, count)| *count > 0)
        .collect();
    if kept.is_empty() {
        buckets.clone()
    } else {
        kept
    }
}

/// The part: the scaled levels, and the histograms and link shares of the profile.
fn shaped_part(
    profile: &HarnessShapeProfile,
    request: &SpecRequest,
    levels: Vec<ShapedLevel>,
) -> ShapedPart {
    // The directories at the deepest depth that holds any directory have no subdirectory, and the
    // directories at the deepest depth that holds anything have no file: they are zeros in the
    // histograms of what a directory holds, by what those depths are. The part draws a share from
    // such a histogram only for the directories above those depths, so they are taken out; the
    // tree would otherwise have more empty directories than the profile, wherever the weights
    // fall. A profile cut at the part's depth limit has what lies below it in the directories at
    // the limit, and is left as it is.
    let (without_subdirectories, without_files) = if profile.max_depth <= MAX_SHAPED_DEPTH {
        (
            profile
                .depths
                .iter()
                .rev()
                .find(|level| level.directories > 0)
                .map_or(0, |level| level.directories),
            profile.depths.last().map_or(0, |level| level.directories),
        )
    } else {
        (0, 0)
    };
    let mut part = ShapedPart {
        root: SHAPED_ROOT.to_owned(),
        max_file_bytes: request.max_file_bytes,
        levels,
        dangling_symlinks: 0,
        subdirectories_per_directory: without_leaves(
            &profile.subdirectories_per_directory.buckets,
            without_subdirectories,
        ),
        files_per_directory: without_leaves(&profile.files_per_directory.buckets, without_files),
        file_sizes: profile.file_sizes.buckets.clone(),
        directory_name_lengths: profile.name_lengths.directories.buckets.clone(),
        file_name_lengths: profile.name_lengths.files.buckets.clone(),
        symlink_name_lengths: profile.name_lengths.symlinks.buckets.clone(),
        hard_links: Vec::new(),
    };

    part.hard_links = hard_link_classes(profile, &part);
    // A profile does not say where a link pointed: the walk never asks. So every symbolic link of a
    // spec points at a file of the part, by the rule of the part, and none dangles; a part with no
    // file has nothing to point at, and its links point at nothing.
    part.dangling_symlinks = if part.file_count() == 0 {
        u32::try_from(part.symlink_count()).unwrap_or(u32::MAX)
    } else {
        0
    };
    part
}

/// The groups of hard links of `part`, from the profile's groups by the class of the size of
/// their file.
///
/// A class of the profile lands in the class of its size after the part's cut at `max_file_bytes`
/// (the classes above it are one). For each such class that had groups, the files it had and the
/// files the part plans in it give the scale, and the profile's groups in it are scaled by it:
/// each size of group by its count, rounded like every other count of the spec. The names of the
/// class's groups are scaled the same way and carried with the groups, so a profile of four
/// groups of four names builds four groups of four, and not groups of whatever size the classes
/// of the histogram allow.
///
/// A group is a file with at least two names, so the profile's groups of one name (a file whose
/// other names were outside the tree) are plain files. The names of a class's groups are among
/// the files of the class, so when the files of a class are fewer than its groups need, groups
/// are left out, the largest first, until they fit.
fn hard_link_classes(profile: &HarnessShapeProfile, part: &ShapedPart) -> Vec<HardLinkClass> {
    let landing = |class: u64| class_floor(class.min(part.max_file_bytes));

    // What the profile had in each class of the part: its files, the sizes of its groups of two
    // names or more, and the names those have.
    let mut had: BTreeMap<u64, (u64, Buckets, u64)> = BTreeMap::new();
    for (class, files) in profile.file_sizes.buckets.iter() {
        had.entry(landing(class)).or_default().0 += files;
    }
    for (class, histogram) in profile.hard_links.group_sizes_by_file_size.iter() {
        let entry = had.entry(landing(class)).or_default();
        for (size, count) in histogram.buckets.iter().filter(|(size, _)| *size >= 2) {
            entry.1.add(size, count);
        }
        // A group of one name has one name, so the rest are the names of groups of two or more.
        entry.2 += histogram.total.saturating_sub(histogram.buckets.get(1));
    }

    let planned: BTreeMap<u64, u64> = part.size_classes().into_iter().collect();
    let mut classes = Vec::new();
    for (class, (files, sizes, names)) in had {
        let room = planned.get(&class).copied().unwrap_or(0);
        if files == 0 || sizes.is_empty() || room < 2 {
            continue;
        }
        let mut entry = HardLinkClass {
            class,
            names: 0,
            group_sizes: sizes
                .iter()
                .map(|(size, count)| (size, scaled(count, room, files)))
                .filter(|(_, count)| *count > 0)
                .collect(),
        };
        // Groups the files of the class have no room for go, the largest first.
        while entry.names_range().0 > u128::from(room) {
            let Some((size, _)) = entry.group_sizes.iter().last() else {
                break;
            };
            entry.group_sizes.take(size, 1);
        }
        if entry.group_sizes.is_empty() {
            continue;
        }
        let (least, most) = entry.names_range();
        let wanted = u128::from(scaled(names, room, files));
        entry.names =
            u64::try_from(wanted.clamp(least, most.min(u128::from(room)))).unwrap_or(u64::MAX);
        classes.push(entry);
    }
    classes
}

/// What the spec says it is: numbers only.
fn describe(profile: &HarnessShapeProfile, request: &SpecRequest, kept: u32) -> String {
    let entries = &profile.entries;
    let folded = if profile.max_depth > kept {
        format!(" Everything deeper than {kept} levels is at {kept}.")
    } else {
        String::new()
    };
    format!(
        "Shaped like a profile of {} entries ({} folders, {} files, {} symbolic links) {} levels \
         deep, scaled to {} entries by excise-shape.{folded}",
        entries.total,
        entries.directories,
        entries.files,
        entries.symlinks,
        profile.max_depth,
        request.entries
    )
}

/// The comment that opens a spec written by [`render_spec`].
fn header(id: &str) -> String {
    format!(
        "# A fixture specification built by `excise-shape spec` from a shape profile.\n\
         #\n\
         # It holds aggregates only: how many entries there are at each depth, and histograms of\n\
         # what a folder holds, how large a file is, and how long a name is. No name, path,\n\
         # owner, or timestamp of the tree that was profiled is in it.\n\
         #\n\
         # To use it, keep it in a directory of fixture specs, named `{id}.toml`, and run\n\
         #   cargo xtask headless --fixture-dir <that directory> --fixture {id}\n\n"
    )
}

/// The text of `spec` as a spec file: TOML, under a comment that says where it came from.
///
/// # Errors
///
/// Returns the error of writing TOML.
pub fn render_spec(spec: &FixtureSpec) -> Result<String, toml::ser::Error> {
    Ok(format!("{}{}", header(&spec.id), toml::to_string(spec)?))
}
