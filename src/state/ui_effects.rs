use ::std::path::PathBuf;
use std::time::Duration;

use super::deletion_work::DeletionWorkSummary;
use super::tiles::Tile;
use crate::deletion::DeletionReport;
use crate::model::NodeId;

/// Human-facing counters from the last reconciled deletion. The report itself
/// remains the source of exact history; this fixed-size summary keeps normal
/// navigation informed without retaining another report-sized buffer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeletionSummary {
    pub deleted: u64,
    pub changed: u64,
    pub missing: u64,
    pub failed: u64,
    pub unattempted: u64,
    /// Whether every reported per-entry outcome is precise.
    pub precise: bool,
    pub reporting_complete: bool,
}

impl DeletionSummary {
    pub(crate) fn from_report(report: &DeletionReport) -> Self {
        Self {
            deleted: report.deleted_entries(),
            changed: report.changed_entries(),
            missing: report.missing_entries(),
            failed: report.failed_entries(),
            unattempted: report.unattempted_entries(),
            precise: report.precise,
            reporting_complete: report.reporting_complete(),
        }
    }
}

/// Shortest visible interval for a completed tile's checkerboard departure.
pub(crate) const DELETION_DEPARTURE_MIN_DURATION: Duration = Duration::from_millis(180);
/// Caps acknowledgement latency so consecutive deletions remain responsive.
pub(crate) const DELETION_DEPARTURE_MAX_DURATION: Duration = Duration::from_millis(420);
const DELETION_DEPARTURE_MIN_MILLIS: u64 = 180;
const DELETION_DEPARTURE_MAX_MILLIS: u64 = 420;
const CELLS_PER_DEPARTURE_MILLISECOND: u64 = 12;
const ENTRY_DEPARTURE_MILLIS: u64 = 3;
const MAX_WEIGHTED_DELETION_ENTRIES: u64 = 64;

/// Scales acknowledgement time by the visual surface and completed operation size.
#[must_use]
pub(crate) fn deletion_departure_duration(deleted_entries: u64, tile_cells: u64) -> Duration {
    let cell_millis = tile_cells / CELLS_PER_DEPARTURE_MILLISECOND;
    let entry_millis = deleted_entries
        .saturating_sub(1)
        .min(MAX_WEIGHTED_DELETION_ENTRIES)
        .saturating_mul(ENTRY_DEPARTURE_MILLIS);
    Duration::from_millis(
        DELETION_DEPARTURE_MIN_MILLIS
            .saturating_add(cell_millis)
            .saturating_add(entry_millis)
            .min(DELETION_DEPARTURE_MAX_MILLIS),
    )
}

#[derive(Clone, Debug)]
pub(crate) struct DeletionDeparture {
    pub(crate) tile: Tile,
    pub(crate) started_at: Duration,
    pub(crate) duration: Duration,
}

impl DeletionDeparture {
    #[must_use]
    pub(crate) fn is_finished(&self, now: Duration) -> bool {
        now.saturating_sub(self.started_at) >= self.duration
    }
}

pub struct UiEffects {
    pub flash_space_freed: bool,
    pub current_path_is_red: bool,
    pub deletion_in_progress: bool,
    pub deletion_work: DeletionWorkSummary,
    /// Entries that reached the model during the active scan, with no guessed total.
    pub loading_entries_indexed: u64,
    pub last_read_path: Option<PathBuf>,
    pub last_deletion_summary: Option<DeletionSummary>,
    /// Concise outcome for a background operation that could not proceed.
    pub last_deletion_notice: Option<&'static str>,
    deletion_departure: Option<DeletionDeparture>,
}

impl UiEffects {
    pub const fn new() -> Self {
        Self {
            flash_space_freed: false,
            current_path_is_red: false,
            deletion_in_progress: false,
            loading_entries_indexed: 0,
            last_read_path: None,
            deletion_work: DeletionWorkSummary::new(),
            last_deletion_summary: None,
            last_deletion_notice: None,
            deletion_departure: None,
        }
    }

    pub(crate) fn record_deletion_result(&mut self, report: &DeletionReport) {
        self.last_deletion_summary = Some(DeletionSummary::from_report(report));
        self.last_deletion_notice = None;
    }

    pub(crate) fn begin_deletion_departure(
        &mut self,
        tile: Tile,
        now: Duration,
        duration: Duration,
    ) {
        self.deletion_departure = Some(DeletionDeparture {
            tile,
            started_at: now,
            duration,
        });
    }

    #[must_use]
    pub(crate) fn deletion_departure(&self) -> Option<&DeletionDeparture> {
        self.deletion_departure.as_ref()
    }

    pub(crate) fn has_deletion_departure_for(&self, node_id: NodeId) -> bool {
        self.deletion_departure
            .as_ref()
            .is_some_and(|departure| departure.tile.node_id == node_id)
    }

    #[must_use]
    pub(crate) fn deletion_departure_is_finished(&self, now: Duration) -> bool {
        self.deletion_departure
            .as_ref()
            .is_some_and(|departure| departure.is_finished(now))
    }

    #[must_use]
    pub(crate) const fn has_deletion_departure(&self) -> bool {
        self.deletion_departure.is_some()
    }

    pub(crate) fn clear_deletion_departure(&mut self) {
        self.deletion_departure = None;
    }

    pub(crate) const fn record_deletion_notice(&mut self, notice: &'static str) {
        self.last_deletion_notice = Some(notice);
    }

    pub(crate) fn reset_loading_activity(&mut self) {
        self.loading_entries_indexed = 0;
        self.last_read_path = None;
    }

    pub(crate) fn record_loading_entry(&mut self, path: PathBuf) {
        self.loading_entries_indexed = self.loading_entries_indexed.saturating_add(1);
        self.last_read_path = Some(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn departure_duration_grows_with_tile_cells_and_deleted_entries() {
        let minimum = deletion_departure_duration(1, 0);
        let larger_tile = deletion_departure_duration(1, 1_200);
        let larger_operation = deletion_departure_duration(64, 0);
        let capped = deletion_departure_duration(u64::MAX, u64::MAX);

        assert_eq!(minimum, DELETION_DEPARTURE_MIN_DURATION);
        assert!(larger_tile > minimum);
        assert!(larger_operation > minimum);
        assert_eq!(capped, DELETION_DEPARTURE_MAX_DURATION);
    }
}
