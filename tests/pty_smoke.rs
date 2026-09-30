use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use portable_pty::{Child, ChildKiller, CommandBuilder, ExitStatus, PtySize, native_pty_system};
use serde_json::{Map, Value};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const EXIT_TIMEOUT: Duration = Duration::from_secs(5);
const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(1);
const FIRST_FRAME_BUDGET: Duration = Duration::from_millis(250);
const INPUT_FRAME_BUDGET: Duration = Duration::from_millis(50);
const NORMAL_QUIT_BUDGET: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug)]
struct PtyMetrics {
    /// Process spawn to the first `frame` event.
    first_frame: Duration,
    /// Key write to the first `frame` event that reflects it.
    input_to_frame: Duration,
    /// Quit confirmation write to process exit.
    normal_quit: Duration,
}

type SharedOutput = Arc<Mutex<Vec<u8>>>;
type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;
/// Serializes complete native terminal sessions so one PTY backend is cleaned
/// up before another test creates its own session.
fn pty_session_guard() -> MutexGuard<'static, ()> {
    static SESSION: OnceLock<Mutex<()>> = OnceLock::new();

    SESSION
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct ChildGuard {
    killer: Box<dyn ChildKiller + Send + Sync>,
    armed: bool,
}

impl ChildGuard {
    fn new(killer: Box<dyn ChildKiller + Send + Sync>) -> Self {
        Self {
            killer,
            armed: true,
        }
    }

    const fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.killer.kill();
        }
    }
}

/// One protocol-v1 line from the test event channel.
#[derive(Clone, Debug)]
struct TestEvent {
    kind: String,
    /// Every field of the line, `v`, `kind`, and `t_us` included.
    fields: Map<String, Value>,
    /// When this test read the line, not when Excise wrote it.
    observed: Instant,
}

impl TestEvent {
    fn number(&self, field: &str) -> Option<u64> {
        self.fields.get(field).and_then(Value::as_u64)
    }
}

fn parse_event(line: &[u8], observed: Instant) -> anyhow::Result<TestEvent> {
    let text = std::str::from_utf8(line).context("event line is not UTF-8")?;
    let Value::Object(fields) =
        serde_json::from_str(text).with_context(|| format!("event line is not JSON: {text:?}"))?
    else {
        bail!("event line is not a JSON object: {text:?}");
    };
    if fields.get("v").and_then(Value::as_u64) != Some(1) {
        bail!("event line does not carry protocol version 1: {text:?}");
    }
    if fields.get("t_us").and_then(Value::as_u64).is_none() {
        bail!("event line has no numeric `t_us`: {text:?}");
    }
    let Some(kind) = fields
        .get("kind")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        bail!("event line has no string `kind`: {text:?}");
    };
    Ok(TestEvent {
        kind,
        fields,
        observed,
    })
}

/// Follows the file Excise appends test events to.
///
/// Waits look only at events after the previous match, so a sequence of waits
/// also checks the order in which the events arrived.
struct EventTail {
    path: PathBuf,
    /// Opened once Excise has created the file.
    file: Option<File>,
    /// Every byte read so far.
    raw: Vec<u8>,
    /// `raw[..parsed]` has been split into events. The rest is an incomplete line.
    parsed: usize,
    events: Vec<TestEvent>,
    /// Index of the first event a wait may still match.
    next: usize,
}

impl EventTail {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            file: None,
            raw: Vec::new(),
            parsed: 0,
            events: Vec::new(),
            next: 0,
        }
    }

    /// Reads what Excise has appended since the last call and parses every complete line.
    fn poll(&mut self) -> anyhow::Result<()> {
        if self.file.is_none() {
            match File::open(&self.path) {
                Ok(file) => self.file = Some(file),
                Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error).context("failed to open the test event file"),
            }
        }
        let Some(file) = self.file.as_mut() else {
            return Ok(());
        };
        let mut chunk = [0_u8; 4096];
        loop {
            let read = file
                .read(&mut chunk)
                .context("failed to read the test event file")?;
            if read == 0 {
                break;
            }
            self.raw.extend_from_slice(&chunk[..read]);
        }
        let observed = Instant::now();
        while let Some(newline) = self.raw[self.parsed..]
            .iter()
            .position(|byte| *byte == b'\n')
        {
            let end = self.parsed + newline;
            self.events
                .push(parse_event(&self.raw[self.parsed..end], observed)?);
            self.parsed = end + 1;
        }
        Ok(())
    }

    /// Waits for the first event after the previous match that satisfies `matches`.
    fn wait_for(
        &mut self,
        what: &str,
        timeout: Duration,
        matches: impl Fn(&TestEvent) -> bool,
    ) -> anyhow::Result<TestEvent> {
        let deadline = Instant::now() + timeout;
        loop {
            self.poll()?;
            if let Some(offset) = self.events[self.next..].iter().position(&matches) {
                let index = self.next + offset;
                self.next = index + 1;
                return Ok(self.events[index].clone());
            }
            if Instant::now() >= deadline {
                bail!("timed out waiting for {what}; {}", self.summary());
            }
            thread::sleep(EVENT_POLL_INTERVAL);
        }
    }

    /// The `inputs` total of the latest frame read so far, or 0 before any frame.
    fn inputs_consumed(&self) -> u64 {
        self.events
            .iter()
            .rev()
            .find(|event| event.kind == "frame")
            .and_then(|event| event.number("inputs"))
            .unwrap_or(0)
    }

    fn summary(&self) -> String {
        let kinds: Vec<&str> = self
            .events
            .iter()
            .rev()
            .take(12)
            .map(|event| event.kind.as_str())
            .collect();
        format!(
            "{} events were read, the latest were {:?}",
            self.events.len(),
            kinds.into_iter().rev().collect::<Vec<_>>()
        )
    }
}

/// Everything one PTY session produced.
struct PtyRun {
    status: ExitStatus,
    /// The terminal bytes Excise wrote, decoded lossily.
    output: String,
    /// Recorded for the normal interaction only.
    metrics: Option<PtyMetrics>,
    /// The test events in the order Excise emitted them.
    events: Vec<TestEvent>,
    /// The event file exactly as Excise wrote it.
    events_raw: Vec<u8>,
    child_id: Option<u32>,
}

/// Files created below a scan root before Excise starts.
struct Fixture<'a> {
    /// Directory Excise scans, created inside the session's scratch directory.
    root: &'a str,
    /// Files below `root`. Missing parent directories are created.
    files: &'a [&'a str],
}

const SMOKE_FIXTURE: Fixture<'static> = Fixture {
    root: "root",
    files: &["smoke-file"],
};

impl Fixture<'_> {
    fn create_in(&self, scratch: &Path) -> anyhow::Result<PathBuf> {
        let root = scratch.join(self.root);
        for file in self.files {
            let path = root.join(file);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).context("failed to create PTY fixture folder")?;
            }
            std::fs::write(&path, b"excise").context("failed to create PTY fixture file")?;
        }
        Ok(root)
    }
}

#[test]
fn launches_renders_accepts_input_and_restores_terminal() -> anyhow::Result<()> {
    let run = run_pty_interaction(None)?;
    if !run.status.success() {
        bail!(
            "Excise exited unexpectedly: {}; expected normal completion; captured {:?}",
            run.status,
            run.output
        );
    }
    if !windows_conpty() {
        // ConPTY repaints the screen instead of relaying Excise's bytes, so only
        // native terminals can be asked for the text Excise drew.
        assert!(
            run.output.contains("SCANNING FOLDER"),
            "the initial TUI frame must present the scan field; captured {:?}",
            run.output
        );
        assert!(
            run.output.contains("MATERIALIZING MAP"),
            "the first measured layout must emerge through the scan field; captured {:?}",
            run.output
        );
    }
    assert_normal_event_stream(&run);
    let metrics = run
        .metrics
        .context("normal run did not record PTY metrics")?;
    if std::env::var_os("EXCISE_PTY_BUDGETS").is_some() {
        assert!(
            metrics.first_frame <= FIRST_FRAME_BUDGET,
            "first frame took {:?}; captured {:?}",
            metrics.first_frame,
            run.output
        );
        assert!(
            metrics.input_to_frame <= INPUT_FRAME_BUDGET,
            "input-to-frame took {:?}",
            metrics.input_to_frame
        );
        assert!(
            metrics.normal_quit <= NORMAL_QUIT_BUDGET,
            "normal quit took {:?}",
            metrics.normal_quit
        );
    }
    Ok(())
}

/// Checks the protocol invariants of a run that scanned the smoke fixture and quit normally.
fn assert_normal_event_stream(run: &PtyRun) {
    let kinds: Vec<&str> = run.events.iter().map(|event| event.kind.as_str()).collect();
    let position = |kind: &str| {
        kinds
            .iter()
            .position(|candidate| *candidate == kind)
            .unwrap_or_else(|| panic!("no {kind} event was emitted; saw {kinds:?}"))
    };

    assert_eq!(kinds.first(), Some(&"hello"), "hello must come first");
    assert_eq!(kinds.last(), Some(&"exit"), "exit must come last");
    for once in ["hello", "scan_complete", "exit"] {
        assert_eq!(
            kinds.iter().filter(|kind| **kind == once).count(),
            1,
            "{once} must be emitted exactly once; saw {kinds:?}"
        );
    }
    assert!(
        position("frame") < position("scan_complete")
            && position("scan_complete") < position("quit_prompt")
            && position("quit_prompt") < position("exit"),
        "events arrived out of order: {kinds:?}"
    );

    let hello = &run.events[0];
    assert_eq!(
        hello.fields.get("version").and_then(Value::as_str),
        Some(env!("CARGO_PKG_VERSION"))
    );
    if let Some(child_id) = run.child_id {
        assert_eq!(hello.number("pid"), Some(u64::from(child_id)));
    }

    let times: Vec<u64> = run
        .events
        .iter()
        .filter_map(|event| event.number("t_us"))
        .collect();
    assert!(
        times.windows(2).all(|pair| pair[0] <= pair[1]),
        "t_us must never go backwards: {times:?}"
    );

    let frames: Vec<&TestEvent> = run
        .events
        .iter()
        .filter(|event| event.kind == "frame")
        .collect();
    for (index, frame) in frames.iter().enumerate() {
        assert_eq!(
            frame.number("seq"),
            Some(u64::try_from(index + 1).expect("frame count should fit")),
            "frame seq must count drawn frames from 1"
        );
    }
    let inputs: Vec<u64> = frames
        .iter()
        .filter_map(|frame| frame.number("inputs"))
        .collect();
    assert!(
        inputs.windows(2).all(|pair| pair[0] <= pair[1]),
        "frame inputs must never decrease: {inputs:?}"
    );
    let quit_frame = run.events[position("quit_prompt")..]
        .iter()
        .find(|event| event.kind == "frame")
        .expect("the quit dialog must be drawn");
    assert!(
        quit_frame.number("inputs") >= Some(1),
        "the frame drawing the quit dialog must count the `q` that opened it"
    );

    // The smoke fixture holds one file.
    assert_eq!(
        run.events[position("scan_complete")].number("entries"),
        Some(1)
    );
    assert_eq!(
        run.events[position("exit")].number("code"),
        Some(u64::from(run.status.exit_code()))
    );
}

#[test]
fn panic_restores_terminal_before_diagnostics() -> anyhow::Result<()> {
    let run = run_pty_interaction(Some("panic"))?;
    let (status, output) = (&run.status, &run.output);
    if status.exit_code() != 101 {
        bail!("injected panic exited with {status}; captured {output:?}");
    }
    let restored = output
        .rfind("\u{1b}[?1049l")
        .context("panic path never left the alternate screen")?;
    let diagnostic = output
        .find("panicked at")
        .context("panic diagnostic was not emitted")?;
    assert!(
        restored < diagnostic,
        "panic diagnostic preceded terminal restoration"
    );
    assert!(
        run.events.iter().all(|event| event.kind != "exit"),
        "a process that panics must not report an exit code"
    );
    Ok(())
}

#[test]
fn typed_runtime_errors_restore_before_diagnostics() -> anyhow::Result<()> {
    for (kind, expected_exit) in [("input", 74), ("render", 70), ("worker", 70)] {
        let run = run_pty_interaction(Some(kind))?;
        let (status, output) = (&run.status, &run.output);
        if status.exit_code() != expected_exit {
            bail!("injected {kind} failure exited with {status}; captured {output:?}");
        }
        let restored = output
            .rfind("\u{1b}[?1049l")
            .context("typed failure never left the alternate screen")?;
        let diagnostic = output
            .find("Error:")
            .context("typed failure diagnostic was not emitted")?;
        assert!(
            restored < diagnostic,
            "{kind} diagnostic preceded restoration"
        );
        let exit = run
            .events
            .last()
            .filter(|event| event.kind == "exit")
            .with_context(|| format!("{kind} failure did not end with an exit event"))?;
        assert_eq!(
            exit.number("code"),
            Some(u64::from(expected_exit)),
            "the exit event must report the class the process returned"
        );
    }
    Ok(())
}

fn windows_conpty() -> bool {
    std::env::consts::OS == "windows"
}

fn run_pty_interaction(injected_failure: Option<&str>) -> anyhow::Result<PtyRun> {
    run_pty_session(&SMOKE_FIXTURE, injected_failure)
}

/// The timings of the first-frame, input, and quit measurements.
struct QuitFlow {
    first_frame: Duration,
    input_to_frame: Duration,
    quit_started: Instant,
}

/// Quits a normal session: first frame, finished scan, `q`, its dialog, then `y`.
fn drive_quit_flow(
    events: &mut EventTail,
    writer: &SharedWriter,
    spawn_started: Instant,
) -> anyhow::Result<QuitFlow> {
    let first = events.wait_for("the first frame", STARTUP_TIMEOUT, |event| {
        event.kind == "frame"
    })?;
    let first_frame = first.observed.duration_since(spawn_started);
    events.wait_for("the scan to complete", STARTUP_TIMEOUT, |event| {
        event.kind == "scan_complete"
    })?;

    // Only a frame that counts this key can answer it, whatever else was consumed before.
    events.poll()?;
    let keys_sent = 1;
    let inputs_before = events.inputs_consumed();
    let input_started = Instant::now();
    write_input(writer, b"q")?;
    events.wait_for("the quit prompt", STARTUP_TIMEOUT, |event| {
        event.kind == "quit_prompt"
    })?;
    let frame = events.wait_for(
        "the frame drawing the quit prompt",
        STARTUP_TIMEOUT,
        |event| {
            event.kind == "frame"
                && event
                    .number("inputs")
                    .is_some_and(|inputs| inputs >= inputs_before + keys_sent)
        },
    )?;
    let input_to_frame = frame.observed.duration_since(input_started);

    let quit_started = Instant::now();
    write_input(writer, b"y")?;
    events.wait_for("the exit event", EXIT_TIMEOUT, |event| event.kind == "exit")?;
    Ok(QuitFlow {
        first_frame,
        input_to_frame,
        quit_started,
    })
}

fn captured(output: &SharedOutput) -> String {
    String::from_utf8_lossy(&output.lock().expect("failed to lock PTY output")).into_owned()
}

fn run_pty_session(
    fixture: &Fixture<'_>,
    injected_failure: Option<&str>,
) -> anyhow::Result<PtyRun> {
    let _session = pty_session_guard();
    let scratch = tempfile::tempdir().context("failed to create PTY scratch directory")?;
    let root = fixture.create_in(scratch.path())?;
    // The event file lives beside the scanned root, never inside it.
    let events_path = scratch.path().join("events.jsonl");

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("failed to open pseudo-terminal")?;

    let binary = excise_binary(injected_failure);
    warm_up(&binary);
    let command = pty_command(binary, &root, &events_path, injected_failure);
    let spawn_started = Instant::now();
    let child = pair
        .slave
        .spawn_command(command)
        .context("failed to launch Excise in pseudo-terminal")?;
    let child_id = child.process_id();
    let mut child_guard = ChildGuard::new(child.clone_killer());
    drop(pair.slave);

    let reader = pair
        .master
        .try_clone_reader()
        .context("failed to clone pseudo-terminal reader")?;
    let writer = Arc::new(Mutex::new(
        pair.master
            .take_writer()
            .context("failed to take pseudo-terminal writer")?,
    ));

    let output = SharedOutput::default();
    let reader_thread = spawn_terminal_reader(reader, output.clone(), writer.clone());

    let mut events = EventTail::new(events_path);
    let mut flow = None;
    if injected_failure.is_none() {
        flow = Some(
            drive_quit_flow(&mut events, &writer, spawn_started)
                .with_context(|| format!("captured {:?}", captured(&output)))?,
        );
    }
    drop(writer);

    let status = wait_for_child(child, EXIT_TIMEOUT)?;
    let normal_quit = flow.as_ref().map(|flow| flow.quit_started.elapsed());
    child_guard.disarm();
    drop(pair.master);
    reader_thread
        .join()
        .map_err(|_| anyhow::anyhow!("PTY reader thread panicked"))?
        .context("failed to read pseudo-terminal output")?;
    events.poll()?;

    let rendered = captured(&output);
    #[cfg(not(windows))]
    {
        if injected_failure.is_none() {
            let hidden = rendered
                .rfind("\u{1b}[?25l")
                .context("terminal output never hid the cursor")?;
            let shown = rendered
                .rfind("\u{1b}[?25h")
                .context("terminal output never restored the cursor")?;
            assert!(
                shown > hidden,
                "cursor was not visible after terminal cleanup"
            );
        }
        assert!(
            rendered.contains("\u{1b}[?1049h"),
            "alternate screen was never entered"
        );
        assert!(
            rendered.contains("\u{1b}[?1049l"),
            "alternate screen was never left"
        );
    }

    let metrics = flow.zip(normal_quit).map(|(flow, normal_quit)| PtyMetrics {
        first_frame: flow.first_frame,
        input_to_frame: flow.input_to_frame,
        normal_quit,
    });
    Ok(PtyRun {
        status,
        output: rendered,
        metrics,
        events: events.events,
        events_raw: events.raw,
        child_id,
    })
}

fn excise_binary(injected_failure: Option<&str>) -> std::ffi::OsString {
    // Failure injection exists only in debug builds.
    let variable = if injected_failure.is_some() {
        "EXCISE_PTY_DEBUG_BINARY"
    } else {
        "EXCISE_PTY_BINARY"
    };
    std::env::var_os(variable)
        .filter(|path| Path::new(path).is_file())
        .unwrap_or_else(|| std::ffi::OsString::from(env!("CARGO_BIN_EXE_excise")))
}

/// Launches the binary once before a timed session.
///
/// The first launch of a freshly built executable can spend hundreds of
/// milliseconds in operating-system verification that has nothing to do with
/// Excise's own startup, and the first-frame budget starts at spawn.
fn warm_up(binary: &std::ffi::OsStr) {
    // A failure surfaces when the session itself launches the binary.
    let _ = std::process::Command::new(binary).arg("--version").output();
}

fn pty_command(
    binary: std::ffi::OsString,
    root: &Path,
    events: &Path,
    injected_failure: Option<&str>,
) -> CommandBuilder {
    let mut command = CommandBuilder::new(binary);
    command.arg(root);
    command.cwd(root);
    command.env("TERM", "xterm-256color");
    command.env_remove("NO_COLOR");
    command.env("EXCISE_TEST_EVENTS", events);
    match injected_failure {
        Some("panic") => command.env("EXCISE_TEST_PANIC_AFTER_TERMINAL_ENTRY", "1"),
        Some(kind) => command.env("EXCISE_TEST_ERROR_AFTER_TERMINAL_ENTRY", kind),
        None => {}
    }
    command
}

fn spawn_terminal_reader(
    mut reader: Box<dyn Read + Send>,
    output: SharedOutput,
    writer: SharedWriter,
) -> thread::JoinHandle<std::io::Result<()>> {
    thread::spawn(move || {
        let mut chunk = [0_u8; 4096];
        let mut answered_queries = 0;
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => return Ok(()),
                Ok(bytes_read) => {
                    let query_count = {
                        let mut bytes = output.lock().expect("failed to lock PTY output");
                        bytes.extend_from_slice(&chunk[..bytes_read]);
                        bytes
                            .windows(b"\x1b[6n".len())
                            .filter(|window| *window == b"\x1b[6n")
                            .count()
                    };
                    if query_count > answered_queries {
                        let mut writer = writer.lock().expect("failed to lock PTY writer");
                        for _ in answered_queries..query_count {
                            writer.write_all(b"\x1b[1;1R")?;
                        }
                        writer.flush()?;
                        answered_queries = query_count;
                    }
                }
                Err(error) if error.raw_os_error() == Some(5) => return Ok(()),
                Err(error) => return Err(error),
            }
        }
    })
}

#[test]
fn invalid_config_fails_before_terminal_entry() -> anyhow::Result<()> {
    let fixture = tempfile::tempdir().context("failed to create config fixture")?;
    let config = fixture.path().join("invalid.toml");
    std::fs::write(&config, "version = 99\n").context("failed to write invalid config")?;
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_excise"))
        .arg("--config")
        .arg(config)
        .output()
        .context("failed to execute invalid-config check")?;
    assert_eq!(output.status.code(), Some(78));
    assert_no_terminal_controls(&output);
    Ok(())
}

#[test]
fn invalid_path_fails_before_terminal_entry() -> anyhow::Result<()> {
    let fixture = tempfile::tempdir().context("failed to create path fixture")?;
    let missing = fixture.path().join("missing");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_excise"))
        .arg(missing)
        .output()
        .context("failed to execute invalid-path check")?;
    assert_eq!(output.status.code(), Some(74));
    assert_no_terminal_controls(&output);
    Ok(())
}

#[test]
fn non_tty_fails_without_emitting_terminal_controls() -> anyhow::Result<()> {
    let fixture = tempfile::tempdir().context("failed to create non-TTY fixture")?;
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_excise"))
        .arg(fixture.path())
        .output()
        .context("failed to execute non-TTY check")?;
    assert_eq!(output.status.code(), Some(70));
    assert_no_terminal_controls(&output);
    Ok(())
}

/// Runs Excise without a terminal. It must fail on the event file before it reaches the terminal check.
fn run_with_events_value(
    root: &Path,
    value: &std::ffi::OsStr,
) -> anyhow::Result<std::process::Output> {
    std::process::Command::new(env!("CARGO_BIN_EXE_excise"))
        .arg(root)
        .env("EXCISE_TEST_EVENTS", value)
        .output()
        .context("failed to execute event-channel check")
}

#[test]
fn existing_events_file_fails_before_terminal_entry() -> anyhow::Result<()> {
    let fixture = tempfile::tempdir().context("failed to create events fixture")?;
    let events = fixture.path().join("events.jsonl");
    std::fs::write(&events, b"earlier run\n").context("failed to write earlier events")?;

    let output = run_with_events_value(fixture.path(), events.as_os_str())?;

    assert_eq!(
        output.status.code(),
        Some(78),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_no_terminal_controls(&output);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("EXCISE_TEST_EVENTS"),
        "the diagnostic must name the variable"
    );
    assert_eq!(
        std::fs::read(&events).context("failed to reread earlier events")?,
        b"earlier run\n",
        "an existing file must never be modified"
    );
    Ok(())
}

#[test]
fn unusable_events_values_fail_before_terminal_entry() -> anyhow::Result<()> {
    let fixture = tempfile::tempdir().context("failed to create events fixture")?;
    let directory = fixture.path().join("directory");
    std::fs::create_dir(&directory).context("failed to create events directory")?;
    let missing_parent = fixture.path().join("missing").join("events.jsonl");

    for (label, value) in [
        ("an empty value", std::ffi::OsString::new()),
        ("a directory", directory.into_os_string()),
        ("a missing parent", missing_parent.into_os_string()),
    ] {
        let output = run_with_events_value(fixture.path(), &value)?;
        assert_eq!(
            output.status.code(),
            Some(78),
            "{label}; stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_no_terminal_controls(&output);
    }
    assert!(
        !fixture.path().join("missing").exists(),
        "a missing parent must not be created"
    );
    Ok(())
}

#[test]
fn event_channel_never_records_names_or_paths() -> anyhow::Result<()> {
    const TOKEN: &str = "privacy-token-7f3a9c1e";
    let root = format!("root-{TOKEN}");
    let top_level = format!("file-{TOKEN}.dat");
    let nested = format!("dir-{TOKEN}/nested-{TOKEN}.dat");
    let run = run_pty_session(
        &Fixture {
            root: &root,
            files: &[&top_level, &nested],
        },
        None,
    )?;
    if !run.status.success() {
        bail!(
            "Excise exited unexpectedly: {}; captured {:?}",
            run.status,
            run.output
        );
    }

    let text = String::from_utf8(run.events_raw.clone()).context("events must be UTF-8")?;
    assert!(
        !text.contains(TOKEN),
        "the event file leaked a fixture name: {text}"
    );

    // An empty stream would prove nothing.
    for kind in ["hello", "frame", "scan_complete", "quit_prompt", "exit"] {
        assert!(
            run.events.iter().any(|event| event.kind == kind),
            "the run never emitted {kind}: {text}"
        );
    }
    // Beyond the package version, protocol v1 carries numbers only.
    for event in &run.events {
        for (field, value) in &event.fields {
            match field.as_str() {
                "kind" | "version" => assert!(value.is_string(), "{field}: {value}"),
                "v" | "t_us" | "pid" | "seq" | "inputs" | "entries" | "removed" | "failed"
                | "code" => assert!(value.is_number(), "{field}: {value}"),
                other => panic!("field {other:?} is not part of protocol v1: {text}"),
            }
        }
    }
    Ok(())
}

fn assert_no_terminal_controls(output: &std::process::Output) {
    assert!(!output.stdout.contains(&0x1b));
    assert!(!output.stderr.contains(&0x1b));
}

fn write_input(writer: &SharedWriter, input: &[u8]) -> anyhow::Result<()> {
    let mut writer = writer.lock().expect("failed to lock PTY writer");
    writer
        .write_all(input)
        .context("failed to send pseudo-terminal input")?;
    writer.flush().context("failed to flush PTY input")
}

fn wait_for_child(
    mut child: Box<dyn Child + Send + Sync>,
    timeout: Duration,
) -> anyhow::Result<ExitStatus> {
    let mut killer = child.clone_killer();
    let (finished, completion) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let _ = finished.send(child.wait());
    });

    match completion.recv_timeout(timeout) {
        Ok(status) => status.context("failed to wait for Excise"),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            killer.kill().context("failed to terminate hung Excise")?;
            bail!("timed out waiting for Excise to exit")
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            bail!("Excise wait thread disconnected")
        }
    }
}
