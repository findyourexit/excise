mod disk_usage;

pub(crate) use disk_usage::physical_size;

#[cfg(target_os = "windows")]
pub mod windows;

#[cfg(not(target_os = "windows"))]
pub mod unix;
