//! Where a run puts its fixture and scratch area.

use std::path::PathBuf;

/// The directory that fixtures and scratch areas are built in by default.
///
/// It is `$EXCISE_E2E_TMPDIR` when that is set. Otherwise it is `/tmp` on Unix, where a short path
/// keeps the deletion dialog readable (the system temporary directory on macOS is over fifty
/// characters long), and the system temporary directory elsewhere. The dialog is at most 78
/// columns wide, and the Windows temporary directory is too long for it, so set the variable to a
/// short directory there.
#[must_use]
pub fn work_base() -> PathBuf {
    if let Some(dir) = std::env::var_os("EXCISE_E2E_TMPDIR").filter(|dir| !dir.is_empty()) {
        return PathBuf::from(dir);
    }
    #[cfg(unix)]
    {
        let tmp = PathBuf::from("/tmp");
        if tmp.is_dir() {
            return tmp;
        }
    }
    std::env::temp_dir()
}
