//! Dead scan-store sessions are swept at the next start (`src/scan_store/sweep.rs`).
//!
//! A session killed with `SIGKILL` (process termination on Windows) cannot remove its own
//! `.excise-scan-*` directory from the scratch parent named by `EXCISE_SCAN_STORE_DIR`. Every
//! start sweeps that parent before it creates its own session, and removes a directory only when
//! it is a verified excise session whose lock no process holds.
//!
//! The first test builds one scan-store parent shared by `excise` processes: a directory with the
//! `.excise-scan-*` name shape that no `excise` process ever made, a session kept running in its
//! own pseudo-terminal until the test quits it, and a session killed with no cleanup once it has
//! finished setting itself up. A further `excise`, started against the same parent and left to
//! finish a headless scan normally, must remove the dead session's directory and nothing else.
//!
//! The second test starts several `excise` processes at once against a parent that also holds a
//! dead session. Each sweeps while the others create and use their sessions, so a sweep that
//! removed a session being set up or in use would fail that session's scan.

use std::{
    collections::BTreeSet,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU32, Ordering},
    thread,
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

/// `excise`'s private scan-store session prefix (`src/scan_store/storage.rs`). The harness crate
/// does not export it: this test hardcodes it exactly as
/// `crates/excise-harness/src/safety/scratch.rs`'s and `src/tests/scenario_runner.rs`'s own tests
/// already do.
const SESSION_PREFIX: &str = ".excise-scan-";
/// The file inside a session directory that holds its lock and, once the session has taken the
/// lock, the owner marker (`src/scan_store/session_lock.rs`). It is empty until then, and a
/// session killed before that is a directory no sweep can tell from one `excise` never made.
const LOCK_FILE: &str = "session.lock";
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

/// Pumps `session` until the session directory `dir` has finished setting itself up: its lock
/// file holds the owner marker, which the session writes only after it has taken the lock.
fn wait_for_session_marker(session: &mut PtySession, dir: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        session.pump().expect("the session's output can be read");
        if fs::metadata(dir.join(LOCK_FILE)).is_ok_and(|meta| meta.len() > 0) {
            return;
        }
        if let Some(exit) = session.exit() {
            panic!(
                "excise exited before its session was set up: {}",
                exit.describe()
            );
        }
        assert!(
            Instant::now() < deadline,
            "{} did not get its owner marker within {timeout:?}:\n{}",
            dir.display(),
            session.screen().text()
        );
        session
            .wait_activity(FS_POLL_INTERVAL)
            .expect("the session's output can be read");
    }
}

/// Starts a pseudo-terminal session against the shared `store`, waits until its session
/// directory exists and is set up, then kills it with no chance to clean up. Returns the
/// directory it leaves behind.
fn leave_dead_session(
    binary: &Path,
    fixture: &Path,
    store: &Path,
    work_dir: &Path,
    known: &mut BTreeSet<String>,
    title: &str,
) -> PathBuf {
    let mut dead = TuiSession::spawn(binary, fixture, store, work_dir, title);
    let name = wait_for_new_session_entry(&mut dead.pty, store, known, SESSION_WAIT);
    known.insert(name.clone());
    let directory = store.join(name);
    wait_for_session_marker(&mut dead.pty, &directory, SESSION_WAIT);
    dead.pty.kill();
    assert!(
        directory.is_dir(),
        "the dead session's directory should still be there right after the kill"
    );
    directory
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
fn a_dead_scan_store_session_is_swept_while_live_and_unverified_directories_survive() {
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

    // An unverified directory: no `excise` process ever made this one. It is there for every
    // start below.
    let unverified_name = format!("{SESSION_PREFIX}not-made-by-excise");
    let unverified_dir = store.join(&unverified_name);
    fs::create_dir(&unverified_dir).expect("the unverified directory can be created");
    known.insert(unverified_name.clone());

    // A live session: stays up in its own pseudo-terminal until it is quit at the end.
    let mut live = TuiSession::spawn(binary, &fixture, &store, &work.0, "live session");
    let live_name = wait_for_new_session_entry(&mut live.pty, &store, &known, SESSION_WAIT);
    known.insert(live_name.clone());
    let live_dir = store.join(&live_name);
    wait_for_complete(&mut live.pty, COMPLETE_WAIT);

    // A dead session: killed once it has set itself up, before it ever reaches `COMPLETE`.
    // Nothing it did is confirmed or cleaned up. Its own start swept the parent too, and found
    // the live session and the unverified directory, which it left alone.
    let dead_dir = leave_dead_session(
        binary,
        &fixture,
        &store,
        &work.0,
        &mut known,
        "dead session",
    );
    assert!(
        live_dir.is_dir() && unverified_dir.is_dir(),
        "a start that swept the parent must leave a live session and an unverified directory"
    );

    // A further excise finishes a headless scan of the same fixture normally, against the same
    // scan-store parent. Starting it sweeps the parent.
    run_headless_scan(binary, &fixture, &store, &work.0);

    assert!(
        !dead_dir.exists(),
        "the dead session's directory should have been swept by the next start"
    );
    assert!(
        live_dir.is_dir(),
        "the live session's directory must survive another excise's start and exit"
    );
    assert!(
        fs::metadata(live_dir.join(LOCK_FILE)).is_ok_and(|meta| meta.len() > 0),
        "the live session must keep its lock file and owner marker"
    );
    assert!(
        unverified_dir.is_dir(),
        "the unverified directory must survive another excise's start and exit"
    );

    // Quit the live session normally. The scan-store parent must then hold exactly the
    // unverified directory: nothing else leaked, and nothing but the dead session was swept.
    quit_normally(&mut live.pty, QUIT_WAIT);
    assert_eq!(
        entry_names(&store),
        BTreeSet::from([unverified_name]),
        "the scan-store parent should hold only the unverified directory"
    );
    remove_tree(&unverified_dir).expect("the unverified directory can be removed");
    assert!(
        entry_names(&store).is_empty(),
        "the scan-store parent should be empty once it is removed"
    );
}

#[test]
fn excise_processes_starting_together_never_remove_each_others_live_sessions() {
    /// Processes started at once in each round.
    const STARTERS: usize = 4;
    /// Rounds of simultaneous starts. Only the first has a dead session to sweep; every round
    /// has each process sweeping while the others set up and use their own sessions.
    const ROUNDS: usize = 3;

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
    let unverified_name = format!("{SESSION_PREFIX}not-made-by-excise");
    let unverified_dir = store.join(&unverified_name);
    fs::create_dir(&unverified_dir).expect("the unverified directory can be created");
    known.insert(unverified_name.clone());
    let dead_dir = leave_dead_session(
        binary,
        &fixture,
        &store,
        &work.0,
        &mut known,
        "dead session",
    );

    for round in 0..ROUNDS {
        // A headless scan fails if its session directory is removed under it, so every process
        // finishing with exit 0 shows that no sweep removed a session that was being set up or
        // used. Each scan panics, and the scope re-panics, when one does not.
        thread::scope(|scope| {
            for _ in 0..STARTERS {
                scope.spawn(|| run_headless_scan(binary, &fixture, &store, &work.0));
            }
        });
        if round == 0 {
            assert!(
                !dead_dir.exists(),
                "the starts of the first round should have swept the dead session's directory"
            );
        }
        assert_eq!(
            entry_names(&store),
            BTreeSet::from([unverified_name.clone()]),
            "after round {round} the scan-store parent should hold only the unverified directory"
        );
    }
    remove_tree(&unverified_dir).expect("the unverified directory can be removed");
}
