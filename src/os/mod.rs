mod disk_usage;

pub(crate) use disk_usage::physical_size;

#[cfg(windows)]
pub(crate) use disk_usage::physical_size_from_handle;

#[cfg(target_os = "windows")]
pub mod windows;

#[cfg(not(target_os = "windows"))]
pub mod unix;

#[cfg(target_os = "windows")]
pub(crate) use windows::is_user_admin;

#[cfg(not(target_os = "windows"))]
pub(crate) use unix::is_user_admin;
