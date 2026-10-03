//! Tests of the in-process scenario runner: what it runs, what it refuses, and how it fails.

use std::fs;
use std::path::Path;
use std::time::Duration;

use crossterm::event::KeyCode;
use excise_harness::fixture::{MARKER_FILE_NAME, mutate};
use excise_harness::report::Verdict;
use excise_harness::scenario::{Profile, Scenario};

use crate::tests::scenario_runner::{
    Fixture, Limits, RunError, ScenarioRun, StepFailure, UnsupportedStep, materialize,
    run_scenario, run_scenario_with, scenarios_directory, select_scenarios, unsupported_steps,
};

/// The top of a scenario over the `delete-file` fixture. The sentinels sit beside the file the
/// scenarios delete and one folder below it.
const DELETE_FILE: &str = r#"
schema_version = 1
name = "scenario-under-test"
description = "A scenario that a test built."
fixture = "delete-file"
sentinels = ["keep-a.bin", "docs/keep-0.txt"]
profiles = ["default", "deterministic"]
"#;

/// Every file of the `delete-file` fixture.
const DELETE_FILE_FILES: [&str; 4] = [
    "victim.bin",
    "keep-a.bin",
    "docs/keep-0.txt",
    "docs/keep-1.txt",
];

const WAIT_FOR_COMPLETE: &str = r#"
[[steps]]
step = "wait_header"
state = "complete"
"#;

fn scenario(header: &str, steps: &str) -> Scenario {
    let source = format!("{header}\n{steps}");
    Scenario::from_toml_str(&source)
        .unwrap_or_else(|error| panic!("the test scenario should parse: {error}\n{source}"))
}

/// Runs `scenario` under `profile` on a fresh fixture, which the caller keeps to inspect it.
fn execute(scenario: &Scenario, profile: Profile) -> (Fixture, ScenarioRun) {
    let fixture = materialize(&scenario.fixture).expect("the fixture should build");
    let run = run_scenario(scenario, profile, fixture.root()).expect("the scenario should run");
    (fixture, run)
}

fn failure_of(run: &ScenarioRun) -> &StepFailure {
    run.failure
        .as_ref()
        .unwrap_or_else(|| panic!("the scenario should have failed:\n{run}"))
}

#[test]
fn bundled_in_process_scenarios_pass_under_every_declared_profile() {
    let selection = select_scenarios(&scenarios_directory()).expect("scenario files should load");
    // A scenario this runner cannot perform is skipped, never silently dropped: say which and why.
    // `cargo test -p excise --lib scenario_runner -- --nocapture` shows the lines.
    for (name, reason) in &selection.skipped {
        println!("SKIP {name}: {reason}");
    }
    // Lifecycle scenarios, and the two that keep a filter from ending the program, must run
    // in-process, under the user defaults and under reduced motion.
    for lifecycle in [
        "delete-file-lifecycle",
        "navigate-and-quit",
        "filter-inside-opened-folder",
        "filter-nested-matches-at-root",
    ] {
        let scenario = selection
            .runnable
            .iter()
            .find(|scenario| scenario.name == lifecycle)
            .unwrap_or_else(|| {
                panic!(
                    "`{lifecycle}` must run in-process; skipped: {:?}",
                    selection.skipped
                )
            });
        assert!(
            scenario.profiles.contains(&Profile::Default)
                && scenario.profiles.contains(&Profile::Deterministic),
            "`{lifecycle}` must declare the default and deterministic profiles"
        );
    }

    let mut failures = Vec::new();
    for scenario in &selection.runnable {
        for &profile in &scenario.profiles {
            let (_fixture, run) = execute(scenario, profile);
            if run.verdict().blocks_run() {
                failures.push(run.to_string());
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

// --- The delete step's guards ----------------------------------------------------------------

fn select_then_delete(selected: &str, name: &str, kind: &str) -> String {
    format!(
        r#"
[[steps]]
step = "wait_header"
state = "complete"

[[steps]]
step = "select"
name = "{selected}"

[[steps]]
step = "delete"
name = "{name}"
kind = "{kind}"
"#
    )
}

/// Runs a scenario whose `delete` step must be refused, and checks that nothing was confirmed and
/// nothing was deleted.
fn assert_delete_refused(header: &str, steps: &str, reason: &str) {
    let (fixture, run) = execute(&scenario(header, steps), Profile::Deterministic);
    let failure = failure_of(&run);
    assert_eq!(failure.step.index, 2, "{run}");
    assert!(failure.step.description.starts_with("delete "), "{run}");
    assert!(failure.message.contains(reason), "{run}");
    assert!(
        failure.screen.actual.contains("DELETE"),
        "the failure should carry the screen with the dialog: {run}"
    );
    assert!(
        !run.sent_keys
            .iter()
            .any(|key| key.code == KeyCode::Char('y')),
        "`y` must never be sent: {:?}",
        run.sent_keys
    );
    for path in DELETE_FILE_FILES {
        assert!(
            fixture.root().join(path).exists(),
            "`{path}` must be intact after a refused delete"
        );
    }
}

#[test]
fn a_delete_step_refuses_the_wrong_entry_and_never_confirms() {
    // The dialog names another entry than the step says.
    assert_delete_refused(
        DELETE_FILE,
        &select_then_delete("keep-a.bin", "victim.bin", "file"),
        "the dialog names \"keep-a.bin\"",
    );
}

#[test]
fn a_delete_step_refuses_the_wrong_kind_and_never_confirms() {
    assert_delete_refused(
        DELETE_FILE,
        &select_then_delete("victim.bin", "victim.bin", "folder"),
        "the dialog asks to delete a file, but the step expects a folder",
    );
    assert_delete_refused(
        DELETE_FILE,
        &select_then_delete("docs", "docs", "file"),
        "the dialog asks to delete a folder, but the step expects a file",
    );
}

#[test]
fn a_delete_step_refuses_a_missing_sentinel_and_never_confirms() {
    let header = DELETE_FILE.replace(
        "sentinels = [\"keep-a.bin\", \"docs/keep-0.txt\"]",
        "sentinels = [\"keep-a.bin\", \"never-created.bin\"]",
    );
    assert_delete_refused(
        &header,
        &select_then_delete("victim.bin", "victim.bin", "file"),
        "the sentinel `never-created.bin` does not exist; refusing to confirm",
    );
}

#[test]
fn a_sentinel_lost_outside_the_delete_step_fails_the_run_at_the_end() {
    // Raw keys delete the sentinel without the delete step's guards. The steps all pass, and the
    // sentinels are asserted again after the last one.
    let (fixture, run) = execute(
        &scenario(
            DELETE_FILE,
            r#"
[[steps]]
step = "wait_header"
state = "complete"

[[steps]]
step = "select"
name = "keep-a.bin"

[[steps]]
step = "key"
key = "backspace"

[[steps]]
step = "key"
key = "y"

[[steps]]
step = "wait_fs_absent"
path = "keep-a.bin"
"#,
        ),
        Profile::Deterministic,
    );
    let failure = failure_of(&run);
    assert_eq!(failure.step.index, 5, "{run}");
    assert_eq!(failure.step.description, "the final sentinel check");
    assert!(
        failure
            .message
            .contains("the sentinel `keep-a.bin` does not exist"),
        "{run}"
    );
    assert!(!fixture.root().join("keep-a.bin").exists());
}

// --- Bounded waits ----------------------------------------------------------------------------

fn never_holds(timeout_ms: u64) -> Scenario {
    scenario(
        DELETE_FILE,
        &format!(
            r#"{WAIT_FOR_COMPLETE}
[[steps]]
step = "wait_text"
text = "text that is nowhere on the screen"
timeout_ms = {timeout_ms}
"#
        ),
    )
}

#[test]
fn a_wait_that_never_holds_fails_at_its_time_bound() {
    let (_fixture, run) = execute(&never_holds(100), Profile::Deterministic);
    let failure = failure_of(&run);
    assert_eq!(failure.step.index, 1, "{run}");
    assert!(failure.message.starts_with("timed out after "), "{run}");
    assert!(failure.message.contains("timeout_ms = 100"), "{run}");
    assert!(
        failure.screen.actual.contains("EXCISE"),
        "the failure should carry the screen: {run}"
    );
    assert!(
        run.duration < Duration::from_secs(5),
        "the wait must end at its bound rather than hang: {:?}",
        run.duration
    );
}

#[test]
fn a_wait_that_never_holds_fails_at_its_round_limit() {
    let scenario = never_holds(600_000);
    let fixture = materialize(&scenario.fixture).expect("the fixture should build");
    let limits = Limits {
        poll_interval: Duration::ZERO,
        max_rounds: 3,
    };
    let run = run_scenario_with(&scenario, Profile::Deterministic, fixture.root(), limits)
        .expect("the scenario should run");
    let failure = failure_of(&run);
    assert_eq!(failure.step.index, 1, "{run}");
    assert!(
        failure.message.starts_with("gave up after 3 barriers"),
        "{run}"
    );
    assert!(
        run.duration < Duration::from_secs(5),
        "the wait must end at its round limit rather than hang: {:?}",
        run.duration
    );
}

#[test]
fn file_system_waits_are_bounded_too() {
    for (step, path, reason) in [
        ("wait_fs_present", "never-created.bin", "does not exist yet"),
        ("wait_fs_absent", "keep-a.bin", "still exists"),
    ] {
        let steps = format!(
            "{WAIT_FOR_COMPLETE}\n[[steps]]\nstep = \"{step}\"\npath = \"{path}\"\ntimeout_ms = 100\n"
        );
        let (_fixture, run) = execute(&scenario(DELETE_FILE, &steps), Profile::Deterministic);
        let failure = failure_of(&run);
        assert!(failure.message.starts_with("timed out after "), "{run}");
        assert!(failure.message.contains(reason), "{run}");
    }
}

#[test]
fn a_scan_is_only_seen_before_the_first_settle() {
    // The first frame is drawn before anything settles, so it can report the scan in progress. A
    // barrier drains the scan, and from then on the header can only report it complete.
    let (_fixture, run) = execute(
        &scenario(
            DELETE_FILE,
            r#"
[[steps]]
step = "wait_header"
state = "scanning"

[[steps]]
step = "wait_header"
state = "complete"
"#,
        ),
        Profile::Deterministic,
    );
    assert!(run.passed(), "{run}");

    let (_fixture, run) = execute(
        &scenario(
            DELETE_FILE,
            &format!(
                r#"{WAIT_FOR_COMPLETE}
[[steps]]
step = "wait_header"
state = "scanning"
timeout_ms = 100
"#
            ),
        ),
        Profile::Deterministic,
    );
    let failure = failure_of(&run);
    assert_eq!(failure.step.index, 1, "{run}");
    assert!(
        failure.message.ends_with("the header reports complete"),
        "{run}"
    );
}

// --- Live changes to the fixture ---------------------------------------------------------------

#[test]
fn a_live_mutation_reaches_the_fixture_between_two_steps() {
    // `victim.bin` is no sentinel here, so it may vanish.
    let scenario = scenario(
        DELETE_FILE,
        &format!(
            r#"{WAIT_FOR_COMPLETE}
[[steps]]
step = "fs_mutate"
op = "appear"
path = "docs/extra.bin"

[[steps]]
step = "wait_fs_present"
path = "docs/extra.bin"

[[steps]]
step = "fs_mutate"
op = "vanish"
path = "victim.bin"

[[steps]]
step = "wait_fs_absent"
path = "victim.bin"

[[steps]]
step = "expect_fs"
present = ["docs/extra.bin", "keep-a.bin"]
absent = ["victim.bin"]
"#
        ),
    );
    let (fixture, run) = execute(&scenario, Profile::Deterministic);
    assert!(run.passed(), "{run}");
    let appeared = fs::metadata(fixture.root().join("docs/extra.bin"))
        .expect("the file that appeared should exist");
    assert_eq!(appeared.len(), mutate::APPEAR_BYTES);
    assert!(!fixture.root().join("victim.bin").exists());
}

#[test]
fn a_mutation_the_harness_refuses_fails_its_step_and_names_the_reason() {
    let scenario = scenario(
        DELETE_FILE,
        &format!(
            r#"{WAIT_FOR_COMPLETE}
[[steps]]
step = "fs_mutate"
op = "vanish"
path = "never-created.bin"
"#
        ),
    );
    let (fixture, run) = execute(&scenario, Profile::Deterministic);
    let failure = failure_of(&run);
    assert_eq!(failure.step.index, 1, "{run}");
    assert!(
        failure.step.description.contains("fs_mutate vanish"),
        "{run}"
    );
    assert!(
        failure
            .message
            .contains("`never-created.bin` does not exist"),
        "{run}"
    );
    for file in DELETE_FILE_FILES {
        assert!(fixture.root().join(file).exists(), "{file} must survive");
    }
}

// --- Refusing to run ---------------------------------------------------------------------------

#[test]
fn steps_only_another_runner_can_perform_are_rejected_before_anything_runs() {
    // The first steps would delete `victim.bin` if the scenario ran at all.
    let scenario = scenario(
        DELETE_FILE,
        &format!(
            r#"{}
[[steps]]
step = "signal"
signal = "term"

[[steps]]
step = "wait_event"
event = "scan_complete"

[[steps]]
step = "measure"
name = "scan"
marker = "start"

[[steps]]
step = "measure"
name = "scan"
marker = "stop"

[[steps]]
step = "expect_budget"
budget = "first_frame_ms"
metric = "scan"

[[steps]]
step = "type"
text = "a\u0007b"

[[steps]]
step = "expect_exit"
code = 0
terminal_restored = false
residue = "none"
"#,
            select_then_delete("victim.bin", "victim.bin", "file")
        ),
    );
    let expected = [
        (3, "signal"),
        (4, "wait_event"),
        (5, "measure"),
        (6, "measure"),
        (7, "expect_budget"),
        (8, "type"),
        (9, "expect_exit"),
    ];
    let found = |steps: &[UnsupportedStep]| {
        steps
            .iter()
            .map(|step| (step.index, step.kind))
            .collect::<Vec<_>>()
    };
    assert_eq!(found(&unsupported_steps(&scenario)), expected);

    let fixture = materialize(&scenario.fixture).expect("the fixture should build");
    let error = run_scenario(&scenario, Profile::Deterministic, fixture.root())
        .expect_err("the scenario must be rejected");
    let RunError::Unsupported { steps } = &error else {
        panic!("expected an unsupported-steps error, got: {error}");
    };
    assert_eq!(found(steps), expected);
    assert!(error.to_string().contains("step 3 (`signal`)"), "{error}");
    assert!(
        fixture.root().join("victim.bin").exists(),
        "nothing may run before the scenario is rejected"
    );
}

#[test]
fn nothing_may_follow_expect_exit_in_process() {
    let scenario = scenario(
        DELETE_FILE,
        r#"
[[steps]]
step = "quit"

[[steps]]
step = "expect_exit"
code = 0
terminal_restored = true
residue = "none"

[[steps]]
step = "settle"
"#,
    );
    let steps = unsupported_steps(&scenario);
    assert_eq!(steps.len(), 1);
    assert_eq!((steps[0].index, steps[0].kind), (1, "expect_exit"));
}

#[test]
fn an_invalid_regular_expression_is_rejected_before_the_run() {
    let scenario = scenario(
        DELETE_FILE,
        &format!(
            r#"{WAIT_FOR_COMPLETE}
[[steps]]
step = "expect_screen"
regex = ["ok", "(unclosed"]
"#
        ),
    );
    let fixture = materialize(&scenario.fixture).expect("the fixture should build");
    let error = run_scenario(&scenario, Profile::Deterministic, fixture.root())
        .expect_err("the scenario must be rejected");
    match error {
        RunError::InvalidRegex { step, pattern, .. } => {
            assert_eq!((step, pattern.as_str()), (1, "(unclosed"));
        }
        other => panic!("expected an invalid-regex error, got: {other}"),
    }
}

fn refused(root: &Path) -> String {
    let scenario = scenario(DELETE_FILE, WAIT_FOR_COMPLETE);
    match run_scenario(&scenario, Profile::Deterministic, root) {
        Err(error @ RunError::UnownedRoot(_)) => error.to_string(),
        Err(other) => panic!("expected the root to be refused, got: {other}"),
        Ok(run) => panic!("a root the harness does not own must be refused:\n{run}"),
    }
}

#[test]
fn a_root_without_the_ownership_marker_is_refused() {
    let root = tempfile::tempdir().expect("the root should exist");
    fs::write(root.path().join("victim.bin"), b"payload").expect("the file should be written");
    let message = refused(root.path());
    assert!(message.contains(MARKER_FILE_NAME), "{message}");
    assert!(root.path().join("victim.bin").exists());

    let missing = root.path().join("does-not-exist");
    let message = refused(&missing);
    assert!(message.contains("does-not-exist"), "{message}");
}

// --- Verdicts ----------------------------------------------------------------------------------

#[test]
fn strict_xfail_reports_a_documented_failure_and_fails_a_fix_that_did_not_flip_it() {
    let header = format!("{DELETE_FILE}expect = \"fail\"\nslice = \"X9\"\n");

    let (_fixture, run) = execute(&never_holds_under(&header, 50), Profile::Deterministic);
    assert_eq!(run.verdict(), Verdict::Xfail, "{run}");
    assert!(!run.verdict().blocks_run());

    let (_fixture, run) = execute(
        &scenario(&header, WAIT_FOR_COMPLETE),
        Profile::Deterministic,
    );
    assert!(run.passed(), "{run}");
    assert_eq!(run.verdict(), Verdict::Xpass, "{run}");
    assert!(run.verdict().blocks_run());

    let result = run.result();
    assert_eq!(result.verdict, Verdict::Xpass);
    assert_eq!(result.profile, Profile::Deterministic);
    assert_eq!(result.name, "scenario-under-test");
    assert!(result.failure_bundle.is_none());
}

fn never_holds_under(header: &str, timeout_ms: u64) -> Scenario {
    let steps = format!(
        "{WAIT_FOR_COMPLETE}\n[[steps]]\nstep = \"wait_text\"\ntext = \"never on screen\"\ntimeout_ms = {timeout_ms}\n"
    );
    scenario(header, &steps)
}

#[test]
fn an_expected_failure_applies_only_on_its_platform() {
    let host = std::env::consts::OS;
    let other = if host == "linux" { "macos" } else { "linux" };

    let header =
        format!("{DELETE_FILE}expect = \"fail\"\nslice = \"H5\"\nfails_on = [\"{host}\"]\n");
    let (_fixture, run) = execute(&never_holds_under(&header, 50), Profile::Deterministic);
    assert_eq!(
        run.verdict(),
        Verdict::Xfail,
        "fails_on names the host: {run}"
    );

    let header =
        format!("{DELETE_FILE}expect = \"fail\"\nslice = \"H5\"\nfails_on = [\"{other}\"]\n");
    let (_fixture, run) = execute(&never_holds_under(&header, 50), Profile::Deterministic);
    assert_eq!(
        run.verdict(),
        Verdict::Fail,
        "fails_on names only another platform: {run}"
    );
}

#[test]
fn expect_exit_judges_the_exit_code() {
    let (_fixture, run) = execute(
        &scenario(
            DELETE_FILE,
            &format!(
                r#"{WAIT_FOR_COMPLETE}
[[steps]]
step = "quit"

[[steps]]
step = "expect_exit"
code = 130
terminal_restored = true
residue = "none"
"#
            ),
        ),
        Profile::Deterministic,
    );
    let failure = failure_of(&run);
    assert_eq!(failure.step.index, 2, "{run}");
    assert_eq!(failure.step.description, "expect_exit code 130");
    assert_eq!(failure.message, "the program exited with code 0, not 130");
}

#[test]
fn a_program_that_is_still_running_did_not_exit() {
    let (_fixture, run) = execute(
        &scenario(
            DELETE_FILE,
            &format!(
                r#"{WAIT_FOR_COMPLETE}
[[steps]]
step = "expect_exit"
code = 0
terminal_restored = true
residue = "none"
timeout_ms = 100
"#
            ),
        ),
        Profile::Deterministic,
    );
    let failure = failure_of(&run);
    assert_eq!(failure.step.index, 1, "{run}");
    assert!(failure.message.starts_with("timed out after "), "{run}");
    assert!(
        failure.message.ends_with("the program is still running"),
        "{run}"
    );
}

// --- Profiles, resizing, dialogs, and typing ---------------------------------------------------

/// Runs the scan to completion under `profile` and asserts `expect_screen`'s lists over `region`.
fn assert_profile_screen(profile: Profile, region: &str, contains: &[&str], not_contains: &[&str]) {
    let quoted = |texts: &[&str]| {
        texts
            .iter()
            .map(|text| format!("{text:?}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let steps = format!(
        "{WAIT_FOR_COMPLETE}\n[[steps]]\nstep = \"expect_screen\"\ncontains = [{}]\nnot_contains = [{}]\n{region}\n",
        quoted(contains),
        quoted(not_contains)
    );
    let (_fixture, run) = execute(&scenario(DELETE_FILE, &steps), profile);
    assert!(run.passed(), "{run}");
}

#[test]
fn each_profile_configures_the_program_it_runs() {
    let header = "region = \"header\"";
    let none: &[&str] = &[];
    // The user defaults carry none of the other profiles' flags.
    assert_profile_screen(
        Profile::Default,
        header,
        &["COMPLETE"],
        &["REDUCED MOTION", "MOUSE", "ASCII"],
    );
    assert_profile_screen(Profile::Deterministic, header, &["REDUCED MOTION"], none);
    assert_profile_screen(Profile::MouseKeymaps, header, &["MOUSE"], none);
    // ASCII replaces the state marker too: `C COMPLETE`, which the wait above already read.
    assert_profile_screen(
        Profile::MonochromeAscii,
        header,
        &["ASCII", "C COMPLETE"],
        none,
    );
    // Sixty columns is too narrow for the storage map, so the workspace is a list.
    assert_profile_screen(Profile::Narrow, "", &["LIST"], &["STORAGE MAP"]);
}

#[test]
fn a_resize_reaches_the_program_and_the_map_lays_out_again() {
    let (_fixture, run) = execute(
        &scenario(
            DELETE_FILE,
            &format!(
                r#"{WAIT_FOR_COMPLETE}
[[steps]]
step = "expect_screen"
contains = ["STORAGE MAP"]
not_contains = ["LIST"]

[[steps]]
step = "resize"
cols = 60
rows = 40

[[steps]]
step = "wait_text"
text = "LIST"

[[steps]]
step = "resize"
cols = 120
rows = 40

[[steps]]
step = "wait_text"
text = "STORAGE MAP"
"#
            ),
        ),
        Profile::Deterministic,
    );
    assert!(run.passed(), "{run}");
}

#[test]
fn a_dialog_region_holds_the_dialog_and_a_cancelled_deletion_leaves_the_file() {
    let (fixture, run) = execute(
        &scenario(
            DELETE_FILE,
            &format!(
                r#"{WAIT_FOR_COMPLETE}
[[steps]]
step = "select"
name = "victim.bin"

[[steps]]
step = "key"
key = "backspace"

[[steps]]
step = "wait_text"
text = "! DELETE FILE"
region = "dialog"

[[steps]]
step = "expect_screen"
contains = ["/victim.bin", "[Enter/y] start"]
not_contains = ["STORAGE MAP", "SELECTED ITEM"]
region = "dialog"

[[steps]]
step = "expect_screen"
contains = ["STORAGE MAP", "SELECTED ITEM"]

[[steps]]
step = "key"
key = "esc"

[[steps]]
step = "settle"

[[steps]]
step = "expect_screen"
not_contains = ["DELETE FILE"]

[[steps]]
step = "expect_screen"
not_contains = ["Quit Excise?"]
region = "dialog"

[[steps]]
step = "expect_fs"
present = ["victim.bin"]
"#
            ),
        ),
        Profile::Deterministic,
    );
    assert!(run.passed(), "{run}");
    assert!(fixture.root().join("victim.bin").exists());
}

#[test]
fn an_export_key_is_refused_outside_a_text_prompt() {
    // Exports go to the process working directory, which an in-process run cannot isolate.
    let (_fixture, run) = execute(
        &scenario(
            DELETE_FILE,
            &format!(
                r#"{WAIT_FOR_COMPLETE}
[[steps]]
step = "key"
key = "e"
"#
            ),
        ),
        Profile::Deterministic,
    );
    let failure = failure_of(&run);
    assert_eq!(failure.step.index, 1, "{run}");
    assert!(failure.message.contains("refusing to press `e`"), "{run}");
    assert!(run.sent_keys.is_empty(), "{:?}", run.sent_keys);
}

#[test]
fn typed_text_reaches_an_open_filter_even_when_it_holds_an_export_key() {
    let (_fixture, run) = execute(
        &scenario(
            DELETE_FILE,
            &format!(
                r#"{WAIT_FOR_COMPLETE}
[[steps]]
step = "key"
key = "/"

[[steps]]
step = "type"
text = "keep-a.bin"

[[steps]]
step = "key"
key = "enter"

[[steps]]
step = "expect_screen"
contains = ["keep-a.bin"]
not_contains = ["victim.bin"]
"#
            ),
        ),
        Profile::Deterministic,
    );
    assert!(run.passed(), "{run}");
    let typed = run
        .sent_keys
        .iter()
        .filter(|key| key.code == KeyCode::Char('e'))
        .count();
    assert_eq!(typed, 2, "both `e` of `keep-a.bin` should be typed");
}

// --- Selecting the scenarios that run ----------------------------------------------------------

fn write_scenario(directory: &Path, file: &str, name: &str, fixture: &str, step: &str) {
    let source = format!(
        r#"schema_version = 1
name = "{name}"
description = "A scenario file that a test wrote."
fixture = "{fixture}"
profiles = ["default"]

[[steps]]
{step}
"#
    );
    fs::write(directory.join(file), source).expect("the scenario file should be written");
}

#[test]
fn scenarios_this_runner_cannot_run_are_skipped_with_the_reason() {
    let directory = tempfile::tempdir().expect("the scenario directory should exist");
    let settle = "step = \"settle\"";
    write_scenario(
        directory.path(),
        "runs-here.toml",
        "runs-here",
        "delete-file",
        settle,
    );
    write_scenario(
        directory.path(),
        "needs-a-process.toml",
        "needs-a-process",
        "delete-file",
        "step = \"signal\"\nsignal = \"term\"",
    );
    write_scenario(
        directory.path(),
        "too-big.toml",
        "too-big",
        "tiny-files-1m",
        settle,
    );
    write_scenario(
        directory.path(),
        "needs-a-volume.toml",
        "needs-a-volume",
        "mount-boundary",
        settle,
    );
    fs::write(directory.path().join("notes.txt"), "not a scenario").expect("notes should exist");

    let selection = select_scenarios(directory.path()).expect("the files should load");
    let runnable = selection
        .runnable
        .iter()
        .map(|scenario| scenario.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(runnable, ["runs-here"]);
    assert_eq!(selection.skipped.len(), 3, "{:?}", selection.skipped);
    let reason = |name: &str| {
        selection
            .skipped
            .iter()
            .find(|(skipped, _)| skipped == name)
            .map_or_else(
                || panic!("`{name}` should be skipped"),
                |(_, reason)| reason.as_str(),
            )
    };
    assert!(reason("needs-a-process").contains("signal"));
    let too_big = reason("too-big");
    assert!(
        too_big.contains("`tiny-files-1m`") && too_big.contains("entries"),
        "{too_big}"
    );
    assert!(reason("needs-a-volume").contains("scratch volume"));
}

#[test]
fn a_scenario_outside_its_platforms_is_skipped_with_the_reason() {
    let directory = tempfile::tempdir().expect("the scenario directory should exist");
    let host = std::env::consts::OS;
    let other = if host == "linux" { "macos" } else { "linux" };
    fs::write(
        directory.path().join("elsewhere.toml"),
        format!(
            "schema_version = 1\nname = \"elsewhere\"\ndescription = \"d\"\n\
             fixture = \"delete-file\"\nprofiles = [\"default\"]\nplatforms = [\"{other}\"]\n\n\
             [[steps]]\nstep = \"settle\"\n"
        ),
    )
    .expect("the scenario file should be written");

    let selection = select_scenarios(directory.path()).expect("the file should load");
    assert!(selection.runnable.is_empty(), "{:?}", selection.runnable);
    assert_eq!(selection.skipped.len(), 1, "{:?}", selection.skipped);
    assert_eq!(selection.skipped[0].0, "elsewhere");
    assert!(
        selection.skipped[0].1.contains(host),
        "{}",
        selection.skipped[0].1
    );
}

#[test]
fn a_scenario_above_the_quick_tier_is_skipped_with_the_reason() {
    let directory = tempfile::tempdir().expect("the scenario directory should exist");
    fs::write(
        directory.path().join("nightly-only.toml"),
        "schema_version = 1\nname = \"nightly-only\"\ndescription = \"d\"\n\
         fixture = \"delete-file\"\nprofiles = [\"default\"]\ntier = \"nightly\"\n\n\
         [[steps]]\nstep = \"settle\"\n",
    )
    .expect("the scenario file should be written");

    let selection = select_scenarios(directory.path()).expect("the file should load");
    assert!(selection.runnable.is_empty(), "{:?}", selection.runnable);
    assert_eq!(selection.skipped.len(), 1, "{:?}", selection.skipped);
    assert_eq!(selection.skipped[0].0, "nightly-only");
    assert!(
        selection.skipped[0].1.contains("nightly"),
        "{}",
        selection.skipped[0].1
    );
}

#[test]
fn a_scenario_file_that_would_break_every_runner_is_an_error_not_a_skip() {
    let directory = tempfile::tempdir().expect("the scenario directory should exist");
    let settle = "step = \"settle\"";

    write_scenario(
        directory.path(),
        "wrong-name.toml",
        "right-name",
        "delete-file",
        settle,
    );
    let error = select_scenarios(directory.path())
        .err()
        .expect("a misnamed file must be an error");
    assert!(
        error.contains("wrong-name.toml") && error.contains("right-name"),
        "{error}"
    );
    fs::remove_file(directory.path().join("wrong-name.toml")).expect("the file should go");

    fs::write(directory.path().join("broken.toml"), "schema_version = ").expect("file");
    let error = select_scenarios(directory.path())
        .err()
        .expect("an unparsable file must be an error");
    assert!(error.contains("broken.toml"), "{error}");
    fs::remove_file(directory.path().join("broken.toml")).expect("the file should go");

    // A scenario the format itself rejects: a delete step with no sentinel.
    write_scenario(
        directory.path(),
        "unguarded.toml",
        "unguarded",
        "delete-file",
        "step = \"delete\"\nname = \"victim.bin\"\nkind = \"file\"",
    );
    let error = select_scenarios(directory.path())
        .err()
        .expect("an invalid scenario must be an error");
    assert!(error.contains("unguarded.toml"), "{error}");
    fs::remove_file(directory.path().join("unguarded.toml")).expect("the file should go");

    // A fixture no spec describes: no runner could materialize it.
    write_scenario(
        directory.path(),
        "no-fixture.toml",
        "no-fixture",
        "no-such-fixture",
        settle,
    );
    let error = select_scenarios(directory.path())
        .err()
        .expect("an unknown fixture must be an error");
    assert!(
        error.contains("no-fixture.toml") && error.contains("no-such-fixture"),
        "{error}"
    );
}
