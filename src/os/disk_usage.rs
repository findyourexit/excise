use std::fs::Metadata;
use std::io;
use std::path::Path;

use filesize::PathExt;

pub(crate) fn physical_size(path: &Path, metadata: &Metadata) -> io::Result<u64> {
    path.size_on_disk_fast(metadata)
}
