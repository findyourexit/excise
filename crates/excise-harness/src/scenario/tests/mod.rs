//! Tests for the scenario model.

mod parse;
mod platform;
mod readme;
mod validate;

use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU32, Ordering},
};

use super::{Scenario, Step, StepError, ValidationError};

/// The smallest scenario that satisfies every rule. Rule tests break exactly one thing in it.
pub(super) const BASE: &str = r#"
schema_version = 1
name = "delete-folder-lifecycle"
description = "Deleting a folder leaves its siblings intact."
fixture = "delete-folder"
sentinels = ["keep-a.bin"]
profiles = ["default", "deterministic"]

[[steps]]
step = "wait_header"
state = "complete"
"#;

/// One of every step, in a scenario that satisfies every rule.
pub(super) const ALL_STEPS: &str = r#"
schema_version = 1
name = "all-steps"
description = "Every step type at least once."
fixture = "all-steps"
sentinels = ["keep-a.bin", "nested/keep-b.txt"]
profiles = ["default", "deterministic", "monochrome-ascii", "narrow", "mouse-keymaps"]
tier = "nightly"
platforms = ["linux", "macos", "windows"]
expect = "fail"
fails_on = ["linux", "macos"]
slice = "X2"

[terminal]
cols = 100
rows = 30

[budgets]
input_to_frame_p99_ms = 100
idle_output_bytes = 0

[[steps]]
step = "wait_text"
text = "SCANNING"
region = "header"
timeout_ms = 5000

[[steps]]
step = "wait_text"
regex = 'COMPLETE\s+·'
region = { rows = [0, 2] }

[[steps]]
step = "wait_header"
state = "complete"
timeout_ms = 30000

[[steps]]
step = "wait_event"
event = "deletion_finished"
fields = { removed = { min = 1 }, failed = { eq = 0 } }

[[steps]]
step = "key"
key = "down"

[[steps]]
step = "key"
key = "c"
ctrl = true

[[steps]]
step = "type"
text = "victim"

[[steps]]
step = "select"
name = "victim"

[[steps]]
step = "measure"
name = "delete-time"
marker = "start"

[[steps]]
step = "delete"
name = "victim"
kind = "folder"
wait_for = "started"

[[steps]]
step = "wait_fs_absent"
path = "victim"
timeout_ms = 60000

[[steps]]
step = "measure"
name = "delete-time"
marker = "stop"

[[steps]]
step = "wait_fs_present"
path = "keep-a.bin"

[[steps]]
step = "fs_mutate"
op = "appear"
path = "nested/new-file.bin"

[[steps]]
step = "resize"
cols = 60
rows = 20

[[steps]]
step = "signal"
signal = "term"

[[steps]]
step = "expect_screen"
contains = ["keep-a.bin"]
not_contains = ["victim"]
regex = ['keep-.*\.bin']
region = "dialog"

[[steps]]
step = "expect_fs"
present = ["keep-a.bin", "nested/keep-b.txt"]
absent = ["victim"]

[[steps]]
step = "expect_budget"
budget = "input_to_frame_p99_ms"
metric = "delete-time"

[[steps]]
step = "idle"
after_ms = 1
window_ms = 1

[[steps]]
step = "settle"

[[steps]]
step = "quit"

[[steps]]
step = "expect_exit"
code = 130
terminal_restored = true
residue = "none"
"#;

/// Parses TOML that the test expects to be well formed.
pub(super) fn parse(source: &str) -> Scenario {
    Scenario::from_toml_str(source).expect("the test scenario should parse")
}

/// The base scenario, which must satisfy every rule.
pub(super) fn valid() -> Scenario {
    let scenario = parse(BASE);
    scenario
        .validate()
        .expect("the base scenario should satisfy every rule");
    scenario
}

/// Every rule that `scenario` breaks, or none.
pub(super) fn errors_of(scenario: &Scenario) -> Vec<ValidationError> {
    scenario
        .validate()
        .err()
        .map(super::ValidationErrors::into_errors)
        .unwrap_or_default()
}

/// The rules broken by `step` alone, as the only step of an otherwise valid scenario.
///
/// Panics if the scenario breaks any rule that is not about that step, because a test of one
/// step must not be satisfied by an unrelated error.
pub(super) fn step_errors(step: Step) -> Vec<StepError> {
    let mut scenario = valid();
    scenario.steps = vec![step];
    errors_of(&scenario)
        .into_iter()
        .map(|error| match error {
            ValidationError::Step {
                index: 0, error, ..
            } => error,
            other => panic!("expected only errors for the step, found: {other}"),
        })
        .collect()
}

/// A directory under the system temporary directory that is removed on drop.
pub(super) struct TempDir(PathBuf);

impl TempDir {
    pub(super) fn new(label: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "excise-harness-test-{}-{label}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).expect("a fresh temporary directory should be creatable");
        Self(path)
    }

    pub(super) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
