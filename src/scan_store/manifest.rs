use sha2::{Digest as _, Sha256};
use thiserror::Error;

use super::run_file::{RunDescriptor, RunKind};
use crate::scan_coordinator::ScanGeneration;
use crate::scan_session::{ScanGenerationState, ScanSessionId};

const MANIFEST_MAGIC: [u8; 4] = *b"EXSM";
const MANIFEST_VERSION: u16 = 3;
const MANIFEST_HEADER_BYTES: usize = 36;
const MANIFEST_ENTRY_BYTES: usize = 17;
const MANIFEST_DIGEST_BYTES: usize = 32;

/// Durable metadata for one sealed run referenced by a manifest.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RunManifestEntry {
    pub(crate) descriptor: RunDescriptor,
    pub(crate) bytes: u64,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub(crate) enum ManifestError {
    #[error("scan manifest has too many runs")]
    TooManyRuns,
    #[error("scan manifest run belongs to another generation")]
    RunGenerationMismatch,
    #[error("scan manifest already contains this run")]
    DuplicateRun,
    #[error("scan manifest cannot accept runs after publication")]
    ClosedForRuns,
    #[error("scan manifest state transition is invalid")]
    InvalidTransition,
    #[error("scan manifest does not contain an obsolete run")]
    MissingRun,
    #[error("scan manifest is malformed")]
    Malformed,
    #[error("scan manifest checksum does not match")]
    ChecksumMismatch,
}

/// Small, complete state record for the currently active scan generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ScanManifest {
    session: ScanSessionId,
    generation: ScanGeneration,
    state: ScanGenerationState,
    runs: Vec<RunManifestEntry>,
}

impl ScanManifest {
    #[must_use]
    pub(crate) const fn new(session: ScanSessionId, generation: ScanGeneration) -> Self {
        Self {
            session,
            generation,
            state: ScanGenerationState::Creating,
            runs: Vec::new(),
        }
    }

    #[must_use]
    pub(crate) const fn session(&self) -> ScanSessionId {
        self.session
    }

    #[must_use]
    pub(crate) const fn generation(&self) -> ScanGeneration {
        self.generation
    }

    #[must_use]
    pub(crate) const fn state(&self) -> ScanGenerationState {
        self.state
    }

    #[must_use]
    pub(crate) fn runs(&self) -> &[RunManifestEntry] {
        &self.runs
    }

    /// # Errors
    ///
    /// Returns an error if the run belongs to another generation, duplicates an
    /// existing descriptor, or follows a terminal manifest state.
    pub(crate) fn add_run(&mut self, run: RunManifestEntry) -> Result<(), ManifestError> {
        if !matches!(
            self.state,
            ScanGenerationState::Scanning | ScanGenerationState::Reducing
        ) {
            return Err(ManifestError::ClosedForRuns);
        }
        if run.descriptor.generation() != self.generation {
            return Err(ManifestError::RunGenerationMismatch);
        }
        if self.runs.iter().any(|existing| {
            existing.descriptor.run_id() == run.descriptor.run_id()
                && existing.descriptor.kind() == run.descriptor.kind()
        }) {
            return Err(ManifestError::DuplicateRun);
        }
        self.runs.push(run);
        Ok(())
    }

    /// Replaces sealed input runs with their durable successor.
    ///
    /// The caller persists the resulting manifest before dropping `removed`.
    pub(crate) fn replace_runs(
        &mut self,
        removed: &[RunDescriptor],
        added: RunManifestEntry,
    ) -> Result<(), ManifestError> {
        if !matches!(
            self.state,
            ScanGenerationState::Scanning | ScanGenerationState::Reducing
        ) {
            return Err(ManifestError::ClosedForRuns);
        }
        if added.descriptor.generation() != self.generation {
            return Err(ManifestError::RunGenerationMismatch);
        }
        if removed
            .iter()
            .any(|descriptor| descriptor.generation() != self.generation)
        {
            return Err(ManifestError::RunGenerationMismatch);
        }
        if removed.iter().any(|descriptor| {
            !self.runs.iter().any(|existing| {
                existing.descriptor.run_id() == descriptor.run_id()
                    && existing.descriptor.kind() == descriptor.kind()
            })
        }) {
            return Err(ManifestError::MissingRun);
        }
        self.runs.retain(|existing| {
            !removed.iter().any(|descriptor| {
                existing.descriptor.run_id() == descriptor.run_id()
                    && existing.descriptor.kind() == descriptor.kind()
            })
        });
        if self.runs.iter().any(|existing| {
            existing.descriptor.run_id() == added.descriptor.run_id()
                && existing.descriptor.kind() == added.descriptor.kind()
        }) {
            return Err(ManifestError::DuplicateRun);
        }
        self.runs.push(added);
        Ok(())
    }

    /// Replaces every retained run with the final published query run.
    pub(crate) fn replace_all_runs(
        &mut self,
        added: RunManifestEntry,
    ) -> Result<(), ManifestError> {
        let removed = self
            .runs
            .iter()
            .map(|entry| entry.descriptor)
            .collect::<Vec<_>>();
        self.replace_runs(&removed, added)
    }

    /// # Errors
    ///
    /// Returns an error when a caller attempts to skip or leave a terminal
    /// manifest state.
    pub(crate) fn transition(&mut self, next: ScanGenerationState) -> Result<(), ManifestError> {
        if !self.state.can_transition_to(next) {
            return Err(ManifestError::InvalidTransition);
        }
        self.state = next;
        Ok(())
    }

    /// Encodes a self-checking manifest snapshot for atomic caller publication.
    ///
    /// # Errors
    ///
    /// Returns an error when the run count cannot fit the wire format.
    pub(crate) fn encode(&self) -> Result<Vec<u8>, ManifestError> {
        let run_count = u32::try_from(self.runs.len()).map_err(|_| ManifestError::TooManyRuns)?;
        let entries_bytes = self
            .runs
            .len()
            .checked_mul(MANIFEST_ENTRY_BYTES)
            .ok_or(ManifestError::TooManyRuns)?;
        let capacity = MANIFEST_HEADER_BYTES
            .checked_add(entries_bytes)
            .and_then(|bytes| bytes.checked_add(MANIFEST_DIGEST_BYTES))
            .ok_or(ManifestError::TooManyRuns)?;
        let mut encoded = Vec::with_capacity(capacity);
        encoded.extend_from_slice(&MANIFEST_MAGIC);
        encoded.extend_from_slice(&MANIFEST_VERSION.to_le_bytes());
        encoded.push(self.state as u8);
        encoded.push(0);
        encoded.extend_from_slice(&self.session.bytes());
        encoded.extend_from_slice(&self.generation.value().to_le_bytes());
        encoded.extend_from_slice(&run_count.to_le_bytes());
        for run in &self.runs {
            encoded.extend_from_slice(&run.descriptor.run_id().to_le_bytes());
            encoded.push(run.descriptor.kind().code());
            encoded.extend_from_slice(&run.bytes.to_le_bytes());
        }
        debug_assert_eq!(
            encoded.len(),
            MANIFEST_HEADER_BYTES.saturating_add(entries_bytes)
        );
        encoded.extend_from_slice(&Sha256::digest(&encoded));
        Ok(encoded)
    }

    /// # Errors
    ///
    /// Returns an error for malformed or checksum-invalid manifest bytes.
    pub(crate) fn decode(encoded: &[u8]) -> Result<Self, ManifestError> {
        if encoded.len() < MANIFEST_HEADER_BYTES.saturating_add(MANIFEST_DIGEST_BYTES) {
            return Err(ManifestError::Malformed);
        }
        let digest_start = encoded.len().saturating_sub(MANIFEST_DIGEST_BYTES);
        let (body, expected_digest) = encoded.split_at(digest_start);
        if Sha256::digest(body).as_slice() != expected_digest {
            return Err(ManifestError::ChecksumMismatch);
        }
        if body[..4] != MANIFEST_MAGIC
            || u16::from_le_bytes([body[4], body[5]]) != MANIFEST_VERSION
            || body[7] != 0
        {
            return Err(ManifestError::Malformed);
        }
        let Some(state) = ScanGenerationState::from_code(body[6]) else {
            return Err(ManifestError::Malformed);
        };
        let mut session = [0_u8; 16];
        session.copy_from_slice(&body[8..24]);
        let generation = ScanGeneration::from_value(u64::from_le_bytes(
            body[24..32]
                .try_into()
                .map_err(|_| ManifestError::Malformed)?,
        ));
        let run_count = usize::try_from(u32::from_le_bytes(
            body[32..36]
                .try_into()
                .map_err(|_| ManifestError::Malformed)?,
        ))
        .map_err(|_| ManifestError::Malformed)?;
        let expected_body_bytes = MANIFEST_HEADER_BYTES
            .checked_add(
                run_count
                    .checked_mul(MANIFEST_ENTRY_BYTES)
                    .ok_or(ManifestError::Malformed)?,
            )
            .ok_or(ManifestError::Malformed)?;
        if body.len() != expected_body_bytes {
            return Err(ManifestError::Malformed);
        }
        let mut runs = Vec::with_capacity(run_count);
        let mut offset = MANIFEST_HEADER_BYTES;
        for _ in 0..run_count {
            let run_id = u64::from_le_bytes(
                body[offset..offset.saturating_add(8)]
                    .try_into()
                    .map_err(|_| ManifestError::Malformed)?,
            );
            let Some(kind) = RunKind::from_code(body[offset.saturating_add(8)]) else {
                return Err(ManifestError::Malformed);
            };
            let bytes = u64::from_le_bytes(
                body[offset.saturating_add(9)..offset.saturating_add(MANIFEST_ENTRY_BYTES)]
                    .try_into()
                    .map_err(|_| ManifestError::Malformed)?,
            );
            let descriptor = RunDescriptor::new(generation, run_id, kind);
            if runs.iter().any(|existing: &RunManifestEntry| {
                existing.descriptor.run_id() == run_id && existing.descriptor.kind() == kind
            }) {
                return Err(ManifestError::Malformed);
            }
            runs.push(RunManifestEntry { descriptor, bytes });
            offset = offset.saturating_add(MANIFEST_ENTRY_BYTES);
        }
        Ok(Self {
            session: ScanSessionId::from_bytes(session),
            generation,
            state,
            runs,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(generation: ScanGeneration, run_id: u64, kind: RunKind) -> RunManifestEntry {
        RunManifestEntry {
            descriptor: RunDescriptor::new(generation, run_id, kind),
            bytes: run_id.saturating_mul(10),
        }
    }

    #[test]
    fn manifest_round_trips_sealed_runs_and_terminal_state() {
        let generation = ScanGeneration::from_value(9);
        let mut manifest = ScanManifest::new(ScanSessionId::from_bytes([7; 16]), generation);
        assert_eq!(manifest.state(), ScanGenerationState::Creating);
        manifest
            .transition(ScanGenerationState::Scanning)
            .expect("generation should begin scanning");
        manifest
            .add_run(run(generation, 1, RunKind::PathObservation))
            .expect("path run should enter manifest");
        manifest
            .add_run(run(generation, 2, RunKind::IdentityObservation))
            .expect("identity run should enter manifest");
        manifest
            .transition(ScanGenerationState::Reducing)
            .expect("scan should begin reduction");
        manifest
            .transition(ScanGenerationState::Published)
            .expect("reduction should publish");

        let encoded = manifest.encode().expect("manifest should encode");

        assert_eq!(
            ScanManifest::decode(&encoded).expect("manifest should decode"),
            manifest
        );
    }
    #[test]
    fn manifest_replaces_consumed_runs_only_after_successor_is_known() {
        let generation = ScanGeneration::initial();
        let mut manifest = ScanManifest::new(ScanSessionId::from_bytes([3; 16]), generation);
        manifest
            .transition(ScanGenerationState::Scanning)
            .expect("generation should scan");
        let first = run(generation, 1, RunKind::PathObservation);
        let second = run(generation, 2, RunKind::PathObservation);
        manifest.add_run(first).expect("first run should enter");
        manifest.add_run(second).expect("second run should enter");
        let merged = run(generation, 3, RunKind::PathObservation);

        manifest
            .replace_runs(&[first.descriptor, second.descriptor], merged)
            .expect("merged successor should replace both inputs");

        assert_eq!(manifest.runs(), &[merged]);
        assert_eq!(
            manifest.replace_runs(
                &[first.descriptor],
                run(generation, 4, RunKind::PathObservation)
            ),
            Err(ManifestError::MissingRun)
        );
        assert_eq!(manifest.runs(), &[merged]);
    }

    #[test]
    fn manifest_rejects_invalid_run_and_state_transitions() {
        let generation = ScanGeneration::initial();
        let mut manifest = ScanManifest::new(ScanSessionId::from_bytes([1; 16]), generation);
        manifest
            .transition(ScanGenerationState::Scanning)
            .expect("generation should begin scanning");
        manifest
            .add_run(run(generation, 1, RunKind::PathObservation))
            .expect("run should enter manifest");
        assert_eq!(
            manifest.add_run(run(generation, 1, RunKind::PathObservation)),
            Err(ManifestError::DuplicateRun)
        );
        assert_eq!(
            manifest.add_run(run(
                ScanGeneration::from_value(1),
                2,
                RunKind::PathObservation,
            )),
            Err(ManifestError::RunGenerationMismatch)
        );
        assert_eq!(
            manifest.transition(ScanGenerationState::Published),
            Err(ManifestError::InvalidTransition)
        );
        manifest
            .transition(ScanGenerationState::Cancelled)
            .expect("scan should cancel");
        assert_eq!(
            manifest.add_run(run(generation, 3, RunKind::PathObservation)),
            Err(ManifestError::ClosedForRuns)
        );
    }

    #[test]
    fn manifest_rejects_corrupted_or_truncated_bytes() {
        let manifest = ScanManifest::new(
            ScanSessionId::from_bytes([4; 16]),
            ScanGeneration::initial(),
        );
        let mut encoded = manifest.encode().expect("manifest should encode");
        encoded[0] ^= 1;
        assert_eq!(
            ScanManifest::decode(&encoded),
            Err(ManifestError::ChecksumMismatch)
        );
        assert_eq!(
            ScanManifest::decode(&encoded[..3]),
            Err(ManifestError::Malformed)
        );
    }

    #[test]
    fn manifest_rejects_prior_wire_versions() {
        let manifest = ScanManifest::new(
            ScanSessionId::from_bytes([8; 16]),
            ScanGeneration::initial(),
        );
        for prior_version in [1_u16, 2] {
            let mut encoded = manifest.encode().expect("manifest should encode");
            encoded[4..6].copy_from_slice(&prior_version.to_le_bytes());
            let digest_start = encoded.len().saturating_sub(MANIFEST_DIGEST_BYTES);
            let digest = Sha256::digest(&encoded[..digest_start]);
            encoded[digest_start..].copy_from_slice(&digest);

            assert_eq!(
                ScanManifest::decode(&encoded),
                Err(ManifestError::Malformed)
            );
        }
    }

    #[test]
    fn random_session_ids_are_not_reused() {
        let first = ScanSessionId::random().expect("first session ID should generate");
        let second = ScanSessionId::random().expect("second session ID should generate");
        assert_ne!(first.bytes(), second.bytes());
    }
}
