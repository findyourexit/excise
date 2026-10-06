//! How much space is free where a directory is.

use std::path::Path;

/// The bytes that a process without special rights can still use on the file system that holds
/// `path`, which must exist. `None` when the system cannot say, and off Unix, where nothing asks.
#[must_use]
pub fn available_bytes(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        let stat = rustix::fs::statvfs(path).ok()?;
        Some(stat.f_bavail.saturating_mul(stat.f_frsize))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn a_directory_that_exists_has_space_that_can_be_read_and_one_that_does_not_has_none() {
        let directory = tempfile::tempdir().expect("a directory");

        let space = available_bytes(directory.path()).expect("the system says how much is free");

        assert!(space > 0, "a test needs room to make its directory");
        assert_eq!(available_bytes(&directory.path().join("not/there")), None);
    }
}
