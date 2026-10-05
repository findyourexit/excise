//! The step vocabulary of a scenario.
//!
//! Steps are written as an array of tables. The `step` key names the step and selects the fields
//! that follow; every step rejects keys it does not define.
//!
//! ```toml
//! [[steps]]
//! step = "wait_header"
//! state = "complete"
//! timeout_ms = 30000
//! ```
//!
//! Steps that wait for the program (or for the file system) take an optional `timeout_ms`.
//! Omitting it applies [`DEFAULT_TIMEOUT_MS`]. `expect_*` steps other than `expect_exit`
//! evaluate once against the current state and never wait.

use std::{collections::BTreeMap, fmt, str::FromStr};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{Budget, DEFAULT_TIMEOUT_MS};
use crate::string_enum::string_enum;

const fn default_timeout_ms() -> u64 {
    DEFAULT_TIMEOUT_MS
}

/// One step of a scenario.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "step", rename_all = "snake_case")]
pub enum Step {
    /// Wait until text or a regular expression appears on the screen.
    WaitText(WaitText),
    /// Wait until the header row reports a scan state.
    WaitHeader(WaitHeader),
    /// Wait until the event channel reports an event.
    WaitEvent(WaitEvent),
    /// Press one key.
    Key(PressKey),
    /// Type literal text.
    Type(TypeText),
    /// Select an entry by name using the filter.
    Select(Select),
    /// Delete the selected entry through the confirmation dialog.
    Delete(Delete),
    /// Wait until the map has caught up with the deletions confirmed so far.
    WaitRefresh(WaitRefresh),
    /// Wait until a fixture-relative path no longer exists.
    WaitFsAbsent(WaitFs),
    /// Wait until a fixture-relative path exists.
    WaitFsPresent(WaitFs),
    /// Change the fixture while the program runs.
    FsMutate(FsMutate),
    /// Resize the terminal.
    Resize(Resize),
    /// Deliver a signal or console event to the program.
    Signal(SendSignal),
    /// Assert what is on the screen now.
    ExpectScreen(ExpectScreen),
    /// Assert which fixture-relative paths exist now.
    ExpectFs(ExpectFs),
    /// Assert a setting in the configuration file the program saved.
    ExpectConfig(ExpectConfig),
    /// Wait for the program to exit and assert how it ended.
    ExpectExit(ExpectExit),
    /// Assert that a recorded metric is within a budget.
    ExpectBudget(ExpectBudget),
    /// Mark the start or the stop of a named measurement.
    Measure(Measure),
    /// Measure the program's idle output and CPU over a quiet window.
    Idle(Idle),
    /// Wait until the program has processed everything sent so far.
    Settle(Settle),
    /// Perform the ordinary confirmed quit.
    Quit(Quit),
}

impl Step {
    /// Every name accepted in the `step` field, in documentation order.
    pub const KINDS: [&'static str; 22] = [
        "wait_text",
        "wait_header",
        "wait_event",
        "key",
        "type",
        "select",
        "delete",
        "wait_refresh",
        "wait_fs_absent",
        "wait_fs_present",
        "fs_mutate",
        "resize",
        "signal",
        "expect_screen",
        "expect_fs",
        "expect_config",
        "expect_exit",
        "expect_budget",
        "measure",
        "idle",
        "settle",
        "quit",
    ];

    /// The name of this step as written in the `step` field.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::WaitText(_) => "wait_text",
            Self::WaitHeader(_) => "wait_header",
            Self::WaitEvent(_) => "wait_event",
            Self::Key(_) => "key",
            Self::Type(_) => "type",
            Self::Select(_) => "select",
            Self::Delete(_) => "delete",
            Self::WaitRefresh(_) => "wait_refresh",
            Self::WaitFsAbsent(_) => "wait_fs_absent",
            Self::WaitFsPresent(_) => "wait_fs_present",
            Self::FsMutate(_) => "fs_mutate",
            Self::Resize(_) => "resize",
            Self::Signal(_) => "signal",
            Self::ExpectScreen(_) => "expect_screen",
            Self::ExpectFs(_) => "expect_fs",
            Self::ExpectConfig(_) => "expect_config",
            Self::ExpectExit(_) => "expect_exit",
            Self::ExpectBudget(_) => "expect_budget",
            Self::Measure(_) => "measure",
            Self::Idle(_) => "idle",
            Self::Settle(_) => "settle",
            Self::Quit(_) => "quit",
        }
    }

    /// The step's bound in milliseconds, or `None` for a step that never waits.
    #[must_use]
    pub const fn timeout_ms(&self) -> Option<u64> {
        match self {
            Self::WaitText(step) => Some(step.timeout_ms),
            Self::WaitHeader(step) => Some(step.timeout_ms),
            Self::WaitEvent(step) => Some(step.timeout_ms),
            Self::Select(step) => Some(step.timeout_ms),
            Self::Delete(step) => Some(step.timeout_ms),
            Self::WaitRefresh(step) => Some(step.timeout_ms),
            Self::WaitFsAbsent(step) | Self::WaitFsPresent(step) => Some(step.timeout_ms),
            Self::ExpectExit(step) => Some(step.timeout_ms),
            Self::Settle(step) => Some(step.timeout_ms),
            Self::Quit(step) => Some(step.timeout_ms),
            Self::Key(_)
            | Self::Type(_)
            | Self::FsMutate(_)
            | Self::Resize(_)
            | Self::Signal(_)
            | Self::ExpectScreen(_)
            | Self::ExpectFs(_)
            | Self::ExpectConfig(_)
            | Self::ExpectBudget(_)
            | Self::Measure(_)
            | Self::Idle(_) => None,
        }
    }
}

/// A part of the screen that a text match is limited to.
///
/// Written as `"header"`, `"dialog"`, or `{ rows = [first, last] }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Region {
    /// The header row, which is the only valid source of scan state.
    Header,
    /// The modal dialog, when one is open.
    Dialog,
    /// An inclusive span of zero-based screen rows, `[first, last]`.
    Rows([u16; 2]),
}

string_enum! {
    /// The scan state shown in the header row.
    pub enum ScanState {
        /// The header reports an active scan.
        Scanning => "scanning",
        /// The header reports a finished scan.
        Complete => "complete",
    }
}

string_enum! {
    /// A kind of event reported by the program's event channel.
    ///
    /// These are the kinds the channel carries, without its `hello` handshake line, which the
    /// runner consumes itself.
    pub enum EventKind {
        /// A frame was presented.
        Frame => "frame",
        /// The scan finished.
        ScanComplete => "scan_complete",
        /// A deletion ended.
        DeletionFinished => "deletion_finished",
        /// The quit prompt was shown.
        QuitPrompt => "quit_prompt",
        /// The program is exiting.
        Exit => "exit",
    }
}

string_enum! {
    /// A numeric field of an event that a `wait_event` predicate can test.
    pub enum EventField {
        /// The frame sequence number.
        Seq => "seq",
        /// Terminal input events the program had consumed when the frame was presented.
        Inputs => "inputs",
        /// Microseconds since the channel opened. Every event carries it.
        TimeMicros => "t_us",
        /// Entries indexed by the finished scan.
        Entries => "entries",
        /// Entries a deletion removed.
        Removed => "removed",
        /// Entries a deletion failed to remove.
        Failed => "failed",
        /// The exit code.
        Code => "code",
    }
}

impl EventKind {
    /// The numeric fields this kind of event carries.
    #[must_use]
    pub const fn fields(self) -> &'static [EventField] {
        match self {
            Self::Frame => &[EventField::Seq, EventField::Inputs, EventField::TimeMicros],
            Self::ScanComplete => &[EventField::Entries, EventField::TimeMicros],
            Self::DeletionFinished => &[
                EventField::Removed,
                EventField::Failed,
                EventField::TimeMicros,
            ],
            Self::QuitPrompt => &[EventField::TimeMicros],
            Self::Exit => &[EventField::Code, EventField::TimeMicros],
        }
    }
}

/// A test applied to one numeric event field.
///
/// Written as a table with exactly one key: `{ eq = 5 }`, `{ min = 5 }`, or `{ max = 5 }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Comparison {
    /// The field equals the value.
    Eq(u64),
    /// The field is at least the value.
    Min(u64),
    /// The field is at most the value.
    Max(u64),
}

/// Waits until `text` or `regex` (exactly one) appears on the screen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaitText {
    /// Literal text to find.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// A regular expression to find, in Rust `regex` syntax.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub regex: Option<String>,
    /// Limits the search to part of the screen.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<Region>,
    /// The bound on the wait.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

/// Waits until the header row reports `state`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaitHeader {
    /// The scan state to wait for.
    pub state: ScanState,
    /// The bound on the wait.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

/// Waits until the event channel reports `event` with every listed field test satisfied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaitEvent {
    /// The event to wait for.
    pub event: EventKind,
    /// Tests on the event's numeric fields. Every field must belong to `event`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub fields: BTreeMap<EventField, Comparison>,
    /// The bound on the wait.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

/// Presses one key, optionally with modifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PressKey {
    /// The key: a single character or a named key.
    pub key: KeyName,
    /// Hold Ctrl.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ctrl: bool,
    /// Hold Alt.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub alt: bool,
}

/// Types literal text, one character at a time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TypeText {
    /// The text to type.
    pub text: String,
}

/// Selects the entry called `name`: opens the filter, types the name, presses Enter, then asserts
/// that the selected item is that entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Select {
    /// The entry name.
    pub name: String,
    /// The bound on the waits inside the step.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

string_enum! {
    /// The kind of entry a deletion targets.
    pub enum EntryKind {
        /// A regular file (or other non-directory entry).
        File => "file",
        /// A directory.
        Folder => "folder",
    }
}

/// Deletes the currently selected entry: presses Backspace, asserts that the dialog names exactly
/// this entry and kind, asserts the sentinels, and only then confirms with `confirm_with`.
/// `wait_for` controls when the step returns relative to the deletion it starts.
///
/// A scenario that sets `disable_delete_confirmation` has no dialog: the step then checks the
/// selected-item panel and the entry on disk instead, presses Backspace alone, and fails if a
/// dialog opens (see [`Scenario::disable_delete_confirmation`](super::Scenario)).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Delete {
    /// The entry name the dialog must show.
    pub name: String,
    /// The kind of entry the dialog must show.
    pub kind: EntryKind,
    /// Where the entry is, relative to the fixture root and `/`-separated; its last component is
    /// `name`. Absent means `name` itself: an entry directly below the fixture root. With a
    /// dialog, the path the dialog shows must be exactly the fixture root and this path. Without
    /// one, this is the only thing that says where the selected entry lives, so a nested entry
    /// needs it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The key that confirms the dialog once it names the entry: `"y"` (the default) or
    /// `"enter"`, the two the dialog offers. A scenario with no dialog
    /// (`disable_delete_confirmation`) must leave it at the default.
    #[serde(default)]
    pub confirm_with: ConfirmKey,
    /// When the step returns relative to the deletion. Defaults to `"finished"`.
    #[serde(default)]
    pub wait_for: DeleteWait,
    /// The bound on the waits inside the step.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

string_enum! {
    /// The key a `delete` step presses to confirm the dialog.
    #[derive(Default)]
    pub enum ConfirmKey {
        /// The `y` key. The default.
        #[default]
        Y => "y",
        /// Enter. The dialog says `[Enter/y] start`, so both start the deletion.
        Enter => "enter",
    }
}

string_enum! {
    /// When a `delete` step returns relative to the deletion it starts.
    #[derive(Default)]
    pub enum DeleteWait {
        /// Wait for the deletion to finish (a `deletion_finished` event) and for the frame drawn
        /// after it, so the screen already shows the result. The default.
        #[default]
        Finished => "finished",
        /// Return once the confirmation has been processed and the deletion has started: the
        /// dialog has closed. The deletion keeps running after the step returns; a step that
        /// needs its result waits for that separately (`wait_fs_absent`, `wait_event`).
        Started => "started",
    }
}

/// Waits for a fixture-relative path to appear or disappear.
///
/// Used by both `wait_fs_absent` and `wait_fs_present`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaitFs {
    /// The fixture-relative path.
    pub path: String,
    /// The bound on the wait.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

string_enum! {
    /// A live change the runner makes to the fixture.
    pub enum MutateOp {
        /// Create a new entry.
        Appear => "appear",
        /// Change an existing entry in place.
        Change => "change",
        /// Remove an existing entry.
        Vanish => "vanish",
        /// Replace an existing entry with a different one of the same name.
        Replace => "replace",
    }
}

/// Changes the fixture while the program runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FsMutate {
    /// What to do.
    pub op: MutateOp,
    /// The fixture-relative path to act on.
    pub path: String,
}

/// Resizes the terminal. Unlike the initial terminal, a resize may go below the supported minimum
/// to exercise the resize message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resize {
    /// The new width in columns.
    pub cols: u16,
    /// The new height in rows.
    pub rows: u16,
}

string_enum! {
    /// A signal or console event a runner can deliver.
    pub enum Signal {
        /// Unix `SIGTERM`.
        Term => "term",
        /// Unix `SIGHUP`.
        Hup => "hup",
        /// Unix `SIGQUIT`.
        Quit => "quit",
        /// Unix `SIGINT`.
        Int => "int",
        /// Windows console close event.
        Close => "close",
        /// Windows console break event.
        Break => "break",
    }
}

/// Delivers a signal or console event to the program.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SendSignal {
    /// The signal to deliver.
    pub signal: Signal,
}

/// Asserts the current screen. At least one of `contains`, `not_contains`, and `regex` must list
/// something.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpectScreen {
    /// Text that must appear.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contains: Vec<String>,
    /// Text that must not appear.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub not_contains: Vec<String>,
    /// Regular expressions that must match, in Rust `regex` syntax.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub regex: Vec<String>,
    /// Limits every check to part of the screen.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<Region>,
}

/// Asserts which fixture-relative paths exist now. At least one list must be non-empty.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpectFs {
    /// Paths that must exist.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub present: Vec<String>,
    /// Paths that must not exist.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub absent: Vec<String>,
}

/// Asserts one setting of the configuration file the program saves: the file `EXCISE_CONFIG`
/// names in a process run, and the file the in-process runner hands the program. It reads the file
/// as it is now and never waits, so put a `settle` before it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpectConfig {
    /// The setting, as the dotted path of table names and the setting's own name that locate it
    /// in the configuration file: `runtime.theme`.
    pub key: String,
    /// The string the setting must be: `excise-light`.
    pub equals: String,
}

string_enum! {
    /// What must remain in the scenario scratch directory after the program exits.
    pub enum Residue {
        /// Nothing at all.
        None => "none",
    }
}

/// Waits for the program to exit and asserts how it ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpectExit {
    /// The exit code.
    pub code: i32,
    /// `true` asserts that the terminal is restored (alternate screen left, cursor visible, echo
    /// and canonical mode on); `false` asserts that it is not. After a `close` event there is no
    /// console left to inspect: the pseudo-terminal runner accepts `true` there without
    /// evaluating it and refuses `false`.
    pub terminal_restored: bool,
    /// What may be left behind.
    pub residue: Residue,
    /// The bound on the wait for the exit.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

/// Asserts that the metric called `metric` is within `budget`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpectBudget {
    /// The budget, including any scenario override.
    pub budget: Budget,
    /// The recorded metric to compare, either built in or produced by `measure`.
    pub metric: String,
}

string_enum! {
    /// Which end of a measurement a `measure` step marks.
    pub enum Marker {
        /// The measurement starts.
        Start => "start",
        /// The measurement stops and its elapsed time is recorded.
        Stop => "stop",
    }
}

/// Marks the start or stop of a named measurement. The elapsed milliseconds between the two are
/// recorded as the metric `name`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Measure {
    /// The metric name.
    pub name: String,
    /// Which end this step marks.
    pub marker: Marker,
}

/// Measures the program's idle output and CPU over a quiet window.
///
/// Sends nothing. Waits `after_ms`, then measures over `window_ms` the terminal output bytes and
/// the child's live CPU time, recording them as the metrics `idle_output_bytes` and
/// `idle_cpu_ms`. Both durations are unconditional, unlike every waiting step above: there is no
/// condition to satisfy and so no `timeout_ms`. Pseudo-terminal only: the in-process runner has no
/// separate process to sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Idle {
    /// How long to wait, sending nothing, before the measurement window starts.
    pub after_ms: u64,
    /// How long the measurement window lasts.
    pub window_ms: u64,
}

/// Waits until the program has processed everything sent so far.
///
/// The in-process runner drains its owner loop with the runtime's existing barrier. The
/// pseudo-terminal runner waits on semantic signals only (events, header state, frame counts),
/// never on the screen being idle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settle {
    /// The bound on the wait.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

/// Waits until the map on screen has caught up with the deletions the scenario has confirmed.
///
/// A deletion that removed entries leaves the map listing them until the program replaces it, with
/// a rebuild of the whole map or with a map without the entries, and the program treats a quit
/// while it does so as a cancellation, exit code 130. The step returns once the deletions
/// confirmed so far have finished and the replacement of the last one that removed anything has
/// landed, and then once a frame shows it. A deletion that removed nothing owes no refresh, so
/// after one the step returns at once.
///
/// The pseudo-terminal runner reads the program's `refresh_finished` event, and only one that
/// follows the last such deletion. The in-process runner drains its owner loop with the runtime's
/// barrier, which does not return while a rebuild or a publication is owed. A scenario needs a
/// `delete` step before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaitRefresh {
    /// The bound on the wait.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

/// Performs the ordinary confirmed quit: presses `q`, waits for the quit prompt, and confirms.
/// It does not assert the exit; follow it with `expect_exit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Quit {
    /// The bound on the wait for the prompt.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

/// A key to press.
///
/// Written as a single character (`"y"`, `"/"`, `" "`) or one of the [`KeyName::NAMES`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum KeyName {
    /// A printable character.
    Char(char),
    /// Enter.
    Enter,
    /// Escape.
    Esc,
    /// Backspace.
    Backspace,
    /// Tab.
    Tab,
    /// Arrow up.
    Up,
    /// Arrow down.
    Down,
    /// Arrow left.
    Left,
    /// Arrow right.
    Right,
    /// Page Up.
    PageUp,
    /// Page Down.
    PageDown,
}

/// A string that is neither a single printable character nor a named key.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error(
    "unknown key {0:?}: use one printable character or one of {names}",
    names = KeyName::NAMES.join(", ")
)]
pub struct KeyNameError(String);

impl KeyName {
    /// The names of the non-character keys.
    pub const NAMES: [&'static str; 10] = [
        "enter",
        "esc",
        "backspace",
        "tab",
        "up",
        "down",
        "left",
        "right",
        "page_up",
        "page_down",
    ];

    /// The name of a non-character key, or `None` for [`KeyName::Char`].
    #[must_use]
    pub const fn name(self) -> Option<&'static str> {
        match self {
            Self::Char(_) => None,
            Self::Enter => Some("enter"),
            Self::Esc => Some("esc"),
            Self::Backspace => Some("backspace"),
            Self::Tab => Some("tab"),
            Self::Up => Some("up"),
            Self::Down => Some("down"),
            Self::Left => Some("left"),
            Self::Right => Some("right"),
            Self::PageUp => Some("page_up"),
            Self::PageDown => Some("page_down"),
        }
    }
}

impl FromStr for KeyName {
    type Err = KeyNameError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let mut characters = text.chars();
        if let (Some(character), None) = (characters.next(), characters.next())
            && !character.is_control()
        {
            return Ok(Self::Char(character));
        }
        match text {
            "enter" => Ok(Self::Enter),
            "esc" => Ok(Self::Esc),
            "backspace" => Ok(Self::Backspace),
            "tab" => Ok(Self::Tab),
            "up" => Ok(Self::Up),
            "down" => Ok(Self::Down),
            "left" => Ok(Self::Left),
            "right" => Ok(Self::Right),
            "page_up" => Ok(Self::PageUp),
            "page_down" => Ok(Self::PageDown),
            _ => Err(KeyNameError(text.to_owned())),
        }
    }
}

impl fmt::Display for KeyName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Char(character) => write!(formatter, "{character}"),
            named => formatter.write_str(named.name().unwrap_or_default()),
        }
    }
}

impl TryFrom<String> for KeyName {
    type Error = KeyNameError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        text.parse()
    }
}

impl From<KeyName> for String {
    fn from(key: KeyName) -> Self {
        key.to_string()
    }
}
