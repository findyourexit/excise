//! Fixture-relative paths as raw bytes, and their lossless JSON encoding.
//!
//! A fixture is a tree of names, and a name is a sequence of bytes on Unix: the hostile class
//! deliberately creates names that are not valid UTF-8. [`RelPath`] therefore holds bytes, not a
//! `String` or an `OsString`, and joins components with `/` on every platform, so a plan, its
//! hash, and its canonical order are identical on every machine.
//!
//! When a path leaves the process (the manifest, the oracle, the ownership marker), it is
//! written with the same encoding the Excise product uses for the paths in its JSON reports
//! (`docs/schemas/native-path.schema.json`): an object with an `encoding` tag and base64 `data`.
//! On Unix the tag is `unix-bytes`; on Windows `windows-utf16-le`, with `\` as the separator, as
//! `EncodedNativePath::encode` in the product produces; elsewhere `utf8`. A `utf8` payload is
//! accepted on every platform.

use std::{
    cmp::Ordering,
    fmt,
    path::{Path, PathBuf},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

use crate::scenario::{PathViolation, check_fixture_relative_path};

/// Why bytes are not a valid fixture-relative path, or why an encoded path cannot be read.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PathError {
    /// A component is empty: a leading, doubled, or trailing `/`.
    #[error("a path component is empty (a leading, doubled, or trailing `/`)")]
    EmptyComponent,
    /// A component is `.` or `..`.
    #[error("a path component is `.` or `..`")]
    DotComponent,
    /// The path contains a NUL byte, which no file system accepts in a name.
    #[error("a path contains a NUL byte")]
    Nul,
    /// The base64 payload is malformed.
    #[error("invalid base64 path payload: {0}")]
    Base64(#[from] base64::DecodeError),
    /// The payload uses an encoding that belongs to another platform.
    #[error("path encoding `{0}` is not native to this platform")]
    WrongPlatform(&'static str),
    /// A UTF-16 payload has an odd number of bytes.
    #[error("UTF-16 path payload has an odd byte length")]
    OddUtf16Length,
    /// A UTF-16 payload is not valid Unicode, or a UTF-8 payload is not valid text.
    #[error("path payload is not valid Unicode text")]
    InvalidText,
}

/// A path below a fixture root, as raw bytes.
///
/// The path is `/`-separated. Every component is non-empty, is neither `.` nor `..`, and contains
/// no NUL byte. The empty path is the root itself. Paths order canonically: component by
/// component in raw byte order, so a directory sorts directly before its contents.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct RelPath(Vec<u8>);

impl RelPath {
    /// The fixture root.
    #[must_use]
    pub const fn root() -> Self {
        Self(Vec::new())
    }

    /// Validates raw bytes as a relative path.
    ///
    /// # Errors
    ///
    /// Returns the first [`PathError`] found: an empty component, a `.` or `..` component, or a
    /// NUL byte.
    pub fn from_bytes(bytes: impl Into<Vec<u8>>) -> Result<Self, PathError> {
        let bytes = bytes.into();
        if bytes.is_empty() {
            return Ok(Self(bytes));
        }
        if bytes.contains(&0) {
            return Err(PathError::Nul);
        }
        for component in bytes.split(|byte| *byte == b'/') {
            match component {
                [] => return Err(PathError::EmptyComponent),
                b"." | b".." => return Err(PathError::DotComponent),
                _ => {}
            }
        }
        Ok(Self(bytes))
    }

    /// Parses a path the way a scenario writes it, applying the fixture-relative path rule that
    /// `Scenario::validate` applies to every path in a scenario file.
    ///
    /// # Errors
    ///
    /// Returns the [`PathViolation`] that makes the path unsafe or non-canonical.
    pub fn from_scenario_path(path: &str) -> Result<Self, PathViolation> {
        check_fixture_relative_path(path)?;
        Ok(Self(path.as_bytes().to_vec()))
    }

    /// Appends one component. The caller must supply a valid component: non-empty, without `/`
    /// or NUL, and neither `.` nor `..`. The generators build names from known-good parts.
    #[must_use]
    pub(crate) fn join(&self, name: &[u8]) -> Self {
        debug_assert!(is_valid_component(name), "invalid path component {name:?}");
        let mut bytes = Vec::with_capacity(self.0.len() + 1 + name.len());
        bytes.extend_from_slice(&self.0);
        if !self.0.is_empty() {
            bytes.push(b'/');
        }
        bytes.extend_from_slice(name);
        Self(bytes)
    }

    /// Appends one component, checking it.
    ///
    /// # Errors
    ///
    /// Returns a [`PathError`] when `name` is not a single valid component.
    pub fn try_join(&self, name: &[u8]) -> Result<Self, PathError> {
        if name.contains(&b'/') {
            return Err(PathError::EmptyComponent);
        }
        let component = Self::from_bytes(name)?;
        if component.is_root() {
            return Err(PathError::EmptyComponent);
        }
        Ok(self.join(name))
    }

    /// Whether this is the fixture root.
    #[must_use]
    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    /// The path as `/`-separated bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// The components, in order. The root has none.
    #[must_use]
    pub fn components(&self) -> impl DoubleEndedIterator<Item = &[u8]> + Clone {
        let bytes: &[u8] = &self.0;
        bytes
            .split(|byte| *byte == b'/')
            .filter(|part| !part.is_empty())
    }

    /// The number of components; the root has depth 0.
    #[must_use]
    pub fn depth(&self) -> usize {
        self.components().count()
    }

    /// The last component, or `None` for the root.
    #[must_use]
    pub fn file_name(&self) -> Option<&[u8]> {
        self.components().next_back()
    }

    /// The bytes of the parent path: empty for a top-level entry and for the root.
    #[must_use]
    pub fn parent_bytes(&self) -> &[u8] {
        match self.0.iter().rposition(|byte| *byte == b'/') {
            Some(index) => &self.0[..index],
            None => &[],
        }
    }

    /// The parent path, or `None` for the root.
    #[must_use]
    pub fn parent(&self) -> Option<Self> {
        if self.is_root() {
            None
        } else {
            Some(Self(self.parent_bytes().to_vec()))
        }
    }

    /// Whether `ancestor` is this path or one of its ancestors, component by component.
    #[must_use]
    pub fn starts_with(&self, ancestor: &Self) -> bool {
        ancestor.is_root()
            || self.0 == ancestor.0
            || (self.0.len() > ancestor.0.len()
                && self.0.starts_with(&ancestor.0)
                && self.0[ancestor.0.len()] == b'/')
    }

    /// This path below `root`, as a native path.
    ///
    /// On Windows a component that is not valid UTF-8 is converted lossily; the generator never
    /// creates one there.
    #[must_use]
    pub fn to_path_buf(&self, root: &Path) -> PathBuf {
        let mut path = root.to_path_buf();
        for component in self.components() {
            path.push(component_to_os(component));
        }
        path
    }
}

/// Whether `name` can be one component of a [`RelPath`].
pub(crate) fn is_valid_component(name: &[u8]) -> bool {
    !name.is_empty() && name != b"." && name != b".." && !name.contains(&b'/') && !name.contains(&0)
}

#[cfg(unix)]
fn component_to_os(component: &[u8]) -> std::ffi::OsString {
    use std::os::unix::ffi::OsStrExt as _;

    std::ffi::OsStr::from_bytes(component).to_os_string()
}

#[cfg(not(unix))]
fn component_to_os(component: &[u8]) -> std::ffi::OsString {
    std::ffi::OsString::from(String::from_utf8_lossy(component).into_owned())
}

impl Ord for RelPath {
    fn cmp(&self, other: &Self) -> Ordering {
        canonical_cmp(&self.0, &other.0)
    }
}

impl PartialOrd for RelPath {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Compares two `/`-separated byte paths component by component. `/` sorts before every other
/// byte (no component contains NUL, so the mapping cannot collide), which puts `a/b` before
/// `a.b`: a directory is followed by its contents, in depth-first order.
pub(crate) fn canonical_cmp(left: &[u8], right: &[u8]) -> Ordering {
    let rank = |byte: u8| if byte == b'/' { 0 } else { byte };
    left.iter()
        .map(|byte| rank(*byte))
        .cmp(right.iter().map(|byte| rank(*byte)))
}

impl fmt::Debug for RelPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "RelPath({self})")
    }
}

/// Writes the path with control characters, bidirectional overrides, and invalid UTF-8 escaped,
/// so a hostile name cannot disturb a terminal or a log. Not a lossless form; the JSON encoding
/// is.
impl fmt::Display for RelPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_escaped(formatter, &self.0)
    }
}

/// Writes `bytes` escaped for display.
pub(crate) fn write_escaped(out: &mut impl fmt::Write, bytes: &[u8]) -> fmt::Result {
    for chunk in bytes.utf8_chunks() {
        for character in chunk.valid().chars() {
            if character.is_control() || is_bidi_control(character) {
                write!(out, "\\u{{{:x}}}", u32::from(character))?;
            } else {
                out.write_char(character)?;
            }
        }
        for byte in chunk.invalid() {
            write!(out, "\\x{byte:02x}")?;
        }
    }
    Ok(())
}

/// Left-to-right and right-to-left marks, embeddings, overrides, and isolates.
const fn is_bidi_control(character: char) -> bool {
    matches!(
        character,
        '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
    )
}

/// The JSON form of a native path: the same shape as the product's `EncodedNativePath`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "encoding", content = "data", rename_all = "kebab-case")]
enum Encoded {
    UnixBytes(String),
    WindowsUtf16Le(String),
    Utf8(String),
}

#[cfg(unix)]
fn encode_native(bytes: &[u8]) -> Encoded {
    Encoded::UnixBytes(STANDARD.encode(bytes))
}

#[cfg(windows)]
fn encode_native(bytes: &[u8]) -> Encoded {
    let text = String::from_utf8_lossy(bytes).replace('/', "\\");
    let units: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
    Encoded::WindowsUtf16Le(STANDARD.encode(units))
}

#[cfg(not(any(unix, windows)))]
fn encode_native(bytes: &[u8]) -> Encoded {
    Encoded::Utf8(String::from_utf8_lossy(bytes).into_owned())
}

#[cfg(unix)]
fn decode_native(encoded: &Encoded) -> Result<Vec<u8>, PathError> {
    match encoded {
        Encoded::UnixBytes(data) => Ok(STANDARD.decode(data)?),
        Encoded::Utf8(text) => Ok(text.as_bytes().to_vec()),
        Encoded::WindowsUtf16Le(_) => Err(PathError::WrongPlatform("windows-utf16-le")),
    }
}

#[cfg(windows)]
fn decode_native(encoded: &Encoded) -> Result<Vec<u8>, PathError> {
    match encoded {
        Encoded::WindowsUtf16Le(data) => {
            let bytes = STANDARD.decode(data)?;
            if bytes.len() % 2 != 0 {
                return Err(PathError::OddUtf16Length);
            }
            let units: Vec<u16> = bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| u16::from_le_bytes(*pair))
                .collect();
            let text = String::from_utf16(&units).map_err(|_| PathError::InvalidText)?;
            Ok(text.replace('\\', "/").into_bytes())
        }
        Encoded::Utf8(text) => Ok(text.replace('\\', "/").into_bytes()),
        Encoded::UnixBytes(_) => Err(PathError::WrongPlatform("unix-bytes")),
    }
}

#[cfg(not(any(unix, windows)))]
fn decode_native(encoded: &Encoded) -> Result<Vec<u8>, PathError> {
    match encoded {
        Encoded::Utf8(text) => Ok(text.as_bytes().to_vec()),
        Encoded::UnixBytes(_) => Err(PathError::WrongPlatform("unix-bytes")),
        Encoded::WindowsUtf16Le(_) => Err(PathError::WrongPlatform("windows-utf16-le")),
    }
}

impl Serialize for RelPath {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        encode_native(&self.0).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for RelPath {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let encoded = Encoded::deserialize(deserializer)?;
        let bytes = decode_native(&encoded).map_err(serde::de::Error::custom)?;
        Self::from_bytes(bytes).map_err(serde::de::Error::custom)
    }
}

/// The text of a symbolic link, as raw bytes.
///
/// Unlike a [`RelPath`], a link target may hold `..` components and any other bytes a link can
/// carry, and it is not validated. It uses the same JSON encoding as a path.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct LinkTarget(Vec<u8>);

impl LinkTarget {
    /// Wraps the bytes of a link target.
    #[must_use]
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Self(bytes.into())
    }

    /// The bytes of the target.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for LinkTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "LinkTarget({self})")
    }
}

impl fmt::Display for LinkTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_escaped(formatter, &self.0)
    }
}

impl Serialize for LinkTarget {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        encode_native(&self.0).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for LinkTarget {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let encoded = Encoded::deserialize(deserializer)?;
        decode_native(&encoded)
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use std::cmp::Ordering;

    #[cfg(unix)]
    use serde_json::json;

    #[cfg(unix)]
    use super::LinkTarget;
    use super::{PathError, RelPath, canonical_cmp};

    fn path(text: &str) -> RelPath {
        RelPath::from_bytes(text).unwrap_or_else(|error| panic!("`{text}` should parse: {error}"))
    }

    #[test]
    fn from_bytes_accepts_only_normal_components() {
        assert!(path("").is_root());
        assert_eq!(path("a/b/c").depth(), 3);
        assert_eq!(RelPath::from_bytes("/a"), Err(PathError::EmptyComponent));
        assert_eq!(RelPath::from_bytes("a/"), Err(PathError::EmptyComponent));
        assert_eq!(RelPath::from_bytes("a//b"), Err(PathError::EmptyComponent));
        assert_eq!(RelPath::from_bytes("a/./b"), Err(PathError::DotComponent));
        assert_eq!(RelPath::from_bytes(".."), Err(PathError::DotComponent));
        assert_eq!(RelPath::from_bytes(*b"a\0b"), Err(PathError::Nul));
        // Dots inside a longer name are fine.
        assert!(RelPath::from_bytes("..a/b..").is_ok());
    }

    #[test]
    fn scenario_paths_go_through_the_scenario_rule() {
        assert!(RelPath::from_scenario_path("keep-b/keep.txt").is_ok());
        assert!(RelPath::from_scenario_path("../escape").is_err());
        assert!(RelPath::from_scenario_path("/abs").is_err());
        assert!(RelPath::from_scenario_path("C:/x").is_err());
    }

    #[test]
    fn try_join_rejects_anything_but_one_component() {
        let base = path("a");
        assert_eq!(
            base.try_join(b"b").map(|joined| joined.to_string()),
            Ok("a/b".to_owned())
        );
        assert!(base.try_join(b"b/c").is_err());
        assert!(base.try_join(b"").is_err());
        assert!(base.try_join(b"..").is_err());
        assert_eq!(RelPath::root().try_join(b"x"), Ok(path("x")));
    }

    #[test]
    fn navigation_helpers_agree_with_each_other() {
        let deep = path("a/bb/ccc");
        assert_eq!(deep.file_name(), Some(&b"ccc"[..]));
        assert_eq!(deep.parent(), Some(path("a/bb")));
        assert_eq!(path("a").parent(), Some(RelPath::root()));
        assert_eq!(RelPath::root().parent(), None);
        assert_eq!(deep.parent_bytes(), b"a/bb");
        assert_eq!(path("a").parent_bytes(), b"");
        assert!(deep.starts_with(&path("a")));
        assert!(deep.starts_with(&deep));
        assert!(deep.starts_with(&RelPath::root()));
        assert!(
            !deep.starts_with(&path("a/b")),
            "`a/b` is not a component prefix of `a/bb`"
        );
        assert!(!path("a").starts_with(&deep));
    }

    #[test]
    fn canonical_order_lists_a_directory_before_its_contents() {
        // Plain byte order would put `a.b` (0x2e) before `a/b` (0x2f).
        let mut paths = [path("a.b"), path("a/b"), path("a"), path("a/a"), path("b")];
        paths.sort();
        let sorted: Vec<String> = paths.iter().map(ToString::to_string).collect();
        assert_eq!(sorted, ["a", "a/a", "a/b", "a.b", "b"]);
        assert_eq!(canonical_cmp(b"a", b"a/x"), Ordering::Less);
        assert_eq!(canonical_cmp(b"a/x", b"a"), Ordering::Greater);
        assert_eq!(canonical_cmp(b"same", b"same"), Ordering::Equal);
    }

    #[test]
    fn display_escapes_what_could_disturb_a_terminal() {
        let hostile = RelPath::from_bytes(b"e\x1b[31mred\nline\xff\xe2\x80\xaefdp".to_vec())
            .unwrap_or_else(|error| panic!("{error}"));
        let shown = hostile.to_string();
        assert_eq!(shown, "e\\u{1b}[31mred\\u{a}line\\xff\\u{202e}fdp");
        assert!(shown.chars().all(|character| !character.is_control()));
    }

    #[cfg(unix)]
    #[test]
    fn json_encoding_is_the_products_unix_bytes_shape_and_round_trips_any_bytes() {
        let value = serde_json::to_value(path("a/b")).unwrap_or_default();
        assert_eq!(value, json!({ "encoding": "unix-bytes", "data": "YS9i" }));

        // Invalid UTF-8 survives, which is the reason for the encoding.
        let hostile = RelPath::from_bytes(b"bad-\xff\xfe/\x80".to_vec())
            .unwrap_or_else(|error| panic!("{error}"));
        let text = serde_json::to_string(&hostile).unwrap_or_default();
        let back: RelPath = serde_json::from_str(&text).unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(back, hostile);

        // The root is the empty payload.
        let root = serde_json::to_value(RelPath::root()).unwrap_or_default();
        assert_eq!(root, json!({ "encoding": "unix-bytes", "data": "" }));
    }

    #[cfg(unix)]
    #[test]
    fn json_decoding_rejects_foreign_and_invalid_payloads() {
        let foreign = r#"{"encoding":"windows-utf16-le","data":"YQA="}"#;
        assert!(serde_json::from_str::<RelPath>(foreign).is_err());
        let not_base64 = r#"{"encoding":"unix-bytes","data":"!!"}"#;
        assert!(serde_json::from_str::<RelPath>(not_base64).is_err());
        // An encoded path that is not a valid relative path is rejected, not smuggled through.
        let escaping = serde_json::to_string(&json!({
            "encoding": "unix-bytes",
            "data": "Li4vZXNjYXBl", // ../escape
        }))
        .unwrap_or_default();
        assert!(serde_json::from_str::<RelPath>(&escaping).is_err());
        // `utf8` is accepted on every platform.
        let plain = r#"{"encoding":"utf8","data":"x/y"}"#;
        assert_eq!(
            serde_json::from_str::<RelPath>(plain).ok(),
            Some(path("x/y"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn link_targets_keep_parent_components_and_raw_bytes() {
        let target = LinkTarget::new(b"../dir/\xff".to_vec());
        let text = serde_json::to_string(&target).unwrap_or_default();
        let back: LinkTarget =
            serde_json::from_str(&text).unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(back, target);
        assert_eq!(target.to_string(), "../dir/\\xff");
    }
}
