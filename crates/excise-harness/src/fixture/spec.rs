//! The fixture specification: a TOML file that composes class generators.
//!
//! One file per fixture, named `<id>.toml`, in this crate's `fixtures/` directory. The `id` is
//! the value a scenario puts in its `fixture` field. A spec has a default seed and one or more
//! parts. Each part is one class generator that fills one top-level entry of the fixture, named
//! by the part's `root`:
//!
//! ```toml
//! schema_version = 1
//! id = "node-modules-2k"
//! description = "The node_modules-shaped tree behind the scan-starvation finding."
//! seed = 1
//!
//! [[parts]]
//! kind = "tree"
//! root = "node_modules"
//! depth = 4
//! fanout = 4
//! files_per_dir = 8
//! file_placement = "leaves"
//! dir_names = { style = "sequential", prefix = "pkg" }
//! file_names = { style = "sequential", prefix = "m", suffix = ".js" }
//! file_size = 1
//! ```
//!
//! Parsing rejects unknown fields everywhere, and loading applies [`FixtureSpec::validate`], so a
//! spec that names an absurd size or a colliding root never reaches the generator. A spec file
//! is not trusted to be one (a directory of specs of one's own can hold anything): it is opened
//! without following a link, must be a regular file, and is read up to [`MAX_SPEC_BYTES`].

use std::{
    fmt, fs, io,
    io::Read as _,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use thiserror::Error;

use crate::{
    fixture::{
        MARKER_FILE_NAME,
        marker::TEMPORARY_NAME,
        names::{MAX_NAME_BYTES, NameStyle, decimal_digits, hex_digits_for},
        rng::SplitMix64,
        sys::open_regular_file,
    },
    histogram::{BucketKind, Buckets, apportion, class_floor, spread_counts},
    scenario::{MAX_IDENTIFIER_LEN, check_fixture_relative_path},
    string_enum::string_enum,
};

/// The only spec `schema_version` this crate reads.
pub const SPEC_SCHEMA_VERSION: u32 = 1;

/// The longest path, in bytes, that a path-based removal can be handed on every platform the
/// harness runs on: 1,023. `PATH_MAX` counts the terminating NUL, and is 1,024 on macOS, the
/// lowest of those platforms (it is 4,096 on Linux).
///
/// A path-based removal (`cargo clean`, `git worktree remove`) is handed the whole path, from the
/// root of the file system, and fails on one that is longer. The path of the fixture root is above
/// every path of the fixture and takes part of the limit, so what a fixture may use depends on
/// where it is: see [`FixtureSpec::removable_by_path`].
pub const MAX_PORTABLE_PATH_BYTES: u64 = 1_023;

/// The most entries one spec may plan. The plan is held in memory, so a fixture much larger than
/// the weekly tier's `tiny-files-10m` (10,010,101 entries) needs a generator that streams its plan.
pub const MAX_ENTRIES: u64 = 10_100_000;

/// The most bytes a spec file may hold, 1 MiB. The largest spec this crate ships is under 1.5
/// KiB and a spec that `excise-shape spec` writes is a few tens of KiB at most, so a file above
/// the cap is not a spec: it is refused without being read in full (see
/// [`FixtureSpec::from_path`]).
pub const MAX_SPEC_BYTES: u64 = 1 << 20;

/// The largest dense (fully written) file a spec may ask for: 1 GiB.
pub const MAX_DENSE_FILE_BYTES: u64 = 1 << 30;

/// The largest apparent size of a sparse file a spec may ask for: 16 GiB.
pub const MAX_SPARSE_MIB: u64 = 16 * 1024;

/// The deepest `tree` part.
pub const MAX_TREE_DEPTH: u32 = 32;

/// The deepest `shaped` part: the same limit as a `tree` part.
pub const MAX_SHAPED_DEPTH: u32 = MAX_TREE_DEPTH;

/// The default `max_file_bytes` of a `shaped` part, and what `excise-shape spec` uses: 16 KiB.
pub const DEFAULT_MAX_FILE_BYTES: u64 = 16 * 1024;

/// The deepest `deep` part, in directories.
pub const MAX_DEEP_DEPTH: u32 = 512;

/// The smallest and largest size of a `volume` part, in MiB.
pub const VOLUME_MIB_RANGE: std::ops::RangeInclusive<u32> = 8..=1024;

/// One fixture specification.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureSpec {
    /// The format version; must be [`SPEC_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// The fixture identifier: 1 to 64 lowercase ASCII letters, digits, `-`, or `_`, starting
    /// with a letter or digit. It is the file name and the value of a scenario's `fixture`.
    pub id: String,
    /// What the fixture is for. Not part of the spec hash.
    pub description: String,
    /// The default seed. Names and sizes are a pure function of the spec and the seed.
    pub seed: u64,
    /// The generators, at least one. Each fills one top-level entry.
    pub parts: Vec<Part>,
}

/// One class generator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Part {
    /// A directory tree with a fixed fan-out: the wide, `node_modules`-shaped, and many-tiny-files
    /// shapes of the scale class.
    Tree(TreePart),
    /// A chain of nested directories deeper than `PATH_MAX`: the depth shape of the scale class.
    Deep(DeepPart),
    /// One regular file at the top level: a sentinel or a single large file.
    File(FilePart),
    /// The identity class: hard links, symbolic links, sparse files, and clones.
    Identity(IdentityPart),
    /// The hostile class: hostile names and unreadable entries.
    Hostile(HostilePart),
    /// A tree drawn from the shape of another: what a profile of a real tree measured, scaled to
    /// a size the generator can build.
    Shaped(ShapedPart),
    /// The volume class: a mount point that a run copy can attach a size-limited volume to.
    Volume(VolumePart),
}

impl Part {
    /// The name of the top-level entry this part creates.
    #[must_use]
    pub fn root(&self) -> &str {
        match self {
            Self::Tree(part) => &part.root,
            Self::Deep(part) => &part.root,
            Self::File(part) => &part.root,
            Self::Identity(part) => &part.root,
            Self::Hostile(part) => &part.root,
            Self::Shaped(part) => &part.root,
            Self::Volume(part) => &part.root,
        }
    }

    /// The value of the part's `kind` field.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Tree(_) => "tree",
            Self::Deep(_) => "deep",
            Self::File(_) => "file",
            Self::Identity(_) => "identity",
            Self::Hostile(_) => "hostile",
            Self::Volume(_) => "volume",
            Self::Shaped(_) => "shaped",
        }
    }

    /// An upper bound on the length in bytes of the longest path the part plans, from the fixture
    /// root, worked out from the fields of the part alone, without planning it: the plan never has
    /// a longer one, whatever the seed. It is the longest path itself for every kind but
    /// `identity` and `shaped`, which can place their longest names in different folders. A
    /// path-based removal can take a fixture only if its parts' paths fit below the cache root:
    /// see [`FixtureSpec::removable_by_path`].
    #[must_use]
    pub fn longest_path_bytes(&self) -> u64 {
        match self {
            Self::Tree(part) => part.longest_path_bytes(),
            Self::Deep(part) => part.longest_path_bytes(),
            Self::File(part) => part.longest_path_bytes(),
            Self::Identity(part) => part.longest_path_bytes(),
            Self::Hostile(part) => part.longest_path_bytes(),
            Self::Shaped(part) => part.longest_path_bytes(),
            Self::Volume(part) => part.longest_path_bytes(),
        }
    }
}

string_enum! {
    /// Which directories of a tree receive files.
    #[derive(Default)]
    pub enum FilePlacement {
        /// Every directory, the root included.
        #[default]
        All => "all",
        /// Only the directories at the deepest level. The shape of `node_modules`, where the files
        /// live in the leaf packages.
        Leaves => "leaves",
    }
}

/// How large a generated file is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SizeSpec {
    /// Every file has exactly this many bytes: `file_size = 1`.
    Fixed(u64),
    /// Each file draws its size from the seed, uniformly and inclusively:
    /// `file_size = { min = 1, max = 64 }`.
    Range(SizeRange),
}

/// An inclusive range of file sizes in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SizeRange {
    /// The smallest size.
    pub min: u64,
    /// The largest size.
    pub max: u64,
}

impl SizeSpec {
    /// Draws one size.
    pub(crate) fn draw(self, rng: &mut SplitMix64) -> u64 {
        match self {
            Self::Fixed(bytes) => bytes,
            Self::Range(SizeRange { min, max }) => rng.range_inclusive(min, max),
        }
    }

    fn check(self) -> Result<(), String> {
        let (min, max) = match self {
            Self::Fixed(bytes) => (bytes, bytes),
            Self::Range(SizeRange { min, max }) => (min, max),
        };
        if min > max {
            return Err(format!("`min` ({min}) is larger than `max` ({max})"));
        }
        if max > MAX_DENSE_FILE_BYTES {
            return Err(format!(
                "{max} bytes exceeds the {MAX_DENSE_FILE_BYTES}-byte limit"
            ));
        }
        Ok(())
    }
}

const fn one_byte() -> SizeSpec {
    SizeSpec::Fixed(1)
}

const fn one() -> u32 {
    1
}

const fn two() -> u32 {
    2
}

fn default_dir_names() -> NameStyle {
    NameStyle::sequential("d", "")
}

fn default_file_names() -> NameStyle {
    NameStyle::sequential("f", ".dat")
}

/// A directory tree with a fixed fan-out.
///
/// The root is level 0 and each level below has `fanout` subdirectories per directory, down to
/// level `depth`. `files_per_dir` files go in every directory, or only in the deepest ones.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TreePart {
    /// The name of the top-level directory.
    pub root: String,
    /// The number of directory levels below the root. `0` is a single wide directory.
    pub depth: u32,
    /// Subdirectories per directory, at every level above the deepest. Default `1`.
    #[serde(default = "one")]
    pub fanout: u32,
    /// Files per directory that receives files. Default `0`.
    #[serde(default)]
    pub files_per_dir: u32,
    /// Which directories receive files. Default `all`.
    #[serde(default)]
    pub file_placement: FilePlacement,
    /// How directories are named. Default `d0`, `d1`, and so on.
    #[serde(default = "default_dir_names")]
    pub dir_names: NameStyle,
    /// How files are named. Default `f0.dat`, `f1.dat`, and so on.
    #[serde(default = "default_file_names")]
    pub file_names: NameStyle,
    /// How large each file is. Default one byte.
    #[serde(default = "one_byte")]
    pub file_size: SizeSpec,
}

impl TreePart {
    /// The number of directories, the root included.
    #[must_use]
    pub fn directory_count(&self) -> u64 {
        let mut total: u64 = 0;
        let mut level: u64 = 1;
        for _ in 0..=self.depth {
            total = total.saturating_add(level);
            level = level.saturating_mul(u64::from(self.fanout));
        }
        total
    }

    /// The number of directories at the deepest level.
    #[must_use]
    pub fn leaf_directory_count(&self) -> u64 {
        (0..self.depth).fold(1_u64, |count, _| {
            count.saturating_mul(u64::from(self.fanout))
        })
    }

    /// The number of entries the part plans: directories and files, the root included.
    #[must_use]
    pub fn entry_count(&self) -> u64 {
        let receiving = match self.file_placement {
            FilePlacement::All => self.directory_count(),
            FilePlacement::Leaves => self.leaf_directory_count(),
        };
        self.directory_count()
            .saturating_add(receiving.saturating_mul(u64::from(self.files_per_dir)))
    }

    /// The length in bytes of the longest path the part plans, from the fixture root, exactly. A
    /// folder's path is the root and, for each level, a separator and a name, and every folder
    /// has a child of every index, so the longest name the style gives is on the longest path at
    /// every level. The deepest folders hold the files, so the longest path is a file's when the
    /// part has files, and a deepest folder's when it has none.
    #[must_use]
    pub fn longest_path_bytes(&self) -> u64 {
        let folder = 1 + self.dir_names.longest_name_bytes(u64::from(self.fanout));
        let file = if self.files_per_dir == 0 {
            0
        } else {
            1 + self
                .file_names
                .longest_name_bytes(u64::from(self.files_per_dir))
        };
        bytes_of(&self.root)
            .saturating_add(u64::from(self.depth).saturating_mul(folder))
            .saturating_add(file)
    }
}

/// A chain of nested directories, `depth` levels below the root, each named with `name_len` bytes.
///
/// The relative path of the deepest directory is `root_len + depth * (name_len + 1)` bytes.
/// Choose it above `PATH_MAX` (1024 on macOS, 4096 on Linux) to force the tools under test past
/// the limit that path-based calls have.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeepPart {
    /// The name of the top-level directory.
    pub root: String,
    /// The number of directories below the root.
    pub depth: u32,
    /// The length of each directory name in bytes, `5..=255`.
    pub name_len: u32,
    /// Files in every directory of the chain, the root included. Default `0`.
    #[serde(default)]
    pub files_per_level: u32,
    /// How large each file is. Default one byte.
    #[serde(default = "one_byte")]
    pub file_size: SizeSpec,
}

impl DeepPart {
    /// The length in bytes of the fixture-relative path of the deepest directory.
    #[must_use]
    pub fn deepest_path_bytes(&self) -> u64 {
        let name = u64::from(self.name_len) + 1;
        (self.root.len() as u64).saturating_add(u64::from(self.depth).saturating_mul(name))
    }

    /// The number of entries the part plans.
    #[must_use]
    pub fn entry_count(&self) -> u64 {
        let directories = u64::from(self.depth) + 1;
        directories.saturating_add(directories.saturating_mul(u64::from(self.files_per_level)))
    }

    /// The length in bytes of the longest path the part plans, from the fixture root, exactly: the
    /// deepest directory, or a file in it (`f0.dat`, `f1.dat`, and so on) when the part has files.
    #[must_use]
    pub fn longest_path_bytes(&self) -> u64 {
        let file = if self.files_per_level == 0 {
            0
        } else {
            // A separator, `f`, the digits of the largest index, and `.dat`.
            1 + 1 + u64::from(decimal_digits(u64::from(self.files_per_level) - 1)) + 4
        };
        self.deepest_path_bytes().saturating_add(file)
    }
}

/// One regular file at the top level of the fixture.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilePart {
    /// The name of the file.
    pub root: String,
    /// How large the file is. Default one byte.
    #[serde(default = "one_byte")]
    pub size: SizeSpec,
}

impl FilePart {
    /// The length in bytes of the longest path the part plans: the name of the file.
    #[must_use]
    pub fn longest_path_bytes(&self) -> u64 {
        bytes_of(&self.root)
    }
}

string_enum! {
    /// A kind of symbolic-link cycle.
    pub enum LoopKind {
        /// A link that names itself: `self -> self`.
        SelfLink => "self",
        /// Two links that name each other: `pair-a -> pair-b`, `pair-b -> pair-a`.
        Pair => "pair",
        /// A link that names its own directory: `cycle/again -> ../cycle`.
        Directory => "directory",
    }
}

const fn default_data_kib() -> u32 {
    4
}

const fn default_clone_kib() -> u32 {
    64
}

fn default_link_size() -> SizeSpec {
    SizeSpec::Fixed(4096)
}

/// One sparse file: a small run of data followed by a hole up to the apparent size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SparseSpec {
    /// The apparent size in MiB.
    pub apparent_mib: u64,
    /// The bytes of real data written at the start of the file, in KiB. Default `4`.
    ///
    /// File systems differ in what stays sparse. APFS allocates in full a file of up to 16 MiB
    /// that holds a full block of data; use an `apparent_mib` above 16 for a file that must be
    /// sparse on every file system.
    #[serde(default = "default_data_kib")]
    pub data_kib: u32,
}

/// The identity class: files that share or hide their identity.
///
/// Every feature is off unless its field asks for it. The entries live in fixed subdirectories
/// of the root: `links/`, `dangling/`, `symlinks/`, `loops/`, `sparse/`, and `clones/`. Every
/// link target is relative, so the fixture is the same wherever it is generated.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdentityPart {
    /// The name of the top-level directory.
    pub root: String,
    /// Hard-link groups: sets of names for one file. Default `0`.
    #[serde(default)]
    pub hard_link_groups: u32,
    /// Names per group, `2..=64`. Default `2`.
    #[serde(default = "two")]
    pub links_per_group: u32,
    /// The number of directories (`links/d0`, `links/d1`, ...) the names of a group spread across,
    /// so a group has names in different directories. Default `2`.
    #[serde(default = "two")]
    pub link_spread: u32,
    /// How large the linked files are. Default 4096 bytes.
    #[serde(default = "default_link_size")]
    pub link_file_size: SizeSpec,
    /// Symbolic links whose target does not exist. Default `0`.
    #[serde(default)]
    pub dangling_symlinks: u32,
    /// Whether to add working links: one to a file and one to a directory. Default `false`.
    #[serde(default)]
    pub valid_symlinks: bool,
    /// The symbolic-link cycles to add. Default none.
    #[serde(default)]
    pub symlink_loops: Vec<LoopKind>,
    /// The sparse files to add. Default none.
    #[serde(default)]
    pub sparse_files: Vec<SparseSpec>,
    /// Clones of one original file. Needs a file system that clones (APFS, or a reflink-capable
    /// Linux file system); skipped with a capability flag elsewhere. Default `0`.
    #[serde(default)]
    pub clones: u32,
    /// The size of the cloned original in KiB. Default `64`.
    #[serde(default = "default_clone_kib")]
    pub clone_kib: u32,
}

impl IdentityPart {
    /// An upper bound on the length in bytes of the longest path the part plans, from the
    /// fixture root, worked out from the fields of the part alone: the root and the longest path
    /// of each feature the part has, in the fixed subfolders the type names. It is exact unless
    /// the files of the longest names in `links/` are spread to folders whose names are shorter
    /// than the longest folder name (`d10` past `d9`, say), which the groups decide.
    #[must_use]
    pub fn longest_path_bytes(&self) -> u64 {
        // The digits of the largest index among `count` names.
        let digits = |count: u64| u64::from(decimal_digits(count.saturating_sub(1)));
        // A path below the root made of names of these lengths, each after a separator.
        let below = |names: &[u64]| names.iter().map(|name| 1 + name).sum::<u64>();
        let mut longest = 0;
        if self.hard_link_groups > 0 {
            // `links/d<n>/g<group>-n<name>.dat`
            let folder = 1 + digits(u64::from(self.link_spread));
            let file = 1
                + digits(u64::from(self.hard_link_groups))
                + 2
                + digits(u64::from(self.links_per_group))
                + 4;
            longest = longest.max(below(&[5, folder, file]));
        }
        if self.dangling_symlinks > 0 {
            // `dangling/dangling<n>`
            longest = longest.max(below(&[8, 8 + digits(u64::from(self.dangling_symlinks))]));
        }
        if self.valid_symlinks {
            // `symlinks/target.dat`, which is longer than `file-link`, `dir-link`, and `real-dir`
            longest = longest.max(below(&[8, 10]));
        }
        for kind in &self.symlink_loops {
            // `loops/self`, `loops/pair-a`, `loops/cycle/again`
            longest = longest.max(match kind {
                LoopKind::SelfLink => below(&[5, 4]),
                LoopKind::Pair => below(&[5, 6]),
                LoopKind::Directory => below(&[5, 5, 5]),
            });
        }
        if !self.sparse_files.is_empty() {
            // `sparse/s<n>.bin`
            let files = u64::try_from(self.sparse_files.len()).unwrap_or(u64::MAX);
            longest = longest.max(below(&[6, 1 + digits(files) + 4]));
        }
        if self.clones > 0 {
            // `clones/original.bin`, and `clones/clone<n>.bin`
            let clone = 5 + digits(u64::from(self.clones)) + 4;
            longest = longest.max(below(&[6, 12.max(clone)]));
        }
        bytes_of(&self.root).saturating_add(longest)
    }
}

string_enum! {
    /// One kind of hostile entry.
    pub enum HostileFeature {
        /// Names with control characters, DEL, and C1 controls.
        ControlNames => "control_names",
        /// Names with bidirectional overrides and isolates.
        BidiNames => "bidi_names",
        /// Names that carry terminal escape sequences.
        EscapeNames => "escape_names",
        /// Names with line breaks.
        NewlineNames => "newline_names",
        /// Names that are not valid UTF-8. Skipped where the file system refuses them.
        InvalidUtf8Names => "invalid_utf8_names",
        /// Names of exactly 255 bytes.
        LongNames => "long_names",
        /// Directories that cannot be listed (mode `000` and mode `100`), with contents.
        UnreadableDirs => "unreadable_dirs",
        /// Files that cannot be read (mode `000`) or written to read (mode `200`).
        UnreadableFiles => "unreadable_files",
        /// A directory that can be listed and entered but not changed (mode `555`), holding a
        /// file: nothing in it can be removed.
        UnwritableDirs => "unwritable_dirs",
    }
}

impl HostileFeature {
    /// The subdirectory of the part's root that holds the entries of this feature.
    #[must_use]
    pub const fn directory(self) -> &'static str {
        match self {
            Self::ControlNames => "control",
            Self::BidiNames => "bidi",
            Self::EscapeNames => "escape",
            Self::NewlineNames => "newline",
            Self::InvalidUtf8Names => "invalid-utf8",
            Self::LongNames => "long",
            Self::UnreadableDirs | Self::UnreadableFiles => "unreadable",
            Self::UnwritableDirs => "unwritable",
        }
    }
}

/// The hostile class. Each name feature adds, for every name of its catalog, one directory with
/// that name holding one file with the same name. The unreadable features add fixed shapes under
/// `unreadable/`, and `unwritable_dirs` adds `unwritable/`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostilePart {
    /// The name of the top-level directory.
    pub root: String,
    /// The features to include, without repeats and in any order; at least one.
    pub features: Vec<HostileFeature>,
}

impl HostilePart {
    /// The length in bytes of the longest path the part plans, from the fixture root, exactly.
    /// A name feature plans, below a folder of its own, a folder and a file that both have a name
    /// of its catalog; the other features plan fixed paths. The names are the catalog's, so the
    /// part cannot drift from the plan.
    #[must_use]
    pub fn longest_path_bytes(&self) -> u64 {
        let mut longest = 0;
        for feature in &self.features {
            let below = match feature {
                // `locked-000/nested/deep.txt`, which is longer than `locked-100/inner.txt`.
                HostileFeature::UnreadableDirs => bytes_of("/locked-000/nested/deep.txt"),
                // `locked-file-000.bin`, which is longer than `write-only.bin`.
                HostileFeature::UnreadableFiles => bytes_of("/locked-file-000.bin"),
                HostileFeature::UnwritableDirs => bytes_of("/stuck.txt"),
                HostileFeature::ControlNames
                | HostileFeature::BidiNames
                | HostileFeature::EscapeNames
                | HostileFeature::NewlineNames
                | HostileFeature::InvalidUtf8Names
                | HostileFeature::LongNames => {
                    let name = name_catalog(*feature)
                        .iter()
                        .flatten()
                        .map(|name| u64::try_from(name.len()).unwrap_or(u64::MAX))
                        .max()
                        .unwrap_or(0);
                    2 * (1 + name)
                }
            };
            longest = longest.max(1 + bytes_of(feature.directory()) + below);
        }
        bytes_of(&self.root).saturating_add(longest)
    }
}

const fn default_volume_file_bytes() -> u64 {
    1024
}

/// The volume class: a mount boundary.
///
/// The master fixture holds only the empty mount-point directory `root`. A run copy can then
/// attach a `size_mib` volume there (an explicit, privileged step) and fill it with `files` files
/// of `file_bytes` bytes each. Without the opt-in the mount point stays an empty directory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumePart {
    /// The name of the top-level mount-point directory.
    pub root: String,
    /// The size of the volume in MiB.
    pub size_mib: u32,
    /// Files written onto the volume after it is attached. Default `0`.
    #[serde(default)]
    pub files: u32,
    /// The size of each file in bytes. Default `1024`.
    #[serde(default = "default_volume_file_bytes")]
    pub file_bytes: u64,
}

impl VolumePart {
    /// The length in bytes of the longest path the part plans: the mount point. The files written
    /// onto the volume once it is attached are in a run copy and never in the cache, whose master
    /// holds only the empty mount point.
    #[must_use]
    pub fn longest_path_bytes(&self) -> u64 {
        bytes_of(&self.root)
    }
}

const fn default_max_file_bytes() -> u64 {
    DEFAULT_MAX_FILE_BYTES
}

/// What one depth of a `shaped` part holds: the entries at that depth, which live in the
/// directories of the depth above.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShapedLevel {
    /// Directories at this depth. Default `0`.
    #[serde(default)]
    pub directories: u32,
    /// Regular files at this depth. Default `0`.
    #[serde(default)]
    pub files: u32,
    /// Symbolic links at this depth. Default `0`.
    #[serde(default)]
    pub symlinks: u32,
}

/// A tree drawn from the shape of another tree, which `excise-shape profile` measured.
///
/// The uniform `tree` part has one fan-out at every depth, so it cannot follow a real tree, where
/// a few directories hold thousands of entries and most hold a handful. This part is told how
/// many directories, files, and symbolic links there are at each depth, and how entries are
/// spread over the directories above them, and builds a tree that has exactly those numbers:
///
/// * **The counts are exact.** Depth `n` holds `levels[n - 1]` entries, so the part plans
///   `1 + the sum of the levels` entries, the root directory included, and that is known before
///   anything is generated.
/// * **The spread follows the histograms.** The directories at depth `n - 1` receive the entries
///   of depth `n` in proportion to weights drawn from `subdirectories_per_directory` (for
///   directories) and `files_per_directory` (for files and symbolic links), so a few of them get
///   many and most get few, as the profile measured. A histogram holds classes of powers of two
///   (see [`crate::histogram`]); a value is spread evenly within its class.
/// * **Sizes and name lengths are the histograms'.** A file's size comes from `file_sizes`, cut
///   off at `max_file_bytes` because the generator writes every byte, and a name's length from
///   `directory_name_lengths`, `file_name_lengths`, or `symlink_name_lengths`. A name is made
///   of hexadecimal digits, the first of them a unique number within its directory, so a name is
///   never shorter than that number needs.
/// * **Links.** `hard_links` has an entry for each class of file size that holds groups of
///   regular files that are names of one file: its `class`, how many `names` its groups have in
///   all, and how many names a group has, in classes, in `group_sizes`. The names of a group are
///   one file, so they share a size and lie in one class of `file_sizes`, which is why a group
///   is given a class and is not put wherever it fits: the groups of a class take their names
///   from the files of that class and no other, so the histogram of sizes is kept exactly and
///   there is nothing to pack. Each group is given an exact number of names within its class so
///   that they add up to `names` (see [`HardLinkClass::sizes`]). A class with fewer files than
///   names, or a `names` that groups of those sizes cannot have, is refused. `dangling_symlinks`
///   of the symbolic links point at nothing, and the others at a file of the part.
///
/// The values in a histogram are not sampled: the spread is the exact quantiles of the
/// histogram, so the shape is the same whatever the seed, and the seed only decides which
/// directory gets which share and which file which size. A part with the same fields and the same
/// seed plans the same tree on every machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShapedPart {
    /// The name of the top-level directory, which is the root of the shape and depth 0.
    pub root: String,
    /// The largest file the part writes, in bytes: a size drawn above it becomes this. Default
    /// 16 KiB, at most 1 GiB.
    #[serde(default = "default_max_file_bytes")]
    pub max_file_bytes: u64,
    /// The entries at each depth from 1, one to [`MAX_SHAPED_DEPTH`] levels. A level below the
    /// first needs a directory at the level above, and no level may be empty.
    pub levels: Vec<ShapedLevel>,
    /// How many of the symbolic links point at nothing. The rest point at a file of the part.
    /// Default `0`.
    #[serde(default)]
    pub dangling_symlinks: u32,
    /// The directories a directory holds, in classes of powers of two. Needed when the levels
    /// have directories.
    #[serde(default, skip_serializing_if = "Buckets::is_empty")]
    pub subdirectories_per_directory: Buckets,
    /// The regular files a directory holds, in classes. Needed when the levels have files or
    /// symbolic links, which are spread like files.
    #[serde(default, skip_serializing_if = "Buckets::is_empty")]
    pub files_per_directory: Buckets,
    /// The size of a regular file in bytes, in classes. Needed when the levels have files.
    #[serde(default, skip_serializing_if = "Buckets::is_empty")]
    pub file_sizes: Buckets,
    /// The length of the name of a directory in bytes, one bucket per length. Needed when the
    /// levels have directories.
    #[serde(default, skip_serializing_if = "Buckets::is_empty")]
    pub directory_name_lengths: Buckets,
    /// The length of the name of a file in bytes. Needed when the levels have files.
    #[serde(default, skip_serializing_if = "Buckets::is_empty")]
    pub file_name_lengths: Buckets,
    /// The length of the name of a symbolic link in bytes. Needed when the levels have links.
    #[serde(default, skip_serializing_if = "Buckets::is_empty")]
    pub symlink_name_lengths: Buckets,
    /// The groups of hard links by the class of the size of their file: at most one entry for a
    /// class, in ascending order of class. Default none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hard_links: Vec<HardLinkClass>,
}

/// The groups of hard links among the files of one class of size, in a [`ShapedPart`].
///
/// The names of a hard-linked file are one file, so they share a size and lie in one class of the
/// part's `file_sizes`. A profile records the groups by the class of the size of their file, and
/// so does the part, which is why it never has to guess in which class a group was.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HardLinkClass {
    /// The class of the size of the files: its smallest value, 0 or a power of two. The cut at
    /// `max_file_bytes` is counted, so a class above it is the class of the cut (see
    /// [`ShapedPart::size_classes`]).
    pub class: u64,
    /// How many names the groups of the class have in all: at least the smallest numbers their
    /// classes allow added up, at most the largest, and at most the files of the class.
    pub names: u64,
    /// How many names a group has, in classes of powers of two from 2, and how many groups have
    /// that many. The counts add up to the number of groups of the class.
    pub group_sizes: Buckets,
}

impl HardLinkClass {
    /// How many groups the class has.
    #[must_use]
    pub fn groups(&self) -> u64 {
        u64::try_from(self.group_sizes.count()).unwrap_or(u64::MAX)
    }

    /// The fewest and the most names that groups of the sizes in `group_sizes` can have: every
    /// group at the smallest number its class allows, and every group at the largest.
    #[must_use]
    pub fn names_range(&self) -> (u128, u128) {
        let (mut low, mut high) = (0_u128, 0_u128);
        for (class, count) in self.group_sizes.iter() {
            let (floor, ceiling) = BucketKind::Class.bounds(class);
            low += u128::from(floor) * u128::from(count);
            high += u128::from(ceiling) * u128::from(count);
        }
        (low, high)
    }

    /// How many names each group has, in ascending order: the groups of `group_sizes`, each given
    /// an exact number within its class, and the numbers add up to `names`.
    ///
    /// Every group starts at the smallest number its class allows. The names that are left are
    /// shared among the classes in proportion to how many more names their groups could take, and
    /// then evenly among the groups of a class, with the groups of a class that get one more
    /// last. That is integer arithmetic on the entry alone, so the same entry gives the same
    /// sizes on every machine, and it never fails when `names` is within
    /// [`HardLinkClass::names_range`], which every entry of a validated part is. A `names`
    /// outside the range is brought within it.
    #[must_use]
    pub fn sizes(&self) -> Vec<u64> {
        let (low, high) = self.names_range();
        let surplus =
            u64::try_from(u128::from(self.names).clamp(low, high) - low).unwrap_or(u64::MAX);
        let buckets: Vec<(u64, u64)> = self
            .group_sizes
            .iter()
            .filter(|(_, count)| *count > 0)
            .collect();
        // What the groups of a class can take over their smallest number.
        let room: Vec<u64> = buckets
            .iter()
            .map(|(class, count)| {
                let (floor, ceiling) = BucketKind::Class.bounds(*class);
                u64::try_from(u128::from(*count) * u128::from(ceiling - floor)).unwrap_or(u64::MAX)
            })
            .collect();
        let mut sizes = Vec::new();
        for ((floor, count), share) in buckets.iter().zip(apportion(surplus, &room)) {
            let (each, extra) = (share / count, share % count);
            for index in 0..*count {
                sizes.push(
                    floor
                        .saturating_add(each)
                        .saturating_add(u64::from(index >= count - extra)),
                );
            }
        }
        sizes
    }
}

impl ShapedPart {
    /// The directories of all the levels, the root not counted.
    #[must_use]
    pub fn directory_count(&self) -> u64 {
        self.levels
            .iter()
            .map(|level| u64::from(level.directories))
            .sum()
    }

    /// The regular files of all the levels.
    #[must_use]
    pub fn file_count(&self) -> u64 {
        self.levels.iter().map(|level| u64::from(level.files)).sum()
    }

    /// The symbolic links of all the levels.
    #[must_use]
    pub fn symlink_count(&self) -> u64 {
        self.levels
            .iter()
            .map(|level| u64::from(level.symlinks))
            .sum()
    }

    /// The number of entries the part plans: the root directory and every level.
    #[must_use]
    pub fn entry_count(&self) -> u64 {
        1 + self.directory_count() + self.file_count() + self.symlink_count()
    }

    /// The regular files of the part in each class of size, after the cut at `max_file_bytes`, as a
    /// class and how many files are in it, in ascending order of class: the counts of the values
    /// [`spread`] makes of `file_sizes`, with the classes above the cut counted in the class of it.
    #[must_use]
    pub fn size_classes(&self) -> Vec<(u64, u64)> {
        let mut classes = Buckets::new();
        for (key, files) in spread_counts(&self.file_sizes, self.file_count()) {
            classes.add(class_floor(key.min(self.max_file_bytes)), files);
        }
        classes.iter().collect()
    }

    /// An upper bound on the length in bytes of the longest path the part plans, from the fixture
    /// root, worked out from the fields of the part alone, without planning it: the plan never has
    /// a longer one, whatever the seed.
    ///
    /// A path is the `root`, and for each level it goes down a separator and the name of the entry
    /// there. The folders on a path are distinct folders of the part, one for each level but the
    /// last, so the longest path is no longer than the `root`, the names of the longest folders
    /// the part can have, as many as the levels but one, and the longest name any entry at the
    /// last level can have. The lengths of the names are the counts of the values that [`spread`]
    /// makes of `directory_name_lengths`, `file_name_lengths`, and `symlink_name_lengths` for as
    /// many names as the part has, the longest first; and no name is shorter than the hexadecimal
    /// digits that tell the entries of its folder apart, which are as many as the part has at the
    /// most.
    #[must_use]
    pub fn longest_path_bytes(&self) -> u64 {
        let levels = u64::try_from(self.levels.len()).unwrap_or(u64::MAX);
        let digits = u64::from(hex_digits_for(self.entry_count()));
        // The longest lengths of `count` names drawn from `buckets`, longest first, `take` of
        // them at the most, each at least as long as the digits.
        let longest = |buckets: &Buckets, count: u64, take: u64| -> Vec<u64> {
            let mut lengths: Vec<u64> = Vec::new();
            for (length, names) in spread_counts(buckets, count).into_iter().rev() {
                let room = take.saturating_sub(lengths.len() as u64);
                for _ in 0..names.min(room) {
                    lengths.push(length.max(digits));
                }
            }
            lengths
        };
        let folders = longest(&self.directory_name_lengths, self.directory_count(), levels);
        let on_the_way = levels.saturating_sub(1);
        let mut path = self.root.len() as u64;
        for index in 0..on_the_way {
            let name = folders
                .get(usize::try_from(index).unwrap_or(usize::MAX))
                .copied()
                .unwrap_or(digits);
            path = path.saturating_add(1 + name);
        }
        let last = |buckets: &Buckets, count: u64| {
            longest(buckets, count, 1)
                .first()
                .copied()
                .unwrap_or(digits)
        };
        let entry = [
            folders
                .get(usize::try_from(on_the_way).unwrap_or(usize::MAX))
                .copied()
                .unwrap_or(digits),
            last(&self.file_name_lengths, self.file_count()),
            last(&self.symlink_name_lengths, self.symlink_count()),
        ]
        .into_iter()
        .max()
        .unwrap_or(digits);
        path.saturating_add(1 + entry)
    }

    /// How many groups of hard links the part has: the groups of every class.
    #[must_use]
    pub fn hard_link_group_count(&self) -> u64 {
        self.hard_links.iter().map(HardLinkClass::groups).sum()
    }
}

/// One rule a spec breaks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecProblem {
    /// Where: `id`, `parts[2].depth`, and so on.
    pub location: String,
    /// What is wrong.
    pub message: String,
}

impl fmt::Display for SpecProblem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.location, self.message)
    }
}

/// Every rule a spec breaks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecProblems(pub Vec<SpecProblem>);

impl fmt::Display for SpecProblems {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, problem) in self.0.iter().enumerate() {
            if index > 0 {
                formatter.write_str("; ")?;
            }
            write!(formatter, "{problem}")?;
        }
        Ok(())
    }
}

impl std::error::Error for SpecProblems {}

/// A fixture spec that could not be loaded.
#[derive(Debug, Error)]
pub enum SpecError {
    /// The file could not be read.
    #[error("cannot read fixture spec `{}`: {source}", path.display())]
    Read {
        /// The path that was read.
        path: PathBuf,
        /// The underlying error.
        source: io::Error,
    },
    /// No spec has this id.
    #[error("no fixture spec `{id}` in `{}`", dir.display())]
    NotFound {
        /// The requested id.
        id: String,
        /// The directory that was searched.
        dir: PathBuf,
    },
    /// The text is not a valid spec document.
    #[error("invalid fixture spec{}: {source}", located(path.as_deref()))]
    Parse {
        /// The file, when the spec came from one.
        path: Option<PathBuf>,
        /// The underlying error, with line and column.
        source: toml::de::Error,
    },
    /// The document parsed but breaks the rules of [`FixtureSpec::validate`].
    #[error("invalid fixture spec{}: {problems}", located(path.as_deref()))]
    Invalid {
        /// The file, when the spec came from one.
        path: Option<PathBuf>,
        /// Every rule that is broken.
        problems: SpecProblems,
    },
}

fn located(path: Option<&Path>) -> String {
    path.map_or_else(String::new, |path| format!(" `{}`", path.display()))
}

/// The text of the spec file at `path`: a regular file that is not a link, of at most
/// [`MAX_SPEC_BYTES`]. Nothing is read from a file above the cap beyond the byte that shows it is
/// above it.
fn read_spec_text(path: &Path) -> io::Result<String> {
    let mut bytes = Vec::new();
    open_regular_file(path)?
        .take(MAX_SPEC_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_SPEC_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("larger than {MAX_SPEC_BYTES} bytes, which no spec is"),
        ));
    }
    String::from_utf8(bytes).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

impl FixtureSpec {
    /// The directory of the specs this crate ships.
    #[must_use]
    pub fn bundled_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
    }

    /// Parses and validates a spec from TOML text.
    ///
    /// # Errors
    ///
    /// Returns [`SpecError::Parse`] for text that is not a spec (unknown fields included) and
    /// [`SpecError::Invalid`] for a spec that breaks a rule.
    pub fn from_toml_str(text: &str) -> Result<Self, SpecError> {
        Self::parse(text, None)
    }

    /// Reads, parses, and validates a spec file. The file is opened without following a link (the
    /// directories above it are resolved as written), must be a regular file, and is read up to
    /// [`MAX_SPEC_BYTES`]: a link, a FIFO (which is not waited on), a folder, a device, and a file
    /// above the cap are refused, each with a message that says which, and none is read in full.
    ///
    /// # Errors
    ///
    /// Returns [`SpecError::Read`] when the file cannot be read, or is refused for what it is, and
    /// the errors of [`FixtureSpec::from_toml_str`] otherwise.
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, SpecError> {
        let path = path.as_ref();
        let text = read_spec_text(path).map_err(|source| SpecError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(&text, Some(path))
    }

    /// Loads the spec `id` from `dir`: the file `<dir>/<id>.toml`. The file's own `id` must
    /// equal the requested one.
    ///
    /// # Errors
    ///
    /// Returns [`SpecError::NotFound`] when there is no such file, and the errors of
    /// [`FixtureSpec::from_path`] otherwise.
    pub fn load(dir: impl AsRef<Path>, id: &str) -> Result<Self, SpecError> {
        let dir = dir.as_ref();
        let path = dir.join(format!("{id}.toml"));
        if !is_identifier(id) {
            return Err(SpecError::Invalid {
                path: None,
                problems: SpecProblems(vec![SpecProblem {
                    location: "id".to_owned(),
                    message: identifier_message(id),
                }]),
            });
        }
        let spec = match Self::from_path(&path) {
            Err(SpecError::Read { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
                return Err(SpecError::NotFound {
                    id: id.to_owned(),
                    dir: dir.to_path_buf(),
                });
            }
            other => other?,
        };
        if spec.id != id {
            return Err(SpecError::Invalid {
                path: Some(path),
                problems: SpecProblems(vec![SpecProblem {
                    location: "id".to_owned(),
                    message: format!("the file is named for `{id}` but its `id` is `{}`", spec.id),
                }]),
            });
        }
        Ok(spec)
    }

    /// Loads the spec `id` from this crate's `fixtures/` directory.
    ///
    /// # Errors
    ///
    /// See [`FixtureSpec::load`].
    pub fn load_bundled(id: &str) -> Result<Self, SpecError> {
        Self::load(Self::bundled_dir(), id)
    }

    /// The ids of the specs in `dir`, sorted.
    ///
    /// # Errors
    ///
    /// Returns the error of reading the directory.
    pub fn ids_in(dir: impl AsRef<Path>) -> io::Result<Vec<String>> {
        let mut ids = Vec::new();
        for entry in fs::read_dir(dir)? {
            let name = entry?.file_name();
            if let Some(id) = name.to_str().and_then(|name| name.strip_suffix(".toml")) {
                ids.push(id.to_owned());
            }
        }
        ids.sort();
        Ok(ids)
    }

    fn parse(text: &str, path: Option<&Path>) -> Result<Self, SpecError> {
        let spec: Self = toml::from_str(text).map_err(|source| SpecError::Parse {
            path: path.map(Path::to_path_buf),
            source,
        })?;
        spec.validate().map_err(|problems| SpecError::Invalid {
            path: path.map(Path::to_path_buf),
            problems,
        })?;
        Ok(spec)
    }

    /// This spec with another seed.
    #[must_use]
    pub fn with_seed(&self, seed: u64) -> Self {
        Self {
            seed,
            ..self.clone()
        }
    }

    /// Whether any part needs a privileged volume step to be complete.
    #[must_use]
    pub fn has_volumes(&self) -> bool {
        self.parts
            .iter()
            .any(|part| matches!(part, Part::Volume(_)))
    }

    /// An upper bound on the length in bytes of the longest path below the fixture root that the
    /// cache writes for this spec, worked out from its fields alone, without planning it: the
    /// longest path of the parts ([`Part::longest_path_bytes`]) and the name of the ownership
    /// marker, which every fixture root carries and which the cache writes under another name
    /// first, 25 bytes, and renames. The plan never has a longer path, whatever the seed.
    #[must_use]
    pub fn longest_path_bytes(&self) -> u64 {
        self.parts
            .iter()
            .map(Part::longest_path_bytes)
            .max()
            .unwrap_or(0)
            .max(bytes_of(MARKER_FILE_NAME))
            .max(bytes_of(TEMPORARY_NAME))
    }

    /// Whether a path-based recursive removal can remove a generated tree of this spec, the way
    /// `cargo clean` and `git worktree remove` remove a target directory, when the fixture root,
    /// the directory the tree is generated in, has an absolute path of `root_bytes` bytes. Such a
    /// removal fails on a directory that its owner cannot list (`unreadable_dirs`), on one it
    /// cannot remove anything from (`unwritable_dirs`), and on a path longer than
    /// [`MAX_PORTABLE_PATH_BYTES`]. A `deep` part always has one. Every other part can too: a
    /// `tree` or a `shaped` part has paths as long as its levels and names allow, up to 8,447
    /// bytes (32 levels of names of 255 bytes, which a spec of one's own can ask for), and a
    /// `file`, `volume`, `identity`, or `hostile` part has a root of up to 255 bytes with names
    /// below it. The marker is a path as well: the cache writes it at the fixture root under a
    /// name of 25 bytes and renames it to its own, of 21. So the fixture is removable only when
    /// the root, a separator, and the longest path it can plan,
    /// [`FixtureSpec::longest_path_bytes`], fit in the limit together: the room it has is what
    /// the root leaves. A fixture in the cache has the root
    /// [`FixtureCache::longest_entry_path_bytes`](super::FixtureCache::longest_entry_path_bytes)
    /// says, and is never cached when it is not removable: see
    /// [`Fixtures::is_cacheable`](super::Fixtures::is_cacheable).
    #[must_use]
    pub fn removable_by_path(&self, root_bytes: u64) -> bool {
        let without_defeats = self.parts.iter().all(|part| match part {
            Part::Deep(_) => false,
            Part::Hostile(hostile) => !hostile.features.iter().any(|feature| {
                matches!(
                    feature,
                    HostileFeature::UnreadableDirs | HostileFeature::UnwritableDirs
                )
            }),
            Part::Tree(_)
            | Part::File(_)
            | Part::Identity(_)
            | Part::Volume(_)
            | Part::Shaped(_) => true,
        });
        without_defeats
            && root_bytes
                .saturating_add(1)
                .saturating_add(self.longest_path_bytes())
                <= MAX_PORTABLE_PATH_BYTES
    }

    /// Whether the fixture does its job only for a user that is not root. A root process ignores
    /// permission modes, so a directory it is not allowed to change (`unwritable_dirs`) refuses it
    /// nothing, and a scenario that expects the refusal would be testing nothing.
    #[must_use]
    pub fn needs_unprivileged_user(&self) -> bool {
        self.parts.iter().any(|part| {
            matches!(
                part,
                Part::Hostile(hostile) if hostile.features.contains(&HostileFeature::UnwritableDirs)
            )
        })
    }

    /// The canonical text the spec hash covers: the schema version, id, seed, and parts as
    /// compact JSON in field order. Formatting, comments, and the description are not part of
    /// it, so editing them does not invalidate a cached fixture.
    #[must_use]
    pub fn canonical_json(&self) -> String {
        #[derive(Serialize)]
        struct Canonical<'a> {
            schema_version: u32,
            id: &'a str,
            seed: u64,
            parts: &'a [Part],
        }
        serde_json::to_string(&Canonical {
            schema_version: self.schema_version,
            id: &self.id,
            seed: self.seed,
            parts: &self.parts,
        })
        .unwrap_or_default()
    }

    /// The SHA-256 of [`FixtureSpec::canonical_json`], as 64 lowercase hex digits.
    #[must_use]
    pub fn spec_hash(&self) -> String {
        hex(&Sha256::digest(self.canonical_json().as_bytes()))
    }

    /// The number of entries the spec plans below the fixture root, from the parameters alone.
    #[must_use]
    pub fn planned_entry_count(&self) -> u64 {
        self.parts.iter().fold(0_u64, |total, part| {
            total.saturating_add(part_entry_count(part))
        })
    }

    /// Checks every rule a spec must obey and reports all the rules it breaks.
    ///
    /// # Errors
    ///
    /// Returns every [`SpecProblem`] found.
    pub fn validate(&self) -> Result<(), SpecProblems> {
        let mut problems = Vec::new();
        let mut problem = |location: String, message: String| {
            problems.push(SpecProblem { location, message });
        };

        if self.schema_version != SPEC_SCHEMA_VERSION {
            problem(
                "schema_version".to_owned(),
                format!(
                    "unsupported version {}; this build reads only {SPEC_SCHEMA_VERSION}",
                    self.schema_version
                ),
            );
        }
        if !is_identifier(&self.id) {
            problem("id".to_owned(), identifier_message(&self.id));
        }
        if self.parts.is_empty() {
            problem(
                "parts".to_owned(),
                "a spec needs at least one part".to_owned(),
            );
        }

        let mut roots: Vec<&str> = Vec::new();
        for (index, part) in self.parts.iter().enumerate() {
            let at = |field: &str| format!("parts[{index}].{field}");
            if let Err(message) = check_root(part.root()) {
                problem(at("root"), message);
            }
            if roots.contains(&part.root()) {
                problem(
                    at("root"),
                    format!("`{}` is already the root of another part", part.root()),
                );
            }
            roots.push(part.root());
            for (field, message) in check_part(part) {
                problem(at(field), message);
            }
        }
        let total = self.planned_entry_count();
        if total > MAX_ENTRIES {
            problem(
                "parts".to_owned(),
                format!("the spec plans {total} entries; the limit is {MAX_ENTRIES}"),
            );
        }

        if problems.is_empty() {
            Ok(())
        } else {
            Err(SpecProblems(problems))
        }
    }
}

/// Lowercase hexadecimal.
pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut text, byte| {
            // Writing to a `String` cannot fail.
            let _ = write!(text, "{byte:02x}");
            text
        })
}

/// The identifier rule scenarios apply to their `name` and `fixture`: 1 to 64 lowercase ASCII
/// letters, digits, `-`, or `_`, starting with a letter or digit.
pub(crate) fn is_identifier(value: &str) -> bool {
    let mut bytes = value.bytes();
    matches!(bytes.next(), Some(b'a'..=b'z' | b'0'..=b'9'))
        && value.len() <= MAX_IDENTIFIER_LEN
        && bytes.all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_'))
}

fn identifier_message(value: &str) -> String {
    format!(
        "{value:?} is not an identifier: use 1 to {MAX_IDENTIFIER_LEN} lowercase ASCII letters, \
         digits, `-`, or `_`, starting with a letter or digit"
    )
}

/// A part's root is one ordinary component that no scenario path rule and no file system would
/// treat specially.
fn check_root(root: &str) -> Result<(), String> {
    if root.is_empty() {
        return Err("the root must not be empty".to_owned());
    }
    if root.contains('/') {
        return Err("the root must be one name, without `/`".to_owned());
    }
    if root == MARKER_FILE_NAME {
        return Err(format!(
            "`{MARKER_FILE_NAME}` is reserved for the ownership marker"
        ));
    }
    if root.len() > MAX_NAME_BYTES {
        return Err(format!("the root is longer than {MAX_NAME_BYTES} bytes"));
    }
    check_fixture_relative_path(root).map_err(|violation| violation.to_string())
}

/// Field problems of one part, collected as `(field, message)`.
#[derive(Default)]
struct Problems(Vec<(&'static str, String)>);

impl Problems {
    /// Records the error of `result` against `field`.
    fn add(&mut self, field: &'static str, result: Result<(), String>) {
        if let Err(message) = result {
            self.0.push((field, message));
        }
    }

    /// Records `message` against `field` unless `holds`.
    fn require(&mut self, field: &'static str, holds: bool, message: impl Into<String>) {
        if !holds {
            self.0.push((field, message.into()));
        }
    }
}

/// Field problems of one part, as `(field, message)`.
fn check_part(part: &Part) -> Vec<(&'static str, String)> {
    let mut problems = Problems::default();
    match part {
        Part::Tree(part) => check_tree(part, &mut problems),
        Part::Deep(part) => check_deep(part, &mut problems),
        Part::File(part) => problems.add("size", part.size.check()),
        Part::Identity(part) => check_identity(part, &mut problems),
        Part::Hostile(part) => {
            problems.require(
                "features",
                !part.features.is_empty(),
                "list at least one feature",
            );
            problems.require(
                "features",
                !has_repeats(&part.features),
                "each feature may be listed once",
            );
        }
        Part::Shaped(part) => check_shaped(part, &mut problems),
        Part::Volume(part) => check_volume(part, &mut problems),
    }
    problems.0
}

fn check_tree(part: &TreePart, problems: &mut Problems) {
    problems.require(
        "depth",
        part.depth <= MAX_TREE_DEPTH,
        format!("at most {MAX_TREE_DEPTH} levels"),
    );
    problems.require(
        "fanout",
        part.depth == 0 || part.fanout > 0,
        "a tree with levels needs a fan-out of at least 1",
    );
    problems.add("file_size", part.file_size.check());
    problems.add("dir_names", part.dir_names.check(u64::from(part.fanout)));
    problems.add(
        "file_names",
        part.file_names.check(u64::from(part.files_per_dir)),
    );
}

fn check_deep(part: &DeepPart, problems: &mut Problems) {
    problems.require(
        "depth",
        (1..=MAX_DEEP_DEPTH).contains(&part.depth),
        format!("use 1 to {MAX_DEEP_DEPTH} levels"),
    );
    problems.require(
        "name_len",
        (5..=255).contains(&part.name_len),
        "use 5 to 255 bytes",
    );
    problems.add("file_size", part.file_size.check());
}

fn check_identity(part: &IdentityPart, problems: &mut Problems) {
    problems.require(
        "links_per_group",
        (2..=64).contains(&part.links_per_group),
        "use 2 to 64 names per group",
    );
    problems.require(
        "link_spread",
        (1..=64).contains(&part.link_spread),
        "use 1 to 64 directories",
    );
    problems.require(
        "hard_link_groups",
        part.hard_link_groups <= 10_000,
        "at most 10,000 groups",
    );
    problems.require(
        "dangling_symlinks",
        part.dangling_symlinks <= 10_000,
        "at most 10,000 links",
    );
    problems.add("link_file_size", part.link_file_size.check());
    problems.require(
        "symlink_loops",
        !has_repeats(&part.symlink_loops),
        "each loop kind may be listed once",
    );
    problems.add("sparse_files", check_sparse_files(&part.sparse_files));
    problems.require("clones", part.clones <= 1024, "at most 1,024 clones");
    problems.require(
        "clone_kib",
        part.clones == 0 || (4..=65_536).contains(&part.clone_kib),
        "use 4 to 65,536 KiB",
    );
}

fn check_sparse_files(files: &[SparseSpec]) -> Result<(), String> {
    if files.len() > 64 {
        return Err("at most 64 sparse files".to_owned());
    }
    if files
        .iter()
        .any(|sparse| sparse.apparent_mib == 0 || sparse.apparent_mib > MAX_SPARSE_MIB)
    {
        return Err(format!("`apparent_mib` must be 1 to {MAX_SPARSE_MIB}"));
    }
    if files
        .iter()
        .any(|sparse| u64::from(sparse.data_kib) * 1024 >= sparse.apparent_mib * 1024 * 1024)
    {
        return Err("`data_kib` must be smaller than the apparent size".to_owned());
    }
    Ok(())
}

fn check_volume(part: &VolumePart, problems: &mut Problems) {
    problems.require(
        "size_mib",
        VOLUME_MIB_RANGE.contains(&part.size_mib),
        format!(
            "use {} to {} MiB",
            VOLUME_MIB_RANGE.start(),
            VOLUME_MIB_RANGE.end()
        ),
    );
    problems.require("files", part.files <= 100_000, "at most 100,000 files");
    problems.require(
        "file_bytes",
        part.file_bytes <= 1 << 20,
        "at most 1 MiB per file",
    );
}

/// The most values one bucket of a `shaped` part may count: what JSON reads back exactly.
const MAX_BUCKET_COUNT: u64 = (1 << 53) - 1;

fn check_shaped(part: &ShapedPart, problems: &mut Problems) {
    problems.require(
        "max_file_bytes",
        part.max_file_bytes <= MAX_DENSE_FILE_BYTES,
        format!("at most {MAX_DENSE_FILE_BYTES} bytes"),
    );
    problems.require(
        "levels",
        (1..=MAX_SHAPED_DEPTH as usize).contains(&part.levels.len()),
        format!("use 1 to {MAX_SHAPED_DEPTH} levels"),
    );
    let mut parents: u64 = 1; // the root directory
    for (index, level) in part.levels.iter().enumerate() {
        let depth = index + 1;
        problems.require(
            "levels",
            level.directories != 0 || level.files != 0 || level.symlinks != 0,
            format!("depth {depth} holds nothing"),
        );
        problems.require(
            "levels",
            parents > 0,
            format!("depth {depth} lies below a depth with no directory"),
        );
        parents = u64::from(level.directories);
    }
    let directories = part.directory_count();
    let files = part.file_count();
    let symlinks = part.symlink_count();
    let dangling = u64::from(part.dangling_symlinks);
    problems.require(
        "dangling_symlinks",
        dangling <= symlinks,
        "more links dangle than the levels hold",
    );
    problems.require(
        "dangling_symlinks",
        dangling >= symlinks || files > 0,
        "a link that does not dangle needs a file to point at",
    );
    for (field, buckets, kind, needed) in [
        (
            "subdirectories_per_directory",
            &part.subdirectories_per_directory,
            BucketKind::Class,
            directories > 0,
        ),
        (
            "files_per_directory",
            &part.files_per_directory,
            BucketKind::Class,
            files + symlinks > 0,
        ),
        ("file_sizes", &part.file_sizes, BucketKind::Class, files > 0),
        (
            "directory_name_lengths",
            &part.directory_name_lengths,
            BucketKind::Length,
            directories > 0,
        ),
        (
            "file_name_lengths",
            &part.file_name_lengths,
            BucketKind::Length,
            files > 0,
        ),
        (
            "symlink_name_lengths",
            &part.symlink_name_lengths,
            BucketKind::Length,
            symlinks > 0,
        ),
    ] {
        problems.add(field, check_buckets(buckets, kind, needed));
    }
    check_hard_links(part, problems);
}

/// Checks the groups of hard links of a `shaped` part, class by class. A class is refused only
/// for what cannot be built: a group of fewer than two names, a `names` that groups of those
/// sizes cannot have, or more names than the class has files.
fn check_hard_links(part: &ShapedPart, problems: &mut Problems) {
    if part.hard_links.is_empty() {
        return;
    }
    let files_in = part.size_classes();
    let mut before: Option<u64> = None;
    for entry in &part.hard_links {
        let class = entry.class;
        if !BucketKind::Class.admits(class) {
            problems.require(
                "hard_links",
                false,
                format!("{class} is not a class: use 0 or a power of two"),
            );
            continue;
        }
        problems.require(
            "hard_links",
            before.is_none_or(|earlier| class > earlier),
            format!("list the classes in ascending order, each once: {class} does not follow the class before it"),
        );
        before = Some(class);
        if let Err(message) = check_buckets(&entry.group_sizes, BucketKind::Class, true) {
            problems.require(
                "hard_links",
                false,
                format!("the class {class}: group_sizes: {message}"),
            );
            continue;
        }
        if entry.group_sizes.iter().any(|(size, _)| size < 2) {
            problems.require(
                "hard_links",
                false,
                format!(
                    "the class {class}: a group has at least two names: use classes from 2 in \
                     group_sizes"
                ),
            );
            continue;
        }
        let (least, most) = entry.names_range();
        problems.require(
            "hard_links",
            (least..=most).contains(&u128::from(entry.names)),
            format!(
                "the class {class}: {} groups of these sizes cannot have {} names: they have \
                 {least} to {most}",
                entry.groups(),
                entry.names
            ),
        );
        let files = files_in
            .iter()
            .find(|(known, _)| *known == class)
            .map_or(0, |(_, files)| *files);
        problems.require(
            "hard_links",
            entry.names <= files,
            format!(
                "the class {class} has {files} files, and its groups have {} names: the names of \
                 a group are files of one class",
                entry.names
            ),
        );
    }
}

/// Checks the buckets of one histogram of a `shaped` part.
fn check_buckets(buckets: &Buckets, kind: BucketKind, needed: bool) -> Result<(), String> {
    if needed && buckets.is_empty() {
        return Err(
            "the part needs this histogram, because its levels hold entries it describes: \
             give it at least one bucket"
                .to_owned(),
        );
    }
    for (key, count) in buckets.iter() {
        if !kind.admits(key) {
            return Err(match kind {
                BucketKind::Class => format!("{key} is not a class: use 0 or a power of two"),
                BucketKind::Length => {
                    format!("{key} is not a name length: use 1 to {MAX_NAME_BYTES}")
                }
            });
        }
        if count == 0 || count > MAX_BUCKET_COUNT {
            return Err(format!(
                "the bucket {key} counts {count}: use 1 to {MAX_BUCKET_COUNT}, or leave it out"
            ));
        }
    }
    Ok(())
}

fn has_repeats<T: PartialEq>(items: &[T]) -> bool {
    items
        .iter()
        .enumerate()
        .any(|(index, item)| items[..index].contains(item))
}

/// The length of `text` in bytes.
fn bytes_of(text: &str) -> u64 {
    u64::try_from(text.len()).unwrap_or(u64::MAX)
}

/// The names a name feature of a hostile part plans, each for a folder and a file of that name;
/// the other features have none.
fn name_catalog(feature: HostileFeature) -> Option<Vec<Vec<u8>>> {
    use crate::fixture::names;

    match feature {
        HostileFeature::ControlNames => Some(names::control_character_names()),
        HostileFeature::BidiNames => Some(names::bidi_names()),
        HostileFeature::EscapeNames => Some(names::escape_sequence_names()),
        HostileFeature::NewlineNames => Some(names::newline_names()),
        HostileFeature::InvalidUtf8Names => Some(names::invalid_utf8_names()),
        HostileFeature::LongNames => Some(names::maximum_length_names()),
        HostileFeature::UnreadableDirs
        | HostileFeature::UnreadableFiles
        | HostileFeature::UnwritableDirs => None,
    }
}

/// The entries one part plans, from its parameters alone.
fn part_entry_count(part: &Part) -> u64 {
    match part {
        Part::Tree(part) => part.entry_count(),
        Part::Deep(part) => part.entry_count(),
        Part::File(_) | Part::Volume(_) => 1,
        Part::Shaped(part) => part.entry_count(),
        Part::Identity(part) => identity_entry_count(part),
        Part::Hostile(part) => hostile_entry_count(part),
    }
}

fn identity_entry_count(part: &IdentityPart) -> u64 {
    let mut total: u64 = 1; // the root
    if part.hard_link_groups > 0 {
        // `links/`, its `d*` directories, and every name.
        total += 1 + u64::from(part.link_spread);
        total += u64::from(part.hard_link_groups) * u64::from(part.links_per_group);
    }
    if part.dangling_symlinks > 0 {
        total += 1 + u64::from(part.dangling_symlinks);
    }
    if part.valid_symlinks {
        // `symlinks/`, the target file, the target directory, and one link to each.
        total += 5;
    }
    if !part.symlink_loops.is_empty() {
        total += 1;
        for kind in &part.symlink_loops {
            total += match kind {
                LoopKind::SelfLink => 1,
                LoopKind::Pair | LoopKind::Directory => 2,
            };
        }
    }
    if !part.sparse_files.is_empty() {
        total += 1 + part.sparse_files.len() as u64;
    }
    if part.clones > 0 {
        // `clones/`, the original, and the clones.
        total += 2 + u64::from(part.clones);
    }
    total
}

fn hostile_entry_count(part: &HostilePart) -> u64 {
    let mut total: u64 = 1; // the root
    let mut feature_directories: Vec<&str> = Vec::new();
    for feature in &part.features {
        if !feature_directories.contains(&feature.directory()) {
            feature_directories.push(feature.directory());
            total += 1;
        }
        // A directory and a file inside it, for every name.
        let names = match feature {
            HostileFeature::UnreadableDirs => {
                // `locked-000/` with `inner.txt`, `nested/`, and `nested/deep.txt`;
                // `locked-100/` with `inner.txt`.
                total += 6;
                0
            }
            HostileFeature::UnreadableFiles => {
                total += 2;
                0
            }
            HostileFeature::UnwritableDirs => {
                // `stuck.txt`; the directory that holds it is the feature's own.
                total += 1;
                0
            }
            name_feature => name_catalog(*name_feature).map_or(0, |catalog| catalog.len()),
        };
        total += 2 * names as u64;
    }
    total
}
