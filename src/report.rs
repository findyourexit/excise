use std::cell::RefCell;
use std::fmt;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::ser::{Error as _, SerializeMap as _, SerializeSeq as _};
use serde::{Deserialize, Serialize, Serializer};

use crate::deletion::{DeletionEntryOutcome, DeletionReport, PlannedKind};
#[cfg(test)]
use crate::model::NodeId;
use crate::model::{ByteBounds, NodeKind, NodeState};
use crate::native_path::{
    EncodedNativePath, NativeIdentity, NativePath, safe_display_path_text, safe_display_text,
};
use crate::outcome::RunSummary;
use crate::scan_store::page::{PageEntryKind, PageIndexError, ScanPageEntry};
use crate::scan_store::path_reducer::{Coverage, SummaryMetrics};
use crate::scan_store::run_file::RunError;
use crate::scan_store::session::PublishedGeneration;

pub const SCAN_REPORT_SCHEMA_VERSION: u16 = 3;
pub const DELETION_HISTORY_SCHEMA_VERSION: u16 = 1;
#[cfg(test)]
const NATIVE_PATH_SCHEMA_ID: &str =
    "https://github.com/findyourexit/excise/schemas/native-path-v1.json";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ScanReportState {
    Exact,
    Uncertain,
    SummaryOnly,
    Cancelled,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AccountingDefinition {
    pub headline: String,
    pub hard_links_deduplicated: bool,
    pub shared_extents_deduplicated: bool,
    pub directory_metadata_included: bool,
}

impl Default for AccountingDefinition {
    fn default() -> Self {
        Self {
            headline: "identity-unique-allocated-bytes".to_string(),
            hard_links_deduplicated: true,
            shared_extents_deduplicated: false,
            directory_metadata_included: false,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScanReportEntry {
    pub path: EncodedNativePath,
    pub display_path: String,
    pub kind: NodeKind,
    pub state: NodeState,
    pub identity: Option<NativeIdentity>,
    pub allocated_bytes: ByteBounds,
    pub reclaimable_bytes: ByteBounds,
    pub apparent_bytes: u128,
    pub descendants: u64,
    pub unscanned_reason: Option<String>,
}

/// Owned, serializable scan-report document used for decoding and contract validation.
///
/// Production export uses [`ScanReport`] instead. It streams the immutable
/// published scan instead of cloning every entry into this document.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScanReportDocument {
    pub document_kind: String,
    pub schema_version: u16,
    pub root: EncodedNativePath,
    pub display_root: String,
    pub state: ScanReportState,
    pub accounting: AccountingDefinition,
    pub summary: RunSummary,
    pub entries: Vec<ScanReportEntry>,
}

/// An owned published scan serialized directly to its output sink.
///
/// The report retains published scan facts, not the limited live model, so
/// noninteractive output cannot silently diverge from the tree-map source.
pub struct ScanReport {
    root: PathBuf,
    root_identity: Option<NativeIdentity>,
    published: Option<RefCell<PublishedGeneration>>,
    summary_root: Option<(SummaryMetrics, Coverage)>,
    summary: RunSummary,
    state: ScanReportState,
}

impl ScanReport {
    #[must_use]
    pub(crate) fn from_published_generation(
        root: PathBuf,
        root_identity: Option<NativeIdentity>,
        published: PublishedGeneration,
        summary: RunSummary,
        state: ScanReportState,
    ) -> Self {
        Self {
            root,
            root_identity,
            published: Some(RefCell::new(published)),
            summary_root: None,
            summary,
            state,
        }
    }

    #[must_use]
    pub(crate) fn cancelled(
        root: PathBuf,
        root_identity: Option<NativeIdentity>,
        summary: RunSummary,
    ) -> Self {
        Self {
            root,
            root_identity,
            published: None,
            summary_root: None,
            summary,
            state: ScanReportState::Cancelled,
        }
    }

    /// Creates a terminal report when capacity prevented retaining a navigable map.
    #[must_use]
    pub(crate) fn summary_only(
        root: PathBuf,
        root_identity: Option<NativeIdentity>,
        summary: RunSummary,
    ) -> Self {
        Self::summary_only_with_root(
            root,
            root_identity,
            SummaryMetrics::default(),
            Coverage::Uncertain,
            summary,
        )
    }

    /// Creates a deterministic directory-summary report without a child map.
    #[must_use]
    pub(crate) fn summary_only_with_root(
        root: PathBuf,
        root_identity: Option<NativeIdentity>,
        root_metrics: SummaryMetrics,
        root_coverage: Coverage,
        summary: RunSummary,
    ) -> Self {
        Self {
            root,
            root_identity,
            published: None,
            summary_root: Some((root_metrics, root_coverage)),
            summary,
            state: ScanReportState::SummaryOnly,
        }
    }

    #[must_use]
    pub const fn state(&self) -> ScanReportState {
        self.state
    }

    #[must_use]
    pub const fn summary(&self) -> &RunSummary {
        &self.summary
    }

    /// Writes this scan report as pretty JSON without materializing its entries into a document.
    ///
    /// # Errors
    ///
    /// Returns [`ReportError`] when JSON serialization or output fails.
    pub fn write_json(&self, writer: impl Write) -> Result<(), ReportError> {
        if let Some(published) = self.published.as_ref() {
            let mut published = published.try_borrow_mut().map_err(|_| {
                ReportError::Invariant("canonical report is already being serialized".to_string())
            })?;
            write_canonical_scan_report_json(
                &self.root,
                self.root_identity.as_ref(),
                &mut published,
                &self.summary,
                self.state,
                writer,
            )
        } else {
            write_empty_scan_report_json(
                &self.root,
                self.root_identity.as_ref(),
                self.summary_root,
                &self.summary,
                self.state,
                writer,
            )
        }
    }

    /// Writes this scan report as a tab-separated table without materializing its entries.
    ///
    /// # Errors
    ///
    /// Returns [`ReportError`] when table output fails.
    pub fn write_table(&self, writer: impl Write) -> Result<(), ReportError> {
        if let Some(published) = self.published.as_ref() {
            let mut published = published.try_borrow_mut().map_err(|_| {
                ReportError::Invariant("canonical report is already being serialized".to_string())
            })?;
            write_canonical_scan_report_table(
                &self.root,
                self.root_identity.as_ref(),
                &mut published,
                writer,
            )
        } else {
            write_empty_scan_report_table(
                &self.root,
                self.root_identity.as_ref(),
                self.summary_root,
                writer,
            )
        }
    }
}

#[must_use]
pub(crate) fn canonical_scan_report_state(
    published: &PublishedGeneration,
    summary: &RunSummary,
    cancelled: bool,
) -> ScanReportState {
    if cancelled {
        return ScanReportState::Cancelled;
    }
    let metrics = published.root_page_metrics();
    if summary.unreadable_entries > 0
        || published.unrecorded_path_count() > 0
        || canonical_node_state(published.root_page_coverage(), metrics) == NodeState::Uncertain
    {
        ScanReportState::Uncertain
    } else {
        ScanReportState::Exact
    }
}

pub(crate) fn write_canonical_scan_report_json(
    root: &Path,
    root_identity: Option<&NativeIdentity>,
    published: &mut PublishedGeneration,
    summary: &RunSummary,
    state: ScanReportState,
    mut writer: impl Write,
) -> Result<(), ReportError> {
    let published = RefCell::new(published);
    serde_json::to_writer_pretty(
        &mut writer,
        &StreamingCanonicalScanReport {
            root,
            root_identity,
            published: &published,
            summary,
            state,
        },
    )?;
    writer.write_all(b"\n")?;
    Ok(())
}

pub(crate) fn write_canonical_scan_report_table(
    root: &Path,
    _root_identity: Option<&NativeIdentity>,
    published: &mut PublishedGeneration,
    mut writer: impl Write,
) -> Result<(), ReportError> {
    writeln!(
        writer,
        "STATE\tALLOCATED\tRECLAIMABLE\tAPPARENT\tKIND\tPATH"
    )?;
    let root_metrics = published.root_page_metrics();
    write_canonical_table_row(
        &mut writer,
        root,
        NodeKind::Root,
        canonical_node_state(published.root_page_coverage(), root_metrics),
        root_metrics,
    )?;
    published
        .visit_page_entries(|entry| {
            let path = root.join(entry.path.to_path_buf());
            write_canonical_table_row(
                &mut writer,
                &path,
                canonical_node_kind(entry.kind),
                canonical_node_state(entry.coverage, entry.metrics),
                entry.metrics,
            )
            .map_err(CanonicalReportVisitError::from)
        })
        .map_err(|error| ReportError::Invariant(error.to_string()))
}

fn write_empty_scan_report_json(
    root: &Path,
    root_identity: Option<&NativeIdentity>,
    summary_root: Option<(SummaryMetrics, Coverage)>,
    summary: &RunSummary,
    state: ScanReportState,
    mut writer: impl Write,
) -> Result<(), ReportError> {
    serde_json::to_writer_pretty(
        &mut writer,
        &StreamingEmptyScanReport {
            root,
            root_identity,
            summary_root,
            summary,
            state,
        },
    )?;
    writer.write_all(b"\n")?;
    Ok(())
}

fn write_empty_scan_report_table(
    root: &Path,
    _root_identity: Option<&NativeIdentity>,
    summary_root: Option<(SummaryMetrics, Coverage)>,
    mut writer: impl Write,
) -> Result<(), ReportError> {
    writeln!(
        writer,
        "STATE\tALLOCATED\tRECLAIMABLE\tAPPARENT\tKIND\tPATH"
    )?;
    let (metrics, coverage) =
        summary_root.unwrap_or((SummaryMetrics::default(), Coverage::Uncertain));
    write_canonical_table_row(
        &mut writer,
        root,
        NodeKind::Root,
        canonical_node_state(coverage, metrics),
        metrics,
    )?;
    Ok(())
}

struct StreamingEmptyScanReport<'a> {
    root: &'a Path,
    root_identity: Option<&'a NativeIdentity>,
    summary_root: Option<(SummaryMetrics, Coverage)>,
    summary: &'a RunSummary,
    state: ScanReportState,
}

impl Serialize for StreamingEmptyScanReport<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut document = serializer.serialize_map(Some(8))?;
        let root = NativePath::new(self.root).encode();
        let display_root = safe_display_path_text(self.root);
        document.serialize_entry("document_kind", "scan-report")?;
        document.serialize_entry("schema_version", &SCAN_REPORT_SCHEMA_VERSION)?;
        document.serialize_entry("root", &root)?;
        document.serialize_entry("display_root", &display_root)?;
        document.serialize_entry("state", &self.state)?;
        document.serialize_entry("accounting", &AccountingDefinition::default())?;
        document.serialize_entry("summary", self.summary)?;
        document.serialize_entry(
            "entries",
            &StreamingEmptyScanEntries {
                root: self.root,
                identity: self.root_identity,
                summary_root: self.summary_root,
            },
        )?;
        document.end()
    }
}

struct StreamingEmptyScanEntries<'a> {
    root: &'a Path,
    identity: Option<&'a NativeIdentity>,
    summary_root: Option<(SummaryMetrics, Coverage)>,
}

impl Serialize for StreamingEmptyScanEntries<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut entries = serializer.serialize_seq(Some(1))?;
        let (metrics, coverage) = self
            .summary_root
            .unwrap_or((SummaryMetrics::default(), Coverage::Uncertain));
        entries.serialize_element(&CanonicalRootEntryRef {
            root: self.root,
            identity: self.identity,
            metrics,
            coverage,
        })?;
        entries.end()
    }
}

struct StreamingCanonicalScanReport<'a> {
    root: &'a Path,
    root_identity: Option<&'a NativeIdentity>,
    published: &'a RefCell<&'a mut PublishedGeneration>,
    summary: &'a RunSummary,
    state: ScanReportState,
}

impl Serialize for StreamingCanonicalScanReport<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut document = serializer.serialize_map(Some(8))?;
        let root = NativePath::new(self.root).encode();
        let display_root = safe_display_path_text(self.root);
        document.serialize_entry("document_kind", "scan-report")?;
        document.serialize_entry("schema_version", &SCAN_REPORT_SCHEMA_VERSION)?;
        document.serialize_entry("root", &root)?;
        document.serialize_entry("display_root", &display_root)?;
        document.serialize_entry("state", &self.state)?;
        document.serialize_entry("accounting", &AccountingDefinition::default())?;
        document.serialize_entry("summary", self.summary)?;
        document.serialize_entry(
            "entries",
            &StreamingCanonicalScanEntries {
                root: self.root,
                root_identity: self.root_identity,
                published: self.published,
            },
        )?;
        document.end()
    }
}

struct StreamingCanonicalScanEntries<'a> {
    root: &'a Path,
    root_identity: Option<&'a NativeIdentity>,
    published: &'a RefCell<&'a mut PublishedGeneration>,
}

impl Serialize for StreamingCanonicalScanEntries<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut entries = serializer.serialize_seq(None)?;
        let mut published = self
            .published
            .try_borrow_mut()
            .map_err(|_| S::Error::custom("canonical report is already being serialized"))?;
        let root_metrics = published.root_page_metrics();
        entries.serialize_element(&CanonicalRootEntryRef {
            root: self.root,
            identity: self.root_identity,
            metrics: root_metrics,
            coverage: published.root_page_coverage(),
        })?;
        (*published)
            .visit_page_entries(|entry| {
                entries
                    .serialize_element(&CanonicalScanEntryRef {
                        root: self.root,
                        entry: &entry,
                    })
                    .map_err(|error| CanonicalReportVisitError::Serialization(error.to_string()))
            })
            .map_err(|error| S::Error::custom(error.to_string()))?;
        entries.end()
    }
}

struct CanonicalRootEntryRef<'a> {
    root: &'a Path,
    identity: Option<&'a NativeIdentity>,
    metrics: SummaryMetrics,
    coverage: Coverage,
}

impl Serialize for CanonicalRootEntryRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serialize_canonical_entry(
            serializer,
            self.root,
            NodeKind::Root,
            self.identity,
            self.metrics,
            self.coverage,
        )
    }
}

struct CanonicalScanEntryRef<'a> {
    root: &'a Path,
    entry: &'a ScanPageEntry,
}

impl Serialize for CanonicalScanEntryRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let path = self.root.join(self.entry.path.to_path_buf());
        let identity = self
            .entry
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.identity.as_ref());
        serialize_canonical_entry(
            serializer,
            &path,
            canonical_node_kind(self.entry.kind),
            identity,
            self.entry.metrics,
            self.entry.coverage,
        )
    }
}

fn serialize_canonical_entry<S>(
    serializer: S,
    path: &Path,
    kind: NodeKind,
    identity: Option<&NativeIdentity>,
    metrics: SummaryMetrics,
    coverage: Coverage,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let state = canonical_node_state(coverage, metrics);
    let mut entry = serializer.serialize_map(Some(10))?;
    entry.serialize_entry("path", &NativePath::new(path).encode())?;
    entry.serialize_entry("display_path", &safe_display_path_text(path))?;
    entry.serialize_entry("kind", &kind)?;
    entry.serialize_entry("state", &state)?;
    entry.serialize_entry("identity", &identity)?;
    entry.serialize_entry("allocated_bytes", &metrics.allocated_bytes)?;
    entry.serialize_entry("reclaimable_bytes", &metrics.reclaimable_bytes)?;
    entry.serialize_entry("apparent_bytes", &metrics.apparent_bytes)?;
    entry.serialize_entry("descendants", &metrics.descendants)?;
    let unscanned_reason = (state == NodeState::Uncertain).then_some("scan coverage is uncertain");
    entry.serialize_entry("unscanned_reason", &unscanned_reason)?;
    entry.end()
}

fn write_canonical_table_row(
    writer: &mut impl Write,
    path: &Path,
    kind: NodeKind,
    state: NodeState,
    metrics: SummaryMetrics,
) -> io::Result<()> {
    writeln!(
        writer,
        "{state:?}\t{}\t{}\t{}\t{kind:?}\t{}",
        display_bounds(metrics.allocated_bytes),
        display_bounds(metrics.reclaimable_bytes),
        metrics.apparent_bytes,
        safe_display_path_text(path),
    )
}

fn canonical_node_kind(kind: PageEntryKind) -> NodeKind {
    match kind {
        PageEntryKind::Directory => NodeKind::Directory,
        PageEntryKind::File => NodeKind::File,
        PageEntryKind::Link => NodeKind::Link,
    }
}

fn canonical_node_state(coverage: Coverage, metrics: SummaryMetrics) -> NodeState {
    if coverage == Coverage::Uncertain
        || metrics.allocated_bytes.upper.is_none()
        || metrics.reclaimable_bytes.upper.is_none()
    {
        NodeState::Uncertain
    } else {
        NodeState::Complete
    }
}

#[derive(Debug)]
enum CanonicalReportVisitError {
    Run(RunError),
    Page(PageIndexError),
    Io(io::Error),
    Serialization(String),
}

impl From<RunError> for CanonicalReportVisitError {
    fn from(error: RunError) -> Self {
        Self::Run(error)
    }
}

impl From<PageIndexError> for CanonicalReportVisitError {
    fn from(error: PageIndexError) -> Self {
        Self::Page(error)
    }
}

impl From<io::Error> for CanonicalReportVisitError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl fmt::Display for CanonicalReportVisitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Run(error) => error.fmt(formatter),
            Self::Page(error) => error.fmt(formatter),
            Self::Io(error) => error.fmt(formatter),
            Self::Serialization(error) => formatter.write_str(error),
        }
    }
}

/// Returns reporting state from retained model uncertainty and unresolved worker failures.
///
/// Explicitly uncertain nodes and either unknown allocated or reclaimable bounds make the scan
/// inexact even when no worker reported an unreadable entry. A worker failure without a retained
/// path remains inexact through the summary fallback rather than being misreported as exact.

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case", tag = "status", content = "detail")]
pub enum DeletionOutcomeRecord {
    Deleted,
    Changed(String),
    Missing,
    Failed(String),
    Unattempted,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeletionHistoryEntry {
    pub path: EncodedNativePath,
    pub display_path: String,
    pub identity: NativeIdentity,
    pub kind: PlannedKind,
    pub apparent_bytes: u128,
    pub allocated_bytes: Option<u128>,
    pub modified_nanos: Option<u128>,
    pub outcome: DeletionOutcomeRecord,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeletionHistoryOperation {
    pub root: EncodedNativePath,
    pub display_root: String,
    pub precise: bool,
    pub soft_cancelled: bool,
    pub entries: Vec<DeletionHistoryEntry>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeletionHistoryDocument {
    pub document_kind: String,
    pub schema_version: u16,
    pub operations: Vec<DeletionHistoryOperation>,
}

impl DeletionHistoryDocument {
    /// Writes this retained deletion-history document as pretty JSON.
    ///
    /// # Errors
    ///
    /// Returns [`ReportError`] when JSON serialization or output fails.
    pub fn write_json(&self, mut writer: impl Write) -> Result<(), ReportError> {
        serde_json::to_writer_pretty(&mut writer, self)?;
        writer.write_all(b"\n")?;
        Ok(())
    }
}

/// Writes retained deletion reports without constructing a second history-sized document.
pub(crate) fn write_deletion_history_json(
    reports: &[Arc<DeletionReport>],
    mut writer: impl Write,
) -> Result<(), ReportError> {
    serde_json::to_writer_pretty(&mut writer, &StreamingDeletionHistory { reports })?;
    writer.write_all(b"\n")?;
    Ok(())
}

struct StreamingDeletionHistory<'a> {
    reports: &'a [Arc<DeletionReport>],
}

impl Serialize for StreamingDeletionHistory<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut document = serializer.serialize_map(Some(3))?;
        document.serialize_entry("document_kind", "deletion-history")?;
        document.serialize_entry("schema_version", &DELETION_HISTORY_SCHEMA_VERSION)?;
        document.serialize_entry(
            "operations",
            &StreamingDeletionOperations {
                reports: self.reports,
            },
        )?;
        document.end()
    }
}

struct StreamingDeletionOperations<'a> {
    reports: &'a [Arc<DeletionReport>],
}

impl Serialize for StreamingDeletionOperations<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut operations = serializer.serialize_seq(Some(self.reports.len()))?;
        for report in self.reports {
            operations.serialize_element(&DeletionHistoryOperationRef { report })?;
        }
        operations.end()
    }
}

struct DeletionHistoryOperationRef<'a> {
    report: &'a DeletionReport,
}

impl Serialize for DeletionHistoryOperationRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let root = self.report.scan_root.join(&self.report.root_relative_path);
        let mut operation = serializer.serialize_map(Some(5))?;
        operation.serialize_entry("root", &NativePath::new(&root).encode())?;
        operation.serialize_entry("display_root", &safe_display_path_text(&root))?;
        operation.serialize_entry("precise", &self.report.precise)?;
        operation.serialize_entry("soft_cancelled", &self.report.soft_cancelled)?;
        operation.serialize_entry(
            "entries",
            &StreamingDeletionEntries {
                report: self.report,
            },
        )?;
        operation.end()
    }
}

struct StreamingDeletionEntries<'a> {
    report: &'a DeletionReport,
}

impl Serialize for StreamingDeletionEntries<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut entries = serializer.serialize_seq(None)?;
        for result in &self.report.entries {
            let result = result.map_err(S::Error::custom)?;
            entries.serialize_element(&DeletionHistoryEntryRef {
                scan_root: &self.report.scan_root,
                result: &result,
            })?;
        }
        entries.end()
    }
}

struct DeletionHistoryEntryRef<'a> {
    scan_root: &'a Path,
    result: &'a crate::deletion::DeletionEntryResult,
}

impl Serialize for DeletionHistoryEntryRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let path = self.scan_root.join(&self.result.entry.relative_path);
        let snapshot = &self.result.entry.snapshot;
        let mut entry = serializer.serialize_map(Some(8))?;
        entry.serialize_entry("path", &NativePath::new(&path).encode())?;
        entry.serialize_entry("display_path", &safe_display_path_text(&path))?;
        entry.serialize_entry("identity", &snapshot.identity)?;
        entry.serialize_entry("kind", &snapshot.kind)?;
        entry.serialize_entry("apparent_bytes", &snapshot.apparent_bytes)?;
        entry.serialize_entry("allocated_bytes", &snapshot.allocated_bytes)?;
        entry.serialize_entry("modified_nanos", &snapshot.modified_nanos)?;
        entry.serialize_entry(
            "outcome",
            &DeletionOutcomeRecordRef {
                outcome: &self.result.outcome,
            },
        )?;
        entry.end()
    }
}

struct DeletionOutcomeRecordRef<'a> {
    outcome: &'a DeletionEntryOutcome,
}

impl Serialize for DeletionOutcomeRecordRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let outcome = match self.outcome {
            DeletionEntryOutcome::Deleted => {
                let mut outcome = serializer.serialize_map(Some(1))?;
                outcome.serialize_entry("status", "deleted")?;
                return outcome.end();
            }
            DeletionEntryOutcome::Missing => {
                let mut outcome = serializer.serialize_map(Some(1))?;
                outcome.serialize_entry("status", "missing")?;
                return outcome.end();
            }
            DeletionEntryOutcome::Unattempted => {
                let mut outcome = serializer.serialize_map(Some(1))?;
                outcome.serialize_entry("status", "unattempted")?;
                return outcome.end();
            }
            DeletionEntryOutcome::Changed(message) => {
                let mut outcome = serializer.serialize_map(Some(2))?;
                outcome.serialize_entry("status", "changed")?;
                let detail = safe_display_text(message);
                outcome.serialize_entry("detail", &detail)?;
                outcome
            }
            DeletionEntryOutcome::Failed(message) => {
                let mut outcome = serializer.serialize_map(Some(2))?;
                outcome.serialize_entry("status", "failed")?;
                let detail = safe_display_text(message);
                outcome.serialize_entry("detail", &detail)?;
                outcome
            }
        };
        outcome.end()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ReportError {
    #[error("report serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("report output failed: {0}")]
    Io(#[from] io::Error),
    #[error("report invariant failed: {0}")]
    Invariant(String),
}

fn display_bounds(bounds: ByteBounds) -> String {
    match bounds.upper {
        Some(upper) if upper == bounds.lower => upper.to_string(),
        Some(upper) => format!("{}..{upper}", bounds.lower),
        None if bounds.lower == 0 => "unknown".to_string(),
        None => format!(">={}", bounds.lower),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_path::DECEPTIVE_DISPLAY_MARKER;

    use file_id::FileId;
    use serde_json::{Value, json};
    use std::path::PathBuf;
    use std::sync::Arc;

    fn identity() -> NativeIdentity {
        NativeIdentity {
            file_id: FileId::new_inode(7, 11),
            link_count: Some(1),
            reparse_point: false,
        }
    }

    fn report_entry() -> ScanReportEntry {
        ScanReportEntry {
            path: EncodedNativePath::Utf8("/scan/example".to_string()),
            display_path: "/scan/example".to_string(),
            kind: NodeKind::File,
            state: NodeState::Complete,
            identity: Some(identity()),
            allocated_bytes: ByteBounds::exact(4096),
            reclaimable_bytes: ByteBounds::exact(4096),
            apparent_bytes: 3,
            descendants: 0,
            unscanned_reason: None,
        }
    }

    fn scan_document() -> ScanReportDocument {
        ScanReportDocument {
            document_kind: "scan-report".to_string(),
            schema_version: SCAN_REPORT_SCHEMA_VERSION,
            root: EncodedNativePath::Utf8("/scan".to_string()),
            display_root: "/scan".to_string(),
            state: ScanReportState::Exact,
            accounting: AccountingDefinition::default(),
            summary: RunSummary::default(),
            entries: vec![report_entry()],
        }
    }

    fn deletion_document() -> DeletionHistoryDocument {
        DeletionHistoryDocument {
            document_kind: "deletion-history".to_string(),
            schema_version: DELETION_HISTORY_SCHEMA_VERSION,
            operations: vec![DeletionHistoryOperation {
                root: EncodedNativePath::Utf8("/scan".to_string()),
                display_root: "/scan".to_string(),
                precise: true,
                soft_cancelled: false,
                entries: vec![DeletionHistoryEntry {
                    path: EncodedNativePath::Utf8("/scan/example".to_string()),
                    display_path: "/scan/example".to_string(),
                    identity: identity(),
                    kind: PlannedKind::File,
                    apparent_bytes: 3,
                    allocated_bytes: Some(4096),
                    modified_nanos: Some(1),
                    outcome: DeletionOutcomeRecord::Deleted,
                }],
            }],
        }
    }
    fn schema(path: &str) -> Value {
        let source = match path {
            "native-path.schema.json" => include_str!("../docs/schemas/native-path.schema.json"),
            "scan-report.schema.json" => include_str!("../docs/schemas/scan-report.schema.json"),
            "deletion-history.schema.json" => {
                include_str!("../docs/schemas/deletion-history.schema.json")
            }
            _ => panic!("unknown published schema: {path}"),
        };
        serde_json::from_str(source).expect("published schema should be valid JSON")
    }

    fn validator_for(schema: &Value, native_path_schema: &Value) -> jsonschema::Validator {
        let registry = jsonschema::Registry::new()
            .add(NATIVE_PATH_SCHEMA_ID, native_path_schema.clone())
            .expect("native-path schema should have a valid identity")
            .prepare()
            .expect("native-path registry should prepare");
        jsonschema::draft202012::options()
            .with_registry(&registry)
            .build(schema)
            .expect("published schema should compile")
    }

    #[test]
    fn deletion_history_json_round_trips_native_paths() {
        let document = deletion_document();
        let encoded = serde_json::to_string(&document).expect("history should serialize");
        let decoded: DeletionHistoryDocument =
            serde_json::from_str(&encoded).expect("history should deserialize");
        assert_eq!(decoded, document);
    }

    #[test]
    fn published_schemas_validate_serialized_documents_and_reject_contract_drift() {
        let native_path_schema = schema("native-path.schema.json");
        let scan_schema = schema("scan-report.schema.json");
        let deletion_schema = schema("deletion-history.schema.json");
        for schema in [&native_path_schema, &scan_schema, &deletion_schema] {
            assert_eq!(
                schema.get("$schema").and_then(Value::as_str),
                Some("https://json-schema.org/draft/2020-12/schema"),
                "published schema must declare Draft 2020-12"
            );
            jsonschema::draft202012::meta::validate(schema)
                .expect("published schema should satisfy Draft 2020-12");
        }
        let native_path_validator = jsonschema::draft202012::options()
            .build(&native_path_schema)
            .expect("native-path schema should compile");
        for path in [
            EncodedNativePath::UnixBytes("L3NjYW4=".to_string()),
            EncodedNativePath::WindowsUtf16Le("LwA=".to_string()),
            EncodedNativePath::Utf8("/scan".to_string()),
        ] {
            let serialized =
                serde_json::to_value(path).expect("typed native path should serialize");
            native_path_validator
                .validate(&serialized)
                .expect("serialized native path should satisfy its schema");
        }

        let scan_validator = validator_for(&scan_schema, &native_path_schema);
        for file_id in [
            FileId::new_inode(7, 11),
            FileId::new_low_res(7, 11),
            FileId::new_high_res(7, 11),
        ] {
            let mut document = scan_document();
            document.entries[0].identity = Some(NativeIdentity {
                file_id,
                link_count: Some(1),
                reparse_point: false,
            });
            let serialized =
                serde_json::to_value(document).expect("typed identity variant should serialize");
            scan_validator
                .validate(&serialized)
                .expect("serialized native identity should satisfy its schema");
        }

        let scan_document =
            serde_json::to_value(scan_document()).expect("typed scan report should serialize");
        scan_validator
            .validate(&scan_document)
            .expect("serialized scan report should satisfy its schema");

        let mut scan_drift = scan_document;
        scan_drift["summary"]["unexpected"] = json!(true);
        assert!(
            scan_validator.validate(&scan_drift).is_err(),
            "schema drift must reject a document field Rust does not define"
        );

        let deletion_validator = validator_for(&deletion_schema, &native_path_schema);
        let deletion_document = serde_json::to_value(deletion_document())
            .expect("typed deletion history should serialize");
        deletion_validator
            .validate(&deletion_document)
            .expect("serialized deletion history should satisfy its schema");
        let mut deletion_drift = deletion_document;
        deletion_drift["operations"][0]["entries"][0]["identity"]["unexpected"] = json!(true);
        assert!(
            deletion_validator.validate(&deletion_drift).is_err(),
            "identity contract drift must reject an undeclared field"
        );
    }
    #[test]
    fn streamed_deletion_history_round_trips_without_materializing_a_document() {
        use crate::deletion::{DeletionEntryResult, PlannedEntry, PlannedSnapshot};

        let report = Arc::new(DeletionReport {
            target_node_id: NodeId(0),
            root_relative_path: PathBuf::from("target"),
            scan_root: PathBuf::from("/scan"),
            entries: vec![DeletionEntryResult {
                entry: PlannedEntry {
                    relative_path: PathBuf::from("target/example"),
                    snapshot: PlannedSnapshot {
                        identity: identity(),
                        kind: PlannedKind::File,
                        apparent_bytes: 3,
                        allocated_bytes: Some(4096),
                        modified_nanos: Some(1),
                    },
                },
                outcome: DeletionEntryOutcome::Deleted,
            }]
            .into(),
            soft_cancelled: false,
            precise: true,
            estimated_bytes: 0,
        });
        let mut encoded = Vec::new();
        write_deletion_history_json(&[report], &mut encoded)
            .expect("streamed history should serialize");
        let document: Value =
            serde_json::from_slice(&encoded).expect("streamed history should parse");
        let native_path_schema = schema("native-path.schema.json");
        let deletion_schema = schema("deletion-history.schema.json");
        validator_for(&deletion_schema, &native_path_schema)
            .validate(&document)
            .expect("streamed history should satisfy its schema");
        let decoded: DeletionHistoryDocument =
            serde_json::from_value(document).expect("streamed history should deserialize");
        assert_eq!(decoded.operations.len(), 1);
        assert_eq!(decoded.operations[0].entries.len(), 1);
        assert_eq!(
            decoded.operations[0].entries[0].outcome,
            DeletionOutcomeRecord::Deleted
        );
    }

    #[test]
    fn summary_only_report_declares_a_partial_inventory() {
        let summary = RunSummary {
            scanned_entries: 17,
            unscanned_entries: 1,
            last_worker_error: Some("scan store capacity exhausted".to_string()),
            ..RunSummary::default()
        };
        let report = ScanReport::summary_only_with_root(
            PathBuf::from("/scan"),
            None,
            SummaryMetrics::leaf(99, ByteBounds::exact(50), ByteBounds::exact(40)),
            Coverage::Complete,
            summary,
        );

        assert_eq!(report.state(), ScanReportState::SummaryOnly);
        let mut encoded = Vec::new();
        report
            .write_json(&mut encoded)
            .expect("summary-only report should serialize");
        let document: Value =
            serde_json::from_slice(&encoded).expect("summary-only report should parse");
        let native_path_schema = schema("native-path.schema.json");
        let scan_schema = schema("scan-report.schema.json");
        validator_for(&scan_schema, &native_path_schema)
            .validate(&document)
            .expect("summary-only report should satisfy its schema");
        let decoded: ScanReportDocument =
            serde_json::from_value(document).expect("summary-only report should deserialize");
        assert_eq!(decoded.state, ScanReportState::SummaryOnly);
        assert_eq!(decoded.entries.len(), 1, "only the root is retained");
        assert_eq!(decoded.entries[0].apparent_bytes, 99);
        assert_eq!(decoded.entries[0].allocated_bytes, ByteBounds::exact(50));
        assert_eq!(decoded.entries[0].reclaimable_bytes, ByteBounds::exact(40));
        assert_eq!(decoded.entries[0].state, NodeState::Complete);
    }

    #[test]
    fn hostile_deletion_history_paths_are_marked_and_escaped() {
        let parent = tempfile::tempdir().expect("report parent should exist");
        let root = parent.path().join("scan-\u{202e}-root");
        std::fs::create_dir(&root).expect("report root should be created");
        let fixture_path = root.join("entry-fixture");
        std::fs::write(&fixture_path, b"payload").expect("report entry should be written");
        let metadata =
            std::fs::symlink_metadata(&fixture_path).expect("report metadata should exist");
        let entry_identity = crate::native_path::identity_for(&fixture_path, &metadata)
            .expect("report identity should be readable")
            .expect("report entry should not be a link");

        assert_hostile_deletion_history(root, entry_identity);
    }

    fn assert_hostile_deletion_history(root: PathBuf, entry_identity: NativeIdentity) {
        use crate::deletion::{DeletionEntryResult, PlannedEntry, PlannedSnapshot};

        let report = Arc::new(DeletionReport {
            target_node_id: NodeId(1),
            root_relative_path: PathBuf::from("entry-\u{1b}[31m"),
            scan_root: root,
            entries: vec![DeletionEntryResult {
                entry: PlannedEntry {
                    relative_path: PathBuf::from("entry-\u{1b}[31m"),
                    snapshot: PlannedSnapshot {
                        identity: entry_identity,
                        kind: PlannedKind::File,
                        apparent_bytes: 1,
                        allocated_bytes: Some(1),
                        modified_nanos: Some(1),
                    },
                },
                outcome: DeletionEntryOutcome::Deleted,
            }]
            .into(),
            soft_cancelled: false,
            precise: true,
            estimated_bytes: 0,
        });
        let mut history = Vec::new();
        write_deletion_history_json(&[report], &mut history)
            .expect("hostile deletion history should serialize");
        let history: Value =
            serde_json::from_slice(&history).expect("deletion history should parse");
        assert!(
            history["operations"][0]["display_root"]
                .as_str()
                .is_some_and(|path| {
                    path.contains(DECEPTIVE_DISPLAY_MARKER)
                        && path.contains("\\x1b")
                        && !path.contains('\u{1b}')
                })
        );
        assert!(
            history["operations"][0]["entries"][0]["display_path"]
                .as_str()
                .is_some_and(|path| {
                    path.contains(DECEPTIVE_DISPLAY_MARKER)
                        && path.contains("\\x1b")
                        && !path.contains('\u{1b}')
                })
        );
    }

    #[test]
    fn deletion_outcome_details_are_marked_and_escaped() {
        use crate::deletion::{DeletionEntryResult, PlannedEntry, PlannedSnapshot};

        let hostile = format!("{DECEPTIVE_DISPLAY_MARKER} changed\n\u{202e}\u{1b}[31m");
        let report = Arc::new(DeletionReport {
            target_node_id: NodeId(0),
            root_relative_path: PathBuf::from("target"),
            scan_root: PathBuf::from("/scan"),
            entries: vec![
                DeletionEntryResult {
                    entry: PlannedEntry {
                        relative_path: PathBuf::from("changed"),
                        snapshot: PlannedSnapshot {
                            identity: identity(),
                            kind: PlannedKind::File,
                            apparent_bytes: 3,
                            allocated_bytes: Some(4096),
                            modified_nanos: Some(1),
                        },
                    },
                    outcome: DeletionEntryOutcome::Changed(hostile.clone()),
                },
                DeletionEntryResult {
                    entry: PlannedEntry {
                        relative_path: PathBuf::from("failed"),
                        snapshot: PlannedSnapshot {
                            identity: identity(),
                            kind: PlannedKind::File,
                            apparent_bytes: 3,
                            allocated_bytes: Some(4096),
                            modified_nanos: Some(1),
                        },
                    },
                    outcome: DeletionEntryOutcome::Failed(hostile),
                },
            ]
            .into(),
            soft_cancelled: false,
            precise: true,
            estimated_bytes: 0,
        });

        let mut encoded = Vec::new();
        write_deletion_history_json(&[report], &mut encoded)
            .expect("hostile deletion outcomes should serialize");
        assert!(!encoded.contains(&0x1b));
        assert!(
            !encoded
                .windows(3)
                .any(|window| window == [0xe2, 0x80, 0xae])
        );

        let history: Value =
            serde_json::from_slice(&encoded).expect("deletion history should parse");
        let entries = history["operations"][0]["entries"]
            .as_array()
            .expect("deletion history entries should be an array");
        for (entry, status) in entries.iter().zip(["changed", "failed"]) {
            assert_eq!(entry["outcome"]["status"], status);
            let detail = entry["outcome"]["detail"]
                .as_str()
                .expect("outcome detail should be a string");
            assert!(detail.starts_with(DECEPTIVE_DISPLAY_MARKER));
            assert_eq!(detail.matches(DECEPTIVE_DISPLAY_MARKER).count(), 2);
            assert!(detail.contains("\\n"));
            assert!(detail.contains("\\u{202e}"));
            assert!(detail.contains("\\x1b"));
            assert!(!detail.chars().any(char::is_control));
            assert!(!detail.contains('\u{202e}'));
        }
    }
}
