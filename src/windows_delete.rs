#![allow(unsafe_code)]

use std::fs::File;
use std::io;
use std::mem::size_of;
use std::os::windows::io::AsRawHandle as _;

use windows_sys::Win32::Storage::FileSystem::{
    FILE_DISPOSITION_FLAG_DELETE, FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
    FILE_DISPOSITION_FLAG_POSIX_SEMANTICS, FILE_DISPOSITION_INFO, FILE_DISPOSITION_INFO_EX,
    FileDispositionInfo, FileDispositionInfoEx, SetFileInformationByHandle,
};

pub fn remove_open_handle(file: &File) -> io::Result<()> {
    let handle = file.as_raw_handle();
    let disposition = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_FLAG_DELETE
            | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS
            | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
    };
    // SAFETY: `handle` is owned by `file` and remains open for the call. The
    // information pointer and byte count describe a live, correctly aligned
    // `FILE_DISPOSITION_INFO_EX` value. The API does not retain the pointer.
    let removed = unsafe {
        SetFileInformationByHandle(
            handle,
            FileDispositionInfoEx,
            (&raw const disposition).cast(),
            u32::try_from(size_of::<FILE_DISPOSITION_INFO_EX>()).unwrap_or(u32::MAX),
        )
    };
    if removed != 0 {
        return Ok(());
    }

    let extended_error = io::Error::last_os_error();
    if !matches!(extended_error.raw_os_error(), Some(50 | 87)) {
        return Err(extended_error);
    }

    let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: As above, with the legacy disposition structure. This fallback
    // remains identity-bound to the already verified handle.
    let removed = unsafe {
        SetFileInformationByHandle(
            handle,
            FileDispositionInfo,
            (&raw const disposition).cast(),
            u32::try_from(size_of::<FILE_DISPOSITION_INFO>()).unwrap_or(u32::MAX),
        )
    };
    if removed == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
