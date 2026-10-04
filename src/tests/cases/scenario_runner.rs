//! Tests of the in-process scenario runner: what it runs, what it refuses, and how it fails.

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

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
    // Lifecycle scenarios, the two that keep a filter from ending the program, and the ports of the
    // old deletion tests must run in-process, under the user defaults and under reduced motion.
    // The two that need a folder the file system refuses to change run wherever it can express one,
    // and the 60-column dialog only where its `platforms` let it run (its scenario file says why).
    let unix_only: &[&str] = if cfg!(unix) {
        &[
            "delete-refused-by-permission",
            "delete-refused-reduced-confirmation",
            "delete-tree-narrow-terminal",
        ]
    } else {
        &[]
    };
    for lifecycle in [
        "delete-file-lifecycle",
        "delete-file-cancelled",
        "delete-file-terminal-too-small",
        "delete-file-reduced-confirmation",
        "delete-tree-confirmed-with-enter",
        "delete-tree-reduced-confirmation",
        "delete-tree-narrow-terminal-reduced-confirmation",
        "exit-prompt-keeps-pending-deletion",
        "theme-commit-saves",
        "navigate-and-quit",
        "filter-inside-opened-folder",
        "filter-nested-matches-at-root",
    ]
    .iter()
    .chain(unix_only)
    .copied()
    {
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

// --- The delete step with no dialog, the confirmation key, and the saved configuration -------------

/// The top of a scenario over the `delete-file` fixture that disables the confirmation dialog, as
/// `--disable-delete-confirmation` does.
fn without_dialog() -> String {
    format!("{DELETE_FILE}disable_delete_confirmation = true\n")
}

/// The keys the runner sent, in order.
fn sent_codes(run: &ScenarioRun) -> Vec<KeyCode> {
    run.sent_keys.iter().map(|key| key.code).collect()
}

/// The last `count` keys the runner sent.
fn last_sent(run: &ScenarioRun, count: usize) -> Vec<KeyCode> {
    let mut codes: Vec<KeyCode> = sent_codes(run).into_iter().rev().take(count).collect();
    codes.reverse();
    codes
}

#[test]
fn a_deletion_with_no_dialog_presses_backspace_and_nothing_confirms_it() {
    let (fixture, run) = execute(
        &scenario(
            &without_dialog(),
            &select_then_delete("victim.bin", "victim.bin", "file"),
        ),
        Profile::Deterministic,
    );
    assert!(run.passed(), "{run}");
    assert!(!fixture.root().join("victim.bin").exists());
    for path in ["keep-a.bin", "docs/keep-0.txt", "docs/keep-1.txt"] {
        assert!(fixture.root().join(path).exists(), "`{path}` must survive");
    }
    assert_eq!(
        last_sent(&run, 2)[1],
        KeyCode::Backspace,
        "{:?}",
        run.sent_keys
    );
    assert!(
        !sent_codes(&run).contains(&KeyCode::Char('y')),
        "no key confirms a deletion that has no dialog: {:?}",
        run.sent_keys
    );
}

/// Runs a scenario with no dialog whose `delete` step must be refused, and checks that Backspace
/// was never pressed and nothing was deleted.
fn assert_delete_refused_without_dialog(header: &str, steps: &str, reason: &str) {
    let (fixture, run) = execute(&scenario(header, steps), Profile::Deterministic);
    let failure = failure_of(&run);
    assert_eq!(failure.step.index, 2, "{run}");
    assert!(failure.step.description.starts_with("delete "), "{run}");
    assert!(failure.message.contains(reason), "{run}");
    assert!(
        !sent_codes(&run).contains(&KeyCode::Backspace),
        "Backspace must never be pressed: {:?}",
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
fn a_deletion_with_no_dialog_refuses_a_wrong_selection_before_it_presses_anything() {
    // The panel shows another entry than the step says.
    assert_delete_refused_without_dialog(
        &without_dialog(),
        &select_then_delete("keep-a.bin", "victim.bin", "file"),
        "the selected-item panel shows \"keep-a.bin\", but the step deletes \"victim.bin\"",
    );
    // The panel shows the right name and another kind.
    assert_delete_refused_without_dialog(
        &without_dialog(),
        &select_then_delete("victim.bin", "victim.bin", "folder"),
        "the selected-item panel shows a file named \"victim.bin\", but the step deletes a folder",
    );
}

#[test]
fn a_deletion_with_no_dialog_checks_the_path_the_step_gives_on_disk() {
    // Nothing on screen says where the selected entry is, so the step's `path` is the only claim:
    // `victim.bin` is selected, and the step says it is in `docs`, where there is no such file.
    assert_delete_refused_without_dialog(
        &without_dialog(),
        &format!(
            "{}path = \"docs/victim.bin\"\n",
            select_then_delete("victim.bin", "victim.bin", "file")
        ),
        "`docs/victim.bin` does not exist in the fixture",
    );
}

/// The steps that open `docs/` and select `keep-1.txt` there, which the `delete-file` fixture
/// keeps beside `keep-0.txt`, and then delete it. The `delete` step is the sixth.
fn select_in_docs_then_delete(delete_fields: &str) -> String {
    format!(
        r#"
[[steps]]
step = "wait_header"
state = "complete"

[[steps]]
step = "select"
name = "docs"

[[steps]]
step = "key"
key = "enter"

[[steps]]
step = "wait_header"
state = "complete"

[[steps]]
step = "select"
name = "keep-1.txt"

[[steps]]
step = "delete"
name = "keep-1.txt"
kind = "file"
{delete_fields}"#
    )
}

#[test]
fn a_delete_step_without_a_path_means_an_entry_directly_below_the_root() {
    // The dialog names `docs/keep-1.txt`: a file of that name under the fixture root, but not the
    // `keep-1.txt` at the root that a step without a `path` means.
    let (fixture, run) = execute(
        &scenario(DELETE_FILE, &select_in_docs_then_delete("")),
        Profile::Deterministic,
    );
    let failure = failure_of(&run);
    assert_eq!(failure.step.index, 5, "{run}");
    assert!(failure.step.description.starts_with("delete "), "{run}");
    assert!(
        failure
            .message
            .contains("the dialog deletes \"docs/keep-1.txt\"")
            && failure
                .message
                .contains("but the step expects \"keep-1.txt\""),
        "{run}"
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

    // The same dialog is confirmed once the step says where the entry is.
    let (fixture, run) = execute(
        &scenario(
            DELETE_FILE,
            &select_in_docs_then_delete("path = \"docs/keep-1.txt\"\n"),
        ),
        Profile::Deterministic,
    );
    assert!(run.passed(), "{run}");
    assert!(!fixture.root().join("docs/keep-1.txt").exists());
    assert!(fixture.root().join("docs/keep-0.txt").exists());
}

/// The top of a scenario over the `twins` fixture, which has a file called `twin.bin` at its root
/// and another in `docs/`, with no dialog to read.
const TWINS_WITHOUT_DIALOG: &str = r#"
schema_version = 1
name = "scenario-under-test"
description = "A scenario that a test built."
fixture = "twins"
sentinels = ["keep-a.bin"]
disable_delete_confirmation = true
profiles = ["default", "deterministic"]
"#;

#[test]
fn a_deletion_with_no_dialog_refuses_a_name_and_kind_that_fit_two_entries() {
    // The fresh map selects the `twin.bin` at the root, and the panel shows a file called
    // `twin.bin`, which is also what the other one would show: with no dialog, Backspace would
    // delete whichever is selected, whatever `path` says.
    let steps = r#"
[[steps]]
step = "wait_header"
state = "complete"

[[steps]]
step = "delete"
name = "twin.bin"
kind = "file"
"#;
    let (fixture, run) = execute(
        &scenario(TWINS_WITHOUT_DIALOG, steps),
        Profile::Deterministic,
    );
    let failure = failure_of(&run);
    assert_eq!(failure.step.index, 1, "{run}");
    assert!(failure.step.description.starts_with("delete "), "{run}");
    assert!(
        failure
            .message
            .contains("the fixture has 2 files called `twin.bin` (`docs/twin.bin`, `twin.bin`)"),
        "{run}"
    );
    assert!(
        !sent_codes(&run).contains(&KeyCode::Backspace),
        "Backspace must never be pressed: {:?}",
        run.sent_keys
    );
    for path in ["twin.bin", "docs/twin.bin", "keep-a.bin"] {
        assert!(
            fixture.root().join(path).exists(),
            "`{path}` must be intact after a refused delete"
        );
    }
}

#[test]
fn a_deletion_with_no_dialog_keeps_every_sentinel() {
    let header = DELETE_FILE.replace(
        "sentinels = [\"keep-a.bin\", \"docs/keep-0.txt\"]",
        "sentinels = [\"keep-a.bin\", \"never-created.bin\"]",
    );
    assert_delete_refused_without_dialog(
        &format!("{header}disable_delete_confirmation = true\n"),
        &select_then_delete("victim.bin", "victim.bin", "file"),
        "the sentinel `never-created.bin` does not exist; refusing to delete",
    );
    // A folder that holds a sentinel is not deleted either.
    assert_delete_refused_without_dialog(
        &without_dialog(),
        &select_then_delete("docs", "docs", "folder"),
        "the target `docs` contains the sentinel `docs/keep-0.txt`, which must survive",
    );
}

#[test]
fn a_deletion_with_no_dialog_fails_when_another_dialog_opens_instead_of_it_starting() {
    // 49 columns are too few for a deletion: Backspace opens an error dialog and starts nothing.
    // The filter prompt does not fit either, so the step acts on the entry the map armed, the
    // largest, instead of one that `select` chose.
    let header = format!("{}[terminal]\ncols = 49\nrows = 50\n", without_dialog());
    let steps = format!(
        r#"{WAIT_FOR_COMPLETE}
[[steps]]
step = "delete"
name = "victim.bin"
kind = "file"
"#
    );
    let (fixture, run) = execute(&scenario(&header, &steps), Profile::Deterministic);
    let failure = failure_of(&run);
    assert_eq!(failure.step.index, 1, "{run}");
    assert!(
        failure
            .message
            .contains("the dialog `ERROR` opened instead of a deletion starting"),
        "{run}"
    );
    assert!(fixture.root().join("victim.bin").exists());
}

#[test]
fn the_reduced_confirmation_mode_reaches_the_program_only_when_the_scenario_asks_for_it() {
    let steps = format!(
        r#"{WAIT_FOR_COMPLETE}
[[steps]]
step = "expect_screen"
contains = ["REDUCED DELETE GUARD"]
region = "header"
"#
    );
    let (_fixture, run) = execute(&scenario(&without_dialog(), &steps), Profile::Deterministic);
    assert!(run.passed(), "{run}");

    let (_fixture, run) = execute(&scenario(DELETE_FILE, &steps), Profile::Deterministic);
    let failure = failure_of(&run);
    assert_eq!(failure.step.index, 1, "{run}");
}

#[test]
fn a_delete_step_confirms_with_the_key_it_names_and_no_other() {
    for (confirm_with, key) in [("y", KeyCode::Char('y')), ("enter", KeyCode::Enter)] {
        let steps = format!(
            r#"{WAIT_FOR_COMPLETE}
[[steps]]
step = "select"
name = "victim.bin"

[[steps]]
step = "delete"
name = "victim.bin"
kind = "file"
confirm_with = "{confirm_with}"
"#
        );
        let (fixture, run) = execute(&scenario(DELETE_FILE, &steps), Profile::Deterministic);
        assert!(run.passed(), "{confirm_with}: {run}");
        assert!(
            !fixture.root().join("victim.bin").exists(),
            "{confirm_with}"
        );
        // Backspace opened the dialog, and the named key confirmed it.
        assert_eq!(
            last_sent(&run, 2),
            [KeyCode::Backspace, key],
            "{confirm_with}: {:?}",
            run.sent_keys
        );
    }
}

// --- A root that loses its ownership marker while the run goes on ---------------------------

/// Takes the ownership marker away, as something other than the harness could. The harness's own
/// mutators refuse to touch it, so a test does it directly.
fn remove_marker(marker: &Path) {
    fs::remove_file(marker).expect("the marker should be removable");
}

/// Puts a directory where the marker was: something is still at that name, but no regular file.
fn replace_marker_with_a_directory(marker: &Path) {
    remove_marker(marker);
    fs::create_dir(marker).expect("a directory should take the marker's place");
}

/// Leaves the marker alone: the control that shows what the scenarios below do when nothing is
/// wrong.
fn keep_marker(_marker: &Path) {}

/// Runs `scenario` on a fresh fixture and has `tamper` change the ownership marker while the run
/// goes on: after the run started and before the scenario's `delete` step.
///
/// The scenario makes `go.bin` appear with `fs_mutate` and then waits for `done.flag`. A thread of
/// the test sees `go.bin`, calls `tamper` with the marker's path, and only then writes
/// `done.flag`, so every step after the wait finds the marker as `tamper` left it.
fn execute_tampering_with_the_marker(
    scenario: &Scenario,
    tamper: fn(&Path),
) -> (Fixture, ScenarioRun) {
    let fixture = materialize(&scenario.fixture).expect("the fixture should build");
    let root = fixture.root().to_path_buf();
    let finished = AtomicBool::new(false);
    let run = thread::scope(|scope| {
        scope.spawn(|| {
            let deadline = Instant::now() + Duration::from_secs(30);
            while !root.join("go.bin").exists() {
                if finished.load(Ordering::Relaxed) || Instant::now() > deadline {
                    return;
                }
                thread::sleep(Duration::from_millis(1));
            }
            tamper(&root.join(MARKER_FILE_NAME));
            fs::write(root.join("done.flag"), b"").expect("the flag should be written");
        });
        let outcome = run_scenario(scenario, Profile::Deterministic, &root);
        finished.store(true, Ordering::Relaxed);
        outcome.expect("the scenario should run")
    });
    (fixture, run)
}

/// A scenario that makes `go.bin` appear, waits for `done.flag` (see
/// `execute_tampering_with_the_marker`), selects `victim.bin`, and deletes it, with
/// `delete_fields` added to the `delete` step, which is the fifth.
fn delete_after_tampering(header: &str, delete_fields: &str) -> Scenario {
    scenario(
        header,
        &format!(
            r#"{WAIT_FOR_COMPLETE}
[[steps]]
step = "fs_mutate"
op = "appear"
path = "go.bin"

[[steps]]
step = "wait_fs_present"
path = "done.flag"

[[steps]]
step = "select"
name = "victim.bin"

[[steps]]
step = "delete"
name = "victim.bin"
kind = "file"
{delete_fields}"#
        ),
    )
}

/// Checks that a `delete` step whose dialog is up refuses to confirm once `tamper` has changed the
/// ownership marker, whichever key it confirms with, and that nothing is gone from the fixture.
fn assert_confirmation_refused_after(tamper: fn(&Path), confirm_with: &str) {
    let fields = format!("confirm_with = \"{confirm_with}\"\n");
    let scenario = delete_after_tampering(DELETE_FILE, &fields);
    let (fixture, run) = execute_tampering_with_the_marker(&scenario, tamper);
    let failure = failure_of(&run);
    assert_eq!(failure.step.index, 4, "{confirm_with}: {run}");
    assert!(
        failure.step.description.starts_with("delete "),
        "{confirm_with}: {run}"
    );
    assert!(
        failure.message.contains(MARKER_FILE_NAME)
            && failure.message.contains("no confirming key was sent"),
        "{confirm_with}: {run}"
    );
    // Backspace opened the dialog, which the failure still shows, and no key followed it.
    assert!(
        failure.screen.actual.contains("DELETE"),
        "{confirm_with}: {run}"
    );
    assert_eq!(
        last_sent(&run, 1),
        [KeyCode::Backspace],
        "{confirm_with}: {:?}",
        run.sent_keys
    );
    assert!(
        !sent_codes(&run).contains(&KeyCode::Char('y')),
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
fn a_delete_step_never_confirms_in_a_root_that_lost_its_marker_after_the_run_started() {
    for confirm_with in ["y", "enter"] {
        assert_confirmation_refused_after(remove_marker, confirm_with);
    }
    assert_confirmation_refused_after(replace_marker_with_a_directory, "y");
}

#[test]
fn the_same_scenario_deletes_when_nothing_touches_the_marker() {
    let scenario = delete_after_tampering(DELETE_FILE, "");
    let (fixture, run) = execute_tampering_with_the_marker(&scenario, keep_marker);
    assert!(run.passed(), "{run}");
    assert!(!fixture.root().join("victim.bin").exists());
    for path in ["keep-a.bin", "docs/keep-0.txt", "docs/keep-1.txt"] {
        assert!(fixture.root().join(path).exists(), "`{path}` must survive");
    }
    assert_eq!(
        last_sent(&run, 2),
        [KeyCode::Backspace, KeyCode::Char('y')],
        "{:?}",
        run.sent_keys
    );
}

#[test]
fn a_deletion_with_no_dialog_never_presses_backspace_in_a_root_that_lost_its_marker() {
    let tampers: [fn(&Path); 2] = [remove_marker, replace_marker_with_a_directory];
    for tamper in tampers {
        let scenario = delete_after_tampering(&without_dialog(), "");
        let (fixture, run) = execute_tampering_with_the_marker(&scenario, tamper);
        let failure = failure_of(&run);
        assert_eq!(failure.step.index, 4, "{run}");
        assert!(failure.step.description.starts_with("delete "), "{run}");
        assert!(failure.message.contains(MARKER_FILE_NAME), "{run}");
        assert!(
            !sent_codes(&run).contains(&KeyCode::Backspace),
            "Backspace must never be pressed: {:?}",
            run.sent_keys
        );
        for path in DELETE_FILE_FILES {
            assert!(
                fixture.root().join(path).exists(),
                "`{path}` must be intact after a refused delete"
            );
        }
    }
}

/// Opens the theme picker, moves to the next theme, and saves it.
const COMMIT_THE_NEXT_THEME: &str = r#"
[[steps]]
step = "key"
key = "t"

[[steps]]
step = "wait_text"
text = "THEME PREVIEW"
region = "dialog"

[[steps]]
step = "key"
key = "down"

[[steps]]
step = "key"
key = "enter"

[[steps]]
step = "settle"
"#;

fn expect_theme(equals: &str) -> String {
    format!(
        r#"
[[steps]]
step = "expect_config"
key = "runtime.theme"
equals = "{equals}"
"#
    )
}

#[test]
fn expect_config_reads_the_setting_a_theme_commit_saved() {
    let steps = format!(
        "{WAIT_FOR_COMPLETE}{COMMIT_THE_NEXT_THEME}{}",
        expect_theme("excise-light")
    );
    let (_fixture, run) = execute(&scenario(DELETE_FILE, &steps), Profile::Deterministic);
    assert!(run.passed(), "{run}");
}

#[test]
fn expect_config_says_what_the_file_holds_when_it_differs_or_holds_nothing() {
    // Nothing was saved: the file is the empty configuration the runner wrote.
    let steps = format!("{WAIT_FOR_COMPLETE}{}", expect_theme("excise-light"));
    let (_fixture, run) = execute(&scenario(DELETE_FILE, &steps), Profile::Deterministic);
    let failure = failure_of(&run);
    assert_eq!(failure.step.index, 1, "{run}");
    assert!(failure.message.contains("has no table `runtime`"), "{run}");

    // The commit saved another theme than the step expects.
    let steps = format!(
        "{WAIT_FOR_COMPLETE}{COMMIT_THE_NEXT_THEME}{}",
        expect_theme("excise-dark")
    );
    let (_fixture, run) = execute(&scenario(DELETE_FILE, &steps), Profile::Deterministic);
    let failure = failure_of(&run);
    assert_eq!(failure.step.index, 6, "{run}");
    assert!(failure.message.contains("it is \"excise-light\""), "{run}");
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
