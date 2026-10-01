//! F4: killed or interrupted `excise` sessions leave `.excise-scan-*` directories in the scratch
//! parent named by `EXCISE_SCAN_STORE_DIR`, and nothing sweeps them
//! (`src/scan_store/storage.rs:11,59-94`).
//!
//! This test builds one scan-store parent shared by three `excise` processes: a session killed
//! with `SIGKILL` (process termination on Windows) right after its session directory appears
//! under it, a session kept running in its own pseudo-terminal until the test quits it, and a
//! directory with the same `.excise-scan-*` name shape that no `excise` process ever made. A
//! third `excise`, started against the same parent and left to finish a headless scan normally,
//! must never touch the live session's directory or the unverified one. It also never touches the
//! dead session's directory today, which is F4. That last assertion is a strict expected
//! failure: it holds while the defect is present, and its failure message names the finding and
//! the slice that fixes it.

use std::{
    collections::BTreeSet,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU32, Ordering},
    time::{Duration, Instant},
};

use excise_harness::{
    fixture::{Fixtures, tree::remove_tree},
    headless::process::{self, Ended},
    pty::{
        PtySession, SpawnSpec,
        ui::{self, DialogView, HeaderState},
    },
    runner::work_base,
    safety::{FixtureRoot, Scratch, isolated_env},
    scenario::{DEFAULT_TERMINAL_COLS, DEFAULT_TERMINAL_ROWS, Profile},
};

/// `excise`'s private scan-store session prefix (`src/scan_store/storage.rs:11`). The harness
/// crate does not export it: this test hardcodes it exactly as
/// `crates/excise-harness/src/safety/scratch.rs`'s and `src/tests/scenario_runner.rs`'s own tests
/// already do.
const SESSION_PREFIX: &str = ".excise-scan-";
/// A fast, privilege-free fixture: a single directory of 1,000 small files. What is scanned does
/// not matter here, only that starting `excise` creates a session directory and a clean exit
/// removes it.
const FIXTURE: &str = "wide-1k";
/// How long a freshly spawned `excise` has to create its scan-store session directory. A first
/// launch on a cold Windows runner takes well over 10 s.
const SESSION_WAIT: Duration = Duration::from_secs(30);
/// How long the live session has to reach `COMPLETE` on the small fixture.
const COMPLETE_WAIT: Duration = Duration::from_secs(30);
/// How long a confirmed quit has to open its dialog and then exit.
const QUIT_WAIT: Duration = Duration::from_secs(10);
/// How long the headless scan may run before its process group is killed.
const HEADLESS_TIMEOUT: Duration = Duration::from_secs(60);
/// How often a bounded wait looks at the file system again.
const FS_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// A unique directory under the harness work area, removed (with everything under it) when
/// dropped.
struct Workspace(PathBuf);

impl Workspace {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = work_base().join(format!(
            "xh-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("the work area is writable");
        Self(path)
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// One `excise` TUI session pointed at the shared scan-store parent `store`.
///
/// `_scratch` is kept alive only so the session's `HOME`, configuration, working directory, and
/// temporary directory outlive it; `EXCISE_SCAN_STORE_DIR` is overridden to `store` instead of the
/// scratch's own (which stays empty), which is how three separate sessions share one parent.
struct TuiSession {
    pty: PtySession,
    _scratch: Scratch,
}

impl TuiSession {
    fn spawn(binary: &Path, fixture: &Path, store: &Path, work_dir: &Path, title: &str) -> Self {
        let scratch = Scratch::create(work_dir).expect("a scratch area can be created");
        let spec = SpawnSpec {
            program: binary.to_path_buf(),
            args: vec![fixture.as_os_str().to_owned()],
            env: session_env(&scratch, store),
            cwd: scratch.cwd(),
            cols: DEFAULT_TERMINAL_COLS,
            rows: DEFAULT_TERMINAL_ROWS,
            drain_bytes_per_sec: None,
            recording: None,
            title: Some(title.to_owned()),
        };
        let pty = PtySession::spawn(&spec).expect("excise spawns in a pseudo-terminal");
        Self {
            pty,
            _scratch: scratch,
        }
    }
}

/// `isolated_env`'s environment for `scratch` under the `deterministic` profile (reduced motion,
/// one scan thread: irrelevant to F4, but it keeps COMPLETE and idle redraws fast and predictable
/// instead of depending on F2/F3, which are different, unfixed findings), with
/// `EXCISE_SCAN_STORE_DIR` pointing at the shared scan-store parent `store`.
fn session_env(scratch: &Scratch, store: &Path) -> Vec<(OsString, OsString)> {
    isolated_env(scratch, Profile::Deterministic, false, Some(store))
}

/// Every entry directly under `parent`, by name.
fn entry_names(parent: &Path) -> BTreeSet<String> {
    fs::read_dir(parent)
        .expect("the scan-store parent can be read")
        .map(|entry| {
            entry
                .expect("a directory entry can be read")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

/// Pumps `session` until an entry whose name starts with `.excise-scan-` and is not in `known`
/// appears under `parent`, or `timeout` passes. Returns the new entry's name.
///
/// Pumping answers the terminal's cursor position requests: `ConPTY` on Windows waits for that
/// answer before it runs anything, so an unpumped session never gets as far as its scan store.
fn wait_for_new_session_entry(
    session: &mut PtySession,
    parent: &Path,
    known: &BTreeSet<String>,
    timeout: Duration,
) -> String {
    let deadline = Instant::now() + timeout;
    loop {
        session.pump().expect("the session's output can be read");
        if let Ok(entries) = fs::read_dir(parent) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with(SESSION_PREFIX) && !known.contains(&name) {
                    return name;
                }
            }
        }
        if let Some(exit) = session.exit() {
            panic!(
                "excise exited before its session directory appeared: {}",
                exit.describe()
            );
        }
        assert!(
            Instant::now() < deadline,
            "no new `{SESSION_PREFIX}*` entry appeared under {} within {timeout:?}:\n{}",
            parent.display(),
            session.screen().text()
        );
        session
            .wait_activity(FS_POLL_INTERVAL)
            .expect("the session's output can be read");
    }
}

/// Waits for the header badge to read `COMPLETE`.
fn wait_for_complete(session: &mut PtySession, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        session.pump().expect("the session's output can be read");
        if matches!(
            ui::header_state(session.screen()),
            Some(HeaderState::Complete)
        ) {
            return;
        }
        if let Some(exit) = session.exit() {
            panic!(
                "the live session exited before COMPLETE: {}",
                exit.describe()
            );
        }
        assert!(
            Instant::now() < deadline,
            "the live session did not reach COMPLETE within {timeout:?}:\n{}",
            session.screen().text()
        );
        session
            .wait_activity(Duration::from_millis(20))
            .expect("the session's output can be read");
    }
}

/// Presses `q`, waits for the plain `[y] Quit` dialog, confirms it, and waits for a clean exit.
fn quit_normally(session: &mut PtySession, timeout: Duration) {
    let dialog_deadline = Instant::now() + timeout;
    session.send(b"q").expect("the session accepts input");
    loop {
        session.pump().expect("the session's output can be read");
        if let DialogView::Other(view) = ui::dialog_view(session.screen())
            && ui::offers_plain_quit(&view)
        {
            break;
        }
        assert!(
            Instant::now() < dialog_deadline,
            "the quit dialog did not open within {timeout:?}:\n{}",
            session.screen().text()
        );
        session
            .wait_activity(Duration::from_millis(20))
            .expect("the session's output can be read");
    }
    session.send(b"y").expect("the session accepts input");
    let exit_deadline = Instant::now() + timeout;
    loop {
        session.pump().expect("the session's output can be read");
        if let Some(exit) = session.exit() {
            assert_eq!(
                exit.code,
                Some(0),
                "a confirmed quit should exit 0: {}",
                exit.describe()
            );
            return;
        }
        assert!(
            Instant::now() < exit_deadline,
            "excise did not exit after a confirmed quit within {timeout:?}"
        );
        session
            .wait_activity(Duration::from_millis(20))
            .expect("the session's output can be read");
    }
}

/// Runs `excise --format json --output <scratch>/scan-report.json <fixture>` to completion, with
/// `EXCISE_SCAN_STORE_DIR` pointed at the shared `store` parent.
fn run_headless_scan(binary: &Path, fixture: &Path, store: &Path, work_dir: &Path) {
    let scratch = Scratch::create(work_dir).expect("a scratch area can be created");
    let mut command = Command::new(binary);
    command
        .args(["--format", "json", "--output"])
        .arg(scratch.report())
        .arg(fixture)
        .env_clear()
        .envs(session_env(&scratch, store))
        .current_dir(scratch.cwd());
    let finished = process::run(&mut command, HEADLESS_TIMEOUT, false, false)
        .expect("the headless scan can run");
    assert!(
        !finished.timed_out,
        "the headless scan timed out: {finished:?}"
    );
    assert_eq!(
        finished.ended,
        Ended::Exited(0),
        "the headless scan should finish cleanly: {finished:?}"
    );
}

#[test]
fn nothing_sweeps_a_dead_scan_store_session_while_live_and_unverified_directories_survive() {
    let binary = Path::new(env!("CARGO_BIN_EXE_excise"));
    let master = Fixtures::bundled()
        .master(FIXTURE)
        .expect("the bundled fixture materializes");
    let fixture = FixtureRoot::open(&master.root)
        .expect("the fixture carries the ownership marker")
        .path()
        .to_path_buf();

    let work = Workspace::new();
    let store = work.0.join("store");
    fs::create_dir_all(&store).expect("the shared scan-store parent can be created");
    let mut known = BTreeSet::new();

    // A dead session: killed as soon as its session directory exists, before it ever reaches
    // `COMPLETE`. Nothing it did is confirmed or cleaned up.
    let mut dead = TuiSession::spawn(binary, &fixture, &store, &work.0, "dead session");
    let dead_name = wait_for_new_session_entry(&mut dead.pty, &store, &known, SESSION_WAIT);
    known.insert(dead_name.clone());
    dead.pty.kill();
    let dead_dir = store.join(&dead_name);
    assert!(
        dead_dir.is_dir(),
        "the dead session's directory should still be there right after the kill"
    );

    // A live session: stays up in its own pseudo-terminal until it is quit at the end.
    let mut live = TuiSession::spawn(binary, &fixture, &store, &work.0, "live session");
    let live_name = wait_for_new_session_entry(&mut live.pty, &store, &known, SESSION_WAIT);
    known.insert(live_name.clone());
    wait_for_complete(&mut live.pty, COMPLETE_WAIT);

    // An unverified directory: no `excise` process ever made this one.
    let unverified_name = format!("{SESSION_PREFIX}not-made-by-excise");
    fs::create_dir(store.join(&unverified_name)).expect("the unverified directory can be created");

    // A third excise finishes a headless scan of the same fixture normally, against the same
    // scan-store parent.
    run_headless_scan(binary, &fixture, &store, &work.0);

    // The live session's directory and the unverified directory survive today, and must keep
    // surviving once X4 ships.
    let live_dir = store.join(&live_name);
    let unverified_dir = store.join(&unverified_name);
    assert!(
        live_dir.is_dir(),
        "the live session's directory must survive a third excise's normal start and exit"
    );
    assert!(
        unverified_dir.is_dir(),
        "the unverified directory must survive a third excise's normal start and exit"
    );
    // F4: nothing sweeps a dead session's directory today. This is a strict expected failure: X4,
    // a startup sweep of verified, unlocked, same-user sessions, must make this assertion fail,
    // and then it flips to assert that the directory is gone.
    assert!(
        dead_dir.is_dir(),
        "F4 is fixed: flip R4 to assert that the dead session is swept (X4)"
    );

    // Quit the live session normally. The scan-store parent must then hold exactly the dead
    // session and the unverified directory: nothing else leaked, and nothing else was swept.
    quit_normally(&mut live.pty, QUIT_WAIT);
    assert_eq!(
        entry_names(&store),
        BTreeSet::from([dead_name, unverified_name]),
        "the scan-store parent should hold only the dead session and the unverified directory"
    );
    remove_tree(&dead_dir).expect("the dead session's directory can be removed");
    remove_tree(&unverified_dir).expect("the unverified directory can be removed");
    assert!(
        entry_names(&store).is_empty(),
        "the scan-store parent should be empty once both are removed"
    );
}
