//! The `harness-shape-profile` document: the aggregates of one tree's shape.
//!
//! `excise-shape profile` walks a tree and writes this document so that a benchmark or a scenario
//! can behave like the tree without anyone running Excise on it, and so that a performance report
//! can describe a slow tree without showing it. It therefore holds **aggregates only**: counts,
//! sizes in classes, name lengths, and the shape of the tree by depth. It never holds a name, a
//! path, a link target, an owner, or a timestamp, and the types below have no field that could.
//!
//! Every histogram is a [`ShapeHistogram`]: how many values it counts, their sum and their
//! largest, and the [`Buckets`] they fell in. The fields that hold sizes and counts of children
//! use power-of-two classes, and the fields that hold name lengths keep every length exactly (see
//! [`crate::histogram`]). A count is written as a whole number that every JSON consumer reads
//! back exactly (at most [`MAX_COUNT`]); the sum of a histogram and its largest value are not
//! bounded that way, because a sparse file can be larger than 2^53 bytes.

use std::{collections::BTreeMap, fmt};

use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{self, MapAccess, Visitor},
};

use super::{Document, MAX_COUNT, SchemaVersion};
use crate::{
    histogram::{BucketKind, Buckets, parse_bucket_key},
    string_enum::string_enum,
};

/// The deepest level a profile may describe: the walk goes no deeper, and a document with more
/// levels is refused.
pub const MAX_PROFILE_DEPTH: u32 = 4096;

string_enum! {
    /// The value of a shape profile's `document_kind` field.
    #[derive(Default)]
    pub enum ShapeProfileKind {
        /// The only kind a shape profile can have.
        #[default]
        HarnessShapeProfile => "harness-shape-profile",
    }
}

/// Where the profile was taken, and what that platform could tell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShapePlatform {
    /// The operating system, as in `std::env::consts::OS`.
    pub os: String,
    /// Whether the platform layer reported a device and an inode for every entry. Without them
    /// (Windows) the walk cannot tell a mount point from a folder or find a hard link, so it
    /// reports no mount points and no hard links.
    pub identity: bool,
}

/// How the walk went about the file systems it met.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShapeWalk {
    /// Whether the walk was asked to cross into other file systems. By default it stays on the
    /// file system of the root.
    pub cross_filesystems: bool,
    /// The folders on another file system that the walk counted and did not enter. Always 0 when
    /// `cross_filesystems` is true.
    pub mount_points_skipped: u64,
}

/// The entries below the root, by kind. The root itself is not counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShapeEntries {
    /// All of them: the four counts below added up.
    pub total: u64,
    /// Folders, mount points and folders that could not be listed included.
    pub directories: u64,
    /// Regular files, every name of a hard-linked file counted.
    pub files: u64,
    /// Symbolic links below the root, which the walk never follows.
    pub symlinks: u64,
    /// Everything else: sockets, FIFOs, devices.
    pub others: u64,
}

/// The entries at one depth. Depth 1 is what the root directly holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShapeDepth {
    /// The depth, counted from 1 in the order of the list.
    pub depth: u32,
    /// Folders at this depth.
    pub directories: u64,
    /// Regular files at this depth.
    pub files: u64,
    /// Symbolic links at this depth.
    pub symlinks: u64,
    /// Everything else at this depth.
    pub others: u64,
    /// The apparent size (`st_size`) of the regular files at this depth, every name counted.
    pub bytes: u64,
}

/// A histogram: the values it counts, summed up, and in buckets.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShapeHistogram {
    /// How many values it counts: the sum of the bucket counts.
    pub count: u64,
    /// The values added up. A sum too large for 64 bits stops at the largest number a 64-bit
    /// integer holds.
    pub total: u64,
    /// The largest value, 0 when there is none.
    pub max: u64,
    /// The buckets: the smallest value of each, and how many values it holds. What a bucket
    /// spans depends on the histogram: a class of powers of two, or one length.
    pub buckets: Buckets,
}

/// The lengths of names in bytes, by what the name belongs to. Each bucket is one length from 1 to
/// 255; a longer name, which some file systems allow, is counted as 255.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShapeNameLengths {
    /// The names of folders.
    pub directories: ShapeHistogram,
    /// The names of regular files.
    pub files: ShapeHistogram,
    /// The names of symbolic links.
    pub symlinks: ShapeHistogram,
}

/// Histograms by class: the smallest value of each class, and the histogram of the values that
/// belong to it. In JSON the keys are written as strings of digits, the way a map names its
/// members, and read back from them, each in the one spelling `to_string` gives a number.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClassHistograms(BTreeMap<u64, ShapeHistogram>);

impl ClassHistograms {
    /// No class at all.
    #[must_use]
    pub const fn new() -> Self {
        Self(BTreeMap::new())
    }

    /// Sets the histogram of `class`, and returns the one it replaces.
    pub fn insert(&mut self, class: u64, histogram: ShapeHistogram) -> Option<ShapeHistogram> {
        self.0.insert(class, histogram)
    }

    /// The classes in ascending order, with their histograms.
    pub fn iter(&self) -> impl Iterator<Item = (u64, &ShapeHistogram)> {
        self.0.iter().map(|(class, histogram)| (*class, histogram))
    }

    /// The histogram of `class`, if it has one.
    #[must_use]
    pub fn get(&self, class: u64) -> Option<&ShapeHistogram> {
        self.0.get(&class)
    }

    /// How many classes have a histogram.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether no class has a histogram.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl FromIterator<(u64, ShapeHistogram)> for ClassHistograms {
    fn from_iter<T: IntoIterator<Item = (u64, ShapeHistogram)>>(pairs: T) -> Self {
        Self(pairs.into_iter().collect())
    }
}

impl Serialize for ClassHistograms {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_map(self.0.iter())
    }
}

impl<'de> Deserialize<'de> for ClassHistograms {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ClassesVisitor;

        impl<'de> Visitor<'de> for ClassesVisitor {
            type Value = ClassHistograms;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a map from class numbers to histograms")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<ClassHistograms, A::Error> {
                let mut classes = BTreeMap::new();
                while let Some((key, histogram)) = map.next_entry::<String, ShapeHistogram>()? {
                    let class = parse_bucket_key(&key).map_err(de::Error::custom)?;
                    if classes.insert(class, histogram).is_some() {
                        return Err(de::Error::custom(format!(
                            "the class {class} appears twice"
                        )));
                    }
                }
                Ok(ClassHistograms(classes))
            }
        }

        deserializer.deserialize_map(ClassesVisitor)
    }
}

/// The regular files that have more than one name.
///
/// The names of a file are one file: they have one size, and so lie in one class of the sizes in
/// [`HarnessShapeProfile::file_sizes`]. That histogram counts every name, so the names of the
/// groups of a class are among the files of that class. `group_sizes_by_file_size` says in which
/// class each group lay, which is what a spec built from the profile needs to place groups
/// without guessing: the one histogram of group sizes does not say how large the files were.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShapeHardLinks {
    /// The files whose link count is above 1, each counted once however many names it has.
    pub groups: u64,
    /// The names of those files that the walk found.
    pub names: u64,
    /// Of the groups, those with names the walk did not find, because the link count the file
    /// system reports is above the names found: the other names are outside the walk.
    pub incomplete_groups: u64,
    /// The names found per group, in classes of powers of two. A group of size 1 is a file whose
    /// other names are all outside the walk.
    pub group_sizes: ShapeHistogram,
    /// The same groups by the class of the size of their file: for each class of file size that
    /// holds any, the names found per group in classes, as in `group_sizes`. A group is in the
    /// class of the size its first name had when the walk met it, and a name met with another
    /// size (a file that changed under the walk) is not a name of the group. The histograms add
    /// up to `group_sizes`, and the names of a class are at most the files of that class.
    pub group_sizes_by_file_size: ClassHistograms,
}

/// The symbolic links: how many there are, and nothing of where they point. A link is an entry of
/// its own and the walk goes no further than the link, so whether its target exists is not asked,
/// and the profile does not say.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShapeSymbolicLinks {
    /// All of them.
    pub count: u64,
}

/// What the walk could not read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShapeUnreadable {
    /// Folders that could not be opened or listed because of their permissions. Counted as
    /// folders, and absent from the histograms of what a folder holds.
    pub directories: u64,
    /// Entries and folders that failed for another reason: gone before the walk reached them, on
    /// Unix a folder that was not the one inspected when the walk opened it (it changed in
    /// between; on Windows the walk cannot tell one ordinary folder from another), an input or
    /// output error, a folder deeper than [`MAX_PROFILE_DEPTH`], or a folder that is its own
    /// ancestor (a bind mount can make one), which is counted and not entered.
    pub errors: u64,
}

/// The shape of one tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessShapeProfile {
    /// Always `harness-shape-profile`.
    pub document_kind: ShapeProfileKind,
    /// Always the current schema version.
    pub schema_version: SchemaVersion,
    /// Where it was taken.
    pub platform: ShapePlatform,
    /// How the walk treated file systems.
    pub walk: ShapeWalk,
    /// The entries below the root, by kind.
    pub entries: ShapeEntries,
    /// The deepest depth that holds an entry; 0 for an empty tree. It equals the length of
    /// `depths`.
    pub max_depth: u32,
    /// The entries and bytes at each depth from 1 to `max_depth`.
    pub depths: Vec<ShapeDepth>,
    /// The entries a folder holds, of every kind, for each folder that was listed (the root
    /// included): in classes of powers of two.
    pub children_per_directory: ShapeHistogram,
    /// The folders a folder holds, for each folder that was listed: in classes.
    pub subdirectories_per_directory: ShapeHistogram,
    /// The regular files a folder holds, for each folder that was listed: in classes.
    pub files_per_directory: ShapeHistogram,
    /// The apparent size (`st_size`) of each regular file, every name counted: in classes. Its
    /// sum is the bytes of the tree.
    pub file_sizes: ShapeHistogram,
    /// The lengths of names in bytes, exactly.
    pub name_lengths: ShapeNameLengths,
    /// Files with more than one name.
    pub hard_links: ShapeHardLinks,
    /// Symbolic links.
    pub symbolic_links: ShapeSymbolicLinks,
    /// What could not be read.
    pub unreadable: ShapeUnreadable,
}

impl Document for HarnessShapeProfile {
    const KIND: &'static str = ShapeProfileKind::HarnessShapeProfile.as_str();
    const SCHEMA_ID: &'static str =
        "https://github.com/findyourexit/excise/harness/schemas/harness-shape-profile-v1.json";
    const SCHEMA_JSON: &'static str =
        include_str!("../../schemas/harness-shape-profile.schema.json");
}

/// Every rule a shape profile breaks that its JSON Schema cannot say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShapeProblems(pub Vec<String>);

impl std::fmt::Display for ShapeProblems {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (index, problem) in self.0.iter().enumerate() {
            if index > 0 {
                formatter.write_str("; ")?;
            }
            formatter.write_str(problem)?;
        }
        Ok(())
    }
}

impl std::error::Error for ShapeProblems {}

impl HarnessShapeProfile {
    /// Checks that the numbers of a profile agree with each other, which the JSON Schema cannot
    /// say: the kinds add up to the total, the depths add up to the kinds, every histogram counts
    /// what it says and keeps its sum and its largest value within its buckets, and what a folder
    /// holds adds up to the entries. A profile from anywhere, a report attached to an issue for
    /// one, is held to this before a field of it is used.
    ///
    /// # Errors
    ///
    /// Returns every rule the document breaks.
    pub fn check(&self) -> Result<(), ShapeProblems> {
        let mut problems = Vec::new();
        let mut problem = |message: String| problems.push(message);

        self.check_entries(&mut problem);
        self.check_depths(&mut problem);
        self.check_histograms(&mut problem);
        self.check_links(&mut problem);

        if problems.is_empty() {
            Ok(())
        } else {
            Err(ShapeProblems(problems))
        }
    }

    fn check_entries(&self, problem: &mut impl FnMut(String)) {
        let entries = &self.entries;
        let kinds = [
            u128::from(entries.directories),
            u128::from(entries.files),
            u128::from(entries.symlinks),
            u128::from(entries.others),
        ];
        if kinds.iter().sum::<u128>() != u128::from(entries.total) {
            problem(format!(
                "`entries.total` is {}, not the sum of the kinds",
                entries.total
            ));
        }
        for (name, value) in [
            ("total", entries.total),
            ("directories", entries.directories),
            ("files", entries.files),
            ("symlinks", entries.symlinks),
            ("others", entries.others),
        ] {
            if value > MAX_COUNT {
                problem(format!("`entries.{name}` is above {MAX_COUNT}"));
            }
        }
        if self.walk.cross_filesystems && self.walk.mount_points_skipped != 0 {
            problem("a walk that crosses file systems skipped no mount point".to_owned());
        }
        if self.walk.mount_points_skipped > entries.directories {
            problem("more mount points were skipped than there are folders".to_owned());
        }
        if !self.platform.identity
            && (self.walk.mount_points_skipped != 0 || self.hard_links.groups != 0)
        {
            problem(
                "a platform without identities finds no mount point and no hard link".to_owned(),
            );
        }
    }

    fn check_depths(&self, problem: &mut impl FnMut(String)) {
        if self.max_depth > MAX_PROFILE_DEPTH {
            problem(format!("`max_depth` is above {MAX_PROFILE_DEPTH}"));
            return;
        }
        if usize::try_from(self.max_depth).ok() != Some(self.depths.len()) {
            problem(format!(
                "`max_depth` is {} and `depths` has {} levels",
                self.max_depth,
                self.depths.len()
            ));
            return;
        }
        let mut sums = [0_u128; 5];
        let mut parents = 1_u64; // the root
        for (index, level) in self.depths.iter().enumerate() {
            if usize::try_from(level.depth).ok() != Some(index + 1) {
                problem(format!(
                    "the level at position {} says it is depth {}",
                    index + 1,
                    level.depth
                ));
            }
            let held = [level.directories, level.files, level.symlinks, level.others];
            if held.iter().all(|value| *value == 0) {
                problem(format!("depth {} holds nothing", level.depth));
            }
            if parents == 0 {
                problem(format!(
                    "depth {} holds entries below a depth with no folder",
                    level.depth
                ));
            }
            parents = level.directories;
            for (sum, value) in sums.iter_mut().zip(held.into_iter().chain([level.bytes])) {
                *sum += u128::from(value);
            }
        }
        let entries = &self.entries;
        for (name, sum, value) in [
            ("directories", sums[0], entries.directories),
            ("files", sums[1], entries.files),
            ("symlinks", sums[2], entries.symlinks),
            ("others", sums[3], entries.others),
        ] {
            if sum != u128::from(value) {
                problem(format!(
                    "the depths hold {sum} {name} and `entries.{name}` is {value}"
                ));
            }
        }
        let bytes = u128::from(self.file_sizes.total);
        if sums[4].min(u128::from(u64::MAX)) != bytes {
            problem(format!(
                "the depths hold {} bytes and `file_sizes.total` is {bytes}",
                sums[4]
            ));
        }
    }

    fn check_histograms(&self, problem: &mut impl FnMut(String)) {
        let entries = &self.entries;
        let lengths = &self.name_lengths;
        for (name, histogram, kind) in [
            (
                "children_per_directory",
                &self.children_per_directory,
                BucketKind::Class,
            ),
            (
                "subdirectories_per_directory",
                &self.subdirectories_per_directory,
                BucketKind::Class,
            ),
            (
                "files_per_directory",
                &self.files_per_directory,
                BucketKind::Class,
            ),
            ("file_sizes", &self.file_sizes, BucketKind::Class),
            (
                "name_lengths.directories",
                &lengths.directories,
                BucketKind::Length,
            ),
            ("name_lengths.files", &lengths.files, BucketKind::Length),
            (
                "name_lengths.symlinks",
                &lengths.symlinks,
                BucketKind::Length,
            ),
            (
                "hard_links.group_sizes",
                &self.hard_links.group_sizes,
                BucketKind::Class,
            ),
        ] {
            check_histogram(problem, name, histogram, kind);
        }

        let listed = self.children_per_directory.count;
        for (name, histogram) in [
            (
                "subdirectories_per_directory",
                &self.subdirectories_per_directory,
            ),
            ("files_per_directory", &self.files_per_directory),
        ] {
            if histogram.count != listed {
                problem(format!(
                    "`{name}` counts {} folders and `children_per_directory` counts {listed}",
                    histogram.count
                ));
            }
        }
        if listed > entries.directories.saturating_add(1) {
            problem("more folders were listed than the tree has".to_owned());
        }
        for (name, histogram, expected) in [
            (
                "children_per_directory",
                &self.children_per_directory,
                entries.total,
            ),
            (
                "subdirectories_per_directory",
                &self.subdirectories_per_directory,
                entries.directories,
            ),
            (
                "files_per_directory",
                &self.files_per_directory,
                entries.files,
            ),
        ] {
            if histogram.total != expected {
                problem(format!(
                    "`{name}` adds up to {} and the tree has {expected}",
                    histogram.total
                ));
            }
        }
        for (name, counted, expected) in [
            ("file_sizes", self.file_sizes.count, entries.files),
            (
                "name_lengths.directories",
                lengths.directories.count,
                entries.directories,
            ),
            ("name_lengths.files", lengths.files.count, entries.files),
            (
                "name_lengths.symlinks",
                lengths.symlinks.count,
                entries.symlinks,
            ),
        ] {
            if counted != expected {
                problem(format!(
                    "`{name}` counts {counted} and the tree has {expected}"
                ));
            }
        }
    }

    fn check_links(&self, problem: &mut impl FnMut(String)) {
        let hard = &self.hard_links;
        if hard.group_sizes.count != hard.groups {
            problem(format!(
                "`hard_links.groups` is {} and `group_sizes` counts {}",
                hard.groups, hard.group_sizes.count
            ));
        }
        if hard.group_sizes.total != hard.names {
            problem(format!(
                "`hard_links.names` is {} and `group_sizes` adds up to {}",
                hard.names, hard.group_sizes.total
            ));
        }
        if hard.incomplete_groups > hard.groups {
            problem("more hard-link groups are incomplete than there are groups".to_owned());
        }
        if hard.names > self.entries.files {
            problem("more names of linked files than there are files".to_owned());
        }
        self.check_hard_link_classes(problem);
        if self.symbolic_links.count != self.entries.symlinks {
            problem(format!(
                "`symbolic_links.count` is {} and `entries.symlinks` is {}",
                self.symbolic_links.count, self.entries.symlinks
            ));
        }
        for (name, value) in [
            ("hard_links.groups", hard.groups),
            ("hard_links.names", hard.names),
            ("hard_links.incomplete_groups", hard.incomplete_groups),
            ("symbolic_links.count", self.symbolic_links.count),
            ("unreadable.directories", self.unreadable.directories),
            ("unreadable.errors", self.unreadable.errors),
            ("walk.mount_points_skipped", self.walk.mount_points_skipped),
        ] {
            if value > MAX_COUNT {
                problem(format!("`{name}` is above {MAX_COUNT}"));
            }
        }
    }

    /// The groups of hard links by the class of the size of their file: every class is one a
    /// size can be in, every histogram holds a group and is consistent, the histograms add up to
    /// `group_sizes`, and the names of a class are among the files of that class, which
    /// `file_sizes` counts name by name.
    fn check_hard_link_classes(&self, problem: &mut impl FnMut(String)) {
        let hard = &self.hard_links;
        let mut groups: u128 = 0;
        let mut names: u128 = 0;
        let mut largest: u64 = 0;
        let mut merged = Buckets::new();
        for (class, histogram) in hard.group_sizes_by_file_size.iter() {
            let name = format!("hard_links.group_sizes_by_file_size[{class}]");
            if !BucketKind::Class.admits(class) {
                problem(format!(
                    "`hard_links.group_sizes_by_file_size` has no class {class}"
                ));
                continue;
            }
            check_histogram(problem, &name, histogram, BucketKind::Class);
            if histogram.count == 0 {
                problem(format!("`{name}` holds no group: leave the class out"));
            }
            let files = self.file_sizes.buckets.get(class);
            if histogram.total > files {
                problem(format!(
                    "`{name}` has {} names and `file_sizes` has {files} files in the class",
                    histogram.total
                ));
            }
            groups += u128::from(histogram.count);
            names += u128::from(histogram.total);
            largest = largest.max(histogram.max);
            for (size, count) in histogram.buckets.iter() {
                merged.add(size, count);
            }
        }
        if groups != u128::from(hard.groups) {
            problem(format!(
                "`hard_links.groups` is {} and the classes of `group_sizes_by_file_size` count \
                 {groups}",
                hard.groups
            ));
        }
        if names.min(u128::from(u64::MAX)) != u128::from(hard.names) {
            problem(format!(
                "`hard_links.names` is {} and the classes of `group_sizes_by_file_size` add up \
                 to {names}",
                hard.names
            ));
        }
        if merged != hard.group_sizes.buckets {
            problem(
                "the classes of `hard_links.group_sizes_by_file_size` do not add up to the \
                 buckets of `group_sizes`"
                    .to_owned(),
            );
        }
        if largest != hard.group_sizes.max {
            problem(format!(
                "the largest group of `group_sizes_by_file_size` is {largest} names and \
                 `group_sizes` says {}",
                hard.group_sizes.max
            ));
        }
    }
}

/// Checks one histogram: its buckets are of the right kind, its count is the sum of them, and its
/// sum and largest value are possible for them.
fn check_histogram(
    problem: &mut impl FnMut(String),
    name: &str,
    histogram: &ShapeHistogram,
    kind: BucketKind,
) {
    let mut low: u128 = 0;
    let mut high: u128 = 0;
    for (key, count) in histogram.buckets.iter() {
        if !kind.admits(key) {
            problem(format!("`{name}` has no bucket {key}"));
        }
        if count == 0 {
            problem(format!("the bucket {key} of `{name}` is empty"));
        }
        if count > MAX_COUNT {
            problem(format!("the bucket {key} of `{name}` is above {MAX_COUNT}"));
        }
        let (floor, ceiling) = kind.bounds(key);
        low += u128::from(floor) * u128::from(count);
        high += u128::from(ceiling) * u128::from(count);
    }
    if histogram.buckets.count() != u128::from(histogram.count) {
        problem(format!(
            "`{name}` counts {} and its buckets hold {}",
            histogram.count,
            histogram.buckets.count()
        ));
    }
    if histogram.count > MAX_COUNT {
        problem(format!("`{name}` counts more than {MAX_COUNT}"));
    }
    // A sum that overflows 64 bits stops at the largest 64-bit number.
    let cap = u128::from(u64::MAX);
    let total = u128::from(histogram.total);
    if total < low.min(cap) || total > high.min(cap) {
        problem(format!(
            "`{name}` adds up to {total}, which its buckets cannot hold"
        ));
    }
    match histogram.buckets.last_key() {
        None => {
            if histogram.max != 0 {
                problem(format!("`{name}` is empty and its largest value is not 0"));
            }
        }
        Some(key) => {
            let (floor, ceiling) = kind.bounds(key);
            if histogram.max < floor || histogram.max > ceiling {
                problem(format!(
                    "the largest value of `{name}` is not in its last bucket"
                ));
            }
        }
    }
}
