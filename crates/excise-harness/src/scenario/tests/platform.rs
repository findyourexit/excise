//! Platform selection and per-platform expected failure: [`Scenario::runs_on`],
//! [`Scenario::effective_platforms`], and [`Scenario::expect_on`].
//!
//! These are plain functions of the scenario and an explicit `os`, so every test below checks
//! every platform from whichever host runs the test, rather than depending on
//! `std::env::consts::OS`.

use super::valid;
use crate::scenario::Expect;

#[test]
fn an_absent_platforms_list_runs_everywhere_the_harness_knows() {
    let scenario = valid();

    assert_eq!(
        scenario.effective_platforms(),
        ["linux", "macos", "windows"]
    );
    for os in ["linux", "macos", "windows"] {
        assert!(scenario.runs_on(os), "{os}");
    }
    assert!(!scenario.runs_on("plan9"));
}

#[test]
fn a_declared_platforms_list_narrows_where_the_scenario_runs() {
    let mut scenario = valid();
    scenario.platforms = Some(vec!["linux".to_owned(), "macos".to_owned()]);

    assert_eq!(scenario.effective_platforms(), ["linux", "macos"]);
    assert!(scenario.runs_on("linux"));
    assert!(scenario.runs_on("macos"));
    assert!(!scenario.runs_on("windows"), "windows is not declared");
}

#[test]
fn an_ordinary_pass_applies_on_every_platform() {
    let scenario = valid();

    for os in ["linux", "macos", "windows", "plan9"] {
        assert_eq!(scenario.expect_on(os), Expect::Pass, "{os}");
    }
}

#[test]
fn an_expected_failure_with_no_fails_on_applies_everywhere_in_platforms() {
    let mut scenario = valid();
    scenario.expect = Expect::Fail;
    scenario.slice = Some("X2".to_owned());
    scenario.platforms = Some(vec!["linux".to_owned(), "macos".to_owned()]);

    assert_eq!(scenario.expect_on("linux"), Expect::Fail);
    assert_eq!(scenario.expect_on("macos"), Expect::Fail);
    // `fails_on` defaults to `platforms`, which excludes `windows` here: an ordinary pass.
    assert_eq!(scenario.expect_on("windows"), Expect::Pass);
}

#[test]
fn an_expected_failure_applies_only_on_fails_on() {
    let mut scenario = valid();
    scenario.expect = Expect::Fail;
    scenario.slice = Some("X2".to_owned());
    scenario.fails_on = Some(vec!["linux".to_owned()]);

    assert_eq!(
        scenario.expect_on("linux"),
        Expect::Fail,
        "named in fails_on"
    );
    assert_eq!(
        scenario.expect_on("macos"),
        Expect::Pass,
        "not named: an ordinary pass"
    );
    assert_eq!(
        scenario.expect_on("windows"),
        Expect::Pass,
        "not named: an ordinary pass"
    );
}
