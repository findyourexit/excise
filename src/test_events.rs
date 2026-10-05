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
//! | `hello` | `version`, `pid` | First line. `version` is the package version. |
//! | `frame` | `seq`, `inputs` | After every render that drew. `seq` counts drawn frames from 1; `inputs` counts terminal input events the owner loop has consumed so far. |
//! | `scan_complete` | `entries` | The initial scan finished and the map switched to its completed state. `entries` counts the scanned entries. |
//! | `quit_prompt` | | The quit dialog was built. |
//! | `deletion_finished` | `removed`, `failed` | A deletion worker reported. The counts come from its report. |
//! | `refresh_finished` | `outcome` | The map on screen has caught up with the deletions that removed entries: the map without them was published (`published`), or no map could be shown (`failed`), and no rebuild or publication is owed. Always after the `deletion_finished` it answers, and once for deletions whose refreshes overlapped. A deletion that removed nothing owes no refresh and emits none. |
//! | `exit` | `code` | An interactive run is about to return its exit code. The terminal, if it was entered, has already been restored, unless it never absorbed the restoration output within a bounded wait. A process that panics or is killed emits none. |

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Instant;

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

/// Reports that a render drew a frame.
#[inline]
pub(crate) fn frame() {
    if let Some(sink) = SINK.get() {
        sink.frame();
    }
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
    Hello { version: &'static str, pid: u32 },
    Frame { seq: u64, inputs: u64 },
    ScanComplete { entries: u64 },
    QuitPrompt,
    DeletionFinished { removed: u64, failed: u64 },
    RefreshFinished { outcome: RefreshOutcome },
    Exit { code: i32 },
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
            state: Mutex::new(State {
                writer: Some(writer),
                frames: 0,
                line: Vec::with_capacity(LINE_CAPACITY),
            }),
        };
        let hello = Event::Hello {
            version: env!("CARGO_PKG_VERSION"),
            pid: std::process::id(),
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

    fn frame(&self) {
        let inputs = self.inputs.load(Ordering::Relaxed);
        let mut state = self.lock();
        state.frames += 1;
        let event = Event::Frame {
            seq: state.frames,
            inputs,
        };
        self.emit_locked(&mut state, &event);
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

    #[test]
    fn every_kind_encodes_to_its_documented_line() {
        let cases = [
            (
                Event::Hello {
                    version: "1.2.3",
                    pid: 4242,
                },
                r#"{"v":1,"kind":"hello","version":"1.2.3","pid":4242,"t_us":7}"#,
            ),
            (
                Event::Frame { seq: 3, inputs: 2 },
                r#"{"v":1,"kind":"frame","seq":3,"inputs":2,"t_us":7}"#,
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
        assert_eq!(events[1]["seq"], 1);
        assert_eq!(events[2]["seq"], 2);
        assert_eq!(events[1]["inputs"], 2);
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
