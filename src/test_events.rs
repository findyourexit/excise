//! Internal test event channel.
//!
//! Tooling that drives a real Excise process needs synchronization points that terminal
//! text cannot provide reliably: a frame was drawn, the scan finished, the quit dialog
//! opened. Setting `EXCISE_TEST_EVENTS` to the path of a file that does not exist yet
//! makes Excise create that file and append one JSON object per line to it.
//!
//! This is an internal testing interface. It is not part of the v1 command-line,
//! configuration, or report contract, and `docs/development.md` is its only
//! documentation.
//!
//! # Behavior
//!
//! - **Unset.** Every emission site is one branch. No file, thread, or allocation exists.
//! - **Creation.** The file is created exclusively and an existing path is never
//!   opened. On Unix a symbolic link at the path is rejected rather than followed, and
//!   the file is private (mode `0600`). An empty or unusable value is a configuration
//!   error reported before terminal entry.
//! - **Writes.** Each event is one complete line, written with one `write_all` on the
//!   calling thread and then flushed. Nothing is synced to disk.
//! - **Failure.** After the first write error the channel disables itself. A reader that
//!   went away can never crash or block the interface.
//! - **Privacy.** Events carry counts, timings, and one fixed word for an outcome. They never
//!   carry names, paths, or any other scan data.
//!
//! # Protocol v1
//!
//! Every line is a JSON object that starts with `v` (the protocol version, `1`) and
//! `kind`, and ends with `t_us` (monotonic microseconds since the channel opened):
//!
//! | `kind` | Fields | Emitted |
//! |---|---|---|
//! | `hello` | `version`, `pid`, `frame_marks`, `input_barrier` | First line. `version` is the package version. `frame_marks` is `true`: an interactive run of this binary follows every `frame` event with a frame mark in the terminal output (see "Frame marks"). `input_barrier` is `true`: this binary answers input barrier requests (see "Input barrier"). Binaries from before the marks omit `frame_marks`, and binaries from before the barrier omit `input_barrier`. |
//! | `frame` | `seq`, `inputs`, `barriers` | After every render that drew, once the frame is queued for the terminal writer thread, which can still hold its bytes back (see "Frame marks"). `seq` counts drawn frames from 1; `inputs` counts terminal input events the owner loop has consumed so far; `barriers` counts the input barrier requests it has consumed so far, which are not inputs (see "Input barrier"). Binaries from before the barrier omit `barriers`. |
//! | `scan_complete` | `entries` | The initial scan finished and the map switched to its completed state. `entries` counts the scanned entries. |
//! | `quit_prompt` | | The quit dialog was built. |
//! | `deletion_finished` | `removed`, `failed` | A deletion worker reported. The counts come from its report. |
//! | `refresh_finished` | `outcome` | The map on screen has caught up with the deletions that removed entries: the map without them was published (`published`), or no map could be shown (`failed`), and no rebuild or publication is owed. Always after the `deletion_finished` it answers, and once for deletions whose refreshes overlapped. A deletion that removed nothing owes no refresh and emits none. |
//! | `exit` | `code` | An interactive run is about to return its exit code. The terminal, if it was entered, has already been restored, unless it never absorbed the restoration output within a bounded wait. A process that panics or is killed emits none. |
//!
//! # Frame marks
//!
//! A `frame` event says that a frame was *queued* for the terminal writer thread, not that its
//! bytes reached the terminal. That thread writes as slowly as the terminal reads, so the
//! screen can lag the event by any amount, and a reader that must know which frame the screen
//! shows cannot take it from the event. While the channel is open, the owner loop therefore
//! queues a mark behind every frame it draws, through the same writer as the frame:
//!
//! ```text
//! ESC ] 9471 ; excise-frame=<seq> BEL      that is "\x1b]9471;excise-frame=<seq>\x07"
//! ```
//!
//! `<seq>` is the decimal `seq` of the `frame` event the mark follows. The writer never
//! reorders or drops what it is handed, so the mark reaches the terminal after every byte of
//! its frame and before every byte of a later frame and of the restoration sequence: a reader
//! that has read up to a mark has read exactly the frames up to that `seq`.
//!
//! - **One mark per event.** `frame` returns the `seq` only when it wrote the event and the
//!   channel is still open afterwards. A frame whose event was not written, because the channel
//!   had already disabled itself or this very write failed, gets no mark either.
//! - **Only with the variable.** Without `EXCISE_TEST_EVENTS`, `frame` returns `None`: nothing
//!   is written to the terminal and nothing is allocated for it.
//! - **Only for an interactive run.** An interactive run has a terminal writer. A headless run
//!   draws no frames and writes no marks, and a run in process (a test or a benchmark) reports
//!   its `frame` events and has no terminal to mark.
//! - **`hello.frame_marks`.** It tells a reader that this binary writes marks. A binary from
//!   before them omits it, and a reader of one must fall back to reading until the output is
//!   quiet.
//!
//! # Input barrier
//!
//! A reader that writes keys to the terminal must know when the program has read them and drawn
//! what they did. It cannot take that from `inputs`: a terminal write is not one decoded input
//! event. crossterm decodes `ESC DEL` as Alt+Backspace when both bytes arrive in one read and as
//! Esc and then Backspace when they do not, and `ESC ESC [ A` as Esc, `[`, and `A`, so the number
//! of writes and `inputs` need not agree, and counting one against the other proves nothing about
//! what the program has read. The program reads its input in order, and a *barrier request* uses
//! that:
//!
//! ```text
//! 0x1D      Ctrl+], one byte, written to the program's terminal input
//! ```
//!
//! crossterm's Unix input parser decodes the byte as the key `5` with the Control modifier, and,
//! when an unread lone `ESC` byte immediately precedes it, as the same key with Alt added: the
//! parser merges `ESC` and the byte after it into one Alt key. Both are requests. Every other
//! modifier combination is not one, and neither is an event that is not a key press. That is the
//! decoding of a terminal whose input crossterm's Unix parser reads; the Windows console builds
//! its key events from console input records instead, and this was not checked there. The owner
//! loop handles a request right after it reads it, before anything else is done with the event:
//!
//! - **Not an input.** A request is not counted in `inputs`, no key binding sees it, and it does
//!   not re-arm the selected tile's sheen. An `ESC` that merged into it is neither counted nor
//!   handled as the Esc key.
//! - **Counted.** The request counts one in `barriers`.
//! - **Drawn.** The interface is marked for a redraw and the batch of inputs ends, so the next
//!   frame is drawn for certain, and not skipped behind a frame that the terminal is still
//!   draining.
//! - **Answered.** The `frame` event of that frame carries `barriers`: the number of requests
//!   consumed before the frame was drawn. Requests count from 1 in the order the program read
//!   them, and the first `frame` whose `barriers` is at least a request's number answers it.
//!
//! The frame is drawn after the request was consumed, and the program reads its input in order,
//! so the answering frame shows every input that was read before the request: an answered request
//! proves that everything written before it was read and drawn, whatever the number of input
//! events the writes made. The frame's mark (see "Frame marks") says when the screen shows it. A
//! request does not wait for background work that an input started, such as a deletion or a
//! rescan.
//!
//! - **A key boundary.** A request is recognized only after whole keys. A byte inside an
//!   unfinished escape sequence belongs to that sequence, so a request written there is not
//!   recognized. A request right behind an `ESC` that is still unread is read with it as one key,
//!   so a reader that wants the Esc key read as one waits for a frame that counts it before it
//!   writes a request.
//! - **Only with the variable.** Without `EXCISE_TEST_EVENTS`, `barrier_request` returns `false`
//!   at the one `SINK.get()` branch that every emission site has: no counter exists, nothing is
//!   drawn for the byte, and it reaches the key handler as Ctrl+5, which nothing is bound to.
//! - **`hello.input_barrier`.** It tells a reader that this binary answers requests. A binary
//!   from before it omits the field and writes no `barriers`, and a request written to it is never
//!   answered. Both fields are additive, so the protocol version stays 1.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Instant;

use crossterm::event::{Event as TerminalEvent, KeyCode, KeyEventKind, KeyModifiers};
use serde::Serialize;

use crate::error::AppError;
use crate::native_path::{safe_display_path_text, safe_display_text};

const ENVIRONMENT_VARIABLE: &str = "EXCISE_TEST_EVENTS";
const PROTOCOL_VERSION: u32 = 1;
/// Room for the longest fixed-width line, so the reused encode buffer normally allocates once.
const LINE_CAPACITY: usize = 128;

static SINK: OnceLock<Sink<File>> = OnceLock::new();

/// Opens the event file named by `EXCISE_TEST_EVENTS`. Does nothing when the variable is unset.
///
/// # Errors
/// Returns a configuration error when the value is empty, names an existing path, or names
/// a file that cannot be created or written.
pub(crate) fn init_from_env() -> Result<(), AppError> {
    let Some(value) = std::env::var_os(ENVIRONMENT_VARIABLE) else {
        return Ok(());
    };
    let path = Path::new(&value);
    let file = create_events_file(path)?;
    let sink = Sink::start(file).map_err(|error| {
        config_error(&format!(
            "{ENVIRONMENT_VARIABLE} could not write to {}: {error}",
            safe_display_path_text(path)
        ))
    })?;
    SINK.set(sink).map_err(|_| {
        AppError::Invariant("the test event channel was initialized twice".to_string())
    })
}

/// Counts one terminal input event consumed by the owner loop.
#[inline]
pub(crate) fn input_consumed() {
    if let Some(sink) = SINK.get() {
        sink.input_consumed();
    }
}

/// Counts one input barrier request, and returns `true`, when the channel is open and `event` is
/// one (see "Input barrier" in the module documentation). The owner loop asks it of every
/// terminal event it reads, before it counts the event as an input, and handles a `true` as a
/// request instead of a key. Without the channel it returns `false` after the one `SINK.get()`
/// branch: nothing is counted, and the event is an ordinary key.
#[inline]
pub(crate) fn barrier_request(event: &TerminalEvent) -> bool {
    SINK.get().is_some_and(|sink| sink.barrier_request(event))
}

/// Whether `event` is an input barrier request: the key press Ctrl+5, which is what crossterm
/// decodes the byte `0x1D` to, or Ctrl+Alt+5, which is what it decodes that byte to right behind
/// an unread lone `ESC`. Any other modifier combination, any other key, any key event that is not
/// a press, and any event that is not a key is not a request.
fn is_barrier_request(event: &TerminalEvent) -> bool {
    let TerminalEvent::Key(key) = event else {
        return false;
    };
    key.kind == KeyEventKind::Press
        && key.code == KeyCode::Char('5')
        && (key.modifiers == KeyModifiers::CONTROL
            || key.modifiers == (KeyModifiers::CONTROL | KeyModifiers::ALT))
}

/// Reports that a render drew a frame. Returns the `seq` of the `frame` event it wrote, or
/// `None` when no event was written: the channel is not open, or it ended with a write error,
/// this frame's own write included.
///
/// The caller marks the frame in the terminal output with [`frame_mark`] when, and only when,
/// it gets a `seq`, so the marks are exactly the `frame` events (see "Frame marks" in the
/// module documentation).
#[inline]
pub(crate) fn frame() -> Option<u64> {
    SINK.get().and_then(Sink::frame)
}

/// The bytes that mark frame `seq` in the terminal output: ESC `]` `9471` `;` `excise-frame=`,
/// the decimal `seq`, and BEL.
pub(crate) fn frame_mark(seq: u64) -> Vec<u8> {
    format!("\x1b]9471;excise-frame={seq}\x07").into_bytes()
}

/// Reports that the initial scan finished after `entries` scanned entries.
#[inline]
pub(crate) fn scan_complete(entries: u64) {
    if let Some(sink) = SINK.get() {
        sink.emit(&Event::ScanComplete { entries });
    }
}

/// Reports that the quit dialog was built.
#[inline]
pub(crate) fn quit_prompt() {
    if let Some(sink) = SINK.get() {
        sink.emit(&Event::QuitPrompt);
    }
}

/// Reports a finished deletion with the entry counts from its report.
#[inline]
pub(crate) fn deletion_finished(removed: u64, failed: u64) {
    if let Some(sink) = SINK.get() {
        sink.emit(&Event::DeletionFinished { removed, failed });
    }
}

/// How the refresh that a deletion owed ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RefreshOutcome {
    /// The map without the removed entries was published, and is the one on screen.
    Published,
    /// No map could be shown: the store thread gave up its generation.
    Failed,
}

/// Reports that the map on screen has caught up with the deletions that removed entries.
#[inline]
pub(crate) fn refresh_finished(outcome: RefreshOutcome) {
    if let Some(sink) = SINK.get() {
        sink.emit(&Event::RefreshFinished { outcome });
    }
}

/// Reports the exit code an interactive run is about to return.
#[inline]
pub(crate) fn exit(code: i32) {
    if let Some(sink) = SINK.get() {
        sink.emit(&Event::Exit { code });
    }
}

fn create_events_file(path: &Path) -> Result<File, AppError> {
    if path.as_os_str().is_empty() {
        return Err(config_error(&format!(
            "{ENVIRONMENT_VARIABLE} must name a new file, not an empty path"
        )));
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path).map_err(|error| {
        let shown = safe_display_path_text(path);
        config_error(&match error.kind() {
            io::ErrorKind::AlreadyExists => {
                format!("{ENVIRONMENT_VARIABLE} must name a new file, but {shown} already exists")
            }
            io::ErrorKind::NotFound => format!(
                "{ENVIRONMENT_VARIABLE} must name a file in an existing directory, but the directory of {shown} does not exist"
            ),
            _ => format!("{ENVIRONMENT_VARIABLE} could not create {shown}: {error}"),
        })
    })
}

/// Escapes untrusted text, such as a hostile path, before it reaches a diagnostic.
fn config_error(message: &str) -> AppError {
    AppError::Config(safe_display_text(message))
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Event {
    Hello {
        version: &'static str,
        pid: u32,
        frame_marks: bool,
        input_barrier: bool,
    },
    Frame {
        seq: u64,
        inputs: u64,
        barriers: u64,
    },
    ScanComplete {
        entries: u64,
    },
    QuitPrompt,
    DeletionFinished {
        removed: u64,
        failed: u64,
    },
    RefreshFinished {
        outcome: RefreshOutcome,
    },
    Exit {
        code: i32,
    },
}

/// One serialized line: the version, the event, then its timestamp.
#[derive(Serialize)]
struct Line<'a> {
    v: u32,
    #[serde(flatten)]
    event: &'a Event,
    t_us: u64,
}

/// Replaces `line` with the newline-terminated encoding of `event`.
fn encode_line(line: &mut Vec<u8>, t_us: u64, event: &Event) -> io::Result<()> {
    line.clear();
    serde_json::to_writer(
        &mut *line,
        &Line {
            v: PROTOCOL_VERSION,
            event,
            t_us,
        },
    )?;
    line.push(b'\n');
    Ok(())
}

/// Writes and numbers events. One instance serves the whole process.
struct Sink<W> {
    /// Origin of every `t_us`.
    started: Instant,
    /// Terminal input events the owner loop has consumed.
    inputs: AtomicU64,
    /// Barrier requests the owner loop has consumed. A request is not an input.
    barriers: AtomicU64,
    state: Mutex<State<W>>,
}

struct State<W> {
    /// Dropped after the first write error, which ends the channel.
    writer: Option<W>,
    /// Frames drawn so far.
    frames: u64,
    /// Reused buffer that holds the line being written.
    line: Vec<u8>,
}

impl<W: Write> State<W> {
    fn write(&mut self, t_us: u64, event: &Event) -> io::Result<()> {
        let Some(writer) = self.writer.as_mut() else {
            return Ok(());
        };
        encode_line(&mut self.line, t_us, event)?;
        writer.write_all(&self.line)?;
        writer.flush()
    }
}

impl<W: Write> Sink<W> {
    /// Opens the channel by writing its `hello` line.
    fn start(writer: W) -> io::Result<Self> {
        let sink = Self {
            started: Instant::now(),
            inputs: AtomicU64::new(0),
            barriers: AtomicU64::new(0),
            state: Mutex::new(State {
                writer: Some(writer),
                frames: 0,
                line: Vec::with_capacity(LINE_CAPACITY),
            }),
        };
        let hello = Event::Hello {
            version: env!("CARGO_PKG_VERSION"),
            pid: std::process::id(),
            frame_marks: true,
            input_barrier: true,
        };
        sink.lock().write(sink.elapsed_micros(), &hello)?;
        Ok(sink)
    }

    fn lock(&self) -> MutexGuard<'_, State<W>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn elapsed_micros(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_micros()).unwrap_or(u64::MAX)
    }

    fn input_consumed(&self) {
        self.inputs.fetch_add(1, Ordering::Relaxed);
    }

    /// Counts `event` when it is a barrier request, and returns whether it was one.
    fn barrier_request(&self, event: &TerminalEvent) -> bool {
        let request = is_barrier_request(event);
        if request {
            self.barriers.fetch_add(1, Ordering::Relaxed);
        }
        request
    }

    /// Writes the `frame` event for the frame just queued and returns its `seq`, or `None` once
    /// the channel has disabled itself, this very write failing included: a frame whose event
    /// was not written gets no mark, so the marks are exactly the events.
    fn frame(&self) -> Option<u64> {
        let inputs = self.inputs.load(Ordering::Relaxed);
        let barriers = self.barriers.load(Ordering::Relaxed);
        let mut state = self.lock();
        state.frames += 1;
        let seq = state.frames;
        self.emit_locked(
            &mut state,
            &Event::Frame {
                seq,
                inputs,
                barriers,
            },
        );
        state.writer.is_some().then_some(seq)
    }

    fn emit(&self, event: &Event) {
        self.emit_locked(&mut self.lock(), event);
    }

    fn emit_locked(&self, state: &mut State<W>, event: &Event) {
        if state.write(self.elapsed_micros(), event).is_err() {
            state.writer = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    use crossterm::event::KeyEvent;
    use serde_json::Value;

    use super::*;

    /// Records every `write` call. Calls from number `fail_from` onward fail.
    #[derive(Clone)]
    struct Recorder {
        writes: Arc<Mutex<Vec<Vec<u8>>>>,
        attempts: Arc<AtomicUsize>,
        fail_from: usize,
    }

    impl Recorder {
        fn new(fail_from: usize) -> Self {
            Self {
                writes: Arc::default(),
                attempts: Arc::default(),
                fail_from,
            }
        }

        fn writes(&self) -> Vec<Vec<u8>> {
            self.writes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }
    }

    impl Write for Recorder {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.attempts.fetch_add(1, Ordering::Relaxed) >= self.fail_from {
                return Err(io::Error::other("the reader went away"));
            }
            self.writes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(bytes.to_vec());
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn parse(write: &[u8]) -> Value {
        let text = std::str::from_utf8(write).expect("a line should be UTF-8");
        assert!(
            text.ends_with('\n') && text.matches('\n').count() == 1,
            "a write must be exactly one complete line: {text:?}"
        );
        serde_json::from_str(text).expect("a line should be a JSON object")
    }

    fn encoded(event: &Event, t_us: u64) -> String {
        let mut line = Vec::new();
        encode_line(&mut line, t_us, event).expect("encoding should succeed");
        String::from_utf8(line).expect("a line should be UTF-8")
    }

    /// A key press, as crossterm decodes the bytes a terminal sends.
    fn key_press(code: KeyCode, modifiers: KeyModifiers) -> TerminalEvent {
        TerminalEvent::Key(KeyEvent::new(code, modifiers))
    }

    /// What crossterm decodes the byte `0x1D` to.
    fn ctrl_5() -> TerminalEvent {
        key_press(KeyCode::Char('5'), KeyModifiers::CONTROL)
    }

    /// What crossterm decodes the byte `0x1D` to right behind an unread lone `ESC`.
    fn ctrl_alt_5() -> TerminalEvent {
        key_press(
            KeyCode::Char('5'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        )
    }

    #[test]
    fn every_kind_encodes_to_its_documented_line() {
        let cases = [
            (
                Event::Hello {
                    version: "1.2.3",
                    pid: 4242,
                    frame_marks: true,
                    input_barrier: true,
                },
                r#"{"v":1,"kind":"hello","version":"1.2.3","pid":4242,"frame_marks":true,"input_barrier":true,"t_us":7}"#,
            ),
            (
                Event::Frame {
                    seq: 3,
                    inputs: 2,
                    barriers: 1,
                },
                r#"{"v":1,"kind":"frame","seq":3,"inputs":2,"barriers":1,"t_us":7}"#,
            ),
            (
                Event::ScanComplete { entries: 12 },
                r#"{"v":1,"kind":"scan_complete","entries":12,"t_us":7}"#,
            ),
            (
                Event::QuitPrompt,
                r#"{"v":1,"kind":"quit_prompt","t_us":7}"#,
            ),
            (
                Event::DeletionFinished {
                    removed: 5,
                    failed: 1,
                },
                r#"{"v":1,"kind":"deletion_finished","removed":5,"failed":1,"t_us":7}"#,
            ),
            (
                Event::RefreshFinished {
                    outcome: RefreshOutcome::Published,
                },
                r#"{"v":1,"kind":"refresh_finished","outcome":"published","t_us":7}"#,
            ),
            (
                Event::RefreshFinished {
                    outcome: RefreshOutcome::Failed,
                },
                r#"{"v":1,"kind":"refresh_finished","outcome":"failed","t_us":7}"#,
            ),
            (
                Event::Exit { code: 130 },
                r#"{"v":1,"kind":"exit","code":130,"t_us":7}"#,
            ),
        ];
        for (event, expected) in cases {
            assert_eq!(encoded(&event, 7), format!("{expected}\n"));
        }
    }

    #[test]
    fn counters_at_their_limits_stay_exact_integers() {
        let line = encoded(
            &Event::DeletionFinished {
                removed: u64::MAX,
                failed: 0,
            },
            u64::MAX,
        );
        let value: Value = serde_json::from_str(&line).expect("a line should be JSON");
        assert_eq!(value["removed"].as_u64(), Some(u64::MAX));
        assert_eq!(value["t_us"].as_u64(), Some(u64::MAX));
    }

    #[test]
    fn a_sink_opens_with_hello_and_writes_one_line_per_event() {
        let recorder = Recorder::new(usize::MAX);
        let sink = Sink::start(recorder.clone()).expect("the first line should be written");
        sink.input_consumed();
        sink.input_consumed();
        sink.frame();
        sink.frame();
        sink.emit(&Event::ScanComplete { entries: 9 });
        sink.emit(&Event::Exit { code: 0 });

        let events: Vec<Value> = recorder.writes().iter().map(|write| parse(write)).collect();
        let kinds: Vec<&str> = events
            .iter()
            .map(|event| event["kind"].as_str().expect("every event has a kind"))
            .collect();
        assert_eq!(kinds, ["hello", "frame", "frame", "scan_complete", "exit"]);
        assert_eq!(events[0]["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(events[0]["pid"], std::process::id());
        assert_eq!(events[0]["frame_marks"], true);
        assert_eq!(events[0]["input_barrier"], true);
        assert_eq!(events[1]["seq"], 1);
        assert_eq!(events[2]["seq"], 2);
        assert_eq!(events[1]["inputs"], 2);
        assert_eq!(events[1]["barriers"], 0);
        assert_eq!(events[3]["entries"], 9);
        assert_eq!(events[4]["code"], 0);
        let times: Vec<u64> = events
            .iter()
            .map(|event| event["t_us"].as_u64().expect("every event has a timestamp"))
            .collect();
        assert!(
            times.windows(2).all(|pair| pair[0] <= pair[1]),
            "timestamps must never go backwards: {times:?}"
        );
    }

    #[test]
    fn frames_report_the_inputs_consumed_before_them() {
        let recorder = Recorder::new(usize::MAX);
        let sink = Sink::start(recorder.clone()).expect("the first line should be written");
        sink.frame();
        sink.input_consumed();
        sink.frame();

        let inputs: Vec<u64> = recorder
            .writes()
            .iter()
            .map(|write| parse(write))
            .filter(|event| event["kind"] == "frame")
            .map(|event| event["inputs"].as_u64().expect("a frame reports inputs"))
            .collect();
        assert_eq!(inputs, [0, 1]);
    }

    #[test]
    fn frames_report_the_barrier_requests_consumed_before_them() {
        let recorder = Recorder::new(usize::MAX);
        let sink = Sink::start(recorder.clone()).expect("the first line should be written");
        let plain_5 = key_press(KeyCode::Char('5'), KeyModifiers::NONE);
        let resize = TerminalEvent::Resize(80, 24);
        sink.input_consumed();
        assert!(!sink.barrier_request(&plain_5), "a plain 5 is a key");
        sink.frame();
        assert!(sink.barrier_request(&ctrl_5()));
        sink.frame();
        assert!(sink.barrier_request(&ctrl_5()));
        assert!(sink.barrier_request(&ctrl_alt_5()));
        assert!(!sink.barrier_request(&resize), "a resize is not a request");
        sink.frame();
        sink.input_consumed();
        sink.frame();

        let frames: Vec<Value> = recorder
            .writes()
            .iter()
            .map(|write| parse(write))
            .filter(|event| event["kind"] == "frame")
            .collect();
        let counts = |field: &str| -> Vec<u64> {
            frames
                .iter()
                .map(|frame| frame[field].as_u64().expect("a frame reports its counters"))
                .collect()
        };
        assert_eq!(counts("barriers"), [0, 1, 3, 3]);
        assert_eq!(counts("inputs"), [1, 1, 1, 2], "a request is not an input");
    }

    #[test]
    fn only_ctrl_and_ctrl_alt_make_a_five_a_barrier_request() {
        let requests = [
            KeyModifiers::CONTROL,
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ];
        for bits in 0..=u8::MAX {
            let modifiers = KeyModifiers::from_bits_truncate(bits);
            let event = key_press(KeyCode::Char('5'), modifiers);
            assert_eq!(
                is_barrier_request(&event),
                requests.contains(&modifiers),
                "{modifiers:?}"
            );
        }
    }

    #[test]
    fn no_other_key_and_no_other_event_is_a_barrier_request() {
        let ctrl = KeyModifiers::CONTROL;
        let release = KeyEvent::new_with_kind(KeyCode::Char('5'), ctrl, KeyEventKind::Release);
        let repeat = KeyEvent::new_with_kind(KeyCode::Char('5'), ctrl, KeyEventKind::Repeat);
        let others = [
            key_press(KeyCode::Char('c'), ctrl),
            key_press(KeyCode::Char('4'), ctrl),
            key_press(KeyCode::Char('6'), ctrl),
            key_press(KeyCode::Char(']'), ctrl),
            key_press(KeyCode::Enter, ctrl),
            TerminalEvent::Key(release),
            TerminalEvent::Key(repeat),
            TerminalEvent::Resize(80, 24),
            TerminalEvent::FocusGained,
        ];
        for event in others {
            assert!(!is_barrier_request(&event), "{event:?}");
        }
    }

    #[test]
    fn a_request_is_not_recognized_while_the_channel_is_closed() {
        // Only `init_from_env` opens the process-wide sink, and no unit test sets the variable it
        // reads, so the sink is closed here, as it is when `EXCISE_TEST_EVENTS` is unset.
        assert!(SINK.get().is_none());
        assert!(!barrier_request(&ctrl_5()));
        assert!(!barrier_request(&ctrl_alt_5()));
    }

    #[test]
    fn a_write_error_ends_the_channel_without_further_attempts() {
        // The hello line and the first frame succeed; the next write fails.
        let recorder = Recorder::new(2);
        let sink = Sink::start(recorder.clone()).expect("the first line should be written");
        sink.frame();
        sink.frame();
        sink.frame();
        sink.emit(&Event::Exit { code: 0 });

        assert_eq!(recorder.writes().len(), 2);
        assert_eq!(
            recorder.attempts.load(Ordering::Relaxed),
            3,
            "the disabled channel must not touch the writer again"
        );
    }

    #[test]
    fn a_frame_returns_the_seq_of_the_event_it_wrote() {
        let recorder = Recorder::new(usize::MAX);
        let sink = Sink::start(recorder.clone()).expect("the first line should be written");

        assert_eq!(sink.frame(), Some(1));
        sink.emit(&Event::ScanComplete { entries: 9 });
        assert_eq!(sink.frame(), Some(2), "only frames are numbered");

        let written: Vec<u64> = recorder
            .writes()
            .iter()
            .map(|write| parse(write))
            .filter(|event| event["kind"] == "frame")
            .map(|event| event["seq"].as_u64().expect("a frame reports its seq"))
            .collect();
        assert_eq!(written, [1, 2], "the seq returned is the seq written");
    }

    #[test]
    fn a_frame_whose_event_was_not_written_returns_no_seq() {
        // The hello line and two frames succeed; every later write fails.
        let recorder = Recorder::new(3);
        let sink = Sink::start(recorder.clone()).expect("the first line should be written");

        assert_eq!(sink.frame(), Some(1));
        assert_eq!(sink.frame(), Some(2));
        assert_eq!(
            sink.frame(),
            None,
            "the write that failed wrote no event, so the frame gets no mark"
        );
        assert_eq!(sink.frame(), None, "a channel that ended marks nothing");

        let frame_events = recorder
            .writes()
            .iter()
            .map(|write| parse(write))
            .filter(|event| event["kind"] == "frame")
            .count();
        assert_eq!(frame_events, 2, "one mark for each event that was written");
    }

    #[test]
    fn a_frame_mark_is_the_documented_sequence() {
        assert_eq!(frame_mark(1), b"\x1b]9471;excise-frame=1\x07");
        assert_eq!(frame_mark(12_345), b"\x1b]9471;excise-frame=12345\x07");
        assert_eq!(
            frame_mark(u64::MAX),
            b"\x1b]9471;excise-frame=18446744073709551615\x07"
        );
    }

    #[test]
    fn a_failed_hello_prevents_the_channel_from_opening() {
        assert!(Sink::start(Recorder::new(0)).is_err());
    }

    #[test]
    fn a_new_path_is_created_empty() {
        let directory = tempfile::tempdir().expect("scratch directory should exist");
        let path = directory.path().join("events.jsonl");

        let file = create_events_file(&path).expect("a new path should be accepted");
        drop(file);

        assert_eq!(
            std::fs::metadata(&path)
                .expect("the file should exist")
                .len(),
            0
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_new_file_is_private_to_its_owner() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().expect("scratch directory should exist");
        let path = directory.path().join("events.jsonl");

        drop(create_events_file(&path).expect("a new path should be accepted"));

        let mode = std::fs::metadata(&path)
            .expect("the file should exist")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn an_existing_file_is_rejected_and_left_untouched() {
        let directory = tempfile::tempdir().expect("scratch directory should exist");
        let path = directory.path().join("events.jsonl");
        std::fs::write(&path, b"earlier run\n").expect("fixture should be written");

        let error = create_events_file(&path).expect_err("an existing file must be rejected");

        assert!(matches!(error, AppError::Config(_)), "{error:?}");
        assert!(error.to_string().contains("already exists"), "{error}");
        assert!(error.to_string().contains(ENVIRONMENT_VARIABLE), "{error}");
        assert_eq!(
            std::fs::read(&path).expect("the file should still exist"),
            b"earlier run\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symbolic_link_is_never_followed() {
        let directory = tempfile::tempdir().expect("scratch directory should exist");
        let target = directory.path().join("elsewhere");
        let link = directory.path().join("events.jsonl");
        std::os::unix::fs::symlink(&target, &link).expect("link should be created");

        let error = create_events_file(&link).expect_err("a link must be rejected");

        assert!(error.to_string().contains("already exists"), "{error}");
        assert!(!target.exists(), "the link target must not be created");
    }

    #[test]
    fn a_missing_directory_is_a_configuration_error() {
        let directory = tempfile::tempdir().expect("scratch directory should exist");
        let path = directory.path().join("missing").join("events.jsonl");

        let error = create_events_file(&path).expect_err("a missing directory must be rejected");

        assert!(matches!(error, AppError::Config(_)), "{error:?}");
        assert!(error.to_string().contains("directory"), "{error}");
        assert!(!directory.path().join("missing").exists());
    }

    #[test]
    fn an_empty_path_is_a_configuration_error() {
        let error = create_events_file(Path::new("")).expect_err("an empty path must be rejected");

        assert!(matches!(error, AppError::Config(_)), "{error:?}");
        assert!(error.to_string().contains("empty"), "{error}");
    }

    #[test]
    fn hostile_path_text_is_escaped_in_errors() {
        let directory = tempfile::tempdir().expect("scratch directory should exist");
        let path = directory
            .path()
            .join("missing\n\u{202e}\u{1b}[31m")
            .join("events.jsonl");

        let message = create_events_file(&path)
            .expect_err("a missing directory must be rejected")
            .to_string();

        assert!(message.contains("[deceptive]"), "{message}");
        assert!(message.contains("\\u{202e}"), "{message}");
        assert!(message.contains("\\x1b"), "{message}");
        assert!(!message.chars().any(char::is_control), "{message:?}");
        assert!(!message.contains('\u{202e}'));
    }
}
