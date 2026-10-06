//! What a headless scan's report says, read without keeping the report.
//!
//! The report of a scan of a real tree can be large (every entry repeats its path), and the soak
//! wants three things from it: `state`, `accounting`, and `summary`. They are read with the rest of
//! the document skipped value by value, so what is held is what is kept, whatever the size of the
//! tree. Nothing in it that names the tree is kept in the document the soak writes: the four text
//! fields of `summary` (`last_unreadable_path` and the others) can be paths, so the document only
//! records whether each held anything. The text itself goes to the local quirk log.
//!
//! Skipping is still reading: a multi-gigabyte report takes a while to get through, and the run is
//! bounded and can be interrupted. So the report is read through a reader that looks at the end of
//! the run and at the person's interrupt before each chunk it takes, and stops with
//! [`FactsError::Cancelled`] when either says so.
//!
//! The report is a file that the program under test wrote, so what it left at the report's path is
//! not taken to be a file. It is opened as a regular file that is not a link, and without waiting
//! ([`open_regular_file`]): opening a FIFO for reading waits for a writer that may never come, and
//! neither the bound nor the interrupt could end that wait. Anything else is a report that cannot
//! be read.

use std::{
    cell::Cell,
    io::{self, BufReader, Read},
    path::Path,
    time::Instant,
};

use serde::Deserialize;
use thiserror::Error;

use crate::{
    fixture::sys::open_regular_file,
    headless::document::{SCAN_REPORT_VERSION, ScanState, ScanSummary},
    report::{AccountingHeadline, SoakAccounting, SoakReportFacts, SoakScanSummary},
};

use super::Interrupt;

/// The `document_kind` of a scan report.
const SCAN_REPORT_KIND: &str = "scan-report";

/// How much of the report is read between two looks at the clock and the interrupt.
const CHUNK: usize = 64 * 1024;

/// Why reading a report stopped before its end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum Cancelled {
    /// The person interrupted the run.
    #[error("the run was interrupted")]
    Interrupted,
    /// The end of the run's bound passed.
    #[error("the bound on the run passed")]
    TimedOut,
}

/// A report could not be read.
#[derive(Debug, Error)]
pub enum FactsError {
    /// The report cannot be opened, or is not a regular file (a FIFO, a link, a folder, a device).
    #[error("cannot open the scan report: {0}")]
    Open(std::io::Error),
    /// The report is not JSON, or lacks `state`, `accounting`, or `summary`.
    #[error("cannot read the scan report: {0}")]
    Json(serde_json::Error),
    /// The run was interrupted, or its bound passed, before the whole report had been read.
    #[error("reading the scan report was stopped: {0}")]
    Cancelled(Cancelled),
    /// The document is not a scan report of the version this harness reads.
    #[error(
        "the document is `{kind}` version {version}, not a scan report of version {SCAN_REPORT_VERSION}"
    )]
    NotAReport {
        /// The `document_kind` it says.
        kind: String,
        /// The `schema_version` it says.
        version: u32,
    },
    /// The report states accounting that is not the contract's headline.
    #[error("the report's accounting headline is `{0}`, not the contract's")]
    Headline(String),
}

/// What the report says in text. Any of it can be a path, so it is for the local quirk log and
/// never for the document that is shared.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReportText {
    /// The last folder the scanner could not read.
    pub unreadable_path: Option<String>,
    /// The last path the scanner did not enter.
    pub unscanned_path: Option<String>,
    /// Why it did not enter it.
    pub unscanned_reason: Option<String>,
    /// The last worker error.
    pub worker_error: Option<String>,
}

/// What a report said: the facts the document keeps, and the text that stays local.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanFacts {
    /// The state, the accounting, and the counts.
    pub facts: SoakReportFacts,
    /// The text fields of `summary`.
    pub text: ReportText,
}

#[derive(Deserialize)]
struct Raw {
    document_kind: String,
    schema_version: u32,
    state: ScanState,
    accounting: RawAccounting,
    summary: ScanSummary,
}

#[derive(Deserialize)]
struct RawAccounting {
    headline: String,
    hard_links_deduplicated: bool,
    shared_extents_deduplicated: bool,
    directory_metadata_included: bool,
}

/// A reader that stops being read when the run's bound has passed or the person has interrupted
/// the run, and says which.
struct Bounded<'a, R> {
    inner: R,
    deadline: Instant,
    interrupt: &'a Interrupt,
    /// Why the last read refused, for the caller that only sees an I/O error.
    cancelled: &'a Cell<Option<Cancelled>>,
}

impl<R: Read> Read for Bounded<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        // Neither error is `Interrupted`: a reader that is asked again after that kind of error
        // is asked again forever, and the flag stays set.
        if self.interrupt.is_set() {
            self.cancelled.set(Some(Cancelled::Interrupted));
            return Err(io::Error::other(Cancelled::Interrupted));
        }
        if Instant::now() >= self.deadline {
            self.cancelled.set(Some(Cancelled::TimedOut));
            return Err(io::Error::other(Cancelled::TimedOut));
        }
        self.inner.read(buffer)
    }
}

/// Reads the facts of the report at `path`. Everything else in it, the entries above all, is
/// skipped as it is met, and the reading stops at `deadline` or when `interrupt` is set. The path
/// must be a regular file: a FIFO or a device there is refused, and never waited on.
///
/// # Errors
///
/// Returns [`FactsError`] for a report that cannot be opened or is not a regular file, that cannot
/// be parsed, that is not a scan report of the version this harness reads, or whose accounting is
/// not the contract's, and [`FactsError::Cancelled`] when the run's bound or the person's interrupt
/// stopped the reading.
pub fn read_facts(
    path: &Path,
    deadline: Instant,
    interrupt: &Interrupt,
) -> Result<ScanFacts, FactsError> {
    let file = open_regular_file(path).map_err(FactsError::Open)?;
    facts_of(file, deadline, interrupt)
}

/// [`read_facts`] of whatever `report` yields.
fn facts_of(
    report: impl Read,
    deadline: Instant,
    interrupt: &Interrupt,
) -> Result<ScanFacts, FactsError> {
    let cancelled = Cell::new(None);
    let bounded = Bounded {
        inner: report,
        deadline,
        interrupt,
        cancelled: &cancelled,
    };
    let raw: Raw =
        serde_json::from_reader(BufReader::with_capacity(CHUNK, bounded)).map_err(|error| {
            cancelled
                .get()
                .map_or(FactsError::Json(error), FactsError::Cancelled)
        })?;
    if raw.document_kind != SCAN_REPORT_KIND || raw.schema_version != SCAN_REPORT_VERSION {
        return Err(FactsError::NotAReport {
            kind: raw.document_kind,
            version: raw.schema_version,
        });
    }
    if raw.accounting.headline != AccountingHeadline::IdentityUniqueAllocatedBytes.as_str() {
        return Err(FactsError::Headline(raw.accounting.headline));
    }
    let summary = &raw.summary;
    Ok(ScanFacts {
        facts: SoakReportFacts {
            state: raw.state,
            accounting: SoakAccounting {
                headline: AccountingHeadline::IdentityUniqueAllocatedBytes,
                hard_links_deduplicated: raw.accounting.hard_links_deduplicated,
                shared_extents_deduplicated: raw.accounting.shared_extents_deduplicated,
                directory_metadata_included: raw.accounting.directory_metadata_included,
            },
            summary: SoakScanSummary {
                scanned_entries: summary.scanned_entries,
                identified_entries: summary.identified_entries,
                unreadable_entries: summary.unreadable_entries,
                unscanned_entries: summary.unscanned_entries,
                excluded_entries: summary.excluded_entries,
                filesystem_boundaries: summary.filesystem_boundaries,
                link_entries: summary.link_entries,
                deleted_entries: summary.deleted_entries,
                deletion_changed_entries: summary.deletion_changed_entries,
                deletion_missing_entries: summary.deletion_missing_entries,
                deletion_failed_entries: summary.deletion_failed_entries,
                deletion_unattempted_entries: summary.deletion_unattempted_entries,
                scan_store_bytes: summary.scan_store_bytes,
                scan_store_limit_bytes: summary.scan_store_limit_bytes,
                last_unreadable_path_present: summary.last_unreadable_path.is_some(),
                last_unscanned_path_present: summary.last_unscanned_path.is_some(),
                last_unscanned_reason_present: summary.last_unscanned_reason.is_some(),
                last_worker_error_present: summary.last_worker_error.is_some(),
            },
        },
        text: ReportText {
            unreadable_path: raw.summary.last_unreadable_path,
            unscanned_path: raw.summary.last_unscanned_path,
            unscanned_reason: raw.summary.last_unscanned_reason,
            worker_error: raw.summary.last_worker_error,
        },
    })
}

#[cfg(test)]
mod tests {
    use std::{fmt::Write as _, fs, time::Duration};

    use super::*;

    /// A bound that nothing in a test reaches.
    fn later() -> Instant {
        Instant::now() + Duration::from_secs(3600)
    }

    const SUMMARY: &str = r#"{
        "scanned_entries": 12, "identified_entries": 11, "unreadable_entries": 2,
        "unscanned_entries": 3, "excluded_entries": 0, "filesystem_boundaries": 1,
        "link_entries": 4, "deleted_entries": 0, "deletion_changed_entries": 0,
        "deletion_missing_entries": 0, "deletion_failed_entries": 0,
        "deletion_unattempted_entries": 0, "scan_store_bytes": 4096,
        "scan_store_limit_bytes": 1048576,
        "last_unreadable_path": "/Users/someone/Private/secret-folder",
        "last_unscanned_path": null, "last_unscanned_reason": "permission denied",
        "last_worker_error": null
    }"#;

    fn report(entries: &str, headline: &str, version: u32) -> String {
        format!(
            r#"{{"document_kind": "scan-report", "schema_version": {version},
            "root": {{"unix": "/a"}}, "display_root": "/a", "state": "uncertain",
            "accounting": {{"headline": "{headline}", "hard_links_deduplicated": true,
                "shared_extents_deduplicated": false, "directory_metadata_included": false}},
            "summary": {SUMMARY},
            "entries": [{entries}]}}"#
        )
    }

    fn read(text: &str) -> Result<ScanFacts, FactsError> {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("report.json");
        fs::write(&path, text).expect("a report");
        read_facts(&path, later(), &Interrupt::new())
    }

    #[test]
    fn the_state_the_accounting_and_the_counts_are_read_and_the_text_is_kept_apart() {
        let entries = r#"{"path": {"unix": "/a/very-distinctive-name"}, "kind": "file"}"#;

        let read = read(&report(entries, "identity-unique-allocated-bytes", 3)).expect("facts");

        assert_eq!(read.facts.state, ScanState::Uncertain);
        assert!(read.facts.accounting.hard_links_deduplicated);
        assert!(!read.facts.accounting.shared_extents_deduplicated);
        assert_eq!(read.facts.summary.scanned_entries, 12);
        assert_eq!(read.facts.summary.unreadable_entries, 2);
        assert_eq!(read.facts.summary.scan_store_bytes, 4096);
        assert!(read.facts.summary.last_unreadable_path_present);
        assert!(read.facts.summary.last_unscanned_reason_present);
        assert!(!read.facts.summary.last_unscanned_path_present);
        assert!(!read.facts.summary.last_worker_error_present);
        assert_eq!(
            read.text.unreadable_path.as_deref(),
            Some("/Users/someone/Private/secret-folder")
        );
        // What the document keeps names nothing.
        let kept = serde_json::to_string(&read.facts).expect("facts serialize");
        assert!(
            !kept.contains("secret-folder") && !kept.contains("someone"),
            "{kept}"
        );
    }

    #[test]
    fn the_entries_are_skipped_however_many_there_are() {
        let mut entries = String::new();
        for index in 0..20_000 {
            if index > 0 {
                entries.push(',');
            }
            write!(
                entries,
                r#"{{"path": {{"unix": "/a/entry-{index}"}}, "kind": "file", "nested": {{"deep": [1, 2, {{"x": null}}]}}}}"#
            )
            .expect("a string");
        }

        let read = read(&report(&entries, "identity-unique-allocated-bytes", 3)).expect("facts");

        assert_eq!(read.facts.summary.link_entries, 4);
    }

    #[test]
    fn a_document_that_is_not_a_scan_report_of_this_version_is_refused() {
        let wrong_version = read(&report("", "identity-unique-allocated-bytes", 2));
        assert!(
            matches!(
                wrong_version,
                Err(FactsError::NotAReport { version: 2, .. })
            ),
            "{wrong_version:?}"
        );
        let not_a_report = read(r#"{"document_kind": "harness-summary", "schema_version": 3}"#);
        assert!(
            matches!(not_a_report, Err(FactsError::Json(_))),
            "{not_a_report:?}"
        );
        assert!(matches!(read("not json"), Err(FactsError::Json(_))));
        assert!(matches!(read(""), Err(FactsError::Json(_))));
    }

    #[test]
    fn a_headline_the_contract_does_not_name_is_refused() {
        let other = read(&report("", "apparent-bytes", 3));

        assert!(
            matches!(other, Err(FactsError::Headline(headline)) if headline == "apparent-bytes")
        );
    }

    #[test]
    fn a_report_that_is_not_there_cannot_be_opened() {
        let dir = tempfile::tempdir().expect("a temporary directory");

        assert!(matches!(
            read_facts(&dir.path().join("absent.json"), later(), &Interrupt::new()),
            Err(FactsError::Open(_))
        ));
    }

    /// A report with this many entries, as text.
    fn big_report(entries: usize) -> String {
        let mut text = String::new();
        for index in 0..entries {
            if index > 0 {
                text.push(',');
            }
            write!(
                text,
                r#"{{"path": {{"unix": "/a/entry-{index}"}}, "kind": "file"}}"#
            )
            .expect("a string");
        }
        report(&text, "identity-unique-allocated-bytes", 3)
    }

    /// A reader that does something at its Nth read, and then goes on.
    struct AtRead<R, F: FnMut()> {
        inner: R,
        reads: usize,
        at: usize,
        action: F,
    }

    impl<R: Read, F: FnMut()> Read for AtRead<R, F> {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.reads += 1;
            if self.reads == self.at {
                (self.action)();
            }
            self.inner.read(buffer)
        }
    }

    #[test]
    fn a_report_that_is_being_read_when_the_person_interrupts_stops_being_read() {
        let text = big_report(60_000);
        assert!(text.len() > 4 * CHUNK, "the report takes several chunks");
        let interrupt = Interrupt::new();

        let read = facts_of(
            AtRead {
                inner: text.as_bytes(),
                reads: 0,
                at: 2,
                action: || interrupt.trigger(),
            },
            later(),
            &interrupt,
        );

        assert!(
            matches!(read, Err(FactsError::Cancelled(Cancelled::Interrupted))),
            "{read:?}"
        );
    }

    #[test]
    fn a_report_that_is_still_being_read_when_the_bound_passes_stops_being_read() {
        let text = big_report(60_000);
        let deadline = Instant::now() + Duration::from_millis(30);

        // The disk is slow: each chunk takes longer than the whole bound.
        let read = facts_of(
            AtRead {
                inner: text.as_bytes(),
                reads: 0,
                at: 2,
                action: || std::thread::sleep(Duration::from_millis(80)),
            },
            deadline,
            &Interrupt::new(),
        );

        assert!(
            matches!(read, Err(FactsError::Cancelled(Cancelled::TimedOut))),
            "{read:?}"
        );
    }

    #[test]
    fn a_bound_that_has_passed_or_an_interrupt_that_is_set_reads_nothing() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("report.json");
        fs::write(&path, big_report(10)).expect("a report");
        let passed = Instant::now()
            .checked_sub(Duration::from_secs(1))
            .unwrap_or_else(Instant::now);
        let interrupted = Interrupt::new();
        interrupted.trigger();

        assert!(matches!(
            read_facts(&path, passed, &Interrupt::new()),
            Err(FactsError::Cancelled(Cancelled::TimedOut))
        ));
        assert!(matches!(
            read_facts(&path, later(), &interrupted),
            Err(FactsError::Cancelled(Cancelled::Interrupted))
        ));
        // The person's word comes first when both hold.
        assert!(matches!(
            read_facts(&path, passed, &interrupted),
            Err(FactsError::Cancelled(Cancelled::Interrupted))
        ));
        assert!(
            read_facts(&path, later(), &Interrupt::new()).is_ok(),
            "the same report reads when nothing stops it"
        );
    }

    #[test]
    fn a_report_that_is_not_json_is_not_taken_for_a_cancellation() {
        let read = facts_of("not json".as_bytes(), later(), &Interrupt::new());

        assert!(matches!(read, Err(FactsError::Json(_))), "{read:?}");
    }

    /// `read_facts` of `path`, on a thread that has `limit` to finish. A read that waits is a
    /// failure of this test and not a test run that never ends: the FIFO is opened for writing,
    /// which lets a reader that waits on it go on, and then the test fails.
    #[cfg(unix)]
    fn read_within(path: &Path, limit: Duration) -> Result<ScanFacts, FactsError> {
        use std::{sync::mpsc, thread};

        let (sender, receiver) = mpsc::channel();
        let target = path.to_path_buf();
        thread::spawn(move || {
            let _ = sender.send(read_facts(&target, later(), &Interrupt::new()));
        });
        if let Ok(result) = receiver.recv_timeout(limit) {
            return result;
        }
        let writer = fs::OpenOptions::new().read(true).write(true).open(path);
        thread::sleep(Duration::from_millis(200));
        drop(writer);
        panic!("reading a report that is a FIFO waited for a writer for {limit:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_report_that_is_a_fifo_is_refused_without_waiting_for_a_writer() {
        use nix::{sys::stat::Mode, unistd::mkfifo};

        let dir = tempfile::tempdir().expect("a temporary directory");
        let fifo = dir.path().join("scan-report.json");
        mkfifo(&fifo, Mode::S_IRUSR | Mode::S_IWUSR).expect("a FIFO");

        let refused = read_within(&fifo, Duration::from_secs(10));

        let Err(FactsError::Open(error)) = refused else {
            panic!("a FIFO was taken for a report: {refused:?}");
        };
        assert!(error.to_string().contains("not a regular file"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn a_report_that_is_a_link_or_a_folder_is_refused_and_a_regular_one_is_read() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let report = dir.path().join("report.json");
        fs::write(&report, big_report(1)).expect("a report");
        let link = dir.path().join("link.json");
        std::os::unix::fs::symlink(&report, &link).expect("a link");
        let folder = dir.path().join("folder.json");
        fs::create_dir(&folder).expect("a folder");

        for refused in [&link, &folder] {
            let read = read_facts(refused, later(), &Interrupt::new());
            assert!(
                matches!(read, Err(FactsError::Open(_))),
                "{}: {read:?}",
                refused.display()
            );
        }
        assert!(read_facts(&report, later(), &Interrupt::new()).is_ok());
    }
}
