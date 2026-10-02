//! The facts the scanner reads about one directory entry.

use std::io;
use std::path::Path;
use std::time::SystemTime;
#[cfg(unix)]
use std::time::{Duration, UNIX_EPOCH};

#[cfg(unix)]
use crate::native_path::NativeIdentity;
#[cfg(unix)]
use rustix::fs::FileType;

/// What the scanner seals about one entry, read without following a link: its kind, apparent
/// size, modification time, and the storage it occupies.
///
/// On Unix it is built from a single `fstatat` against the handle of the folder that lists the
/// entry (`scanner::read_entry`), so the read depends on the entry's name and never on the length
/// of its path, which the kernel rejects past `PATH_MAX`. Windows reads an entry's identity by
/// opening its path, so there it wraps the metadata of a path-based `symlink_metadata` unchanged.
#[derive(Clone, Debug)]
pub struct EntryMetadata(Stat);

/// `st_dev`, `st_blocks`, and the rest of what `lstat` returns that the scan store keeps, and
/// nothing else: a sealed batch holds many of these.
#[cfg(unix)]
#[derive(Clone, Debug)]
struct Stat {
    is_dir: bool,
    is_symlink: bool,
    len: u64,
    modified: Option<SystemTime>,
    device: u64,
    /// `st_blocks`, in the 512-byte units `stat(2)` reports whatever the file system's block size.
    blocks: u64,
}

#[cfg(not(unix))]
type Stat = std::fs::Metadata;

/// The fields of a `stat` that the scanner keeps, with every number widened to `i128`: whatever
/// integer types a platform gives them fit, and a value the scanner's types cannot hold (a
/// negative size from a file system that misreports one) reaches the conversion as itself, not
/// wrapped and not as a panic. `st_rdev` is not here: no scanner fact needs it, and a device
/// number too large for the type a library converts it to is where that conversion panics.
#[cfg(unix)]
#[derive(Clone, Copy, Debug)]
struct StatFields {
    file_type: FileType,
    size: i128,
    blocks: i128,
    mtime_secs: i128,
    mtime_nanos: i128,
    dev: i128,
    ino: i128,
    nlink: i128,
}

#[cfg(unix)]
impl StatFields {
    fn from_stat(stat: &rustix::fs::Stat) -> Self {
        Self {
            file_type: FileType::from_raw_mode(stat.st_mode),
            size: i128::from(stat.st_size),
            blocks: i128::from(stat.st_blocks),
            mtime_secs: i128::from(stat.st_mtime),
            mtime_nanos: i128::from(stat.st_mtime_nsec),
            dev: i128::from(stat.st_dev),
            ino: i128::from(stat.st_ino),
            nlink: i128::from(stat.st_nlink),
        }
    }
}

/// `value` as the unsigned number the scanner keeps, or a message naming what the file system
/// reported.
#[cfg(unix)]
fn non_negative(value: i128, what: &str) -> Result<u64, String> {
    u64::try_from(value).map_err(|_| format!("the file system reported {what} of {value}"))
}

/// A modification time as `SystemTime`, before the epoch as well as after it. `None` when the
/// seconds or nanoseconds are out of range.
#[cfg(unix)]
fn modification_time(secs: i128, nanos: i128) -> Option<SystemTime> {
    let secs = i64::try_from(secs).ok()?;
    let nanos = u32::try_from(nanos)
        .ok()
        .filter(|nanos| *nanos < 1_000_000_000)?;
    match u64::try_from(secs) {
        Ok(secs) => UNIX_EPOCH.checked_add(Duration::new(secs, nanos)),
        Err(_) => UNIX_EPOCH
            .checked_sub(Duration::new(secs.unsigned_abs(), 0))?
            .checked_add(Duration::new(0, nanos)),
    }
}

#[cfg(unix)]
impl EntryMetadata {
    /// Builds the entry's facts and its identity from one `stat`.
    ///
    /// # Errors
    ///
    /// Returns a message for a value that does not convert: a negative size, block count, or link
    /// count. The entry is then reported like any other unreadable one; nothing here panics, as
    /// the conversions of a library `Metadata` can on a device node or a corrupt size. A
    /// modification time that does not convert is `None`, as `Metadata::modified().ok()` gives
    /// it, so an odd timestamp never hides an entry.
    pub(super) fn from_stat(stat: &rustix::fs::Stat) -> Result<(Self, NativeIdentity), String> {
        Self::from_fields(&StatFields::from_stat(stat))
    }

    /// The pure conversion behind [`Self::from_stat`]. `dev` wraps into `u64` exactly as
    /// `std::os::unix::fs::MetadataExt::dev` does (a signed `dev_t` sign-extends), so the identity
    /// equals what `identity_for` and the deletion planner compute for the same file.
    fn from_fields(fields: &StatFields) -> Result<(Self, NativeIdentity), String> {
        let len = non_negative(fields.size, "a size")?;
        let blocks = non_negative(fields.blocks, "an allocated block count")?;
        let nlink = non_negative(fields.nlink, "a link count")?;
        let ino = non_negative(fields.ino, "an inode number")?;
        let device = non_negative(fields.dev.rem_euclid(1_i128 << 64), "a device number")?;
        let is_symlink = fields.file_type == FileType::Symlink;
        let identity = NativeIdentity {
            file_id: file_id::FileId::new_inode(device, ino),
            link_count: Some(nlink),
            reparse_point: is_symlink,
        };
        let metadata = Self(Stat {
            is_dir: fields.file_type == FileType::Directory,
            is_symlink,
            len,
            modified: modification_time(fields.mtime_secs, fields.mtime_nanos),
            device,
            blocks,
        });
        Ok((metadata, identity))
    }

    #[cfg(test)]
    pub(crate) fn from_std(metadata: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt as _;

        Self(Stat {
            is_dir: metadata.is_dir(),
            is_symlink: metadata.file_type().is_symlink(),
            len: metadata.len(),
            modified: metadata.modified().ok(),
            device: metadata.dev(),
            blocks: metadata.blocks(),
        })
    }

    pub fn is_dir(&self) -> bool {
        self.0.is_dir
    }

    pub fn is_symlink(&self) -> bool {
        self.0.is_symlink
    }

    pub fn len(&self) -> u64 {
        self.0.len
    }

    pub fn modified(&self) -> Option<SystemTime> {
        self.0.modified
    }

    /// The device the entry lives on, for the filesystem-boundary check.
    pub fn device(&self) -> u64 {
        self.0.device
    }

    /// Bytes allocated to the entry: `st_blocks * 512`, the rule `os::physical_size` applies to a
    /// `std::fs::Metadata`. Reads no file, so `path` plays no part.
    #[allow(
        clippy::unnecessary_wraps,
        reason = "Windows opens the path, so the shared signature is fallible"
    )]
    pub fn physical_size(&self, _path: &Path) -> io::Result<u64> {
        Ok(self.0.blocks.saturating_mul(512))
    }
}

#[cfg(not(unix))]
impl EntryMetadata {
    pub(crate) fn from_std(metadata: &std::fs::Metadata) -> Self {
        Self(metadata.clone())
    }

    pub fn is_dir(&self) -> bool {
        self.0.is_dir()
    }

    pub fn is_symlink(&self) -> bool {
        self.0.file_type().is_symlink()
    }

    pub fn len(&self) -> u64 {
        self.0.len()
    }

    pub fn modified(&self) -> Option<SystemTime> {
        self.0.modified().ok()
    }

    pub fn physical_size(&self, path: &Path) -> io::Result<u64> {
        crate::os::physical_size(path, &self.0)
    }

    /// The path-based metadata this wraps, for the checks that still take a `std::fs::Metadata`.
    pub fn as_std(&self) -> &std::fs::Metadata {
        &self.0
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn regular_file() -> StatFields {
        StatFields {
            file_type: FileType::RegularFile,
            size: 1234,
            blocks: 8,
            mtime_secs: 1_700_000_000,
            mtime_nanos: 500,
            dev: 16_777_230,
            ino: 42,
            nlink: 2,
        }
    }

    #[test]
    fn a_value_the_scanner_cannot_hold_is_an_error_not_a_panic() {
        for (what, stat) in [
            (
                "size",
                StatFields {
                    size: -1,
                    ..regular_file()
                },
            ),
            (
                "block count",
                StatFields {
                    blocks: -8,
                    ..regular_file()
                },
            ),
            (
                "link count",
                StatFields {
                    nlink: -1,
                    ..regular_file()
                },
            ),
            (
                "inode number",
                StatFields {
                    ino: -1,
                    ..regular_file()
                },
            ),
        ] {
            assert!(
                EntryMetadata::from_fields(&stat).is_err(),
                "a negative {what} should be reported, not converted"
            );
        }
    }

    #[test]
    fn a_modification_time_out_of_range_is_unknown_and_the_entry_still_converts() {
        for (secs, nanos) in [
            (i128::from(i64::MAX) + 1, 0),
            (i128::from(i64::MIN) - 1, 0),
            (0, 1_000_000_000),
            (0, -1),
        ] {
            let stat = StatFields {
                mtime_secs: secs,
                mtime_nanos: nanos,
                ..regular_file()
            };
            let (metadata, identity) = EntryMetadata::from_fields(&stat)
                .unwrap_or_else(|error| panic!("{secs} s {nanos} ns hid the entry: {error}"));
            assert_eq!(metadata.modified(), None, "{secs} s {nanos} ns");
            assert_eq!(metadata.len(), 1234);
            assert_eq!(identity.file_id, file_id::FileId::new_inode(16_777_230, 42));
        }
    }

    #[test]
    fn a_time_before_the_epoch_converts() {
        let stat = StatFields {
            mtime_secs: -1,
            mtime_nanos: 500_000_000,
            ..regular_file()
        };
        let (metadata, _) = EntryMetadata::from_fields(&stat).expect("a pre-epoch time converts");
        assert_eq!(
            metadata.modified(),
            Some(UNIX_EPOCH - Duration::from_millis(500))
        );
    }

    #[test]
    fn a_signed_device_number_sign_extends_as_std_does() {
        // macOS `dev_t` is an `i32`; std reports `st_dev as u64`.
        let stat = StatFields {
            dev: i128::from(-5_i32),
            ..regular_file()
        };
        let (metadata, identity) = EntryMetadata::from_fields(&stat).expect("a stat converts");
        let expected = u64::from_ne_bytes((-5_i64).to_ne_bytes());
        assert_eq!(metadata.device(), expected);
        assert_eq!(identity.file_id, file_id::FileId::new_inode(expected, 42));
    }
}
