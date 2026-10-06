//! Excise's `scan-report` document, versions 1 and 3, as the headless runner reads it.
//!
//! The published contract is one JSON schema per version, with the `native-path` schema they both
//! reference. [`ScanDocument::read`] reads the `schema_version` a report declares and holds the
//! report to that version's schema before it trusts a single field: it validates the file against
//! the schema, one entry at a time so that the file never has to fit in memory twice, and only then
//! reads it into typed values.
//!
//! * Version 3 is `docs/schemas/scan-report.schema.json`: the report of Excise 1.3.0 and of every
//!   later build. It is the file that v1.3.0 published, and the repository's copy was identical to
//!   it when the legacy schema was added.
//! * Version 1 is `legacy/scan-report-v1.schema.json` in this crate: the report of Excise 1.0.0
//!   through 1.2.4. It is a byte-for-byte copy of the schema those ten releases published as
//!   `docs/schemas/scan-report.schema.json`, which is one and the same file in v1.0.0, v1.0.1,
//!   v1.0.2, v1.1.0, v1.1.1, v1.2.0, v1.2.1, v1.2.2, v1.2.3, and v1.2.4: its SHA-256 is
//!   `2d60bc0ef78d133a2fb029ad2fef80c1298f2b4a5d196b2646a7805009c47725`. A build is therefore held
//!   to the contract it published, not to the current one. The `native-path` schema is the same
//!   file in all eleven releases.
//!
//! Apart from the schema's own `$id`, title, and `schema_version`, the two versions differ in four
//! places, and the typed values carry each of them:
//!
//! * a report's `state` is `summary-only` in version 3 only ([`ScanState::SummaryOnly`]);
//! * an entry's `state` is `aggregated` in version 1 only ([`EntryState::Aggregated`]);
//! * an entry's `kind` is `{"synthetic": "other"}` or `{"synthetic": "aggregate"}` in version 1
//!   only ([`EntryKind::Other`], [`EntryKind::Aggregate`]);
//! * the run summary gives the size of the scan store and its limit as `scan_store_bytes` and
//!   `scan_store_limit_bytes` in version 3, and as `model_bytes` and `model_limit_bytes` in
//!   version 1 ([`ScanSummary`] reads either name into the version-3 field).
//!
//! Version 1 also has a run-summary flag, `identity_spilled`, that the typed values do not carry:
//! nothing in the oracle diff compares it.
//!
//! The typed values are the harness's own and carry only what the oracle diff compares. They
//! do not depend on the `excise` crate. Every path is decoded from its native-path form into the
//! bytes of an absolute path with `/` as the separator on every platform, so that a path can be
//! related to the report's root and to the oracle by plain comparison.

use std::{
    fmt,
    fs::File,
    io::{self, BufReader, Read},
    path::Path,
};

use serde::{
    Deserialize, Deserializer,
    de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor},
};
use serde_json::{Map, Value};
use thiserror::Error;

use crate::{fixture::path::EncodedPath, string_enum::string_enum};

/// The report version that Excise 1.3.0 and every later build write, held to
/// `docs/schemas/scan-report.schema.json`.
pub const SCAN_REPORT_VERSION: u32 = 3;

/// The report version that Excise 1.0.0 through 1.2.4 write, held to
/// `legacy/scan-report-v1.schema.json`.
pub const LEGACY_SCAN_REPORT_VERSION: u32 = 1;

/// The published scan-report schema of version 3, as shipped in the repository's `docs/schemas`.
const SCAN_REPORT_SCHEMA: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/schemas/scan-report.schema.json"
));
/// The scan-report schema of version 1: a byte-for-byte copy of the one that each of Excise 1.0.0
/// through 1.2.4 published in its `docs/schemas`.
const LEGACY_SCAN_REPORT_SCHEMA: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/legacy/scan-report-v1.schema.json"
));
/// The published native-path schema, which both scan-report schemas reference.
const NATIVE_PATH_SCHEMA: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/schemas/native-path.schema.json"
));
const NATIVE_PATH_SCHEMA_ID: &str =
    "https://github.com/findyourexit/excise/schemas/native-path-v1.json";

/// How many schema violations a failed read reports. The rest are counted, not listed.
const LISTED_VIOLATIONS: usize = 20;

/// How many bytes from the start of a report are searched for its `schema_version`.
const VERSION_WINDOW: usize = 4096;

/// The name of the member that declares a report's version, with its quotes.
const VERSION_MEMBER: &[u8] = b"\"schema_version\"";

string_enum! {
    /// The state of a whole scan report (`state`).
    pub enum ScanState {
        /// The scan covered everything it was asked to cover and has no unknown space.
        Exact => "exact",
        /// The report is usable but something is unknown: an unreadable folder, a filesystem
        /// boundary, or an exclusion.
        Uncertain => "uncertain",
        /// Scan-store capacity ended the scan after directory reduction: the report has only a
        /// summary and the root. Version 3 only: version 1 has no such state.
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
        /// Version 1 only: the scan's memory limit folded the entry together with others, and what
        /// was below it is no longer listed.
        Aggregated => "aggregated",
    }
}

/// What an entry is (`entries[].kind`).
///
/// A report of version 3 has one synthetic kind, [`EntryKind::Shared`]. A report of version 1
/// also has [`EntryKind::Other`] and [`EntryKind::Aggregate`].
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
    /// Version 1 only: the synthetic `Other` entry, which stands for entries that the scan's memory
    /// limit kept out of a folder's own listing. It is not the fixture oracle's `NodeKind::Other`
    /// (a FIFO, socket, or device), which a report writes as a `file`.
    Other,
    /// Version 1 only: a folder that the scan's memory limit folded into one entry. Its totals are
    /// kept and the entries below it are not listed.
    Aggregate,
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
            Self::Other => "synthetic other",
            Self::Aggregate => "synthetic aggregate",
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
            Other,
            Aggregate,
        }

        Ok(match Raw::deserialize(deserializer)? {
            Raw::Named(Named::Root) => Self::Root,
            Raw::Named(Named::Directory) => Self::Directory,
            Raw::Named(Named::File) => Self::File,
            Raw::Named(Named::Link) => Self::Link,
            Raw::Synthetic {
                synthetic: Synthetic::Shared,
            } => Self::Shared,
            Raw::Synthetic {
                synthetic: Synthetic::Other,
            } => Self::Other,
            Raw::Synthetic {
                synthetic: Synthetic::Aggregate,
            } => Self::Aggregate,
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
///
/// A report of version 1 names the two sizes of the scan store `model_bytes` and
/// `model_limit_bytes`, and both versions read into the fields below. The `identity_spilled` flag
/// that version 1 also has is not carried.
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
    /// Bytes the scan store held at the end. A report of version 1 has no scan store and gives the
    /// memory its in-memory model held, as `model_bytes`.
    #[serde(alias = "model_bytes")]
    pub scan_store_bytes: u64,
    /// The scan store's limit. A report of version 1 gives the limit of its in-memory model, as
    /// `model_limit_bytes`.
    #[serde(alias = "model_limit_bytes")]
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

/// A scan report, read and validated: a report of version 1 or of version 3.
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
    /// The report declares a `schema_version` that this reader does not read.
    #[error(
        "the report declares schema_version {version}, but only versions {} and {} are read",
        LEGACY_SCAN_REPORT_VERSION,
        SCAN_REPORT_VERSION
    )]
    UnsupportedVersion {
        /// The version the report declares.
        version: u64,
    },
    /// The start of the report declares no `schema_version` that can be read: the member is
    /// absent, lies past the first 4,096 bytes, or has a value that is not an unsigned integer.
    /// Only [`ScanDocument::version_of`] reports it: [`ScanDocument::read`] holds such a report to
    /// version 3.
    #[error(
        "the report declares no schema_version, as an unsigned integer, in its first {} bytes",
        VERSION_WINDOW
    )]
    NoVersion,
    /// The published schema cannot be compiled. This is a harness defect, not a report's.
    #[error("the published scan-report schema cannot be compiled: {0}")]
    Schema(String),
    /// The report breaks the published scan-report schema of the version it declares.
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
    /// Reads the report at `path`: validates it against the published schema of the version it
    /// declares, then reads it.
    ///
    /// The version is the `schema_version` that the first 4,096 bytes of the file declare: 3 holds
    /// the report to the schema in `docs/schemas`, and 1 to the legacy schema of Excise 1.0.0
    /// through 1.2.4. A report with no readable `schema_version` in that window (the member is
    /// absent or lies further on, or its value is not an unsigned integer) is held to version 3,
    /// and the schema says what is wrong with it. Excise writes the member second, so the window
    /// is never a limit for a report of the product.
    ///
    /// # Errors
    ///
    /// Returns [`DocumentError::Io`] or [`DocumentError::Json`] for a file that cannot be read or
    /// is not JSON, [`DocumentError::UnsupportedVersion`] for a report that declares a version
    /// other than 1 or 3, [`DocumentError::Violations`] for a report that breaks the published
    /// schema of its version, and [`DocumentError::Unreadable`] for one that passes it but cannot
    /// be decoded.
    pub fn read(path: &Path) -> Result<Self, DocumentError> {
        let version = match declared_version(path)? {
            Some(declared) => Version::from_declared(declared)?,
            None => Version::Current,
        };
        validate_file(path, version)?;
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

    /// The version of the report at `path`: the `schema_version` that the first 4,096 bytes of the
    /// file declare, found as [`ScanDocument::read`] finds it.
    ///
    /// Where `read` holds a report with no readable version to version 3, this is an error, so
    /// that a caller that records which version a build wrote does not record a guess.
    ///
    /// # Errors
    ///
    /// Returns [`DocumentError::Io`] for a file that cannot be read, [`DocumentError::NoVersion`]
    /// for one whose start declares no readable `schema_version`, and
    /// [`DocumentError::UnsupportedVersion`] for one that declares a version other than 1 or 3.
    pub fn version_of(path: &Path) -> Result<u32, DocumentError> {
        let declared = declared_version(path)?.ok_or(DocumentError::NoVersion)?;
        Version::from_declared(declared).map(Version::number)
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
// Versions.

/// A report version that this module reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Version {
    /// Version 1, the report of Excise 1.0.0 through 1.2.4.
    Legacy,
    /// Version 3, the report of Excise 1.3.0 and later.
    Current,
}

impl Version {
    /// Every version that this module reads.
    const ALL: [Self; 2] = [Self::Legacy, Self::Current];

    /// The version that a report declaring `declared` is held to.
    fn from_declared(declared: u64) -> Result<Self, DocumentError> {
        Self::ALL
            .into_iter()
            .find(|version| u64::from(version.number()) == declared)
            .ok_or(DocumentError::UnsupportedVersion { version: declared })
    }

    /// The `schema_version` that a report of this version declares.
    const fn number(self) -> u32 {
        match self {
            Self::Legacy => LEGACY_SCAN_REPORT_VERSION,
            Self::Current => SCAN_REPORT_VERSION,
        }
    }

    /// The text of this version's published scan-report schema.
    const fn schema(self) -> &'static str {
        match self {
            Self::Legacy => LEGACY_SCAN_REPORT_SCHEMA,
            Self::Current => SCAN_REPORT_SCHEMA,
        }
    }
}

/// What the first `VERSION_WINDOW` bytes of the file at `path` declare as their `schema_version`,
/// or `None` when they declare none that `version_in` can read.
fn declared_version(path: &Path) -> Result<Option<u64>, DocumentError> {
    let mut file = open(path)?;
    let mut window = [0_u8; VERSION_WINDOW];
    let filled = fill(&mut file, &mut window).map_err(|source| DocumentError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(version_in(&window[..filled]))
}

/// Reads from `reader` until `buffer` is full or the reader ends, and returns how many bytes it
/// read. One `read` can return less than is left, so a single call would not do.
fn fill(reader: &mut impl Read, buffer: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(count) => filled += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

/// The version that `window`, the start of a report, declares: the value of its first
/// `"schema_version"` member, when that value is an unsigned integer that ends inside `window`.
/// `None` when there is no such member or its value is anything else.
///
/// The window is searched as bytes, not parsed, so a report need not be valid JSON for this and
/// may end in the middle of a token. A string value that says `schema_version` is not a member: a
/// member name is followed by a colon, and a string that quotes the name carries its quotes
/// escaped.
fn version_in(window: &[u8]) -> Option<u64> {
    let mut rest = window;
    while let Some(start) = rest
        .windows(VERSION_MEMBER.len())
        .position(|candidate| candidate == VERSION_MEMBER)
    {
        let after = rest[start + VERSION_MEMBER.len()..].trim_ascii_start();
        if let Some(value) = after.strip_prefix(b":") {
            return unsigned(value.trim_ascii_start());
        }
        rest = &rest[start + 1..];
    }
    None
}

/// The unsigned integer that `text` starts with. `None` when `text` does not start with digits,
/// when the digits are followed by anything but white space, a comma, or a closing brace (a
/// fraction, an exponent, or the end of `text`, where the number may go on), when they begin with
/// a zero that is not the whole number (JSON allows none), or when they do not fit in a `u64`.
fn unsigned(text: &[u8]) -> Option<u64> {
    let length = text.iter().take_while(|byte| byte.is_ascii_digit()).count();
    let (digits, rest) = text.split_at(length);
    let ends = matches!(
        rest.first().copied(),
        Some(b' ' | b'\t' | b'\n' | b'\r' | b',' | b'}')
    );
    if digits.is_empty() || (digits.len() > 1 && digits.starts_with(b"0")) || !ends {
        return None;
    }
    digits.iter().try_fold(0_u64, |value, figure| {
        value
            .checked_mul(10)?
            .checked_add(u64::from(*figure - b'0'))
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

/// The compiled published schema of one report version, and the same schema narrowed to one
/// entry.
struct Validators {
    document: jsonschema::Validator,
    entry: jsonschema::Validator,
}

fn validators(version: Version) -> Result<Validators, DocumentError> {
    let schema_error =
        |what: &str, error: &dyn fmt::Display| DocumentError::Schema(format!("{what}: {error}"));
    let scan: Value = serde_json::from_str(version.schema())
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

/// Validates the file at `path` against the published schema of `version`.
///
/// The document is read once as a stream. Each entry is validated on its own and dropped; the rest
/// of the document, with an empty `entries` array in place of the real one, is validated as a
/// whole. The two together are exactly what the schema says of the document, because the schema
/// constrains `entries` only through its items.
fn validate_file(path: &Path, version: Version) -> Result<(), DocumentError> {
    let validators = validators(version)?;
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

#[cfg(test)]
mod tests;
