//! Semantic validation: each rule has a failing case that asserts the typed error.

use std::collections::BTreeMap;

use super::{ALL_STEPS, errors_of, parse, step_errors, valid};
use crate::scenario::{
    Budget, Comparison, ConfirmKey, DEFAULT_TIMEOUT_MS, Delete, DeleteWait, EntryKind, EventField,
    EventKind, Expect, ExpectBudget, ExpectConfig, ExpectExit, ExpectFs, ExpectScreen, Field,
    FsMutate, Idle, MAX_TIMEOUT_MS, Marker, Measure, MutateOp, PathViolation, Profile, Quit,
    Region, Residue, Resize, ScanState, Scenario, Select, Settle, Step, StepError, TypeText,
    ValidationError, WaitEvent, WaitFs, WaitHeader, WaitRefresh, WaitText,
    check_fixture_relative_path,
};

fn wait_text(text: Option<&str>, regex: Option<&str>, region: Option<Region>) -> Step {
    Step::WaitText(WaitText {
        text: text.map(str::to_owned),
        regex: regex.map(str::to_owned),
        region,
        timeout_ms: DEFAULT_TIMEOUT_MS,
    })
}

fn expect_screen(contains: &[&str], not_contains: &[&str], regex: &[&str]) -> ExpectScreen {
    let owned = |list: &[&str]| list.iter().map(|&text| text.to_owned()).collect();
    ExpectScreen {
        contains: owned(contains),
        not_contains: owned(not_contains),
        regex: owned(regex),
        region: None,
    }
}

fn measure(name: &str, marker: Marker) -> Step {
    Step::Measure(Measure {
        name: name.to_owned(),
        marker,
    })
}

/// A scenario with `steps` and every rule about the rest of the scenario satisfied.
fn with_steps(steps: Vec<Step>) -> Scenario {
    let mut scenario = valid();
    scenario.steps = steps;
    scenario
}

/// Every step that waits, each with `timeout_ms`.
fn waiting_steps(timeout_ms: u64) -> Vec<Step> {
    vec![
        wait_text_with(timeout_ms),
        Step::WaitHeader(WaitHeader {
            state: ScanState::Complete,
            timeout_ms,
        }),
        Step::WaitEvent(WaitEvent {
            event: EventKind::Exit,
            fields: BTreeMap::new(),
            timeout_ms,
        }),
        Step::Select(Select {
            name: "victim".to_owned(),
            timeout_ms,
        }),
        Step::Delete(Delete {
            name: "victim".to_owned(),
            kind: EntryKind::Folder,
            path: None,
            confirm_with: ConfirmKey::Y,
            wait_for: DeleteWait::Finished,
            timeout_ms,
        }),
        Step::WaitRefresh(WaitRefresh { timeout_ms }),
        Step::WaitFsAbsent(WaitFs {
            path: "victim".to_owned(),
            timeout_ms,
        }),
        Step::WaitFsPresent(WaitFs {
            path: "keep-a.bin".to_owned(),
            timeout_ms,
        }),
        Step::ExpectExit(ExpectExit {
            code: 0,
            terminal_restored: true,
            residue: Residue::None,
            timeout_ms,
        }),
        Step::Settle(Settle { timeout_ms }),
        Step::Quit(Quit { timeout_ms }),
    ]
}

fn wait_text_with(timeout_ms: u64) -> Step {
    Step::WaitText(WaitText {
        text: Some("COMPLETE".to_owned()),
        regex: None,
        region: None,
        timeout_ms,
    })
}

#[test]
fn the_base_and_full_scenarios_satisfy_every_rule() {
    assert_eq!(errors_of(&valid()), []);
    assert_eq!(errors_of(&parse(ALL_STEPS)), []);
}

#[test]
fn an_unsupported_schema_version_is_rejected() {
    for found in [0, 2, u32::MAX] {
        let mut scenario = valid();
        scenario.schema_version = found;

        assert_eq!(
            errors_of(&scenario),
            [ValidationError::UnsupportedSchemaVersion { found }]
        );
    }
}

#[test]
fn names_and_fixtures_must_be_identifiers() {
    let too_long = "a".repeat(65);
    let longest = "a".repeat(64);
    for bad in [
        "",
        "Upper",
        "has space",
        "-leading",
        "_leading",
        "a/b",
        "a\\b",
        "a.b",
        "../x",
        "ünï",
        &too_long,
    ] {
        let mut scenario = valid();
        scenario.name = bad.to_owned();
        scenario.fixture = bad.to_owned();

        assert_eq!(
            errors_of(&scenario),
            [
                ValidationError::InvalidIdentifier {
                    field: "name",
                    value: bad.to_owned()
                },
                ValidationError::InvalidIdentifier {
                    field: "fixture",
                    value: bad.to_owned()
                },
            ],
            "{bad:?} is not an identifier"
        );
    }
    for good in ["a", "0", "delete-folder_2", "9-lives", longest.as_str()] {
        let mut scenario = valid();
        scenario.name = good.to_owned();
        scenario.fixture = good.to_owned();

        assert_eq!(errors_of(&scenario), [], "{good:?} is an identifier");
    }
}

#[test]
fn profiles_must_not_be_empty() {
    let mut scenario = valid();
    scenario.profiles.clear();

    assert_eq!(errors_of(&scenario), [ValidationError::NoProfiles]);
}

#[test]
fn profiles_must_be_unique() {
    let mut scenario = valid();
    scenario.profiles = vec![Profile::Default, Profile::Deterministic, Profile::Default];

    assert_eq!(
        errors_of(&scenario),
        [ValidationError::DuplicateProfile {
            profile: Profile::Default
        }]
    );

    scenario.profiles = Profile::ALL.to_vec();
    assert_eq!(
        errors_of(&scenario),
        [],
        "all five distinct profiles are fine"
    );
}

#[test]
fn the_initial_terminal_must_be_at_least_32_by_8() {
    for (cols, rows, accepted) in [
        (32, 8, true),
        (120, 40, true),
        (31, 8, false),
        (32, 7, false),
        (31, 7, false),
        (0, 0, false),
        (200, 1, false),
        (1, 200, false),
    ] {
        let mut scenario = valid();
        scenario.terminal.cols = cols;
        scenario.terminal.rows = rows;
        let expected = if accepted {
            Vec::new()
        } else {
            vec![ValidationError::TerminalTooSmall { cols, rows }]
        };

        assert_eq!(errors_of(&scenario), expected, "{cols}x{rows}");
    }
}

#[test]
fn a_scenario_needs_at_least_one_step() {
    let scenario = with_steps(Vec::new());

    assert_eq!(errors_of(&scenario), [ValidationError::NoSteps]);
}

#[test]
fn an_expected_failure_requires_a_slice() {
    let mut scenario = valid();
    scenario.expect = Expect::Fail;

    assert_eq!(
        errors_of(&scenario),
        [ValidationError::ExpectedFailureWithoutSlice]
    );

    scenario.slice = Some("X2".to_owned());
    assert_eq!(errors_of(&scenario), []);

    scenario.expect = Expect::Pass;
    scenario.slice = None;
    assert_eq!(
        errors_of(&scenario),
        [],
        "a passing scenario needs no slice"
    );
}

#[test]
fn a_slice_must_be_shaped_like_a_slice_id() {
    for bad in ["", "x2", "2X", "X-2", "X 2", "X_2", "TOOLONGSLICE", "É1"] {
        for expect in [Expect::Pass, Expect::Fail] {
            let mut scenario = valid();
            scenario.expect = expect;
            scenario.slice = Some(bad.to_owned());

            assert_eq!(
                errors_of(&scenario),
                [ValidationError::InvalidSlice {
                    value: bad.to_owned()
                }],
                "{bad:?} with expect = {expect}"
            );
        }
    }
    for good in ["H1", "X2", "R10", "REL", "W3", "A"] {
        let mut scenario = valid();
        scenario.expect = Expect::Fail;
        scenario.slice = Some(good.to_owned());

        assert_eq!(errors_of(&scenario), [], "{good:?}");
    }
}

#[test]
fn fails_on_requires_an_expected_failure() {
    let mut scenario = valid();
    scenario.fails_on = Some(vec!["linux".to_owned()]);

    assert_eq!(
        errors_of(&scenario),
        [ValidationError::FailsOnWithoutExpectedFailure]
    );

    scenario.expect = Expect::Fail;
    scenario.slice = Some("X2".to_owned());
    assert_eq!(errors_of(&scenario), []);
}

#[test]
fn platforms_and_fails_on_must_not_be_explicitly_empty() {
    let mut scenario = valid();
    scenario.platforms = Some(Vec::new());

    assert_eq!(
        errors_of(&scenario),
        [ValidationError::EmptyPlatformList { field: "platforms" }]
    );

    scenario.platforms = None;
    scenario.expect = Expect::Fail;
    scenario.slice = Some("X2".to_owned());
    scenario.fails_on = Some(Vec::new());
    assert_eq!(
        errors_of(&scenario),
        [ValidationError::EmptyPlatformList { field: "fails_on" }]
    );
}

#[test]
fn platform_names_must_be_known() {
    let mut scenario = valid();
    scenario.platforms = Some(vec!["linux".to_owned(), "plan9".to_owned()]);

    assert_eq!(
        errors_of(&scenario),
        [ValidationError::UnknownPlatform {
            field: Field::at("platforms", 1),
            value: "plan9".to_owned()
        }]
    );

    scenario.platforms = None;
    scenario.expect = Expect::Fail;
    scenario.slice = Some("X2".to_owned());
    scenario.fails_on = Some(vec!["plan9".to_owned()]);
    assert_eq!(
        errors_of(&scenario),
        [ValidationError::UnknownPlatform {
            field: Field::at("fails_on", 0),
            value: "plan9".to_owned()
        }],
        "an unknown name in `fails_on` is not also reported as outside `platforms`"
    );
}

#[test]
fn platform_names_must_not_repeat() {
    let mut scenario = valid();
    scenario.platforms = Some(vec!["linux".to_owned(), "linux".to_owned()]);

    assert_eq!(
        errors_of(&scenario),
        [ValidationError::DuplicatePlatform {
            field: Field::at("platforms", 1),
            value: "linux".to_owned()
        }]
    );

    scenario.platforms = None;
    scenario.expect = Expect::Fail;
    scenario.slice = Some("X2".to_owned());
    scenario.fails_on = Some(vec!["linux".to_owned(), "linux".to_owned()]);
    assert_eq!(
        errors_of(&scenario),
        [ValidationError::DuplicatePlatform {
            field: Field::at("fails_on", 1),
            value: "linux".to_owned()
        }]
    );
}

#[test]
fn fails_on_names_must_be_within_platforms() {
    let mut scenario = valid();
    scenario.platforms = Some(vec!["linux".to_owned()]);
    scenario.expect = Expect::Fail;
    scenario.slice = Some("X2".to_owned());
    scenario.fails_on = Some(vec!["macos".to_owned()]);

    assert_eq!(
        errors_of(&scenario),
        [ValidationError::FailsOnOutsidePlatforms {
            field: Field::at("fails_on", 0),
            value: "macos".to_owned()
        }]
    );

    scenario.fails_on = Some(vec!["linux".to_owned()]);
    assert_eq!(errors_of(&scenario), []);
}

#[test]
fn budget_overrides_must_be_finite_and_non_negative() {
    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0, -0.001] {
        let mut scenario = valid();
        scenario.budgets = BTreeMap::from([(Budget::QuitMs, bad)]);

        assert_eq!(
            errors_of(&scenario),
            [ValidationError::InvalidBudgetLimit {
                budget: Budget::QuitMs
            }],
            "{bad}"
        );
    }
    let mut scenario = valid();
    scenario.budgets = BTreeMap::from([
        (Budget::QuitMs, 0.0),
        (Budget::IdleOutputBytes, 0.0),
        (Budget::HeadlessScanRatio, 3.5),
    ]);
    assert_eq!(errors_of(&scenario), []);
}

#[test]
fn a_delete_step_requires_at_least_one_sentinel() {
    let delete = Step::Delete(Delete {
        name: "victim".to_owned(),
        kind: EntryKind::Folder,
        path: None,
        confirm_with: ConfirmKey::Y,
        wait_for: DeleteWait::Finished,
        timeout_ms: DEFAULT_TIMEOUT_MS,
    });
    let mut scenario = with_steps(vec![delete]);
    scenario.sentinels.clear();

    assert_eq!(
        errors_of(&scenario),
        [ValidationError::DeleteWithoutSentinel]
    );

    scenario.sentinels.push("keep-a.bin".to_owned());
    assert_eq!(errors_of(&scenario), []);
}

#[test]
fn a_scenario_that_never_deletes_needs_no_sentinel() {
    let mut scenario = valid();
    scenario.sentinels.clear();

    assert_eq!(errors_of(&scenario), []);
}

/// Every way a fixture-relative path can be wrong, with the rule it breaks.
const BAD_PATHS: &[(&str, PathViolation)] = &[
    ("", PathViolation::Empty),
    ("/", PathViolation::Absolute),
    ("/etc/passwd", PathViolation::Absolute),
    ("\\", PathViolation::Absolute),
    ("\\\\server\\share", PathViolation::Absolute),
    ("C:", PathViolation::WindowsPrefix),
    ("C:\\Windows", PathViolation::WindowsPrefix),
    ("C:/Windows", PathViolation::WindowsPrefix),
    ("c:relative", PathViolation::WindowsPrefix),
    ("a/C:x", PathViolation::WindowsPrefix),
    ("a\\b", PathViolation::Backslash),
    ("..\\x", PathViolation::Backslash),
    ("..", PathViolation::ParentDirectory),
    ("../x", PathViolation::ParentDirectory),
    ("a/../b", PathViolation::ParentDirectory),
    ("a/b/..", PathViolation::ParentDirectory),
    (".", PathViolation::CurrentDirectory),
    ("./a", PathViolation::CurrentDirectory),
    ("a/./b", PathViolation::CurrentDirectory),
    ("a//b", PathViolation::EmptyComponent),
    ("a/", PathViolation::EmptyComponent),
    ("a/b/", PathViolation::EmptyComponent),
    ("a/file:stream", PathViolation::Colon),
    ("a.", PathViolation::TrailingDotOrSpace),
    ("a/b ", PathViolation::TrailingDotOrSpace),
    ("a/NUL.txt", PathViolation::ReservedDeviceName),
];

#[test]
fn the_path_rule_names_the_violation() {
    for &(path, violation) in BAD_PATHS {
        assert_eq!(
            check_fixture_relative_path(path),
            Err(violation),
            "{path:?}"
        );
    }
}

#[test]
fn ordinary_names_are_fixture_relative_paths() {
    for path in [
        "a",
        "a/b",
        "a b/c d.txt",
        ".hidden",
        "..hidden",
        "a..b",
        "dir/.hidden/file",
        "~",
        "ünï/çode",
        "node_modules/pkg0/m0.js",
    ] {
        assert_eq!(check_fixture_relative_path(path), Ok(()), "{path:?}");
    }
}

#[test]
fn a_drive_designator_is_rejected_in_every_component() {
    // On Windows, joining a component that has a prefix but no root (`C:x`) onto a base path
    // replaces the whole base, so a designator after the first component escapes the fixture.
    for path in ["C:x", "a/C:x", "a/b/z:", "a/Z:/b"] {
        assert_eq!(
            check_fixture_relative_path(path),
            Err(PathViolation::WindowsPrefix),
            "{path:?}"
        );
    }
    for path in ["C", "a/C", "a/Cx", "a/C.x"] {
        assert_eq!(check_fixture_relative_path(path), Ok(()), "{path:?}");
    }
}

#[test]
fn a_colon_that_is_not_a_drive_designator_is_rejected_in_every_component() {
    // A designator needs an ASCII letter directly before the colon. Any other colon is an
    // alternate data stream on Windows.
    for path in [":x", "1:x", "ab:c", "é:x", "a/file:stream", "a/b/name:"] {
        assert_eq!(
            check_fixture_relative_path(path),
            Err(PathViolation::Colon),
            "{path:?}"
        );
    }
}

#[test]
fn a_component_ending_in_a_dot_or_a_space_is_rejected() {
    // Windows removes trailing dots and spaces, so these would name a different entry there.
    for path in [
        "a.",
        "a ",
        "a. ",
        "a /b",
        "a./b",
        "dir/file.",
        "dir/file ",
        "...",
        "a/.../b",
    ] {
        assert_eq!(
            check_fixture_relative_path(path),
            Err(PathViolation::TrailingDotOrSpace),
            "{path:?}"
        );
    }
    // Only the end of a component matters.
    for path in [
        " a", "a b", "a.b", ".hidden", "..hidden", "a..b", "dir/.x/y",
    ] {
        assert_eq!(check_fixture_relative_path(path), Ok(()), "{path:?}");
    }
}

/// Superscript one, two, and three (U+00B9, U+00B2, U+00B3), which Windows reserves after `COM`
/// and `LPT` just as it reserves the digits.
const SUPERSCRIPT_DIGITS: [char; 3] = ['\u{b9}', '\u{b2}', '\u{b3}'];

/// Every name Windows reserves for a DOS device, as Microsoft lists them.
fn reserved_device_names() -> Vec<String> {
    let mut names = Vec::from(["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"].map(str::to_owned));
    for device in ["COM", "LPT"] {
        for number in ('0'..='9').chain(SUPERSCRIPT_DIGITS) {
            names.push(format!("{device}{number}"));
        }
    }
    names
}

#[test]
fn reserved_device_names_are_rejected_in_any_case_with_or_without_an_extension() {
    for name in reserved_device_names() {
        let alternating: String = name
            .chars()
            .enumerate()
            .map(|(index, letter)| {
                if index % 2 == 0 {
                    letter.to_ascii_lowercase()
                } else {
                    letter
                }
            })
            .collect();
        for spelling in [name.clone(), name.to_ascii_lowercase(), alternating] {
            for path in [
                spelling.clone(),
                format!("{spelling}.txt"),
                format!("{spelling}.tar.gz"),
                format!("{spelling} .txt"),
                format!("dir/{spelling}"),
                format!("dir/{spelling}.d/file"),
            ] {
                assert_eq!(
                    check_fixture_relative_path(&path),
                    Err(PathViolation::ReservedDeviceName),
                    "{path:?}"
                );
            }
        }
    }
}

#[test]
fn names_that_only_resemble_reserved_devices_are_accepted() {
    for path in [
        "console",
        "CONX",
        "xcon",
        "COM",
        "COM10",
        "LPT",
        "LPT10",
        "nullable",
        "auxiliary",
        "communication",
        "a.con",
        "x.nul",
        ".nul",
        "con-x",
        "dir/prn_",
        // Windows reserves only superscript one, two, and three after COM and LPT.
        "COM\u{2074}",
        "LPT\u{2074}",
        "COM\u{b4}",
        "COM\u{ba}",
        "COM\u{b9}\u{b2}",
        "COM\u{2081}",
        // CONIN$ and CONOUT$ are reserved only exactly as spelled.
        "CONIN",
        "CONOUT",
        "CONIN$$",
        "CONOUT$x",
        "xCONIN$",
    ] {
        assert_eq!(check_fixture_relative_path(path), Ok(()), "{path:?}");
    }
}

#[test]
fn sentinel_paths_must_be_fixture_relative() {
    for &(path, violation) in BAD_PATHS {
        let mut scenario = valid();
        scenario.sentinels = vec!["keep-a.bin".to_owned(), path.to_owned()];

        assert_eq!(
            errors_of(&scenario),
            [ValidationError::InvalidPath {
                field: Field::at("sentinels", 1),
                path: path.to_owned(),
                violation,
            }],
            "{path:?}"
        );
    }
}

#[test]
fn step_paths_must_be_fixture_relative() {
    for &(path, violation) in BAD_PATHS {
        let expected = |field: Field| {
            vec![StepError::InvalidPath {
                field,
                path: path.to_owned(),
                violation,
            }]
        };
        let path_step = |build: fn(String) -> Step| step_errors(build(path.to_owned()));

        assert_eq!(
            path_step(|path| Step::WaitFsAbsent(WaitFs {
                path,
                timeout_ms: DEFAULT_TIMEOUT_MS
            })),
            expected(Field::new("path")),
            "wait_fs_absent {path:?}"
        );
        assert_eq!(
            path_step(|path| Step::WaitFsPresent(WaitFs {
                path,
                timeout_ms: DEFAULT_TIMEOUT_MS
            })),
            expected(Field::new("path")),
            "wait_fs_present {path:?}"
        );
        assert_eq!(
            path_step(|path| Step::FsMutate(FsMutate {
                op: MutateOp::Vanish,
                path
            })),
            expected(Field::new("path")),
            "fs_mutate {path:?}"
        );
        assert_eq!(
            path_step(|path| Step::ExpectFs(ExpectFs {
                present: vec!["ok".to_owned(), path],
                absent: Vec::new(),
            })),
            expected(Field::at("present", 1)),
            "expect_fs present {path:?}"
        );
        assert_eq!(
            path_step(|path| Step::ExpectFs(ExpectFs {
                present: Vec::new(),
                absent: vec![path],
            })),
            expected(Field::at("absent", 0)),
            "expect_fs absent {path:?}"
        );
    }
}

/// The rules broken by `step` alone, placed after a `delete` step: a `wait_refresh` is valid only
/// after one, and every other step is the same wherever it stands.
fn errors_after_a_delete(step: Step) -> Vec<StepError> {
    errors_of(&with_steps(vec![delete_at("stuck.txt"), step]))
        .into_iter()
        .map(|error| match error {
            ValidationError::Step {
                index: 1, error, ..
            } => error,
            other => panic!("expected only errors for the step, found: {other}"),
        })
        .collect()
}

#[test]
fn every_timeout_must_be_between_one_and_the_cap() {
    let waiting = waiting_steps(DEFAULT_TIMEOUT_MS);
    for bad in [0, MAX_TIMEOUT_MS + 1, u64::MAX] {
        for step in waiting_steps(bad) {
            let kind = step.kind();

            assert_eq!(
                errors_after_a_delete(step),
                [StepError::TimeoutOutOfRange { timeout_ms: bad }],
                "{kind} with timeout_ms = {bad}"
            );
        }
    }
    for good in [1, DEFAULT_TIMEOUT_MS, MAX_TIMEOUT_MS] {
        for step in waiting_steps(good) {
            let kind = step.kind();

            assert_eq!(
                errors_after_a_delete(step),
                [],
                "{kind} with timeout_ms = {good}"
            );
        }
    }
    let waiting_kinds: Vec<&str> = waiting.iter().map(Step::kind).collect();
    let bounded_kinds: Vec<&str> = parse(ALL_STEPS)
        .steps
        .iter()
        .filter(|step| step.timeout_ms().is_some())
        .map(Step::kind)
        .collect();
    for kind in bounded_kinds {
        assert!(
            waiting_kinds.contains(&kind),
            "{kind} has a timeout but this test does not cover it"
        );
    }
}

#[test]
fn steps_that_never_wait_have_no_timeout() {
    let never_wait = [
        Step::Type(TypeText {
            text: "x".to_owned(),
        }),
        Step::Resize(Resize { cols: 80, rows: 24 }),
        Step::ExpectFs(ExpectFs {
            present: vec!["a".to_owned()],
            absent: Vec::new(),
        }),
    ];
    for step in never_wait {
        assert_eq!(step.timeout_ms(), None, "{}", step.kind());
    }
}

#[test]
fn wait_text_needs_exactly_one_matcher() {
    assert_eq!(
        step_errors(wait_text(None, None, None)),
        [StepError::MissingMatcher]
    );
    assert_eq!(
        step_errors(wait_text(Some("a"), Some("b"), None)),
        [StepError::ConflictingMatchers]
    );
    assert_eq!(step_errors(wait_text(Some("a"), None, None)), []);
    assert_eq!(step_errors(wait_text(None, Some("a+"), None)), []);
}

#[test]
fn wait_text_rejects_a_matcher_that_matches_everything() {
    assert_eq!(
        step_errors(wait_text(Some(""), None, None)),
        [StepError::EmptyValue {
            field: Field::new("text")
        }]
    );
    assert_eq!(
        step_errors(wait_text(None, Some(""), None)),
        [StepError::EmptyValue {
            field: Field::new("regex")
        }]
    );
}

#[test]
fn typed_text_and_entry_names_must_not_be_empty() {
    let select = Step::Select(Select {
        name: String::new(),
        timeout_ms: DEFAULT_TIMEOUT_MS,
    });
    let delete = Step::Delete(Delete {
        name: String::new(),
        kind: EntryKind::File,
        path: None,
        confirm_with: ConfirmKey::Y,
        wait_for: DeleteWait::Finished,
        timeout_ms: DEFAULT_TIMEOUT_MS,
    });
    let typed = Step::Type(TypeText {
        text: String::new(),
    });
    for (step, field) in [(select, "name"), (delete, "name"), (typed, "text")] {
        let kind = step.kind();

        assert_eq!(
            step_errors(step),
            [StepError::EmptyValue {
                field: Field::new(field)
            }],
            "{kind}"
        );
    }
}

#[test]
fn expect_screen_must_check_something() {
    let errors = step_errors(Step::ExpectScreen(expect_screen(&[], &[], &[])));

    assert!(
        matches!(errors.as_slice(), [StepError::NothingToCheck { .. }]),
        "{errors:?}"
    );
    for checked in [
        expect_screen(&["a"], &[], &[]),
        expect_screen(&[], &["a"], &[]),
        expect_screen(&[], &[], &["a"]),
    ] {
        assert_eq!(step_errors(Step::ExpectScreen(checked)), []);
    }
}

#[test]
fn expect_screen_rejects_an_entry_that_would_always_or_never_match() {
    let step = Step::ExpectScreen(expect_screen(&["a", ""], &["", "b"], &["c", "d", ""]));

    assert_eq!(
        step_errors(step),
        [
            StepError::EmptyValue {
                field: Field::at("contains", 1)
            },
            StepError::EmptyValue {
                field: Field::at("not_contains", 0)
            },
            StepError::EmptyValue {
                field: Field::at("regex", 2)
            },
        ]
    );
}

#[test]
fn expect_fs_must_check_something() {
    let errors = step_errors(Step::ExpectFs(ExpectFs {
        present: Vec::new(),
        absent: Vec::new(),
    }));

    assert!(
        matches!(errors.as_slice(), [StepError::NothingToCheck { .. }]),
        "{errors:?}"
    );
}

#[test]
fn a_row_region_must_not_end_before_it_starts() {
    let reversed = Region::Rows([5, 2]);
    let mut screen = expect_screen(&["a"], &[], &[]);
    screen.region = Some(reversed);

    assert_eq!(
        step_errors(wait_text(Some("a"), None, Some(reversed))),
        [StepError::ReversedRows { first: 5, last: 2 }]
    );
    assert_eq!(
        step_errors(Step::ExpectScreen(screen)),
        [StepError::ReversedRows { first: 5, last: 2 }]
    );
    for accepted in [
        Region::Rows([2, 2]),
        Region::Rows([0, u16::MAX]),
        Region::Header,
    ] {
        assert_eq!(step_errors(wait_text(Some("a"), None, Some(accepted))), []);
    }
}

#[test]
fn a_resize_needs_non_zero_dimensions_but_may_go_below_the_minimum_terminal() {
    for (cols, rows) in [(0, 24), (80, 0), (0, 0)] {
        assert_eq!(
            step_errors(Step::Resize(Resize { cols, rows })),
            [StepError::ZeroDimension { cols, rows }]
        );
    }
    for (cols, rows) in [(1, 1), (20, 5), (31, 7), (500, 200)] {
        assert_eq!(
            step_errors(Step::Resize(Resize { cols, rows })),
            [],
            "{cols}x{rows} exercises the resize message"
        );
    }
}

#[test]
fn an_idle_duration_must_be_between_one_and_the_cap() {
    for bad in [0, MAX_TIMEOUT_MS + 1, u64::MAX] {
        assert_eq!(
            step_errors(Step::Idle(Idle {
                after_ms: bad,
                window_ms: 5000
            })),
            [StepError::DurationOutOfRange {
                field: "after_ms",
                value: bad
            }]
        );
        assert_eq!(
            step_errors(Step::Idle(Idle {
                after_ms: 3200,
                window_ms: bad
            })),
            [StepError::DurationOutOfRange {
                field: "window_ms",
                value: bad
            }]
        );
    }
    assert_eq!(
        step_errors(Step::Idle(Idle {
            after_ms: 3200,
            window_ms: 5000
        })),
        []
    );
}

fn wait_event(event: EventKind, field: EventField) -> Step {
    Step::WaitEvent(WaitEvent {
        event,
        fields: BTreeMap::from([(field, Comparison::Min(1))]),
        timeout_ms: DEFAULT_TIMEOUT_MS,
    })
}

#[test]
fn a_wait_event_may_only_test_fields_its_event_carries() {
    for (event, field, accepted) in [
        (EventKind::ScanComplete, EventField::Entries, true),
        (EventKind::DeletionFinished, EventField::Removed, true),
        (EventKind::DeletionFinished, EventField::Failed, true),
        (EventKind::Frame, EventField::Seq, true),
        (EventKind::Frame, EventField::Inputs, true),
        (EventKind::Exit, EventField::Code, true),
        (EventKind::Frame, EventField::Entries, false),
        (EventKind::Frame, EventField::Code, false),
        (EventKind::ScanComplete, EventField::Removed, false),
        (EventKind::DeletionFinished, EventField::Entries, false),
        (EventKind::QuitPrompt, EventField::Seq, false),
        (EventKind::Exit, EventField::Failed, false),
        (EventKind::Exit, EventField::Inputs, false),
    ] {
        let expected = if accepted {
            Vec::new()
        } else {
            vec![StepError::UnknownEventField { event, field }]
        };

        assert_eq!(
            step_errors(wait_event(event, field)),
            expected,
            "{event}.{field}"
        );
    }
}

#[test]
fn every_event_carries_a_timestamp_that_can_be_tested() {
    for &event in EventKind::ALL {
        assert_eq!(
            step_errors(wait_event(event, EventField::TimeMicros)),
            [],
            "{event}"
        );
    }
}

#[test]
fn every_event_field_belongs_to_some_event() {
    for &field in EventField::ALL {
        assert!(
            EventKind::ALL
                .iter()
                .any(|kind| kind.fields().contains(&field)),
            "{field} is carried by no event, so no scenario could test it"
        );
    }
}

#[test]
fn metric_and_measurement_names_must_be_identifiers() {
    for bad in ["", "Bad Name", "UPPER", "a.b", "-x"] {
        assert_eq!(
            step_errors(Step::ExpectBudget(ExpectBudget {
                budget: Budget::QuitMs,
                metric: bad.to_owned(),
            })),
            [StepError::InvalidIdentifier {
                field: "metric",
                value: bad.to_owned()
            }],
            "{bad:?}"
        );
        let scenario = with_steps(vec![
            measure(bad, Marker::Start),
            measure(bad, Marker::Stop),
        ]);
        let invalid = |index: usize| ValidationError::Step {
            index,
            kind: "measure",
            error: StepError::InvalidIdentifier {
                field: "name",
                value: bad.to_owned(),
            },
        };

        assert_eq!(errors_of(&scenario), [invalid(0), invalid(1)], "{bad:?}");
    }
}

fn measurement_errors(steps: Vec<Step>) -> Vec<(usize, StepError)> {
    errors_of(&with_steps(steps))
        .into_iter()
        .map(|error| match error {
            ValidationError::Step { index, error, .. } => (index, error),
            other => panic!("expected only step errors, found: {other}"),
        })
        .collect()
}

#[test]
fn a_measurement_must_start_before_it_stops_and_stop_once() {
    assert_eq!(
        measurement_errors(vec![measure("t", Marker::Stop)]),
        [(
            0,
            StepError::MeasureStopWithoutStart {
                name: "t".to_owned()
            }
        )]
    );
    assert_eq!(
        measurement_errors(vec![
            measure("t", Marker::Start),
            measure("t", Marker::Stop),
            measure("t", Marker::Stop),
        ]),
        [(
            2,
            StepError::MeasureStopWithoutStart {
                name: "t".to_owned()
            }
        )]
    );
}

#[test]
fn a_measurement_that_starts_must_stop() {
    assert_eq!(
        measurement_errors(vec![
            wait_text(Some("a"), None, None),
            measure("t", Marker::Start),
        ]),
        [(
            1,
            StepError::MeasureNeverStopped {
                name: "t".to_owned()
            }
        )]
    );
}

#[test]
fn a_measurement_name_is_used_once() {
    assert_eq!(
        measurement_errors(vec![
            measure("t", Marker::Start),
            measure("t", Marker::Start),
            measure("t", Marker::Stop),
        ]),
        [(
            1,
            StepError::MeasureRestarted {
                name: "t".to_owned()
            }
        )]
    );
    assert_eq!(
        measurement_errors(vec![
            measure("t", Marker::Start),
            measure("t", Marker::Stop),
            measure("t", Marker::Start),
            measure("t", Marker::Stop),
        ]),
        [
            (
                2,
                StepError::MeasureRestarted {
                    name: "t".to_owned()
                }
            ),
            (
                3,
                StepError::MeasureStopWithoutStart {
                    name: "t".to_owned()
                }
            ),
        ]
    );
}

#[test]
fn distinct_measurements_may_overlap() {
    assert_eq!(
        measurement_errors(vec![
            measure("a", Marker::Start),
            measure("b", Marker::Start),
            measure("a", Marker::Stop),
            measure("b", Marker::Stop),
        ]),
        []
    );
}

#[test]
fn every_broken_rule_is_reported_in_scenario_order() {
    let mut scenario = valid();
    scenario.schema_version = 9;
    scenario.name = "Bad".to_owned();
    scenario.profiles.clear();
    scenario.terminal.cols = 10;
    scenario.expect = Expect::Fail;
    scenario.sentinels = vec!["../escape".to_owned()];
    scenario.steps = vec![
        wait_text(None, None, None),
        Step::Delete(Delete {
            name: "victim".to_owned(),
            kind: EntryKind::File,
            path: None,
            confirm_with: ConfirmKey::Y,
            wait_for: DeleteWait::Finished,
            timeout_ms: 0,
        }),
        measure("t", Marker::Stop),
    ];

    assert_eq!(
        errors_of(&scenario),
        [
            ValidationError::UnsupportedSchemaVersion { found: 9 },
            ValidationError::InvalidIdentifier {
                field: "name",
                value: "Bad".to_owned()
            },
            ValidationError::NoProfiles,
            ValidationError::TerminalTooSmall {
                cols: 10,
                rows: scenario.terminal.rows
            },
            ValidationError::ExpectedFailureWithoutSlice,
            ValidationError::InvalidPath {
                field: Field::at("sentinels", 0),
                path: "../escape".to_owned(),
                violation: PathViolation::ParentDirectory,
            },
            ValidationError::Step {
                index: 0,
                kind: "wait_text",
                error: StepError::MissingMatcher
            },
            ValidationError::Step {
                index: 1,
                kind: "delete",
                error: StepError::TimeoutOutOfRange { timeout_ms: 0 }
            },
            ValidationError::Step {
                index: 2,
                kind: "measure",
                error: StepError::MeasureStopWithoutStart {
                    name: "t".to_owned()
                }
            },
        ]
    );
}

#[test]
fn the_rendered_report_locates_each_broken_rule() {
    let scenario = with_steps(vec![
        Step::Settle(Settle {
            timeout_ms: DEFAULT_TIMEOUT_MS,
        }),
        wait_text(None, None, None),
    ]);

    let report = scenario
        .validate()
        .expect_err("the second step is invalid")
        .to_string();

    assert!(report.contains("steps[1] (wait_text)"), "{report}");
}

fn delete_at(path: &str) -> Step {
    Step::Delete(Delete {
        name: "stuck.txt".to_owned(),
        kind: EntryKind::File,
        path: Some(path.to_owned()),
        confirm_with: ConfirmKey::Y,
        wait_for: DeleteWait::Finished,
        timeout_ms: DEFAULT_TIMEOUT_MS,
    })
}

#[test]
fn a_delete_path_must_be_safe_and_end_in_the_entrys_name() {
    assert_eq!(step_errors(delete_at("stuck.txt")), []);
    assert_eq!(step_errors(delete_at("hostile/unwritable/stuck.txt")), []);

    // Where the entry is must name the entry, or the step would delete one thing and check another.
    assert_eq!(
        step_errors(delete_at("hostile/unwritable")),
        [StepError::PathNotNamed {
            name: "stuck.txt".to_owned(),
            path: "hostile/unwritable".to_owned(),
        }]
    );
    // A path that could leave the fixture is reported as that, and only that.
    let errors = step_errors(delete_at("../stuck.txt"));
    assert!(
        matches!(errors.as_slice(), [StepError::InvalidPath { .. }]),
        "{errors:?}"
    );
}

#[test]
fn a_wait_refresh_needs_a_delete_step_before_it() {
    let wait_refresh = Step::WaitRefresh(WaitRefresh {
        timeout_ms: DEFAULT_TIMEOUT_MS,
    });
    let refused = |error_index| {
        [ValidationError::Step {
            index: error_index,
            kind: "wait_refresh",
            error: StepError::RefreshWithoutDelete,
        }]
    };

    // Nothing owes a refresh before a deletion, so the step would pass without waiting for
    // anything.
    assert_eq!(
        errors_of(&with_steps(vec![wait_refresh.clone()])),
        refused(0)
    );
    assert_eq!(
        errors_of(&with_steps(vec![
            wait_refresh.clone(),
            delete_at("stuck.txt")
        ])),
        refused(0),
        "a deletion after the step is no deletion for it to wait for"
    );
    assert_eq!(
        errors_of(&with_steps(vec![delete_at("stuck.txt"), wait_refresh])),
        []
    );
}

#[test]
fn a_delete_that_waits_for_its_dialog_to_close_needs_a_dialog() {
    let started = Step::Delete(Delete {
        name: "victim".to_owned(),
        kind: EntryKind::Folder,
        path: None,
        confirm_with: ConfirmKey::Y,
        wait_for: DeleteWait::Started,
        timeout_ms: DEFAULT_TIMEOUT_MS,
    });
    let mut scenario = with_steps(vec![started]);
    assert_eq!(errors_of(&scenario), []);

    scenario.disable_delete_confirmation = true;
    assert_eq!(
        errors_of(&scenario),
        [ValidationError::Step {
            index: 0,
            kind: "delete",
            error: StepError::StartedWithoutDialog,
        }]
    );
}

#[test]
fn a_confirm_key_other_than_the_default_needs_a_dialog() {
    let enter = Step::Delete(Delete {
        name: "victim".to_owned(),
        kind: EntryKind::Folder,
        path: None,
        confirm_with: ConfirmKey::Enter,
        wait_for: DeleteWait::Finished,
        timeout_ms: DEFAULT_TIMEOUT_MS,
    });
    let mut scenario = with_steps(vec![enter]);
    assert_eq!(errors_of(&scenario), []);

    scenario.disable_delete_confirmation = true;
    assert_eq!(
        errors_of(&scenario),
        [ValidationError::Step {
            index: 0,
            kind: "delete",
            error: StepError::ConfirmWithoutDialog,
        }]
    );
}

#[test]
fn expect_config_needs_a_dotted_key_and_a_string_to_equal() {
    let expect = |key: &str, equals: &str| {
        Step::ExpectConfig(ExpectConfig {
            key: key.to_owned(),
            equals: equals.to_owned(),
        })
    };
    assert_eq!(step_errors(expect("runtime.theme", "excise-light")), []);
    assert_eq!(step_errors(expect("version", "1")), []);
    for key in [
        "",
        ".theme",
        "runtime.",
        "runtime..theme",
        "Runtime.theme",
        "runtime theme",
        "runtime.the/me",
    ] {
        assert_eq!(
            step_errors(expect(key, "x")),
            [StepError::InvalidConfigKey {
                key: key.to_owned()
            }],
            "{key:?}"
        );
    }
    assert_eq!(
        step_errors(expect("runtime.theme", "")),
        [StepError::EmptyValue {
            field: Field::new("equals")
        }]
    );
}
