use thiserror::Error;

use super::run_file::{RunError, RunReader, RunWriter};

#[derive(Debug, Error)]
pub(crate) enum RunMergeError {
    #[error(transparent)]
    Run(#[from] RunError),
    #[error("input and output runs must share one generation and record kind")]
    DescriptorMismatch,
    #[error("input runs contain a duplicate canonical key")]
    DuplicateKey,
}

struct BufferedRecord {
    key: Vec<u8>,
    value: Vec<u8>,
    ready: bool,
}

impl BufferedRecord {
    fn new() -> Self {
        Self {
            key: Vec::new(),
            value: Vec::new(),
            ready: false,
        }
    }
}

/// Merges sorted sealed runs into one sorted output run using one key/value
/// buffer per input. It does not materialize the input record set.
///
/// # Errors
///
/// Returns an error for descriptor mismatch, corrupt input, duplicate keys, or
/// output reservation/write failure.
pub(crate) fn merge_sorted_runs(
    inputs: &mut [RunReader],
    output: &mut RunWriter,
) -> Result<(), RunMergeError> {
    let descriptor = output.descriptor();
    if inputs.iter().any(|input| {
        let input_descriptor = input.descriptor();
        input_descriptor.generation() != descriptor.generation()
            || input_descriptor.kind() != descriptor.kind()
    }) {
        return Err(RunMergeError::DescriptorMismatch);
    }

    let mut records = (0..inputs.len())
        .map(|_| BufferedRecord::new())
        .collect::<Vec<_>>();
    for (input, record) in inputs.iter_mut().zip(&mut records) {
        record.ready = input.next_record_into(&mut record.key, &mut record.value)?;
    }

    let mut previous_key = Vec::new();
    let mut has_previous_key = false;
    while let Some(index) = next_record_index(&records) {
        let record = &mut records[index];
        if has_previous_key && record.key == previous_key {
            return Err(RunMergeError::DuplicateKey);
        }
        output.append(&record.key, &record.value)?;
        previous_key.clear();
        previous_key.extend_from_slice(&record.key);
        has_previous_key = true;
        record.ready = inputs[index].next_record_into(&mut record.key, &mut record.value)?;
    }
    Ok(())
}

fn next_record_index(records: &[BufferedRecord]) -> Option<usize> {
    records
        .iter()
        .enumerate()
        .filter(|(_, record)| record.ready)
        .min_by(|(_, left), (_, right)| left.key.cmp(&right.key))
        .map(|(index, _)| index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan_coordinator::ScanGeneration;
    use crate::scan_store::run_file::{RunDescriptor, RunKind, SealedRun};
    use crate::temporary_storage::TemporaryStorage;

    fn seal_run(
        storage: &TemporaryStorage,
        run_id: u64,
        kind: RunKind,
        records: &[(&[u8], &[u8])],
    ) -> SealedRun {
        let mut writer = RunWriter::new(
            tempfile::tempfile().expect("temporary input run should open"),
            storage
                .reservation(0)
                .expect("input reservation should fit"),
            RunDescriptor::new(ScanGeneration::initial(), run_id, kind),
            128,
        )
        .expect("input writer should initialize");
        for (key, value) in records {
            writer
                .append(key, value)
                .expect("input records should remain sorted");
        }
        writer.seal().expect("input run should seal")
    }

    fn output_writer(storage: &TemporaryStorage, run_id: u64, kind: RunKind) -> RunWriter {
        RunWriter::new(
            tempfile::tempfile().expect("temporary output run should open"),
            storage
                .reservation(0)
                .expect("output reservation should fit"),
            RunDescriptor::new(ScanGeneration::initial(), run_id, kind),
            128,
        )
        .expect("output writer should initialize")
    }

    #[test]
    fn merge_streams_interleaved_runs_into_one_canonical_run() {
        let storage = TemporaryStorage::with_limit_bytes(16 * 1024);
        let first = seal_run(
            &storage,
            1,
            RunKind::PathObservation,
            &[(b"alpha", b"one"), (b"charlie", b"three")],
        );
        let second = seal_run(
            &storage,
            2,
            RunKind::PathObservation,
            &[(b"bravo", b"two"), (b"delta", b"four")],
        );
        let mut inputs = [
            first.into_reader().expect("first input should open"),
            second.into_reader().expect("second input should open"),
        ];
        let mut output = output_writer(&storage, 3, RunKind::PathObservation);
        merge_sorted_runs(&mut inputs, &mut output).expect("runs should merge");
        let mut reader = output
            .seal()
            .expect("merged run should seal")
            .into_reader()
            .expect("merged run should open");
        let mut merged = Vec::new();
        reader
            .visit_records(|key, value| {
                merged.push((key.to_vec(), value.to_vec()));
                Ok(())
            })
            .expect("merged records should validate");
        assert_eq!(
            merged,
            vec![
                (b"alpha".to_vec(), b"one".to_vec()),
                (b"bravo".to_vec(), b"two".to_vec()),
                (b"charlie".to_vec(), b"three".to_vec()),
                (b"delta".to_vec(), b"four".to_vec()),
            ]
        );
    }

    #[test]
    fn merge_rejects_duplicate_keys_and_descriptor_mismatch() {
        let storage = TemporaryStorage::with_limit_bytes(16 * 1024);
        let first = seal_run(&storage, 1, RunKind::PathObservation, &[(b"alpha", b"one")]);
        let second = seal_run(&storage, 2, RunKind::PathObservation, &[(b"alpha", b"two")]);
        let mut duplicate_inputs = [
            first.into_reader().expect("first input should open"),
            second.into_reader().expect("second input should open"),
        ];
        let mut output = output_writer(&storage, 3, RunKind::PathObservation);
        assert!(matches!(
            merge_sorted_runs(&mut duplicate_inputs, &mut output),
            Err(RunMergeError::DuplicateKey)
        ));

        let mismatched = seal_run(
            &storage,
            4,
            RunKind::IdentityObservation,
            &[(b"beta", b"value")],
        );
        let mut mismatch_inputs = [mismatched
            .into_reader()
            .expect("mismatch input should open")];
        let mut mismatch_output = output_writer(&storage, 5, RunKind::PathObservation);
        assert!(matches!(
            merge_sorted_runs(&mut mismatch_inputs, &mut mismatch_output),
            Err(RunMergeError::DescriptorMismatch)
        ));
    }
}
