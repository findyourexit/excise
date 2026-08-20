#![allow(unsafe_code)]

use std::io;
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt as _;
use std::path::Path;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    GetSecurityInfo, SE_FILE_OBJECT, SetSecurityInfo,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_REVISION, ACL_SIZE_INFORMATION, AclSizeInformation,
    DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetAclInformation, GetLengthSid,
    GetSecurityDescriptorControl, GetTokenInformation, InitializeAcl, InitializeSecurityDescriptor,
    IsValidAcl, IsValidSid, OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR, PSID, SE_DACL_PROTECTED, SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR,
    SetSecurityDescriptorControl, SetSecurityDescriptorDacl, SetSecurityDescriptorOwner,
    TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateDirectoryW, CreateFileW, FILE_ALL_ACCESS,
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_DISPOSITION_FLAG_DELETE,
    FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE, FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
    FILE_DISPOSITION_INFO, FILE_DISPOSITION_INFO_EX, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FileDispositionInfo, FileDispositionInfoEx, GetFileInformationByHandle, OPEN_EXISTING,
    READ_CONTROL, SetFileInformationByHandle, WRITE_DAC, WRITE_OWNER,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetExitCodeProcess, OpenProcess, OpenProcessToken,
    PROCESS_QUERY_LIMITED_INFORMATION,
};

const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
const PRIVATE_ACE_FLAGS: u8 = 0;
const SECURITY_DESCRIPTOR_REVISION: u32 = 1;
const DELETE_ACCESS: u32 = 0x0001_0000;
const STILL_ACTIVE: u32 = 259;

#[cfg(test)]
pub(crate) fn is_user_admin() -> bool {
    false
}

#[cfg(not(test))]
pub(crate) fn is_user_admin() -> bool {
    is_elevated::is_elevated()
}

/// A held directory handle whose share mode denies deletion while a private
/// spill session is active. This prevents namespace replacement after the
/// directory's DACL has been verified.
pub(crate) struct PrivateDirectoryHandle(OwnedHandle);

impl PrivateDirectoryHandle {
    pub(crate) fn close(&mut self) {
        self.0.close();
    }

    pub(crate) fn delete_on_close(&mut self) -> io::Result<()> {
        mark_delete_on_close(self.0.raw())
    }
}

/// Creates a directory with a protected current-user-only DACL before it is
/// visible, then sets and verifies that ACL through a held non-reparse handle.
pub(crate) fn create_private_directory(path: &Path) -> io::Result<PrivateDirectoryHandle> {
    let user = CurrentUserSid::current()?;
    let security = PrivateSecurity::new(&user)?;
    let wide_path = wide_path(path);
    let attributes = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(u32::MAX),
        lpSecurityDescriptor: (&raw const security.descriptor).cast_mut().cast(),
        bInheritHandle: 0,
    };

    // SAFETY: the descriptor and its backing ACL remain live during the call.
    // Windows applies the descriptor atomically when it creates the directory.
    if unsafe { CreateDirectoryW(wide_path.as_ptr(), &raw const attributes) } == 0 {
        return Err(io::Error::last_os_error());
    }

    (|| {
        let handle = open_private_path(
            path,
            true,
            READ_CONTROL | WRITE_DAC | WRITE_OWNER | FILE_READ_ATTRIBUTES,
        )?;
        set_private_security(handle.raw(), &security, &user)?;
        verify_private_handle(handle.raw(), true, &user)?;
        Ok(PrivateDirectoryHandle(handle))
    })()
}

/// Replaces a path's inherited ACL through a non-reparse handle, then proves
/// that the same held object is private to the current user.
pub(crate) fn restrict_private_path(path: &Path, directory: bool) -> io::Result<()> {
    let user = CurrentUserSid::current()?;
    let security = PrivateSecurity::new(&user)?;
    let handle = open_private_path(
        path,
        directory,
        READ_CONTROL | WRITE_DAC | WRITE_OWNER | FILE_READ_ATTRIBUTES,
    )?;
    set_private_security(handle.raw(), &security, &user)?;
    verify_private_handle(handle.raw(), directory, &user)
}

/// Verifies a path through one held non-reparse handle rather than separate
/// pathname metadata and ACL queries.
pub(crate) fn verify_private_path(path: &Path, directory: bool) -> io::Result<()> {
    let user = CurrentUserSid::current()?;
    let handle = open_private_path(path, directory, READ_CONTROL | FILE_READ_ATTRIBUTES)?;
    verify_private_handle(handle.raw(), directory, &user)
}

/// Opens and verifies a stale session directory through one held no-delete
/// share handle. The caller must retain this handle through classification and
/// every child deletion.
pub(crate) fn open_verified_private_directory_for_cleanup(
    path: &Path,
) -> io::Result<PrivateDirectoryHandle> {
    let user = CurrentUserSid::current()?;
    let handle = open_private_path(
        path,
        true,
        READ_CONTROL | FILE_READ_ATTRIBUTES | DELETE_ACCESS,
    )?;
    verify_private_handle(handle.raw(), true, &user)?;
    Ok(PrivateDirectoryHandle(handle))
}

/// Deletes one expected private regular file by the same no-reparse handle
/// used to verify its DACL and file type.
pub(crate) fn delete_verified_private_file(path: &Path) -> io::Result<()> {
    let user = CurrentUserSid::current()?;
    let mut handle = open_private_path(
        path,
        false,
        READ_CONTROL | FILE_READ_ATTRIBUTES | DELETE_ACCESS,
    )?;
    verify_private_handle(handle.raw(), false, &user)?;
    mark_delete_on_close(handle.raw())?;
    handle.close();
    Ok(())
}

/// Returns `true` unless Windows can positively establish that the process has
/// exited. Cleanup callers treat uncertainty as an active session.
pub(crate) fn is_process_active(pid: u32) -> bool {
    if pid == 0 {
        return true;
    }

    // SAFETY: Windows returns an owned process handle or null. The requested
    // right only reads process state.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return true;
    }
    let _process = OwnedHandle(process);
    let mut exit_code = 0_u32;
    // SAFETY: the owned process handle and the output value remain live here.
    unsafe { GetExitCodeProcess(process, &raw mut exit_code) != 0 && exit_code == STILL_ACTIVE }
}

fn open_private_path(path: &Path, directory: bool, access: u32) -> io::Result<OwnedHandle> {
    let wide_path = wide_path(path);
    let flags = FILE_FLAG_OPEN_REPARSE_POINT
        | if directory {
            FILE_FLAG_BACKUP_SEMANTICS
        } else {
            0
        };
    // SAFETY: the nul-terminated path remains live during the call. The null
    // security/template pointers are permitted, and the returned handle is
    // owned by this function on success. Omitting FILE_SHARE_DELETE keeps a
    // verified active spill directory from being renamed or removed.
    let handle = unsafe {
        CreateFileW(
            wide_path.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            null(),
            OPEN_EXISTING,
            flags,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        Err(io::Error::last_os_error())
    } else {
        Ok(OwnedHandle(handle))
    }
}

fn mark_delete_on_close(handle: HANDLE) -> io::Result<()> {
    let disposition = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_FLAG_DELETE
            | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS
            | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
    };
    // SAFETY: `handle` is owned by the caller and remains open for this call.
    // The stack disposition value is correctly aligned and Windows does not
    // retain its pointer.
    if unsafe {
        SetFileInformationByHandle(
            handle,
            FileDispositionInfoEx,
            (&raw const disposition).cast(),
            u32::try_from(size_of::<FILE_DISPOSITION_INFO_EX>()).unwrap_or(u32::MAX),
        )
    } != 0
    {
        return Ok(());
    }

    let error = io::Error::last_os_error();
    if !matches!(error.raw_os_error(), Some(50 | 87)) {
        return Err(error);
    }
    let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: as above, with the legacy disposition structure for systems that
    // do not implement FileDispositionInfoEx.
    if unsafe {
        SetFileInformationByHandle(
            handle,
            FileDispositionInfo,
            (&raw const disposition).cast(),
            u32::try_from(size_of::<FILE_DISPOSITION_INFO>()).unwrap_or(u32::MAX),
        )
    } == 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn set_private_security(
    handle: HANDLE,
    security: &PrivateSecurity,
    user: &CurrentUserSid,
) -> io::Result<()> {
    let information = OWNER_SECURITY_INFORMATION
        | DACL_SECURITY_INFORMATION
        | PROTECTED_DACL_SECURITY_INFORMATION;
    // SAFETY: `handle` is held by the caller, and the owner SID and ACL backing
    // storage remain live for the call. SetSecurityInfo does not retain either.
    let result = unsafe {
        SetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            information,
            user.as_psid(),
            null_mut(),
            security.acl_ptr(),
            null(),
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(win32_error(result))
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "One held-handle security check must preserve its verification order as one invariant."
)]
fn verify_private_handle(handle: HANDLE, directory: bool, user: &CurrentUserSid) -> io::Result<()> {
    let mut file_information = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the held handle is valid and `file_information` is an aligned
    // writable output value which Windows does not retain.
    if unsafe { GetFileInformationByHandle(handle, &raw mut file_information) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let attributes = file_information.dwFileAttributes;
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || (directory && attributes & FILE_ATTRIBUTE_DIRECTORY == 0)
        || (!directory && attributes & FILE_ATTRIBUTE_DIRECTORY != 0)
    {
        return Err(private_path_error(
            "private path is not the expected non-reparse type",
        ));
    }

    let information = OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
    let mut owner = null_mut();
    let mut dacl = null_mut();
    let mut descriptor = null_mut();
    // SAFETY: all output pointers are valid for the call. Windows allocates the
    // returned descriptor, which LocalSecurityDescriptor frees below.
    let result = unsafe {
        GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            information,
            &raw mut owner,
            null_mut(),
            &raw mut dacl,
            null_mut(),
            &raw mut descriptor,
        )
    };
    if result != 0 {
        return Err(win32_error(result));
    }
    let _descriptor = LocalSecurityDescriptor(descriptor);
    if descriptor.is_null() || owner.is_null() || dacl.is_null() {
        return Err(private_path_error(
            "private path did not return owner and DACL",
        ));
    }
    // SAFETY: both SIDs are owned by the returned descriptor/current token and
    // stay live until this function returns.
    if unsafe { EqualSid(owner, user.as_psid()) } == 0 {
        return Err(private_path_error(
            "private path owner is not the current user",
        ));
    }

    let mut control = 0_u16;
    let mut revision = 0_u32;
    // SAFETY: the returned descriptor and output variables are live here.
    if unsafe { GetSecurityDescriptorControl(descriptor, &raw mut control, &raw mut revision) } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if control & SE_DACL_PROTECTED == 0 {
        return Err(private_path_error(
            "private path DACL inherits access entries",
        ));
    }
    // SAFETY: `dacl` belongs to the valid returned descriptor.
    if unsafe { IsValidAcl(dacl) } == 0 {
        return Err(private_path_error("private path DACL is invalid"));
    }

    let mut size = ACL_SIZE_INFORMATION::default();
    // SAFETY: `dacl` is valid and `size` is a live aligned output buffer.
    if unsafe {
        GetAclInformation(
            dacl,
            (&raw mut size).cast(),
            u32::try_from(size_of::<ACL_SIZE_INFORMATION>()).unwrap_or(u32::MAX),
            AclSizeInformation,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if size.AceCount != 1 {
        return Err(private_path_error(
            "private path DACL grants more than one principal",
        ));
    }

    let mut ace = null_mut();
    // SAFETY: `dacl` contains exactly one validated ACE, and `ace` is a live
    // output pointer. The returned pointer remains owned by the descriptor.
    if unsafe { GetAce(dacl, 0, &raw mut ace) } == 0 || ace.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a valid ACL guarantees a readable ACE header at this pointer.
    let header = unsafe { &*ace.cast::<ACE_HEADER>() };
    if header.AceType != ACCESS_ALLOWED_ACE_TYPE || header.AceFlags != PRIVATE_ACE_FLAGS {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "private path DACL has unexpected ACE type {} or flags {}; expected type {} and flags {}",
                header.AceType, header.AceFlags, ACCESS_ALLOWED_ACE_TYPE, PRIVATE_ACE_FLAGS,
            ),
        ));
    }
    if usize::from(header.AceSize) < size_of::<ACCESS_ALLOWED_ACE>() {
        return Err(private_path_error("private path DACL ACE is truncated"));
    }
    // SAFETY: the checked type and minimum size make the fixed
    // ACCESS_ALLOWED_ACE prefix valid to read.
    let allowed = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
    if allowed.Mask != FILE_ALL_ACCESS {
        return Err(private_path_error(
            "private path DACL does not grant user full access",
        ));
    }
    let ace_sid = (&raw const allowed.SidStart).cast_mut().cast();
    // SAFETY: the access-allowed ACE stores its SID at SidStart.
    if unsafe { IsValidSid(ace_sid) } == 0 {
        return Err(private_path_error(
            "private path DACL contains an invalid SID",
        ));
    }
    // SAFETY: IsValidSid established that Windows may read this SID length.
    let sid_len = unsafe { GetLengthSid(ace_sid) };
    let required_size = size_of::<ACCESS_ALLOWED_ACE>()
        .saturating_sub(size_of::<u32>())
        .saturating_add(usize::try_from(sid_len).unwrap_or(usize::MAX));
    if usize::from(header.AceSize) < required_size {
        return Err(private_path_error(
            "private path DACL ACE does not contain its SID",
        ));
    }
    // SAFETY: both validated SIDs remain live through this comparison.
    if unsafe { EqualSid(ace_sid, user.as_psid()) } == 0 {
        return Err(private_path_error(
            "private path DACL grants a different principal",
        ));
    }

    Ok(())
}

struct CurrentUserSid(AlignedBytes);

impl CurrentUserSid {
    fn current() -> io::Result<Self> {
        let mut token = null_mut();
        // SAFETY: GetCurrentProcess returns a current-process pseudo-handle and
        // `token` is a valid output location for OpenProcessToken.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let _token = OwnedHandle(token);

        let mut size = 0_u32;
        // SAFETY: this sizing call intentionally supplies no token buffer.
        let _ = unsafe { GetTokenInformation(token, TokenUser, null_mut(), 0, &raw mut size) };
        if usize::try_from(size).unwrap_or(0) < size_of::<TOKEN_USER>() {
            return Err(private_path_error("token did not provide a user SID"));
        }
        let mut bytes = AlignedBytes::zeroed(usize::try_from(size).unwrap_or(usize::MAX));
        // SAFETY: `bytes` has exactly the size requested by Windows and stays
        // live while Windows writes TOKEN_USER data into it.
        if unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                bytes.as_mut_ptr().cast(),
                size,
                &raw mut size,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }

        // SAFETY: successful GetTokenInformation initialized TOKEN_USER at the
        // start of the aligned output buffer.
        let token_user = unsafe { bytes.as_ptr().cast::<TOKEN_USER>().read() };
        if token_user.User.Sid.is_null() {
            return Err(private_path_error("token did not contain a user SID"));
        }
        // SAFETY: Windows supplied the SID inside the live token data.
        if unsafe { IsValidSid(token_user.User.Sid) } == 0 {
            return Err(private_path_error("token contained an invalid user SID"));
        }
        // SAFETY: a valid SID has a byte length reported by Windows.
        let sid_len = unsafe { GetLengthSid(token_user.User.Sid) };
        if sid_len == 0 {
            return Err(private_path_error("token user SID had no length"));
        }
        let mut sid = AlignedBytes::zeroed(usize::try_from(sid_len).unwrap_or(usize::MAX));
        // SAFETY: `sid` has the exact SID length and does not overlap token data.
        unsafe {
            std::ptr::copy_nonoverlapping(
                token_user.User.Sid.cast::<u8>(),
                sid.as_mut_ptr().cast(),
                sid.len(),
            );
        }
        Ok(Self(sid))
    }

    fn as_psid(&self) -> PSID {
        self.0.as_ptr().cast_mut().cast()
    }
}

struct PrivateSecurity {
    acl: AlignedBytes,
    descriptor: SECURITY_DESCRIPTOR,
}

impl PrivateSecurity {
    fn new(user: &CurrentUserSid) -> io::Result<Self> {
        let access_entry_bytes = size_of::<ACCESS_ALLOWED_ACE>()
            .saturating_sub(size_of::<u32>())
            .saturating_add(user.0.len());
        let acl_allocation_bytes = size_of::<ACL>().saturating_add(access_entry_bytes);
        let mut acl = AlignedBytes::zeroed(acl_allocation_bytes);
        let acl_ptr = acl.as_mut_ptr().cast::<ACL>();
        let mut descriptor = SECURITY_DESCRIPTOR::default();
        let acl_size = u32::try_from(acl_allocation_bytes)
            .map_err(|_| private_path_error("private DACL was too large"))?;

        // SAFETY: `acl` is an aligned allocation large enough for the ACL and
        // one access-allowed ACE carrying the validated current-user SID.
        if unsafe { InitializeAcl(acl_ptr, acl_size, ACL_REVISION) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: Windows copies the current-user SID into the initialized ACL.
        if unsafe {
            windows_sys::Win32::Security::AddAccessAllowedAceEx(
                acl_ptr,
                ACL_REVISION,
                u32::from(PRIVATE_ACE_FLAGS),
                FILE_ALL_ACCESS,
                user.as_psid(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: descriptor is a live aligned output value.
        if unsafe {
            InitializeSecurityDescriptor((&raw mut descriptor).cast(), SECURITY_DESCRIPTOR_REVISION)
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: descriptor stores these live SID/ACL pointers only while
        // CreateDirectoryW consumes it below.
        if unsafe { SetSecurityDescriptorOwner((&raw mut descriptor).cast(), user.as_psid(), 0) }
            == 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: acl_ptr names the initialized ACL owned by `acl`.
        if unsafe { SetSecurityDescriptorDacl((&raw mut descriptor).cast(), 1, acl_ptr, 0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: sets the initialized descriptor's protected-DACL control bit.
        if unsafe {
            SetSecurityDescriptorControl(
                (&raw mut descriptor).cast(),
                SE_DACL_PROTECTED,
                SE_DACL_PROTECTED,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }

        Ok(Self { acl, descriptor })
    }

    fn acl_ptr(&self) -> *const ACL {
        self.acl.as_ptr().cast()
    }
}

struct AlignedBytes {
    words: Vec<usize>,
    len: usize,
}

impl AlignedBytes {
    fn zeroed(len: usize) -> Self {
        let word_size = size_of::<usize>();
        let words = len.saturating_add(word_size.saturating_sub(1)) / word_size;
        Self {
            words: vec![0; words],
            len,
        }
    }

    fn as_ptr(&self) -> *const usize {
        self.words.as_ptr()
    }

    fn as_mut_ptr(&mut self) -> *mut usize {
        self.words.as_mut_ptr()
    }

    const fn len(&self) -> usize {
        self.len
    }
}

struct LocalSecurityDescriptor(PSECURITY_DESCRIPTOR);

impl Drop for LocalSecurityDescriptor {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: GetSecurityInfo allocated this descriptor for the caller,
            // so LocalFree releases it exactly once here.
            let _ = unsafe { LocalFree(self.0.cast()) };
        }
    }
}

struct OwnedHandle(HANDLE);

impl OwnedHandle {
    const fn raw(&self) -> HANDLE {
        self.0
    }

    fn close(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            // SAFETY: this wrapper owns the handle and closes it at most once.
            let _ = unsafe { CloseHandle(self.0) };
            self.0 = null_mut();
        }
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        self.close();
    }
}

fn wide_path(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

fn private_path_error(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}

fn win32_error(code: u32) -> io::Error {
    io::Error::from_raw_os_error(i32::try_from(code).unwrap_or(i32::MAX))
}
