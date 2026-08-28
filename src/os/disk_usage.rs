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
        physical_size_from_handle(&handle)
    }
    #[cfg(not(windows))]
    path.size_on_disk_fast(metadata)
}

#[cfg(windows)]
pub(crate) fn physical_size_from_handle(handle: &File) -> io::Result<u64> {
    windows_allocation_size(handle)
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

#[cfg(windows)]
#[allow(unsafe_code)]
fn windows_allocation_size(handle: &File) -> io::Result<u64> {
    use std::mem::size_of;
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_STANDARD_INFO, FileStandardInfo, GetFileInformationByHandleEx,
    };

    let mut information = FILE_STANDARD_INFO::default();
    // SAFETY: the handle is borrowed and valid for this call. `information`
    // is an aligned writable output value whose size is passed exactly.
    if unsafe {
        GetFileInformationByHandleEx(
            handle.as_raw_handle(),
            FileStandardInfo,
            (&raw mut information).cast(),
            u32::try_from(size_of::<FILE_STANDARD_INFO>()).unwrap_or(u32::MAX),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    u64::try_from(information.AllocationSize)
        .map_err(|_| io::Error::other("Windows allocation size was negative"))
}
