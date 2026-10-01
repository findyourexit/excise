//! Excise's `scan-report` document (version 3), as the headless runner reads it.
//!
//! The published contract is `docs/schemas/scan-report.schema.json` (with the `native-path`
//! schema it references). [`ScanDocument::read`] holds a report to it before it trusts a single
//! field: it validates the file against the published schema, one entry at a time so that the
//! file never has to fit in memory twice, and only then reads it into typed values.
//!
//! The typed values are the harness's own and carry only what the oracle diff compares. They
//! do not depend on the `excise` crate. Every path is decoded from its native-path form into the
//! bytes of an absolute path with `/` as the separator on every platform, so that a path can be
//! related to the report's root and to the oracle by plain comparison.

use std::{
    fmt,
    fs::File,
    io::{self, BufReader},
    path::Path,
};

use serde::{
    Deserialize, Deserializer,
    de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor},
};
use serde_json::{Map, Value};
use thiserror::Error;

use crate::{fixture::path::EncodedPath, string_enum::string_enum};

/// The only report version this module reads.
pub const SCAN_REPORT_VERSION: u32 = 3;

/// The published scan-report schema, as shipped in the repository's `docs/schemas`.
const SCAN_REPORT_SCHEMA: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/schemas/scan-report.schema.json"
));
/// The published native-path schema, which the scan-report schema references.
const NATIVE_PATH_SCHEMA: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/schemas/native-path.schema.json"
));
const NATIVE_PATH_SCHEMA_ID: &str =
    "https://github.com/findyourexit/excise/schemas/native-path-v1.json";

/// How many schema violations a failed read reports. The rest are counted, not listed.
const LISTED_VIOLATIONS: usize = 20;

string_enum! {
    /// The state of a whole scan report (`state`).
    pub enum ScanState {
        /// The scan covered everything it was asked to cover and has no unknown space.
        Exact => "exact",
        /// The report is usable but something is unknown: an unreadable folder, a filesystem
        /// boundary, or an exclusion.
        Uncertain => "uncertain",
        /// Scan-store capacity ended the scan after directory reduction: the report has only a
        /// summary and the root.
        SummaryOnly => "summary-only",
        /// The scan was cancelled.
        Cancelled => "cancelled",
    }
}

impl ScanState {
    /// The exit code that goes with a document in this state (`docs/reports.md`, "Exit Codes"):
    /// 0 for an exact result, 2 for a usable result with uncertainty, 3 for a partial result,
    /// and 130 for an interrupted operation.
    #[must_use]
    pub const fn exit_code(self) -> i32 {
        match self {
            Self::Exact => 0,
            Self::Uncertain => 2,
            Self::SummaryOnly => 3,
            Self::Cancelled => 130,
        }
    }
}

string_enum! {
    /// The state of one entry (`entries[].state`).
    pub enum EntryState {
        /// The entry is still being scanned.
        Scanning => "scanning",
        /// The entry and everything below it is fully accounted for.
        Complete => "complete",
        /// Something at or below the entry is unknown.
        Uncertain => "uncertain",
    }
}

/// What an entry is (`entries[].kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EntryKind {
    /// The scan root.
    Root,
    /// A directory.
    Directory,
    /// A regular file, or any other entry that is not a directory or a link.
    File,
    /// A symbolic link or reparse point, never followed.
    Link,
    /// The synthetic `Shared` entry that holds allocation shared between folders.
    Shared,
}

impl EntryKind {
    /// The name the report uses.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Root => "root",
            Self::Directory => "directory",
            Self::File => "file",
            Self::Link => "link",
            Self::Shared => "synthetic shared",
        }
    }
}

impl fmt::Display for EntryKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for EntryKind {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Named(Named),
            Synthetic { synthetic: Synthetic },
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "lowercase")]
        enum Named {
            Root,
            Directory,
            File,
            Link,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "lowercase")]
        enum Synthetic {
            Shared,
        }

        Ok(match Raw::deserialize(deserializer)? {
            Raw::Named(Named::Root) => Self::Root,
            Raw::Named(Named::Directory) => Self::Directory,
            Raw::Named(Named::File) => Self::File,
            Raw::Named(Named::Link) => Self::Link,
            Raw::Synthetic {
                synthetic: Synthetic::Shared,
            } => Self::Shared,
        })
    }
}

/// A lower bound and an upper bound that is `None` where it is unknown (`bounds`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct Bounds {
    /// What is certainly there.
    pub lower: u128,
    /// The most that can be there, or `None` when unknown. Never an apparent length.
    pub upper: Option<u128>,
}

impl fmt::Display for Bounds {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.upper {
            Some(upper) => write!(formatter, "lower {}, upper {upper}", self.lower),
            None => write!(formatter, "lower {}, upper unknown", self.lower),
        }
    }
}

/// The platform's identity for a file (`native_identity.file_id`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileId {
    /// A device and inode number.
    Inode {
        /// The device (`st_dev`).
        device: u64,
        /// The inode number (`st_ino`).
        inode: u64,
    },
    /// A volume serial number and a 64-bit file index.
    Lowres {
        /// The volume serial number.
        volume: u64,
        /// The file index.
        index: u64,
    },
    /// A volume serial number and a 128-bit file identifier.
    Highres {
        /// The volume serial number.
        volume: u64,
        /// The file identifier.
        file: u128,
    },
}

/// What the report knows of an entry's identity (`native_identity`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct Identity {
    /// The platform's identity for the file.
    pub file_id: FileId,
    /// The link count the file system declared, when it said.
    pub link_count: Option<u64>,
    /// Whether the entry is a reparse point.
    pub reparse_point: bool,
}

/// One entry of a report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportEntry {
    /// The absolute path, as `/`-separated bytes.
    pub path: Vec<u8>,
    /// What the entry is.
    pub kind: EntryKind,
    /// How complete the entry is.
    pub state: EntryState,
    /// The entry's identity, when the report has one.
    pub identity: Option<Identity>,
    /// Allocated space, counted once for each file identity.
    pub allocated: Bounds,
    /// The space that deleting the entry reclaims.
    pub reclaimable: Bounds,
    /// File length, counted for every name.
    pub apparent_bytes: u128,
    /// The entries below this one.
    pub descendants: u64,
    /// Why the entry is uncertain, when it is.
    pub unscanned_reason: Option<String>,
}

impl<'de> Deserialize<'de> for ReportEntry {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            path: EncodedPath,
            kind: EntryKind,
            state: EntryState,
            identity: Option<Identity>,
            allocated_bytes: Bounds,
            reclaimable_bytes: Bounds,
            apparent_bytes: u128,
            descendants: u64,
            unscanned_reason: Option<String>,
        }

        let raw = Raw::deserialize(deserializer)?;
        Ok(Self {
            path: raw.path.decode().map_err(de::Error::custom)?,
            kind: raw.kind,
            state: raw.state,
            identity: raw.identity,
            allocated: raw.allocated_bytes,
            reclaimable: raw.reclaimable_bytes,
            apparent_bytes: raw.apparent_bytes,
            descendants: raw.descendants,
            unscanned_reason: raw.unscanned_reason,
        })
    }
}

/// The run summary of a report (`run_summary`): counts of what the scan met.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ScanSummary {
    /// Entries the scanner visited.
    pub scanned_entries: u64,
    /// Entries identified by the identity pass.
    pub identified_entries: u64,
    /// Folders the scanner could not read.
    pub unreadable_entries: u64,
    /// Entries the scanner did not enter.
    pub unscanned_entries: u64,
    /// Entries a configured exclusion left out.
    pub excluded_entries: u64,
    /// Mount points the scanner did not cross.
    pub filesystem_boundaries: u64,
    /// Links the scanner did not follow.
    pub link_entries: u64,
    /// Entries a deletion removed.
    pub deleted_entries: u64,
    /// Entries a deletion found changed.
    pub deletion_changed_entries: u64,
    /// Entries a deletion found missing.
    pub deletion_missing_entries: u64,
    /// Entries a deletion failed on.
    pub deletion_failed_entries: u64,
    /// Entries a deletion did not attempt.
    pub deletion_unattempted_entries: u64,
    /// Bytes the scan store held at the end.
    pub scan_store_bytes: u64,
    /// The scan store's limit.
    pub scan_store_limit_bytes: u64,
    /// The last unreadable path, as display text.
    pub last_unreadable_path: Option<String>,
    /// The last path the scanner did not enter, as display text.
    pub last_unscanned_path: Option<String>,
    /// The reason for the last such path.
    pub last_unscanned_reason: Option<String>,
    /// The last worker error.
    pub last_worker_error: Option<String>,
}

/// A scan report, read and validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanDocument {
    /// The scan root, as an absolute path in `/`-separated bytes.
    pub root: Vec<u8>,
    /// The state of the whole report.
    pub state: ScanState,
    /// The run summary.
    pub summary: ScanSummary,
    /// Every entry, the root included, in the order the report lists them.
    pub entries: Vec<ReportEntry>,
}

/// A report could not be read.
#[derive(Debug, Error)]
pub enum DocumentError {
    /// The file cannot be opened or read.
    #[error("cannot read the report `{}`: {source}", path.display())]
    Io {
        /// The report file.
        path: std::path::PathBuf,
        /// The underlying error.
        source: io::Error,
    },
    /// The file is not JSON.
    #[error("the report is not valid JSON: {0}")]
    Json(serde_json::Error),
    /// The published schema cannot be compiled. This is a harness defect, not a report's.
    #[error("the published scan-report schema cannot be compiled: {0}")]
    Schema(String),
    /// The report breaks the published scan-report schema.
    #[error("the report breaks the published scan-report schema: {}", summarize(violations, *total))]
    Violations {
        /// The first violations, each with the path of the value that breaks the schema.
        violations: Vec<String>,
        /// How many violations there are in all.
        total: usize,
    },
    /// The report passes the schema but holds a value this reader cannot use: a path in another
    /// platform's encoding, say.
    #[error("the report cannot be read: {0}")]
    Unreadable(serde_json::Error),
}

fn summarize(violations: &[String], total: usize) -> String {
    let listed = violations.join("; ");
    if total > violations.len() {
        format!("{listed}; and {} more", total - violations.len())
    } else {
        listed
    }
}

impl ScanDocument {
    /// Reads the report at `path`: validates it against the published schema, then reads it.
    ///
    /// # Errors
    ///
    /// Returns [`DocumentError::Io`] or [`DocumentError::Json`] for a file that cannot be read or
    /// is not JSON, [`DocumentError::Violations`] for a report that breaks the published schema,
    /// and [`DocumentError::Unreadable`] for one that passes it but cannot be decoded.
    pub fn read(path: &Path) -> Result<Self, DocumentError> {
        validate_file(path)?;
        let file = open(path)?;
        let raw: Raw =
            serde_json::from_reader(BufReader::new(file)).map_err(DocumentError::Unreadable)?;
        Ok(Self {
            root: raw.root.decode().map_err(|error| {
                DocumentError::Unreadable(<serde_json::Error as de::Error>::custom(error))
            })?,
            state: raw.state,
            summary: raw.summary,
            entries: raw.entries,
        })
    }
}

/// The fields of a report that this reader uses. The schema, not this type, is the strict gate:
/// it forbids unknown fields everywhere.
#[derive(Deserialize)]
struct Raw {
    root: EncodedPath,
    state: ScanState,
    summary: ScanSummary,
    entries: Vec<ReportEntry>,
}

fn open(path: &Path) -> Result<File, DocumentError> {
    File::open(path).map_err(|source| DocumentError::Io {
        path: path.to_path_buf(),
        source,
    })
}

// ---------------------------------------------------------------------------------------------
// Paths.

/// The bytes of `path` in the form a report's paths decode to: `/` as the separator on every
/// platform. On Windows the path is converted lossily, as the report's own encoding would
/// otherwise carry what a `String` cannot.
#[must_use]
pub fn path_bytes(path: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;

        path.as_os_str().as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        path.to_string_lossy().replace('\\', "/").into_bytes()
    }
}

/// The part of `path` below `root`: the `/`-separated bytes of a relative path, empty for the root
/// itself. `None` when `path` is not `root` or something below it.
#[must_use]
pub fn relative_to<'a>(root: &[u8], path: &'a [u8]) -> Option<&'a [u8]> {
    let rest = path.strip_prefix(root)?;
    if rest.is_empty() {
        return Some(rest);
    }
    if root.ends_with(b"/") {
        return Some(rest);
    }
    rest.strip_prefix(b"/")
}

// ---------------------------------------------------------------------------------------------
// Schema validation.

/// The compiled published schema, and the same schema narrowed to one entry.
struct Validators {
    document: jsonschema::Validator,
    entry: jsonschema::Validator,
}

fn validators() -> Result<Validators, DocumentError> {
    let schema_error =
        |what: &str, error: &dyn fmt::Display| DocumentError::Schema(format!("{what}: {error}"));
    let scan: Value = serde_json::from_str(SCAN_REPORT_SCHEMA)
        .map_err(|error| schema_error("scan-report is not JSON", &error))?;
    let native: Value = serde_json::from_str(NATIVE_PATH_SCHEMA)
        .map_err(|error| schema_error("native-path is not JSON", &error))?;
    let registry = jsonschema::Registry::new()
        .add(NATIVE_PATH_SCHEMA_ID, native)
        .map_err(|error| schema_error("native-path has no valid identity", &error))?
        .prepare()
        .map_err(|error| schema_error("the schema registry cannot be prepared", &error))?;
    let build = |schema: &Value, what: &str| {
        jsonschema::draft202012::options()
            .with_registry(&registry)
            .build(schema)
            .map_err(|error| schema_error(what, &error))
    };

    // The entry validator is the document schema with its own constraints replaced by a reference
    // to the `entry` definition, so that every `$ref` and `$defs` entry still resolves.
    let mut entry_schema = scan.clone();
    if let Some(object) = entry_schema.as_object_mut() {
        for key in [
            "title",
            "type",
            "required",
            "additionalProperties",
            "properties",
        ] {
            object.remove(key);
        }
        object.insert("$ref".to_owned(), Value::String("#/$defs/entry".to_owned()));
    }
    Ok(Validators {
        document: build(&scan, "scan-report does not compile")?,
        entry: build(&entry_schema, "the entry definition does not compile")?,
    })
}

/// Validates the file at `path` against the published schema.
///
/// The document is read once as a stream. Each entry is validated on its own and dropped; the rest
/// of the document, with an empty `entries` array in place of the real one, is validated as a
/// whole. The two together are exactly what the schema says of the document, because the schema
/// constrains `entries` only through its items.
fn validate_file(path: &Path) -> Result<(), DocumentError> {
    let validators = validators()?;
    let mut violations = Violations::default();
    let file = open(path)?;
    let mut deserializer = serde_json::Deserializer::from_reader(BufReader::new(file));
    let shell = Shell {
        entry: &validators.entry,
        violations: &mut violations,
    }
    .deserialize(&mut deserializer)
    .map_err(DocumentError::Json)?;
    deserializer.end().map_err(DocumentError::Json)?;
    for error in validators.document.iter_errors(&Value::Object(shell)) {
        violations.add(format!("{} ({})", error, error.instance_path()));
    }
    if violations.total == 0 {
        Ok(())
    } else {
        Err(DocumentError::Violations {
            violations: violations.listed,
            total: violations.total,
        })
    }
}

#[derive(Default)]
struct Violations {
    listed: Vec<String>,
    total: usize,
}

impl Violations {
    fn add(&mut self, violation: String) {
        self.total += 1;
        if self.listed.len() < LISTED_VIOLATIONS {
            self.listed.push(violation);
        }
    }
}

/// The document as a stream: every key but `entries` is kept, and `entries` is checked item by
/// item.
struct Shell<'a> {
    entry: &'a jsonschema::Validator,
    violations: &'a mut Violations,
}

impl<'de> DeserializeSeed<'de> for Shell<'_> {
    type Value = Map<String, Value>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for Shell<'_> {
    type Value = Map<String, Value>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a scan-report object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let Self { entry, violations } = self;
        let mut shell = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if key == "entries" {
                map.next_value_seed(Entries {
                    validator: entry,
                    violations: &mut *violations,
                })?;
                shell.insert(key, Value::Array(Vec::new()));
            } else {
                shell.insert(key, map.next_value()?);
            }
        }
        Ok(shell)
    }
}

/// The `entries` array, validated item by item.
struct Entries<'a, 'b> {
    validator: &'a jsonschema::Validator,
    violations: &'b mut Violations,
}

impl<'de> DeserializeSeed<'de> for Entries<'_, '_> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_seq(self)
    }
}

impl<'de> Visitor<'de> for Entries<'_, '_> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an array of entries")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        let mut index = 0_usize;
        while let Some(entry) = seq.next_element::<Value>()? {
            if !self.validator.is_valid(&entry) {
                for error in self.validator.iter_errors(&entry) {
                    self.violations.add(format!(
                        "entries[{index}]: {} ({})",
                        error,
                        error.instance_path()
                    ));
                }
            }
            index += 1;
        }
        Ok(())
    }
}
