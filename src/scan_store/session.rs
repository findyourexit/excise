use std::io;

use thiserror::Error;

use super::directory_summary::{DirectorySummaryRunError, reduce_path_observation_run};
use super::identity_observation::{
    AllocationContributionRunError, IdentityObservation, IdentityObservationRunError,
    IdentityReductionError, append_identity_observation, compare_identity_observations,
    decode_identity_observation, reduce_identity_observations,
};
use super::manifest::{
    ManifestError, ManifestState, RunManifestEntry, ScanManifest, ScanSessionId,
};
use super::path_observation::{
    PathObservationRunError, append_path_observation, decode_path_observation,
};
use super::path_reducer::PathObservation;
use super::run_file::{RunDescriptor, RunError, RunKind, RunReader, RunWriter, SealedRun};
use super::run_merge::{RunMergeError, merge_sorted_runs};
use crate::scan_coordinator::ScanGeneration;
use crate::temporary_storage::TemporaryStorage;

/// Bound the number of open readers and manifest candidates needed to compact
/// live scanner batches. Each family is folded immediately at this limit.
const MAX_ACTIVE_INPUT_RUNS: usize = 8;
const RUN_BLOCK_BYTES: usize = 64 * 1024;

/// Scanner workers emit bounded batches. Keep this input cap below the event
/// channel's worst-case payload so the owner never needs an unbounded sort.
pub(crate) const MAX_OBSERVATIONS_PER_BATCH: usize = 128;

#[derive(Debug, Error)]
pub(crate) enum ScanStoreError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Run(#[from] RunError),
    #[error(transparent)]
    Merge(#[from] RunMergeError),
    #[error(transparent)]
    PathReduction(#[from] DirectorySummaryRunError),
    #[error(transparent)]
    IdentityReduction(#[from] IdentityReductionError),
    #[error(transparent)]
    AllocationContribution(#[from] AllocationContributionRunError),
    #[error(transparent)]
    PathObservation(#[from] PathObservationRunError),
    #[error(transparent)]
    IdentityObservation(#[from] IdentityObservationRunError),
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    #[error("scan store has no active generation")]
    NoActiveGeneration,
    #[error("scan store has no published generation to use as an overlay base")]
    NoPublishedGeneration,
    #[error("scan store generation is not accepting scanner input")]
    ClosedGeneration,
    #[error("scan store accepted a run for a different generation")]
    GenerationMismatch,
    #[error("scan store accepts only raw path or identity observation runs")]
    InvalidInputRunKind,
    #[error("scan store generation must advance monotonically")]
    NonMonotonicGeneration,
    #[error("scan store exhausted run identifiers")]
    RunIdOverflow,
    #[error("scan store observation batch exceeds {MAX_OBSERVATIONS_PER_BATCH} entries")]
    BatchTooLarge,
}

/// A coherent, immutable scan generation retained after successful reduction.
///
/// The opaque run files remain owned by this value so their temporary-storage
/// reservations cannot be released while a consumer still needs this snapshot.
pub(crate) struct PublishedGeneration {
    manifest: ScanManifest,
    path_observations: SealedRun,
    identity_observations: SealedRun,
    allocation_contributions: SealedRun,
    directory_summaries: SealedRun,
}

impl PublishedGeneration {
    #[must_use]
    pub(crate) const fn manifest(&self) -> &ScanManifest {
        &self.manifest
    }

    #[must_use]
    pub(crate) const fn generation(&self) -> ScanGeneration {
        self.manifest.generation()
    }

    /// Runs `visit` against the retained canonical path run, then returns it to
    /// the published snapshot even when the visitor stops early.
    ///
    /// # Errors
    ///
    /// Returns a run validation error or the visitor's error.
    pub(crate) fn with_path_observations<T, E>(
        &mut self,
        visit: impl FnOnce(&mut RunReader) -> Result<T, E>,
    ) -> Result<T, E>
    where
        E: From<RunError>,
    {
        with_run_reader(&mut self.path_observations, visit)
    }

    /// Runs `visit` against the retained identity-ordered observations.
    ///
    /// # Errors
    ///
    /// Returns a run validation error or the visitor's error.
    pub(crate) fn with_identity_observations<T, E>(
        &mut self,
        visit: impl FnOnce(&mut RunReader) -> Result<T, E>,
    ) -> Result<T, E>
    where
        E: From<RunError>,
    {
        with_run_reader(&mut self.identity_observations, visit)
    }

    /// Runs `visit` against the once-per-identity allocation contributions.
    ///
    /// # Errors
    ///
    /// Returns a run validation error or the visitor's error.
    pub(crate) fn with_allocation_contributions<T, E>(
        &mut self,
        visit: impl FnOnce(&mut RunReader) -> Result<T, E>,
    ) -> Result<T, E>
    where
        E: From<RunError>,
    {
        with_run_reader(&mut self.allocation_contributions, visit)
    }

    /// Runs `visit` against post-order directory summaries.
    ///
    /// # Errors
    ///
    /// Returns a run validation error or the visitor's error.
    pub(crate) fn with_directory_summaries<T, E>(
        &mut self,
        visit: impl FnOnce(&mut RunReader) -> Result<T, E>,
    ) -> Result<T, E>
    where
        E: From<RunError>,
    {
        with_run_reader(&mut self.directory_summaries, visit)
    }
}

/// Bounded run staging for the one scan generation currently being assembled.
struct ActiveGeneration {
    manifest: ScanManifest,
    path_runs: Vec<SealedRun>,
    identity_runs: Vec<SealedRun>,
}

impl ActiveGeneration {
    fn new(session: ScanSessionId, generation: ScanGeneration) -> Self {
        Self {
            manifest: ScanManifest::new(session, generation),
            path_runs: Vec::with_capacity(MAX_ACTIVE_INPUT_RUNS),
            identity_runs: Vec::with_capacity(MAX_ACTIVE_INPUT_RUNS),
        }
    }
}

/// Session-local external scan pipeline.
///
/// The owner receives small sorted scanner runs, folds each run family before
/// the family grows beyond a fixed fan-in, then publishes exactly one immutable
/// generation. At most one active and one published generation are retained.
pub(crate) struct ScanStore {
    temporary_storage: TemporaryStorage,
    session: ScanSessionId,
    next_run_id: u64,
    active: Option<ActiveGeneration>,
    published: Option<PublishedGeneration>,
}

impl ScanStore {
    /// # Errors
    ///
    /// Returns an error if the operating system cannot create a session ID.
    pub(crate) fn new(
        generation: ScanGeneration,
        temporary_storage: TemporaryStorage,
    ) -> Result<Self, ScanStoreError> {
        let session = ScanSessionId::random()?;
        Ok(Self {
            temporary_storage,
            session,
            next_run_id: 0,
            active: Some(ActiveGeneration::new(session, generation)),
            published: None,
        })
    }

    #[must_use]
    pub(crate) fn active_generation(&self) -> Option<ScanGeneration> {
        self.active
            .as_ref()
            .map(|active| active.manifest.generation())
    }

    #[must_use]
    pub(crate) fn published_generation(&self) -> Option<ScanGeneration> {
        self.published.as_ref().map(PublishedGeneration::generation)
    }

    #[must_use]
    pub(crate) fn published(&self) -> Option<&PublishedGeneration> {
        self.published.as_ref()
    }

    #[must_use]
    pub(crate) fn published_mut(&mut self) -> Option<&mut PublishedGeneration> {
        self.published.as_mut()
    }

    /// Abandons unsealed work and begins a strictly newer generation. The last
    /// published snapshot remains queryable until its replacement publishes.
    ///
    /// # Errors
    ///
    /// Returns an error when `generation` does not advance the active/published
    /// generation or no identifier can be allocated.
    pub(crate) fn begin_generation(
        &mut self,
        generation: ScanGeneration,
    ) -> Result<(), ScanStoreError> {
        let latest = self
            .active_generation()
            .or_else(|| self.published_generation());
        if latest.is_some_and(|latest| generation <= latest) {
            return Err(ScanStoreError::NonMonotonicGeneration);
        }
        self.active = Some(ActiveGeneration::new(self.session, generation));
        Ok(())
    }

    /// Drops the active unpublishable generation while retaining the last
    /// immutable published snapshot.
    pub(crate) fn discard_active(&mut self) {
        self.active = None;
    }

    /// Starts a newer generation by retaining every prior raw fact outside
    /// `replaced_prefix`. A focused scanner may then write the authoritative
    /// replacement subtree before publication.
    ///
    /// # Errors
    ///
    /// Returns an error when no published base exists, generation ordering is
    /// invalid, a retained run is corrupt, or the copy cannot fit the shared
    /// temporary-storage budget. Copy failure leaves the new generation marked
    /// incomplete and preserves the prior published snapshot.
    pub(crate) fn begin_overlay_generation(
        &mut self,
        generation: ScanGeneration,
        replaced_prefix: &crate::scan_coordinator::RelativePath,
    ) -> Result<(), ScanStoreError> {
        if self.published.is_none() {
            return Err(ScanStoreError::NoPublishedGeneration);
        }
        self.begin_generation(generation)?;
        if replaced_prefix.is_root() {
            return Ok(());
        }

        let mut path_writer = match self.begin_input_run(RunKind::PathObservation) {
            Ok(writer) => writer,
            Err(error) => {
                self.mark_active_incomplete();
                return Err(error);
            }
        };
        let copied_paths = self
            .published
            .as_mut()
            .ok_or(ScanStoreError::NoPublishedGeneration)?
            .with_path_observations(|reader| -> Result<(), ScanStoreError> {
                let mut key = Vec::new();
                let mut value = Vec::new();
                while reader.next_record_into(&mut key, &mut value)? {
                    let observation = decode_path_observation(&key, &value)
                        .map_err(PathObservationRunError::from)?;
                    if !observation.path.starts_with(replaced_prefix) {
                        append_path_observation(
                            &mut path_writer,
                            &observation,
                            &mut key,
                            &mut value,
                        )?;
                    }
                }
                Ok(())
            });
        if let Err(error) = copied_paths {
            self.mark_active_incomplete();
            return Err(error);
        }
        let path_run = match path_writer.seal() {
            Ok(run) => run,
            Err(error) => {
                self.mark_active_incomplete();
                return Err(error.into());
            }
        };
        if let Err(error) = self.accept_input_run(path_run) {
            self.mark_active_incomplete();
            return Err(error);
        }

        let mut identity_writer = match self.begin_input_run(RunKind::IdentityObservation) {
            Ok(writer) => writer,
            Err(error) => {
                self.mark_active_incomplete();
                return Err(error);
            }
        };
        let copied_identities = self
            .published
            .as_mut()
            .ok_or(ScanStoreError::NoPublishedGeneration)?
            .with_identity_observations(|reader| -> Result<(), ScanStoreError> {
                let mut key = Vec::new();
                let mut value = Vec::new();
                while reader.next_record_into(&mut key, &mut value)? {
                    let observation = decode_identity_observation(&key, &value)
                        .map_err(IdentityObservationRunError::from)?;
                    if !observation.path.starts_with(replaced_prefix) {
                        append_identity_observation(
                            &mut identity_writer,
                            &observation,
                            &mut key,
                            &mut value,
                        )?;
                    }
                }
                Ok(())
            });
        if let Err(error) = copied_identities {
            self.mark_active_incomplete();
            return Err(error);
        }
        let identity_run = match identity_writer.seal() {
            Ok(run) => run,
            Err(error) => {
                self.mark_active_incomplete();
                return Err(error.into());
            }
        };
        if let Err(error) = self.accept_input_run(identity_run) {
            self.mark_active_incomplete();
            return Err(error);
        }
        Ok(())
    }

    /// Creates one empty scanner-input run for the active generation.
    ///
    /// The caller writes a locally sorted bounded batch, seals it, then returns
    /// it through [`Self::accept_input_run`].
    ///
    /// # Errors
    ///
    /// Returns an error if no generation is accepting input, temporary storage
    /// cannot reserve the run, or run identifiers are exhausted.
    pub(crate) fn begin_input_run(&mut self, kind: RunKind) -> Result<RunWriter, ScanStoreError> {
        if !is_input_kind(kind) {
            return Err(ScanStoreError::InvalidInputRunKind);
        }
        let generation = self.accepting_generation()?;
        self.new_writer(generation, kind)
    }

    /// Adds one sealed raw scanner run to the active generation.
    ///
    /// Once a family reaches the fixed fan-in, it is externally merged back to
    /// one run before this method returns.
    ///
    /// # Errors
    ///
    /// Returns an error for stale/invalid input, malformed runs, or temporary
    /// storage failure. A compaction failure transitions the active generation
    /// to an honest incomplete terminal state.
    pub(crate) fn accept_input_run(&mut self, run: SealedRun) -> Result<(), ScanStoreError> {
        let descriptor = run.descriptor();
        if !is_input_kind(descriptor.kind()) {
            return Err(ScanStoreError::InvalidInputRunKind);
        }
        let generation = self.accepting_generation()?;
        if descriptor.generation() != generation {
            return Err(ScanStoreError::GenerationMismatch);
        }
        let should_compact = {
            let active = self
                .active
                .as_mut()
                .ok_or(ScanStoreError::NoActiveGeneration)?;
            let runs = match descriptor.kind() {
                RunKind::PathObservation => &mut active.path_runs,
                RunKind::IdentityObservation => &mut active.identity_runs,
                RunKind::AllocationContribution
                | RunKind::DirectorySummary
                | RunKind::ChildQuery => {
                    return Err(ScanStoreError::InvalidInputRunKind);
                }
            };
            runs.push(run);
            runs.len() >= MAX_ACTIVE_INPUT_RUNS
        };
        if should_compact && let Err(error) = self.compact_input_family(descriptor.kind()) {
            self.mark_active_incomplete();
            return Err(error);
        }
        Ok(())
    }

    /// Sorts and persists one bounded scanner batch in both raw fact families.
    ///
    /// Physical bytes belong only to the identity observations. Path metrics
    /// retain hierarchy, apparent size, and coverage; publishing derives the
    /// once-per-identity physical totals from the companion run.
    ///
    /// # Errors
    ///
    /// Returns an error for an oversized batch, stale generation, duplicate
    /// canonical key, or temporary-storage failure. Any partial batch failure
    /// marks the active generation incomplete instead of permitting a false
    /// exact result.
    pub(crate) fn append_observation_batch(
        &mut self,
        mut paths: Vec<PathObservation>,
        mut identities: Vec<IdentityObservation>,
    ) -> Result<(), ScanStoreError> {
        if paths.len() > MAX_OBSERVATIONS_PER_BATCH || identities.len() > MAX_OBSERVATIONS_PER_BATCH
        {
            return Err(ScanStoreError::BatchTooLarge);
        }
        let result = (|| {
            paths.sort_unstable_by(|left, right| left.path.cmp(&right.path));
            identities.sort_unstable_by(compare_identity_observations);
            if !paths.is_empty() {
                let mut writer = self.begin_input_run(RunKind::PathObservation)?;
                let mut key = Vec::new();
                let mut value = Vec::new();
                for observation in &paths {
                    append_path_observation(&mut writer, observation, &mut key, &mut value)?;
                }
                self.accept_input_run(writer.seal()?)?;
            }
            if !identities.is_empty() {
                let mut writer = self.begin_input_run(RunKind::IdentityObservation)?;
                let mut key = Vec::new();
                let mut value = Vec::new();
                for observation in &identities {
                    append_identity_observation(&mut writer, observation, &mut key, &mut value)?;
                }
                self.accept_input_run(writer.seal()?)?;
            }
            Ok(())
        })();
        if result.is_err() {
            self.mark_active_incomplete();
        }
        result
    }

    /// Seals all raw runs, reduces them, and atomically installs the resulting
    /// immutable generation as the newest published snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error for an incomplete/stale active generation, malformed
    /// records, merge failure, or temporary-storage exhaustion. Failure leaves
    /// the active generation marked incomplete rather than publishing a mix of
    /// partial output and prior data.
    pub(crate) fn publish(&mut self) -> Result<ScanGeneration, ScanStoreError> {
        let mut active = self
            .active
            .take()
            .ok_or(ScanStoreError::NoActiveGeneration)?;
        if active.manifest.state() != ManifestState::Scanning {
            self.active = Some(active);
            return Err(ScanStoreError::ClosedGeneration);
        }
        active.manifest.transition(ManifestState::Reducing)?;
        let generation = active.manifest.generation();
        let result = self.reduce_active_generation(&mut active);
        match result {
            Ok((
                path_observations,
                identity_observations,
                allocation_contributions,
                directory_summaries,
            )) => {
                let mut manifest = active.manifest;
                for run in [
                    &path_observations,
                    &identity_observations,
                    &allocation_contributions,
                    &directory_summaries,
                ] {
                    manifest.add_run(RunManifestEntry {
                        descriptor: run.descriptor(),
                        bytes: run.bytes(),
                    })?;
                }
                manifest.transition(ManifestState::Published)?;
                self.published = Some(PublishedGeneration {
                    manifest,
                    path_observations,
                    identity_observations,
                    allocation_contributions,
                    directory_summaries,
                });
                Ok(generation)
            }
            Err(error) => {
                let _ = active.manifest.transition(ManifestState::Incomplete);
                self.active = Some(active);
                Err(error)
            }
        }
    }

    fn reduce_active_generation(
        &mut self,
        active: &mut ActiveGeneration,
    ) -> Result<(SealedRun, SealedRun, SealedRun, SealedRun), ScanStoreError> {
        let generation = active.manifest.generation();
        let path_observations = self.merge_family(
            generation,
            RunKind::PathObservation,
            std::mem::take(&mut active.path_runs),
        )?;
        let identity_observations = self.merge_family(
            generation,
            RunKind::IdentityObservation,
            std::mem::take(&mut active.identity_runs),
        )?;

        let mut path_reader = path_observations.into_reader()?;
        let mut directory_writer = self.new_writer(generation, RunKind::DirectorySummary)?;
        reduce_path_observation_run(&mut path_reader, &mut directory_writer)?;
        let path_observations = path_reader.into_sealed();
        let directory_summaries = directory_writer.seal()?;

        let mut identity_reader = identity_observations.into_reader()?;
        let mut allocation_writer = self.new_writer(generation, RunKind::AllocationContribution)?;
        reduce_identity_observations(&mut identity_reader, &mut allocation_writer)?;
        let identity_observations = identity_reader.into_sealed();
        let allocation_contributions = allocation_writer.seal()?;

        Ok((
            path_observations,
            identity_observations,
            allocation_contributions,
            directory_summaries,
        ))
    }

    fn compact_input_family(&mut self, kind: RunKind) -> Result<(), ScanStoreError> {
        let generation = self.accepting_generation()?;
        let runs = {
            let active = self
                .active
                .as_mut()
                .ok_or(ScanStoreError::NoActiveGeneration)?;
            match kind {
                RunKind::PathObservation => std::mem::take(&mut active.path_runs),
                RunKind::IdentityObservation => std::mem::take(&mut active.identity_runs),
                RunKind::AllocationContribution
                | RunKind::DirectorySummary
                | RunKind::ChildQuery => {
                    return Err(ScanStoreError::InvalidInputRunKind);
                }
            }
        };
        let merged = self.merge_family(generation, kind, runs)?;
        let active = self
            .active
            .as_mut()
            .ok_or(ScanStoreError::NoActiveGeneration)?;
        match kind {
            RunKind::PathObservation => active.path_runs.push(merged),
            RunKind::IdentityObservation => active.identity_runs.push(merged),
            RunKind::AllocationContribution | RunKind::DirectorySummary | RunKind::ChildQuery => {
                return Err(ScanStoreError::InvalidInputRunKind);
            }
        }
        Ok(())
    }

    fn merge_family(
        &mut self,
        generation: ScanGeneration,
        kind: RunKind,
        runs: Vec<SealedRun>,
    ) -> Result<SealedRun, ScanStoreError> {
        match runs.len() {
            0 => self
                .new_writer(generation, kind)?
                .seal()
                .map_err(Into::into),
            1 => Ok(runs.into_iter().next().expect("one run was checked above")),
            _ => {
                let mut readers = Vec::with_capacity(runs.len());
                for run in runs {
                    readers.push(run.into_reader()?);
                }
                let mut output = self.new_writer(generation, kind)?;
                merge_sorted_runs(&mut readers, &mut output)?;
                output.seal().map_err(Into::into)
            }
        }
    }

    fn accepting_generation(&self) -> Result<ScanGeneration, ScanStoreError> {
        let active = self
            .active
            .as_ref()
            .ok_or(ScanStoreError::NoActiveGeneration)?;
        if active.manifest.state() != ManifestState::Scanning {
            return Err(ScanStoreError::ClosedGeneration);
        }
        Ok(active.manifest.generation())
    }

    fn new_writer(
        &mut self,
        generation: ScanGeneration,
        kind: RunKind,
    ) -> Result<RunWriter, ScanStoreError> {
        let run_id = self.next_run_id;
        self.next_run_id = self
            .next_run_id
            .checked_add(1)
            .ok_or(ScanStoreError::RunIdOverflow)?;
        let file = tempfile::tempfile()?;
        let reservation = self.temporary_storage.reservation(0)?;
        Ok(RunWriter::new(
            file,
            reservation,
            RunDescriptor::new(generation, run_id, kind),
            RUN_BLOCK_BYTES,
        )?)
    }

    fn mark_active_incomplete(&mut self) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        if matches!(
            active.manifest.state(),
            ManifestState::Scanning | ManifestState::Reducing
        ) {
            let _ = active.manifest.transition(ManifestState::Incomplete);
        }
    }

    #[cfg(test)]
    fn active_run_counts(&self) -> Option<(usize, usize)> {
        self.active
            .as_ref()
            .map(|active| (active.path_runs.len(), active.identity_runs.len()))
    }
}

fn is_input_kind(kind: RunKind) -> bool {
    matches!(
        kind,
        RunKind::PathObservation | RunKind::IdentityObservation
    )
}

fn with_run_reader<T, E>(
    run: &mut SealedRun,
    visit: impl FnOnce(&mut RunReader) -> Result<T, E>,
) -> Result<T, E>
where
    E: From<RunError>,
{
    run.with_reader(visit)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::model::ByteBounds;
    use crate::scan_coordinator::RelativePath;
    use crate::scan_store::directory_summary::visit_directory_summaries;
    use crate::scan_store::identity_observation::{
        AllocationPlacement, IdentityObservation, append_identity_observation,
        visit_allocation_contributions,
    };
    use crate::scan_store::page::PageRequest;
    use crate::scan_store::path_observation::append_path_observation;
    use crate::scan_store::path_reducer::{
        Coverage, PathEntryKind, PathObservation, SummaryMetrics,
    };

    fn path(value: &str) -> RelativePath {
        RelativePath::from_path(Path::new(value)).expect("fixture path should be relative")
    }

    fn path_observation(
        path_text: &str,
        kind: PathEntryKind,
        apparent_bytes: u128,
    ) -> PathObservation {
        PathObservation::new(
            path(path_text),
            kind,
            SummaryMetrics::leaf(apparent_bytes, ByteBounds::exact(0), ByteBounds::exact(0)),
            Coverage::Complete,
        )
    }

    fn identity_observation(path_text: &str, file_id: file_id::FileId) -> IdentityObservation {
        IdentityObservation {
            path: path(path_text),
            file_id,
            declared_links: Some(1),
            allocated_bytes: ByteBounds::exact(8),
        }
    }

    fn store(storage: TemporaryStorage) -> ScanStore {
        ScanStore::new(ScanGeneration::initial(), storage).expect("store should initialize")
    }

    fn add_path_run(store: &mut ScanStore, observations: &[PathObservation]) {
        let mut writer = store
            .begin_input_run(RunKind::PathObservation)
            .expect("path run should begin");
        let mut key = Vec::new();
        let mut value = Vec::new();
        for observation in observations {
            append_path_observation(&mut writer, observation, &mut key, &mut value)
                .expect("path observations should remain sorted");
        }
        store
            .accept_input_run(writer.seal().expect("path run should seal"))
            .expect("path run should be accepted");
    }

    fn add_identity_run(store: &mut ScanStore, observations: &[IdentityObservation]) {
        let mut writer = store
            .begin_input_run(RunKind::IdentityObservation)
            .expect("identity run should begin");
        let mut key = Vec::new();
        let mut value = Vec::new();
        for observation in observations {
            append_identity_observation(&mut writer, observation, &mut key, &mut value)
                .expect("identity observations should remain sorted");
        }
        store
            .accept_input_run(writer.seal().expect("identity run should seal"))
            .expect("identity run should be accepted");
    }

    #[test]
    fn publisher_installs_one_complete_immutable_generation() {
        let mut store = store(TemporaryStorage::with_limit_bytes(128 * 1024));
        add_path_run(
            &mut store,
            &[
                path_observation("alpha", PathEntryKind::Directory, 0),
                path_observation("alpha/file", PathEntryKind::File, 4),
            ],
        );
        let file_id = file_id::FileId::new_inode(1, 2);
        add_identity_run(&mut store, &[identity_observation("alpha/file", file_id)]);

        assert_eq!(
            store.publish().expect("generation should publish"),
            ScanGeneration::initial()
        );
        let published = store
            .published
            .as_ref()
            .expect("generation should be retained");
        assert_eq!(published.manifest.state(), ManifestState::Published);
        assert_eq!(published.manifest.runs().len(), 4);
        assert_eq!(
            published
                .manifest
                .runs()
                .iter()
                .map(|entry| entry.descriptor.kind())
                .collect::<Vec<_>>(),
            vec![
                RunKind::PathObservation,
                RunKind::IdentityObservation,
                RunKind::AllocationContribution,
                RunKind::DirectorySummary,
            ]
        );

        let mut summaries = Vec::new();
        store
            .published_mut()
            .expect("published generation should be retained")
            .with_directory_summaries(|reader| {
                visit_directory_summaries(reader, |ordinal, summary| {
                    summaries.push((ordinal, summary));
                    Ok(())
                })
                .map_err(ScanStoreError::from)
            })
            .expect("summary run should validate");

        let mut contributions = Vec::new();
        store
            .published_mut()
            .expect("published generation should be retained")
            .with_allocation_contributions(|reader| {
                visit_allocation_contributions(reader, |file_id, contribution| {
                    contributions.push((file_id, contribution));
                    Ok(())
                })
                .map_err(ScanStoreError::from)
            })
            .expect("allocation run should validate");
        let summary_count = store
            .published_mut()
            .expect("published generation should be retained")
            .with_directory_summaries(|reader| -> Result<usize, ScanStoreError> {
                let mut key = Vec::new();
                let mut value = Vec::new();
                let mut count = 0_usize;
                while reader.next_record_into(&mut key, &mut value)? {
                    count = count.saturating_add(1);
                }
                Ok(count)
            })
            .expect("summary run should be reusable");
        assert_eq!(summary_count, 2);
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].1.path, path("alpha"));
        assert_eq!(summaries[1].1.path, RelativePath::root());
        assert_eq!(contributions.len(), 1);
        assert_eq!(contributions[0].0, file_id);
        assert_eq!(contributions[0].1.placement, AllocationPlacement::Leaf);
    }

    #[test]
    fn active_input_families_compact_at_a_fixed_fan_in() {
        let storage = TemporaryStorage::with_limit_bytes(512 * 1024);
        let mut store = store(storage);
        for index in 0..MAX_ACTIVE_INPUT_RUNS {
            add_path_run(
                &mut store,
                &[path_observation(
                    &format!("entry-{index:02}"),
                    PathEntryKind::File,
                    1,
                )],
            );
        }
        assert_eq!(store.active_run_counts(), Some((1, 0)));
    }

    #[test]
    fn newer_active_generation_keeps_prior_published_snapshot_and_rejects_stale_runs() {
        let storage = TemporaryStorage::with_limit_bytes(128 * 1024);
        let mut store = store(storage.clone());
        store.publish().expect("empty generation should publish");
        let next = ScanGeneration::from_value(1);
        store
            .begin_generation(next)
            .expect("newer generation should begin");
        assert_eq!(
            store.published_generation(),
            Some(ScanGeneration::initial())
        );
        assert_eq!(store.active_generation(), Some(next));

        let stale = RunWriter::new(
            tempfile::tempfile().expect("temporary stale run should open"),
            storage
                .reservation(0)
                .expect("stale reservation should fit"),
            RunDescriptor::new(ScanGeneration::initial(), 99, RunKind::PathObservation),
            RUN_BLOCK_BYTES,
        )
        .expect("stale writer should initialize")
        .seal()
        .expect("stale writer should seal");
        assert!(matches!(
            store.accept_input_run(stale),
            Err(ScanStoreError::GenerationMismatch)
        ));
    }
    #[test]
    fn overlay_generation_replaces_only_the_focused_subtree() {
        let mut store = store(TemporaryStorage::with_limit_bytes(256 * 1024));
        add_path_run(
            &mut store,
            &[
                path_observation("alpha", PathEntryKind::Directory, 0),
                path_observation("alpha/old", PathEntryKind::File, 4),
                path_observation("beta", PathEntryKind::File, 3),
            ],
        );
        add_identity_run(
            &mut store,
            &[
                identity_observation("alpha/old", file_id::FileId::new_inode(1, 1)),
                identity_observation("beta", file_id::FileId::new_inode(2, 2)),
            ],
        );
        store.publish().expect("base generation should publish");

        let next = ScanGeneration::from_value(1);
        store
            .begin_overlay_generation(next, &path("alpha"))
            .expect("focused overlay should start");
        add_path_run(
            &mut store,
            &[
                path_observation("alpha", PathEntryKind::Directory, 0),
                path_observation("alpha/new", PathEntryKind::File, 9),
            ],
        );
        add_identity_run(
            &mut store,
            &[identity_observation(
                "alpha/new",
                file_id::FileId::new_inode(3, 3),
            )],
        );
        assert_eq!(store.publish().expect("overlay should publish"), next);

        let root = store
            .published_mut()
            .expect("overlay should be published")
            .page(PageRequest::first(RelativePath::root(), 8))
            .expect("root page should load");
        assert_eq!(
            root.entries
                .iter()
                .map(|entry| entry.path.clone())
                .collect::<Vec<_>>(),
            vec![path("alpha"), path("beta")]
        );
        let alpha = store
            .published_mut()
            .expect("overlay should be published")
            .page(PageRequest::first(path("alpha"), 8))
            .expect("focused page should load");
        assert_eq!(
            alpha
                .entries
                .iter()
                .map(|entry| entry.path.clone())
                .collect::<Vec<_>>(),
            vec![path("alpha/new")]
        );
    }
}
