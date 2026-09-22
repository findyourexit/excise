use ::std::path::PathBuf;

use super::deletion_work::DeletionWorkSummary;
use crate::deletion::DeletionReport;

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

pub struct UiEffects {
    pub flash_space_freed: bool,
    pub current_path_is_red: bool,
    pub deletion_in_progress: bool,
    pub deletion_work: DeletionWorkSummary,
    pub loading_progress_indicator: u64,
    pub last_read_path: Option<PathBuf>,
    pub last_deletion_summary: Option<DeletionSummary>,
    /// Concise outcome for a background operation that could not proceed.
    pub last_deletion_notice: Option<&'static str>,
}

impl UiEffects {
    pub const fn new() -> Self {
        Self {
            flash_space_freed: false,
            current_path_is_red: false,
            deletion_in_progress: false,
            loading_progress_indicator: 0,
            last_read_path: None,
            deletion_work: DeletionWorkSummary::new(),
            last_deletion_summary: None,
            last_deletion_notice: None,
        }
    }

    pub(crate) fn record_deletion_result(&mut self, report: &DeletionReport) {
        self.last_deletion_summary = Some(DeletionSummary::from_report(report));
        self.last_deletion_notice = None;
    }

    pub(crate) const fn record_deletion_notice(&mut self, notice: &'static str) {
        self.last_deletion_notice = Some(notice);
    }

    pub const fn increment_loading_progress_indicator(&mut self) {
        // increasing and decreasing this number will increase
        // the scanning text animation speed
        self.loading_progress_indicator += 3;
    }
}
