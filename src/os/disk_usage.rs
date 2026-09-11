#[cfg(windows)]
use std::fs::File;
use std::fs::Metadata;
use std::io;
use std::path::Path;

#[cfg(not(windows))]
use filesize::PathExt;

pub(crate) fn physical_size(path: &Path, metadata: &Metadata) -> io::Result<u64> {
    #[cfg(windows)]
    {
        let _ = metadata;
        let handle = open_nofollow(path)?;
        crate::os::windows::physical_size_from_handle(&handle)
    }
    #[cfg(not(windows))]
    path.size_on_disk_fast(metadata)
}

#[cfg(windows)]
fn open_nofollow(path: &Path) -> io::Result<File> {
    use cap_primitives::ambient_authority;
    use cap_primitives::fs::OpenOptionsExt as _;
    use cap_primitives::fs::{self as cap_fs, FollowSymlinks};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    let mut options = cap_fs::OpenOptions::new();
    options
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        ._cap_fs_ext_follow(FollowSymlinks::No);
    cap_fs::open_ambient(path, &options, ambient_authority())
}
