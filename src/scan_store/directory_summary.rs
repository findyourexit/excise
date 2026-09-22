use thiserror::Error;

use super::path_key::{PathKeyError, append_path_key, decode_path_key};
use super::path_observation::{PathObservationCodecError, decode_path_observation};
use super::path_reducer::{
    DirectorySummary, PathReductionError, coverage_code, coverage_from_code,
    reduce_sorted_path_stream,
};
use super::run_file::{RunError, RunKind, RunReader, RunWriter};
use super::summary_metrics::{
    SummaryMetricsCodecError, decode_summary_metrics, encode_summary_metrics_into,
    summary_metrics_flags,
};

const DIRECTORY_SUMMARY_VERSION: u8 = 1;
const ORDINAL_BYTES: usize = size_of::<u64>();
const PATH_LENGTH_BYTES: usize = size_of::<u32>();

#[derive(Debug, Error)]
pub(crate) enum DirectorySummaryCodecError {
    #[error(transparent)]
    PathKey(#[from] PathKeyError),
    #[error(transparent)]
    Metrics(#[from] SummaryMetricsCodecError),
    #[error("directory summary value is malformed")]
    Malformed,
}

#[derive(Debug, Error)]
pub(crate) enum DirectorySummaryRunError {
    #[error(transparent)]
    InputCodec(#[from] PathObservationCodecError),
    #[error(transparent)]
    OutputCodec(#[from] DirectorySummaryCodecError),
    #[error(transparent)]
    Reduction(#[from] PathReductionError),
    #[error(transparent)]
    Run(#[from] RunError),
    #[error("directory summary input must be a path-observation run")]
    WrongInputKind,
    #[error("directory summary output must be a directory-summary run")]
    WrongOutputKind,
    #[error("directory summary runs must belong to the same scan generation")]
    GenerationMismatch,
    #[error("directory summary ordinal overflow")]
    OrdinalOverflow,
}

/// Reuses record buffers to encode one post-order directory summary.
///
/// The key is a monotonic big-endian post-order ordinal. The value owns the
/// path so streaming reduction can preserve its natural post-order without a
/// second unbounded sort.
///
/// # Errors
///
/// Returns an error when the summary path cannot be canonically represented or
/// does not fit the wire format.
pub(crate) fn encode_directory_summary_into(
    ordinal: u64,
    summary: &DirectorySummary,
    key: &mut Vec<u8>,
    value: &mut Vec<u8>,
) -> Result<(), DirectorySummaryCodecError> {
    key.clear();
    key.extend_from_slice(&ordinal.to_be_bytes());
    value.clear();
    value.push(DIRECTORY_SUMMARY_VERSION);
    value.push(coverage_code(summary.coverage));
    value.push(summary_metrics_flags(summary.metrics));
    let path_length_offset = value.len();
    value.extend_from_slice(&[0_u8; PATH_LENGTH_BYTES]);
    let path_start = value.len();
    append_path_key(&summary.path, value)?;
    let path_length = u32::try_from(value.len().saturating_sub(path_start))
        .map_err(|_| DirectorySummaryCodecError::Malformed)?;
    value[path_length_offset..path_start].copy_from_slice(&path_length.to_le_bytes());
    encode_summary_metrics_into(summary.metrics, value);
    Ok(())
}

/// # Errors
///
/// Returns an error for malformed ordinal keys, paths, coverage, or metrics.
pub(crate) fn decode_directory_summary(
    key: &[u8],
    mut value: &[u8],
) -> Result<(u64, DirectorySummary), DirectorySummaryCodecError> {
    if key.len() != ORDINAL_BYTES {
        return Err(DirectorySummaryCodecError::Malformed);
    }
    let ordinal = u64::from_be_bytes(
        key.try_into()
            .map_err(|_| DirectorySummaryCodecError::Malformed)?,
    );
    if take_u8(&mut value)? != DIRECTORY_SUMMARY_VERSION {
        return Err(DirectorySummaryCodecError::Malformed);
    }
    let coverage =
        coverage_from_code(take_u8(&mut value)?).ok_or(DirectorySummaryCodecError::Malformed)?;
    let flags = take_u8(&mut value)?;
    let path_length = usize::try_from(take_u32(&mut value)?)
        .map_err(|_| DirectorySummaryCodecError::Malformed)?;
    let path_bytes = take_bytes(&mut value, path_length)?;
    let path = decode_path_key(path_bytes)?;
    let metrics = decode_summary_metrics(flags, &mut value)?;
    if !value.is_empty() {
        return Err(DirectorySummaryCodecError::Malformed);
    }
    Ok((
        ordinal,
        DirectorySummary {
            path,
            metrics,
            coverage,
        },
    ))
}

/// Appends one directory summary to a directory-summary run.
///
/// # Errors
///
/// Returns an error when the run kind is wrong or encoding/writing fails.
pub(crate) fn append_directory_summary(
    writer: &mut RunWriter,
    ordinal: u64,
    summary: &DirectorySummary,
    key: &mut Vec<u8>,
    value: &mut Vec<u8>,
) -> Result<(), DirectorySummaryRunError> {
    if writer.descriptor().kind() != RunKind::DirectorySummary {
        return Err(DirectorySummaryRunError::WrongOutputKind);
    }
    encode_directory_summary_into(ordinal, summary, key, value)?;
    writer.append(key, value)?;
    Ok(())
}

/// Visits one verified post-order directory-summary run.
///
/// # Errors
///
/// Returns an error when the run kind, framing, ordinal order, or summary value
/// is invalid.
pub(crate) fn visit_directory_summaries(
    reader: &mut RunReader,
    mut visit: impl FnMut(u64, DirectorySummary) -> Result<(), DirectorySummaryRunError>,
) -> Result<(), DirectorySummaryRunError> {
    if reader.descriptor().kind() != RunKind::DirectorySummary {
        return Err(DirectorySummaryRunError::WrongInputKind);
    }
    let mut expected_ordinal = 0_u64;
    let mut key = Vec::new();
    let mut value = Vec::new();
    while reader.next_record_into(&mut key, &mut value)? {
        let (ordinal, summary) = decode_directory_summary(&key, &value)?;
        if ordinal != expected_ordinal {
            return Err(DirectorySummaryRunError::OrdinalOverflow);
        }
        expected_ordinal = expected_ordinal
            .checked_add(1)
            .ok_or(DirectorySummaryRunError::OrdinalOverflow)?;
        visit(ordinal, summary)?;
    }
    Ok(())
}

/// Reduces one canonical path-observation run into one post-order summary run.
///
/// # Errors
///
/// Returns an error for mismatched run descriptors, malformed observations, a
/// noncanonical path stream, or output I/O.
pub(crate) fn reduce_path_observation_run(
    input: &mut RunReader,
    output: &mut RunWriter,
) -> Result<(), DirectorySummaryRunError> {
    if input.descriptor().kind() != RunKind::PathObservation {
        return Err(DirectorySummaryRunError::WrongInputKind);
    }
    if output.descriptor().kind() != RunKind::DirectorySummary {
        return Err(DirectorySummaryRunError::WrongOutputKind);
    }
    if input.descriptor().generation() != output.descriptor().generation() {
        return Err(DirectorySummaryRunError::GenerationMismatch);
    }

    let mut input_key = Vec::new();
    let mut input_value = Vec::new();
    let mut output_key = Vec::with_capacity(ORDINAL_BYTES);
    let mut output_value = Vec::new();
    let mut ordinal = 0_u64;
    reduce_sorted_path_stream(
        || {
            if input.next_record_into(&mut input_key, &mut input_value)? {
                Ok(Some(decode_path_observation(&input_key, &input_value)?))
            } else {
                Ok(None)
            }
        },
        |summary| {
            append_directory_summary(
                output,
                ordinal,
                &summary,
                &mut output_key,
                &mut output_value,
            )?;
            ordinal = ordinal
                .checked_add(1)
                .ok_or(DirectorySummaryRunError::OrdinalOverflow)?;
            Ok(())
        },
    )
}

fn take_u8(input: &mut &[u8]) -> Result<u8, DirectorySummaryCodecError> {
    let (&value, remainder) = input
        .split_first()
        .ok_or(DirectorySummaryCodecError::Malformed)?;
    *input = remainder;
    Ok(value)
}

fn take_u32(input: &mut &[u8]) -> Result<u32, DirectorySummaryCodecError> {
    Ok(u32::from_le_bytes(take_array(input)?))
}

fn take_bytes<'a>(
    input: &mut &'a [u8],
    count: usize,
) -> Result<&'a [u8], DirectorySummaryCodecError> {
    if input.len() < count {
        return Err(DirectorySummaryCodecError::Malformed);
    }
    let (taken, remainder) = input.split_at(count);
    *input = remainder;
    Ok(taken)
}

fn take_array<const N: usize>(input: &mut &[u8]) -> Result<[u8; N], DirectorySummaryCodecError> {
    let bytes = take_bytes(input, N)?;
    let mut value = [0_u8; N];
    value.copy_from_slice(bytes);
    Ok(value)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::model::ByteBounds;
    use crate::scan_coordinator::{RelativePath, ScanGeneration};
    use crate::scan_store::path_observation::append_path_observation;
    use crate::scan_store::path_reducer::Coverage;
    use crate::scan_store::path_reducer::{PathEntryKind, PathObservation, SummaryMetrics};
    use crate::scan_store::run_file::RunDescriptor;
    use crate::temporary_storage::TemporaryStorage;

    fn path(value: &str) -> RelativePath {
        RelativePath::from_path(Path::new(value)).expect("fixture path should be relative")
    }

    fn observation(path_text: &str, kind: PathEntryKind, apparent: u128) -> PathObservation {
        PathObservation::new(
            path(path_text),
            kind,
            SummaryMetrics::leaf(
                apparent,
                ByteBounds::exact(apparent),
                ByteBounds::exact(apparent),
            ),
            Coverage::Complete,
        )
    }

    fn writer(storage: &TemporaryStorage, run_id: u64, kind: RunKind) -> RunWriter {
        RunWriter::new(
            tempfile::tempfile().expect("temporary run file should open"),
            storage
                .reservation(0)
                .expect("empty reservation should fit"),
            RunDescriptor::new(ScanGeneration::initial(), run_id, kind),
            512,
        )
        .expect("run writer should initialize")
    }

    #[test]
    fn directory_summary_codec_round_trips_root_and_unknown_bounds() {
        let summary = DirectorySummary {
            path: RelativePath::root(),
            metrics: SummaryMetrics {
                apparent_bytes: 12,
                allocated_bytes: ByteBounds::exact(8),
                reclaimable_bytes: ByteBounds::unknown(),
                descendants: 4,
            },
            coverage: Coverage::Uncertain,
        };
        let mut key = Vec::new();
        let mut value = Vec::new();
        encode_directory_summary_into(7, &summary, &mut key, &mut value)
            .expect("summary should encode");
        assert_eq!(
            decode_directory_summary(&key, &value).expect("summary should decode"),
            (7, summary)
        );
    }

    #[test]
    fn streamed_reduction_emits_post_order_summaries() {
        let storage = TemporaryStorage::with_limit_bytes(32 * 1024);
        let mut input = writer(&storage, 1, RunKind::PathObservation);
        let mut key = Vec::new();
        let mut value = Vec::new();
        for observation in [
            observation("alpha", PathEntryKind::Directory, 0),
            observation("alpha/first", PathEntryKind::File, 3),
            observation("beta", PathEntryKind::Directory, 0),
            observation("beta/second", PathEntryKind::File, 5),
        ] {
            append_path_observation(&mut input, &observation, &mut key, &mut value)
                .expect("observation should append");
        }
        let mut input = input
            .seal()
            .expect("input should seal")
            .into_reader()
            .expect("input should open");
        let mut output = writer(&storage, 2, RunKind::DirectorySummary);
        reduce_path_observation_run(&mut input, &mut output).expect("reduction should succeed");
        let mut output = output
            .seal()
            .expect("summary output should seal")
            .into_reader()
            .expect("summary output should open");
        let mut summaries = Vec::new();
        visit_directory_summaries(&mut output, |ordinal, summary| {
            summaries.push((ordinal, summary));
            Ok(())
        })
        .expect("summary output should validate");

        assert_eq!(
            summaries
                .iter()
                .map(|(_, summary)| summary.path.clone())
                .collect::<Vec<_>>(),
            vec![path("alpha"), path("beta"), RelativePath::root()]
        );
        assert_eq!(summaries[0].1.metrics.apparent_bytes, 3);
        assert_eq!(summaries[1].1.metrics.apparent_bytes, 5);
        assert_eq!(summaries[2].1.metrics.apparent_bytes, 8);
    }

    #[test]
    fn reduction_rejects_wrong_run_kind_and_mismatched_generation() {
        let storage = TemporaryStorage::with_limit_bytes(16 * 1024);
        let mut input = writer(&storage, 1, RunKind::DirectorySummary)
            .seal()
            .expect("input should seal")
            .into_reader()
            .expect("input should open");
        let mut output = writer(&storage, 2, RunKind::DirectorySummary);
        assert!(matches!(
            reduce_path_observation_run(&mut input, &mut output),
            Err(DirectorySummaryRunError::WrongInputKind)
        ));

        let input_file = tempfile::tempfile().expect("temporary input should open");
        let mut path_writer = RunWriter::new(
            input_file,
            storage.reservation(0).expect("reservation should fit"),
            RunDescriptor::new(ScanGeneration::from_value(1), 3, RunKind::PathObservation),
            512,
        )
        .expect("path writer should initialize");
        append_path_observation(
            &mut path_writer,
            &observation("entry", PathEntryKind::File, 1),
            &mut Vec::new(),
            &mut Vec::new(),
        )
        .expect("path observation should append");
        let mut different_generation_input = path_writer
            .seal()
            .expect("input should seal")
            .into_reader()
            .expect("input should open");
        assert!(matches!(
            reduce_path_observation_run(&mut different_generation_input, &mut output),
            Err(DirectorySummaryRunError::GenerationMismatch)
        ));
    }
}
