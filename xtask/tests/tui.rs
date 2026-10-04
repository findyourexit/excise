//! `cargo xtask tui` as a person or an agent runs it: the real command line, a real supervisor
//! process, and a real `excise` on a fixture, in a pseudo-terminal.
//!
//! Every test makes its own state directory, work directory, and fixture copy, so the tests are
//! safe under parallel `cargo test`, and every test that opens a session checks that nothing of
//! it is left behind. Every document a command prints is checked against the `harness-tui` schema
//! and read back through the harness's strict reader, and every command's stdout is exactly that
//! one document, with the exit status the document calls for.
//!
//! The `excise` binary is the one `EXCISE_E2E_BINARY` names, or the one `cargo test` built next to
//! the `xtask` binary (`cargo test --workspace` builds it for the integration tests of the
//! `excise` package).

#![cfg(unix)]

use std::{
    env, fs,
    io::Read as _,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::LazyLock,
    thread,
    time::{Duration, Instant},
};

use excise_harness::report::{Document as _, HarnessTui, tui::TuiErrorKind};
use jsonschema::Validator;
use serde_json::{Value, json};
use tempfile::TempDir;

/// The longest any one command may take before the test gives up on it. The commands bound their
/// own waits; this only keeps a hang from hanging the test run.
const COMMAND_LIMIT: Duration = Duration::from_secs(180);
/// The longest the tests wait for something that happens by itself.
const PATIENCE: Duration = Duration::from_secs(60);

/// The terminal modes of a terminal as a shell expects it.
fn restored_modes() -> Value {
    json!({"alternate_screen": false, "cursor_visible": true, "echo": true, "icanon": true})
}

static NULL: Value = Value::Null;

fn excise_binary() -> PathBuf {
    if let Some(path) = env::var_os("EXCISE_E2E_BINARY").filter(|path| !path.is_empty()) {
        return PathBuf::from(path);
    }
    let binary = Path::new(env!("CARGO_BIN_EXE_xtask")).with_file_name("excise");
    assert!(
        binary.is_file(),
        "there is no `excise` binary at {}: `cargo test --workspace` builds it, `cargo build -p \
         excise` does, and EXCISE_E2E_BINARY names another",
        binary.display()
    );
    binary
}

static VALIDATOR: LazyLock<Validator> = LazyLock::new(|| {
    let schema: Value = serde_json::from_str(HarnessTui::SCHEMA_JSON).expect("the schema is JSON");
    jsonschema::draft202012::options()
        .should_validate_formats(true)
        .build(&schema)
        .expect("the schema compiles")
});

/// Runs `command` to its end, or kills it after `limit`. Returns its status code and what it
/// wrote to stdout and stderr.
fn run_bounded(mut command: Command, limit: Duration) -> (i32, String, String) {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("the command starts");
    let drain = |mut stream: Box<dyn std::io::Read + Send>| {
        thread::spawn(move || {
            let mut text = String::new();
            let _ = stream.read_to_string(&mut text);
            text
        })
    };
    let stdout = drain(Box::new(child.stdout.take().expect("a piped stdout")));
    let stderr = drain(Box::new(child.stderr.take().expect("a piped stderr")));
    let deadline = Instant::now() + limit;
    let status = loop {
        if let Some(status) = child.try_wait().expect("the command can be waited for") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("a command did not end within {limit:?}: {command:?}");
        }
        thread::sleep(Duration::from_millis(5));
    };
    (
        status.code().expect("the command exited by itself"),
        stdout.join().expect("stdout is read"),
        stderr.join().expect("stderr is read"),
    )
}

/// Whether the process is there and is not a zombie.
fn alive(pid: u64) -> bool {
    let output = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .stderr(Stdio::null())
        .output()
        .expect("ps runs");
    let state = String::from_utf8_lossy(&output.stdout);
    output.status.success() && !state.trim().is_empty() && !state.trim().starts_with('Z')
}

fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + PATIENCE;
    while !condition() {
        assert!(Instant::now() < deadline, "gave up waiting for {what}");
        thread::sleep(Duration::from_millis(25));
    }
}

/// The names in `dir`, sorted; none when it is not there.
fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// What a command printed, once it has been checked.
#[derive(Clone)]
struct Reply {
    code: i32,
    value: Value,
}

impl Reply {
    fn ok(&self) -> bool {
        self.value["ok"] == true
    }

    /// The value at `pointer` in the document, or `null`.
    fn at(&self, pointer: &str) -> &Value {
        self.value.pointer(pointer).unwrap_or(&NULL)
    }

    fn text(&self, pointer: &str) -> &str {
        self.at(pointer)
            .as_str()
            .unwrap_or_else(|| panic!("{pointer} is not text in {}", self.value))
    }

    fn number(&self, pointer: &str) -> u64 {
        self.at(pointer)
            .as_u64()
            .unwrap_or_else(|| panic!("{pointer} is not a number in {}", self.value))
    }

    fn expect_ok(self, what: &str) -> Self {
        assert!(
            self.ok(),
            "{what} failed (exit {}): {}",
            self.code,
            self.value["error"]
        );
        self
    }

    /// The kinds of the records the events field lists.
    fn event_kinds(&self, pointer: &str) -> Vec<String> {
        self.at(pointer)
            .as_array()
            .unwrap_or_else(|| panic!("{pointer} is not a list in {}", self.value))
            .iter()
            .map(|record| record["kind"].as_str().expect("a kind").to_owned())
            .collect()
    }
}

/// An open session, as `open` reported it.
struct Session {
    id: String,
    root: PathBuf,
    program: u64,
    supervisor: u64,
    opened: Reply,
}

/// One test's world: its own target directory (the state directory and the fixture cache live in
/// it) and its own work directory (the run copies live in it).
struct Driver {
    target: TempDir,
    /// A short path, because a deletion dialog cuts a path that does not fit its 78 columns.
    work: TempDir,
    excise: PathBuf,
    /// How fast the supervisor reads the program's output, in bytes per second, when the test
    /// puts the screen behind the program: see [`Driver::slow`].
    drain_cap: Option<u64>,
}

impl Driver {
    fn new() -> Self {
        Self {
            target: TempDir::new().expect("a target directory"),
            work: tempfile::Builder::new()
                .prefix("xt-")
                .rand_bytes(4)
                .tempdir_in("/tmp")
                .expect("a work directory"),
            excise: excise_binary(),
            drain_cap: None,
        }
    }

    /// A driver whose sessions read the program's output at `bytes_per_second` at most, as a slow
    /// terminal does: the screen trails the program by as long as its frames take to arrive.
    fn slow(bytes_per_second: u64) -> Self {
        let mut driver = Self::new();
        driver.drain_cap = Some(bytes_per_second);
        driver
    }

    fn state_dir(&self) -> PathBuf {
        self.target.path().join("excise-tui")
    }

    fn work_dir(&self) -> &Path {
        self.work.path()
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_xtask"));
        command
            .arg("tui")
            .args(args)
            .env("CARGO_TARGET_DIR", self.target.path())
            .env("EXCISE_E2E_TMPDIR", self.work.path())
            .env("EXCISE_E2E_BINARY", &self.excise);
        match self.drain_cap {
            Some(rate) => command.env(DRAIN_CAP_VARIABLE, rate.to_string()),
            // A cap in the environment of whoever runs the tests is not this test's.
            None => command.env_remove(DRAIN_CAP_VARIABLE),
        };
        command
    }

    /// Runs `cargo xtask tui <args>` and checks what every command promises: stdout is one JSON
    /// document that the schema and the harness's reader accept, and the exit status is the one
    /// the document calls for.
    fn run(&self, args: &[&str]) -> Reply {
        Self::run_command(self.command(args), &format!("`tui {}`", args.join(" ")))
    }

    /// Runs `command`, which a test made itself, with the same checks as [`Driver::run`].
    fn run_command(command: Command, described: &str) -> Reply {
        let (code, stdout, stderr) = run_bounded(command, COMMAND_LIMIT);
        let value: Value = serde_json::from_str(&stdout).unwrap_or_else(|error| {
            panic!(
                "{described} did not print exactly one JSON document ({error}):\nstdout: \
                 {stdout}\nstderr: {stderr}"
            )
        });
        let violations: Vec<String> = VALIDATOR
            .iter_errors(&value)
            .map(|error| error.to_string())
            .collect();
        assert!(
            violations.is_empty(),
            "{described} printed a document the schema rejects: {violations:#?}\n{value}"
        );
        let document = HarnessTui::from_json_str(&stdout).unwrap_or_else(|error| {
            panic!("{described}: the harness cannot read its own document ({error})")
        });
        assert!(
            stdout.ends_with("}\n"),
            "{described}: the document ends with one newline"
        );
        let expected = match (&document.error, document.ok) {
            (None, true) => 0,
            (Some(error), false) if error.kind == TuiErrorKind::Usage => 2,
            _ => 1,
        };
        assert_eq!(code, expected, "{described}: the exit status\n{value}");
        Reply { code, value }
    }

    fn open(&self, fixture: &str, options: &[&str]) -> Session {
        let mut args = vec!["open", "--fixture", fixture];
        args.extend_from_slice(options);
        let opened = self.run(&args).expect_ok("open");
        Session {
            id: opened.text("/session").to_owned(),
            root: PathBuf::from(opened.text("/result/root")),
            program: opened.number("/result/pid"),
            supervisor: opened.number("/result/supervisor_pid"),
            opened: opened.clone(),
        }
    }

    fn keys(&self, session: &Session, keys: &[&str]) -> Reply {
        let mut args = vec!["keys", &session.id, "--timeout", "20s"];
        args.extend_from_slice(keys);
        self.run(&args)
    }

    fn delete(&self, session: &Session, name: &str, kind: &str) -> Reply {
        self.run(&["delete", &session.id, "--name", name, "--kind", kind])
    }

    fn screen(&self, session: &Session) -> Reply {
        self.run(&["screen", &session.id])
    }

    fn events(&self, session: &Session, since: u64) -> Reply {
        self.run(&["events", &session.id, "--since", &since.to_string()])
    }

    fn close(&self, session: &Session) -> Reply {
        self.run(&["close", &session.id])
    }

    fn list(&self) -> Reply {
        self.run(&["list"])
    }

    /// Waits until the scan is done: the header badge reads `COMPLETE`.
    fn wait_for_the_scan(&self, session: &Session) {
        wait_until("the scan to complete", || {
            self.screen(session).at("/result/screen/header_state") == "COMPLETE"
        });
    }

    /// Reads the screen until `done` holds for it. A frame that counts a key is not always the
    /// frame that shows what the key did: a prompt or a dialog can take a frame to draw.
    fn wait_for_screen(
        &self,
        session: &Session,
        what: &str,
        done: impl Fn(&Value) -> bool,
    ) -> Reply {
        let mut found = None;
        wait_until(what, || {
            let reply = self.screen(session);
            let reached = done(reply.at("/result/screen"));
            if reached {
                found = Some(reply);
            }
            reached
        });
        found.expect("the screen was reached")
    }

    /// Waits until the header names the folder that is open: `needle` is in it, or is not.
    fn wait_for_header(&self, session: &Session, needle: &str, present: bool) {
        self.wait_for_screen(session, "the header to name the open folder", |screen| {
            screen["rows"][0]
                .as_str()
                .is_some_and(|row| row.contains(needle) == present)
        });
    }

    /// Opens the filter, erases the text it opens with, types `name`, and applies it, in the
    /// commands a person would use: the keys that edit the prompt and the key that applies it
    /// are not in one command, because a confirmation key is never sent in a command that sent
    /// Backspace.
    fn apply_filter(&self, session: &Session, name: &str) {
        self.keys(session, &["/"]).expect_ok("the filter prompt");
        let prompt = self.wait_for_screen(session, "the filter prompt", |screen| {
            screen["filter"].is_object()
        });
        let erase = prompt.text("/result/screen/filter/input").chars().count();
        if erase > 0 {
            self.keys(session, &vec!["backspace"; erase])
                .expect_ok("erase the filter");
            self.wait_for_screen(session, "the filter to be empty", |screen| {
                screen["filter"]["input"] == ""
            });
        }
        let typed = format!("type:{name}");
        self.keys(session, &[typed.as_str(), "enter"])
            .expect_ok("apply the filter");
        self.wait_for_screen(session, "the filter prompt to close", |screen| {
            screen["filter"].is_null()
        });
    }

    /// Selects the entry the filter finds.
    fn select(&self, session: &Session, name: &str) -> Reply {
        self.apply_filter(session, name);
        self.wait_for_screen(session, "the entry to be selected", |screen| {
            screen["selected"]["name"] == name
        })
    }

    /// Nothing of the session is left: no process, no session directory, no run copy.
    fn assert_gone(&self, session: &Session) {
        wait_until("the supervisor and the program to be gone", || {
            !alive(session.supervisor) && !alive(session.program)
        });
        assert!(
            !self.state_dir().join(&session.id).exists(),
            "the session directory is left"
        );
        assert!(!session.root.exists(), "the run copy is left");
        assert_eq!(
            names(self.work_dir()),
            Vec::<String>::new(),
            "the work directory is not empty"
        );
    }
}

/// How long the cleanup of a test waits for one command.
const CLEANUP_LIMIT: Duration = Duration::from_secs(90);

/// Runs `command` to its end, or kills it after `limit`, and returns what it printed. It is for
/// the cleanup of a test, which has to end the same way whether or not the test failed, so it
/// never panics.
fn run_quietly(mut command: Command, limit: Duration) -> Option<String> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command.spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        text
    });
    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
        }
    }
    reader.join().ok()
}

/// The command line of process `pid`, when the system shows one.
fn command_line(pid: u64) -> Option<String> {
    let output = Command::new("ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let line = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (output.status.success() && !line.is_empty()).then_some(line)
}

impl Drop for Driver {
    /// A session the test did not end is ended through the driver, so that a failing test leaves
    /// no process behind, and no process is signaled because a file says its id: `close` ends a
    /// live session and cleans a stale one, whose program is found by the mark that only the
    /// session's processes carry, and `list` sweeps what is left. A supervisor that lives on
    /// after that (one that does not answer) is killed only when its command line names its
    /// session directory, a path of this test's own that no other process has. A process id that
    /// a state file holds is not that.
    ///
    /// The sessions are the ones in the test's own state directory, not the ones whose `open` the
    /// test saw succeed: an `open` that printed a document the test rejects has started a
    /// supervisor all the same.
    fn drop(&mut self) {
        for session in names(&self.state_dir()) {
            if session.len() == 8 && session.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                let _ = run_quietly(self.command(&["close", &session]), CLEANUP_LIMIT);
            }
        }
        let Some(listed) = run_quietly(self.command(&["list"]), CLEANUP_LIMIT)
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        else {
            return;
        };
        for session in listed["result"]["sessions"]
            .as_array()
            .into_iter()
            .flatten()
        {
            let directory = self
                .state_dir()
                .join(session["session"].as_str().unwrap_or_default());
            let Some(pid) = session["supervisor_pid"].as_u64() else {
                continue;
            };
            if command_line(pid)
                .is_some_and(|line| line.contains(directory.to_string_lossy().as_ref()))
            {
                let _ = Command::new("kill")
                    .args(["-KILL", &pid.to_string()])
                    .stderr(Stdio::null())
                    .status();
            }
        }
    }
}

#[test]
fn a_session_runs_from_open_to_close_and_leaves_nothing() {
    let driver = Driver::new();
    let session = driver.open("navigate-folders", &["--record"]);
    let opened = &session.opened;
    assert_eq!(opened.at("/result/fixture"), "navigate-folders");
    assert_eq!(opened.at("/result/profile"), "default");
    assert_eq!(*opened.at("/result/size"), json!({"cols": 120, "rows": 40}));
    assert_eq!(opened.at("/result/screen/modes/alternate_screen"), true);
    assert_eq!(opened.event_kinds("/result/events/records"), ["hello"]);
    for entry in ["photos", "music", "readme.txt"] {
        assert!(
            session.root.join(entry).exists(),
            "{entry} is in the fixture"
        );
    }
    assert!(Path::new(opened.text("/result/session_dir")).is_dir());
    driver.wait_for_the_scan(&session);

    // Keys that open a folder, and a screen that shows it.
    let selected = driver.select(&session, "photos");
    assert_eq!(selected.at("/result/screen/selected/name"), "photos");
    assert_eq!(selected.at("/result/screen/selected/kind"), "folder");
    let entered = driver.keys(&session, &["enter"]).expect_ok("enter");
    assert_eq!(entered.at("/result/settled"), true);
    assert_eq!(
        *entered.at("/result/inputs"),
        json!({"sent": 9, "consumed": 9})
    );
    driver.wait_for_header(&session, "/photos", true);

    // The events include the keys the program consumed.
    let events = driver.events(&session, 0).expect_ok("events");
    let counted: Vec<u64> = events
        .at("/result/events/records")
        .as_array()
        .expect("records")
        .iter()
        .filter(|record| record["kind"] == "frame")
        .map(|record| record["inputs"].as_u64().expect("inputs"))
        .collect();
    assert_eq!(counted.last(), Some(&9), "the last frame counts every key");
    assert!(counted.windows(2).all(|pair| pair[0] <= pair[1]));

    // Esc goes up a folder.
    driver.keys(&session, &["esc"]).expect_ok("esc");
    driver.wait_for_header(&session, "/photos", false);
    let listed = driver.list().expect_ok("list");
    assert_eq!(listed.at("/result/sessions/0/session"), session.id.as_str());
    assert_eq!(listed.at("/result/sessions/0/state"), "running");
    assert_eq!(
        listed.number("/result/sessions/0/supervisor_pid"),
        session.supervisor
    );
    assert_eq!(*listed.at("/result/stale"), json!([]));

    let closed = driver.close(&session).expect_ok("close");
    assert_eq!(closed.at("/result/exit/code"), 0);
    assert_eq!(closed.at("/result/exit/via"), "quit");
    assert_eq!(closed.at("/result/exit/terminal_restored"), true);
    assert_eq!(*closed.at("/result/exit/modes"), restored_modes());
    assert_eq!(*closed.at("/result/residue"), json!([]));
    assert_eq!(
        *closed.at("/result/cleanup"),
        json!({"removed": true, "problems": []})
    );
    assert_eq!(
        *closed.at("/result/fixture"),
        json!({"removed": 0, "unexpected": []})
    );

    // The recording is kept, and it is an asciicast of this terminal.
    let cast = fs::read_to_string(closed.text("/result/recording")).expect("the recording is kept");
    let header: Value = serde_json::from_str(cast.lines().next().expect("a header")).expect("JSON");
    assert_eq!(
        (header["version"].as_u64(), header["width"].as_u64()),
        (Some(2), Some(120))
    );
    assert_eq!(names(&driver.state_dir()), [format!("{}.cast", session.id)]);
    driver.assert_gone(&session);
    assert_eq!(
        *driver.list().at("/result"),
        json!({"sessions": [], "stale": []})
    );
}

#[test]
fn delete_removes_the_entry_and_every_sentinel_survives() {
    let driver = Driver::new();
    let session = driver.open("delete-file", &[]);

    let deleted = driver
        .delete(&session, "victim.bin", "file")
        .expect_ok("delete");
    assert_eq!(deleted.at("/result/removed"), 1);
    assert_eq!(deleted.at("/result/failed"), 0);
    assert!(deleted.number("/result/sentinels_checked") >= 3);
    assert_eq!(deleted.at("/result/dialog/delete/kind"), "file");
    assert_eq!(
        deleted.at("/result/dialog/delete/confirmation"),
        "single_key"
    );
    assert_eq!(
        Path::new(deleted.text("/result/dialog/delete/path")),
        session.root.join("victim.bin"),
        "the dialog named exactly the entry"
    );
    assert_eq!(
        *deleted.at("/result/fixture"),
        json!({"removed": 1, "unexpected": []})
    );
    assert_eq!(*deleted.at("/result/screen/dialog"), Value::Null);
    assert!(
        deleted
            .event_kinds("/result/events/records")
            .contains(&"deletion_finished".to_owned())
    );

    assert!(
        !session.root.join("victim.bin").exists(),
        "the victim is gone"
    );
    for sentinel in ["keep-a.bin", "docs/keep-0.txt", "docs/keep-1.txt"] {
        assert!(session.root.join(sentinel).exists(), "{sentinel} survived");
    }

    let closed = driver.close(&session).expect_ok("close");
    assert_eq!(closed.at("/result/exit/code"), 0, "{}", closed.value);
    assert_eq!(
        *closed.at("/result/fixture"),
        json!({"removed": 1, "unexpected": []})
    );
    driver.assert_gone(&session);
}

#[test]
fn delete_acts_on_the_open_folder_and_says_so_at_once() {
    let driver = Driver::new();
    let session = driver.open("delete-file", &[]);
    driver.wait_for_the_scan(&session);

    // At the root, an entry of a folder is not in view.
    let started = Instant::now();
    let refused = driver.delete(&session, "docs/keep-0.txt", "file");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "it fails at once"
    );
    assert_eq!(refused.code, 1);
    assert_eq!(refused.at("/error/kind"), "not_in_view");
    assert_eq!(refused.at("/error/confirmed"), false);
    let message = refused.text("/error/message");
    for needle in [
        "docs/keep-0.txt",
        "the fixture's root",
        "`docs`",
        "navigate",
    ] {
        assert!(message.contains(needle), "{needle:?} is in: {message}");
    }
    assert!(refused.at("/error/screen/rows").is_array());
    assert!(session.root.join("docs/keep-0.txt").exists());

    // Inside `docs`, the entries of the root are not in view.
    driver.select(&session, "docs");
    driver.keys(&session, &["enter"]).expect_ok("enter");
    driver.wait_for_header(&session, "/docs", true);
    let refused = driver.delete(&session, "victim.bin", "file");
    assert_eq!(refused.at("/error/kind"), "not_in_view");
    let message = refused.text("/error/message");
    assert!(
        message.contains("(`docs`)") && message.contains("the fixture's root"),
        "{message}"
    );
    assert!(session.root.join("victim.bin").exists());

    // The entry of the open folder is deleted, by its path from the fixture's root.
    let deleted = driver
        .delete(&session, "docs/keep-0.txt", "file")
        .expect_ok("delete");
    assert_eq!(deleted.at("/result/removed"), 1);
    assert_eq!(
        *deleted.at("/result/fixture"),
        json!({"removed": 1, "unexpected": []})
    );
    assert!(!session.root.join("docs/keep-0.txt").exists());
    for sentinel in ["victim.bin", "keep-a.bin", "docs/keep-1.txt"] {
        assert!(session.root.join(sentinel).exists(), "{sentinel} survived");
    }

    driver.close(&session).expect_ok("close");
    driver.assert_gone(&session);
}

#[test]
fn keys_never_sends_a_key_that_confirms_a_deletion_dialog() {
    let driver = Driver::new();
    let session = driver.open("delete-file", &[]);
    driver.wait_for_the_scan(&session);
    let selected = driver.select(&session, "victim.bin");
    assert_eq!(selected.at("/result/screen/selected/name"), "victim.bin");

    // Backspace opens the deletion dialog.
    driver.keys(&session, &["backspace"]).expect_ok("backspace");
    let dialog = driver.wait_for_screen(&session, "the deletion dialog", |screen| {
        screen["dialog"]["delete"]["kind"] == "file"
    });
    assert!(
        dialog
            .text("/result/screen/dialog/delete/path")
            .ends_with("/victim.bin")
    );

    // Every way to confirm it is refused, and nothing of it is sent.
    for keys in [&["y"][..], &["enter"], &["type:y"], &["ctrl+m"]] {
        let refused = driver.keys(&session, keys);
        assert_eq!(refused.code, 1, "{keys:?}");
        assert_eq!(refused.at("/error/kind"), "refused", "{keys:?}");
        assert_eq!(*refused.at("/error/sent"), json!([]), "{keys:?}");
        assert!(
            refused
                .text("/error/message")
                .contains("deletion dialog is open"),
            "{keys:?}: {}",
            refused.text("/error/message")
        );
        assert_eq!(
            refused.at("/error/screen/dialog/delete/kind"),
            "file",
            "the dialog is still open"
        );
    }
    // Keys before the one that is refused are sent, and reported.
    let partial = driver.keys(&session, &["down", "y"]);
    assert_eq!(partial.at("/error/kind"), "refused");
    assert_eq!(partial.at("/error/sent/0/key"), "down");

    assert!(
        session.root.join("victim.bin").exists(),
        "nothing was deleted"
    );
    let screen = driver.screen(&session).expect_ok("screen");
    assert_eq!(screen.at("/result/screen/dialog/delete/kind"), "file");

    // Esc cancels the dialog, which is no confirmation.
    driver.keys(&session, &["esc"]).expect_ok("esc");
    driver.wait_for_screen(&session, "the dialog to close", |screen| {
        screen["dialog"].is_null()
    });
    assert!(session.root.join("victim.bin").exists());

    let closed = driver.close(&session).expect_ok("close");
    assert_eq!(
        *closed.at("/result/fixture"),
        json!({"removed": 0, "unexpected": []})
    );
    driver.assert_gone(&session);
}

#[test]
fn an_idle_session_ends_by_itself_and_leaves_no_files() {
    let driver = Driver::new();
    let session = driver.open("delete-file", &["--idle-timeout", "2s"]);
    assert_eq!(session.opened.at("/result/idle_timeout_ms"), 2000);
    // The idle clock runs from the moment the session is ready, so from here on nothing may count
    // on the session still being there.

    // Nobody asked: the supervisor ended the session and removed everything of it.
    wait_until("the idle session to end", || {
        names(&driver.state_dir()).is_empty()
    });
    driver.assert_gone(&session);
    assert_eq!(
        *driver.list().at("/result"),
        json!({"sessions": [], "stale": []}),
        "the session ended cleanly, so it is not stale"
    );
}

#[test]
fn a_session_whose_supervisor_died_is_reported_stale_and_cleaned() {
    let driver = Driver::new();
    let session = driver.open("delete-file", &[]);
    assert!(alive(session.program));

    let killed = Command::new("kill")
        .args(["-KILL", &session.supervisor.to_string()])
        .status()
        .expect("kill runs");
    assert!(killed.success());
    wait_until("the supervisor to die", || !alive(session.supervisor));

    let listed = driver.list().expect_ok("list");
    assert_eq!(*listed.at("/result/sessions"), json!([]));
    assert_eq!(listed.at("/result/stale/0/session"), session.id.as_str());
    assert_eq!(listed.at("/result/stale/0/fixture"), "delete-file");
    assert_eq!(listed.at("/result/stale/0/cleaned"), true);
    assert_eq!(*listed.at("/result/stale/0/problems"), json!([]));
    driver.assert_gone(&session);
    assert_eq!(
        *driver.list().at("/result"),
        json!({"sessions": [], "stale": []})
    );

    // A command for the dead session says what happened.
    let after = driver.keys(&session, &["down"]);
    assert_eq!(after.at("/error/kind"), "no_such_session");
}

#[test]
fn a_command_for_a_session_that_died_cleans_it_and_says_so() {
    let driver = Driver::new();
    let session = driver.open("delete-file", &[]);
    Command::new("kill")
        .args(["-KILL", &session.supervisor.to_string()])
        .status()
        .expect("kill runs");
    wait_until("the supervisor to die", || !alive(session.supervisor));

    let lost = driver.screen(&session);
    assert_eq!(lost.code, 1);
    assert_eq!(lost.at("/error/kind"), "session_lost");
    assert!(
        lost.text("/error/message").contains("stale"),
        "{}",
        lost.value
    );
    driver.assert_gone(&session);
}

/// Kills the session's supervisor the way a crash would, and waits until it is gone.
fn kill_supervisor(session: &Session) {
    let killed = Command::new("kill")
        .args(["-KILL", &session.supervisor.to_string()])
        .status()
        .expect("kill runs");
    assert!(killed.success());
    wait_until("the supervisor to die", || !alive(session.supervisor));
}

/// Changes what the session's state file records about its processes.
fn rewrite_state(
    driver: &Driver,
    session: &Session,
    change: impl FnOnce(&mut serde_json::Map<String, Value>),
) {
    let path = driver.state_dir().join(&session.id).join("state.json");
    let mut state: Value =
        serde_json::from_slice(&fs::read(&path).expect("the state file")).expect("JSON");
    change(state.as_object_mut().expect("an object"));
    fs::write(&path, serde_json::to_vec_pretty(&state).expect("JSON"))
        .expect("the state file is rewritten");
}

#[test]
fn the_program_of_a_stale_session_is_found_by_its_mark_when_its_identity_was_not_recorded() {
    let driver = Driver::new();
    let without_identity = driver.open("delete-file", &[]);
    let without_anything = driver.open("delete-file", &[]);
    for session in [&without_identity, &without_anything] {
        assert!(alive(session.program));
        kill_supervisor(session);
    }
    // The supervisor can die before it says who its program is: with no identity for one of the
    // sessions, and not even the program's process id for the other, the mark in the program's
    // environment is all that says whose it is.
    rewrite_state(&driver, &without_identity, |state| {
        state.remove("child");
    });
    rewrite_state(&driver, &without_anything, |state| {
        state.remove("child");
        state.insert("child_pid".to_owned(), Value::Null);
    });

    let listed = driver.list().expect_ok("list");

    assert_eq!(*listed.at("/result/sessions"), json!([]));
    let stale = listed.at("/result/stale").as_array().expect("a list");
    assert_eq!(stale.len(), 2, "{}", listed.value);
    for entry in stale {
        assert_eq!(entry["cleaned"], true, "{entry}");
        assert_eq!(entry["problems"], json!([]), "{entry}");
    }
    for session in [&without_identity, &without_anything] {
        assert!(!alive(session.program), "the program was found and killed");
    }
    driver.assert_gone(&without_identity);
    driver.assert_gone(&without_anything);
}

#[test]
fn a_stale_cleanup_never_kills_the_process_that_took_over_a_recorded_id() {
    let driver = Driver::new();
    let stale = driver.open("delete-file", &[]);
    let bystander = driver.open("delete-file", &[]);
    kill_supervisor(&stale);
    // The program id the stale session recorded now belongs to another process: the program of
    // another session, which has the same executable and the same kind of arguments, and another
    // mark.
    rewrite_state(&driver, &stale, |state| {
        state.remove("child");
        state.insert("child_pid".to_owned(), json!(bystander.program));
    });

    let listed = driver.list().expect_ok("list");

    assert_eq!(listed.at("/result/stale/0/session"), stale.id.as_str());
    assert_eq!(
        listed.at("/result/stale/0/cleaned"),
        true,
        "{}",
        listed.value
    );
    assert_eq!(
        listed.at("/result/sessions/0/session"),
        bystander.id.as_str()
    );
    assert!(
        !alive(stale.program),
        "the stale session's own program is found by its mark"
    );
    assert!(
        alive(bystander.program),
        "the process that has the recorded id is not the stale session's"
    );
    driver
        .screen(&bystander)
        .expect_ok("the other session still answers");
    driver.close(&bystander).expect_ok("close");
    driver.assert_gone(&bystander);
}

#[test]
fn a_long_idle_between_commands_does_not_grow_the_documents() {
    let driver = Driver::new();
    let session = driver.open("delete-file", &[]);

    // The program draws about twenty frames a second for its first few seconds. Wait for enough
    // of them to make a digest worth summarizing; `events` reads what was drawn and takes none of
    // it away from the digest.
    wait_until("the program to draw some frames", || {
        driver
            .events(&session, 0)
            .event_kinds("/result/events/records")
            .iter()
            .filter(|kind| *kind == "frame")
            .count()
            >= 25
    });
    let keys = driver.keys(&session, &["/", "esc"]).expect_ok("keys");
    let frames = keys.at("/result/events/frames");
    assert!(frames["count"].as_u64().expect("a count") >= 20, "{frames}");
    assert!(frames["first_t_us"].as_u64() <= frames["last_t_us"].as_u64());
    assert_eq!(frames["last"]["kind"], "frame");
    assert_eq!(frames["last"]["t_us"], frames["last_t_us"]);
    let kinds = keys.event_kinds("/result/events/records");
    assert!(
        !kinds.contains(&"frame".to_owned()),
        "no frame is listed: {kinds:?}"
    );
    assert_eq!(kinds, ["scan_complete"]);
    let size = keys.at("/result/events").to_string().len();
    assert!(size < 1500, "the events of the document take {size} bytes");

    // `events` still returns every record, frames included.
    let since = keys.number("/result/events/since");
    let all = driver.events(&session, since).expect_ok("events");
    assert_eq!(all.number("/result/events/since"), since);
    let listed = all
        .event_kinds("/result/events/records")
        .iter()
        .filter(|kind| *kind == "frame")
        .count() as u64;
    assert!(listed >= frames["count"].as_u64().expect("a count"));

    driver.close(&session).expect_ok("close");
    driver.assert_gone(&session);
}

#[test]
fn a_command_that_is_not_valid_is_a_document_with_a_status_and_starts_nothing() {
    let driver = Driver::new();
    for (args, command, needle) in [
        (&["open"][..], Some("open"), "--fixture"),
        (
            &["open", "--fixture", "/tmp"][..],
            Some("open"),
            "delete-file",
        ),
        (
            &["open", "--fixture", "../../etc/passwd"][..],
            Some("open"),
            "the fixtures are",
        ),
        (
            &["open", "--fixture", "mount-boundary"][..],
            Some("open"),
            "volume",
        ),
        (
            &["open", "--fixture", "delete-file", "--size", "10x4"][..],
            Some("open"),
            "smaller",
        ),
        (&["bogus"][..], None, "unknown command"),
        (&["keys", "NOTANID", "down"][..], Some("keys"), "session"),
        (
            &["keys", "0123abcd", "no_such_key"][..],
            Some("keys"),
            "key",
        ),
        (
            &["delete", "0123abcd", "--name", "../x", "--kind", "file"][..],
            Some("delete"),
            "fixture-relative",
        ),
    ] {
        let reply = driver.run(args);
        assert_eq!(reply.code, 2, "{args:?}");
        assert_eq!(reply.at("/error/kind"), "usage", "{args:?}");
        assert_eq!(*reply.at("/command"), json!(command), "{args:?}");
        assert!(
            reply.text("/error/message").contains(needle),
            "{args:?}: {}",
            reply.value
        );
    }

    let missing = driver.run(&["keys", "0123abcd", "down"]);
    assert_eq!(missing.code, 1);
    assert_eq!(missing.at("/error/kind"), "no_such_session");

    // Nothing was started, made, or left.
    assert_eq!(
        *driver.list().at("/result"),
        json!({"sessions": [], "stale": []})
    );
    assert_eq!(names(&driver.state_dir()), Vec::<String>::new());
    assert_eq!(names(driver.work_dir()), Vec::<String>::new());
}

#[test]
fn an_argument_that_is_not_text_is_a_document_and_not_a_panic() {
    use std::os::unix::ffi::OsStrExt as _;

    let driver = Driver::new();
    let mut command = driver.command(&["keys", "0123abcd"]);
    command.arg(std::ffi::OsStr::from_bytes(b"k\xff"));

    let reply = Driver::run_command(command, "`tui keys` with a key that is not text");

    assert_eq!(reply.code, 2);
    assert_eq!(reply.at("/error/kind"), "usage");
    assert!(
        reply.text("/error/message").contains("UTF-8"),
        "{}",
        reply.value
    );
    assert_eq!(names(&driver.state_dir()), Vec::<String>::new());
}

#[test]
fn an_open_whose_program_ends_at_once_is_a_failure_and_leaves_nothing() {
    use std::os::unix::fs::PermissionsExt as _;

    let driver = Driver::new();
    // A program that ends before it draws anything.
    let program = driver.target.path().join("ends-at-once");
    fs::write(&program, "#!/bin/sh\nexit 3\n").expect("a script");
    fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).expect("executable");
    let mut command = driver.command(&["open", "--fixture", "delete-file"]);
    command.env("EXCISE_E2E_BINARY", &program);

    let reply = Driver::run_command(command, "`tui open` of a program that ends at once");

    assert_eq!(reply.code, 1);
    assert_eq!(reply.at("/error/kind"), "program_exited", "{}", reply.value);
    // The session directory and the run copy are gone, and so is the supervisor that reported it:
    // nothing is listed, stale or alive.
    assert_eq!(names(&driver.state_dir()), Vec::<String>::new());
    assert_eq!(names(driver.work_dir()), Vec::<String>::new());
    assert_eq!(
        *driver.list().at("/result"),
        json!({"sessions": [], "stale": []})
    );
}

#[test]
fn delete_follows_a_selection_a_cancelled_dialog_and_an_earlier_deletion() {
    let driver = Driver::new();
    let session = driver.open("delete-file", &[]);
    driver.wait_for_the_scan(&session);

    // The entry is selected first, through the filter, and `delete` takes the selection.
    driver.select(&session, "keep-a.bin");
    let first = driver
        .delete(&session, "keep-a.bin", "file")
        .expect_ok("the first delete");
    assert_eq!(first.at("/result/removed"), 1);
    assert!(!session.root.join("keep-a.bin").exists());

    // A dialog is opened and cancelled, and the filter is applied again with the same text, which
    // leaves the program with nothing selected: `delete` still finds the entry.
    driver.select(&session, "victim.bin");
    driver.keys(&session, &["backspace"]).expect_ok("backspace");
    driver.keys(&session, &["esc"]).expect_ok("esc");
    driver.apply_filter(&session, "victim.bin");

    // The entry the first deletion removed is no sentinel of the second.
    let second = driver
        .delete(&session, "victim.bin", "file")
        .expect_ok("the second delete");
    assert_eq!(second.at("/result/removed"), 1);
    assert_eq!(
        *second.at("/result/fixture"),
        json!({"removed": 2, "unexpected": []})
    );
    assert!(!session.root.join("victim.bin").exists());
    for sentinel in ["docs/keep-0.txt", "docs/keep-1.txt"] {
        assert!(session.root.join(sentinel).exists(), "{sentinel} survived");
    }

    let closed = driver.close(&session).expect_ok("close");
    assert_eq!(
        *closed.at("/result/fixture"),
        json!({"removed": 2, "unexpected": []})
    );
    driver.assert_gone(&session);
}

#[test]
fn a_folder_is_deleted_and_two_sessions_do_not_touch_each_other() {
    let driver = Driver::new();
    let first = driver.open("navigate-folders", &[]);
    let second = driver.open("navigate-folders", &[]);
    assert_ne!(first.id, second.id);
    assert_ne!(first.root, second.root);
    let listed = driver.list().expect_ok("list");
    assert_eq!(
        listed.at("/result/sessions").as_array().map(Vec::len),
        Some(2)
    );

    let deleted = driver
        .delete(&first, "music", "folder")
        .expect_ok("the folder is deleted");
    assert_eq!(deleted.at("/result/dialog/delete/kind"), "folder");
    assert_eq!(deleted.at("/result/fixture/unexpected"), &json!([]));
    assert!(!first.root.join("music").exists());
    for sentinel in ["photos/photo-0.jpg", "photos/photo-1.jpg", "readme.txt"] {
        assert!(first.root.join(sentinel).exists(), "{sentinel} survived");
    }
    assert!(
        second.root.join("music/track.mp3").exists(),
        "the other session is untouched"
    );

    for session in [&first, &second] {
        let closed = driver.close(session).expect_ok("close");
        assert_eq!(closed.at("/result/exit/code"), 0);
    }
    driver.assert_gone(&first);
    driver.assert_gone(&second);
}

#[test]
fn a_program_that_quits_during_keys_ends_the_session_with_its_exit_status() {
    let driver = Driver::new();
    let session = driver.open("delete-file", &[]);
    driver.wait_for_the_scan(&session);

    // The program quits the way a person quits it: `q` opens the quit dialog, and `y` confirms it.
    // The confirmation is a command of its own, once the screen shows the dialog, because a frame
    // that counts `q` can be drawn before the terminal has delivered the dialog.
    driver.keys(&session, &["q"]).expect_ok("q");
    driver.wait_for_screen(&session, "the quit dialog", |screen| {
        screen["dialog"].is_object()
    });
    let quit = driver.keys(&session, &["y"]).expect_ok("y");
    assert_eq!(quit.at("/result/exit/code"), 0, "{}", quit.value);
    assert_eq!(quit.at("/result/exit/via"), "exited");
    assert_eq!(quit.at("/result/exit/terminal_restored"), true);
    driver.assert_gone(&session);
    assert_eq!(driver.screen(&session).at("/error/kind"), "no_such_session");
}

#[test]
fn a_confirmation_key_is_never_sent_in_the_command_that_opens_the_dialog() {
    let driver = Driver::new();
    let session = driver.open("delete-file", &[]);
    driver.wait_for_the_scan(&session);
    driver.select(&session, "victim.bin");

    for confirmation in ["y", "enter"] {
        let refused = driver.keys(&session, &["backspace", confirmation]);
        assert_eq!(refused.code, 1, "{confirmation}");
        assert_eq!(refused.at("/error/kind"), "refused", "{confirmation}");
        assert_eq!(
            refused.at("/error/sent/0/key"),
            "backspace",
            "only the key before it was sent"
        );
        assert_eq!(refused.at("/error/sent/1"), &Value::Null, "{confirmation}");
        assert!(
            refused
                .text("/error/message")
                .contains("in a command of its own"),
            "{}",
            refused.text("/error/message")
        );
        driver.wait_for_screen(&session, "the deletion dialog", |screen| {
            screen["dialog"]["delete"]["kind"] == "file"
        });
        driver.keys(&session, &["esc"]).expect_ok("esc");
        driver.wait_for_screen(&session, "the dialog to close", |screen| {
            screen["dialog"].is_null()
        });
        assert!(session.root.join("victim.bin").exists(), "{confirmation}");
    }

    let closed = driver.close(&session).expect_ok("close");
    assert_eq!(
        *closed.at("/result/fixture"),
        json!({"removed": 0, "unexpected": []})
    );
    driver.assert_gone(&session);
}

#[test]
fn an_escape_and_a_confirmation_sent_as_one_key_are_refused_behind_the_quit_prompt() {
    let driver = Driver::new();
    let session = driver.open("delete-file", &[]);
    driver.wait_for_the_scan(&session);
    driver.select(&session, "victim.bin");

    // Backspace opens the deletion dialog, and Ctrl+C puts the quit prompt over it, so that the
    // screen shows no deletion dialog while one waits behind the prompt.
    driver.keys(&session, &["backspace"]).expect_ok("backspace");
    driver.wait_for_screen(&session, "the deletion dialog", |screen| {
        screen["dialog"]["delete"]["kind"] == "file"
    });
    driver.keys(&session, &["ctrl+c"]).expect_ok("ctrl+c");
    driver.wait_for_screen(&session, "the quit prompt", |screen| {
        screen["dialog"]["title"]
            .as_str()
            .is_some_and(|title| title.contains("QUIT"))
            && screen["dialog"]["delete"].is_null()
    });

    // The escape closes the prompt and the key then confirms the dialog it uncovers, if the
    // program reads them as two inputs: nothing of it is sent.
    for keys in [&["alt+y"][..], &["alt+Y"], &["alt+enter"]] {
        let refused = driver.keys(&session, keys);
        assert_eq!(refused.code, 1, "{keys:?}");
        assert_eq!(refused.at("/error/kind"), "refused", "{keys:?}");
        assert_eq!(*refused.at("/error/sent"), json!([]), "{keys:?}");
        assert!(
            refused.text("/error/message").contains("escape key"),
            "{keys:?}: {}",
            refused.text("/error/message")
        );
    }
    assert!(session.root.join("victim.bin").exists());

    // Sent apart, each key is guarded on what the screen shows when it is sent: the escape
    // uncovers the deletion dialog, and the key that would confirm it is refused.
    driver.keys(&session, &["esc"]).expect_ok("esc");
    driver.wait_for_screen(&session, "the deletion dialog to come back", |screen| {
        screen["dialog"]["delete"]["kind"] == "file"
    });
    let refused = driver.keys(&session, &["y"]);
    assert_eq!(refused.at("/error/kind"), "refused");
    assert!(session.root.join("victim.bin").exists());

    driver.keys(&session, &["esc"]).expect_ok("esc");
    driver.wait_for_screen(&session, "the dialog to close", |screen| {
        screen["dialog"].is_null()
    });
    let closed = driver.close(&session).expect_ok("close");
    assert_eq!(
        *closed.at("/result/fixture"),
        json!({"removed": 0, "unexpected": []})
    );
    driver.assert_gone(&session);
}

#[test]
fn text_that_would_finish_an_escape_sequence_is_never_sent_behind_a_deletion_dialog() {
    let driver = Driver::new();
    let session = driver.open("delete-file", &[]);
    driver.wait_for_the_scan(&session);
    driver.select(&session, "victim.bin");

    // `keys backspace` opens the deletion dialog. `alt+[` is `ESC [`, and the text `121u` after it
    // finishes `ESC [ 121 u`, which the program reads as `y`, though no key is a `y`: the program
    // joins the bytes of a sequence across writes. Only the key that begins the sequence is sent;
    // the first character of the text, which continues it, is refused.
    driver.keys(&session, &["backspace"]).expect_ok("backspace");
    driver.wait_for_screen(&session, "the deletion dialog", |screen| {
        screen["dialog"]["delete"]["kind"] == "file"
    });
    let refused = driver.keys(&session, &["alt+[", "type:121u"]);
    assert_eq!(refused.code, 1, "{}", refused.value);
    assert_eq!(refused.at("/error/kind"), "refused");
    assert_eq!(refused.at("/error/sent/0/key"), "alt+[");
    assert_eq!(
        refused.at("/error/sent/1"),
        &Value::Null,
        "`1` was not sent"
    );
    assert!(
        refused.text("/error/message").contains("escape sequence"),
        "{}",
        refused.text("/error/message")
    );

    // The session remembers what it wrote: the same text in a command of its own is refused too.
    let again = driver.keys(&session, &["type:121u"]);
    assert_eq!(again.code, 1, "{}", again.value);
    assert_eq!(again.at("/error/kind"), "refused");
    assert_eq!(*again.at("/error/sent"), json!([]), "no key was sent");

    // The dialog remains, and the fixture is as it was.
    driver.wait_for_screen(&session, "the deletion dialog", |screen| {
        screen["dialog"]["delete"]["kind"] == "file"
    });
    assert!(session.root.join("victim.bin").exists());

    // Every key after the one that began the sequence is refused, so the session is closed.
    let closed = driver.close(&session).expect_ok("close");
    assert_eq!(
        *closed.at("/result/fixture"),
        json!({"removed": 0, "unexpected": []})
    );
    driver.assert_gone(&session);
}

/// The variable through which a test paces the supervisor's reads of the program's output (see
/// the supervisor of the driver). It is not an option of any command.
const DRAIN_CAP_VARIABLE: &str = "EXCISE_HARNESS_TUI_DRAIN_BYTES_PER_SEC";

/// The pace of the terminal in the tests that put the screen behind the program, in bytes per
/// second: a frame takes a good part of a second to arrive, far longer than the few milliseconds
/// that a guard which read the screen right after the program reported a frame could count on.
const SLOW_TERMINAL: u64 = 20_000;

#[test]
fn on_a_slow_terminal_no_confirmation_reaches_a_dialog_the_screen_does_not_show_yet() {
    let driver = Driver::slow(SLOW_TERMINAL);
    let session = driver.open("delete-file", &[]);
    driver.wait_for_the_scan(&session);
    driver.select(&session, "victim.bin");

    // `keys backspace`, `keys ctrl+c`, `keys w y`: Backspace opens the deletion dialog, Ctrl+C puts
    // the quit prompt over it, and `w` closes the prompt, which uncovers the dialog again. The `y`
    // after it would confirm a dialog that nobody has read: it is refused, and only `w` is sent.
    driver.keys(&session, &["backspace"]).expect_ok("backspace");
    driver.keys(&session, &["ctrl+c"]).expect_ok("ctrl+c");
    let refused = driver.keys(&session, &["w", "y"]);
    assert_eq!(refused.code, 1, "{}", refused.value);
    assert_eq!(refused.at("/error/kind"), "refused");
    assert_eq!(refused.at("/error/sent/0/key"), "w");
    assert_eq!(
        refused.at("/error/sent/1"),
        &Value::Null,
        "`y` was not sent"
    );
    // The dialog remains, and the fixture is as it was.
    driver.wait_for_screen(&session, "the deletion dialog", |screen| {
        screen["dialog"]["delete"]["kind"] == "file"
    });
    assert!(session.root.join("victim.bin").exists());
    driver.keys(&session, &["esc"]).expect_ok("esc");
    driver.wait_for_screen(&session, "the dialog to close", |screen| {
        screen["dialog"].is_null()
    });

    // `keys backspace`, then `keys y` more than a second later, which is how long the driver once
    // waited after a Backspace before it believed a screen that showed no dialog.
    driver.keys(&session, &["backspace"]).expect_ok("backspace");
    thread::sleep(Duration::from_millis(1500));
    let refused = driver.keys(&session, &["y"]);
    assert_eq!(refused.code, 1, "{}", refused.value);
    assert_eq!(refused.at("/error/kind"), "refused");
    assert_eq!(*refused.at("/error/sent"), json!([]), "no key was sent");
    driver.wait_for_screen(&session, "the deletion dialog", |screen| {
        screen["dialog"]["delete"]["kind"] == "file"
    });
    assert!(session.root.join("victim.bin").exists());
    driver.keys(&session, &["esc"]).expect_ok("esc");
    driver.wait_for_screen(&session, "the dialog to close", |screen| {
        screen["dialog"].is_null()
    });

    let closed = driver.close(&session).expect_ok("close");
    assert_eq!(
        *closed.at("/result/fixture"),
        json!({"removed": 0, "unexpected": []})
    );
    driver.assert_gone(&session);
}

#[test]
fn on_a_slow_terminal_delete_still_deletes_exactly_its_target() {
    let driver = Driver::slow(SLOW_TERMINAL);
    let session = driver.open("delete-file", &[]);

    let deleted = driver
        .delete(&session, "victim.bin", "file")
        .expect_ok("delete");
    assert_eq!(deleted.at("/result/removed"), 1);
    assert_eq!(
        *deleted.at("/result/fixture"),
        json!({"removed": 1, "unexpected": []})
    );
    assert!(!session.root.join("victim.bin").exists());
    for sentinel in ["keep-a.bin", "docs/keep-0.txt", "docs/keep-1.txt"] {
        assert!(session.root.join(sentinel).exists(), "{sentinel} survived");
    }

    driver.close(&session).expect_ok("close");
    driver.assert_gone(&session);
}

#[test]
fn a_state_directory_that_is_a_link_is_followed_by_no_command() {
    let driver = Driver::new();
    let session = driver.open("delete-file", &[]);
    // Another target directory whose state directory is a link to the first one's, and, in the
    // state directory, a directory that is named like a session and old enough to look stale.
    let linked = TempDir::new().expect("another target directory");
    std::os::unix::fs::symlink(driver.state_dir(), linked.path().join("excise-tui"))
        .expect("a link");
    let decoy = driver.state_dir().join("cafef00d");
    fs::create_dir(&decoy).expect("a decoy");
    fs::write(decoy.join("precious.txt"), b"keep").expect("a file");
    fs::File::open(&decoy)
        .and_then(|file| file.set_modified(std::time::SystemTime::now() - Duration::from_hours(1)))
        .expect("an old directory");

    let through_the_link = |args: &[&str]| {
        let mut command = driver.command(args);
        command.env("CARGO_TARGET_DIR", linked.path());
        Driver::run_command(command, &format!("`tui {}` through a link", args.join(" ")))
    };
    for args in [
        vec!["list"],
        vec!["screen", session.id.as_str()],
        vec!["events", session.id.as_str()],
        vec!["keys", session.id.as_str(), "down"],
        vec!["close", session.id.as_str()],
    ] {
        let refused = through_the_link(&args);
        assert_eq!(refused.code, 1, "{args:?}");
        assert!(
            refused.text("/error/message").contains("link"),
            "{args:?}: {}",
            refused.text("/error/message")
        );
    }

    assert!(
        decoy.join("precious.txt").exists(),
        "nothing behind the link was cleaned"
    );
    // The session was not touched, and answers in its own state directory.
    driver.screen(&session).expect_ok("the session answers");
    driver.close(&session).expect_ok("close");
    driver.assert_gone(&session);
}

#[test]
fn the_cleanup_of_a_test_never_signals_a_process_it_cannot_prove_is_its_own() {
    // A test that crashed leaves a state file that names process ids, and one of them is an
    // unrelated process by the time the next cleanup reads it.
    let mut bystander = Command::new("/bin/sleep")
        .arg("120")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("sleep starts");
    let pid = u64::from(bystander.id());
    {
        let driver = Driver::new();
        let session = driver.state_dir().join("0123abcd");
        fs::create_dir_all(session.join("req")).expect("a session directory");
        fs::create_dir_all(session.join("rep")).expect("a session directory");
        fs::write(
            session.join("state.json"),
            json!({"supervisor_pid": pid, "ready": true, "root": null, "child_pid": pid})
                .to_string(),
        )
        .expect("a state file");
        // The driver goes out of scope here: its cleanup is what is tested.
    }

    let still_there = alive(pid);
    let _ = bystander.kill();
    let _ = bystander.wait();
    assert!(
        still_there,
        "the cleanup signaled a process because a state file held its id"
    );
}
