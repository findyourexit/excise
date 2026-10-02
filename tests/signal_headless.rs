//! A headless run's own confirmed-quit semantics (no terminal, no owner loop): SIGTERM, SIGHUP,
//! or SIGQUIT delivered while a scan is still running still exits 130 with a cancelled
//! `scan-report` document, and leaves no `.excise-scan-*` entry in the scratch parent named by
//! `EXCISE_SCAN_STORE_DIR` (`src/signals.rs`, `src/runtime/mod.rs`'s
//! `scan_headless_with_scan_store_session`). Unix-only: delivering a real signal to an arbitrary
//! child process needs `excise_harness::safety::send_signal`, which reports every signal as
//! unsupported elsewhere (see that module's own documentation).

#![cfg(unix)]

use std::{
    fs,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use excise_harness::{
    fixture::Fixtures,
    safety::{FixtureRoot, Scratch, isolated_env, send_signal},
    scenario::{Profile, Signal},
};

/// A fixture large enough that a headless scan is still running a short while after the
/// scan-store session directory first appears: the same fixture the `signal-*-mid-scan` PTY
/// scenarios use for the equivalent interactive case.
const FIXTURE: &str = "node-modules-2k";
/// How long the session directory has to appear after spawning.
const SESSION_WAIT: Duration = Duration::from_secs(30);
/// How long the signalled process has to exit afterward.
const EXIT_WAIT: Duration = Duration::from_secs(15);
/// How often a bounded wait looks at the file system or the child again.
const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// `excise`'s private scan-store session prefix (`src/scan_store/storage.rs:11`). The harness
/// crate does not export it: this test hardcodes it exactly as other tests already do.
const SESSION_PREFIX: &str = ".excise-scan-";

/// The scan-store parent's only entries starting with `SESSION_PREFIX`.
fn session_entries(parent: &std::path::Path) -> Vec<String> {
    let Ok(read) = fs::read_dir(parent) else {
        return Vec::new();
    };
    read.filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.starts_with(SESSION_PREFIX))
        .collect()
}

/// Waits for exactly one `.excise-scan-*` entry to appear under `parent`, and returns once it
/// does. Panics if `timeout` passes first.
fn wait_for_session_entry(parent: &std::path::Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while session_entries(parent).is_empty() {
        assert!(
            Instant::now() < deadline,
            "the headless scan never created its scan-store session directory under {}",
            parent.display()
        );
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Waits for `child` to exit on its own, without killing it. Panics if `timeout` passes first.
fn wait_for_exit(child: &mut std::process::Child, timeout: Duration) -> std::process::ExitStatus {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child
            .try_wait()
            .expect("waiting on the child should not fail")
        {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "the signalled headless scan never exited"
        );
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Spawns a headless scan of `fixture` under `scratch`, writing its report to
/// `scratch.report()`, and returns the running child.
fn spawn_headless(
    binary: &std::path::Path,
    fixture: &std::path::Path,
    scratch: &Scratch,
) -> std::process::Child {
    let mut command = Command::new(binary);
    command
        .args(["--format", "json", "--output"])
        .arg(scratch.report())
        .arg(fixture)
        .env_clear()
        .envs(isolated_env(scratch, Profile::Deterministic, false, None))
        .current_dir(scratch.cwd())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command.spawn().expect("excise should spawn")
}

/// One signal's end-to-end proof: delivered mid-scan, it exits 130, writes a cancelled report,
/// and leaves no session directory behind.
fn assert_signal_mid_scan_is_a_clean_confirmed_quit(signal: Signal) {
    let binary = std::path::Path::new(env!("CARGO_BIN_EXE_excise"));
    let master = Fixtures::bundled()
        .master(FIXTURE)
        .expect("the bundled fixture materializes");
    let fixture = FixtureRoot::open(&master.root)
        .expect("the fixture carries the ownership marker")
        .path()
        .to_path_buf();

    let work = excise_harness::runner::work_base();
    fs::create_dir_all(&work).expect("the harness work area exists");
    let scratch = Scratch::create(&work).expect("a scratch area can be created");

    let mut child = spawn_headless(binary, &fixture, &scratch);
    wait_for_session_entry(&scratch.store(), SESSION_WAIT);

    // The session directory existing is not proof the scan is still running (it is created
    // before the scan starts), so give it a brief moment to actually be mid-scan; 2,000 entries
    // of real filesystem I/O reliably outlasts this on every machine this suite runs on, exactly
    // as the equivalent interactive `signal-*-mid-scan` PTY scenarios already rely on.
    std::thread::sleep(Duration::from_millis(20));
    assert!(
        child
            .try_wait()
            .expect("checking the child should not fail")
            .is_none(),
        "the scan finished before the signal could land: make the fixture larger or the sleep \
         longer, not weaker assertions below"
    );

    send_signal(child.id(), signal).unwrap_or_else(|error| {
        panic!("sending {signal} to the running headless scan should succeed: {error}")
    });

    let status = wait_for_exit(&mut child, EXIT_WAIT);
    assert_eq!(
        status.code(),
        Some(130),
        "a signal mid-scan must exit 130, not {status:?}"
    );

    let report = fs::read_to_string(scratch.report())
        .unwrap_or_else(|error| panic!("the cancelled report should have been written: {error}"));
    assert!(
        report.contains("\"state\": \"cancelled\""),
        "a signal mid-scan must write a cancelled report, got:\n{report}"
    );

    assert_eq!(
        session_entries(&scratch.store()),
        Vec::<String>::new(),
        "the scan-store session directory must not survive a signal mid-scan"
    );
}

#[test]
fn sigterm_mid_scan_exits_130_with_a_cancelled_report_and_no_session_residue() {
    assert_signal_mid_scan_is_a_clean_confirmed_quit(Signal::Term);
}

#[test]
fn sighup_mid_scan_exits_130_with_a_cancelled_report_and_no_session_residue() {
    assert_signal_mid_scan_is_a_clean_confirmed_quit(Signal::Hup);
}

#[test]
fn sigquit_mid_scan_exits_130_with_a_cancelled_report_and_no_session_residue() {
    assert_signal_mid_scan_is_a_clean_confirmed_quit(Signal::Quit);
}
