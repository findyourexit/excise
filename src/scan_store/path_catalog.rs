use std::ffi::OsString;

use thiserror::Error;

use super::path_key::{
    PathKeyError, append_path_component_key, decode_path_key, encode_path_key_into,
};
use super::path_observation::{PathObservationCodecError, decode_path_observation};
use super::path_reducer::PathEntryKind;
use super::run_file::{RunError, RunKind, RunReader, RunWriter, SealedRun};
use crate::scan_coordinator::RelativePath;

const PATH_CATALOG_VERSION: u8 = 1;
const ID_BYTES: usize = size_of::<u64>();

/// Stable compact identity for one canonical path within a generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PathId(u64);

impl PathId {
    #[must_use]
    pub(crate) const fn value(self) -> u64 {
        self.0
    }
}

/// One path row retaining only its parent identity and raw terminal component.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PathCatalogEntry {
    pub(crate) id: PathId,
    pub(crate) parent_id: PathId,
    pub(crate) name: OsString,
}

#[derive(Debug, Error)]
pub(crate) enum PathCatalogError {
    #[error(transparent)]
    Run(#[from] RunError),
    #[error(transparent)]
    PathKey(#[from] PathKeyError),
    #[error(transparent)]
    Observation(#[from] PathObservationCodecError),
    #[error("path catalog input must be a path-observation run")]
    WrongInputKind,
    #[error("path catalog output must be a path-catalog run")]
    WrongOutputKind,
    #[error("path catalog runs must belong to the same generation")]
    GenerationMismatch,
    #[error("path catalog path is not a direct child of its retained parent")]
    MissingParent,
    #[error("path catalog identifier overflow")]
    IdOverflow,
    #[error("path catalog record is malformed")]
    Malformed,
}

/// Builds compact parent/name rows from one canonical path-observation stream.
///
/// IDs are assigned only while reading canonical path order, so worker timing
/// cannot influence them. The root is implicit as path ID zero.
pub(crate) fn build_path_catalog(
    input: &mut RunReader,
    output: &mut RunWriter,
) -> Result<(), PathCatalogError> {
    if input.descriptor().kind() != RunKind::PathObservation {
        return Err(PathCatalogError::WrongInputKind);
    }
    if output.descriptor().kind() != RunKind::PathCatalog {
        return Err(PathCatalogError::WrongOutputKind);
    }
    if input.descriptor().generation() != output.descriptor().generation() {
        return Err(PathCatalogError::GenerationMismatch);
    }

    let mut ancestors = vec![(RelativePath::root(), PathId(0))];
    let mut next_id = 1_u64;
    let mut key = Vec::new();
    let mut value = Vec::new();
    let mut output_value = Vec::new();
    while input.next_record_into(&mut key, &mut value)? {
        let observation = decode_path_observation(&key, &value)?;
        while !observation
            .path
            .starts_with(&ancestors.last().expect("root must remain").0)
        {
            if ancestors.len() == 1 {
                return Err(PathCatalogError::MissingParent);
            }
            ancestors.pop();
        }
        let (parent_path, parent_id) = ancestors.last().expect("root must remain");
        if !observation.path.is_direct_child_of(parent_path) {
            return Err(PathCatalogError::MissingParent);
        }
        let name = observation
            .path
            .components()
            .last()
            .ok_or(PathCatalogError::MissingParent)?;
        let id = PathId(next_id);
        next_id = next_id.checked_add(1).ok_or(PathCatalogError::IdOverflow)?;
        output_value.clear();
        output_value.push(PATH_CATALOG_VERSION);
        output_value.extend_from_slice(&id.value().to_le_bytes());
        output_value.extend_from_slice(&parent_id.value().to_le_bytes());
        append_path_component_key(name, &mut output_value)?;
        output.append(&key, &output_value)?;
        if observation.kind == PathEntryKind::Directory {
            ancestors.push((observation.path, id));
        }
    }
    Ok(())
}

/// Reads one compact catalog row through the catalog's sparse path index.
pub(crate) fn read_path_catalog_entry(
    run: &SealedRun,
    path: &RelativePath,
) -> Result<Option<PathCatalogEntry>, PathCatalogError> {
    if run.descriptor().kind() != RunKind::PathCatalog {
        return Err(PathCatalogError::WrongOutputKind);
    }
    let mut expected = Vec::new();
    encode_path_key_into(path, &mut expected)?;
    let mut reader = run.range_reader(&expected)?;
    let mut key = Vec::new();
    let mut value = Vec::new();
    while reader.next_record_into(&mut key, &mut value)? {
        if key < expected {
            continue;
        }
        if key != expected {
            return Ok(None);
        }
        return decode_path_catalog_entry(&value).map(Some);
    }
    Ok(None)
}

fn decode_path_catalog_entry(mut value: &[u8]) -> Result<PathCatalogEntry, PathCatalogError> {
    let version = take_byte(&mut value)?;
    if version != PATH_CATALOG_VERSION {
        return Err(PathCatalogError::Malformed);
    }
    let id = PathId(u64::from_le_bytes(take_array(&mut value)?));
    let parent_id = PathId(u64::from_le_bytes(take_array(&mut value)?));
    let name_path = decode_path_key(value)?;
    if name_path.depth() != 1 {
        return Err(PathCatalogError::Malformed);
    }
    let name = name_path
        .components()
        .first()
        .cloned()
        .ok_or(PathCatalogError::Malformed)?;
    Ok(PathCatalogEntry {
        id,
        parent_id,
        name,
    })
}

fn take_byte(value: &mut &[u8]) -> Result<u8, PathCatalogError> {
    let (&byte, remainder) = value.split_first().ok_or(PathCatalogError::Malformed)?;
    *value = remainder;
    Ok(byte)
}

fn take_array<const N: usize>(value: &mut &[u8]) -> Result<[u8; N], PathCatalogError> {
    if value.len() < N {
        return Err(PathCatalogError::Malformed);
    }
    let mut output = [0_u8; N];
    output.copy_from_slice(&value[..N]);
    *value = &value[N..];
    Ok(output)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::model::ByteBounds;
    use crate::scan_coordinator::ScanGeneration;
    use crate::scan_store::path_observation::append_path_observation;
    use crate::scan_store::path_reducer::{Coverage, PathObservation, SummaryMetrics};
    use crate::scan_store::run_file::RunDescriptor;
    use crate::temporary_storage::TemporaryStorage;

    fn observation(path: &str, kind: PathEntryKind) -> PathObservation {
        PathObservation::new(
            RelativePath::from_path(Path::new(path)).expect("fixture path should be relative"),
            kind,
            SummaryMetrics::leaf(1, ByteBounds::exact(0), ByteBounds::exact(0)),
            Coverage::Complete,
        )
    }

    #[test]
    fn catalog_assigns_deterministic_parent_name_rows() {
        let storage = TemporaryStorage::with_limit_bytes(16 * 1024);
        let generation = ScanGeneration::initial();
        let mut paths = RunWriter::new(
            tempfile::tempfile().expect("path run should open"),
            storage.reservation(0).expect("path reservation should fit"),
            RunDescriptor::new(generation, 1, RunKind::PathObservation),
            256,
        )
        .expect("path writer should initialize");
        let mut key = Vec::new();
        let mut value = Vec::new();
        for item in [
            observation("alpha", PathEntryKind::Directory),
            observation("alpha/file", PathEntryKind::File),
            observation("beta", PathEntryKind::File),
        ] {
            append_path_observation(&mut paths, &item, &mut key, &mut value)
                .expect("path observation should append");
        }
        let mut paths = paths
            .seal()
            .expect("path run should seal")
            .into_reader()
            .expect("path run should open");
        let mut catalog = RunWriter::new(
            tempfile::tempfile().expect("catalog run should open"),
            storage
                .reservation(0)
                .expect("catalog reservation should fit"),
            RunDescriptor::new(generation, 2, RunKind::PathCatalog),
            256,
        )
        .expect("catalog writer should initialize");
        build_path_catalog(&mut paths, &mut catalog).expect("catalog should build");
        let catalog = catalog.seal().expect("catalog should seal");
        let alpha_file = read_path_catalog_entry(
            &catalog,
            &RelativePath::from_path(Path::new("alpha/file"))
                .expect("fixture path should be relative"),
        )
        .expect("catalog query should succeed")
        .expect("catalog row should exist");
        assert_eq!(alpha_file.id, PathId(2));
        assert_eq!(alpha_file.parent_id, PathId(1));
        assert_eq!(alpha_file.name, "file");
    }
}
