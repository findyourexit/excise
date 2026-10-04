//! Parsing: every step type, the strictness of the schema, and the file loader.

use std::{collections::BTreeMap, collections::BTreeSet, fs};

use super::{ALL_STEPS, BASE, TempDir, parse, valid};
use crate::scenario::{
    Budget, Comparison, ConfirmKey, DEFAULT_TIMEOUT_MS, Delete, DeleteWait, EntryKind, EventField,
    EventKind, Expect, ExpectBudget, ExpectConfig, ExpectExit, ExpectFs, ExpectScreen, FsMutate,
    Idle, KeyName, LoadError, Marker, Measure, MutateOp, PressKey, Profile, Quit, Region, Residue,
    Resize, ScanState, Scenario, Select, SendSignal, Settle, Signal, Step, Terminal, Tier,
    TypeText, WaitEvent, WaitFs, WaitHeader, WaitText,
};

const HEAD: &str = r#"schema_version = 1
name = "x"
description = "d"
fixture = "f"
profiles = ["default"]
"#;

/// Parses `fragment` as the only step of an otherwise minimal scenario.
fn step(fragment: &str) -> Step {
    let mut steps = parse(&format!("{HEAD}\n[[steps]]\n{fragment}\n")).steps;
    assert_eq!(
        steps.len(),
        1,
        "the fragment should define exactly one step"
    );
    steps.remove(0)
}

/// The parse error for `source`, which the test expects to be rejected.
fn rejection(source: &str) -> String {
    Scenario::from_toml_str(source)
        .expect_err("the document should be rejected")
        .to_string()
}

#[test]
fn a_minimal_scenario_gets_the_documented_defaults() {
    let scenario = valid();

    assert_eq!(
        scenario.terminal,
        Terminal {
            cols: 120,
            rows: 40,
            drain_bytes_per_sec: None,
        }
    );
    assert_eq!(scenario.expect, Expect::Pass);
    assert_eq!(scenario.tier, Tier::Quick);
    assert_eq!(scenario.platforms, None);
    assert_eq!(scenario.fails_on, None);
    assert_eq!(scenario.slice, None);
    assert!(scenario.budgets.is_empty());
    assert_eq!(
        scenario.steps,
        [Step::WaitHeader(WaitHeader {
            state: ScanState::Complete,
            timeout_ms: DEFAULT_TIMEOUT_MS,
        })],
        "a waiting step without timeout_ms takes the documented default"
    );
}

#[test]
fn a_full_scenario_parses_every_top_level_field() {
    let scenario = parse(ALL_STEPS);

    assert_eq!(scenario.schema_version, 1);
    assert_eq!(scenario.name, "all-steps");
    assert_eq!(scenario.fixture, "all-steps");
    assert_eq!(scenario.sentinels, ["keep-a.bin", "nested/keep-b.txt"]);
    assert_eq!(scenario.profiles, Profile::ALL);
    assert_eq!(
        scenario.terminal,
        Terminal {
            cols: 100,
            rows: 30,
            drain_bytes_per_sec: None,
        }
    );
    assert_eq!(scenario.tier, Tier::Nightly);
    assert_eq!(
        scenario.platforms,
        Some(vec![
            "linux".to_owned(),
            "macos".to_owned(),
            "windows".to_owned()
        ])
    );
    assert_eq!(scenario.expect, Expect::Fail);
    assert_eq!(
        scenario.fails_on,
        Some(vec!["linux".to_owned(), "macos".to_owned()])
    );
    assert_eq!(scenario.slice.as_deref(), Some("X2"));
    assert_eq!(
        scenario.budgets,
        BTreeMap::from([
            (Budget::InputToFrameP99Ms, 100.0),
            (Budget::IdleOutputBytes, 0.0)
        ])
    );
}

#[test]
fn the_full_scenario_contains_every_step_kind() {
    let kinds: BTreeSet<&str> = parse(ALL_STEPS).steps.iter().map(Step::kind).collect();

    assert_eq!(kinds, BTreeSet::from(Step::KINDS));
}

#[test]
fn every_tier_parses() {
    for &tier in Tier::ALL {
        let scenario = parse(&BASE.replace(
            "profiles = [\"default\", \"deterministic\"]\n",
            &format!("profiles = [\"default\", \"deterministic\"]\ntier = \"{tier}\"\n"),
        ));
        assert_eq!(scenario.tier, tier);
    }
}

#[test]
fn platforms_and_fails_on_parse_as_lists() {
    let scenario = parse(&format!(
        "{HEAD}platforms = [\"linux\", \"macos\"]\nexpect = \"fail\"\nfails_on = [\"linux\"]\n\
         slice = \"X1\"\n\n[[steps]]\nstep = \"settle\"\n"
    ));
    assert_eq!(
        scenario.platforms,
        Some(vec!["linux".to_owned(), "macos".to_owned()])
    );
    assert_eq!(scenario.fails_on, Some(vec!["linux".to_owned()]));
}

#[test]
fn wait_text_parses_text_or_regex_with_an_optional_region() {
    assert_eq!(
        step("step = \"wait_text\"\ntext = \"COMPLETE\"\nregion = \"header\"\ntimeout_ms = 5"),
        Step::WaitText(WaitText {
            text: Some("COMPLETE".to_owned()),
            regex: None,
            region: Some(Region::Header),
            timeout_ms: 5,
        })
    );
    assert_eq!(
        step("step = \"wait_text\"\nregex = 'a\\d+'\nregion = { rows = [2, 4] }"),
        Step::WaitText(WaitText {
            text: None,
            regex: Some(r"a\d+".to_owned()),
            region: Some(Region::Rows([2, 4])),
            timeout_ms: DEFAULT_TIMEOUT_MS,
        })
    );
    assert_eq!(
        step("step = \"wait_text\"\ntext = \"Delete\"\nregion = \"dialog\""),
        Step::WaitText(WaitText {
            text: Some("Delete".to_owned()),
            regex: None,
            region: Some(Region::Dialog),
            timeout_ms: DEFAULT_TIMEOUT_MS,
        })
    );
}

#[test]
fn wait_header_parses_both_scan_states() {
    assert_eq!(
        step("step = \"wait_header\"\nstate = \"scanning\"\ntimeout_ms = 7"),
        Step::WaitHeader(WaitHeader {
            state: ScanState::Scanning,
            timeout_ms: 7,
        })
    );
    assert_eq!(
        step("step = \"wait_header\"\nstate = \"complete\""),
        Step::WaitHeader(WaitHeader {
            state: ScanState::Complete,
            timeout_ms: DEFAULT_TIMEOUT_MS,
        })
    );
}

#[test]
fn wait_event_parses_every_event_kind_and_field_predicates() {
    for &event in EventKind::ALL {
        assert_eq!(
            step(&format!("step = \"wait_event\"\nevent = \"{event}\"")),
            Step::WaitEvent(WaitEvent {
                event,
                fields: BTreeMap::new(),
                timeout_ms: DEFAULT_TIMEOUT_MS,
            })
        );
    }
    assert_eq!(
        step(
            "step = \"wait_event\"\nevent = \"deletion_finished\"\n\
             fields = { removed = { min = 1 }, failed = { eq = 0 } }"
        ),
        Step::WaitEvent(WaitEvent {
            event: EventKind::DeletionFinished,
            fields: BTreeMap::from([
                (EventField::Removed, Comparison::Min(1)),
                (EventField::Failed, Comparison::Eq(0)),
            ]),
            timeout_ms: DEFAULT_TIMEOUT_MS,
        })
    );
    assert_eq!(
        step(
            "step = \"wait_event\"\nevent = \"frame\"\n\
             fields = { seq = { max = 9 }, inputs = { min = 2 }, t_us = { min = 1 } }"
        ),
        Step::WaitEvent(WaitEvent {
            event: EventKind::Frame,
            fields: BTreeMap::from([
                (EventField::Seq, Comparison::Max(9)),
                (EventField::Inputs, Comparison::Min(2)),
                (EventField::TimeMicros, Comparison::Min(1)),
            ]),
            timeout_ms: DEFAULT_TIMEOUT_MS,
        })
    );
    assert_eq!(
        step(
            "step = \"wait_event\"\nevent = \"scan_complete\"\n\
             fields = { entries = { min = 100 } }"
        ),
        Step::WaitEvent(WaitEvent {
            event: EventKind::ScanComplete,
            fields: BTreeMap::from([(EventField::Entries, Comparison::Min(100))]),
            timeout_ms: DEFAULT_TIMEOUT_MS,
        })
    );
    assert_eq!(
        step("step = \"wait_event\"\nevent = \"exit\"\nfields = { code = { eq = 130 } }"),
        Step::WaitEvent(WaitEvent {
            event: EventKind::Exit,
            fields: BTreeMap::from([(EventField::Code, Comparison::Eq(130))]),
            timeout_ms: DEFAULT_TIMEOUT_MS,
        })
    );
}

#[test]
fn key_parses_characters_and_named_keys_with_modifiers() {
    assert_eq!(
        step("step = \"key\"\nkey = \"enter\""),
        Step::Key(PressKey {
            key: KeyName::Enter,
            ctrl: false,
            alt: false,
        })
    );
    assert_eq!(
        step("step = \"key\"\nkey = \"c\"\nctrl = true"),
        Step::Key(PressKey {
            key: KeyName::Char('c'),
            ctrl: true,
            alt: false,
        })
    );
    assert_eq!(
        step("step = \"key\"\nkey = \"page_down\"\nalt = true"),
        Step::Key(PressKey {
            key: KeyName::PageDown,
            ctrl: false,
            alt: true,
        })
    );
}

#[test]
fn type_parses_literal_text() {
    assert_eq!(
        step("step = \"type\"\ntext = \"victim dir\""),
        Step::Type(TypeText {
            text: "victim dir".to_owned(),
        })
    );
}

#[test]
fn select_parses_an_entry_name() {
    assert_eq!(
        step("step = \"select\"\nname = \"victim\"\ntimeout_ms = 3"),
        Step::Select(Select {
            name: "victim".to_owned(),
            timeout_ms: 3,
        })
    );
}

#[test]
fn delete_parses_a_name_and_both_kinds() {
    assert_eq!(
        step("step = \"delete\"\nname = \"victim\"\nkind = \"folder\""),
        Step::Delete(Delete {
            name: "victim".to_owned(),
            kind: EntryKind::Folder,
            path: None,
            confirm_with: ConfirmKey::Y,
            wait_for: DeleteWait::Finished,
            timeout_ms: DEFAULT_TIMEOUT_MS,
        })
    );
    assert_eq!(
        step("step = \"delete\"\nname = \"keep.bin\"\nkind = \"file\""),
        Step::Delete(Delete {
            name: "keep.bin".to_owned(),
            kind: EntryKind::File,
            path: None,
            confirm_with: ConfirmKey::Y,
            wait_for: DeleteWait::Finished,
            timeout_ms: DEFAULT_TIMEOUT_MS,
        })
    );
}

#[test]
fn delete_parses_where_a_nested_entry_is() {
    let source = "step = \"delete\"\nname = \"stuck.txt\"\nkind = \"file\"\n\
                  path = \"hostile/unwritable/stuck.txt\"";
    assert_eq!(
        step(source),
        Step::Delete(Delete {
            name: "stuck.txt".to_owned(),
            kind: EntryKind::File,
            path: Some("hostile/unwritable/stuck.txt".to_owned()),
            confirm_with: ConfirmKey::Y,
            wait_for: DeleteWait::Finished,
            timeout_ms: DEFAULT_TIMEOUT_MS,
        })
    );
}

#[test]
fn delete_confirms_with_y_unless_it_names_enter() {
    let confirm_with = |line: &str| {
        let source = format!("step = \"delete\"\nname = \"victim\"\nkind = \"folder\"\n{line}");
        match step(&source) {
            Step::Delete(delete) => delete.confirm_with,
            other => panic!("expected a delete step, got {other:?}"),
        }
    };
    assert_eq!(confirm_with(""), ConfirmKey::Y);
    assert_eq!(confirm_with("confirm_with = \"y\""), ConfirmKey::Y);
    assert_eq!(confirm_with("confirm_with = \"enter\""), ConfirmKey::Enter);
}

#[test]
fn expect_config_parses_a_key_and_the_string_it_must_equal() {
    assert_eq!(
        step("step = \"expect_config\"\nkey = \"runtime.theme\"\nequals = \"excise-light\""),
        Step::ExpectConfig(ExpectConfig {
            key: "runtime.theme".to_owned(),
            equals: "excise-light".to_owned(),
        })
    );
}

#[test]
fn the_delete_confirmation_is_on_unless_the_scenario_disables_it() {
    assert!(!valid().disable_delete_confirmation);
    let source =
        format!("{HEAD}disable_delete_confirmation = true\n\n[[steps]]\nstep = \"settle\"\n");
    assert!(parse(&source).disable_delete_confirmation);
}

#[test]
fn wait_fs_steps_parse_a_path_and_timeout() {
    assert_eq!(
        step("step = \"wait_fs_absent\"\npath = \"victim\"\ntimeout_ms = 60000"),
        Step::WaitFsAbsent(WaitFs {
            path: "victim".to_owned(),
            timeout_ms: 60_000,
        })
    );
    assert_eq!(
        step("step = \"wait_fs_present\"\npath = \"a/b.txt\""),
        Step::WaitFsPresent(WaitFs {
            path: "a/b.txt".to_owned(),
            timeout_ms: DEFAULT_TIMEOUT_MS,
        })
    );
}

#[test]
fn fs_mutate_parses_every_operation() {
    for &op in MutateOp::ALL {
        assert_eq!(
            step(&format!(
                "step = \"fs_mutate\"\nop = \"{op}\"\npath = \"a/b\""
            )),
            Step::FsMutate(FsMutate {
                op,
                path: "a/b".to_owned(),
            })
        );
    }
}

#[test]
fn resize_parses_dimensions() {
    assert_eq!(
        step("step = \"resize\"\ncols = 60\nrows = 20"),
        Step::Resize(Resize { cols: 60, rows: 20 })
    );
}

#[test]
fn idle_parses_both_durations() {
    assert_eq!(
        step("step = \"idle\"\nafter_ms = 3200\nwindow_ms = 5000"),
        Step::Idle(Idle {
            after_ms: 3200,
            window_ms: 5000
        })
    );
}

#[test]
fn signal_parses_every_signal_and_console_event() {
    for &signal in Signal::ALL {
        assert_eq!(
            step(&format!("step = \"signal\"\nsignal = \"{signal}\"")),
            Step::Signal(SendSignal { signal })
        );
    }
}

#[test]
fn expect_screen_parses_all_three_assertion_kinds() {
    assert_eq!(
        step(
            "step = \"expect_screen\"\ncontains = [\"a\", \"b\"]\nnot_contains = [\"c\"]\n\
             regex = ['d+']\nregion = \"header\""
        ),
        Step::ExpectScreen(ExpectScreen {
            contains: vec!["a".to_owned(), "b".to_owned()],
            not_contains: vec!["c".to_owned()],
            regex: vec!["d+".to_owned()],
            region: Some(Region::Header),
        })
    );
    assert_eq!(
        step("step = \"expect_screen\"\nnot_contains = [\"ERROR\"]"),
        Step::ExpectScreen(ExpectScreen {
            contains: Vec::new(),
            not_contains: vec!["ERROR".to_owned()],
            regex: Vec::new(),
            region: None,
        })
    );
}

#[test]
fn expect_fs_parses_present_and_absent_lists() {
    assert_eq!(
        step("step = \"expect_fs\"\npresent = [\"a\", \"b/c\"]\nabsent = [\"d\"]"),
        Step::ExpectFs(ExpectFs {
            present: vec!["a".to_owned(), "b/c".to_owned()],
            absent: vec!["d".to_owned()],
        })
    );
    assert_eq!(
        step("step = \"expect_fs\"\nabsent = [\"d\"]"),
        Step::ExpectFs(ExpectFs {
            present: Vec::new(),
            absent: vec!["d".to_owned()],
        })
    );
}

#[test]
fn expect_exit_parses_code_terminal_state_and_residue() {
    assert_eq!(
        step("step = \"expect_exit\"\ncode = 130\nterminal_restored = true\nresidue = \"none\""),
        Step::ExpectExit(ExpectExit {
            code: 130,
            terminal_restored: true,
            residue: Residue::None,
            timeout_ms: DEFAULT_TIMEOUT_MS,
        })
    );
    assert_eq!(
        step(
            "step = \"expect_exit\"\ncode = 0\nterminal_restored = false\nresidue = \"none\"\n\
             timeout_ms = 9"
        ),
        Step::ExpectExit(ExpectExit {
            code: 0,
            terminal_restored: false,
            residue: Residue::None,
            timeout_ms: 9,
        })
    );
}

#[test]
fn expect_budget_parses_every_budget_name() {
    for &budget in Budget::ALL {
        assert_eq!(
            step(&format!(
                "step = \"expect_budget\"\nbudget = \"{budget}\"\nmetric = \"m\""
            )),
            Step::ExpectBudget(ExpectBudget {
                budget,
                metric: "m".to_owned(),
            })
        );
    }
}

#[test]
fn measure_parses_both_markers() {
    for (text, marker) in [("start", Marker::Start), ("stop", Marker::Stop)] {
        assert_eq!(
            step(&format!(
                "step = \"measure\"\nname = \"t\"\nmarker = \"{text}\""
            )),
            Step::Measure(Measure {
                name: "t".to_owned(),
                marker,
            })
        );
    }
}

#[test]
fn settle_and_quit_parse_with_and_without_a_timeout() {
    assert_eq!(
        step("step = \"settle\""),
        Step::Settle(Settle {
            timeout_ms: DEFAULT_TIMEOUT_MS,
        })
    );
    assert_eq!(
        step("step = \"settle\"\ntimeout_ms = 4"),
        Step::Settle(Settle { timeout_ms: 4 })
    );
    assert_eq!(
        step("step = \"quit\"\ntimeout_ms = 5"),
        Step::Quit(Quit { timeout_ms: 5 })
    );
}

#[test]
fn every_profile_parses() {
    for &profile in Profile::ALL {
        let scenario = parse(&BASE.replace(
            "profiles = [\"default\", \"deterministic\"]",
            &format!("profiles = [\"{profile}\"]"),
        ));
        assert_eq!(scenario.profiles, [profile]);
    }
}

#[test]
fn key_names_round_trip_between_text_and_keys() {
    for name in KeyName::NAMES {
        let key: KeyName = name.parse().expect("a named key should parse");
        assert_eq!(key.name(), Some(name));
        assert_eq!(key.to_string(), name);
    }
    for character in ['y', 'Y', '/', ' ', '?', 'é', '中'] {
        let key: KeyName = character.to_string().parse().expect("a character key");
        assert_eq!(key, KeyName::Char(character));
        assert_eq!(key.name(), None);
        assert_eq!(key.to_string(), character.to_string());
    }
}

#[test]
fn ambiguous_key_names_are_rejected() {
    for text in [
        "", "Enter", "ENTER", "f5", "ab", "enter ", " enter", "\u{1b}", "\n", "\t", "\u{7f}",
    ] {
        assert!(
            text.parse::<KeyName>().is_err(),
            "{text:?} must not be accepted as a key"
        );
    }
    let message = rejection(&format!(
        "{HEAD}[[steps]]\nstep = \"key\"\nkey = \"Enter\"\n"
    ));
    assert!(
        message.contains("unknown key") && message.contains("page_down"),
        "the error should list the valid names: {message}"
    );
}

#[test]
fn malformed_regions_are_rejected() {
    for region in [
        "\"footer\"",
        "{ rows = [1] }",
        "{ rows = [1, 2, 3] }",
        "{ rows = [\"a\", \"b\"] }",
        "{ rows = [-1, 2] }",
        "{ rows = [1, 70000] }",
        "{ columns = [1, 2] }",
        "{ header = true }",
        "{}",
    ] {
        let source =
            format!("{HEAD}[[steps]]\nstep = \"wait_text\"\ntext = \"x\"\nregion = {region}\n");
        assert!(
            Scenario::from_toml_str(&source).is_err(),
            "region {region} must be rejected"
        );
    }
}

#[test]
fn field_predicates_take_exactly_one_known_operator() {
    for predicate in [
        "{ eq = 1, min = 2 }",
        "{ min = 1, max = 2 }",
        "{ gt = 1 }",
        "{ eq = -1 }",
        "{ eq = \"one\" }",
        "{}",
        "5",
    ] {
        let source = format!(
            "{HEAD}[[steps]]\nstep = \"wait_event\"\nevent = \"frame\"\nfields = {{ seq = {predicate} }}\n"
        );
        assert!(
            Scenario::from_toml_str(&source).is_err(),
            "predicate {predicate} must be rejected"
        );
    }
}

#[test]
fn unknown_fields_are_rejected_at_the_top_level_in_steps_and_in_nested_tables() {
    let step = "[[steps]]\nstep = \"settle\"\n";
    let cases = [
        ("top level", format!("{HEAD}bogus = 1\n{step}")),
        (
            "terminal table",
            format!("{HEAD}[terminal]\ncols = 80\nrows = 24\nbogus = 1\n{step}"),
        ),
        (
            "step",
            format!("{HEAD}[[steps]]\nstep = \"settle\"\nbogus = 1\n"),
        ),
        (
            "step with a shared shape",
            format!("{HEAD}[[steps]]\nstep = \"wait_fs_present\"\npath = \"a\"\nbogus = 1\n"),
        ),
        (
            "region table",
            format!(
                "{HEAD}[[steps]]\nstep = \"wait_text\"\ntext = \"x\"\nregion = {{ bogus = 1 }}\n"
            ),
        ),
        (
            "event field table",
            format!(
                "{HEAD}[[steps]]\nstep = \"wait_event\"\nevent = \"frame\"\n\
                 fields = {{ bogus = {{ eq = 1 }} }}\n"
            ),
        ),
        (
            "field predicate table",
            format!(
                "{HEAD}[[steps]]\nstep = \"wait_event\"\nevent = \"frame\"\n\
                 fields = {{ seq = {{ bogus = 1 }} }}\n"
            ),
        ),
        (
            "budgets table",
            format!("{HEAD}[budgets]\nbogus = 1\n{step}"),
        ),
        ("step kind", format!("{HEAD}[[steps]]\nstep = \"bogus\"\n")),
        (
            "profile",
            format!("{}\n{step}", HEAD.replace("\"default\"", "\"bogus\"")),
        ),
    ];
    for (place, source) in cases {
        let message = rejection(&source);
        assert!(
            message.contains("bogus"),
            "{place}: the error should name the unknown value: {message}"
        );
    }
}

#[test]
fn missing_required_fields_are_rejected() {
    let cases = [
        ("steps", HEAD.to_owned()),
        (
            "profiles",
            HEAD.replace("profiles = [\"default\"]\n", "") + "[[steps]]\nstep = \"settle\"\n",
        ),
        ("step", format!("{HEAD}[[steps]]\ntimeout_ms = 1\n")),
        (
            "kind",
            format!("{HEAD}[[steps]]\nstep = \"delete\"\nname = \"x\"\n"),
        ),
        (
            "state",
            format!("{HEAD}[[steps]]\nstep = \"wait_header\"\n"),
        ),
        (
            "terminal_restored",
            format!("{HEAD}[[steps]]\nstep = \"expect_exit\"\ncode = 0\nresidue = \"none\"\n"),
        ),
        (
            "rows",
            format!("{HEAD}[terminal]\ncols = 80\n[[steps]]\nstep = \"settle\"\n"),
        ),
    ];
    for (field, source) in cases {
        let message = rejection(&source);
        assert!(
            message.contains("missing field") && message.contains(field),
            "a document without `{field}` should be rejected for it: {message}"
        );
    }
}

#[test]
fn wrongly_typed_values_are_rejected() {
    let step = "[[steps]]\nstep = \"settle\"\n";
    for (place, source) in [
        (
            "negative timeout",
            format!("{HEAD}[[steps]]\nstep = \"settle\"\ntimeout_ms = -1\n"),
        ),
        (
            "text timeout",
            format!("{HEAD}[[steps]]\nstep = \"settle\"\ntimeout_ms = \"5\"\n"),
        ),
        (
            "oversized column count",
            format!("{HEAD}[terminal]\ncols = 70000\nrows = 24\n{step}"),
        ),
        (
            "text schema_version",
            format!(
                "{}\n{step}",
                HEAD.replace("schema_version = 1", "schema_version = \"1\"")
            ),
        ),
        (
            "text sentinels",
            format!("{HEAD}sentinels = \"keep-a.bin\"\n{step}"),
        ),
    ] {
        assert!(
            Scenario::from_toml_str(&source).is_err(),
            "{place} must be rejected"
        );
    }
}

#[test]
fn a_scenario_survives_a_round_trip_through_toml() {
    for source in [BASE, ALL_STEPS] {
        let scenario = parse(source);
        let rendered = toml::to_string(&scenario).expect("a scenario should serialize");
        assert_eq!(
            parse(&rendered),
            scenario,
            "the rendered document was:\n{rendered}"
        );
    }
}

#[test]
fn from_path_reads_a_scenario_file() {
    let directory = TempDir::new("read");
    let path = directory.path().join("scenario.toml");
    fs::write(&path, BASE).expect("the scenario file should be writable");

    assert_eq!(
        Scenario::from_path(&path).expect("the file should load"),
        parse(BASE)
    );
}

#[test]
fn from_path_names_the_file_it_could_not_read() {
    let directory = TempDir::new("missing");
    let path = directory.path().join("absent.toml");

    let error = Scenario::from_path(&path).expect_err("a missing file cannot load");

    assert!(matches!(error, LoadError::Read { .. }), "{error}");
    assert!(error.to_string().contains("absent.toml"), "{error}");
}

#[test]
fn from_path_names_the_file_and_line_of_a_syntax_error() {
    let directory = TempDir::new("syntax");
    let path = directory.path().join("broken.toml");
    fs::write(&path, format!("{HEAD}[[steps]\nstep = \"settle\"\n"))
        .expect("the scenario file should be writable");

    let error = Scenario::from_path(&path).expect_err("a syntax error cannot load");

    assert!(matches!(error, LoadError::Parse { .. }), "{error}");
    let message = error.to_string();
    assert!(message.contains("broken.toml"), "{message}");
    assert!(message.contains("line 6"), "{message}");
}
