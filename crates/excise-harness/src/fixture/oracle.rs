//! The oracle: what an independent `lstat` walk finds in a materialized tree.
//!
//! The manifest records what the generator intends; the oracle records what is there. It is built
//! by walking the tree with `lstat` (never following a link), reading every fact from the file
//! system and nothing from the plan, so it also catches a generator that does not do what it
//! says. Later runners compare Excise's own report with it.
//!
//! # Facts, not rules
//!
//! The oracle keeps raw facts and applies none of Excise's accounting rules (directory metadata
//! excluded, allocation counted once per identity, and so on: see `docs/safety/accounting.md`).
//! Per entry it records the kind, the apparent size (`st_size`), the allocated bytes
//! (`st_blocks * 512`), the device, inode, and link count, the permission bits, and whether the
//! entry is readable. Per directory it adds a [`Subtree`]: descendant counts and byte totals,
//! with allocated bytes counted once per `(dev, ino)` within the subtree.
//!
//! # Reading it like `du`
//!
//! `du -sk` on the root reports, in KiB rounded up, the allocated bytes of the root directory,
//! of every directory in the subtree, and of every file in it once per identity. In oracle terms:
//! `root.allocated + subtree.directory_allocated_bytes + subtree.allocated_bytes`. The apparent
//! total (`du --apparent-size`) adds `size` instead of `allocated`. BSD `du -A` rounds every entry
//! up to 512 bytes first. The tests spell these formulas out and check them against the real
//! `du`.
//!
//! # Platforms
//!
//! On Unix every field is filled in. On Windows the layer beneath the fixture generator has only
//! stable `std`, which offers neither the volume serial number, the file index, the link count,
//! nor the allocated size (they need `GetFileInformationByHandle` and `GetCompressedFileSize`,
//! or the unstable `windows_by_handle` feature), so `allocated`, `dev`, `ino`, `nlink`, and
//! `mode` are `null` and no hard-link groups are found.
//!
//! # Paths
//!
//! Paths are relative to the root and use the encoding of Excise's own JSON reports
//! (`docs/schemas/native-path.schema.json`): `unix-bytes` with base64 data on Unix, so a name that
//! is not valid UTF-8 survives. The root itself has the empty path and comes first; the rest
//! follow in canonical order.

use std::{collections::BTreeMap, io, path::Path};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    fixture::{
        MARKER_FILE_NAME, NodeKind,
        caps::Capabilities,
        path::{LinkTarget, RelPath},
        plan::{ManifestEntry, Plan},
        sys::{Dir, Stat},
    },
    string_enum::string_enum,
};

/// The `schema_version` of the oracle document.
pub const ORACLE_SCHEMA_VERSION: u32 = 1;

string_enum! {
    /// The `document_kind` of an oracle document.
    pub enum OracleKind {
        /// The oracle of one tree.
        Oracle => "harness-fixture-oracle",
    }
}

/// What the platform layer could report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OraclePlatform {
    /// The operating system, as `std::env::consts::OS` names it.
    pub os: String,
    /// Whether `dev`, `ino`, and `nlink` are filled in.
    pub identity: bool,
    /// Whether `allocated` is filled in.
    pub allocation: bool,
}

/// The oracle of one tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Oracle {
    /// Always `harness-fixture-oracle`.
    pub document_kind: OracleKind,
    /// The version of this document's shape.
    pub schema_version: u32,
    /// What the platform could report.
    pub platform: OraclePlatform,
    /// The root (empty path) first, then every entry below it in canonical order.
    pub entries: Vec<OracleEntry>,
    /// The files that have more than one name, with the names the walk found.
    pub hard_links: Vec<HardLinkGroup>,
}

/// The facts about one entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OracleEntry {
    /// The path relative to the root; empty for the root.
    pub path: RelPath,
    /// The kind `lstat` reports.
    pub kind: NodeKind,
    /// `st_size`. For a directory, a property of the file system; for a symbolic link, the length
    /// of its target.
    pub size: u64,
    /// `st_blocks * 512`, or `null` where the platform cannot say.
    pub allocated: Option<u64>,
    /// `st_dev`, or `null`.
    pub dev: Option<u64>,
    /// `st_ino`, or `null`.
    pub ino: Option<u64>,
    /// `st_nlink`, or `null`.
    pub nlink: Option<u64>,
    /// The permission bits, `st_mode & 0o7777`, or `null`.
    pub mode: Option<u32>,
    /// Whether this process can read the entry: for a directory, open and list it; for a regular
    /// file, open it for reading. Always `true` for a link. A fact about the process that walked
    /// the tree: a root process reads a mode `000` file.
    pub readable: bool,
    /// Whether this directory is on another device than its parent: a mount point.
    pub device_boundary: bool,
    /// The text of a symbolic link.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symlink_target: Option<LinkTarget>,
    /// The totals below a directory. Absent for other kinds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtree: Option<Subtree>,
}

/// The totals below one directory, the directory itself excluded.
///
/// A directory that could not be listed contributes no descendants and counts in
/// `unreadable_directories` of its ancestors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Subtree {
    /// Descendants of every kind.
    pub entries: u64,
    /// Regular files.
    pub files: u64,
    /// Directories.
    pub directories: u64,
    /// Symbolic links.
    pub symlinks: u64,
    /// Everything else.
    pub others: u64,
    /// Descendant directories that could not be listed.
    pub unreadable_directories: u64,
    /// The sum of `size` over the descendants that are not directories, every name counted.
    pub apparent_bytes: u64,
    /// The same sum with each `(dev, ino)` counted once.
    pub apparent_unique_bytes: u64,
    /// The sum of `allocated` over the descendants that are not directories, each `(dev, ino)`
    /// counted once. `null` where the platform cannot say.
    pub allocated_bytes: Option<u64>,
    /// The sum of `size` over the descendant directories.
    pub directory_apparent_bytes: u64,
    /// The sum of `allocated` over the descendant directories. `null` where the platform cannot
    /// say.
    pub directory_allocated_bytes: Option<u64>,
}

/// One file with several names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HardLinkGroup {
    /// The device.
    pub dev: u64,
    /// The inode.
    pub ino: u64,
    /// The link count the file system reports, which may exceed the names found when other names
    /// lie outside the walked tree.
    pub nlink: u64,
    /// The names the walk found, in canonical order.
    pub paths: Vec<RelPath>,
}

/// How to walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OracleOptions {
    /// Whether to try opening every regular file to learn if it is readable. One more system call
    /// per file; without it every file is reported readable.
    pub probe_file_readability: bool,
}

impl Default for OracleOptions {
    fn default() -> Self {
        Self {
            probe_file_readability: true,
        }
    }
}

/// Why the walk failed.
#[derive(Debug, Error)]
pub enum OracleError {
    /// The root cannot be opened.
    #[error("cannot open `{}` for the oracle walk: {source}", path.display())]
    Root {
        /// The root.
        path: std::path::PathBuf,
        /// The underlying error.
        source: io::Error,
    },
    /// An entry could not be inspected.
    #[error("cannot {operation} `{path}`: {source}")]
    Entry {
        /// What was being done.
        operation: &'static str,
        /// The path, escaped for display.
        path: RelPath,
        /// The underlying error.
        source: io::Error,
    },
}

impl Oracle {
    /// Walks the tree at `root` with the default options.
    ///
    /// # Errors
    ///
    /// See [`Oracle::collect_with`].
    pub fn collect(root: &Path) -> Result<Self, OracleError> {
        Self::collect_with(root, OracleOptions::default())
    }

    /// Walks the tree at `root`. The root must be a directory and not a link. A directory that
    /// cannot be listed because of its permissions is recorded as unreadable; any other failure
    /// aborts the walk.
    ///
    /// # Errors
    ///
    /// Returns [`OracleError::Root`] when the root cannot be opened, and [`OracleError::Entry`]
    /// for an I/O failure other than a permission error.
    pub fn collect_with(root: &Path, options: OracleOptions) -> Result<Self, OracleError> {
        let directory = Dir::open_root(root).map_err(|source| OracleError::Root {
            path: root.to_path_buf(),
            source,
        })?;
        let own = directory.stat_self().map_err(|source| OracleError::Entry {
            operation: "inspect",
            path: RelPath::root(),
            source,
        })?;
        let mut walker = Walker {
            options,
            entries: Vec::new(),
            identities: BTreeMap::new(),
        };
        let index = walker.push(RelPath::root(), &own, None, true);
        let below = walker.list_directory(&directory, &RelPath::root(), &own)?;
        walker.finish_directory(index, below.as_ref());

        let hard_links = walker
            .identities
            .into_iter()
            .filter(|(_, group)| group.nlink > 1)
            .map(|((dev, ino), group)| HardLinkGroup {
                dev,
                ino,
                nlink: group.nlink,
                paths: group.paths,
            })
            .collect();
        Ok(Self {
            document_kind: OracleKind::Oracle,
            schema_version: ORACLE_SCHEMA_VERSION,
            platform: OraclePlatform {
                os: std::env::consts::OS.to_owned(),
                identity: own.ino.is_some(),
                allocation: own.allocated.is_some(),
            },
            entries: walker.entries,
            hard_links,
        })
    }

    /// The root directory.
    #[must_use]
    pub fn root(&self) -> &OracleEntry {
        &self.entries[0]
    }

    /// The entry at `path`, if the walk found one.
    #[must_use]
    pub fn find(&self, path: &RelPath) -> Option<&OracleEntry> {
        self.entries
            .binary_search_by(|entry| entry.path.cmp(path))
            .ok()
            .map(|index| &self.entries[index])
    }

    /// The entries below the root, in canonical order.
    pub fn descendants(&self) -> impl Iterator<Item = &OracleEntry> {
        self.entries.iter().skip(1)
    }
}

/// The running totals of a walk below one directory.
#[derive(Debug, Default)]
struct Totals {
    entries: u64,
    files: u64,
    directories: u64,
    symlinks: u64,
    others: u64,
    unreadable_directories: u64,
    apparent: u64,
    /// Apparent bytes of entries with one name, or whose identity is unknown.
    single_apparent: u64,
    /// Allocated bytes of entries with one name, or whose identity is unknown.
    single_allocated: u64,
    /// The identities with several names inside this subtree: `(dev, ino)` to size and allocation.
    shared: BTreeMap<(u64, u64), (u64, Option<u64>)>,
    directory_apparent: u64,
    directory_allocated: u64,
    /// Whether every entry so far reported its allocation.
    allocation_known: bool,
}

impl Totals {
    fn new() -> Self {
        Self {
            allocation_known: true,
            ..Self::default()
        }
    }

    fn add_entry(&mut self, stat: &Stat) {
        self.entries += 1;
        match stat.kind {
            NodeKind::Directory => {
                self.directories += 1;
                self.directory_apparent += stat.size;
                match stat.allocated {
                    Some(allocated) => self.directory_allocated += allocated,
                    None => self.allocation_known = false,
                }
                return;
            }
            NodeKind::File => self.files += 1,
            NodeKind::Symlink => self.symlinks += 1,
            NodeKind::Other => self.others += 1,
        }
        self.apparent += stat.size;
        match (stat.dev, stat.ino, stat.nlink) {
            (Some(dev), Some(ino), Some(nlink)) if nlink > 1 => {
                self.shared
                    .entry((dev, ino))
                    .or_insert((stat.size, stat.allocated));
            }
            _ => {
                self.single_apparent += stat.size;
                match stat.allocated {
                    Some(allocated) => self.single_allocated += allocated,
                    None => self.allocation_known = false,
                }
            }
        }
        if stat.allocated.is_none() {
            self.allocation_known = false;
        }
    }

    fn merge(&mut self, child: Self) {
        self.entries += child.entries;
        self.files += child.files;
        self.directories += child.directories;
        self.symlinks += child.symlinks;
        self.others += child.others;
        self.unreadable_directories += child.unreadable_directories;
        self.apparent += child.apparent;
        self.single_apparent += child.single_apparent;
        self.single_allocated += child.single_allocated;
        for (identity, value) in child.shared {
            self.shared.entry(identity).or_insert(value);
        }
        self.directory_apparent += child.directory_apparent;
        self.directory_allocated += child.directory_allocated;
        self.allocation_known &= child.allocation_known;
    }

    fn subtree(&self) -> Subtree {
        let shared_apparent: u64 = self.shared.values().map(|(size, _)| *size).sum();
        let shared_allocated: Option<u64> = self
            .shared
            .values()
            .try_fold(0_u64, |total, (_, allocated)| {
                allocated.map(|bytes| total + bytes)
            });
        let allocated_bytes = if self.allocation_known {
            shared_allocated.map(|shared| self.single_allocated + shared)
        } else {
            None
        };
        Subtree {
            entries: self.entries,
            files: self.files,
            directories: self.directories,
            symlinks: self.symlinks,
            others: self.others,
            unreadable_directories: self.unreadable_directories,
            apparent_bytes: self.apparent,
            apparent_unique_bytes: self.single_apparent + shared_apparent,
            allocated_bytes,
            directory_apparent_bytes: self.directory_apparent,
            directory_allocated_bytes: self.allocation_known.then_some(self.directory_allocated),
        }
    }
}

struct IdentityBuilder {
    nlink: u64,
    paths: Vec<RelPath>,
}

struct Walker {
    options: OracleOptions,
    entries: Vec<OracleEntry>,
    identities: BTreeMap<(u64, u64), IdentityBuilder>,
}

impl Walker {
    fn push(
        &mut self,
        path: RelPath,
        stat: &Stat,
        parent_device: Option<u64>,
        readable: bool,
    ) -> usize {
        let device_boundary = stat.kind == NodeKind::Directory
            && parent_device.is_some()
            && stat.dev.is_some()
            && parent_device != stat.dev;
        self.entries.push(OracleEntry {
            path,
            kind: stat.kind,
            size: stat.size,
            allocated: stat.allocated,
            dev: stat.dev,
            ino: stat.ino,
            nlink: stat.nlink,
            mode: stat.mode,
            readable,
            device_boundary,
            symlink_target: None,
            subtree: None,
        });
        self.entries.len() - 1
    }

    /// Stores the totals of a directory once its children are done. A directory that could not
    /// be listed (`below` is `None`) is unreadable and has empty totals.
    fn finish_directory(&mut self, index: usize, below: Option<&Totals>) {
        let subtree = below.map_or_else(|| Totals::new().subtree(), Totals::subtree);
        let entry = &mut self.entries[index];
        entry.readable = below.is_some();
        entry.subtree = Some(subtree);
    }

    /// Lists an open directory and visits every child, returning the totals below it, or `None`
    /// when the directory cannot be listed because of its permissions.
    fn list_directory(
        &mut self,
        directory: &Dir,
        path: &RelPath,
        own: &Stat,
    ) -> Result<Option<Totals>, OracleError> {
        let mut children = match directory.list() {
            Ok(children) => children,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => return Ok(None),
            Err(source) => {
                return Err(OracleError::Entry {
                    operation: "list",
                    path: path.clone(),
                    source,
                });
            }
        };
        children.sort_unstable_by(|left, right| left.name.cmp(&right.name));
        let mut totals = Totals::new();
        for child in children {
            let child_path = path.join(&child.name);
            let stat = directory
                .stat(&child.name)
                .map_err(|source| OracleError::Entry {
                    operation: "inspect",
                    path: child_path.clone(),
                    source,
                })?;
            totals.add_entry(&stat);
            match stat.kind {
                NodeKind::Directory => {
                    match self.visit_directory(directory, &child.name, child_path, &stat, own)? {
                        Some(below) => totals.merge(below),
                        None => totals.unreadable_directories += 1,
                    }
                }
                NodeKind::Symlink => {
                    let target =
                        directory
                            .read_link(&child.name)
                            .map_err(|source| OracleError::Entry {
                                operation: "read the link",
                                path: child_path.clone(),
                                source,
                            })?;
                    self.note_identity(&child_path, &stat);
                    let index = self.push(child_path, &stat, own.dev, true);
                    self.entries[index].symlink_target = Some(LinkTarget::new(target));
                }
                NodeKind::File | NodeKind::Other => {
                    let readable = !self.options.probe_file_readability
                        || stat.kind != NodeKind::File
                        || directory.can_read(&child.name);
                    self.note_identity(&child_path, &stat);
                    self.push(child_path, &stat, own.dev, readable);
                }
            }
        }
        Ok(Some(totals))
    }

    /// Records a non-directory entry with more than one name, under its `(dev, ino)`.
    fn note_identity(&mut self, path: &RelPath, stat: &Stat) {
        if let (Some(dev), Some(ino), Some(nlink)) = (stat.dev, stat.ino, stat.nlink)
            && nlink > 1
        {
            self.identities
                .entry((dev, ino))
                .or_insert_with(|| IdentityBuilder {
                    nlink,
                    paths: Vec::new(),
                })
                .paths
                .push(path.clone());
        }
    }

    /// Visits one child directory. Returns `None` when it cannot be opened or listed.
    fn visit_directory(
        &mut self,
        parent: &Dir,
        name: &[u8],
        path: RelPath,
        stat: &Stat,
        parent_stat: &Stat,
    ) -> Result<Option<Totals>, OracleError> {
        let index = self.push(path.clone(), stat, parent_stat.dev, true);
        let opened = match parent.open_dir(name) {
            Ok(directory) => Some(directory),
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => None,
            Err(source) => {
                return Err(OracleError::Entry {
                    operation: "open",
                    path,
                    source,
                });
            }
        };
        let below = match opened {
            Some(directory) => self.list_directory(&directory, &path, stat)?,
            None => None,
        };
        self.finish_directory(index, below.as_ref());
        Ok(below)
    }
}

/// One way a tree differs from its plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Discrepancy {
    /// The plan creates the entry and the tree does not have it.
    Missing(RelPath),
    /// The tree has an entry the plan does not create.
    Unexpected(RelPath),
    /// The entry is of another kind.
    Kind {
        /// The entry.
        path: RelPath,
        /// The planned kind.
        expected: NodeKind,
        /// The kind found.
        found: NodeKind,
    },
    /// A file is not the planned size.
    Size {
        /// The entry.
        path: RelPath,
        /// The planned size.
        expected: u64,
        /// The size found.
        found: u64,
    },
    /// A symbolic link has another target.
    Target(RelPath),
    /// A permission mask is not the planned one.
    Mode {
        /// The entry.
        path: RelPath,
        /// The planned bits.
        expected: u32,
        /// The bits found.
        found: u32,
    },
    /// The names of a hard-link group are not all one file, or two groups share a file.
    LinkGroup(RelPath),
}

/// The result of checking a tree against its plan.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Comparison {
    /// Every difference found.
    pub discrepancies: Vec<Discrepancy>,
    /// The planned entries that were compared.
    pub checked: u64,
    /// The planned entries below a directory the walk could not list: they cannot be observed.
    pub unverifiable: u64,
}

impl Comparison {
    /// Whether the tree matches its plan everywhere it can be observed.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.discrepancies.is_empty()
    }
}

impl Oracle {
    /// Checks this walk against the entries `plan` creates where `capabilities` allow.
    ///
    /// The ownership marker is expected and ignored. Entries below a directory the walk could not
    /// list are counted as unverifiable instead of missing.
    #[must_use]
    pub fn compare(&self, plan: &Plan, capabilities: &Capabilities) -> Comparison {
        let mut comparison = Comparison::default();
        let marker = RelPath::from_bytes(MARKER_FILE_NAME).unwrap_or_else(|_| RelPath::root());
        let expected: Vec<&ManifestEntry> = plan.realized(capabilities).collect();

        // The directories the walk could not list.
        let sealed: Vec<&RelPath> = self
            .entries
            .iter()
            .filter(|entry| entry.kind == NodeKind::Directory && !entry.readable)
            .map(|entry| &entry.path)
            .collect();
        let is_sealed = |path: &RelPath| {
            sealed
                .iter()
                .any(|sealed| path.starts_with(sealed) && path != *sealed)
        };

        let mut observed = self.descendants().peekable();
        for planned in expected {
            while let Some(entry) = observed.peek() {
                if entry.path < planned.path {
                    if entry.path != marker && !is_sealed(&entry.path) {
                        comparison
                            .discrepancies
                            .push(Discrepancy::Unexpected(entry.path.clone()));
                    }
                    observed.next();
                } else {
                    break;
                }
            }
            let found = match observed.peek() {
                Some(entry) if entry.path == planned.path => observed.next(),
                _ => None,
            };
            if is_sealed(&planned.path) {
                comparison.unverifiable += 1;
                continue;
            }
            comparison.checked += 1;
            match found {
                None => comparison
                    .discrepancies
                    .push(Discrepancy::Missing(planned.path.clone())),
                Some(entry) => check_entry(planned, entry, &mut comparison.discrepancies),
            }
        }
        for entry in observed {
            if entry.path != marker && !is_sealed(&entry.path) {
                comparison
                    .discrepancies
                    .push(Discrepancy::Unexpected(entry.path.clone()));
            }
        }
        self.check_link_groups(plan, capabilities, &is_sealed, &mut comparison);
        comparison
    }

    fn check_link_groups(
        &self,
        plan: &Plan,
        capabilities: &Capabilities,
        is_sealed: &dyn Fn(&RelPath) -> bool,
        comparison: &mut Comparison,
    ) {
        let mut identity_of_group: BTreeMap<u32, (u64, u64)> = BTreeMap::new();
        let mut group_of_identity: BTreeMap<(u64, u64), u32> = BTreeMap::new();
        for planned in plan.realized(capabilities) {
            let Some(group) = planned.link_group else {
                continue;
            };
            if is_sealed(&planned.path) {
                continue;
            }
            let Some(entry) = self.find(&planned.path) else {
                continue;
            };
            let (Some(dev), Some(ino)) = (entry.dev, entry.ino) else {
                continue;
            };
            let identity = *identity_of_group.entry(group).or_insert((dev, ino));
            let owner = *group_of_identity.entry((dev, ino)).or_insert(group);
            if identity != (dev, ino) || owner != group {
                comparison
                    .discrepancies
                    .push(Discrepancy::LinkGroup(planned.path.clone()));
            }
        }
    }
}

/// Compares one planned entry with the entry found at its path.
fn check_entry(planned: &ManifestEntry, found: &OracleEntry, out: &mut Vec<Discrepancy>) {
    let path = || planned.path.clone();
    if found.kind != planned.kind {
        out.push(Discrepancy::Kind {
            path: path(),
            expected: planned.kind,
            found: found.kind,
        });
        return;
    }
    match planned.kind {
        NodeKind::File => {
            if found.size != planned.size {
                out.push(Discrepancy::Size {
                    path: path(),
                    expected: planned.size,
                    found: found.size,
                });
            }
        }
        NodeKind::Symlink => {
            if found.symlink_target != planned.target {
                out.push(Discrepancy::Target(path()));
            }
        }
        NodeKind::Directory | NodeKind::Other => {}
    }
    if let (Some(expected), Some(found)) = (planned.mode, found.mode)
        && expected != found
    {
        out.push(Discrepancy::Mode {
            path: path(),
            expected,
            found,
        });
    }
}
