use thiserror::Error;

use crate::model::{ByteBounds, EntrySnapshot};
use crate::scan_coordinator::RelativePath;

/// Scanner-facing kind for a canonical path observation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PathEntryKind {
    Directory,
    File,
    Link,
}

/// Completeness of one path and every contribution folded beneath it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Coverage {
    Complete,
    Uncertain,
}

impl Coverage {
    fn combine(self, other: Self) -> Self {
        if self == Self::Uncertain || other == Self::Uncertain {
            Self::Uncertain
        } else {
            Self::Complete
        }
    }
}

#[must_use]
pub(crate) const fn coverage_code(coverage: Coverage) -> u8 {
    match coverage {
        Coverage::Complete => 1,
        Coverage::Uncertain => 2,
    }
}

#[must_use]
pub(crate) const fn coverage_from_code(value: u8) -> Option<Coverage> {
    match value {
        1 => Some(Coverage::Complete),
        2 => Some(Coverage::Uncertain),
        _ => None,
    }
}

/// Accounting facts that can be reduced without a mutable presentation tree.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct SummaryMetrics {
    pub(crate) apparent_bytes: u128,
    pub(crate) allocated_bytes: ByteBounds,
    pub(crate) reclaimable_bytes: ByteBounds,
    pub(crate) descendants: u64,
}

impl SummaryMetrics {
    #[must_use]
    pub(crate) const fn leaf(
        apparent_bytes: u128,
        allocated_bytes: ByteBounds,
        reclaimable_bytes: ByteBounds,
    ) -> Self {
        Self {
            apparent_bytes,
            allocated_bytes,
            reclaimable_bytes,
            descendants: 0,
        }
    }

    fn add_child(&mut self, child: Self) {
        self.apparent_bytes = self.apparent_bytes.saturating_add(child.apparent_bytes);
        self.allocated_bytes.add(child.allocated_bytes);
        self.reclaimable_bytes.add(child.reclaimable_bytes);
        self.descendants = self
            .descendants
            .saturating_add(child.descendants)
            .saturating_add(1);
    }
}

/// One key-sorted path fact accepted from a sealed observation run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PathObservation {
    pub(crate) path: RelativePath,
    pub(crate) kind: PathEntryKind,
    pub(crate) metrics: SummaryMetrics,
    pub(crate) coverage: Coverage,
    pub(crate) snapshot: Option<EntrySnapshot>,
}

impl PathObservation {
    #[must_use]
    pub(crate) const fn new(
        path: RelativePath,
        kind: PathEntryKind,
        metrics: SummaryMetrics,
        coverage: Coverage,
    ) -> Self {
        Self {
            path,
            kind,
            metrics,
            coverage,
            snapshot: None,
        }
    }

    #[must_use]
    pub(crate) fn with_snapshot(
        path: RelativePath,
        kind: PathEntryKind,
        metrics: SummaryMetrics,
        coverage: Coverage,
        snapshot: Option<EntrySnapshot>,
    ) -> Self {
        Self {
            path,
            kind,
            metrics,
            coverage,
            snapshot,
        }
    }
}

/// Final summary emitted once every descendant of one directory is reduced.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DirectorySummary {
    pub(crate) path: RelativePath,
    pub(crate) metrics: SummaryMetrics,
    pub(crate) coverage: Coverage,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub(crate) enum PathReductionError {
    #[error("path observations must not include the scan root")]
    RootObservation,
    #[error("path observations are not strictly sorted")]
    UnsortedPath,
    #[error("path observations contain a duplicate path")]
    DuplicatePath,
    #[error("path observation has no retained directory parent")]
    MissingParent,
}

struct OpenDirectory {
    path: RelativePath,
    metrics: SummaryMetrics,
    coverage: Coverage,
}

impl OpenDirectory {
    fn from_observation(observation: PathObservation) -> Self {
        Self {
            path: observation.path,
            metrics: observation.metrics,
            coverage: observation.coverage,
        }
    }

    fn into_summary(self) -> DirectorySummary {
        DirectorySummary {
            path: self.path,
            metrics: self.metrics,
            coverage: self.coverage,
        }
    }

    fn add_child(&mut self, metrics: SummaryMetrics, coverage: Coverage) {
        self.metrics.add_child(metrics);
        self.coverage = self.coverage.combine(coverage);
    }
}

/// Reduces canonical path observations in one forward pass and bounded depth.
///
/// Input must be strictly ordered by [`RelativePath`] and must contain every
/// directory before its descendants. Summaries are emitted post-order so a
/// caller can persist them directly in a bottom-up indexed run.
///
/// # Errors
///
/// Returns an error for noncanonical paths or when `emit` rejects a summary.
pub(crate) fn reduce_sorted_paths(
    observations: impl IntoIterator<Item = PathObservation>,
    emit: impl FnMut(DirectorySummary) -> Result<(), PathReductionError>,
) -> Result<(), PathReductionError> {
    let mut observations = observations.into_iter();
    reduce_sorted_path_stream::<PathReductionError>(|| Ok(observations.next()), emit)
}

/// Reduces a fallible observation source without materializing its records.
///
/// This is the streaming counterpart of [`reduce_sorted_paths`]. It preserves
/// the source's typed error instead of collapsing malformed on-disk records
/// into a reduction error.
///
/// # Errors
///
/// Returns a source, reduction, or emit error.
pub(crate) fn reduce_sorted_path_stream<E>(
    mut next: impl FnMut() -> Result<Option<PathObservation>, E>,
    mut emit: impl FnMut(DirectorySummary) -> Result<(), E>,
) -> Result<(), E>
where
    E: From<PathReductionError>,
{
    let mut directories = vec![OpenDirectory {
        path: RelativePath::root(),
        metrics: SummaryMetrics::default(),
        coverage: Coverage::Complete,
    }];
    let mut previous = None;

    while let Some(observation) = next()? {
        if observation.path.is_root() {
            return Err(PathReductionError::RootObservation.into());
        }
        if let Some(previous) = previous.as_ref() {
            match observation.path.cmp(previous) {
                std::cmp::Ordering::Less => return Err(PathReductionError::UnsortedPath.into()),
                std::cmp::Ordering::Equal => return Err(PathReductionError::DuplicatePath.into()),
                std::cmp::Ordering::Greater => {}
            }
        }
        previous = Some(observation.path.clone());

        while !observation
            .path
            .starts_with(&directories.last().expect("root directory must remain").path)
        {
            close_directory(&mut directories, &mut emit)?;
        }
        let parent = directories.last().expect("root directory must remain");
        if !observation.path.is_direct_child_of(&parent.path) {
            return Err(PathReductionError::MissingParent.into());
        }
        match observation.kind {
            PathEntryKind::Directory => {
                directories.push(OpenDirectory::from_observation(observation));
            }
            PathEntryKind::File | PathEntryKind::Link => {
                directories
                    .last_mut()
                    .expect("root directory must remain")
                    .add_child(observation.metrics, observation.coverage);
            }
        }
    }

    while directories.len() > 1 {
        close_directory(&mut directories, &mut emit)?;
    }
    let root = directories.pop().expect("root directory must remain");
    emit(root.into_summary())
}

fn close_directory<E>(
    directories: &mut Vec<OpenDirectory>,
    emit: &mut impl FnMut(DirectorySummary) -> Result<(), E>,
) -> Result<(), E> {
    let directory = directories.pop().expect("root directory must remain");
    let metrics = directory.metrics;
    let coverage = directory.coverage;
    emit(DirectorySummary {
        path: directory.path,
        metrics,
        coverage,
    })?;
    directories
        .last_mut()
        .expect("closed directory must have a parent")
        .add_child(metrics, coverage);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn path(value: &str) -> RelativePath {
        RelativePath::from_path(Path::new(value)).expect("fixture path should be relative")
    }

    fn observation(
        path: &str,
        kind: PathEntryKind,
        apparent: u128,
        allocated: u128,
    ) -> PathObservation {
        PathObservation::new(
            self::path(path),
            kind,
            SummaryMetrics::leaf(
                apparent,
                ByteBounds::exact(allocated),
                ByteBounds::exact(allocated),
            ),
            Coverage::Complete,
        )
    }

    fn reduce(
        observations: Vec<PathObservation>,
    ) -> Result<Vec<DirectorySummary>, PathReductionError> {
        let mut summaries = Vec::new();
        reduce_sorted_paths(observations, |summary| {
            summaries.push(summary);
            Ok(())
        })?;
        Ok(summaries)
    }

    #[test]
    fn reduction_emits_complete_bottom_up_directory_summaries() {
        let summaries = reduce(vec![
            observation("alpha", PathEntryKind::Directory, 0, 0),
            observation("alpha/first", PathEntryKind::File, 3, 4),
            observation("beta", PathEntryKind::Directory, 0, 0),
            observation("beta/second", PathEntryKind::Link, 5, 8),
        ])
        .expect("canonical observations should reduce");

        assert_eq!(summaries.len(), 3);
        assert_eq!(summaries[0].path, path("alpha"));
        assert_eq!(
            summaries[0].metrics,
            SummaryMetrics {
                apparent_bytes: 3,
                allocated_bytes: ByteBounds::exact(4),
                reclaimable_bytes: ByteBounds::exact(4),
                descendants: 1,
            }
        );
        assert_eq!(summaries[1].path, path("beta"));
        assert_eq!(
            summaries[1].metrics,
            SummaryMetrics {
                apparent_bytes: 5,
                allocated_bytes: ByteBounds::exact(8),
                reclaimable_bytes: ByteBounds::exact(8),
                descendants: 1,
            }
        );
        assert_eq!(summaries[2].path, RelativePath::root());
        assert_eq!(
            summaries[2].metrics,
            SummaryMetrics {
                apparent_bytes: 8,
                allocated_bytes: ByteBounds::exact(12),
                reclaimable_bytes: ByteBounds::exact(12),
                descendants: 4,
            }
        );
        assert!(
            summaries
                .iter()
                .all(|summary| summary.coverage == Coverage::Complete)
        );
    }

    #[test]
    fn canonical_sort_makes_arrival_order_irrelevant() {
        let canonical = vec![
            observation("alpha", PathEntryKind::Directory, 0, 0),
            observation("alpha/first", PathEntryKind::File, 3, 4),
            observation("beta", PathEntryKind::Directory, 0, 0),
            observation("beta/second", PathEntryKind::File, 5, 8),
        ];
        let expected = reduce(canonical.clone()).expect("canonical fixture should reduce");
        let mut reordered = canonical.into_iter().rev().collect::<Vec<_>>();
        reordered.sort_by(|left, right| left.path.cmp(&right.path));

        assert_eq!(
            reduce(reordered).expect("sorted fixture should reduce"),
            expected
        );
    }

    #[test]
    fn uncertainty_propagates_without_losing_known_content_size() {
        let mut unknown = observation("alpha/unreadable", PathEntryKind::File, 7, 0);
        unknown.metrics.allocated_bytes = ByteBounds::unknown();
        unknown.metrics.reclaimable_bytes = ByteBounds::unknown();
        unknown.coverage = Coverage::Uncertain;
        let summaries = reduce(vec![
            observation("alpha", PathEntryKind::Directory, 0, 0),
            unknown,
        ])
        .expect("uncertain child should reduce");

        for summary in summaries {
            assert_eq!(summary.coverage, Coverage::Uncertain);
            assert_eq!(summary.metrics.apparent_bytes, 7);
            assert_eq!(summary.metrics.allocated_bytes, ByteBounds::unknown());
            assert_eq!(summary.metrics.reclaimable_bytes, ByteBounds::unknown());
        }
    }

    #[test]
    fn reducer_rejects_missing_parent_and_noncanonical_order() {
        assert_eq!(
            reduce(vec![observation("alpha/child", PathEntryKind::File, 1, 1)]),
            Err(PathReductionError::MissingParent)
        );
        assert_eq!(
            reduce(vec![
                observation("beta", PathEntryKind::File, 1, 1),
                observation("alpha", PathEntryKind::File, 1, 1),
            ]),
            Err(PathReductionError::UnsortedPath)
        );
        assert_eq!(
            reduce(vec![
                observation("alpha", PathEntryKind::File, 1, 1),
                observation("alpha", PathEntryKind::File, 1, 1),
            ]),
            Err(PathReductionError::DuplicatePath)
        );
    }
}
