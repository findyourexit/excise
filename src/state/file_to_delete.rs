use crate::model::NodeId;
use ::std::ffi::OsString;
use ::std::path::PathBuf;

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
