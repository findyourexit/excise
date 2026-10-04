//! The `harness-tui` document: what one `cargo xtask tui` command prints.
//!
//! Every command prints exactly one of these on stdout, and a failure is one too (`ok` is `false`
//! and `error` says why). A successful document carries the command's `result`; the shape of the
//! result depends on `command`, which the schema (`schemas/harness-tui.schema.json`) spells out
//! per command. The same type is the supervisor's reply to a client, so the documents the
//! commands print are exactly the documents the supervisor wrote.
//!
//! Reading is strict: [`HarnessTui`] accepts a document only if `result` is the result of
//! `command` (and only on success), `error` is present only on failure, and no field is unknown.

use serde::{Deserialize, Serialize, de};
use serde_json::Value;

use super::{Document, SchemaVersion};
use crate::{
    scenario::{EntryKind, Profile},
    string_enum::string_enum,
};

string_enum! {
    /// The value of a `harness-tui` document's `document_kind` field.
    #[derive(Default)]
    pub enum TuiKind {
        /// The only kind this document can have.
        #[default]
        HarnessTui => "harness-tui",
    }
}

string_enum! {
    /// The commands of `cargo xtask tui`.
    pub enum TuiCommand {
        /// Starts a session on a fixture.
        Open => "open",
        /// Sends keys to a session.
        Keys => "keys",
        /// Deletes one entry of the fixture through the verified deletion protocol.
        Delete => "delete",
        /// Reads the screen.
        Screen => "screen",
        /// Reads the event channel's records.
        Events => "events",
        /// Ends a session.
        Close => "close",
        /// Lists the open sessions.
        List => "list",
    }
}

string_enum! {
    /// Why a command failed.
    pub enum TuiErrorKind {
        /// The command line is not valid.
        Usage => "usage",
        /// This platform cannot run sessions.
        UnsupportedPlatform => "unsupported_platform",
        /// There is no session with that id (it never existed, or it has ended).
        NoSuchSession => "no_such_session",
        /// The session's supervisor ended while the command was waiting for it.
        SessionLost => "session_lost",
        /// The driver refused to send a key or to confirm a deletion, and sent nothing that would.
        Refused => "refused",
        /// The session is in a state the command cannot start from.
        Conflict => "conflict",
        /// The entry a deletion names is not in the folder the session shows.
        NotInView => "not_in_view",
        /// A bounded wait ended before its condition held.
        Timeout => "timeout",
        /// The program ended while the command was running.
        ProgramExited => "program_exited",
        /// A deletion changed more of the fixture than its target.
        FixtureChanged => "fixture_changed",
        /// The harness could not do what the command needed.
        Failed => "failed",
    }
}

string_enum! {
    /// How a session's program ended.
    pub enum ExitVia {
        /// The driver quit it the way a user does (`q`, then the confirmation the dialog offered).
        Quit => "quit",
        /// It ended by itself while a command was running.
        Exited => "exited",
        /// It did not end within the bounded wait, so its process group was killed.
        Killed => "killed",
    }
}

string_enum! {
    /// Whether a session is ready for commands.
    pub enum SessionState {
        /// Its fixture is being generated or its program is starting.
        Starting => "starting",
        /// It is ready for commands.
        Running => "running",
    }
}

string_enum! {
    /// What a deletion dialog says it deletes.
    pub enum DeleteDialogKind {
        /// `! DELETE FOLDER`.
        Folder => "folder",
        /// `! DELETE FILE`.
        File => "file",
        /// `! DELETE ITEM`: a summary entry that is neither.
        Item => "item",
    }
}

string_enum! {
    /// How a deletion dialog asks to be confirmed.
    pub enum ConfirmationKind {
        /// One key (`[Enter/y] start`).
        SingleKey => "single_key",
        /// A typed phrase.
        TypedPhrase => "typed_phrase",
        /// Neither.
        Unknown => "unknown",
    }
}

string_enum! {
    /// The kind of one line of the event channel.
    pub enum EventRecordKind {
        /// The first line: the package version and the process id.
        Hello => "hello",
        /// A render drew a frame.
        Frame => "frame",
        /// The initial scan finished.
        ScanComplete => "scan_complete",
        /// The quit dialog was built.
        QuitPrompt => "quit_prompt",
        /// A deletion worker reported.
        DeletionFinished => "deletion_finished",
        /// The map on screen caught up with the deletions that removed entries.
        RefreshFinished => "refresh_finished",
        /// The program is about to return its exit code.
        Exit => "exit",
    }
}

string_enum! {
    /// How the refresh that deletions owed ended (`refresh_finished`).
    pub enum RefreshOutcomeKind {
        /// The map without the removed entries was published, and is the one on screen.
        Published => "published",
        /// No map could be shown.
        Failed => "failed",
    }
}

/// A terminal size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Size {
    /// Width in columns.
    pub cols: u16,
    /// Height in rows.
    pub rows: u16,
}

/// A cursor position, zero-based.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cursor {
    /// The row.
    pub row: u16,
    /// The column.
    pub col: u16,
}

/// The terminal modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Modes {
    /// The alternate screen is active.
    pub alternate_screen: bool,
    /// The cursor is visible.
    pub cursor_visible: bool,
    /// The terminal echoes input; `null` where the platform cannot say.
    pub echo: Option<bool>,
    /// The terminal is in canonical (line-buffered) mode; `null` where the platform cannot say.
    pub icanon: Option<bool>,
}

/// The inclusive cell rectangle of a box.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rect {
    /// The column of the left border.
    pub left: u16,
    /// The row of the top border.
    pub top: u16,
    /// The column of the right border.
    pub right: u16,
    /// The row of the bottom border.
    pub bottom: u16,
}

/// A titled box on the screen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoxInfo {
    /// The title in the top border.
    pub title: String,
    /// Where the box is.
    pub rect: Rect,
}

/// What a deletion dialog says.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteDialogInfo {
    /// What the title says is deleted.
    pub kind: DeleteDialogKind,
    /// The path line, as drawn. It is cut with an ellipsis when it does not fit.
    pub path: String,
    /// How the dialog is confirmed.
    pub confirmation: ConfirmationKind,
}

/// The modal dialog on the screen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DialogInfo {
    /// The title in the top border.
    pub title: String,
    /// Where the dialog is.
    pub rect: Rect,
    /// The whole dialog as drawn, borders included, one line per row.
    pub text: String,
    /// What a deletion dialog says; `null` for any other dialog.
    pub delete: Option<DeleteDialogInfo>,
}

/// The entry the inspector pane describes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectedInfo {
    /// The entry's name, as the pane shows it.
    pub name: String,
    /// The state line, such as `◆ COMPLETE`.
    pub state: String,
    /// The entry kind: `folder`, `file`, `link`, or `shared item`.
    pub kind: String,
}

/// The filter prompt in the header band.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilterInfo {
    /// The text typed so far.
    pub input: String,
    /// The reason the filter was rejected, if it was.
    pub error: Option<String>,
}

/// What the screen model shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScreenInfo {
    /// The terminal size.
    pub size: Size,
    /// The cursor.
    pub cursor: Cursor,
    /// The terminal modes.
    pub modes: Modes,
    /// The scan state in the header badge (`SCANNING`, `COMPLETE`, or another state's text);
    /// `null` before the header is drawn.
    pub header_state: Option<String>,
    /// The entry the inspector describes; `null` when the pane is not drawn or nothing is
    /// selected.
    pub selected: Option<SelectedInfo>,
    /// The open filter prompt; `null` when no filter is being typed.
    pub filter: Option<FilterInfo>,
    /// The modal dialog; `null` when none is open.
    pub dialog: Option<DialogInfo>,
    /// Every titled, closed box, from the top down.
    pub boxes: Vec<BoxInfo>,
    /// The visible text, one entry per row without trailing spaces.
    pub rows: Vec<String>,
}

/// A frame the program drew.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrameInfo {
    /// The drawn-frame counter, from 1.
    pub seq: u64,
    /// The terminal input events the program had consumed when it drew the frame.
    pub inputs: u64,
}

/// How many inputs the driver has sent a session, and how many the program has counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputCounts {
    /// The input events sent since the session opened.
    pub sent: u64,
    /// How many of them the latest frame counts.
    pub consumed: u64,
}

/// One input event the driver sent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SentKey {
    /// The key as written on the command line (one character for typed text).
    pub key: String,
    /// The bytes written to the terminal, in lowercase hexadecimal.
    pub bytes: String,
}

/// One line of the event channel.
///
/// `kind` says which of the optional fields the record carries: `hello` has `version`, `pid`,
/// `frame_marks`, and `input_barrier`; `frame` has `seq`, `inputs`, and `barriers`;
/// `scan_complete` has `entries`; `deletion_finished` has `removed` and `failed`;
/// `refresh_finished` has `outcome`; `exit` has `code`; `quit_prompt` has none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventRecord {
    /// The zero-based position of the record in the channel.
    pub index: u64,
    /// Microseconds since the channel opened, as `excise` measured them.
    pub t_us: u64,
    /// What the record reports.
    pub kind: EventRecordKind,
    /// `hello`: the `excise` package version.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// `hello`: the process id of `excise`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u64>,
    /// `hello`: whether the program marks its frames in the terminal output, so that the screen
    /// can be told to show a frame exactly.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame_marks: Option<bool>,
    /// `hello`: whether the program answers input barrier requests, so that a reader can tell
    /// that it has read everything written before a request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_barrier: Option<bool>,
    /// `frame`: the drawn-frame counter.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    /// `frame`: the terminal input events consumed so far.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inputs: Option<u64>,
    /// `frame`: the input barrier requests the program had read when it drew the frame.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub barriers: Option<u64>,
    /// `scan_complete`: the number of scanned entries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entries: Option<u64>,
    /// `deletion_finished`: the entries removed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub removed: Option<u64>,
    /// `deletion_finished`: the entries that could not be removed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed: Option<u64>,
    /// `refresh_finished`: how the refresh ended.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<RefreshOutcomeKind>,
    /// `exit`: the exit code.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<u64>,
}

/// A run of the event channel's records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventsPage {
    /// The index of the first record the page covers.
    pub since: u64,
    /// The index to pass as `--since` for the records after this page: the number of records the
    /// channel held when the page was made.
    pub next: u64,
    /// The records, from `since`.
    pub records: Vec<EventRecord>,
}

/// The frame records of a run of the event channel, summarized.
///
/// A program that animates draws about twenty frames a second, so a document that listed every
/// frame since the previous command would grow with the time between commands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrameSummary {
    /// How many frame records the run holds.
    pub count: u64,
    /// When the first of them was drawn: its `t_us`.
    pub first_t_us: u64,
    /// When the last of them was drawn: the `t_us` of `last`.
    pub last_t_us: u64,
    /// The last frame record, in full.
    pub last: EventRecord,
}

/// The records since the previous command that reported events, with the frames summarized.
///
/// Every record that is not a frame is listed in full and in order. `since` and `next` are those
/// of the page the digest was made from, so `events --since <since>` returns every record it
/// covers, frames included.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventsDigest {
    /// The index of the first record the digest covers.
    pub since: u64,
    /// The index to pass as `--since` for the records after the digest.
    pub next: u64,
    /// The frame records the digest covers, summarized; `null` when there are none.
    pub frames: Option<FrameSummary>,
    /// The records that are not frames, in order.
    pub records: Vec<EventRecord>,
}

impl EventsPage {
    /// The page with its frame records summarized: see [`EventsDigest`].
    #[must_use]
    pub fn digest(self) -> EventsDigest {
        let mut frames: Option<FrameSummary> = None;
        let mut records = Vec::new();
        for record in self.records {
            if record.kind != EventRecordKind::Frame {
                records.push(record);
                continue;
            }
            frames = Some(match frames {
                None => FrameSummary {
                    count: 1,
                    first_t_us: record.t_us,
                    last_t_us: record.t_us,
                    last: record,
                },
                Some(earlier) => FrameSummary {
                    count: earlier.count + 1,
                    last_t_us: record.t_us,
                    last: record,
                    ..earlier
                },
            });
        }
        EventsDigest {
            since: self.since,
            next: self.next,
            frames,
            records,
        }
    }
}

/// How a session's program ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExitInfo {
    /// The exit code; `null` if a signal ended the program.
    pub code: Option<i32>,
    /// The signal that ended it; `null` if it exited by itself.
    pub signal: Option<i32>,
    /// How it came to end.
    pub via: ExitVia,
    /// Whether the terminal was left as a shell expects it: the alternate screen left, the cursor
    /// visible, and echo and canonical mode on.
    pub terminal_restored: bool,
    /// The terminal modes after the exit.
    pub modes: Modes,
}

/// What a session's fixture looks like against what it was before any deletion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureChanges {
    /// The entries that were there and are gone.
    pub removed: u64,
    /// Changes that no confirmed deletion explains; empty when the program touched nothing else.
    pub unexpected: Vec<String>,
}

/// The result of `open`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenResult {
    /// The fixture's id.
    pub fixture: String,
    /// The profile the session runs under.
    pub profile: Profile,
    /// The terminal size (a profile can fix the width).
    pub size: Size,
    /// The disposable copy of the fixture that `excise` scans.
    pub root: String,
    /// The session's directory, with the supervisor's log.
    pub session_dir: String,
    /// The process id of `excise`.
    pub pid: u64,
    /// The process id of the session's supervisor.
    pub supervisor_pid: u64,
    /// How long the session may go without a command before it ends.
    pub idle_timeout_ms: u64,
    /// Where the asciicast is written, when the session records.
    pub recording: Option<String>,
    /// The first screen.
    pub screen: ScreenInfo,
    /// The events since the session started, with the frames summarized.
    pub events: EventsDigest,
}

/// The result of `keys`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeysResult {
    /// The input events sent, in order.
    pub sent: Vec<SentKey>,
    /// Whether the program drew a frame that counts every input sent, so the screen shows their
    /// effect. A key that changes nothing draws no frame.
    pub settled: bool,
    /// The latest frame the program drew.
    pub frame: Option<FrameInfo>,
    /// How many inputs the session has been sent and has counted.
    pub inputs: InputCounts,
    /// The screen after the keys.
    pub screen: ScreenInfo,
    /// The events since the previous command that reported events, with the frames summarized.
    pub events: EventsDigest,
    /// How the program ended, if it ended while the keys were sent. The session ends with it.
    pub exit: Option<ExitInfo>,
}

/// The result of `delete`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteResult {
    /// The fixture-relative path that was deleted.
    pub name: String,
    /// Whether it was a file or a folder.
    pub kind: EntryKind,
    /// The dialog, as it was verified before the confirmation key was sent.
    pub dialog: DialogInfo,
    /// How many fixture-relative paths had to exist, and not lie in the entry, before the
    /// confirmation key was sent.
    pub sentinels_checked: u64,
    /// The entries the program reported removed.
    pub removed: u64,
    /// The entries the program reported it could not remove.
    pub failed: u64,
    /// The fixture against what it was before any deletion.
    pub fixture: FixtureChanges,
    /// The screen after the deletion.
    pub screen: ScreenInfo,
    /// The events since the previous command that reported events, with the frames summarized.
    pub events: EventsDigest,
}

/// The result of `screen`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScreenResult {
    /// The screen now.
    pub screen: ScreenInfo,
}

/// The result of `events`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventsResult {
    /// The records.
    pub events: EventsPage,
}

/// What removing a session's files came to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cleanup {
    /// Whether the run copy and the scratch area are gone.
    pub removed: bool,
    /// What could not be removed, and why.
    pub problems: Vec<String>,
}

/// The result of `close`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloseResult {
    /// How the program ended.
    pub exit: ExitInfo,
    /// The screen after the program ended.
    pub screen: ScreenInfo,
    /// The events since the previous command that reported events, with the frames summarized.
    pub events: EventsDigest,
    /// Where the asciicast was kept, when the session recorded.
    pub recording: Option<String>,
    /// The fixture against what it was before any deletion.
    pub fixture: FixtureChanges,
    /// What the program left in its scratch area, as paths relative to it; empty when it left
    /// nothing.
    pub residue: Vec<String>,
    /// What removing the session's files came to.
    pub cleanup: Cleanup,
}

/// An open session, as `list` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionInfo {
    /// The session id.
    pub session: String,
    /// Whether it is ready for commands.
    pub state: SessionState,
    /// The fixture's id.
    pub fixture: String,
    /// The profile it runs under.
    pub profile: Profile,
    /// The terminal size.
    pub size: Size,
    /// The process id of its supervisor, once known.
    pub supervisor_pid: Option<u64>,
    /// When it was opened, as an RFC 3339 timestamp.
    pub started_at: String,
    /// How long it may go without a command before it ends.
    pub idle_timeout_ms: u64,
    /// Where its asciicast is written, when it records.
    pub recording: Option<String>,
}

/// A session whose supervisor had ended without cleaning up, as `list` and `open` found it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StaleSession {
    /// The session id.
    pub session: String,
    /// Why it counts as stale.
    pub reason: String,
    /// The fixture's id, when the session's configuration could be read.
    pub fixture: Option<String>,
    /// Where its asciicast was kept, when it recorded: cleaning never removes it.
    pub recording: Option<String>,
    /// Whether everything of the session is gone.
    pub cleaned: bool,
    /// What was left, and why: a process or a directory that cannot be proven to be the
    /// session's is never signaled or removed, and neither is anything that could not be removed.
    /// A session with a problem stays, and `list` reports it again.
    pub problems: Vec<String>,
}

/// The result of `list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListResult {
    /// The open sessions.
    pub sessions: Vec<SessionInfo>,
    /// The stale sessions this command cleaned.
    pub stale: Vec<StaleSession>,
}

/// The result of a successful command; which one depends on the document's `command`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum TuiResult {
    /// `open`.
    Open(OpenResult),
    /// `keys`.
    Keys(KeysResult),
    /// `delete`.
    Delete(DeleteResult),
    /// `screen`.
    Screen(ScreenResult),
    /// `events`.
    Events(EventsResult),
    /// `close`.
    Close(CloseResult),
    /// `list`.
    List(ListResult),
}

impl TuiResult {
    /// The command this is the result of.
    #[must_use]
    pub const fn command(&self) -> TuiCommand {
        match self {
            Self::Open(_) => TuiCommand::Open,
            Self::Keys(_) => TuiCommand::Keys,
            Self::Delete(_) => TuiCommand::Delete,
            Self::Screen(_) => TuiCommand::Screen,
            Self::Events(_) => TuiCommand::Events,
            Self::Close(_) => TuiCommand::Close,
            Self::List(_) => TuiCommand::List,
        }
    }

    fn parse(command: TuiCommand, value: Value) -> Result<Self, serde_json::Error> {
        Ok(match command {
            TuiCommand::Open => Self::Open(serde_json::from_value(value)?),
            TuiCommand::Keys => Self::Keys(serde_json::from_value(value)?),
            TuiCommand::Delete => Self::Delete(serde_json::from_value(value)?),
            TuiCommand::Screen => Self::Screen(serde_json::from_value(value)?),
            TuiCommand::Events => Self::Events(serde_json::from_value(value)?),
            TuiCommand::Close => Self::Close(serde_json::from_value(value)?),
            TuiCommand::List => Self::List(serde_json::from_value(value)?),
        })
    }
}

/// Why a command failed.
///
/// The optional fields carry what the command knew when it failed: the keys it had sent, whether
/// a deletion's confirmation key had been sent, the screen, and how the program ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TuiError {
    /// What kind of failure this is.
    pub kind: TuiErrorKind,
    /// What happened, in a sentence or two.
    pub message: String,
    /// The input events the command had sent before it failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sent: Option<Vec<SentKey>>,
    /// For `delete`: whether the confirmation key may have been sent: it was attempted, and
    /// nothing says that it did not arrive (a write that failed does; a recording of the input
    /// that failed after the write does not). When it is true the deletion may be running or done
    /// even though the command failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirmed: Option<bool>,
    /// The screen when the command failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screen: Option<Box<ScreenInfo>>,
    /// How the program ended, if it did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit: Option<ExitInfo>,
}

impl TuiError {
    /// An error of `kind` with nothing but its message.
    #[must_use]
    pub fn new(kind: TuiErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            sent: None,
            confirmed: None,
            screen: None,
            exit: None,
        }
    }
}

/// The document one `cargo xtask tui` command prints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HarnessTui {
    /// Always `harness-tui`.
    pub document_kind: TuiKind,
    /// Always the current schema version.
    pub schema_version: SchemaVersion,
    /// The command; `null` for a command line that names none.
    pub command: Option<TuiCommand>,
    /// Whether the command did what it was asked.
    pub ok: bool,
    /// The session the command acted on, when it names one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// The result; present exactly when `ok`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<TuiResult>,
    /// The failure; present exactly when not `ok`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<TuiError>,
}

impl HarnessTui {
    /// The document of a command that did what it was asked.
    #[must_use]
    pub fn success(session: Option<String>, result: TuiResult) -> Self {
        Self {
            document_kind: TuiKind::HarnessTui,
            schema_version: SchemaVersion,
            command: Some(result.command()),
            ok: true,
            session,
            result: Some(result),
            error: None,
        }
    }

    /// The document of a command that failed.
    #[must_use]
    pub const fn failure(
        command: Option<TuiCommand>,
        session: Option<String>,
        error: TuiError,
    ) -> Self {
        Self {
            document_kind: TuiKind::HarnessTui,
            schema_version: SchemaVersion,
            command,
            ok: false,
            session,
            result: None,
            error: Some(error),
        }
    }
}

/// The wire form that [`HarnessTui`] is read from: the result is not yet typed.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    document_kind: TuiKind,
    schema_version: SchemaVersion,
    command: Option<TuiCommand>,
    ok: bool,
    #[serde(default)]
    session: Option<String>,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<TuiError>,
}

impl<'de> Deserialize<'de> for HarnessTui {
    fn deserialize<D: de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = Wire::deserialize(deserializer)?;
        let result = match (wire.ok, wire.result, &wire.error) {
            (true, Some(value), None) => {
                let command = wire
                    .command
                    .ok_or_else(|| de::Error::custom("a successful document names its command"))?;
                Some(TuiResult::parse(command, value).map_err(|error| {
                    de::Error::custom(format!("the result of `{command}`: {error}"))
                })?)
            }
            (false, None, Some(_)) => None,
            (true, _, _) => {
                return Err(de::Error::custom(
                    "a successful document has a `result` and no `error`",
                ));
            }
            (false, _, _) => {
                return Err(de::Error::custom(
                    "a failed document has an `error` and no `result`",
                ));
            }
        };
        Ok(Self {
            document_kind: wire.document_kind,
            schema_version: wire.schema_version,
            command: wire.command,
            ok: wire.ok,
            session: wire.session,
            result,
            error: wire.error,
        })
    }
}

impl Document for HarnessTui {
    const KIND: &'static str = TuiKind::HarnessTui.as_str();
    const SCHEMA_ID: &'static str =
        "https://github.com/findyourexit/excise/harness/schemas/harness-tui-v1.json";
    const SCHEMA_JSON: &'static str = include_str!("../../schemas/harness-tui.schema.json");
}
