//! The generation plan: the deterministic listing of everything a spec creates.
//!
//! [`Plan::new`] expands a validated [`FixtureSpec`] into a sorted list of [`ManifestEntry`]
//! values without touching the file system. The list is the manifest: it records what the
//! generator intends, and the [oracle](crate::fixture::oracle) records what a walk of the
//! generated tree actually finds. Names and sizes come from the spec and its seed only, through
//! the generator's own PRNG; there are no timestamps and no hash-map iteration anywhere, so the
//! same spec and seed give the same manifest on every machine.
//!
//! # Canonical order and hash
//!
//! Entries are sorted component by component in raw byte order ([`RelPath`]'s order), so a
//! directory is followed by its contents. The manifest hash is the SHA-256 of a byte string that
//! is defined here and pinned by tests:
//!
//! ```text
//! "excise-harness-manifest\0"  u32le 1  u64le entry_count
//! then, for every entry in order:
//!   u32le len, path bytes                       the `/`-separated relative path
//!   u8    kind                                  0 directory, 1 file, 2 symlink
//!   u64le size
//!   u8    1 u32le len, bytes | 0                the symlink target
//!   u8    1 u32le | 0                           the hard-link group
//!   u8    1 u32le | 0                           the permission override
//!   u8    1 u64le | 0                           the bytes of data at the start of a sparse file
//!   u8    1 u32le len, bytes | 0                the path this file is a clone of
//!   u8    1 u32le len, bytes | 0                the capability the entry needs
//! ```
//!
//! # Capabilities
//!
//! An entry that needs a [`Capability`] is part of the plan on every machine. Where the file
//! system lacks the capability the generator skips the entry, and the fixture's *realized* hash,
//! computed over the entries that were created, differs from the plan hash by exactly those
//! entries.

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::{
    fixture::{
        GENERATOR_VERSION, NodeKind,
        caps::{Capabilities, Capability},
        path::{LinkTarget, RelPath},
        spec::{FixtureSpec, SpecError, SpecProblem, SpecProblems, hex},
    },
    string_enum::string_enum,
};

mod parts;

#[cfg(test)]
mod tests;

/// The version of the manifest hash encoding. A change here is a change to every manifest hash.
const HASH_FORMAT_VERSION: u32 = 1;

/// The `schema_version` of the manifest document.
pub const MANIFEST_SCHEMA_VERSION: u32 = 1;

/// One entry the generator creates, below the fixture root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestEntry {
    /// The path relative to the fixture root. Never empty: the root itself is not an entry.
    pub path: RelPath,
    /// The kind of entry.
    pub kind: NodeKind,
    /// The apparent size in bytes: `st_size`. For a file, its length; for a symbolic link, the
    /// length of its target; `0` for a directory, whose own size is a property of the file
    /// system.
    pub size: u64,
    /// The text of a symbolic link.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<LinkTarget>,
    /// The hard-link group, numbered from 1 in order of first appearance. Every member of a group
    /// is another name for the same file. The first member in canonical order is the file itself
    /// and the others are created as links to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_group: Option<u32>,
    /// A permission mask applied after everything below the entry exists, for the unreadable
    /// entries of the hostile class. `None` leaves the default mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
    /// For a sparse file: the bytes of real data written at the start. The rest is a hole.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sparse_data: Option<u64>,
    /// For a clone: the path of the file it is a copy-on-write clone of.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clone_of: Option<RelPath>,
    /// The capability the entry needs. The generator skips the entry where it is missing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires: Option<Capability>,
}

impl ManifestEntry {
    fn new(path: RelPath, kind: NodeKind, size: u64) -> Self {
        Self {
            path,
            kind,
            size,
            target: None,
            link_group: None,
            mode: None,
            sparse_data: None,
            clone_of: None,
            requires: None,
        }
    }

    pub(crate) fn directory(path: RelPath) -> Self {
        Self::new(path, NodeKind::Directory, 0)
    }

    pub(crate) fn file(path: RelPath, size: u64) -> Self {
        Self::new(path, NodeKind::File, size)
    }

    pub(crate) fn symlink(path: RelPath, target: &[u8]) -> Self {
        let mut entry = Self::new(path, NodeKind::Symlink, target.len() as u64);
        entry.target = Some(LinkTarget::new(target));
        entry.requires = Some(Capability::Symlinks);
        entry
    }

    pub(crate) fn requiring(mut self, capability: Capability) -> Self {
        self.requires = Some(capability);
        self
    }

    pub(crate) const fn with_mode(mut self, mode: u32) -> Self {
        self.mode = Some(mode);
        self
    }

    /// Whether the entry is created where `capabilities` are the available ones.
    #[must_use]
    pub fn is_created_with(&self, capabilities: &Capabilities) -> bool {
        self.requires
            .is_none_or(|capability| capabilities.supports(capability))
    }
}

/// A volume a run copy can attach at a mount point of the fixture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumePlan {
    /// The mount-point directory, an entry of the plan.
    pub mount: RelPath,
    /// The size of the volume in MiB.
    pub size_mib: u32,
    /// Files written onto the volume after it is attached.
    pub files: u32,
    /// The size of each of those files.
    pub file_bytes: u64,
}

impl VolumePlan {
    /// The names of the files written onto the volume, relative to the mount point.
    pub fn file_names(&self) -> impl Iterator<Item = Vec<u8>> {
        (0..self.files).map(|index| format!("v{index}.dat").into_bytes())
    }
}

string_enum! {
    /// The `document_kind` of a manifest document.
    pub enum ManifestKind {
        /// The manifest of one fixture.
        Manifest => "harness-fixture-manifest",
    }
}

/// The manifest as a JSON document: the header that identifies the fixture, then every entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Always `harness-fixture-manifest`.
    pub document_kind: ManifestKind,
    /// The version of this document's shape.
    pub schema_version: u32,
    /// The generator version that produced the plan.
    pub generator_version: u32,
    /// The spec id.
    pub spec_id: String,
    /// The spec hash.
    pub spec_hash: String,
    /// The seed the plan was expanded with.
    pub seed: u64,
    /// The manifest hash of `entries`.
    pub manifest_sha256: String,
    /// Every entry, in canonical order.
    pub entries: Vec<ManifestEntry>,
}

/// The expanded, hashed plan of one spec and seed.
#[derive(Debug, Clone)]
pub struct Plan {
    spec: FixtureSpec,
    spec_hash: String,
    entries: Vec<ManifestEntry>,
    volumes: Vec<VolumePlan>,
    manifest_sha256: String,
}

impl Plan {
    /// Expands `spec`, with the seed it carries. Call [`FixtureSpec::with_seed`] first to plan
    /// another seed.
    ///
    /// # Errors
    ///
    /// Returns [`SpecError::Invalid`] when the spec breaks a rule, or when two of its parts plan
    /// the same name (for example a directory style and a file style that produce one name).
    pub fn new(spec: &FixtureSpec) -> Result<Self, SpecError> {
        spec.validate().map_err(|problems| SpecError::Invalid {
            path: None,
            problems,
        })?;
        let mut entries = Vec::new();
        let mut volumes = Vec::new();
        for part in &spec.parts {
            parts::expand(part, spec.seed, &mut entries, &mut volumes);
        }
        entries.sort_unstable_by(|left, right| left.path.cmp(&right.path));
        if let Some(pair) = entries.windows(2).find(|pair| pair[0].path == pair[1].path) {
            return Err(SpecError::Invalid {
                path: None,
                problems: SpecProblems(vec![SpecProblem {
                    location: "parts".to_owned(),
                    message: format!("the name `{}` is planned twice", pair[0].path),
                }]),
            });
        }
        finish_link_groups(&mut entries);
        let manifest_sha256 = hash_entries(entries.iter());
        Ok(Self {
            spec_hash: spec.spec_hash(),
            spec: spec.clone(),
            entries,
            volumes,
            manifest_sha256,
        })
    }

    /// The spec, with the seed the plan was expanded with.
    #[must_use]
    pub const fn spec(&self) -> &FixtureSpec {
        &self.spec
    }

    /// The spec hash: SHA-256 of the canonical spec, seed included, as 64 hex digits.
    #[must_use]
    pub fn spec_hash(&self) -> &str {
        &self.spec_hash
    }

    /// The seed.
    #[must_use]
    pub const fn seed(&self) -> u64 {
        self.spec.seed
    }

    /// Every entry, in canonical order.
    #[must_use]
    pub fn entries(&self) -> &[ManifestEntry] {
        &self.entries
    }

    /// The volumes a run copy can attach.
    #[must_use]
    pub fn volumes(&self) -> &[VolumePlan] {
        &self.volumes
    }

    /// The manifest hash of the whole plan, as 64 hex digits. The same on every machine.
    #[must_use]
    pub fn manifest_sha256(&self) -> &str {
        &self.manifest_sha256
    }

    /// The entries created where `capabilities` are the available ones, in canonical order.
    pub fn realized<'a>(
        &'a self,
        capabilities: &'a Capabilities,
    ) -> impl Iterator<Item = &'a ManifestEntry> + Clone {
        self.entries
            .iter()
            .filter(|entry| entry.is_created_with(capabilities))
    }

    /// The manifest hash of the entries created where `capabilities` are the available ones. It
    /// equals [`Plan::manifest_sha256`] when nothing is skipped.
    #[must_use]
    pub fn realized_sha256(&self, capabilities: &Capabilities) -> String {
        hash_entries(self.realized(capabilities))
    }

    /// The names of the top-level entries, in order: one per part.
    #[must_use]
    pub fn top_level_names(&self) -> Vec<&[u8]> {
        self.entries
            .iter()
            .filter(|entry| entry.path.depth() == 1)
            .filter_map(|entry| entry.path.file_name())
            .collect()
    }

    /// The manifest as a document.
    #[must_use]
    pub fn manifest(&self) -> Manifest {
        Manifest {
            document_kind: ManifestKind::Manifest,
            schema_version: MANIFEST_SCHEMA_VERSION,
            generator_version: GENERATOR_VERSION,
            spec_id: self.spec.id.clone(),
            spec_hash: self.spec_hash.clone(),
            seed: self.spec.seed,
            manifest_sha256: self.manifest_sha256.clone(),
            entries: self.entries.clone(),
        }
    }
}

/// Numbers the hard-link groups from 1 in canonical order of first appearance, and marks every
/// member but the first as needing hard links: the first is the file, the rest are links to it.
fn finish_link_groups(entries: &mut [ManifestEntry]) {
    let mut renumbered: Vec<(u32, u32)> = Vec::new();
    for entry in entries.iter_mut() {
        let Some(temporary) = entry.link_group else {
            continue;
        };
        if let Some((_, number)) = renumbered.iter().find(|(old, _)| *old == temporary) {
            entry.link_group = Some(*number);
            entry.requires = Some(Capability::HardLinks);
        } else {
            let number = u32::try_from(renumbered.len() + 1).unwrap_or(u32::MAX);
            renumbered.push((temporary, number));
            entry.link_group = Some(number);
        }
    }
}

/// The manifest hash of `entries`, which must be in canonical order.
pub(crate) fn hash_entries<'a>(entries: impl Iterator<Item = &'a ManifestEntry> + Clone) -> String {
    let mut feed = Feed(Sha256::new());
    feed.raw(b"excise-harness-manifest\0");
    feed.u32(HASH_FORMAT_VERSION);
    feed.u64(entries.clone().count() as u64);
    for entry in entries {
        feed.bytes(entry.path.as_bytes());
        feed.u8(match entry.kind {
            NodeKind::Directory => 0,
            NodeKind::File => 1,
            NodeKind::Symlink => 2,
            NodeKind::Other => 3,
        });
        feed.u64(entry.size);
        feed.optional_bytes(entry.target.as_ref().map(LinkTarget::as_bytes));
        feed.optional_u32(entry.link_group);
        feed.optional_u32(entry.mode);
        feed.optional_u64(entry.sparse_data);
        feed.optional_bytes(entry.clone_of.as_ref().map(RelPath::as_bytes));
        feed.optional_bytes(
            entry
                .requires
                .map(|capability| capability.as_str().as_bytes()),
        );
    }
    hex(&feed.0.finalize())
}

/// Feeds the hash in the fixed-width, length-prefixed encoding documented on the module.
struct Feed(Sha256);

impl Feed {
    fn raw(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    fn u8(&mut self, value: u8) {
        self.0.update([value]);
    }

    fn u32(&mut self, value: u32) {
        self.0.update(value.to_le_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.0.update(value.to_le_bytes());
    }

    fn bytes(&mut self, bytes: &[u8]) {
        self.u32(u32::try_from(bytes.len()).unwrap_or(u32::MAX));
        self.0.update(bytes);
    }

    fn optional_bytes(&mut self, bytes: Option<&[u8]>) {
        match bytes {
            Some(bytes) => {
                self.u8(1);
                self.bytes(bytes);
            }
            None => self.u8(0),
        }
    }

    fn optional_u32(&mut self, value: Option<u32>) {
        match value {
            Some(value) => {
                self.u8(1);
                self.u32(value);
            }
            None => self.u8(0),
        }
    }

    fn optional_u64(&mut self, value: Option<u64>) {
        match value {
            Some(value) => {
                self.u8(1);
                self.u64(value);
            }
            None => self.u8(0),
        }
    }
}
