//! Tests of the scan-report reader: the two versions it reads, the choice between them, and the
//! typed values that come out.
//!
//! Every report is built by hand and written to a file of its own, member by member in the order
//! the product writes them, because the version is read from the start of the file. The paths use
//! the `utf8` encoding, which every platform reads.

use std::{
    collections::BTreeSet,
    fs,
    io::{self, Read},
    path::PathBuf,
};

use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use tempfile::TempDir;

use crate::fixture::spec::hex;

use super::{
    DocumentError, EntryKind, EntryState, FileId, Identity, LEGACY_SCAN_REPORT_SCHEMA,
    LEGACY_SCAN_REPORT_VERSION, SCAN_REPORT_SCHEMA, SCAN_REPORT_VERSION, ScanDocument, ScanState,
    ScanSummary, VERSION_WINDOW, Version, fill, validators, version_in,
};

const ROOT: &str = "/scan/root";

/// The reason that version 1 gives for an entry its memory limit folded.
const REASON: &str = "MemoryAggregation";

/// The SHA-256 of the scan-report schema that each of Excise 1.0.0 through 1.2.4 published.
const LEGACY_SCHEMA_SHA256: &str =
    "2d60bc0ef78d133a2fb029ad2fef80c1298f2b4a5d196b2646a7805009c47725";

// ---------------------------------------------------------------------------------------------
// Reports.

/// The members of a report as JSON text, in the order the product writes them. A test changes
/// some of them, and `render` writes them out.
type Members = Vec<(&'static str, String)>;

/// A native path in the encoding that every platform reads.
fn native(text: &str) -> Value {
    json!({ "encoding": "utf8", "data": text })
}

/// One entry, with the bounds, sizes, and counts that every entry has.
fn entry(path: &str, kind: Value, state: &str, reason: Value) -> Value {
    let mut object = json!({
        "path": native(path),
        "display_path": path,
        "state": state,
        "identity": null,
        "allocated_bytes": { "lower": 4096, "upper": 4096 },
        "reclaimable_bytes": { "lower": 4096, "upper": 4096 },
        "apparent_bytes": 1000,
        "descendants": 0
    });
    object["kind"] = kind;
    object["unscanned_reason"] = reason;
    object
}

/// The accounting definition that every report carries.
fn accounting() -> Value {
    json!({
        "headline": "identity-unique-allocated-bytes",
        "hard_links_deduplicated": true,
        "shared_extents_deduplicated": false,
        "directory_metadata_included": false
    })
}

/// A run summary, with `store` for the members that name the scan store, which differ by version.
fn summary(store: &Value) -> Value {
    let mut object = json!({
        "scanned_entries": 10,
        "identified_entries": 9,
        "unreadable_entries": 8,
        "unscanned_entries": 7,
        "excluded_entries": 6,
        "filesystem_boundaries": 5,
        "link_entries": 4,
        "deleted_entries": 3,
        "deletion_changed_entries": 2,
        "deletion_missing_entries": 1,
        "deletion_failed_entries": 11,
        "deletion_unattempted_entries": 12,
        "last_unreadable_path": "/scan/root/locked",
        "last_unscanned_path": "/scan/root/skipped",
        "last_unscanned_reason": "excluded",
        "last_worker_error": "worker failed"
    });
    if let (Some(into), Some(from)) = (object.as_object_mut(), store.as_object()) {
        for (name, value) in from {
            into.insert(name.clone(), value.clone());
        }
    }
    object
}

/// A run summary as version 1 writes it.
fn version_1_summary() -> Value {
    summary(&json!({
        "model_bytes": 123_456,
        "model_limit_bytes": 654_321,
        "identity_spilled": true
    }))
}

/// A run summary as version 3 writes it.
fn version_3_summary() -> Value {
    summary(&json!({
        "scan_store_bytes": 123_456,
        "scan_store_limit_bytes": 654_321
    }))
}

/// What either run summary reads to.
fn expected_summary() -> ScanSummary {
    ScanSummary {
        scanned_entries: 10,
        identified_entries: 9,
        unreadable_entries: 8,
        unscanned_entries: 7,
        excluded_entries: 6,
        filesystem_boundaries: 5,
        link_entries: 4,
        deleted_entries: 3,
        deletion_changed_entries: 2,
        deletion_missing_entries: 1,
        deletion_failed_entries: 11,
        deletion_unattempted_entries: 12,
        scan_store_bytes: 123_456,
        scan_store_limit_bytes: 654_321,
        last_unreadable_path: Some("/scan/root/locked".to_owned()),
        last_unscanned_path: Some("/scan/root/skipped".to_owned()),
        last_unscanned_reason: Some("excluded".to_owned()),
        last_worker_error: Some("worker failed".to_owned()),
    }
}

/// The root, a folder, and a file: entries that both versions write alike.
fn plain_entries() -> Vec<Value> {
    let mut root = entry(ROOT, json!("root"), "complete", Value::Null);
    root["identity"] = json!({
        "file_id": { "inode": { "device": 7, "inode": 11 } },
        "link_count": 2,
        "reparse_point": false
    });
    let directory = json!("directory");
    let file = json!("file");
    vec![
        root,
        entry("/scan/root/docs", directory, "complete", Value::Null),
        entry("/scan/root/docs/notes.txt", file, "complete", Value::Null),
    ]
}

/// The entries that only version 1 writes: a folder that is `aggregated`, and the two synthetic
/// kinds that stand for entries the scan's memory limit folded together.
fn folded_entries() -> Vec<Value> {
    let reason = || json!(REASON);
    let directory = json!("directory");
    let aggregate = json!({ "synthetic": "aggregate" });
    let other = json!({ "synthetic": "other" });
    vec![
        entry("/scan/root/big", directory, "aggregated", reason()),
        entry("/scan/root/cold", aggregate, "aggregated", reason()),
        entry("/scan/root/Other", other, "aggregated", reason()),
    ]
}

/// The members of a report: `document_kind` and `schema_version` first and `entries` last, as the
/// product writes them. `version` is the text of the `schema_version` value.
fn members(version: &str, state: &str, run_summary: &Value, entries: &[Value]) -> Members {
    vec![
        ("document_kind", json!("scan-report").to_string()),
        ("schema_version", version.to_owned()),
        ("root", native(ROOT).to_string()),
        ("display_root", json!(ROOT).to_string()),
        ("state", json!(state).to_string()),
        ("accounting", accounting().to_string()),
        ("summary", run_summary.to_string()),
        ("entries", Value::Array(entries.to_vec()).to_string()),
    ]
}

/// A report of version 1 with every kind of entry that only version 1 writes.
fn version_1() -> Members {
    let mut entries = plain_entries();
    entries.extend(folded_entries());
    members("1", "uncertain", &version_1_summary(), &entries)
}

/// A report of version 3 with its one synthetic entry.
fn version_3() -> Members {
    let mut entries = plain_entries();
    let shared = json!({ "synthetic": "shared" });
    entries.push(entry("/scan/root/Shared", shared, "complete", Value::Null));
    members("3", "exact", &version_3_summary(), &entries)
}

/// Replaces the text of the member called `name`.
fn set(members: &mut [(&'static str, String)], name: &str, value: &str) {
    let slot = members
        .iter_mut()
        .find(|candidate| candidate.0 == name)
        .expect("the report has the member");
    slot.1 = value.to_owned();
}

/// Moves `schema_version` behind a `display_root` as long as the window that is searched for it,
/// which puts it out of the window's reach.
fn with_the_version_past_the_window(mut report: Members) -> Members {
    let version = report.remove(1);
    report.insert(3, version);
    let long_name = Value::String("x".repeat(VERSION_WINDOW)).to_string();
    set(&mut report, "display_root", &long_name);
    report
}

/// A text that is no report, with the member `"schema_version": 1,` ending `extra` bytes after
/// the end of the window that is searched for it.
fn padded_to_the_window(extra: usize) -> String {
    let head = "{\"pad\": \"";
    let gap = "\", ";
    let member = "\"schema_version\": 1,";
    let filler = VERSION_WINDOW - head.len() - gap.len() - member.len() + extra;
    format!("{head}{}{gap}{member} \"rest\": 0}}", "x".repeat(filler))
}

/// The text of a report: one member to a line, in the order given.
fn render(members: &[(&str, String)]) -> String {
    let lines: Vec<String> = members
        .iter()
        .map(|(name, value)| format!("  \"{name}\": {value}"))
        .collect();
    format!("{{\n{}\n}}\n", lines.join(",\n"))
}

/// Writes `text` to a file of its own in `directory`.
fn written(directory: &TempDir, text: &str) -> PathBuf {
    let path = directory.path().join("scan-report.json");
    fs::write(&path, text).expect("a report is written");
    path
}

/// Writes the report to a file and reads it with [`ScanDocument::read`].
fn read(members: &[(&str, String)]) -> Result<ScanDocument, DocumentError> {
    let directory = tempfile::tempdir().expect("a temporary directory");
    ScanDocument::read(&written(&directory, &render(members)))
}

/// Writes the report to a file and asks [`ScanDocument::version_of`] for its version.
fn declared_by(members: &[(&str, String)]) -> Result<u32, DocumentError> {
    let directory = tempfile::tempdir().expect("a temporary directory");
    ScanDocument::version_of(&written(&directory, &render(members)))
}

/// The violations that the schema refuses a report with.
fn violations_of(members: &[(&str, String)]) -> Vec<String> {
    match read(members) {
        Err(DocumentError::Violations { violations, .. }) => violations,
        Err(error) => panic!("the schema should refuse the report, but it fails with: {error}"),
        Ok(document) => panic!("the schema should refuse the report, but it reads: {document:?}"),
    }
}

// ---------------------------------------------------------------------------------------------
// The two versions read into the same typed values.

#[test]
fn a_version_1_report_reads_into_the_typed_values() {
    let document = read(&version_1()).expect("a version-1 report reads");

    assert_eq!(document.root, ROOT.as_bytes());
    assert_eq!(document.state, ScanState::Uncertain);
    assert_eq!(document.summary, expected_summary());
    // `model_bytes` and `model_limit_bytes` read into the scan-store fields.
    assert_eq!(document.summary.scan_store_bytes, 123_456);
    assert_eq!(document.summary.scan_store_limit_bytes, 654_321);
    let reported: Vec<_> = document
        .entries
        .iter()
        .map(|item| (item.kind, item.state))
        .collect();
    assert_eq!(
        reported,
        [
            (EntryKind::Root, EntryState::Complete),
            (EntryKind::Directory, EntryState::Complete),
            (EntryKind::File, EntryState::Complete),
            (EntryKind::Directory, EntryState::Aggregated),
            (EntryKind::Aggregate, EntryState::Aggregated),
            (EntryKind::Other, EntryState::Aggregated),
        ]
    );
    assert_eq!(document.entries[0].unscanned_reason, None);
    for folded in &document.entries[3..] {
        assert_eq!(folded.unscanned_reason.as_deref(), Some(REASON));
    }
    assert_eq!(
        document.entries[0].identity,
        Some(Identity {
            file_id: FileId::Inode {
                device: 7,
                inode: 11,
            },
            link_count: Some(2),
            reparse_point: false,
        })
    );
}

#[test]
fn a_version_1_report_and_a_version_3_report_of_the_same_scan_read_to_the_same_document() {
    let before = members("1", "exact", &version_1_summary(), &plain_entries());
    let after = members("3", "exact", &version_3_summary(), &plain_entries());

    let old = read(&before).expect("a version-1 report reads");
    let new = read(&after).expect("a version-3 report reads");

    assert_eq!(old, new);
    assert_eq!(new.summary, expected_summary());
    assert_eq!(new.entries.len(), 3);
}

#[test]
fn a_version_3_report_still_reads() {
    let document = read(&version_3()).expect("a version-3 report reads");

    assert_eq!(document.state, ScanState::Exact);
    assert_eq!(document.summary, expected_summary());
    let last = document.entries.last().expect("the report has entries");
    assert_eq!(last.kind, EntryKind::Shared);
    assert_eq!(last.state, EntryState::Complete);
    for (text, state) in [
        ("exact", ScanState::Exact),
        ("uncertain", ScanState::Uncertain),
        ("summary-only", ScanState::SummaryOnly),
        ("cancelled", ScanState::Cancelled),
    ] {
        let mut report = version_3();
        set(&mut report, "state", &json!(text).to_string());
        let read_state = read(&report).expect("the state reads").state;
        assert_eq!(read_state, state, "{text}");
    }
}

// ---------------------------------------------------------------------------------------------
// The schema of each version is the gate.

#[test]
fn an_undeclared_member_breaks_a_version_1_report_at_the_top_and_in_an_entry() {
    let mut at_the_top = version_1();
    at_the_top.push(("surprise", "true".to_owned()));
    assert!(!violations_of(&at_the_top).is_empty());

    let mut entries = plain_entries();
    entries[1]["surprise"] = json!(true);
    let in_an_entry = members("1", "exact", &version_1_summary(), &entries);
    let violations = violations_of(&in_an_entry);
    assert!(
        violations
            .iter()
            .any(|violation| violation.starts_with("entries[1]: ")),
        "{violations:?}"
    );
}

#[test]
fn values_only_version_1_has_break_a_report_that_declares_version_3() {
    let mut entries = plain_entries();
    entries.extend(folded_entries());
    let folded = members("3", "exact", &version_3_summary(), &entries);
    let violations = violations_of(&folded);
    for index in [3, 4, 5] {
        let prefix = format!("entries[{index}]: ");
        assert!(
            violations
                .iter()
                .any(|violation| violation.starts_with(&prefix)),
            "{prefix}{violations:?}"
        );
    }

    // The names of version 1 in a summary cannot slip through the aliases of the typed values.
    let plain = plain_entries();
    let renamed = members("3", "exact", &version_1_summary(), &plain);
    assert!(!violations_of(&renamed).is_empty());
}

#[test]
fn values_only_version_3_has_break_a_report_that_declares_version_1() {
    let entries = plain_entries();
    let summary_only = members("1", "summary-only", &version_1_summary(), &entries);
    assert!(!violations_of(&summary_only).is_empty());

    let renamed = members("1", "exact", &version_3_summary(), &entries);
    assert!(!violations_of(&renamed).is_empty());
}

#[test]
fn a_report_that_declares_no_version_is_held_to_version_3() {
    let mut report = version_3();
    let removed = report.remove(1);
    assert_eq!(removed.0, "schema_version");
    let violations = violations_of(&report);
    assert!(
        violations
            .iter()
            .any(|violation| violation.contains("schema_version")),
        "{violations:?}"
    );

    let directory = tempfile::tempdir().expect("a temporary directory");
    let path = written(&directory, r#"{"version": 3}"#);
    assert!(matches!(
        ScanDocument::read(&path),
        Err(DocumentError::Violations { .. })
    ));
}

#[test]
fn a_report_that_declares_another_version_is_refused_before_anything_else() {
    for declared in [0_u64, 2, 4, u64::MAX] {
        let mut report = version_3();
        set(&mut report, "schema_version", &declared.to_string());

        let error = read(&report).expect_err("another version is refused");

        assert!(
            matches!(error, DocumentError::UnsupportedVersion { version } if version == declared),
            "{declared}: {error}"
        );
    }

    let directory = tempfile::tempdir().expect("a temporary directory");
    let path = written(&directory, r#"{"schema_version": 2}"#);
    let error = ScanDocument::read(&path).expect_err("version 2 is refused");
    assert_eq!(
        error.to_string(),
        "the report declares schema_version 2, but only versions 1 and 3 are read"
    );
}

#[test]
fn a_report_that_is_not_json_fails_as_json_whatever_it_declares() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    for text in [
        "",
        "not json",
        r#"{"schema_version": 1, "root""#,
        r#"{"schema_version": 3, "root""#,
    ] {
        let path = written(&directory, text);
        assert!(
            matches!(ScanDocument::read(&path), Err(DocumentError::Json(_))),
            "{text}"
        );
    }
}

#[test]
fn a_missing_report_is_an_io_error_for_read_and_for_version_of() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let missing = directory.path().join("absent.json");

    assert!(matches!(
        ScanDocument::read(&missing),
        Err(DocumentError::Io { .. })
    ));
    assert!(matches!(
        ScanDocument::version_of(&missing),
        Err(DocumentError::Io { .. })
    ));
}

// ---------------------------------------------------------------------------------------------
// Finding the version in the first 4,096 bytes.

#[test]
fn version_of_says_which_version_a_report_declares() {
    let old = declared_by(&version_1()).expect("version 1");
    let new = declared_by(&version_3()).expect("version 3");
    assert_eq!(old, LEGACY_SCAN_REPORT_VERSION);
    assert_eq!(new, SCAN_REPORT_VERSION);

    let mut other = version_3();
    set(&mut other, "schema_version", "2");
    assert!(matches!(
        declared_by(&other),
        Err(DocumentError::UnsupportedVersion { version: 2 })
    ));

    let mut quoted = version_3();
    set(&mut quoted, "schema_version", "\"3\"");
    let error = declared_by(&quoted).expect_err("a string is no version");
    assert!(matches!(error, DocumentError::NoVersion), "{error}");
    assert_eq!(
        error.to_string(),
        "the report declares no schema_version, as an unsigned integer, in its first 4096 bytes"
    );
}

#[test]
fn white_space_around_the_colon_does_not_hide_the_version() {
    let original = render(&version_1());
    let compact = "\"schema_version\": 1";
    let spread = "\"schema_version\"\r\n\t :\n   1";
    let spaced = original.replace(compact, spread);
    assert_ne!(spaced, original, "the version was rewritten");
    let directory = tempfile::tempdir().expect("a temporary directory");
    let path = written(&directory, &spaced);

    assert_eq!(ScanDocument::version_of(&path).expect("a version"), 1);
    ScanDocument::read(&path).expect("a version-1 report reads");
}

#[test]
fn a_version_past_the_first_4096_bytes_is_not_seen_and_the_report_is_held_to_version_3() {
    let report = with_the_version_past_the_window(version_3());
    assert!(
        render(&report)
            .find("\"schema_version\"")
            .is_some_and(|at| at >= VERSION_WINDOW),
        "the version must lie past the window"
    );

    read(&report).expect("a version-3 report reads wherever its version lies");
    let error = declared_by(&report).expect_err("the version is out of sight");
    assert!(matches!(error, DocumentError::NoVersion), "{error}");

    // Held to version 3, a report of version 1 breaks it at `schema_version` itself.
    let violations = violations_of(&with_the_version_past_the_window(version_1()));
    assert!(
        violations
            .iter()
            .any(|violation| violation.contains("/schema_version")),
        "{violations:?}"
    );
}

#[test]
fn the_window_ends_at_byte_4096() {
    assert_eq!(VERSION_WINDOW, 4096, "the documentation says 4,096 bytes");
    let directory = tempfile::tempdir().expect("a temporary directory");

    // The comma that ends the number is the last byte of the window.
    let inside = written(&directory, &padded_to_the_window(0));
    assert_eq!(ScanDocument::version_of(&inside).expect("a version"), 1);

    // One byte later the number runs to the end of the window, and may go on.
    let outside = written(&directory, &padded_to_the_window(1));
    assert!(matches!(
        ScanDocument::version_of(&outside),
        Err(DocumentError::NoVersion)
    ));
}

#[test]
fn the_version_is_the_unsigned_integer_of_the_first_schema_version_member() {
    for (text, expected) in [
        (r#"{"schema_version": 3, "root": {}}"#, Some(3)),
        (r#"{"schema_version":1}"#, Some(1)),
        (r#"{"schema_version": 0}"#, Some(0)),
        ("{\n  \"schema_version\"\r\n\t:\n  3\n}", Some(3)),
        (
            r#"{"display_root": "schema_version", "schema_version": 1}"#,
            Some(1),
        ),
        (
            r#"{"display_root": "say \"schema_version\": 7", "schema_version": 1}"#,
            Some(1),
        ),
        (
            r#"{"schema_version": 3, "other": {"schema_version": 1}}"#,
            Some(3),
        ),
    ] {
        assert_eq!(version_in(text.as_bytes()), expected, "{text}");
    }

    let largest = format!("{{\"schema_version\": {}}}", u64::MAX);
    assert_eq!(version_in(largest.as_bytes()), Some(u64::MAX));
}

#[test]
fn a_schema_version_that_is_no_unsigned_integer_is_no_version() {
    for text in [
        "",
        "{}",
        r#"{"version": 3}"#,
        r#"{"schema_version": "3"}"#,
        r#"{"schema_version": -3}"#,
        r#"{"schema_version": 3.0}"#,
        r#"{"schema_version": 3e0}"#,
        r#"{"schema_version": 03}"#,
        r#"{"schema_version": 3x}"#,
        r#"{"schema_version": 18446744073709551616}"#,
        r#"{"schema_version": null}"#,
        r#"{"schema_version": }"#,
        r#"{"schema_version" 3}"#,
        // The number may go on past the end of what was read.
        r#"{"schema_version": 3"#,
        // So may the name.
        r#"{"schema_version"#,
        // The first member decides, even when its value is no number.
        r#"{"schema_version": "x", "schema_version": 3}"#,
    ] {
        assert_eq!(version_in(text.as_bytes()), None, "{text}");
    }
}

/// A reader that fails once with `Interrupted` and then hands out one byte per call.
struct Trickle<'a> {
    rest: &'a [u8],
    interrupted: bool,
}

impl Read for Trickle<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if !self.interrupted {
            self.interrupted = true;
            return Err(io::ErrorKind::Interrupted.into());
        }
        let Some((first, rest)) = self.rest.split_first() else {
            return Ok(0);
        };
        let Some(slot) = buffer.first_mut() else {
            return Ok(0);
        };
        *slot = *first;
        self.rest = rest;
        Ok(1)
    }
}

#[test]
fn the_start_of_a_report_is_filled_across_short_and_interrupted_reads() {
    let mut window = [0_u8; 8];
    let mut long = Trickle {
        rest: b"abcdefgh12345",
        interrupted: false,
    };

    assert_eq!(fill(&mut long, &mut window).expect("the reader works"), 8);
    assert_eq!(&window, b"abcdefgh");

    let mut window = [0_u8; 8];
    let mut short = Trickle {
        rest: b"abc",
        interrupted: false,
    };

    assert_eq!(fill(&mut short, &mut window).expect("the reader works"), 3);
    assert_eq!(&window[..3], b"abc");
}

// ---------------------------------------------------------------------------------------------
// The published schemas and the typed values agree.

#[test]
fn the_legacy_schema_is_the_file_every_version_1_release_published() {
    let digest = hex(&Sha256::digest(LEGACY_SCAN_REPORT_SCHEMA.as_bytes()));

    assert_eq!(
        digest, LEGACY_SCHEMA_SHA256,
        "legacy/scan-report-v1.schema.json is a copy of what v1.0.0 through v1.2.4 published"
    );
}

#[test]
fn each_version_constant_is_the_version_its_schema_declares() {
    for (schema, version) in [
        (SCAN_REPORT_SCHEMA, SCAN_REPORT_VERSION),
        (LEGACY_SCAN_REPORT_SCHEMA, LEGACY_SCAN_REPORT_VERSION),
    ] {
        let schema: Value = serde_json::from_str(schema).expect("the schema is JSON");

        assert_eq!(
            schema.pointer("/properties/schema_version/const"),
            Some(&json!(version))
        );
    }
}

#[test]
fn both_published_schemas_compile_with_their_entry_definitions() {
    for version in Version::ALL {
        validators(version).expect("the published schema compiles");
    }
}

/// The names that `pointer` finds in the two published schemas together.
fn declared(pointer: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for schema in [SCAN_REPORT_SCHEMA, LEGACY_SCAN_REPORT_SCHEMA] {
        let schema: Value = serde_json::from_str(schema).expect("the schema is JSON");
        let listed = schema
            .pointer(pointer)
            .and_then(Value::as_array)
            .expect("the schema lists values there");
        for item in listed {
            found.insert(item.as_str().expect("a name").to_owned());
        }
    }
    found
}

/// The wire names of every variant in `all`.
fn names<T: Copy>(all: &[T], name: fn(T) -> &'static str) -> BTreeSet<String> {
    all.iter()
        .map(|variant| name(*variant).to_owned())
        .collect()
}

#[test]
fn the_states_name_every_value_that_either_schema_allows_and_no_other() {
    assert_eq!(
        names(ScanState::ALL, ScanState::as_str),
        declared("/$defs/scan_state/enum")
    );
    assert_eq!(
        names(EntryState::ALL, EntryState::as_str),
        declared("/$defs/node_state/enum")
    );
}

#[test]
fn the_entry_kinds_read_every_value_that_either_schema_allows() {
    let plain = declared("/$defs/node_kind/oneOf/0/enum");
    let synthetic = declared("/$defs/node_kind/oneOf/1/properties/synthetic/enum");
    let mut read_kinds = BTreeSet::new();
    for name in &plain {
        let kind: EntryKind = serde_json::from_value(json!(name)).expect("a kind");
        assert_eq!(kind.as_str(), name);
        read_kinds.insert(kind);
    }
    for name in &synthetic {
        let wrapped = json!({ "synthetic": name });
        let kind: EntryKind = serde_json::from_value(wrapped).expect("a kind");
        assert_eq!(kind.as_str(), format!("synthetic {name}"));
        read_kinds.insert(kind);
    }

    let expected = plain.len() + synthetic.len();
    assert_eq!(read_kinds.len(), expected, "one kind per name");
}
