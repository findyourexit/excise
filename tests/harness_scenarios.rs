//! Runs the harness's pseudo-terminal scenarios against this crate's own `excise` binary.
//!
//! One run per scenario and profile keeps this fast. The full matrix and the repeat runs that
//! prove identical verdicts are `cargo xtask e2e`.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{
        Mutex, MutexGuard, PoisonError,
        atomic::{AtomicU32, Ordering},
    },
};

use excise_harness::{
    fixture::Fixtures,
    report::Verdict,
    runner::{FailureCause, RunReport, RunRequest, run_scenario, work_base},
    scenario::{Profile, Scenario, Step},
};
use serde_json::Value;

/// The scenarios `cargo xtask e2e` runs.
const SCENARIOS: &str = "crates/excise-harness/scenarios";
/// Controls only this test runs. They are not product scenarios and `xtask e2e` never sees them.
const CONTROLS: &str = "crates/excise-harness/tests/controls";

/// One pseudo-terminal session at a time, so a session is fully cleaned up before the next one.
fn session_guard() -> MutexGuard<'static, ()> {
    static SESSION: Mutex<()> = Mutex::new(());
    SESSION.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A unique directory under the harness work area, removed when dropped.
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

/// Loads and validates `<dir>/<name>.toml`, with `dir` relative to the workspace root.
fn load(dir: &str, name: &str) -> Scenario {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(dir)
        .join(format!("{name}.toml"));
    let scenario = Scenario::from_path(&path).expect("the scenario loads");
    scenario.validate().expect("the scenario is valid");
    scenario
}

/// A one-off scenario over the `delete-folder` fixture: it waits for the scan, then runs `steps`.
/// The scan wait matches the scenario files' 30 s bound; a Windows runner needs over 10 s.
fn inline(name: &str, steps: &str) -> Scenario {
    let scenario = Scenario::from_toml_str(&format!(
        "schema_version = 1\nname = \"{name}\"\ndescription = \"a control for tests/harness_scenarios.rs\"\n\
         fixture = \"delete-folder\"\nsentinels = [\"keep-a.bin\", \"keep-b/keep.txt\"]\n\
         profiles = [\"default\"]\n\
         [[steps]]\nstep = \"wait_header\"\nstate = \"complete\"\ntimeout_ms = 30000\n{steps}"
    ))
    .expect("a scenario");
    scenario.validate().expect("a valid scenario");
    scenario
}

/// A one-off scenario over the `delete-folder` fixture that documents an `expect = "fail"`
/// restricted to `fails_on`, and fails deterministically: it waits for the scan, then asserts
/// text that never appears on the screen.
fn inline_expected_failure(name: &str, fails_on: &str) -> Scenario {
    let scenario = Scenario::from_toml_str(&format!(
        "schema_version = 1\nname = \"{name}\"\ndescription = \"a control for tests/harness_scenarios.rs\"\n\
         fixture = \"delete-folder\"\nsentinels = [\"keep-a.bin\", \"keep-b/keep.txt\"]\n\
         profiles = [\"default\"]\nexpect = \"fail\"\nslice = \"H5\"\nfails_on = [\"{fails_on}\"]\n\
         [[steps]]\nstep = \"wait_header\"\nstate = \"complete\"\ntimeout_ms = 30000\n\
         [[steps]]\nstep = \"expect_screen\"\ncontains = [\"XYZZY-TEXT-NEVER-ON-SCREEN\"]\n"
    ))
    .expect("a scenario");
    scenario.validate().expect("a valid scenario");
    scenario
}

/// Runs `scenario` once against the fixture at `root`. Returns the report and the directory a
/// failure bundle is written to.
fn run(
    scenario: &Scenario,
    profile: Profile,
    root: &Path,
    work: &Workspace,
) -> (RunReport, PathBuf) {
    let bundle = work.0.join("bundle");
    let report = run_scenario(&RunRequest {
        scenario,
        profile,
        binary: Path::new(env!("CARGO_BIN_EXE_excise")),
        fixture_root: root,
        work_dir: &work.0,
        bundle_dir: Some(&bundle),
        repro_command: "cargo test --test harness_scenarios",
        fixture_seed: 0,
        keep_scratch: false,
    });
    (report, bundle)
}

/// What a fixture entry is, with the bytes of a file and the target of a link.
#[derive(Debug, PartialEq, Eq)]
enum Entry {
    Directory,
    File(Vec<u8>),
    Link(PathBuf),
}

/// Every entry under `root` by relative path. Two equal trees are equal byte for byte.
fn tree(root: &Path) -> BTreeMap<PathBuf, Entry> {
    fn walk(directory: &Path, relative: &Path, into: &mut BTreeMap<PathBuf, Entry>) {
        for entry in fs::read_dir(directory).expect("a readable directory") {
            let entry = entry.expect("a directory entry");
            let path = entry.path();
            let name = relative.join(entry.file_name());
            let kind = entry.file_type().expect("a file type");
            if kind.is_dir() {
                into.insert(name.clone(), Entry::Directory);
                walk(&path, &name, into);
            } else if kind.is_symlink() {
                into.insert(
                    name,
                    Entry::Link(fs::read_link(&path).expect("a link target")),
                );
            } else {
                into.insert(name, Entry::File(fs::read(&path).expect("a readable file")));
            }
        }
    }
    let mut entries = BTreeMap::new();
    walk(root, Path::new(""), &mut entries);
    entries
}

#[test]
fn the_delete_folder_lifecycle_passes_under_every_profile_it_names() {
    let _session = session_guard();
    let scenario = load(SCENARIOS, "delete-folder-lifecycle");
    assert!(scenario.profiles.contains(&Profile::Default));
    assert!(scenario.profiles.contains(&Profile::Deterministic));
    for &profile in &scenario.profiles {
        let work = Workspace::new();
        let fixture = Fixtures::bundled()
            .run_copy(&scenario.fixture, &work.0)
            .expect("the fixture is built");
        let (report, _) = run(&scenario, profile, fixture.root(), &work);
        assert_eq!(
            report.verdict,
            Verdict::Pass,
            "{profile}: failure {:?}, error {:?}",
            report.failure,
            report.error
        );
    }
}

#[test]
fn a_wrong_delete_target_fails_the_step_and_no_confirmation_key_is_ever_sent() {
    let _session = session_guard();
    let scenario = load(CONTROLS, "delete-wrong-target");
    let work = Workspace::new();
    let fixture = Fixtures::bundled()
        .run_copy(&scenario.fixture, &work.0)
        .expect("the fixture is built");
    let before = tree(fixture.root());
    let (report, bundle) = run(&scenario, Profile::Default, fixture.root(), &work);

    // The `delete` step failed because it refused, not because of a timeout or a crash.
    assert_eq!(report.verdict, Verdict::Fail, "{report:?}");
    let failure = report.failure.as_ref().expect("a failed step");
    assert_eq!(failure.cause, FailureCause::DeleteRefused, "{failure}");
    assert!(
        matches!(scenario.steps.get(failure.index), Some(Step::Delete(_))),
        "the failed step is not the delete step: {failure}"
    );

    // The recording shows every byte the runner wrote. The dialog was opened with Backspace, and
    // nothing was sent after it: the filter's Enter came before it, and no `y` was ever sent.
    let cast = fs::read_to_string(bundle.join("session.cast")).expect("the bundle has a recording");
    let inputs: Vec<String> = cast
        .lines()
        .skip(1)
        .map(|line| serde_json::from_str::<Value>(line).expect("a cast event"))
        .filter(|event| event[1] == "i")
        .map(|event| event[2].as_str().expect("input text").to_owned())
        .collect();
    assert_eq!(
        inputs.last().map(String::as_str),
        Some("\u{7f}"),
        "{inputs:?}"
    );
    assert!(!inputs.iter().any(|input| input == "y"), "{inputs:?}");

    // Nothing was deleted, and nothing else changed either.
    assert!(tree(fixture.root()) == before, "the fixture changed");
}

#[test]
fn live_mutations_are_applied_at_their_steps_and_are_not_reported_as_damage() {
    let _session = session_guard();
    let scenario = inline(
        "mutate",
        r#"
[[steps]]
step = "fs_mutate"
op = "appear"
path = "added/note.bin"

[[steps]]
step = "fs_mutate"
op = "change"
path = "keep-a.bin"

[[steps]]
step = "fs_mutate"
op = "replace"
path = "keep-b/keep.txt"

[[steps]]
step = "fs_mutate"
op = "vanish"
path = "victim/part00"

[[steps]]
step = "expect_fs"
present = ["added/note.bin", "keep-a.bin", "keep-b/keep.txt"]
absent = ["victim/part00"]

[[steps]]
step = "quit"

[[steps]]
step = "expect_exit"
code = 0
terminal_restored = true
residue = "none"
"#,
    );
    let work = Workspace::new();
    let fixture = Fixtures::bundled()
        .run_copy(&scenario.fixture, &work.0)
        .expect("the fixture is built");
    let before = tree(fixture.root());

    let (report, _) = run(&scenario, Profile::Default, fixture.root(), &work);

    // The final comparison with the fixture as it was accepts the four intended changes.
    assert_eq!(
        report.verdict,
        Verdict::Pass,
        "failure {:?}, error {:?}",
        report.failure,
        report.error
    );
    let after = tree(fixture.root());
    assert!(
        matches!(after.get(Path::new("added/note.bin")), Some(Entry::File(bytes)) if !bytes.is_empty())
    );
    assert!(
        matches!(
            (before.get(Path::new("keep-a.bin")), after.get(Path::new("keep-a.bin"))),
            (Some(Entry::File(old)), Some(Entry::File(new))) if new.len() > old.len()
        ),
        "`change` must grow the file"
    );
    assert!(!after.contains_key(Path::new("victim/part00")));
}

#[test]
fn a_refused_mutation_fails_its_step_and_changes_nothing() {
    let _session = session_guard();
    let scenario = inline(
        "mutate-refused",
        "[[steps]]\nstep = \"fs_mutate\"\nop = \"change\"\npath = \"absent.bin\"\n",
    );
    let work = Workspace::new();
    let fixture = Fixtures::bundled()
        .run_copy(&scenario.fixture, &work.0)
        .expect("the fixture is built");
    let before = tree(fixture.root());

    let (report, _) = run(&scenario, Profile::Default, fixture.root(), &work);

    assert_eq!(report.verdict, Verdict::Fail, "{report:?}");
    let failure = report.failure.as_ref().expect("a failed step");
    assert_eq!(failure.cause, FailureCause::Mismatch, "{failure}");
    assert_eq!(failure.index, 1, "{failure}");
    assert!(failure.detail.contains("does not exist"), "{failure}");
    assert!(tree(fixture.root()) == before, "the fixture changed");
}

#[test]
fn an_expected_failure_applies_only_on_its_platform() {
    let _session = session_guard();
    let host = std::env::consts::OS;
    let other = if host == "linux" { "macos" } else { "linux" };

    // `fails_on` names the host: the deterministic failure is documented here, so it is `xfail`.
    let scenario = inline_expected_failure("xfail-here", host);
    let work = Workspace::new();
    let fixture = Fixtures::bundled()
        .run_copy(&scenario.fixture, &work.0)
        .expect("the fixture is built");
    let (report, _) = run(&scenario, Profile::Default, fixture.root(), &work);
    assert_eq!(report.verdict, Verdict::Xfail, "{report:?}");

    // `fails_on` names only another platform: the same deterministic failure is an ordinary,
    // undocumented `fail` here.
    let scenario = inline_expected_failure("fail-elsewhere", other);
    let work = Workspace::new();
    let fixture = Fixtures::bundled()
        .run_copy(&scenario.fixture, &work.0)
        .expect("the fixture is built");
    let (report, _) = run(&scenario, Profile::Default, fixture.root(), &work);
    assert_eq!(report.verdict, Verdict::Fail, "{report:?}");
}

#[test]
fn a_deferred_delete_returns_once_the_dialog_closes_and_the_deletion_still_finishes() {
    let _session = session_guard();
    let scenario = inline(
        "deferred-delete",
        r#"
[[steps]]
step = "select"
name = "victim"

[[steps]]
step = "delete"
name = "victim"
kind = "folder"
wait_for = "started"
timeout_ms = 60000

[[steps]]
step = "expect_fs"
present = ["victim"]

[[steps]]
step = "wait_fs_absent"
path = "victim"
timeout_ms = 60000

[[steps]]
step = "wait_event"
event = "deletion_finished"
fields = { removed = { eq = 5011 }, failed = { eq = 0 } }
timeout_ms = 30000

[[steps]]
step = "expect_fs"
present = ["keep-a.bin", "keep-b/keep.txt"]
absent = ["victim"]
"#,
    );
    let work = Workspace::new();
    let fixture = Fixtures::bundled()
        .run_copy(&scenario.fixture, &work.0)
        .expect("the fixture is built");

    // `wait_for = "started"` must return before the 5,011-entry deletion can possibly have
    // finished: `expect_fs { present = ["victim"] }` right after the step, with no wait in
    // between, asserts exactly that. The deletion still finishes normally afterward, on its own:
    // the filesystem loses `victim`, the program reports it through `deletion_finished`, and the
    // sentinels survive. What quitting afterward does is a different, already-covered behavior
    // (`delete-folder-lifecycle`), not this option's. `wait_fs_absent` waits for the whole deletion
    // here, so it gets the 60 s the lifecycle's `delete` step gives the same deletion: this test
    // runs the debug binary, and under load 30 s was not enough.
    let (report, _) = run(&scenario, Profile::Deterministic, fixture.root(), &work);

    assert_eq!(
        report.verdict,
        Verdict::Pass,
        "failure {:?}, error {:?}",
        report.failure,
        report.error
    );
}
