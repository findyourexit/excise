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
    pty::keys::{encode_key, encode_text},
    scenario::{Region, Scenario, Signal, Step, WaitText},
};

use super::{budget::limit_for, outcome::RunError};

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
            if limit_for(scenario, expect.budget).is_none() {
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
        | Step::Measure(_)
        | Step::Idle(_)
        | Step::Settle(_)
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
}
