//! Preflight: everything that can be checked before the program starts.
//!
//! A scenario that passes [`Scenario::validate`] can still be unrunnable: a regular expression may
//! not compile, a key may have no terminal encoding, a step may need something this runner does not
//! do. [`prepare`] finds all of that before the first process is spawned, so a bad scenario is a
//! harness error at once and not a failure halfway through a run. It also compiles each step into
//! the form the executor uses.

use std::fmt::Write as _;

use regex::Regex;

use crate::{
    pty::{
        input::InputScan,
        keys::{encode_key, encode_text},
    },
    scenario::{Region, Scenario, Signal, Step, WaitText},
};

use super::{
    budget::{LatencyScale, limit_for},
    outcome::RunError,
};

/// A text a step looks for.
#[derive(Debug, Clone)]
pub(crate) enum Matcher {
    /// Literal text.
    Literal(String),
    /// A compiled regular expression.
    Pattern(Regex),
}

impl Matcher {
    pub(crate) fn is_match(&self, text: &str) -> bool {
        match self {
            Self::Literal(literal) => text.contains(literal.as_str()),
            Self::Pattern(pattern) => pattern.is_match(text),
        }
    }

    pub(crate) fn describe(&self) -> String {
        match self {
            Self::Literal(literal) => format!("{literal:?}"),
            Self::Pattern(pattern) => format!("/{pattern}/"),
        }
    }
}

/// A step in the form the executor uses.
#[derive(Debug, Clone)]
pub(crate) enum Prepared {
    /// Nothing to prepare.
    Nothing,
    /// The text `wait_text` looks for.
    Text(Matcher),
    /// The checks of an `expect_screen`.
    Screen {
        contains: Vec<Matcher>,
        not_contains: Vec<Matcher>,
        patterns: Vec<Matcher>,
    },
    /// The bytes of a `key` press.
    Key(Vec<u8>),
    /// The bytes of each character of a `type` or `select` name.
    Typed(Vec<Vec<u8>>),
}

/// Checks the scenario's steps and compiles them, one entry per step.
///
/// # Errors
///
/// Returns the first thing that makes the scenario unrunnable.
pub(crate) fn prepare(scenario: &Scenario) -> Result<Vec<Prepared>, RunError> {
    scenario
        .steps
        .iter()
        .enumerate()
        .map(|(index, step)| prepare_step(scenario, index, step))
        .collect()
}

/// The index of the first step of `scenario` whose bytes the program can read as a request for a
/// deletion outside a `delete` step, if there is one: a `key` that encodes to Backspace
/// (`backspace`, `alt+backspace`, `ctrl+?`, and `ctrl+h`, which the Windows console path may read
/// as one), or `key` and `type` steps that compose an escape sequence between them (`alt+[` and
/// the text `127u`, or `esc` and `[127u`), which the program joins across writes and which may
/// be that Backspace, a confirmation, or any other key ([`InputScan`] says which bytes count).
///
/// It is the static counterpart of what the executor notes while it runs
/// (`Live::note_raw_write`): the same scan over the bytes the steps would write, in step order,
/// whether or not the run would get that far. It does not see the writes of the protocols (the
/// `/` of a `select`, the `q` of a `quit`), which can only end an escape sequence that a key
/// began, so it never engages a run later than the executor does. A run that makes such a request
/// is engaged: every write that could confirm a deletion needs an exact screen (see "Raw deletion
/// requests" in `runner::live`). Where the screen is not exact the pseudo-terminal runner skips a
/// scenario that has one, as it skips one with a `delete` step. A key that has no encoding asks
/// for nothing here: the run would end in an error before it was written.
pub(crate) fn raw_deletion_request(scenario: &Scenario) -> Option<usize> {
    let mut scan = InputScan::default();
    scenario.steps.iter().position(|step| match step {
        Step::Key(press) => encode_key(press.key, press.ctrl, press.alt)
            .is_ok_and(|bytes| scan.note(&bytes).request),
        Step::Type(typed) => {
            encode_text(&typed.text).is_ok_and(|keys| keys.iter().any(|key| scan.note(key).request))
        }
        _ => false,
    })
}

#[allow(clippy::too_many_lines)]
fn prepare_step(scenario: &Scenario, index: usize, step: &Step) -> Result<Prepared, RunError> {
    let invalid = |reason: String| RunError::InvalidStep {
        index,
        step: step.kind(),
        reason,
    };
    match step {
        Step::WaitText(WaitText { text, regex, .. }) => {
            let matcher = match (text, regex) {
                (Some(text), None) => Matcher::Literal(text.clone()),
                (None, Some(pattern)) => Matcher::Pattern(compile(index, pattern)?),
                _ => {
                    return Err(invalid(
                        "needs exactly one of `text` and `regex`".to_owned(),
                    ));
                }
            };
            Ok(Prepared::Text(matcher))
        }
        Step::ExpectScreen(expect) => Ok(Prepared::Screen {
            contains: expect
                .contains
                .iter()
                .cloned()
                .map(Matcher::Literal)
                .collect(),
            not_contains: expect
                .not_contains
                .iter()
                .cloned()
                .map(Matcher::Literal)
                .collect(),
            patterns: expect
                .regex
                .iter()
                .map(|pattern| compile(index, pattern).map(Matcher::Pattern))
                .collect::<Result<_, _>>()?,
        }),
        Step::Key(press) => encode_key(press.key, press.ctrl, press.alt)
            .map(Prepared::Key)
            .map_err(|error| invalid(error.to_string())),
        Step::Type(typed) => encode_text(&typed.text)
            .map(Prepared::Typed)
            .map_err(|error| invalid(error.to_string())),
        Step::Select(select) => {
            if select.name.contains(['*', '?', '[', '{']) {
                return Err(invalid(format!(
                    "the name {:?} contains glob syntax, which the filter would interpret \
                     instead of matching literally",
                    select.name
                )));
            }
            encode_text(&select.name)
                .map(Prepared::Typed)
                .map_err(|error| invalid(error.to_string()))
        }
        Step::Signal(send) => {
            supported_signal(send.signal).map_err(|reason| RunError::Unsupported {
                index,
                step: step.kind(),
                reason,
            })?;
            Ok(Prepared::Nothing)
        }
        Step::ExpectBudget(expect) => {
            if limit_for(scenario, expect.budget, LatencyScale::STRICT).is_none() {
                return Err(invalid(format!(
                    "budget `{}` has no default limit; set one in `[budgets]`",
                    expect.budget
                )));
            }
            Ok(Prepared::Nothing)
        }
        Step::ExpectExit(exit) => {
            let console_closed = scenario.steps.iter().take(index).any(
                |earlier| matches!(earlier, Step::Signal(send) if send.signal == Signal::Close),
            );
            if console_closed && !exit.terminal_restored {
                return Err(invalid(
                    "a `close` event before it leaves no console to inspect, so \
                     `terminal_restored = false` could never be checked; after a `close` the \
                     runner accepts `terminal_restored = true` without evaluating it"
                        .to_owned(),
                ));
            }
            Ok(Prepared::Nothing)
        }
        Step::WaitHeader(_)
        | Step::WaitEvent(_)
        | Step::Delete(_)
        | Step::WaitFsAbsent(_)
        | Step::WaitFsPresent(_)
        | Step::Resize(_)
        | Step::ExpectFs(_)
        | Step::ExpectConfig(_)
        | Step::Measure(_)
        | Step::Idle(_)
        | Step::Settle(_)
        | Step::WaitRefresh(_)
        | Step::Quit(_)
        | Step::FsMutate(_) => Ok(Prepared::Nothing),
    }
}

fn compile(index: usize, pattern: &str) -> Result<Regex, RunError> {
    Regex::new(pattern).map_err(|error| RunError::InvalidPattern {
        index,
        pattern: pattern.to_owned(),
        reason: error.to_string(),
    })
}

/// Whether this platform can deliver `signal`; the reason if it cannot.
fn supported_signal(signal: Signal) -> Result<(), String> {
    match signal {
        Signal::Term | Signal::Hup | Signal::Quit | Signal::Int if cfg!(unix) => Ok(()),
        Signal::Term | Signal::Hup | Signal::Quit | Signal::Int => Err(format!(
            "the `{signal}` signal is a Unix signal and this platform has no safe way to deliver it \
             to a pseudo-terminal child"
        )),
        // `close` needs no unsafe call: it closes the pseudo console, which Windows itself
        // delivers as `CTRL_CLOSE_EVENT` to every attached process (`PtySession::close_console`).
        Signal::Close if cfg!(windows) => Ok(()),
        Signal::Close => Err(format!(
            "the `{signal}` console event is a Windows event; this platform has no equivalent"
        )),
        Signal::Break => Err(format!(
            "the `{signal}` console event needs unsafe Windows console calls, which this \
             workspace does not allow"
        )),
    }
}

/// A one-line description of `step`, for failure messages.
pub(crate) fn describe_step(step: &Step) -> String {
    match step {
        Step::WaitText(wait) => {
            let what = wait
                .text
                .as_ref()
                .map(|text| format!("{text:?}"))
                .or_else(|| wait.regex.as_ref().map(|pattern| format!("/{pattern}/")))
                .unwrap_or_default();
            format!("wait_text {what}{}", describe_region(wait.region))
        }
        Step::WaitHeader(wait) => format!("wait_header {}", wait.state),
        Step::WaitEvent(wait) => {
            let mut text = format!("wait_event {}", wait.event);
            for (field, test) in &wait.fields {
                let _ = write!(text, " {field} {test:?}");
            }
            text
        }
        Step::Key(press) => {
            let mut text = format!("key {}", press.key);
            if press.ctrl {
                text.push_str(" +ctrl");
            }
            if press.alt {
                text.push_str(" +alt");
            }
            text
        }
        Step::Type(typed) => format!("type {:?}", typed.text),
        Step::Select(select) => format!("select {:?}", select.name),
        Step::Delete(delete) => format!("delete {:?} ({})", delete.name, delete.kind),
        Step::WaitFsAbsent(wait) => format!("wait_fs_absent {:?}", wait.path),
        Step::WaitFsPresent(wait) => format!("wait_fs_present {:?}", wait.path),
        Step::FsMutate(mutate) => format!("fs_mutate {} {:?}", mutate.op, mutate.path),
        Step::Resize(resize) => format!("resize {}x{}", resize.cols, resize.rows),
        Step::Signal(send) => format!("signal {}", send.signal),
        Step::ExpectScreen(expect) => format!("expect_screen{}", describe_region(expect.region)),
        Step::ExpectFs(_) => "expect_fs".to_owned(),
        Step::ExpectConfig(config) => {
            format!("expect_config {} equals {:?}", config.key, config.equals)
        }
        Step::ExpectExit(exit) => format!(
            "expect_exit code {} terminal_restored {}",
            exit.code, exit.terminal_restored
        ),
        Step::ExpectBudget(expect) => {
            format!("expect_budget {} of {}", expect.budget, expect.metric)
        }
        Step::Measure(measure) => format!("measure {} {}", measure.marker, measure.name),
        Step::Idle(idle) => format!("idle after {} ms for {} ms", idle.after_ms, idle.window_ms),
        Step::Settle(_) => "settle".to_owned(),
        Step::WaitRefresh(_) => "wait_refresh".to_owned(),
        Step::Quit(_) => "quit".to_owned(),
    }
}

/// ` in <region>` for a step limited to a region, or nothing.
pub(crate) fn describe_region(region: Option<Region>) -> String {
    match region {
        None => String::new(),
        Some(Region::Header) => " in the header".to_owned(),
        Some(Region::Dialog) => " in the dialog".to_owned(),
        Some(Region::Rows([first, last])) => format!(" in rows {first}-{last}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scenario(steps: &str) -> Scenario {
        let scenario = Scenario::from_toml_str(&format!(
            r#"
schema_version = 1
name = "plan"
description = "d"
fixture = "f"
sentinels = ["keep"]
profiles = ["default"]
{steps}
"#
        ))
        .expect("a scenario");
        scenario.validate().expect("a valid scenario");
        scenario
    }

    #[test]
    fn a_runnable_scenario_prepares_one_entry_per_step() {
        let scenario = scenario(
            r#"
[[steps]]
step = "wait_text"
regex = "^ EXCISE"
[[steps]]
step = "key"
key = "c"
ctrl = true
[[steps]]
step = "type"
text = "ab"
[[steps]]
step = "expect_screen"
contains = ["x"]
regex = ["a.c"]
"#,
        );

        let prepared = prepare(&scenario).expect("preflight");

        assert_eq!(prepared.len(), 4);
        assert!(matches!(&prepared[0], Prepared::Text(Matcher::Pattern(_))));
        assert!(matches!(&prepared[1], Prepared::Key(bytes) if bytes == &[0x03]));
        assert!(matches!(&prepared[2], Prepared::Typed(keys) if keys.len() == 2));
        assert!(matches!(&prepared[3], Prepared::Screen { patterns, .. } if patterns.len() == 1));
    }

    #[test]
    fn a_pattern_that_does_not_compile_is_reported_with_its_step() {
        let scenario = scenario(
            r#"
[[steps]]
step = "settle"
[[steps]]
step = "wait_text"
regex = "(unclosed"
"#,
        );

        let error = prepare(&scenario).expect_err("the pattern is malformed");

        assert!(
            matches!(error, RunError::InvalidPattern { index: 1, .. }),
            "{error}"
        );
    }

    #[test]
    fn a_key_with_no_terminal_encoding_is_a_preflight_error() {
        let scenario = scenario("[[steps]]\nstep = \"key\"\nkey = \"enter\"\nctrl = true\n");

        assert!(matches!(
            prepare(&scenario),
            Err(RunError::InvalidStep {
                index: 0,
                step: "key",
                ..
            })
        ));
    }

    #[test]
    fn typed_text_with_a_control_character_is_a_preflight_error() {
        let scenario = scenario("[[steps]]\nstep = \"type\"\ntext = \"a\\nb\"\n");

        assert!(matches!(
            prepare(&scenario),
            Err(RunError::InvalidStep { step: "type", .. })
        ));
    }

    #[test]
    fn a_selected_name_with_glob_syntax_is_refused_before_anything_runs() {
        for name in ["*.log", "a?b", "[abc]", "{a,b}"] {
            let scenario = scenario(&format!(
                "[[steps]]\nstep = \"select\"\nname = \"{name}\"\n"
            ));

            assert!(
                matches!(
                    prepare(&scenario),
                    Err(RunError::InvalidStep { step: "select", .. })
                ),
                "{name}"
            );
        }
    }

    #[test]
    fn break_is_always_unsupported_and_close_and_unix_signals_follow_the_platform() {
        let brk = scenario("[[steps]]\nstep = \"signal\"\nsignal = \"break\"\n");
        assert!(matches!(prepare(&brk), Err(RunError::Unsupported { .. })));

        let close = scenario("[[steps]]\nstep = \"signal\"\nsignal = \"close\"\n");
        assert_eq!(prepare(&close).is_ok(), cfg!(windows));

        let term = scenario("[[steps]]\nstep = \"signal\"\nsignal = \"term\"\n");
        assert_eq!(prepare(&term).is_ok(), cfg!(unix));
    }

    #[test]
    fn after_a_close_only_a_terminal_check_that_is_skipped_anyway_is_accepted() {
        let close_then_exit = |restored: bool| {
            scenario(&format!(
                "[[steps]]\nstep = \"signal\"\nsignal = \"close\"\n[[steps]]\nstep = \
                 \"expect_exit\"\ncode = 130\nterminal_restored = {restored}\nresidue = \"none\"\n"
            ))
        };

        if cfg!(windows) {
            assert!(prepare(&close_then_exit(true)).is_ok());
            assert!(matches!(
                prepare(&close_then_exit(false)),
                Err(RunError::InvalidStep {
                    index: 1,
                    step: "expect_exit",
                    ..
                })
            ));
        } else {
            // `close` is refused first wherever it cannot be delivered.
            assert!(matches!(
                prepare(&close_then_exit(false)),
                Err(RunError::Unsupported { index: 0, .. })
            ));
        }

        let exit_only = scenario(
            "[[steps]]\nstep = \"expect_exit\"\ncode = 0\nterminal_restored = false\nresidue = \"none\"\n",
        );
        assert!(prepare(&exit_only).is_ok(), "without a close it is checked");
    }

    #[test]
    fn a_budget_without_a_limit_is_refused_unless_the_scenario_sets_one() {
        let without = scenario(
            "[[steps]]\nstep = \"expect_budget\"\nbudget = \"threads\"\nmetric = \"threads\"\n",
        );
        assert!(matches!(
            prepare(&without),
            Err(RunError::InvalidStep {
                step: "expect_budget",
                ..
            })
        ));

        let with = scenario(
            "[budgets]\nthreads = 12\n[[steps]]\nstep = \"expect_budget\"\nbudget = \"threads\"\nmetric = \"threads\"\n",
        );
        assert!(prepare(&with).is_ok());
    }

    #[test]
    fn steps_describe_themselves_for_failure_messages() {
        let scenario = scenario(
            r#"
[[steps]]
step = "wait_text"
text = "Quit"
region = "dialog"
[[steps]]
step = "delete"
name = "victim"
kind = "folder"
[[steps]]
step = "wait_event"
event = "frame"
fields = { inputs = { min = 3 } }
"#,
        );

        let descriptions: Vec<String> = scenario.steps.iter().map(describe_step).collect();

        assert_eq!(
            descriptions,
            [
                "wait_text \"Quit\" in the dialog",
                "delete \"victim\" (folder)",
                "wait_event frame inputs Min(3)",
            ]
        );
    }

    #[test]
    fn the_keys_a_program_reads_as_backspace_are_raw_deletion_requests() {
        for key in [
            "key = \"backspace\"",
            "key = \"backspace\"\nalt = true",
            "key = \"h\"\nctrl = true",
        ] {
            let scenario = scenario(&format!(
                "[[steps]]\nstep = \"settle\"\n[[steps]]\nstep = \"key\"\n{key}\n"
            ));

            assert_eq!(raw_deletion_request(&scenario), Some(1), "{key}");
        }
    }

    #[test]
    fn a_sequence_that_a_key_begins_and_the_text_after_it_finishes_is_a_raw_deletion_request() {
        // The program joins the bytes of an escape sequence across writes. `alt+[` is `ESC [`, and
        // the text that follows finishes `ESC [ 127 u` (Backspace), `ESC [ 121 u` (`y`),
        // `ESC [ 13 u` and `ESC [ 57414 u` (Enter), or `ESC [ 97:121 ; 2 u` (`y` again), with no
        // Backspace, `y`, or Enter byte in any write: the step whose bytes continue the sequence
        // is the one that asks, whatever they are.
        for text in ["127u", "121u", "13u", "57414u", "97:121;2u", "1"] {
            let scenario = scenario(&format!(
                "[[steps]]\nstep = \"key\"\nkey = \"[\"\nalt = true\n\
                 [[steps]]\nstep = \"type\"\ntext = \"{text}\"\n"
            ));

            assert_eq!(raw_deletion_request(&scenario), Some(1), "alt+[ and {text}");
        }
    }

    #[test]
    fn a_lone_escape_that_the_next_write_turns_into_a_sequence_is_a_raw_deletion_request() {
        // `esc` and then `[121u` are `ESC` and `[` `1` `2` `1` `u`: a program that reads the
        // first two bytes together reads `ESC [`, and the rest finishes `ESC [ 121 u`.
        for (what, steps) in [
            (
                "esc, then text",
                "[[steps]]\nstep = \"key\"\nkey = \"esc\"\n\
                 [[steps]]\nstep = \"type\"\ntext = \"[121u\"\n",
            ),
            (
                "esc, then `[`",
                "[[steps]]\nstep = \"key\"\nkey = \"esc\"\n\
                 [[steps]]\nstep = \"key\"\nkey = \"[\"\n",
            ),
            (
                "alt+O, then a key",
                "[[steps]]\nstep = \"key\"\nkey = \"O\"\nalt = true\n\
                 [[steps]]\nstep = \"key\"\nkey = \"x\"\n",
            ),
            (
                "a sequence cut in three",
                "[[steps]]\nstep = \"key\"\nkey = \"[\"\nalt = true\n\
                 [[steps]]\nstep = \"type\"\ntext = \"12\"\n\
                 [[steps]]\nstep = \"type\"\ntext = \"1u\"\n",
            ),
        ] {
            assert_eq!(
                raw_deletion_request(&scenario(steps)),
                Some(1),
                "{what}: the second step continues it"
            );
        }
    }

    #[test]
    fn a_protocol_step_between_the_writes_does_not_hide_a_sequence_from_the_plan() {
        // The `/` of a `select` ends the sequence for the program that runs, so the executor does
        // not engage here. The plan reads only the steps' own writes and so does: where it skips a
        // scenario that the executor would have run, it errs towards the guard.
        let scenario = scenario(
            "[[steps]]\nstep = \"key\"\nkey = \"[\"\nalt = true\n\
             [[steps]]\nstep = \"select\"\nname = \"victim\"\n\
             [[steps]]\nstep = \"type\"\ntext = \"121u\"\n",
        );

        assert_eq!(raw_deletion_request(&scenario), Some(2));
    }

    #[test]
    fn keys_that_cannot_be_read_as_backspace_ask_for_no_deletion() {
        // Nor does a `delete` step: its Backspace is the protocol's, which verifies the dialog. A
        // key that begins and finishes a sequence of its own (an arrow, a page key) asks for
        // nothing, and neither does one that begins a sequence which no write continues.
        let scenario = scenario(
            r#"
[[steps]]
step = "key"
key = "y"
[[steps]]
step = "key"
key = "enter"
[[steps]]
step = "key"
key = "esc"
[[steps]]
step = "key"
key = "tab"
[[steps]]
step = "key"
key = "up"
[[steps]]
step = "key"
key = "x"
alt = true
[[steps]]
step = "key"
key = "page_down"
[[steps]]
step = "key"
key = "esc"
[[steps]]
step = "key"
key = "x"
[[steps]]
step = "key"
key = "c"
ctrl = true
[[steps]]
step = "type"
text = "backspace [127u]"
[[steps]]
step = "key"
key = "["
alt = true
[[steps]]
step = "select"
name = "victim"
[[steps]]
step = "delete"
name = "victim"
kind = "file"
[[steps]]
step = "quit"
"#,
        );

        assert_eq!(raw_deletion_request(&scenario), None);
    }
}
