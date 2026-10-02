use thiserror::Error;

use super::run_file::RunDescriptor;
use crate::scan_coordinator::ScanGeneration;
use crate::scan_session::{ScanGenerationState, ScanSessionId};

/// Metadata for one sealed run referenced by a manifest.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RunManifestEntry {
    pub(crate) descriptor: RunDescriptor,
    pub(crate) bytes: u64,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub(crate) enum ManifestError {
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
}

#[cfg(test)]
mod tests {
    use super::super::run_file::RunKind;
    use super::*;

    fn run(generation: ScanGeneration, run_id: u64, kind: RunKind) -> RunManifestEntry {
        RunManifestEntry {
            descriptor: RunDescriptor::new(generation, run_id, kind),
            bytes: run_id.saturating_mul(10),
        }
    }

    #[test]
    fn manifest_tracks_sealed_runs_to_a_published_terminal_state() {
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

        assert_eq!(manifest.state(), ScanGenerationState::Published);
        assert_eq!(manifest.runs().len(), 2);
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
    fn random_session_ids_are_not_reused() {
        let first = ScanSessionId::random().expect("first session ID should generate");
        let second = ScanSessionId::random().expect("second session ID should generate");
        assert_ne!(first, second);
    }
}
