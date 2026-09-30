//! The in-process scenario runner.
//!
//! It drives the real `runtime::run` with a ratatui `TestBackend` and walks a scenario's steps as
//! the owner loop's input source ([`input::ScenarioInput`]). Assertions run inside the input
//! source's `read`, against the terminal the loop just drew and the fixture on disk.
//!
//! * The runner takes an already-materialized fixture root and refuses one that the harness does
//!   not own ([`verify_owned`]: a real directory holding the ownership marker as a regular file).
//!   The suite gets its roots from the harness's fixture generator through [`fixture::materialize`].
//! * A scenario is refused before it runs when it is invalid, contains a step only another runner
//!   can perform ([`RunError::Unsupported`]), or holds a regular expression that does not compile.
//! * `settle` is the runtime's barrier. Because the barrier drains worker events outside the
//!   production scheduling path, this runner never judges scheduling, throughput, or timing.
//! * Each run gets a scratch directory for its scan-store session. `expect_exit` with
//!   `residue = "none"` asserts that the directory is empty once the program has exited.
//! * Every sentinel is asserted again once the run is over.
//!
//! The README of the harness crate documents the supported steps and how to add a scenario.

mod backend;
mod checks;
mod fixture;
mod input;
mod plan;
mod profile;
mod report;
mod screen;

use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Instant;

use excise_harness::fixture::{Fixtures, verify_owned};
use excise_harness::report::{FailedStep, ScreenComparison};
use excise_harness::scenario::{ExpectExit, Profile, Residue, Scenario, Step};

pub use fixture::{Fixture, materialize};
pub use input::Limits;
pub use plan::{RunError, UnsupportedStep, unsupported_steps};
pub use report::{ScenarioRun, StepFailure};
pub use screen::Screen;

use self::backend::SharedBackend;
use self::input::{Progress, ScenarioInput};
use crate::error::AppError;
use crate::outcome::{OperationOutcome, RunSummary};
use crate::runtime::{VirtualClock, run};
use crate::tests::fixtures::register_snapshot_root;

/// Runs `scenario` under `profile` against the fixture at `root`.
///
/// # Errors
///
/// Returns why the scenario was not run: the root is not an owned fixture, the scenario is
/// invalid, it has steps this runner cannot perform, or a pattern in it does not compile. A
/// scenario that ran and failed is not an error: it is a [`ScenarioRun`] with a failure.
pub fn run_scenario(
    scenario: &Scenario,
    profile: Profile,
    root: &Path,
) -> Result<ScenarioRun, RunError> {
    run_scenario_with(scenario, profile, root, Limits::default())
}

/// Like [`run_scenario`], with the wait limits chosen by the caller.
///
/// # Errors
///
/// As for [`run_scenario`].
pub fn run_scenario_with(
    scenario: &Scenario,
    profile: Profile,
    root: &Path,
    limits: Limits,
) -> Result<ScenarioRun, RunError> {
    verify_owned(root)?;
    let plan = Rc::new(plan::plan(scenario)?);
    // The dialog cuts a long path short. Displayed fixture paths stay short whatever the root.
    register_snapshot_root(root, &scenario.fixture);

    let scratch = tempfile::Builder::new()
        .prefix("excise-scenario-")
        .tempdir()
        .map_err(|error| RunError::Setup(format!("no scratch directory: {error}")))?;
    let scan_store = scratch.path().join("scan-store");
    let configuration = profile::configure(profile, scenario.terminal, root, scan_store.clone())
        .map_err(RunError::Setup)?;
    let backend = SharedBackend::new(configuration.cols, configuration.rows);
    let progress = Rc::new(RefCell::new(Progress::default()));
    let input = ScenarioInput::new(
        Rc::clone(&plan),
        backend.clone(),
        root.to_path_buf(),
        limits,
        Rc::clone(&progress),
    );

    let started = Instant::now();
    let result = run(
        backend,
        Box::new(input),
        configuration.settings,
        Box::new(VirtualClock::new()),
    );
    let duration = started.elapsed();

    let progress = progress.borrow();
    let failure = judge(&plan, &progress, &result, &scan_store, root);
    Ok(ScenarioRun {
        name: scenario.name.clone(),
        profile,
        expect: scenario.expect,
        duration,
        failure,
        sent_keys: progress.sent_keys.clone(),
    })
}

/// Decides whether the scenario passed, once the program has stopped.
///
/// A step that failed was recorded by the input source. Otherwise the program stopped either
/// because the steps ran out, or because it ended on its own: through a `quit` step, which leaves
/// the `expect_exit` step after it to be judged here, or through something the scenario did not
/// ask for. Whatever the ending, every sentinel is asserted again.
fn judge(
    plan: &plan::Plan,
    progress: &Progress,
    result: &Result<OperationOutcome<RunSummary>, AppError>,
    scan_store: &Path,
    root: &Path,
) -> Option<StepFailure> {
    if let Some(failure) = &progress.failure {
        return Some(failure.clone());
    }
    let ending = if progress.finished {
        None
    } else {
        judge_ending(plan, progress, result, scan_store)
    };
    ending.or_else(|| judge_sentinels(plan, progress, root))
}

/// Judges a program that ended on its own.
fn judge_ending(
    plan: &plan::Plan,
    progress: &Progress,
    result: &Result<OperationOutcome<RunSummary>, AppError>,
    scan_store: &Path,
) -> Option<StepFailure> {
    let code = match result {
        Ok(outcome) => outcome.exit_class().code(),
        Err(error) => error.exit_class().code(),
    };
    let remaining = &plan.steps[progress.cursor.min(plan.steps.len())..];
    let Some(next) = remaining.first() else {
        // The last step ran and the program is gone. That is a pass unless it went wrong.
        return result.as_ref().err().map(|error| {
            failure_at(
                progress,
                progress.cursor,
                "the run",
                "the program runs to the end of the scenario",
                format!("the program failed: {error}"),
            )
        });
    };
    if let Step::ExpectExit(exit) = &next.step {
        let problem = judge_exit(exit, code, result, scan_store)?;
        return Some(failure_at(
            progress,
            progress.cursor,
            &next.description,
            format!(
                "the program exits with code {} and leaves no residue",
                exit.code
            ),
            problem,
        ));
    }
    Some(failure_at(
        progress,
        progress.cursor,
        &next.description,
        "the program is still running for this step",
        format!("the program exited with code {code} before this step could run"),
    ))
}

/// What is wrong with the way the program exited, if anything.
fn judge_exit(
    exit: &ExpectExit,
    code: i32,
    result: &Result<OperationOutcome<RunSummary>, AppError>,
    scan_store: &Path,
) -> Option<String> {
    match result {
        Err(error) if exit.code != code => Some(format!("the program failed: {error}")),
        _ if exit.code != code => Some(format!(
            "the program exited with code {code}, not {}",
            exit.code
        )),
        _ => match exit.residue {
            Residue::None => residue(scan_store),
        },
    }
}

/// A message when the run left files in its scratch directory.
fn residue(scan_store: &Path) -> Option<String> {
    match fixture::entry_names(scan_store) {
        Ok(names) if names.is_empty() => None,
        Ok(names) => Some(format!(
            "the program left files in its scratch directory: {}",
            names.join(", ")
        )),
        Err(error) => Some(format!("the scratch directory cannot be read: {error}")),
    }
}

/// Asserts every sentinel again, whatever happened in between.
fn judge_sentinels(plan: &plan::Plan, progress: &Progress, root: &Path) -> Option<StepFailure> {
    for sentinel in &plan.sentinels {
        let problem = match fixture::entry_exists(root, sentinel) {
            Ok(true) => continue,
            Ok(false) => format!("the sentinel `{sentinel}` does not exist"),
            Err(message) => message,
        };
        return Some(failure_at(
            progress,
            plan.steps.len(),
            "the final sentinel check",
            "every sentinel exists after the last step",
            problem,
        ));
    }
    None
}

/// A failure that is not tied to a step the input source was running.
fn failure_at(
    progress: &Progress,
    index: usize,
    description: impl Into<String>,
    expected: impl Into<String>,
    message: String,
) -> StepFailure {
    StepFailure {
        step: FailedStep {
            index: u32::try_from(index).unwrap_or(u32::MAX),
            description: description.into(),
        },
        message,
        screen: ScreenComparison {
            expected: expected.into(),
            actual: progress
                .last_screen
                .as_ref()
                .map(Screen::text)
                .unwrap_or_default(),
        },
    }
}

/// The scenario files that were found and what became of them.
pub struct Selection {
    /// Scenarios this runner can run, with a fixture it can build.
    pub runnable: Vec<Scenario>,
    /// Scenarios left to another runner, by name, with the reason.
    pub skipped: Vec<(String, String)>,
}

/// The directory that holds the scenario files.
pub fn scenarios_directory() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("crates/excise-harness/scenarios")
}

/// The most entries a fixture may plan for this runner to generate it. The runner's tests run on
/// every `cargo test`, so the large fixtures stay with the runners built for them.
const MAX_FIXTURE_ENTRIES: u64 = 10_000;

/// Loads every scenario file in `directory` and selects the ones this runner can run.
///
/// A file that does not parse, does not validate, is not named after its scenario, or names a
/// fixture the harness cannot load is an error: it would break every runner. A scenario is skipped,
/// with the reason, when it has a step only another runner can perform, when its fixture needs a
/// scratch volume that only a privileged runner attaches, or when its fixture plans more than
/// [`MAX_FIXTURE_ENTRIES`] entries.
///
/// # Errors
///
/// Returns a message for the first file that cannot be used.
pub fn select_scenarios(directory: &Path) -> Result<Selection, String> {
    let unreadable =
        |error: std::io::Error| format!("cannot read `{}`: {error}", directory.display());
    let mut paths = fs::read_dir(directory)
        .map_err(unreadable)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(unreadable)?;
    paths.retain(|path| {
        path.extension()
            .is_some_and(|extension| extension == "toml")
    });
    paths.sort();

    let mut selection = Selection {
        runnable: Vec::new(),
        skipped: Vec::new(),
    };
    let fixtures = Fixtures::bundled();
    for path in paths {
        let scenario = Scenario::from_path(&path).map_err(|error| error.to_string())?;
        scenario
            .validate()
            .map_err(|error| format!("`{}`: {error}", path.display()))?;
        if path.file_stem().and_then(|stem| stem.to_str()) != Some(scenario.name.as_str()) {
            return Err(format!(
                "`{}` does not carry the name of its scenario, `{}`",
                path.display(),
                scenario.name
            ));
        }
        let spec = fixtures.spec(&scenario.fixture).map_err(|error| {
            format!(
                "`{}` names the fixture `{}`: {error}",
                path.display(),
                scenario.fixture
            )
        })?;
        let unsupported = unsupported_steps(&scenario);
        if !unsupported.is_empty() {
            let kinds = unsupported
                .iter()
                .map(|step| step.kind)
                .collect::<Vec<_>>()
                .join(", ");
            let reason = format!("has steps only another runner can perform: {kinds}");
            selection.skipped.push((scenario.name, reason));
        } else if spec.has_volumes() {
            let reason = format!(
                "its fixture `{}` needs a scratch volume, which only a privileged runner attaches",
                spec.id
            );
            selection.skipped.push((scenario.name, reason));
        } else if spec.planned_entry_count() > MAX_FIXTURE_ENTRIES {
            let reason = format!(
                "its fixture `{}` plans {} entries, more than the {MAX_FIXTURE_ENTRIES} this \
                 runner generates",
                spec.id,
                spec.planned_entry_count()
            );
            selection.skipped.push((scenario.name, reason));
        } else {
            selection.runnable.push(scenario);
        }
    }
    Ok(selection)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expected_exit(code: i32) -> ExpectExit {
        ExpectExit {
            code,
            terminal_restored: true,
            residue: Residue::None,
            timeout_ms: 10_000,
        }
    }

    #[test]
    fn an_exit_is_judged_by_its_code_and_by_what_it_leaves_behind() {
        let scratch = tempfile::tempdir().expect("the scratch directory should exist");
        let exact: Result<OperationOutcome<RunSummary>, AppError> =
            Ok(OperationOutcome::Exact(RunSummary::default()));
        assert_eq!(
            judge_exit(&expected_exit(0), 0, &exact, scratch.path()),
            None
        );
        // A run that never created its scan-store directory left nothing in it.
        let never_created = scratch.path().join("never-created");
        assert_eq!(
            judge_exit(&expected_exit(0), 0, &exact, &never_created),
            None
        );
        assert_eq!(
            judge_exit(&expected_exit(130), 0, &exact, scratch.path()).as_deref(),
            Some("the program exited with code 0, not 130")
        );

        // An error ends the program with the exit code of its class. It is only a problem when
        // the scenario expected another exit, and then the message names it.
        let failed: Result<OperationOutcome<RunSummary>, AppError> =
            Err(AppError::Worker("the scanner died".to_owned()));
        assert_eq!(
            judge_exit(&expected_exit(70), 70, &failed, scratch.path()),
            None
        );
        let message = judge_exit(&expected_exit(0), 70, &failed, scratch.path())
            .expect("an unexpected failure should be reported");
        assert!(message.contains("the scanner died"), "{message}");

        // A clean exit that left files behind is not clean.
        fs::create_dir(scratch.path().join(".excise-scan-left-over")).expect("residue");
        let message = judge_exit(&expected_exit(0), 0, &exact, scratch.path())
            .expect("residue should be reported");
        assert!(message.contains(".excise-scan-left-over"), "{message}");
    }
}
