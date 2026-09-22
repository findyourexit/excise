use thiserror::Error;

use super::path_key::{PathKeyError, decode_path_key, encode_path_key_into};
use super::path_reducer::{Coverage, PathEntryKind, PathObservation, SummaryMetrics};
use super::run_file::{RunError, RunKind, RunReader, RunWriter};
use crate::model::ByteBounds;

const PATH_OBSERVATION_VERSION: u8 = 1;
const ALLOCATED_UPPER_PRESENT: u8 = 1;
const RECLAIMABLE_UPPER_PRESENT: u8 = 1 << 1;
const KNOWN_FLAGS: u8 = ALLOCATED_UPPER_PRESENT | RECLAIMABLE_UPPER_PRESENT;
const FIXED_VALUE_BYTES: usize = 4 + 3 * size_of::<u128>() + size_of::<u64>();

#[derive(Debug, Error)]
pub(crate) enum PathObservationCodecError {
    #[error(transparent)]
    PathKey(#[from] PathKeyError),
    #[error("path observation value is malformed")]
    Malformed,
}

#[derive(Debug, Error)]
pub(crate) enum PathObservationRunError {
    #[error(transparent)]
    Codec(#[from] PathObservationCodecError),
    #[error(transparent)]
    Run(#[from] RunError),
    #[error("path observations require a path-observation run")]
    WrongRunKind,
}

/// Reuses `key` and `value` to encode one canonical path observation.
///
/// The key is a component-order-preserving native path representation; the
/// value stores only scanner facts, never another copy of the path.
///
/// # Errors
///
/// Returns an error when the path cannot be represented natively.
pub(crate) fn encode_path_observation_into(
    observation: &PathObservation,
    key: &mut Vec<u8>,
    value: &mut Vec<u8>,
) -> Result<(), PathObservationCodecError> {
    encode_path_key_into(&observation.path, key)?;
    value.clear();
    let optional_bytes = observation
        .metrics
        .allocated_bytes
        .upper
        .map_or(0, |_| size_of::<u128>())
        .saturating_add(
            observation
                .metrics
                .reclaimable_bytes
                .upper
                .map_or(0, |_| size_of::<u128>()),
        );
    value.reserve(
        FIXED_VALUE_BYTES
            .saturating_add(optional_bytes)
            .saturating_sub(value.capacity()),
    );
    let mut flags = 0_u8;
    if observation.metrics.allocated_bytes.upper.is_some() {
        flags |= ALLOCATED_UPPER_PRESENT;
    }
    if observation.metrics.reclaimable_bytes.upper.is_some() {
        flags |= RECLAIMABLE_UPPER_PRESENT;
    }
    value.push(PATH_OBSERVATION_VERSION);
    value.push(entry_kind_byte(observation.kind));
    value.push(coverage_byte(observation.coverage));
    value.push(flags);
    push_u128(value, observation.metrics.apparent_bytes);
    push_u128(value, observation.metrics.allocated_bytes.lower);
    push_u128(value, observation.metrics.reclaimable_bytes.lower);
    push_u64(value, observation.metrics.descendants);
    if let Some(upper) = observation.metrics.allocated_bytes.upper {
        push_u128(value, upper);
    }
    if let Some(upper) = observation.metrics.reclaimable_bytes.upper {
        push_u128(value, upper);
    }
    Ok(())
}

/// Decodes one path observation from the key/value pair emitted to a run.
///
/// # Errors
///
/// Returns an error when either the path key or value is malformed.
pub(crate) fn decode_path_observation(
    key: &[u8],
    mut value: &[u8],
) -> Result<PathObservation, PathObservationCodecError> {
    let version = take_u8(&mut value)?;
    if version != PATH_OBSERVATION_VERSION {
        return Err(PathObservationCodecError::Malformed);
    }
    let kind =
        entry_kind_from_byte(take_u8(&mut value)?).ok_or(PathObservationCodecError::Malformed)?;
    let coverage =
        coverage_from_byte(take_u8(&mut value)?).ok_or(PathObservationCodecError::Malformed)?;
    let flags = take_u8(&mut value)?;
    if flags & !KNOWN_FLAGS != 0 {
        return Err(PathObservationCodecError::Malformed);
    }
    let apparent_bytes = take_u128(&mut value)?;
    let allocated_lower = take_u128(&mut value)?;
    let reclaimable_lower = take_u128(&mut value)?;
    let descendants = take_u64(&mut value)?;
    let allocated_upper = (flags & ALLOCATED_UPPER_PRESENT != 0)
        .then(|| take_u128(&mut value))
        .transpose()?;
    let reclaimable_upper = (flags & RECLAIMABLE_UPPER_PRESENT != 0)
        .then(|| take_u128(&mut value))
        .transpose()?;
    if !value.is_empty() {
        return Err(PathObservationCodecError::Malformed);
    }
    Ok(PathObservation::new(
        decode_path_key(key)?,
        kind,
        SummaryMetrics {
            apparent_bytes,
            allocated_bytes: ByteBounds {
                lower: allocated_lower,
                upper: allocated_upper,
            },
            reclaimable_bytes: ByteBounds {
                lower: reclaimable_lower,
                upper: reclaimable_upper,
            },
            descendants,
        },
        coverage,
    ))
}

/// Encodes and appends an observation to a path-observation run without
/// allocating a fresh key/value buffer for each record.
///
/// # Errors
///
/// Returns an error when the writer kind is wrong or encoding/writing fails.
pub(crate) fn append_path_observation(
    writer: &mut RunWriter,
    observation: &PathObservation,
    key: &mut Vec<u8>,
    value: &mut Vec<u8>,
) -> Result<(), PathObservationRunError> {
    if writer.descriptor().kind() != RunKind::PathObservation {
        return Err(PathObservationRunError::WrongRunKind);
    }
    encode_path_observation_into(observation, key, value)?;
    writer.append(key, value)?;
    Ok(())
}

/// Visits decoded observations from one path-observation run.
///
/// # Errors
///
/// Returns an error when the run kind, frame, or encoded observation is invalid.
pub(crate) fn visit_path_observations(
    reader: &mut RunReader,
    mut visit: impl FnMut(PathObservation) -> Result<(), PathObservationRunError>,
) -> Result<(), PathObservationRunError> {
    if reader.descriptor().kind() != RunKind::PathObservation {
        return Err(PathObservationRunError::WrongRunKind);
    }
    reader.visit_records(|key, value| {
        let observation =
            decode_path_observation(key, value).map_err(|_| RunError::InvalidBlock)?;
        visit(observation).map_err(|_| RunError::InvalidBlock)
    })?;
    Ok(())
}

const fn entry_kind_byte(kind: PathEntryKind) -> u8 {
    match kind {
        PathEntryKind::Directory => 1,
        PathEntryKind::File => 2,
        PathEntryKind::Link => 3,
    }
}

const fn entry_kind_from_byte(value: u8) -> Option<PathEntryKind> {
    match value {
        1 => Some(PathEntryKind::Directory),
        2 => Some(PathEntryKind::File),
        3 => Some(PathEntryKind::Link),
        _ => None,
    }
}

const fn coverage_byte(coverage: Coverage) -> u8 {
    match coverage {
        Coverage::Complete => 1,
        Coverage::Uncertain => 2,
    }
}

const fn coverage_from_byte(value: u8) -> Option<Coverage> {
    match value {
        1 => Some(Coverage::Complete),
        2 => Some(Coverage::Uncertain),
        _ => None,
    }
}

fn push_u64(output: &mut Vec<u8>, value: u64) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn push_u128(output: &mut Vec<u8>, value: u128) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn take_u8(input: &mut &[u8]) -> Result<u8, PathObservationCodecError> {
    let (&value, remainder) = input
        .split_first()
        .ok_or(PathObservationCodecError::Malformed)?;
    *input = remainder;
    Ok(value)
}

fn take_u64(input: &mut &[u8]) -> Result<u64, PathObservationCodecError> {
    Ok(u64::from_le_bytes(take_array(input)?))
}

fn take_u128(input: &mut &[u8]) -> Result<u128, PathObservationCodecError> {
    Ok(u128::from_le_bytes(take_array(input)?))
}

fn take_array<const N: usize>(input: &mut &[u8]) -> Result<[u8; N], PathObservationCodecError> {
    if input.len() < N {
        return Err(PathObservationCodecError::Malformed);
    }
    let mut value = [0_u8; N];
    value.copy_from_slice(&input[..N]);
    *input = &input[N..];
    Ok(value)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::scan_coordinator::{RelativePath, ScanGeneration};
    use crate::scan_store::run_file::{RunDescriptor, RunWriter};
    use crate::temporary_storage::TemporaryStorage;

    fn observation(path: &str, kind: PathEntryKind, coverage: Coverage) -> PathObservation {
        PathObservation::new(
            RelativePath::from_path(Path::new(path)).expect("fixture path should be relative"),
            kind,
            SummaryMetrics {
                apparent_bytes: 7,
                allocated_bytes: ByteBounds {
                    lower: 3,
                    upper: Some(11),
                },
                reclaimable_bytes: ByteBounds::unknown(),
                descendants: 2,
            },
            coverage,
        )
    }

    fn writer(storage: &TemporaryStorage, kind: RunKind) -> RunWriter {
        RunWriter::new(
            tempfile::tempfile().expect("temporary run file should open"),
            storage
                .reservation(0)
                .expect("empty reservation should fit"),
            RunDescriptor::new(ScanGeneration::initial(), 1, kind),
            256,
        )
        .expect("run writer should initialize")
    }

    #[test]
    fn observation_codec_round_trips_complete_and_unknown_bounds() {
        for observation in [
            observation("alpha", PathEntryKind::Directory, Coverage::Complete),
            observation("alpha/child", PathEntryKind::Link, Coverage::Uncertain),
        ] {
            let mut key = Vec::new();
            let mut value = Vec::new();
            encode_path_observation_into(&observation, &mut key, &mut value)
                .expect("observation should encode");
            assert_eq!(
                decode_path_observation(&key, &value).expect("observation should decode"),
                observation
            );
        }
    }

    #[test]
    fn path_observation_run_reuses_buffers_and_preserves_order() {
        let storage = TemporaryStorage::with_limit_bytes(16 * 1024);
        let mut writer = writer(&storage, RunKind::PathObservation);
        let mut key = Vec::with_capacity(64);
        let mut value = Vec::with_capacity(128);
        append_path_observation(
            &mut writer,
            &observation("alpha", PathEntryKind::Directory, Coverage::Complete),
            &mut key,
            &mut value,
        )
        .expect("first observation should append");
        append_path_observation(
            &mut writer,
            &observation("alpha/child", PathEntryKind::File, Coverage::Uncertain),
            &mut key,
            &mut value,
        )
        .expect("second observation should append");
        let mut reader = writer
            .seal()
            .expect("path observation run should seal")
            .into_reader()
            .expect("path observation run should open");
        let mut decoded = Vec::new();
        visit_path_observations(&mut reader, |observation| {
            decoded.push(observation);
            Ok(())
        })
        .expect("path observation run should decode");

        assert_eq!(
            decoded,
            vec![
                observation("alpha", PathEntryKind::Directory, Coverage::Complete),
                observation("alpha/child", PathEntryKind::File, Coverage::Uncertain),
            ]
        );
    }

    #[test]
    fn path_observation_run_rejects_wrong_run_kind() {
        let storage = TemporaryStorage::with_limit_bytes(16 * 1024);
        let mut writer = writer(&storage, RunKind::DirectorySummary);
        let error = append_path_observation(
            &mut writer,
            &observation("alpha", PathEntryKind::File, Coverage::Complete),
            &mut Vec::new(),
            &mut Vec::new(),
        )
        .expect_err("wrong run kind must not accept a path observation");
        assert!(matches!(error, PathObservationRunError::WrongRunKind));
    }
}
