//! Residue after error exits that the release binary can reach before it ever enters the
//! terminal: an invalid configuration (`src/test_events.rs`'s `EXCISE_TEST_EVENTS` validation,
//! `src/error.rs`'s `ExitClass::Config`).
//!
//! Setting `EXCISE_TEST_EVENTS` to a path that already exists is one such configuration error:
//! the channel is created exclusively and never overwrites an existing path. `excise` checks it
//! before it validates the terminal or enters the alternate screen, so the failure happens before
//! any of that, and the process never touches anything but the scratch area the harness already
//! isolates it to.

use std::{process::Command, time::Duration};

use excise_harness::{
    fixture::Fixtures,
    headless::process::{self, Ended},
    runner::work_base,
    safety::{FixtureRoot, Scratch, isolated_env},
    scenario::Profile,
};

/// A fast, privilege-free fixture: what is scanned does not matter here, because the process
/// exits before it reads a single entry.
const FIXTURE: &str = "wide-1k";
/// How long the process may run before its process group is killed. The failure is immediate;
/// this only bounds a process that, contrary to the finding, does not fail fast.
const TIMEOUT: Duration = Duration::from_secs(20);

#[test]
fn an_events_path_that_already_exists_is_a_configuration_error_with_no_residue() {
    let binary = std::path::Path::new(env!("CARGO_BIN_EXE_excise"));
    let master = Fixtures::bundled()
        .master(FIXTURE)
        .expect("the bundled fixture materializes");
    let fixture = FixtureRoot::open(&master.root)
        .expect("the fixture carries the ownership marker")
        .path()
        .to_path_buf();

    let work = work_base().join(format!("xh-test-error-exits-{}", std::process::id()));
    std::fs::create_dir_all(&work).expect("the work area is writable");
    let scratch = Scratch::create(&work).expect("a scratch area can be created");

    // Pre-create the event file. `excise` creates it exclusively and never opens an existing
    // path, so this one byte of setup turns the channel into a configuration error.
    std::fs::write(scratch.events(), b"").expect("the event file can be pre-created");

    let mut command = Command::new(binary);
    command
        .arg(&fixture)
        .env_clear()
        .envs(isolated_env(&scratch, Profile::Default, true, None))
        .current_dir(scratch.cwd());
    let finished =
        process::run(&mut command, TIMEOUT, false, false).expect("the process can be run");

    assert!(
        !finished.timed_out,
        "a configuration error must fail fast, not run out the clock: {finished:?}"
    );
    assert_eq!(
        finished.ended,
        Ended::Exited(78),
        "an unusable EXCISE_TEST_EVENTS path is a configuration error (exit 78): {finished:?}"
    );
    assert_eq!(
        scratch.residue().expect("the scratch area can be read"),
        Vec::<String>::new(),
        "a configuration error caught before terminal entry must leave nothing behind"
    );

    let _ = std::fs::remove_dir_all(&work);
}
