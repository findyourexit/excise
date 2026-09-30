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
//! spec that names an absurd size or a colliding root never reaches the generator.

use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use thiserror::Error;

use crate::{
    fixture::{MARKER_FILE_NAME, names::MAX_NAME_BYTES, names::NameStyle, rng::SplitMix64},
    scenario::{MAX_IDENTIFIER_LEN, check_fixture_relative_path},
    string_enum::string_enum,
};

/// The only spec `schema_version` this crate reads.
pub const SPEC_SCHEMA_VERSION: u32 = 1;

/// The most entries one spec may plan. Larger fixtures need a generator that streams its plan.
pub const MAX_ENTRIES: u64 = 2_000_000;

/// The largest dense (fully written) file a spec may ask for: 1 GiB.
pub const MAX_DENSE_FILE_BYTES: u64 = 1 << 30;

/// The largest apparent size of a sparse file a spec may ask for: 16 GiB.
pub const MAX_SPARSE_MIB: u64 = 16 * 1024;

/// The deepest `tree` part.
pub const MAX_TREE_DEPTH: u32 = 32;

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
        }
    }
}

/// The hostile class. Each name feature adds, for every name of its catalog, one directory with
/// that name holding one file with the same name. The unreadable features add fixed shapes under
/// `unreadable/`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostilePart {
    /// The name of the top-level directory.
    pub root: String,
    /// The features to include, without repeats and in any order; at least one.
    pub features: Vec<HostileFeature>,
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

    /// Reads, parses, and validates a spec file.
    ///
    /// # Errors
    ///
    /// Returns [`SpecError::Read`] when the file cannot be read, and the errors of
    /// [`FixtureSpec::from_toml_str`] otherwise.
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, SpecError> {
        let path = path.as_ref();
        let text = fs::read_to_string(path).map_err(|source| SpecError::Read {
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

fn has_repeats<T: PartialEq>(items: &[T]) -> bool {
    items
        .iter()
        .enumerate()
        .any(|(index, item)| items[..index].contains(item))
}

/// The entries one part plans, from its parameters alone.
fn part_entry_count(part: &Part) -> u64 {
    match part {
        Part::Tree(part) => part.entry_count(),
        Part::Deep(part) => part.entry_count(),
        Part::File(_) | Part::Volume(_) => 1,
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
    use crate::fixture::names;

    let mut total: u64 = 1; // the root
    let mut feature_directories: Vec<&str> = Vec::new();
    for feature in &part.features {
        if !feature_directories.contains(&feature.directory()) {
            feature_directories.push(feature.directory());
            total += 1;
        }
        // A directory and a file inside it, for every name.
        let names = match feature {
            HostileFeature::ControlNames => names::control_character_names().len(),
            HostileFeature::BidiNames => names::bidi_names().len(),
            HostileFeature::EscapeNames => names::escape_sequence_names().len(),
            HostileFeature::NewlineNames => names::newline_names().len(),
            HostileFeature::InvalidUtf8Names => names::invalid_utf8_names().len(),
            HostileFeature::LongNames => names::maximum_length_names().len(),
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
        };
        total += 2 * names as u64;
    }
    total
}
