use crate::model::NodeId;
use ::std::ffi::OsString;
use ::std::path::{Path, PathBuf};

use crate::state::tiles::FileType;

#[derive(Clone, Debug)]
pub struct FileToDelete {
    pub node_id: NodeId,
    pub synthetic: bool,
    pub path_in_filesystem: PathBuf,
    pub path_to_file: Vec<OsString>,
    pub file_type: FileType,
    pub num_descendants: Option<u64>,
    pub size: u128,
    pub expected_snapshot: crate::model::EntrySnapshot,
    pub reviewed_entries: Vec<crate::deletion::ReviewedEntry>,
}

impl FileToDelete {
    #[must_use]
    pub fn full_path(&self) -> PathBuf {
        let mut full_path = self.path_in_filesystem.clone();
        for component in &self.path_to_file {
            full_path.push(component);
        }
        full_path
    }

    #[must_use]
    pub fn display_copy(&self) -> Self {
        Self {
            node_id: self.node_id,
            synthetic: self.synthetic,
            path_in_filesystem: self.path_in_filesystem.clone(),
            path_to_file: self.path_to_file.clone(),
            file_type: self.file_type,
            num_descendants: self.num_descendants,
            size: self.size,
            expected_snapshot: self.expected_snapshot.clone(),
            reviewed_entries: Vec::new(),
        }
    }
}

/// A memory-compacted concrete directory that must be scanned again before it
/// can become a deletion target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirectoryMaterialization {
    node_id: NodeId,
    path: PathBuf,
    expected_identity: crate::native_path::NativeIdentity,
}

impl DirectoryMaterialization {
    pub(crate) fn new(
        node_id: NodeId,
        path: PathBuf,
        expected_identity: crate::native_path::NativeIdentity,
    ) -> Self {
        Self {
            node_id,
            path,
            expected_identity,
        }
    }

    #[must_use]
    pub const fn node_id(&self) -> NodeId {
        self.node_id
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn expected_identity(&self) -> &crate::native_path::NativeIdentity {
        &self.expected_identity
    }
}

/// The deletion action available for a retained model entry.
///
/// [`Self::RequiresMaterialization`] is not deletion authorization. Its path
/// and identity must root a fresh no-follow scan, after which callers ask the
/// [`crate::state::files::FileTree`] for eligibility again.
#[derive(Clone, Debug)]
pub enum DeletionEligibility {
    Ready(FileToDelete),
    RequiresMaterialization(DirectoryMaterialization),
}
