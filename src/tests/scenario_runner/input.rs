//! `ScenarioInput`: a scenario walked as the owner loop's input source.
//!
//! The owner loop asks its input source for an event whenever it wants one. `ScenarioInput`
//! answers with the scenario's next key press, a barrier, or, when a step fails or the steps run
//! out, an error that makes the loop shut down cleanly. Assertions run inside `read`, against the
//! terminal the loop just drew and the fixture on disk.
//!
//! * `key`, `type`, and `resize` deliver events and pass.
//! * `settle` and `wait_refresh` deliver one barrier, which renders and then drains scan work,
//!   deletion work, the rebuild or publication that replaces the map after a deletion, timers, and
//!   animation until the loop is quiescent.
//! * Every wait is a bounded loop: check, and while the condition does not hold, deliver a barrier
//!   and check again, until the step's `timeout_ms` or the round limit is reached.
//! * A check runs on a fresh screen. The screen is fresh when the last thing delivered was a
//!   barrier, because a barrier renders. Anything else since then can have left the frame behind,
//!   so a barrier comes first.
//! * `fs_mutate` applies the harness's live mutators to the fixture once the program is at rest:
//!   unless the last thing delivered was a barrier, one comes first, so even a mutation that opens
//!   the scenario lands after the first scan has settled.
//! * `select`, `delete`, and `quit` are short sequences of the above with checks in between.
//!   `delete` sends `y` only after the dialog, the target, the sentinels, and the ownership marker
//!   have all been verified, the marker last.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::rc::Rc;
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use excise_harness::fixture::{MARKER_FILE_NAME, mutate, verify_owned};
use excise_harness::report::{FailedStep, ScreenComparison};
use excise_harness::safety::FixtureRoot;
use excise_harness::scenario::{
    ConfirmKey, Delete, EntryKind, ExpectConfig, ExpectExit, ExpectFs, ExpectScreen, FsMutate,
    KeyName, PressKey, Quit, Resize, ScanState, Select, Step, TypeText, WaitText, config_setting,
};

use super::backend::SharedBackend;
use super::checks::{
    check_target, check_target_path, expectation, fs_expectation, fs_violations, is_delete_dialog,
    region_name, screen_violations,
};
use super::fixture::{entry_exists, entry_metadata};
use super::plan::{Plan, PlannedStep};
use super::report::StepFailure;
use super::screen::{Panel, Screen};
use crate::error::AppError;
use crate::input::{InputEvent, InputSource};

/// How the runner bounds a wait, besides the step's own `timeout_ms`.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// The pause between a check that failed and the next barrier.
    pub poll_interval: Duration,
    /// The most barriers one wait may deliver.
    pub max_rounds: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_millis(2),
            max_rounds: 100_000,
        }
    }
}

/// What the runner learns while the program runs. The input source writes it and the runner reads
/// it once the program has stopped.
#[derive(Default)]
pub struct Progress {
    /// The step in flight. Every earlier step has passed.
    pub cursor: usize,
    /// Set when a step fails; the input source then stops the program.
    pub failure: Option<StepFailure>,
    /// Set when every step has passed and the program is still running; the input source then
    /// stops it.
    pub finished: bool,
    /// The key events delivered to the program, in order.
    pub sent_keys: Vec<KeyEvent>,
    /// The last screen a check looked at.
    pub last_screen: Option<Screen>,
}

/// The error that ends the run when the input source stops the program.
fn stop_error() -> AppError {
    AppError::Invariant("the scenario runner stopped the program".to_owned())
}

/// What a check found.
enum Check<T> {
    /// The condition holds.
    Ready(T),
    /// The condition does not hold yet. Carries what was seen instead.
    Pending(String),
    /// The condition can never hold. Carries why.
    Fail(String),
}

/// What a bounded wait does next.
enum Advance<T> {
    Ready(T),
    Again(Wait),
    Fail(String),
}

/// One bounded wait.
struct Wait {
    started: Instant,
    timeout: Duration,
    rounds: usize,
}

impl Wait {
    fn new(timeout_ms: u64) -> Self {
        Self {
            started: Instant::now(),
            timeout: Duration::from_millis(timeout_ms),
            rounds: 0,
        }
    }

    /// Spends one round: pauses, and counts it. Fails with the reason once a bound is reached.
    fn round(&mut self, limits: &Limits) -> Result<(), String> {
        let elapsed = self.started.elapsed();
        if elapsed >= self.timeout {
            return Err(format!(
                "timed out after {} ms ({} barriers, timeout_ms = {})",
                elapsed.as_millis(),
                self.rounds,
                self.timeout.as_millis()
            ));
        }
        if self.rounds >= limits.max_rounds {
            return Err(format!(
                "gave up after {} barriers, the round limit ({} ms, timeout_ms = {})",
                self.rounds,
                elapsed.as_millis(),
                self.timeout.as_millis()
            ));
        }
        self.rounds += 1;
        thread::sleep(
            limits
                .poll_interval
                .min(self.timeout.saturating_sub(elapsed)),
        );
        Ok(())
    }
}

/// Where a step is. Steps that do one thing at once need no state.
#[derive(Default)]
enum State {
    #[default]
    Start,
    Waiting(Wait),
    Select(SelectPhase),
    Delete(DeletePhase),
    Quit(QuitPhase),
}

enum SelectPhase {
    /// Open the filter.
    Start,
    /// The filter was opened; wait for its prompt.
    Prompt(Wait),
    /// The name was typed; wait for the prompt to show it.
    Typed(Wait),
    /// The filter was applied; wait for the selected item.
    Applied(Wait),
}

enum DeletePhase {
    /// Ask to delete the selected entry.
    Start,
    /// Wait for the confirmation dialog.
    Dialog(Wait),
    /// The confirmation was sent; wait for the dialog to close.
    Closed(Wait),
    /// No dialog confirms the deletion (`disable_delete_confirmation`): Backspace was sent; wait
    /// for the program to settle, and fail if a dialog is open.
    Begun(Wait),
}

enum QuitPhase {
    /// Ask to quit.
    Start,
    /// Wait for the quit prompt.
    Prompt(Wait),
}

/// What the input source does next.
enum Drive {
    /// Deliver these events.
    Emit(Vec<Event>),
    /// Deliver a barrier.
    Barrier,
    /// The step is done; go on to the next one.
    Next,
    /// The step failed.
    Fail { expected: String, message: String },
}

fn fail(expected: impl Into<String>, message: impl Into<String>) -> Drive {
    Drive::Fail {
        expected: expected.into(),
        message: message.into(),
    }
}

/// The scenario as an input source.
pub struct ScenarioInput {
    plan: Rc<Plan>,
    backend: SharedBackend,
    root: PathBuf,
    /// The configuration file the program was given (`EXCISE_CONFIG`'s counterpart).
    config: PathBuf,
    limits: Limits,
    progress: Rc<RefCell<Progress>>,
    /// Events a step has decided on and not yet delivered.
    queued: VecDeque<Event>,
    state: State,
    /// Whether the last thing delivered was a barrier, or nothing was delivered yet.
    fresh: bool,
    /// Whether the last thing delivered was a barrier. Unlike `fresh` it is false before anything
    /// was delivered, when the program may still be starting its scan.
    settled: bool,
}

impl ScenarioInput {
    pub fn new(
        plan: Rc<Plan>,
        backend: SharedBackend,
        root: PathBuf,
        config: PathBuf,
        limits: Limits,
        progress: Rc<RefCell<Progress>>,
    ) -> Self {
        Self {
            plan,
            backend,
            root,
            config,
            limits,
            progress,
            queued: VecDeque::new(),
            state: State::Start,
            fresh: true,
            settled: false,
        }
    }

    /// Hands an event to the program and remembers that the screen is no longer fresh.
    fn deliver(&mut self, event: Event) -> InputEvent {
        self.fresh = false;
        self.settled = false;
        if let Event::Key(key) = &event {
            self.progress.borrow_mut().sent_keys.push(*key);
        }
        InputEvent::Terminal(event)
    }

    /// Reads the terminal and remembers what was seen.
    fn snapshot(&self) -> Screen {
        let screen = self.backend.screen();
        self.progress.borrow_mut().last_screen = Some(screen.clone());
        screen
    }

    /// Marks the step in flight as passed.
    fn complete(&mut self) {
        self.progress.borrow_mut().cursor += 1;
        self.state = State::Start;
    }

    fn record_failure(
        &self,
        index: usize,
        planned: &PlannedStep,
        expected: String,
        message: String,
    ) {
        self.progress.borrow_mut().failure = Some(StepFailure {
            step: FailedStep {
                index: u32::try_from(index).unwrap_or(u32::MAX),
                description: planned.description.clone(),
            },
            message,
            screen: ScreenComparison {
                expected,
                actual: self.backend.screen().text(),
            },
        });
    }

    /// Decides what the step in flight does next.
    fn drive(&mut self, planned: &PlannedStep) -> Drive {
        match &planned.step {
            Step::Key(press) => self.press(*press),
            Step::Type(typed) => self.type_text(typed),
            // The barrier renders and then drains everything the owner loop has outstanding: scan
            // work, deletion work, and the rebuild or publication that replaces the map a deletion
            // left stale. It does not return until all of it has ended, so it is `wait_refresh`
            // exactly, with none of the races a process has to wait out.
            Step::Settle(_) | Step::WaitRefresh(_) => {
                self.complete();
                Drive::Barrier
            }
            Step::Resize(resize) => self.resize(*resize),
            Step::WaitText(wait) => self.wait_text(wait, planned),
            Step::WaitHeader(wait) => self.wait_header(wait.state, wait.timeout_ms),
            Step::WaitFsAbsent(wait) => self.wait_fs(&wait.path, false, wait.timeout_ms),
            Step::WaitFsPresent(wait) => self.wait_fs(&wait.path, true, wait.timeout_ms),
            Step::ExpectScreen(expect) => self.expect_screen(expect, planned),
            Step::ExpectFs(expect) => self.expect_fs(expect),
            Step::ExpectConfig(expect) => self.expect_config(expect),
            Step::FsMutate(change) => self.fs_mutate(change),
            Step::ExpectExit(exit) => self.wait_for_exit(exit),
            Step::Select(select) => self.select(select),
            Step::Delete(delete) => self.delete(delete),
            Step::Quit(quit) => self.quit(*quit),
            Step::Signal(_)
            | Step::WaitEvent(_)
            | Step::ExpectBudget(_)
            | Step::Measure(_)
            | Step::Idle(_) => fail(
                "a step the in-process runner can perform",
                "the plan should have refused this step before the run",
            ),
        }
    }

    /// Spends a round of a wait on what a check found.
    fn advance<T>(&self, mut wait: Wait, check: Check<T>) -> Advance<T> {
        match check {
            Check::Ready(value) => Advance::Ready(value),
            Check::Fail(message) => Advance::Fail(message),
            Check::Pending(seen) => match wait.round(&self.limits) {
                Ok(()) => Advance::Again(wait),
                Err(bound) => Advance::Fail(format!("{bound}: {seen}")),
            },
        }
    }

    /// Keeps `wait` in the step's state and delivers a barrier.
    fn barrier_for(&mut self, wait: Wait, state: impl FnOnce(Wait) -> State) -> Drive {
        self.state = state(wait);
        Drive::Barrier
    }

    // --- Steps that deliver events ---------------------------------------------------------

    fn press(&mut self, press: PressKey) -> Drive {
        let exports = !press.ctrl && !press.alt && matches!(press.key, KeyName::Char('e' | 'E'));
        if exports && let Some(refusal) = self.export_guard() {
            return refusal;
        }
        self.complete();
        Drive::Emit(vec![press_event(press)])
    }

    fn type_text(&mut self, typed: &TypeText) -> Drive {
        if typed.text.contains(['e', 'E'])
            && let Some(refusal) = self.export_guard()
        {
            return refusal;
        }
        self.complete();
        Drive::Emit(typed.text.chars().map(char_event).collect())
    }

    /// An unmodified `e` or `E` outside a text prompt exports a report into the process working
    /// directory, which an in-process run cannot isolate. Returns the drive that refuses it, or
    /// `None` when a text prompt is open and the key is just text.
    fn export_guard(&mut self) -> Option<Drive> {
        if !self.fresh {
            return Some(Drive::Barrier);
        }
        if self.snapshot().text_entry_open() {
            return None;
        }
        Some(fail(
            "a text prompt open for the typed `e` or `E`",
            "refusing to press `e` or `E` outside a text prompt: it exports a report into the \
             process working directory, which the in-process runner cannot isolate",
        ))
    }

    fn resize(&mut self, resize: Resize) -> Drive {
        self.backend.resize(resize.cols, resize.rows);
        self.complete();
        Drive::Emit(vec![Event::Resize(resize.cols, resize.rows)])
    }

    // --- Steps that change the fixture -----------------------------------------------------

    /// Applies a live change to the fixture once the program is at rest: after a barrier, with
    /// nothing delivered since. The change therefore lands after everything the scenario did
    /// before it has taken effect, and after the first scan has settled. Nothing tells the
    /// program: what it does about the change is what the steps after this one observe.
    fn fs_mutate(&mut self, change: &FsMutate) -> Drive {
        if !self.settled {
            return Drive::Barrier;
        }
        match mutate::apply(&self.root, change.op, &change.path) {
            Ok(_) => {
                self.complete();
                Drive::Next
            }
            Err(error) => fail(
                format!("`{}` applies to `{}`", change.op, change.path),
                error.to_string(),
            ),
        }
    }

    // --- Waits and assertions ---------------------------------------------------------------

    /// A bounded wait on `check`, which runs on a fresh screen.
    fn wait_for(
        &mut self,
        timeout_ms: u64,
        expected: String,
        check: impl FnOnce(&Self) -> Check<()>,
    ) -> Drive {
        let wait = match std::mem::take(&mut self.state) {
            State::Waiting(wait) => wait,
            _ => Wait::new(timeout_ms),
        };
        if !self.fresh {
            return self.barrier_for(wait, State::Waiting);
        }
        let check = check(self);
        match self.advance(wait, check) {
            Advance::Ready(()) => {
                self.complete();
                Drive::Next
            }
            Advance::Again(wait) => self.barrier_for(wait, State::Waiting),
            Advance::Fail(message) => fail(expected, message),
        }
    }

    fn wait_text(&mut self, wait: &WaitText, planned: &PlannedStep) -> Drive {
        let region = wait.region;
        let named = region_name(region);
        let expected = match (&wait.text, &wait.regex) {
            (Some(text), _) => format!("{named} contains {text:?}"),
            (None, Some(regex)) => format!("{named} matches /{regex}/"),
            (None, None) => format!("{named} matches nothing"),
        };
        self.wait_for(wait.timeout_ms, expected, |input| {
            let Some(text) = input.snapshot().region_text(region) else {
                return Check::Pending("no dialog is open".to_owned());
            };
            let found = match (&wait.text, planned.regexes.first()) {
                (Some(needle), _) => text.contains(needle.as_str()),
                (None, Some(regex)) => regex.is_match(&text),
                (None, None) => false,
            };
            if found {
                Check::Ready(())
            } else if region.is_none() {
                Check::Pending("it is not on the screen".to_owned())
            } else {
                Check::Pending(format!("{named} reads:\n{text}"))
            }
        })
    }

    fn wait_header(&mut self, wanted: ScanState, timeout_ms: u64) -> Drive {
        self.wait_for(
            timeout_ms,
            format!("the header reports the scan state {wanted}"),
            |input| match input.snapshot().header_state() {
                Some(state) if state == wanted => Check::Ready(()),
                Some(state) => Check::Pending(format!("the header reports {state}")),
                None => Check::Pending("the header reports no scan state".to_owned()),
            },
        )
    }

    fn wait_fs(&mut self, path: &str, present: bool, timeout_ms: u64) -> Drive {
        let expected = if present {
            format!("`{path}` exists")
        } else {
            format!("`{path}` does not exist")
        };
        self.wait_for(timeout_ms, expected, |input| {
            match entry_exists(&input.root, path) {
                Ok(exists) if exists == present => Check::Ready(()),
                Ok(true) => Check::Pending(format!("`{path}` still exists")),
                Ok(false) => Check::Pending(format!("`{path}` does not exist yet")),
                Err(message) => Check::Fail(message),
            }
        })
    }

    fn expect_screen(&mut self, expect: &ExpectScreen, planned: &PlannedStep) -> Drive {
        if !self.fresh {
            return Drive::Barrier;
        }
        let violations = screen_violations(&self.snapshot(), expect, &planned.regexes);
        if violations.is_empty() {
            self.complete();
            return Drive::Next;
        }
        fail(expectation(expect), violations.join("; "))
    }

    fn expect_fs(&mut self, expect: &ExpectFs) -> Drive {
        if !self.fresh {
            return Drive::Barrier;
        }
        let violations = fs_violations(&self.root, expect);
        if violations.is_empty() {
            self.complete();
            return Drive::Next;
        }
        fail(fs_expectation(expect), violations.join("; "))
    }

    /// Reads the configuration file the program was given and compares one setting of it. The
    /// program writes the file as it handles a key, so the check runs on a fresh screen: a barrier
    /// comes first when input was delivered since the last one.
    fn expect_config(&mut self, expect: &ExpectConfig) -> Drive {
        if !self.fresh {
            return Drive::Barrier;
        }
        let expected = format!(
            "`{}` in the configuration file to be {:?}",
            expect.key, expect.equals
        );
        let found = std::fs::read_to_string(&self.config)
            .map_err(|error| format!("the configuration file cannot be read: {error}"))
            .and_then(|text| config_setting(&text, &expect.key));
        match found {
            Ok(value) if value == expect.equals => {
                self.complete();
                Drive::Next
            }
            Ok(value) => fail(expected, format!("it is {value:?}")),
            Err(message) => fail(expected, message),
        }
    }

    /// `expect_exit` is only ever read while the program is still running: once it has exited,
    /// nothing asks for input again and the runner judges the exit. So this waits, with barriers,
    /// for an exit that is still pending, and fails when it never comes.
    fn wait_for_exit(&mut self, exit: &ExpectExit) -> Drive {
        self.wait_for(
            exit.timeout_ms,
            format!("the program exits with code {}", exit.code),
            |_| Check::Pending("the program is still running".to_owned()),
        )
    }

    // --- Steps that are short sequences ----------------------------------------------------

    /// Opens the filter, replaces whatever it holds with the name, applies it, and asserts that the
    /// selected item is that entry.
    fn select(&mut self, select: &Select) -> Drive {
        let phase = match std::mem::take(&mut self.state) {
            State::Select(phase) => phase,
            _ => SelectPhase::Start,
        };
        let bound = select.timeout_ms;
        let name = select.name.as_str();
        match phase {
            SelectPhase::Start => {
                if !self.fresh {
                    self.state = State::Select(SelectPhase::Start);
                    return Drive::Barrier;
                }
                self.state = State::Select(SelectPhase::Prompt(Wait::new(bound)));
                Drive::Emit(vec![char_event('/')])
            }
            SelectPhase::Prompt(wait) => {
                if !self.fresh {
                    return self.barrier_for(wait, |wait| State::Select(SelectPhase::Prompt(wait)));
                }
                let screen = self.snapshot();
                let check = match screen.filter_prompt() {
                    Some(held) => Check::Ready(held),
                    None => Check::Pending(format!(
                        "the filter prompt is not open; the status line reads {:?}",
                        screen.row(2)
                    )),
                };
                match self.advance(wait, check) {
                    Advance::Ready(held) => {
                        let mut events = vec![key_event(KeyCode::Backspace); held.chars().count()];
                        events.extend(name.chars().map(char_event));
                        self.state = State::Select(SelectPhase::Typed(Wait::new(bound)));
                        Drive::Emit(events)
                    }
                    Advance::Again(wait) => {
                        self.barrier_for(wait, |wait| State::Select(SelectPhase::Prompt(wait)))
                    }
                    Advance::Fail(message) => fail("the filter prompt opens", message),
                }
            }
            SelectPhase::Typed(wait) => {
                if !self.fresh {
                    return self.barrier_for(wait, |wait| State::Select(SelectPhase::Typed(wait)));
                }
                match self.snapshot().filter_prompt() {
                    Some(typed) if typed == name => {
                        self.state = State::Select(SelectPhase::Applied(Wait::new(bound)));
                        Drive::Emit(vec![key_event(KeyCode::Enter)])
                    }
                    other => fail(
                        format!("the filter prompt shows {name:?}"),
                        format!("the filter prompt shows {other:?} after typing {name:?}"),
                    ),
                }
            }
            SelectPhase::Applied(wait) => {
                if !self.fresh {
                    return self
                        .barrier_for(wait, |wait| State::Select(SelectPhase::Applied(wait)));
                }
                let check = match self.snapshot().selected_item() {
                    Some(item) if item.name == name => Check::Ready(()),
                    Some(item) => Check::Pending(format!("the selected item is {:?}", item.name)),
                    None => Check::Pending(
                        "no item is selected, or the selected-item panel is not visible at this \
                         terminal size"
                            .to_owned(),
                    ),
                };
                match self.advance(wait, check) {
                    Advance::Ready(()) => {
                        self.complete();
                        Drive::Next
                    }
                    Advance::Again(wait) => {
                        self.barrier_for(wait, |wait| State::Select(SelectPhase::Applied(wait)))
                    }
                    Advance::Fail(message) => {
                        fail(format!("the selected item is {name:?}"), message)
                    }
                }
            }
        }
    }

    /// Asks to delete the selected entry. It sends `y` only when the dialog names exactly this
    /// entry and kind under the fixture root, every sentinel exists, and the fixture root still
    /// carries its ownership marker. Any mismatch fails the step and `y` is never sent.
    ///
    /// A scenario that disables the confirmation (`disable_delete_confirmation`) has no dialog:
    /// the step checks the selected-item panel, the disk, the sentinels, and the marker, sends
    /// Backspace alone, and fails if a dialog opens.
    fn delete(&mut self, delete: &Delete) -> Drive {
        let phase = match std::mem::take(&mut self.state) {
            State::Delete(phase) => phase,
            _ => DeletePhase::Start,
        };
        let bound = delete.timeout_ms;
        let expected = self.delete_expectation(delete);
        match phase {
            DeletePhase::Start => {
                if !self.fresh {
                    self.state = State::Delete(DeletePhase::Start);
                    return Drive::Barrier;
                }
                if self.plan.without_dialog {
                    return match self.verify_selection(delete) {
                        Ok(()) => {
                            self.state = State::Delete(DeletePhase::Begun(Wait::new(bound)));
                            Drive::Emit(vec![key_event(KeyCode::Backspace)])
                        }
                        Err(message) => fail(expected, message),
                    };
                }
                self.state = State::Delete(DeletePhase::Dialog(Wait::new(bound)));
                Drive::Emit(vec![key_event(KeyCode::Backspace)])
            }
            DeletePhase::Dialog(wait) => {
                if !self.fresh {
                    return self.barrier_for(wait, |wait| State::Delete(DeletePhase::Dialog(wait)));
                }
                let check = match self.snapshot().dialog() {
                    Some(dialog) if is_delete_dialog(&dialog) => Check::Ready(dialog),
                    Some(dialog) => Check::Fail(format!(
                        "no delete dialog opened; a `{}` dialog is open instead:\n{}",
                        dialog.title(),
                        dialog.lines().join("\n")
                    )),
                    None => Check::Pending("no dialog is open".to_owned()),
                };
                match self.advance(wait, check) {
                    Advance::Ready(dialog) => match self.verify_delete(&dialog, delete) {
                        Ok(()) => {
                            self.state = State::Delete(DeletePhase::Closed(Wait::new(bound)));
                            Drive::Emit(vec![match delete.confirm_with {
                                ConfirmKey::Y => char_event('y'),
                                ConfirmKey::Enter => key_event(KeyCode::Enter),
                            }])
                        }
                        Err(message) => fail(expected, message),
                    },
                    Advance::Again(wait) => {
                        self.barrier_for(wait, |wait| State::Delete(DeletePhase::Dialog(wait)))
                    }
                    Advance::Fail(message) => fail(expected, message),
                }
            }
            DeletePhase::Closed(wait) => {
                if !self.fresh {
                    return self.barrier_for(wait, |wait| State::Delete(DeletePhase::Closed(wait)));
                }
                let check = match self.snapshot().dialog() {
                    Some(dialog) if is_delete_dialog(&dialog) => Check::Pending(format!(
                        "the confirmation dialog is still open after `{}`",
                        delete.confirm_with
                    )),
                    _ => Check::Ready(()),
                };
                match self.advance(wait, check) {
                    Advance::Ready(()) => {
                        self.complete();
                        Drive::Next
                    }
                    Advance::Again(wait) => {
                        self.barrier_for(wait, |wait| State::Delete(DeletePhase::Closed(wait)))
                    }
                    Advance::Fail(message) => fail("the confirmation dialog closes", message),
                }
            }
            DeletePhase::Begun(wait) => self.delete_begun(wait, expected),
        }
    }

    /// What the delete step looks for, in words: the dialog it reads, or with no dialog the panel.
    fn delete_expectation(&self, delete: &Delete) -> String {
        if self.plan.without_dialog {
            format!(
                "the selected-item panel shows the {} {:?}, which Backspace alone deletes at {:?}, \
                 every sentinel exists, and the fixture root still carries its ownership marker",
                delete.kind,
                delete.name,
                delete.path.as_deref().unwrap_or(&delete.name)
            )
        } else {
            format!(
                "the delete dialog names {:?} ({}) under the fixture root, every sentinel exists, \
                 and the fixture root still carries its ownership marker",
                delete.name, delete.kind
            )
        }
    }

    /// After the Backspace of a deletion with no dialog: any dialog that is open means the program
    /// did not start the deletion, and none means it did.
    fn delete_begun(&mut self, wait: Wait, expected: String) -> Drive {
        if !self.fresh {
            return self.barrier_for(wait, |wait| State::Delete(DeletePhase::Begun(wait)));
        }
        match self.snapshot().dialog() {
            Some(dialog) if is_delete_dialog(&dialog) => fail(
                expected,
                format!(
                    "a confirmation dialog opened although the scenario disables confirmation, \
                     so the program did not honour the mode; no key was sent to confirm it:\n{}",
                    dialog.lines().join("\n")
                ),
            ),
            Some(dialog) => fail(
                expected,
                format!(
                    "the dialog `{}` opened instead of a deletion starting:\n{}",
                    dialog.title(),
                    dialog.lines().join("\n")
                ),
            ),
            None => {
                self.complete();
                Drive::Next
            }
        }
    }

    /// Checks the delete dialog against the step, then the sentinels, and last that the fixture
    /// root still carries its ownership marker, so that the confirming key follows the check at
    /// once.
    fn verify_delete(&self, dialog: &Panel, delete: &Delete) -> Result<(), String> {
        let shown_kind = match dialog.title() {
            "! DELETE FOLDER" => EntryKind::Folder,
            "! DELETE FILE" => EntryKind::File,
            other => {
                return Err(format!(
                    "the dialog `{other}` is not a folder or file deletion"
                ));
            }
        };
        if shown_kind != delete.kind {
            return Err(format!(
                "the dialog asks to delete a {shown_kind}, but the step expects a {}",
                delete.kind
            ));
        }
        let Some(shown) = dialog.lines().first() else {
            return Err("the dialog shows no path".to_owned());
        };
        check_target(shown, &self.root, &delete.name)?;
        // Where the entry is: the step's `path`, or `name` itself directly below the root. The
        // dialog's name alone would accept a same-named entry in another folder.
        let path = delete.path.as_deref().unwrap_or(&delete.name);
        check_target_path(shown, &self.root, path)?;
        for sentinel in &self.plan.sentinels {
            match entry_exists(&self.root, sentinel) {
                Ok(true) => {}
                Ok(false) => {
                    return Err(format!(
                        "the sentinel `{sentinel}` does not exist; refusing to confirm"
                    ));
                }
                Err(message) => return Err(message),
            }
        }
        self.verify_ownership("refusing to confirm: no confirming key was sent")
    }

    /// For a deletion no dialog confirms: checks the selected-item panel, the entry on disk, and
    /// the sentinels before Backspace, and last that the fixture root still carries its ownership
    /// marker. Nothing on screen shows where the selected entry lives, so that is the step's
    /// `path` (or its `name`, directly below the root), and the panel's name and kind prove it
    /// only when no other entry of the fixture has them.
    fn verify_selection(&self, delete: &Delete) -> Result<(), String> {
        let selected = self.snapshot().selected_item().ok_or_else(|| {
            "the selected-item panel shows no selection, or is not drawn at this terminal size"
                .to_owned()
        })?;
        if selected.name != delete.name {
            return Err(format!(
                "the selected-item panel shows {:?}, but the step deletes {:?}",
                selected.name, delete.name
            ));
        }
        if selected.kind != delete.kind.to_string() {
            return Err(format!(
                "the selected-item panel shows a {} named {:?}, but the step deletes a {}",
                selected.kind, delete.name, delete.kind
            ));
        }
        let relative = delete.path.as_deref().unwrap_or(&delete.name);
        match entry_metadata(&self.root, relative)? {
            Some(metadata) if metadata.is_dir() == (delete.kind == EntryKind::Folder) => {}
            Some(metadata) => {
                return Err(format!(
                    "`{relative}` is a {} on disk, but the step deletes a {}",
                    if metadata.is_dir() { "folder" } else { "file" },
                    delete.kind
                ));
            }
            None => return Err(format!("`{relative}` does not exist in the fixture")),
        }
        for sentinel in &self.plan.sentinels {
            if !entry_exists(&self.root, sentinel)? {
                return Err(format!(
                    "the sentinel `{sentinel}` does not exist; refusing to delete"
                ));
            }
            if sentinel == relative || sentinel.starts_with(&format!("{relative}/")) {
                return Err(format!(
                    "the target `{relative}` contains the sentinel `{sentinel}`, which must survive"
                ));
            }
        }
        // The panel shows a name and a kind only, so they must point at one entry in the fixture.
        FixtureRoot::open(&self.root)
            .map_err(|error| error.to_string())?
            .require_only_entry(&delete.name, delete.kind, relative)?;
        self.verify_ownership("refusing to delete: no key was sent")
    }

    /// Checks that the fixture root still carries its ownership marker, as the last check before
    /// the key that confirms a deletion or, with no dialog, starts it. A run verifies the marker
    /// once, when it starts, and the program under test can delete anything it shows, the marker
    /// included: without this, a deletion that follows one that removed or replaced the marker
    /// would go ahead in a directory the harness no longer owns. `unsent` says what the refusal
    /// keeps from being sent.
    fn verify_ownership(&self, unsent: &str) -> Result<(), String> {
        verify_owned(&self.root).map_err(|error| {
            format!(
                "the fixture root lost its `{MARKER_FILE_NAME}` ownership marker after the run \
                 started ({error}); {unsent}"
            )
        })
    }

    /// Asks to quit, waits for the ordinary quit prompt, and confirms it.
    fn quit(&mut self, quit: Quit) -> Drive {
        let phase = match std::mem::take(&mut self.state) {
            State::Quit(phase) => phase,
            _ => QuitPhase::Start,
        };
        match phase {
            QuitPhase::Start => {
                if !self.fresh {
                    self.state = State::Quit(QuitPhase::Start);
                    return Drive::Barrier;
                }
                self.state = State::Quit(QuitPhase::Prompt(Wait::new(quit.timeout_ms)));
                Drive::Emit(vec![char_event('q')])
            }
            QuitPhase::Prompt(wait) => {
                if !self.fresh {
                    return self.barrier_for(wait, |wait| State::Quit(QuitPhase::Prompt(wait)));
                }
                let check = match self.snapshot().dialog() {
                    Some(dialog) if dialog.title() == "QUIT" => {
                        if dialog.lines().iter().any(|line| line == "Quit Excise?") {
                            Check::Ready(())
                        } else {
                            Check::Fail(format!(
                                "the quit prompt reports work in flight, so the ordinary quit \
                                 does not apply:\n{}",
                                dialog.lines().join("\n")
                            ))
                        }
                    }
                    _ => Check::Pending("no quit prompt is open".to_owned()),
                };
                match self.advance(wait, check) {
                    Advance::Ready(()) => {
                        self.complete();
                        Drive::Emit(vec![char_event('y')])
                    }
                    Advance::Again(wait) => {
                        self.barrier_for(wait, |wait| State::Quit(QuitPhase::Prompt(wait)))
                    }
                    Advance::Fail(message) => fail("the quit prompt asks `Quit Excise?`", message),
                }
            }
        }
    }
}

impl InputSource for ScenarioInput {
    /// There is always something to deliver: the next event, a barrier, or the stop.
    fn poll(&mut self, _timeout: Duration) -> Result<bool, AppError> {
        Ok(true)
    }

    fn read(&mut self) -> Result<InputEvent, AppError> {
        if let Some(event) = self.queued.pop_front() {
            return Ok(self.deliver(event));
        }
        let plan = Rc::clone(&self.plan);
        loop {
            let cursor = self.progress.borrow().cursor;
            let Some(planned) = plan.steps.get(cursor) else {
                self.progress.borrow_mut().finished = true;
                return Err(stop_error());
            };
            match self.drive(planned) {
                Drive::Emit(events) => {
                    self.queued.extend(events);
                    if let Some(event) = self.queued.pop_front() {
                        return Ok(self.deliver(event));
                    }
                }
                Drive::Barrier => {
                    self.fresh = true;
                    self.settled = true;
                    return Ok(InputEvent::Barrier);
                }
                Drive::Next => {}
                Drive::Fail { expected, message } => {
                    self.record_failure(cursor, planned, expected, message);
                    return Err(stop_error());
                }
            }
        }
    }
}

// --- Events ---------------------------------------------------------------------------------

fn key_event(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

/// A typed character. A terminal reports an uppercase letter with Shift held.
fn char_event(character: char) -> Event {
    let modifiers = if character.is_uppercase() {
        KeyModifiers::SHIFT
    } else {
        KeyModifiers::NONE
    };
    Event::Key(KeyEvent::new(KeyCode::Char(character), modifiers))
}

fn press_event(press: PressKey) -> Event {
    let code = match press.key {
        KeyName::Char(character) => KeyCode::Char(character),
        KeyName::Enter => KeyCode::Enter,
        KeyName::Esc => KeyCode::Esc,
        KeyName::Backspace => KeyCode::Backspace,
        KeyName::Tab => KeyCode::Tab,
        KeyName::Up => KeyCode::Up,
        KeyName::Down => KeyCode::Down,
        KeyName::Left => KeyCode::Left,
        KeyName::Right => KeyCode::Right,
        KeyName::PageUp => KeyCode::PageUp,
        KeyName::PageDown => KeyCode::PageDown,
    };
    let mut modifiers = KeyModifiers::NONE;
    if press.ctrl {
        modifiers |= KeyModifiers::CONTROL;
    }
    if press.alt {
        modifiers |= KeyModifiers::ALT;
    }
    if matches!(press.key, KeyName::Char(character) if character.is_uppercase()) {
        modifiers |= KeyModifiers::SHIFT;
    }
    Event::Key(KeyEvent::new(code, modifiers))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use excise_harness::scenario::Scenario;

    use super::*;
    use crate::tests::fixtures::register_snapshot_root;
    use crate::tests::scenario_runner::fixture::materialize;
    use crate::tests::scenario_runner::plan::plan;

    #[test]
    fn a_mutation_that_opens_the_scenario_waits_for_a_barrier_before_it_lands() {
        let scenario = Scenario::from_toml_str(
            r#"
schema_version = 1
name = "mutation-first"
description = "A scenario whose first step changes the fixture."
fixture = "delete-file"
profiles = ["default"]

[[steps]]
step = "fs_mutate"
op = "appear"
path = "extra.bin"
"#,
        )
        .expect("the scenario should parse");
        let plan = Rc::new(plan(&scenario).expect("the scenario should plan"));
        let fixture = materialize("delete-file").expect("the fixture should build");
        let mut input = ScenarioInput::new(
            plan,
            SharedBackend::new(120, 40),
            fixture.root().to_path_buf(),
            std::path::PathBuf::new(),
            Limits::default(),
            Rc::new(RefCell::new(Progress::default())),
        );
        let landed = || fixture.root().join("extra.bin").exists();

        // Nothing was delivered yet, so the program may still be starting its scan: what it
        // gets first is a barrier, and the fixture is untouched.
        assert!(matches!(input.read(), Ok(InputEvent::Barrier)));
        assert!(!landed());
        // Once the barrier is through, the mutation lands and the scenario is over.
        assert!(input.read().is_err());
        assert!(landed());
    }

    /// The rows of a box `width` cells wide: a top border with its title tab, the `inner` lines,
    /// and a bottom border, which is how `Screen` reads a dialog.
    fn boxed(title: &str, inner: &[&str], width: usize) -> Vec<String> {
        let tab = format!(" {title} ");
        let fill = "▔".repeat(width - 2 - tab.chars().count());
        let mut rows = vec![format!("▟{tab}{fill}▜")];
        rows.extend(
            inner
                .iter()
                .map(|line| format!("▏{line:<w$}▕", w = width - 2)),
        );
        rows.push("▔".repeat(width));
        rows
    }

    /// The deletion dialog the program draws for `victim.bin` of the `delete-file` fixture.
    fn victim_dialog() -> Panel {
        let rows = boxed(
            "! DELETE FILE",
            &[
                "/tmp/excise_tests/delete-file/victim.bin",
                "[Enter/y] start",
            ],
            60,
        );
        Screen::from_lines(&rows.iter().map(String::as_str).collect::<Vec<_>>())
            .dialog()
            .expect("the box should read as a dialog")
    }

    #[test]
    fn the_ownership_marker_is_the_last_check_before_the_confirming_key() {
        let scenario = Scenario::from_toml_str(
            r#"
schema_version = 1
name = "delete-the-victim"
description = "A scenario whose delete step is checked without running the program."
fixture = "delete-file"
sentinels = ["keep-a.bin"]
profiles = ["default"]

[[steps]]
step = "delete"
name = "victim.bin"
kind = "file"
"#,
        )
        .expect("the scenario should parse");
        let Step::Delete(delete) = &scenario.steps[0] else {
            panic!("the scenario's only step deletes");
        };
        let fixture = materialize("delete-file").expect("the fixture should build");
        // The dialog's path reads as the program draws it, under the registered snapshot root.
        register_snapshot_root(fixture.root(), "delete-file");
        let input = ScenarioInput::new(
            Rc::new(plan(&scenario).expect("the scenario should plan")),
            SharedBackend::new(120, 40),
            fixture.root().to_path_buf(),
            PathBuf::new(),
            Limits::default(),
            Rc::new(RefCell::new(Progress::default())),
        );
        let dialog = victim_dialog();
        let marker = fixture.root().join(MARKER_FILE_NAME);

        // The dialog names the victim and the sentinel is there, so the marker is all that is
        // left to check.
        assert_eq!(input.verify_delete(&dialog, delete), Ok(()));

        fs::remove_file(&marker).expect("the marker should go");
        let refusal = input
            .verify_delete(&dialog, delete)
            .expect_err("a root that lost its marker must not be confirmed");
        assert!(refusal.contains(MARKER_FILE_NAME), "{refusal}");
        assert!(
            refusal.contains("no confirming key was sent"),
            "the refusal should say that no key was sent: {refusal}"
        );

        // A directory in its place is no marker either.
        fs::create_dir(&marker).expect("a directory should take its place");
        assert!(input.verify_delete(&dialog, delete).is_err());

        // With a marker again, the very same dialog is accepted: only the marker stood in the way.
        fs::remove_dir(&marker).expect("the directory should go");
        fs::write(&marker, b"owned").expect("the marker should come back");
        assert_eq!(input.verify_delete(&dialog, delete), Ok(()));
    }
}
