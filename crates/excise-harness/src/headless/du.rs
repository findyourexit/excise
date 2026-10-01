//! The `du -sk` reference: what a plain walk of the same tree costs.
//!
//! `du` is the yardstick of the headless scan budget (`headless_scan_ratio`): the scan may take a
//! multiple of the time `du -sk` takes on the same warm fixture in the same session. This module
//! runs it the way the scan is run, through the same supervised process runner, in an empty
//! environment so that no `BLOCKSIZE`-style variable can change its output.
//!
//! It also says what `du -sk` must print for a tree, from the oracle's raw facts, so that a timing
//! is only compared with a `du` that walked the whole tree: BSD `du` stops where a path passes
//! `PATH_MAX`, for one. The rule is the one the oracle's own tests pin: `du -sk` is the allocated
//! bytes of the root, of every directory, and of every file once per `(device, inode)`, in KiB
//! rounded up. The flavors differ only in what they do with a directory they cannot list: GNU adds
//! the directory's own blocks, BSD leaves them out.

use std::{
    collections::HashSet,
    env,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use thiserror::Error;

use crate::fixture::{NodeKind, Oracle};

use super::process::{self, Finished, ProcessError};

/// How long `du --version` may take.
const VERSION_TIMEOUT: Duration = Duration::from_secs(10);

/// Which `du` this is, as far as `du -sk` goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DuFlavor {
    /// GNU coreutils: a directory that cannot be listed still adds its own blocks.
    Gnu,
    /// BSD (macOS): a directory that cannot be listed adds nothing.
    Bsd,
}

impl DuFlavor {
    /// A name for tables.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Gnu => "GNU du",
            Self::Bsd => "BSD du",
        }
    }
}

/// `du` could not be run.
#[derive(Debug, Error)]
pub enum DuError {
    /// The process could not be run.
    #[error(transparent)]
    Process(#[from] ProcessError),
}

/// The `du` on this machine, and which flavor it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Du {
    program: PathBuf,
    flavor: DuFlavor,
}

impl Du {
    /// Finds `du` on `PATH` and tells GNU from BSD by asking it for its version, as GNU answers
    /// and BSD refuses. `None` when there is no `du` (Windows has none) or it cannot be run.
    #[must_use]
    pub fn find() -> Option<Self> {
        let program = find_on_path("du")?;
        let finished = run_with(
            &program,
            &["--version"],
            Path::new("."),
            VERSION_TIMEOUT,
            None,
        )
        .ok()?;
        let gnu = finished.ended.code() == Some(0)
            && String::from_utf8_lossy(&finished.stdout.head).contains("GNU");
        Some(Self {
            program,
            flavor: if gnu { DuFlavor::Gnu } else { DuFlavor::Bsd },
        })
    }

    /// The flavor of this `du`.
    #[must_use]
    pub const fn flavor(&self) -> DuFlavor {
        self.flavor
    }

    /// Runs `du -sk <root>` in `cwd`, bounded by `timeout`.
    ///
    /// # Errors
    ///
    /// Returns [`DuError`] when the process cannot be run. A `du` that fails, or is killed, is a
    /// successful call: see [`DuRun`].
    pub fn run(&self, root: &Path, cwd: &Path, timeout: Duration) -> Result<DuRun, DuError> {
        let finished = run_with(&self.program, &["-sk"], cwd, timeout, Some(root))?;
        let kib = String::from_utf8_lossy(&finished.stdout.head)
            .split_whitespace()
            .next()
            .and_then(|total| total.parse().ok());
        Ok(DuRun { kib, finished })
    }

    /// What `du -sk` prints for the tree in `oracle`, or `None` where the oracle has no
    /// allocation facts.
    #[must_use]
    pub fn expected_kib(&self, oracle: &Oracle) -> Option<u64> {
        expected_kib(oracle, self.flavor)
    }
}

/// One `du -sk` run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuRun {
    /// The total `du` printed, in KiB, when it printed one. `du` prints the total of what it could
    /// read even when it exits non-zero.
    pub kib: Option<u64>,
    /// How the process ended, and what was measured.
    pub finished: Finished,
}

/// What `du -sk` prints for the tree in `oracle` under `flavor`.
///
/// `None` when the oracle has no allocation facts (Windows).
#[must_use]
pub fn expected_kib(oracle: &Oracle, flavor: DuFlavor) -> Option<u64> {
    if !oracle.platform.allocation {
        return None;
    }
    let mut seen: HashSet<(u64, u64)> = HashSet::new();
    let mut allocated: u128 = 0;
    for entry in &oracle.entries {
        if entry.kind == NodeKind::Directory {
            if !entry.readable && flavor == DuFlavor::Bsd {
                continue;
            }
        } else if let (Some(device), Some(inode), Some(links)) = (entry.dev, entry.ino, entry.nlink)
            && links > 1
            && !seen.insert((device, inode))
        {
            continue;
        }
        allocated += u128::from(entry.allocated.unwrap_or(0));
    }
    u64::try_from(allocated.div_ceil(1024)).ok()
}

fn run_with(
    program: &Path,
    flags: &[&str],
    cwd: &Path,
    timeout: Duration,
    root: Option<&Path>,
) -> Result<Finished, ProcessError> {
    let mut command = Command::new(program);
    command.args(flags);
    if let Some(root) = root {
        command.arg(root);
    }
    command.env_clear().current_dir(cwd);
    process::run(&mut command, timeout, false, false)
}

/// The first executable file called `name` in a directory of `PATH`.
fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|directory| directory.join(name))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}
