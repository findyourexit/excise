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
use crate::state::files::FileTree;

pub const REPORT_SCHEMA_VERSION: u16 = 1;
#[cfg(test)]
const NATIVE_PATH_SCHEMA_ID: &str =
    "https://github.com/findyourexit/excise/schemas/native-path-v1.json";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ScanReportState {
    Exact,
    Uncertain,
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

/// The owned, serializable scan-report document used for decoding and contract validation.
///
/// Production export deliberately uses [`ScanReport`] instead: it walks the bounded model and
/// serializes each entry as it is encountered rather than cloning every entry into this document.
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

/// An owned completed scan whose model is serialized directly to its output sink.
///
/// Keeping the model here avoids constructing a second, full report-sized allocation after a
/// near-budget scan has completed.
pub struct ScanReport {
    tree: FileTree,
    summary: RunSummary,
    state: ScanReportState,
}

impl ScanReport {
    #[must_use]
    pub(crate) fn from_completed_tree(
        tree: FileTree,
        summary: RunSummary,
        state: ScanReportState,
    ) -> Self {
        Self {
            tree,
            summary,
            state,
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
        write_scan_report_json(
            &self.tree.path_in_filesystem,
            &self.tree,
            &self.summary,
            self.state,
            writer,
        )
    }

    /// Writes this scan report as a tab-separated table without materializing its entries.
    ///
    /// # Errors
    ///
    /// Returns [`ReportError`] when table output fails.
    pub fn write_table(&self, writer: impl Write) -> Result<(), ReportError> {
        write_scan_report_table(&self.tree, writer)
    }
}

/// Returns reporting state from retained model uncertainty and unresolved worker failures.
///
/// Explicitly uncertain nodes and either unknown allocated or reclaimable bounds make the scan
/// inexact even when no worker reported an unreadable entry. A worker failure without a retained
/// path remains inexact through the summary fallback rather than being misreported as exact.
#[must_use]
pub(crate) fn scan_report_state(
    tree: &FileTree,
    summary: &RunSummary,
    cancelled: bool,
) -> ScanReportState {
    if cancelled {
        ScanReportState::Cancelled
    } else if summary.unreadable_entries > 0
        || tree.nodes().any(|node| {
            node.state == NodeState::Uncertain
                || node.metrics.allocated_bytes.upper.is_none()
                || node.metrics.reclaimable_bytes.upper.is_none()
        })
    {
        ScanReportState::Uncertain
    } else {
        ScanReportState::Exact
    }
}

#[must_use]
pub(crate) fn scan_is_uncertain(tree: &FileTree, summary: &RunSummary) -> bool {
    scan_report_state(tree, summary, false) == ScanReportState::Uncertain
}

pub(crate) fn write_scan_report_json(
    root: &Path,
    tree: &FileTree,
    summary: &RunSummary,
    state: ScanReportState,
    mut writer: impl Write,
) -> Result<(), ReportError> {
    serde_json::to_writer_pretty(
        &mut writer,
        &StreamingScanReport {
            root,
            tree,
            summary,
            state,
        },
    )?;
    writer.write_all(b"\n")?;
    Ok(())
}

pub(crate) fn write_scan_report_table(
    tree: &FileTree,
    mut writer: impl Write,
) -> Result<(), ReportError> {
    writeln!(
        writer,
        "STATE\tALLOCATED\tRECLAIMABLE\tAPPARENT\tKIND\tPATH"
    )?;
    write_scan_table_entries(tree, &mut writer)
}

struct StreamingScanReport<'a> {
    root: &'a Path,
    tree: &'a FileTree,
    summary: &'a RunSummary,
    state: ScanReportState,
}

impl Serialize for StreamingScanReport<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut document = serializer.serialize_map(Some(8))?;
        let root = NativePath::new(self.root).encode();
        let display_root = safe_display_path_text(self.root);
        document.serialize_entry("document_kind", "scan-report")?;
        document.serialize_entry("schema_version", &REPORT_SCHEMA_VERSION)?;
        document.serialize_entry("root", &root)?;
        document.serialize_entry("display_root", &display_root)?;
        document.serialize_entry("state", &self.state)?;
        document.serialize_entry("accounting", &AccountingDefinition::default())?;
        document.serialize_entry("summary", self.summary)?;
        document.serialize_entry("entries", &StreamingScanEntries { tree: self.tree })?;
        document.end()
    }
}

struct StreamingScanEntries<'a> {
    tree: &'a FileTree,
}

impl Serialize for StreamingScanEntries<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut entries = serializer.serialize_seq(None)?;
        serialize_scan_entries(self.tree, &mut entries)?;
        entries.end()
    }
}

fn serialize_scan_entries<S>(tree: &FileTree, entries: &mut S) -> Result<(), S::Error>
where
    S: serde::ser::SerializeSeq,
{
    let mut stack = vec![tree.total_node().id];
    while let Some(id) = stack.pop() {
        let node = tree
            .node(id)
            .ok_or_else(|| S::Error::custom("report node disappeared during serialization"))?;
        let path = tree
            .path_for_id(id)
            .ok_or_else(|| S::Error::custom("report path disappeared during serialization"))?;
        entries.serialize_element(&ScanReportEntryRef { path, node })?;
        // Children are lexically ordered. Reverse-push preserves that order when popped.
        stack.extend(node.children.iter().rev().copied());
    }
    Ok(())
}

struct ScanReportEntryRef<'a> {
    path: PathBuf,
    node: &'a crate::model::Node,
}

impl Serialize for ScanReportEntryRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut entry = serializer.serialize_map(Some(10))?;
        let path = NativePath::new(&self.path).encode();
        let display_path = safe_display_path_text(&self.path);
        let unscanned_reason = self
            .node
            .unscanned_reason
            .as_ref()
            .map(|reason| safe_display_text(&format!("{reason:?}")));
        entry.serialize_entry("path", &path)?;
        entry.serialize_entry("display_path", &display_path)?;
        entry.serialize_entry("kind", &self.node.kind)?;
        entry.serialize_entry("state", &self.node.state)?;
        entry.serialize_entry("identity", &self.node.snapshot.identity)?;
        entry.serialize_entry("allocated_bytes", &self.node.metrics.allocated_bytes)?;
        entry.serialize_entry("reclaimable_bytes", &self.node.metrics.reclaimable_bytes)?;
        entry.serialize_entry("apparent_bytes", &self.node.metrics.apparent_bytes)?;
        entry.serialize_entry("descendants", &self.node.metrics.descendants)?;
        entry.serialize_entry("unscanned_reason", &unscanned_reason)?;
        entry.end()
    }
}

fn write_scan_table_entries(tree: &FileTree, writer: &mut impl Write) -> Result<(), ReportError> {
    let mut stack = vec![tree.total_node().id];
    while let Some(id) = stack.pop() {
        let node = tree.node(id).ok_or_else(|| {
            ReportError::Invariant("report node disappeared during table export".into())
        })?;
        let path = tree.path_for_id(id).ok_or_else(|| {
            ReportError::Invariant("report path disappeared during table export".into())
        })?;
        writeln!(
            writer,
            "{:?}\t{}\t{}\t{}\t{:?}\t{}",
            node.state,
            display_bounds(node.metrics.allocated_bytes),
            display_bounds(node.metrics.reclaimable_bytes),
            node.metrics.apparent_bytes,
            node.kind,
            safe_display_path_text(&path),
        )?;
        stack.extend(node.children.iter().rev().copied());
    }
    Ok(())
}

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
        document.serialize_entry("schema_version", &REPORT_SCHEMA_VERSION)?;
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
        let mut entries = serializer.serialize_seq(Some(self.report.entries.len()))?;
        for result in &self.report.entries {
            entries.serialize_element(&DeletionHistoryEntryRef {
                scan_root: &self.report.scan_root,
                result,
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
                outcome.serialize_entry("detail", message)?;
                outcome
            }
            DeletionEntryOutcome::Failed(message) => {
                let mut outcome = serializer.serialize_map(Some(2))?;
                outcome.serialize_entry("status", "failed")?;
                outcome.serialize_entry("detail", message)?;
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
            schema_version: REPORT_SCHEMA_VERSION,
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
            schema_version: REPORT_SCHEMA_VERSION,
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
        let root = tempfile::tempdir().expect("streamed report root should exist");
        let tree = FileTree::new(
            root.path().to_path_buf(),
            false,
            crate::model::DEFAULT_PROCESS_MIB,
        )
        .expect("streamed report model should fit its process budget");
        let summary = RunSummary::default();
        let mut streamed = Vec::new();
        write_scan_report_json(
            root.path(),
            &tree,
            &summary,
            scan_report_state(&tree, &summary, false),
            &mut streamed,
        )
        .expect("streamed scan report should serialize");
        let streamed_document: Value =
            serde_json::from_slice(&streamed).expect("streamed scan report should parse");
        scan_validator
            .validate(&streamed_document)
            .expect("streamed scan report should satisfy its schema");

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
            }],
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
    fn iterative_exports_handle_a_deep_model_chain() {
        const DEPTH: usize = 4_096;

        let root = tempfile::tempdir().expect("deep model root should exist");
        let source = root.path().join("source");
        std::fs::write(&source, b"x").expect("source fixture should be written");
        let metadata =
            std::fs::symlink_metadata(&source).expect("source metadata should be readable");
        let identity = crate::native_path::identity_for(&source, &metadata)
            .expect("source identity should be readable")
            .expect("source fixture should not be a symbolic link");
        let mut tree = FileTree::new(
            root.path().to_path_buf(),
            false,
            crate::model::DEFAULT_PROCESS_MIB,
        )
        .expect("deep model should fit its process budget");
        let mut deep_path = root.path().to_path_buf();
        for _ in 0..DEPTH {
            deep_path.push("d");
        }
        deep_path.push("leaf");
        tree.add_entry(&metadata, &deep_path, identity)
            .expect("deep model entry should be retained");

        let summary = RunSummary::default();
        let state = scan_report_state(&tree, &summary, false);
        let mut json = Vec::new();
        write_scan_report_json(root.path(), &tree, &summary, state, &mut json)
            .expect("deep JSON export should complete iteratively");
        assert!(!json.is_empty());
        let mut table = Vec::new();
        write_scan_report_table(&tree, &mut table)
            .expect("deep table export should complete iteratively");
        let table = String::from_utf8(table).expect("table export should be UTF-8");
        assert_eq!(table.lines().count(), DEPTH + 3);
    }

    #[test]
    fn unknown_model_bounds_are_uncertain_without_summary_counters() {
        let root = tempfile::tempdir().expect("model root should exist");
        let mut tree = FileTree::new(
            root.path().to_path_buf(),
            false,
            crate::model::DEFAULT_PROCESS_MIB,
        )
        .expect("model should fit its process budget");
        tree.record_unscanned(
            &root.path().join("unreadable"),
            crate::model::UnscannedReason::Metadata("fixture metadata failure".to_string()),
        )
        .expect("unknown entry should be retained");

        assert!(tree.total_node().metrics.allocated_bytes.upper.is_none());
        assert_eq!(
            scan_report_state(&tree, &RunSummary::default(), false),
            ScanReportState::Uncertain,
            "unknown model bounds must not depend on transport counters"
        );
    }
    #[test]
    fn hostile_paths_in_exports_are_marked_and_escaped() {
        use crate::deletion::{DeletionEntryResult, PlannedEntry, PlannedSnapshot};

        let parent = tempfile::tempdir().expect("report parent should exist");
        let root = parent.path().join("scan-\u{202e}-root");
        std::fs::create_dir(&root).expect("report root should be created");
        // Keep the hostile path synthetic: Windows rejects ESC in filesystem names.
        let fixture_path = root.join("entry-fixture");
        std::fs::write(&fixture_path, b"payload").expect("report entry should be written");
        let metadata =
            std::fs::symlink_metadata(&fixture_path).expect("report metadata should exist");
        let entry_identity = crate::native_path::identity_for(&fixture_path, &metadata)
            .expect("report identity should be readable")
            .expect("report entry should not be a link");
        let hostile_path = root.join("entry-\u{1b}[31m");
        let mut tree = FileTree::new(root.clone(), false, crate::model::DEFAULT_PROCESS_MIB)
            .expect("report model should be created");
        tree.add_entry(&metadata, &hostile_path, entry_identity.clone())
            .expect("hostile report entry should be added");
        tree.complete_directory(&root)
            .expect("report root should complete");
        tree.finalize().expect("report model should finalize");

        let summary = RunSummary::default();
        let mut encoded = Vec::new();
        write_scan_report_json(
            &root,
            &tree,
            &summary,
            scan_report_state(&tree, &summary, false),
            &mut encoded,
        )
        .expect("hostile scan report should serialize");
        let document: Value = serde_json::from_slice(&encoded).expect("scan report should parse");
        let display_root = document["display_root"]
            .as_str()
            .expect("scan display root should be a string");
        assert!(display_root.contains(DECEPTIVE_DISPLAY_MARKER));
        assert!(display_root.contains("\\u{202e}"));
        let entry = document["entries"]
            .as_array()
            .and_then(|entries| {
                entries.iter().find(|entry| {
                    entry["display_path"]
                        .as_str()
                        .is_some_and(|path| path.contains("entry-"))
                })
            })
            .expect("hostile scan entry should be reported");
        let display_path = entry["display_path"]
            .as_str()
            .expect("scan display path should be a string");
        assert!(display_path.contains(DECEPTIVE_DISPLAY_MARKER));
        assert!(display_path.contains("\\x1b"));
        assert!(!display_path.contains('\u{1b}'));

        let mut table = Vec::new();
        write_scan_report_table(&tree, &mut table).expect("hostile scan table should serialize");
        let table = String::from_utf8(table).expect("scan table should be UTF-8");
        assert!(table.contains(DECEPTIVE_DISPLAY_MARKER));
        assert!(table.contains("\\x1b"));

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
            }],
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
}
