use std::ffi::{OsStr, OsString};
use std::fs::{self, Metadata};
use std::io;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::error::AppError;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct NativePath(PathBuf);

impl NativePath {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }

    #[must_use]
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    #[must_use]
    pub fn safe_display(&self) -> SafeDisplayPath {
        safe_display_path(&self.0)
    }

    #[must_use]
    pub fn encode(&self) -> EncodedNativePath {
        encode_path(&self.0)
    }

    /// # Errors
    /// Returns a codec error when the payload is malformed or belongs to another platform.
    pub fn decode(encoded: &EncodedNativePath) -> Result<Self, PathCodecError> {
        decode_path(encoded).map(Self)
    }
}

impl AsRef<Path> for NativePath {
    fn as_ref(&self) -> &Path {
        self.as_path()
    }
}

impl From<PathBuf> for NativePath {
    fn from(path: PathBuf) -> Self {
        Self::new(path)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SafeDisplayPath {
    pub text: String,
    pub deceptive: bool,
}
/// Prefix shown whenever a display value contains deceptive native text.
pub const DECEPTIVE_DISPLAY_MARKER: &str = "[deceptive]";

fn marked(displayed: SafeDisplayPath) -> String {
    if displayed.deceptive {
        format!("{DECEPTIVE_DISPLAY_MARKER} {}", displayed.text)
    } else {
        displayed.text
    }
}

#[must_use]
pub fn safe_display_path_text(path: &Path) -> String {
    marked(safe_display_path(path))
}

#[must_use]
pub fn safe_display_os_str_text(value: &OsStr) -> String {
    marked(safe_display_os_str(value))
}

#[must_use]
pub fn safe_display_text(value: &str) -> String {
    let displayed = safe_display_os_str(OsStr::new(value));
    if displayed.deceptive {
        marked(displayed)
    } else if value.contains(DECEPTIVE_DISPLAY_MARKER) {
        value.to_string()
    } else {
        displayed.text
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "encoding", content = "data", rename_all = "kebab-case")]
pub enum EncodedNativePath {
    UnixBytes(String),
    WindowsUtf16Le(String),
    Utf8(String),
}

#[derive(Debug, Error)]
pub enum PathCodecError {
    #[error("invalid base64 path payload: {0}")]
    Base64(#[from] base64::DecodeError),
    #[error("path encoding {0} is not native to this target")]
    WrongPlatform(&'static str),
    #[error("Windows UTF-16 path payload has an odd byte length")]
    OddUtf16Length,
    #[error("UTF-8 path payload is not valid on this target")]
    InvalidUtf8,
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct NativeIdentity {
    pub file_id: file_id::FileId,
    pub link_count: Option<u64>,
    pub reparse_point: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedRoot {
    pub requested: NativePath,
    pub resolved: NativePath,
    pub identity: NativeIdentity,
}

impl ResolvedRoot {
    /// # Errors
    /// Returns a filesystem, usage, or identity error when the requested root cannot resolve.
    pub fn resolve(path: PathBuf) -> Result<Self, AppError> {
        let requested = NativePath::new(path);
        let resolved_path = fs::canonicalize(requested.as_path()).map_err(|error| {
            AppError::io(
                format!(
                    "could not resolve {}",
                    safe_display_path_text(requested.as_path())
                ),
                error,
            )
        })?;
        let metadata = fs::metadata(&resolved_path).map_err(|error| {
            AppError::io(
                format!(
                    "could not inspect {}",
                    safe_display_path_text(&resolved_path)
                ),
                error,
            )
        })?;
        if !metadata.is_dir() {
            return Err(AppError::Cli(format!(
                "scan root is not a directory: {}",
                safe_display_path_text(requested.as_path())
            )));
        }
        let identity = identity_for(&resolved_path, &metadata)
            .map_err(|error| AppError::io("could not identify scan root", error))?
            .ok_or_else(|| AppError::Invariant("resolved root is a symbolic link".to_string()))?;

        Ok(Self {
            requested,
            resolved: NativePath::new(resolved_path),
            identity,
        })
    }
}

/// # Errors
/// Returns an I/O error when the platform identity provider cannot inspect the path.
pub fn identity_for(path: &Path, metadata: &Metadata) -> io::Result<Option<NativeIdentity>> {
    #[cfg(unix)]
    let (file_id, link_count, reparse_point) = {
        use std::os::unix::fs::MetadataExt as _;

        let _ = path;
        (
            file_id::FileId::new_inode(metadata.dev(), metadata.ino()),
            Some(metadata.nlink()),
            is_reparse_point(metadata),
        )
    };
    #[cfg(windows)]
    let (file_id, link_count, reparse_point) = {
        use cap_primitives::fs::{_WindowsByHandle as _, OpenOptionsExt as _};

        const FILE_READ_ATTRIBUTES: u32 = 0x0000_0080;
        const FILE_SHARE_READ: u32 = 0x0000_0001;
        const FILE_SHARE_WRITE: u32 = 0x0000_0002;
        const FILE_SHARE_DELETE: u32 = 0x0000_0004;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        let _ = metadata;
        let mut options = cap_primitives::fs::OpenOptions::new();
        options
            .access_mode(FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            ._cap_fs_ext_follow(cap_primitives::fs::FollowSymlinks::No);
        let handle =
            cap_primitives::fs::open_ambient(path, &options, cap_primitives::ambient_authority())?;
        let metadata = cap_primitives::fs::Metadata::from_file(&handle)?;
        let volume = metadata
            .volume_serial_number()
            .ok_or_else(|| io::Error::other("file handle did not expose a volume serial number"))?;
        let index = metadata
            .file_index()
            .ok_or_else(|| io::Error::other("file handle did not expose a file index"))?;
        (
            file_id::FileId::new_low_res(volume, index),
            metadata.number_of_links().map(u64::from),
            metadata.file_attributes() & 0x0000_0400 != 0,
        )
    };
    #[cfg(not(any(unix, windows)))]
    let (file_id, link_count, reparse_point) = {
        if metadata.file_type().is_symlink() {
            return Ok(None);
        }
        (
            file_id::get_file_id(path)?,
            None,
            is_reparse_point(metadata),
        )
    };
    Ok(Some(NativeIdentity {
        file_id,
        link_count,
        reparse_point,
    }))
}

#[must_use]
pub fn safe_display_path(path: &Path) -> SafeDisplayPath {
    safe_display_os(path.as_os_str())
}

#[must_use]
pub fn safe_display_os_str(value: &OsStr) -> SafeDisplayPath {
    safe_display_os(value)
}

fn escape_valid_text(value: &str) -> SafeDisplayPath {
    let mut text = String::with_capacity(value.len());
    let mut deceptive = false;
    for ch in value.chars() {
        match ch {
            '\\' => text.push_str("\\\\"),
            '\n' => {
                text.push_str("\\n");
                deceptive = true;
            }
            '\r' => {
                text.push_str("\\r");
                deceptive = true;
            }
            '\t' => {
                text.push_str("\\t");
                deceptive = true;
            }
            '\u{1b}' => {
                text.push_str("\\x1b");
                deceptive = true;
            }
            _ if ch.is_control() || is_bidi_control(ch) => {
                use std::fmt::Write as _;
                let _ = write!(text, "\\u{{{:04x}}}", u32::from(ch));
                deceptive = true;
            }
            _ => text.push(ch),
        }
    }
    SafeDisplayPath { text, deceptive }
}

const fn is_bidi_control(ch: char) -> bool {
    matches!(
        ch,
        '\u{061c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{206f}'
    )
}

#[cfg(unix)]
fn safe_display_os(value: &std::ffi::OsStr) -> SafeDisplayPath {
    use std::os::unix::ffi::OsStrExt as _;

    if let Some(value) = value.to_str() {
        return escape_valid_text(value);
    }

    let mut text = String::new();
    for byte in value.as_bytes() {
        if byte.is_ascii_graphic() || *byte == b' ' {
            text.push(char::from(*byte));
        } else {
            use std::fmt::Write as _;
            let _ = write!(text, "\\x{byte:02x}");
        }
    }
    SafeDisplayPath {
        text,
        deceptive: true,
    }
}

#[cfg(windows)]
fn safe_display_os(value: &std::ffi::OsStr) -> SafeDisplayPath {
    use std::os::windows::ffi::OsStrExt as _;

    if let Some(value) = value.to_str() {
        return escape_valid_text(value);
    }

    let mut text = String::new();
    for decoded in char::decode_utf16(value.encode_wide()) {
        match decoded {
            Ok(ch) => text.push_str(&escape_valid_text(&ch.to_string()).text),
            Err(error) => {
                use std::fmt::Write as _;
                let _ = write!(text, "\\u{{{:04x}}}", error.unpaired_surrogate());
            }
        }
    }
    SafeDisplayPath {
        text,
        deceptive: true,
    }
}

#[cfg(not(any(unix, windows)))]
fn safe_display_os(value: &std::ffi::OsStr) -> SafeDisplayPath {
    escape_valid_text(&value.to_string_lossy())
}

#[cfg(unix)]
fn encode_path(path: &Path) -> EncodedNativePath {
    use std::os::unix::ffi::OsStrExt as _;

    EncodedNativePath::UnixBytes(STANDARD.encode(path.as_os_str().as_bytes()))
}

#[cfg(windows)]
fn encode_path(path: &Path) -> EncodedNativePath {
    use std::os::windows::ffi::OsStrExt as _;

    let bytes: Vec<_> = path
        .as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect();
    EncodedNativePath::WindowsUtf16Le(STANDARD.encode(bytes))
}

#[cfg(not(any(unix, windows)))]
fn encode_path(path: &Path) -> EncodedNativePath {
    EncodedNativePath::Utf8(path.to_string_lossy().into_owned())
}

#[cfg(unix)]
fn decode_path(encoded: &EncodedNativePath) -> Result<PathBuf, PathCodecError> {
    use std::os::unix::ffi::OsStringExt as _;

    match encoded {
        EncodedNativePath::UnixBytes(value) => {
            let bytes = STANDARD.decode(value)?;
            Ok(PathBuf::from(OsString::from_vec(bytes)))
        }
        EncodedNativePath::WindowsUtf16Le(_) => {
            Err(PathCodecError::WrongPlatform("windows-utf16-le"))
        }
        EncodedNativePath::Utf8(value) => Ok(PathBuf::from(value)),
    }
}

#[cfg(windows)]
fn decode_path(encoded: &EncodedNativePath) -> Result<PathBuf, PathCodecError> {
    use std::os::windows::ffi::OsStringExt as _;

    match encoded {
        EncodedNativePath::WindowsUtf16Le(value) => {
            let bytes = STANDARD.decode(value)?;
            if bytes.len() % 2 != 0 {
                return Err(PathCodecError::OddUtf16Length);
            }
            let units = bytes
                .chunks_exact(2)
                .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
                .collect::<Vec<_>>();
            Ok(PathBuf::from(OsString::from_wide(&units)))
        }
        EncodedNativePath::UnixBytes(_) => Err(PathCodecError::WrongPlatform("unix-bytes")),
        EncodedNativePath::Utf8(value) => Ok(PathBuf::from(value)),
    }
}

#[cfg(not(any(unix, windows)))]
fn decode_path(encoded: &EncodedNativePath) -> Result<PathBuf, PathCodecError> {
    match encoded {
        EncodedNativePath::Utf8(value) => Ok(PathBuf::from(value)),
        EncodedNativePath::UnixBytes(_) => Err(PathCodecError::WrongPlatform("unix-bytes")),
        EncodedNativePath::WindowsUtf16Le(_) => {
            Err(PathCodecError::WrongPlatform("windows-utf16-le"))
        }
    }
}

#[cfg(not(windows))]
fn is_reparse_point(metadata: &Metadata) -> bool {
    metadata.file_type().is_symlink()
}
