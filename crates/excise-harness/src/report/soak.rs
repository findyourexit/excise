//! The `harness-soak` document: what one read-only soak of a real tree measured.
//!
//! A soak runs on a tree the harness did not generate, and the person who ran it may want to share
//! what it found: a regression, a number that looks wrong, the cost of a scan on a big home
//! directory. So this document holds metrics and counts and nothing else. It has no path and no
//! name from the tree, no host name, and no free text at all: every string in it is a number's
//! name, an enumerated word, an identifier, a timestamp, or a digest, and its JSON Schema says so
//! (`enum`, `const`, and `pattern`), so a document that carried a name would not validate. The
//! names of the numbers are enumerated too, and not matched by a pattern, which a folder called
//! `photos` would pass: a key of `metrics` is one of the names a session records, and no other.
//! What a person needs in order to look into a quirk (a dialog's text, the last folder a scan
//! could not read) goes to `quirks.txt`, which stays on the machine that made it. See
//! [`crate::soak`].

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{Document, SchemaVersion, tui::ExitVia};
use crate::{headless::document::ScanState, scenario::Profile, string_enum::string_enum};

string_enum! {
    /// The value of a soak document's `document_kind` field.
    #[derive(Default)]
    pub enum SoakKind {
        /// The only kind a soak document can have.
        #[default]
        HarnessSoak => "harness-soak",
    }
}

string_enum! {
    /// How a soak ended.
    pub enum SoakOutcome {
        /// Every round ran.
        Finished => "finished",
        /// The person interrupted the run, or the bound on the whole run passed. The sessions that
        /// finished are recorded, and the rest never ran.
        Interrupted => "interrupted",
        /// The harness could not go on (it could not make a scratch area, for instance). The
        /// sessions before the failure are recorded.
        Failed => "failed",
    }
}

string_enum! {
    /// What a quirk is a quirk of: the kinds of unexpected thing the soak notes. The text of each
    /// quirk is in `quirks.txt`, which can name things in the tree and so stays local; the
    /// document only counts them by kind.
    pub enum QuirkKind {
        /// A dialog other than the quit prompt the soak opened was on the screen: an error, a
        /// notice, a warning.
        ErrorDialog => "error_dialog",
        /// The screen showed text that reads as a deletion dialog (`[Enter/y] start`, or `Type this
        /// exactly: `), so the soak did not send `Enter` or `y`. The soak never asks for a
        /// deletion, so no such dialog can be there: the text is a name in the tree, or a program
        /// that is not the one under test. The folder was not opened, or the program was killed
        /// instead of quit.
        DialogText => "dialog_text",
        /// A scan ended with an uncertain result: folders it could not read, a file system
        /// boundary, an exclusion.
        UncertainScan => "uncertain_scan",
        /// A scan report that the soak could not read, or whose `accounting` is not the contract's.
        ReportUnreadable => "report_unreadable",
        /// A scan's report grew past what the soak allows it in its scratch area, and the scan's
        /// process group was killed. A tree of very deep folders makes a report grow with the
        /// square of its depth.
        ReportTooLarge => "report_too_large",
        /// A program ended with a status other than 0.
        NonZeroExit => "non_zero_exit",
        /// A bound on a phase passed, and the program's process group was killed.
        Timeout => "timeout",
        /// A gap between two frames, while the program was busy, longer than a second.
        Stall => "stall",
        /// A key drew no frame within its bound.
        NoFrameForKey => "no_frame_for_key",
        /// The program ended before the soak asked it to.
        ExitedEarly => "exited_early",
        /// The quit prompt did not open, or did not offer the plain quit, so the program was killed
        /// instead of quit.
        QuitRefused => "quit_refused",
        /// No folder was opened: the cursor started on a file, and the walk with the arrow keys
        /// to a folder was cut short (its keys ran out, a key drew no frame, the inspector went
        /// blank, or the cursor could not get back to an entry that had keys left), or the header
        /// cut the root's path in the middle on a terminal whose screen is not exact, and a folder
        /// could not be told from the root. A root with no folder at all ends the walk without
        /// one.
        DrillSkipped => "drill_skipped",
        /// The terminal was not as a shell expects it after the program ended.
        TerminalNotRestored => "terminal_not_restored",
        /// The program left something in its scratch area.
        Residue => "residue",
        /// The program does not mark its frames or answer an input barrier, or its event channel
        /// is not the one this harness reads.
        Protocol => "protocol",
        /// The harness could not go on.
        HarnessError => "harness_error",
    }
}

string_enum! {
    /// A phase of a session, for the one that a bound ended.
    pub enum SoakPhase {
        /// From the start to the first frame.
        FirstFrame => "first_frame",
        /// The keys sent while the scan runs, up to the bound on the scan, which is counted from
        /// the first frame.
        Scan => "scan",
        /// From the end of the keys to the end of the scan on the screen: the `scan_complete`
        /// event and a header badge that says the scan ended (`COMPLETE`, or the label of an
        /// uncertain result), within the same bound on the scan.
        Complete => "complete",
        /// Opening a folder: the largest entry, or the first folder the arrow keys reach.
        Drill => "drill",
        /// Going back up.
        Up => "up",
        /// The quit.
        Quit => "quit",
        /// The wait for the program to end after the quit was confirmed.
        Exit => "exit",
    }
}

string_enum! {
    /// What a scan report's `accounting.headline` says. The published schema allows one value; a
    /// report that says another is a quirk, and its facts are not kept.
    pub enum AccountingHeadline {
        /// Allocation counted once for each file identity.
        IdentityUniqueAllocatedBytes => "identity-unique-allocated-bytes",
    }
}

/// How many rounds the soak ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SoakRounds {
    /// What the command line asked for.
    pub requested: u32,
    /// The rounds that ran to their end: a round that was interrupted is not counted.
    pub completed: u32,
}

/// The bounds the soak ran under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SoakLimits {
    /// The bound on the whole run, in milliseconds.
    pub run_ms: u64,
}

/// The accounting a scan report states (`accounting` in the published `scan-report` schema).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SoakAccounting {
    /// What the headline number counts.
    pub headline: AccountingHeadline,
    /// Whether every name of a hard-linked file shares one allocation.
    pub hard_links_deduplicated: bool,
    /// Whether extents that files share are counted once.
    pub shared_extents_deduplicated: bool,
    /// Whether a directory's own metadata is in its total.
    pub directory_metadata_included: bool,
}

/// What a scan report's `summary` counts, and whether the text fields beside the counts held
/// anything. The text itself is not kept: it can be a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "the four flags say whether a text field of the report held anything: they mirror the document, and none is a state of another"
)]
pub struct SoakScanSummary {
    /// Entries the scanner visited.
    pub scanned_entries: u64,
    /// Entries identified by the identity pass.
    pub identified_entries: u64,
    /// Folders the scanner could not read.
    pub unreadable_entries: u64,
    /// Entries the scanner did not enter.
    pub unscanned_entries: u64,
    /// Entries a configured exclusion left out.
    pub excluded_entries: u64,
    /// Mount points the scanner did not cross.
    pub filesystem_boundaries: u64,
    /// Links the scanner did not follow.
    pub link_entries: u64,
    /// Entries a deletion removed. A soak deletes nothing, so this is 0 in every report it reads.
    pub deleted_entries: u64,
    /// Entries a deletion found changed.
    pub deletion_changed_entries: u64,
    /// Entries a deletion found missing.
    pub deletion_missing_entries: u64,
    /// Entries a deletion failed on.
    pub deletion_failed_entries: u64,
    /// Entries a deletion did not attempt.
    pub deletion_unattempted_entries: u64,
    /// Bytes the scan store held at the end.
    pub scan_store_bytes: u64,
    /// The scan store's limit.
    pub scan_store_limit_bytes: u64,
    /// Whether the report names the last folder it could not read.
    pub last_unreadable_path_present: bool,
    /// Whether the report names the last path the scanner did not enter.
    pub last_unscanned_path_present: bool,
    /// Whether the report gives a reason for the last path the scanner did not enter.
    pub last_unscanned_reason_present: bool,
    /// Whether the report carries a worker error.
    pub last_worker_error_present: bool,
}

/// What a headless scan's report said: its state, its accounting, and its counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SoakReportFacts {
    /// The state of the whole report.
    pub state: ScanState,
    /// The accounting the report states.
    pub accounting: SoakAccounting,
    /// The counts of what the scan met.
    pub summary: SoakScanSummary,
}

/// One headless scan: `excise --format json` on the root, bounded and measured.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SoakHeadless {
    /// The round, from 1.
    pub round: u32,
    /// The profile the scan ran under.
    pub profile: Profile,
    /// The exit code, when the process exited by itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    /// The signal that ended it, when one did: the bound's kill is signal 9.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i64>,
    /// Whether the bound passed and the process group was killed.
    pub timed_out: bool,
    /// From just before the spawn to the exit, in milliseconds.
    pub wall_ms: f64,
    /// CPU time in user mode, where the platform says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_ms: Option<f64>,
    /// CPU time in the kernel, where the platform says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sys_ms: Option<f64>,
    /// Peak memory in bytes, where the platform says (see `Finished::peak_memory_bytes`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peak_rss_bytes: Option<u64>,
    /// Entries the scan left in its scratch area besides its report.
    pub residue_files: u64,
    /// What the report said. Absent when the scan wrote none that could be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<SoakReportFacts>,
}

/// How a session's program ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SoakExit {
    /// The exit code, when the program exited by itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<i64>,
    /// The signal that ended it, when one did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i64>,
    /// Whether the soak quit it, it ended by itself, or its process group was killed.
    pub via: ExitVia,
}

/// One session in a pseudo-terminal: the scan, the keys, the largest folder, the quit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SoakTui {
    /// The round, from 1.
    pub round: u32,
    /// The profile the session ran under: `default` or `deterministic`.
    pub profile: Profile,
    /// How the program ended.
    pub exit: SoakExit,
    /// The phase whose bound passed, when one did. The session then ended with the program's
    /// process group killed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timed_out_phase: Option<SoakPhase>,
    /// Whether the terminal was as a shell expects it once the program had ended.
    pub terminal_restored: bool,
    /// Entries the program left in its scratch area.
    pub residue_files: u64,
    /// Whether the soak opened a folder and came back up: the largest entry when that is a folder,
    /// and otherwise the first folder the arrow keys reached from it.
    pub drilled: bool,
    /// Named measurements, every value a finite number. A name is one of those that a session of
    /// the soak records and the schema lists (`$defs/metric_name`): `first_frame_ms`,
    /// `scan_complete_ms`, `scan_complete_entries`, `complete_ms`, `duration_ms`, `drill_ms`,
    /// `up_ms`, `quit_ms`, `input_samples`, `input_to_frame_p50_ms`, `input_to_frame_p99_ms`,
    /// `input_to_frame_max_ms`, `inputs_sent`, `max_stall_ms`, `frames`, `output_bytes`,
    /// `output_bytes_per_s`, `peak_rss_bytes`, `threads`, `fds`, `scan_store_peak_bytes`,
    /// `scan_store_bytes_per_entry`, `user_ms`, and `sys_ms`. A document with any other key does
    /// not validate, so a name from the tree cannot be one; a session that did not get as far as a
    /// measurement has no such name.
    pub metrics: BTreeMap<String, f64>,
}

/// How many quirks of one kind the soak noted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SoakQuirkCount {
    /// The kind.
    pub kind: QuirkKind,
    /// How many, at least 1.
    pub count: u64,
}

/// What one soak measured, and nothing that names the tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessSoak {
    /// Always `harness-soak`.
    pub document_kind: SoakKind,
    /// Always the current schema version.
    pub schema_version: SchemaVersion,
    /// The run identifier, which is also the name of the run's output directory.
    pub run_id: String,
    /// When the run started, as an RFC 3339 timestamp.
    pub started_at: String,
    /// When the run ended, as an RFC 3339 timestamp.
    pub finished_at: String,
    /// The operating system, as in `std::env::consts::OS`.
    pub os: String,
    /// The CPU architecture, as in `std::env::consts::ARCH`.
    pub arch: String,
    /// The 40-character commit of the checkout the harness ran from.
    pub git_sha: String,
    /// The lowercase hexadecimal SHA-256 of the `excise` binary that was soaked. Its path is not
    /// recorded.
    pub excise_sha256: String,
    /// How the run ended.
    pub outcome: SoakOutcome,
    /// How many rounds it ran.
    pub rounds: SoakRounds,
    /// The bounds it ran under.
    pub limits: SoakLimits,
    /// One entry per headless scan, in the order they ran.
    pub headless: Vec<SoakHeadless>,
    /// One entry per session in a pseudo-terminal, in the order they ran.
    pub tui: Vec<SoakTui>,
    /// The quirks the soak noted, by kind. Absent kinds had none.
    pub quirks: Vec<SoakQuirkCount>,
}

impl Document for HarnessSoak {
    const KIND: &'static str = SoakKind::HarnessSoak.as_str();
    const SCHEMA_ID: &'static str =
        "https://github.com/findyourexit/excise/harness/schemas/harness-soak-v1.json";
    const SCHEMA_JSON: &'static str = include_str!("../../schemas/harness-soak.schema.json");
}
