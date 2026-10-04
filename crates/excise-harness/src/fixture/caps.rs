//! What the file system under a fixture, and the process generating it, can do.
//!
//! Some fixture entries need more than a plain file system offers: APFS rejects names that are
//! not valid UTF-8, ext4 shares no extents between clones, Windows takes no permission masks.
//! Instead of failing, the generator probes the file system once, skips the entries that need a
//! missing capability, and records each capability with the reason in the fixture's marker.

use std::{
    collections::BTreeMap,
    io::{self, Write as _},
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

use serde::{Deserialize, Serialize};

use crate::{
    fixture::{sys::Dir, tree::TreeGuard},
    string_enum::string_enum,
};

string_enum! {
    /// Something a file system, or the process, may not be able to do.
    ///
    /// A plan entry names the capability it needs; the generator skips the entry where the
    /// capability is missing.
    pub enum Capability {
        /// Creating symbolic links.
        Symlinks => "symlinks",
        /// Creating hard links.
        HardLinks => "hard_links",
        /// Sparse files: an apparent size much larger than the allocated size.
        SparseFiles => "sparse_files",
        /// Copy-on-write clones: an APFS clone on macOS, a reflink on Linux.
        Clones => "clones",
        /// File names that are not valid UTF-8. APFS answers `EILSEQ`; Windows names are UTF-16.
        InvalidUtf8Names => "invalid_utf8_names",
        /// File names with control characters and line breaks. Windows rejects them.
        ControlCharacterNames => "control_character_names",
        /// Setting permission masks such as `000`.
        RestrictedModes => "restricted_modes",
    }
}

/// Whether one [`Capability`] is available, and the evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityStatus {
    /// Whether the capability is available.
    pub supported: bool,
    /// What the probe observed: the error it got, or how it succeeded.
    pub detail: String,
}

/// The capabilities of the file system a fixture is generated on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    statuses: BTreeMap<Capability, CapabilityStatus>,
    /// Whether the process runs as root, so that a mode `000` file is still readable to it. Not a
    /// capability of the file system: it decides whether restricted entries actually restrict.
    /// `None` where the platform cannot say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    running_as_root: Option<bool>,
}

/// The apparent size of the probe's sparse file. APFS keeps a file sparse only when it is larger
/// than 16 MiB (a smaller file that holds a full block of data is allocated in full when it is
/// extended), so the probe uses a size a sparse fixture file would also need.
const SPARSE_PROBE_BYTES: u64 = 32 * 1024 * 1024;
/// The real data the probe writes before the hole: one full block.
const SPARSE_PROBE_DATA: [u8; 4096] = [0x5a; 4096];
/// The size of the probe's clone source: several file system blocks of real data.
const CLONE_PROBE_BYTES: usize = 16 * 1024;

impl Capabilities {
    /// Every capability available, as on a permissive Unix file system. What a plan-only
    /// computation assumes.
    #[must_use]
    pub fn all_supported() -> Self {
        Self {
            statuses: Capability::ALL
                .iter()
                .map(|capability| {
                    (
                        *capability,
                        CapabilityStatus {
                            supported: true,
                            detail: "assumed".to_owned(),
                        },
                    )
                })
                .collect(),
            running_as_root: None,
        }
    }

    /// Whether `capability` is available.
    #[must_use]
    pub fn supports(&self, capability: Capability) -> bool {
        self.statuses
            .get(&capability)
            .is_some_and(|status| status.supported)
    }

    /// The status of `capability`.
    #[must_use]
    pub fn status(&self, capability: Capability) -> Option<&CapabilityStatus> {
        self.statuses.get(&capability)
    }

    /// Every capability with its status, in a fixed order.
    pub fn iter(&self) -> impl Iterator<Item = (Capability, &CapabilityStatus)> {
        self.statuses
            .iter()
            .map(|(capability, status)| (*capability, status))
    }

    /// Whether the probing process runs as root, if that could be determined.
    #[must_use]
    pub const fn running_as_root(&self) -> Option<bool> {
        self.running_as_root
    }

    /// Returns these capabilities with `capability` overridden as unsupported. For tests and for
    /// runs that want to exercise the skip path.
    #[must_use]
    pub fn without(mut self, capability: Capability, reason: &str) -> Self {
        self.statuses.insert(
            capability,
            CapabilityStatus {
                supported: false,
                detail: reason.to_owned(),
            },
        );
        self
    }

    /// Returns these capabilities with the answer to whether the process runs as root replaced.
    /// For tests, which cannot become root.
    #[must_use]
    pub fn with_running_as_root(mut self, running_as_root: Option<bool>) -> Self {
        self.running_as_root = running_as_root;
        self
    }

    /// Probes the file system that holds `parent`.
    ///
    /// The probe creates one scratch directory inside `parent`, tries each capability for real,
    /// and removes the directory again.
    ///
    /// # Errors
    ///
    /// Returns an error when the scratch directory cannot be created or removed. A capability
    /// that merely fails is reported as unsupported, not as an error.
    pub fn probe(parent: &Path) -> io::Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);

        let mut attempts = 0;
        let (guard, dir) = loop {
            let path = parent.join(format!(
                ".excise-probe-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match Dir::create_root(&path) {
                Ok(dir) => break (TreeGuard::new(path), dir),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists && attempts < 8 => {
                    attempts += 1;
                }
                Err(error) => return Err(error),
            }
        };

        let mut statuses = BTreeMap::new();
        statuses.insert(
            Capability::Symlinks,
            judge(dir.symlink(b"link", b"target"), "created a link"),
        );
        statuses.insert(Capability::HardLinks, probe_hard_link(&dir));
        statuses.insert(Capability::SparseFiles, probe_sparse(&dir));
        statuses.insert(Capability::Clones, probe_clone(&dir));
        statuses.insert(
            Capability::InvalidUtf8Names,
            judge(
                dir.create_file(b"bad-\xff\xfe").map(drop),
                "created a file named with invalid UTF-8",
            ),
        );
        statuses.insert(
            Capability::ControlCharacterNames,
            judge(
                dir.create_file(b"ctl-\x01\n\x1b").map(drop),
                "created a file named with control characters",
            ),
        );
        statuses.insert(Capability::RestrictedModes, probe_modes(&dir));
        let running_as_root = dir
            .create_file(b"owner")
            .ok()
            .and_then(|_| dir.stat(b"owner").ok())
            .and_then(|stat| stat.uid)
            .map(|uid| uid == 0);

        drop(dir);
        guard.remove()?;
        Ok(Self {
            statuses,
            running_as_root,
        })
    }
}

fn judge<T>(result: io::Result<T>, success: &str) -> CapabilityStatus {
    match result {
        Ok(_) => CapabilityStatus {
            supported: true,
            detail: success.to_owned(),
        },
        Err(error) => CapabilityStatus {
            supported: false,
            detail: error.to_string(),
        },
    }
}

fn probe_hard_link(dir: &Dir) -> CapabilityStatus {
    let result = dir
        .create_file(b"original")
        .and_then(|_| dir.hard_link(b"original", dir, b"linked"));
    judge(result, "created a hard link")
}

fn probe_sparse(dir: &Dir) -> CapabilityStatus {
    let outcome = (|| -> io::Result<CapabilityStatus> {
        let mut file = dir.create_file(b"sparse")?;
        file.write_all(&SPARSE_PROBE_DATA)?;
        file.set_len(SPARSE_PROBE_BYTES)?;
        drop(file);
        let stat = dir.stat(b"sparse")?;
        Ok(match stat.allocated {
            Some(allocated) if allocated < SPARSE_PROBE_BYTES / 2 => CapabilityStatus {
                supported: true,
                detail: format!(
                    "{SPARSE_PROBE_BYTES} apparent bytes occupy {allocated} allocated bytes"
                ),
            },
            Some(allocated) => CapabilityStatus {
                supported: false,
                detail: format!(
                    "{allocated} of {SPARSE_PROBE_BYTES} bytes were allocated after extending the file"
                ),
            },
            None => CapabilityStatus {
                supported: false,
                detail: "the allocated size of a file is not available here".to_owned(),
            },
        })
    })();
    outcome.unwrap_or_else(|error| CapabilityStatus {
        supported: false,
        detail: error.to_string(),
    })
}

fn probe_clone(dir: &Dir) -> CapabilityStatus {
    let result = (|| -> io::Result<()> {
        let mut source = dir.create_file(b"clone-source")?;
        source.write_all(&vec![0x5a; CLONE_PROBE_BYTES])?;
        drop(source);
        dir.clone_file(b"clone-copy", dir, b"clone-source")
    })();
    judge(result, "cloned a file")
}

fn probe_modes(dir: &Dir) -> CapabilityStatus {
    let result = dir
        .create_dir(b"locked")
        .and_then(|()| dir.chmod(b"locked", 0o000))
        // The probe directory must stay removable.
        .and_then(|()| dir.chmod(b"locked", 0o700));
    judge(result, "set a restrictive mode")
}
