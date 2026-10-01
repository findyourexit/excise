//! Runs the harness's ratio comparisons (`cargo xtask compare`) against this crate's own `excise`
//! binary: a cheap smoke proof that `run_comparison` and `load_comparisons` work end to end, and a
//! parse/validate check of every bundled `comparisons/*.toml` file. The real F2 evidence (the
//! shipped `node-modules-2k`/`node-modules-50k` comparisons) is `cargo xtask compare --full`
//! and `--nightly`: they take tens of seconds to tens of minutes, far past what `cargo test` should
//! spend.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU32, Ordering},
};

use excise_harness::{
    comparison::{CompareOptions, load_comparisons, run_comparison},
    fixture::{FixtureCache, FixtureSpec, Fixtures},
    runner::work_base,
};

/// A unique directory under the harness work area, removed when dropped.
struct Workspace(PathBuf);

impl Workspace {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = work_base().join(format!(
            "xh-ratio-test-{}-{}",
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

/// Every comparison this crate ships parses, validates, and is named after its file: the same
/// check `cargo xtask compare` applies via `load_comparisons`, kept here so a typo in
/// `comparisons/*.toml` fails fast in `cargo test` instead of only in a tens-of-minutes nightly
/// run.
#[test]
fn every_bundled_comparison_parses_and_validates() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("crates/excise-harness/comparisons");
    let comparisons = load_comparisons(&dir).expect("every comparison file loads and validates");
    assert!(
        !comparisons.is_empty(),
        "the comparisons directory should not be empty"
    );
    for id in [
        "motion-complete-node-modules-2k",
        "motion-complete-node-modules-2k-slow-terminal",
        "tui-complete-node-modules-2k-default",
        "tui-complete-node-modules-2k-default-slow-terminal",
        "tui-complete-node-modules-2k-reduced-motion",
        "tui-complete-node-modules-2k-reduced-motion-slow-terminal",
        "motion-complete-node-modules-50k",
        "motion-complete-node-modules-50k-slow-terminal",
        "tui-complete-node-modules-50k-default",
        "tui-complete-node-modules-50k-default-slow-terminal",
        "tui-complete-node-modules-50k-reduced-motion",
        "tui-complete-node-modules-50k-reduced-motion-slow-terminal",
    ] {
        assert!(
            comparisons.iter().any(|c| c.name == id),
            "expected a comparison named {id:?}"
        );
    }
}

/// `run_comparison` drives a real `motion_complete_ratio` comparison (default against
/// reduced-motion) end to end on a small, fast fixture, with this crate's own binary on both
/// sides. This only proves the wiring: `run_comparison`, `run_scenario_once`, and the bootstrap
/// summary all ran without a harness error and both sides produced a measurement. It must not
/// assert `Outcome::Passed` or a specific `Verdict`: on a shared CI runner under load, with the
/// debug binary, the measured ratio itself is not a promise (the timing pattern that broke `main`
/// in #155). The real F2 evidence lives in `cargo xtask compare --full`/`--nightly` against the
/// release binary.
#[test]
fn a_motion_ratio_comparison_runs_end_to_end_with_no_harness_error() {
    let work = Workspace::new();
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_excise"));
    let fixtures = Fixtures::new(
        FixtureSpec::bundled_dir(),
        FixtureCache::at(work.0.join("cache")),
    );
    let comparison = excise_harness::comparison::Comparison::from_toml_str(
        "schema_version = 1\nname = \"smoke-motion\"\ndescription = \"smoke test\"\n\
         fixture = \"wide-1k\"\nbudget = \"motion_complete_ratio\"\npairs = 1\n\
         timeout_ms = 20000\n",
    )
    .expect("a comparison");
    comparison.validate().expect("a valid comparison");

    let report = run_comparison(
        &comparison,
        &CompareOptions {
            binary: &binary,
            fixtures: &fixtures,
            work_dir: &work.0,
            seed: 0,
        },
    );

    assert!(
        report.error.is_none(),
        "harness error: {:?}, baseline {:?}, candidate {:?}",
        report.error,
        report.baseline,
        report.candidate
    );
    assert_eq!(report.baseline.len(), 1);
    assert_eq!(report.candidate.len(), 1);
    assert_eq!(
        report.baseline_completed, 1,
        "the baseline run must complete"
    );
    assert_eq!(
        report.candidate_completed, 1,
        "the candidate run must complete"
    );
    assert!(
        report.median_ratio.is_finite() && report.median_ratio > 0.0,
        "median ratio {}",
        report.median_ratio
    );
}

/// The same proof for `tui_complete_ratio` (interactive against headless), which exercises
/// `run_fixture_once` as the baseline side instead of a second interactive run. See
/// [`a_motion_ratio_comparison_runs_end_to_end_with_no_harness_error`] for why this checks
/// wiring, not the ratio's verdict.
#[test]
fn a_tui_ratio_comparison_runs_end_to_end_with_no_harness_error() {
    let work = Workspace::new();
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_excise"));
    let fixtures = Fixtures::new(
        FixtureSpec::bundled_dir(),
        FixtureCache::at(work.0.join("cache")),
    );
    let comparison = excise_harness::comparison::Comparison::from_toml_str(
        "schema_version = 1\nname = \"smoke-tui\"\ndescription = \"smoke test\"\n\
         fixture = \"wide-1k\"\nbudget = \"tui_complete_ratio\"\nprofile = \"default\"\n\
         pairs = 1\ntimeout_ms = 20000\n",
    )
    .expect("a comparison");
    comparison.validate().expect("a valid comparison");

    let report = run_comparison(
        &comparison,
        &CompareOptions {
            binary: &binary,
            fixtures: &fixtures,
            work_dir: &work.0,
            seed: 0,
        },
    );

    assert!(
        report.error.is_none(),
        "harness error: {:?}, baseline {:?}, candidate {:?}",
        report.error,
        report.baseline,
        report.candidate
    );
    assert_eq!(report.baseline.len(), 1);
    assert_eq!(report.candidate.len(), 1);
    assert_eq!(
        report.baseline_completed, 1,
        "the baseline run must complete"
    );
    assert_eq!(
        report.candidate_completed, 1,
        "the candidate run must complete"
    );
}
