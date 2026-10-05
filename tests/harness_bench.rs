//! Runs the harness's `bench-e2e` comparison with this crate's own `excise` binary as both sides,
//! on a small cacheable fixture with few pairs: fast enough for `cargo test`, and proof that the
//! comparison produces a `harness-ab` document that validates against its schema with nothing
//! blocking when there is, by construction, no regression (baseline and candidate are the same
//! binary). The real baseline-vs-candidate comparison is `cargo xtask bench-e2e`.
//!
//! A comparison in which one fixture id would name two different trees is refused before anything
//! runs: `ab.json` names a fixture by its id alone.

use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU32, Ordering},
    time::Duration,
};

use excise_harness::{
    bench::{BenchOptions, Case, run_bench_e2e},
    fixture::{FixtureCache, FixtureSpec, Fixtures},
    report::{AbVerdict, Document, HarnessAb},
    runner::work_base,
    scenario::{Profile, Scenario},
};

/// A unique directory under the harness work area, removed when dropped.
struct Workspace(PathBuf);

impl Workspace {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = work_base().join(format!(
            "xh-bench-test-{}-{}",
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

#[test]
fn the_same_binary_on_both_sides_validates_and_blocks_nothing() {
    let work = Workspace::new();
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_excise"));

    let fixtures = Fixtures::new(
        FixtureSpec::bundled_dir(),
        FixtureCache::at(work.0.join("cache")),
    );
    let options = BenchOptions {
        baseline_binary: binary.clone(),
        baseline_ref: "test-baseline".to_owned(),
        candidate_binary: binary,
        candidate_ref: "test-candidate".to_owned(),
        fixtures: fixtures.clone(),
        scenario_fixtures: fixtures,
        cases: vec![Case::Fixture {
            id: "wide-1k".to_owned(),
        }],
        pairs: 5,
        seed: 1,
        // Five pairs on a shared machine can judge only a gross difference; the verdict rules
        // themselves are covered by the verdict unit tests. Timing is not judged at all: five pairs
        // of a scan that costs a few tens of milliseconds cannot resolve it, and in CI build
        // sandboxes this same binary has measured 1.25x user CPU and 2.44x wall time against
        // itself with confident intervals. Peak memory moves by a few percent between identical
        // runs on a hosted macOS runner (once 5.3% lower on the second side of all five pairs;
        // 1-2% over twenty pairs, in either direction), so only a move past half of it blocks.
        timing_threshold: f64::INFINITY,
        memory_tolerance: 0.5,
        strict: false,
        timeout: Duration::from_secs(120),
        work_dir: work.0.join("scratch"),
        out_root: work.0.join("out"),
    };

    let report = run_bench_e2e(&options).expect("the comparison runs");

    assert!(
        !report.document.metrics.is_empty(),
        "the comparison should have measured at least one metric"
    );
    assert!(
        report.is_success(),
        "the same binary on both sides must not block: {:#?}",
        report.document.metrics
    );
    for metric in &report.document.metrics {
        assert_ne!(
            metric.verdict,
            AbVerdict::Block,
            "{} blocked: {:?}",
            metric.name,
            metric
        );
    }

    let rendered = report
        .document
        .to_json_pretty()
        .expect("the document renders");
    let instance: serde_json::Value =
        serde_json::from_str(&rendered).expect("the rendered document is JSON");
    let schema: serde_json::Value =
        serde_json::from_str(HarnessAb::SCHEMA_JSON).expect("the schema is valid JSON");
    let validator = jsonschema::draft202012::options()
        .build(&schema)
        .expect("the schema compiles");
    let errors: Vec<String> = validator
        .iter_errors(&instance)
        .map(|error| error.to_string())
        .collect();
    assert!(
        errors.is_empty(),
        "the document must validate against {}: {errors:#?}",
        HarnessAb::SCHEMA_ID
    );
}

/// A fixture id names one tree in a comparison. A directory of one's own specs (`--fixture-dir`)
/// can hold a spec with the id of the bundled fixture that a selected scenario runs on, and
/// `ab.json` names a fixture by its id alone: the document would show one of the two trees and
/// say nothing of the other. The comparison is refused before it makes a directory or runs a
/// process. This uses real binaries: a missing one would fail the comparison for another reason,
/// and the test would pass for the wrong one.
#[test]
fn a_fixture_id_that_names_two_trees_is_refused_before_anything_runs() {
    let work = Workspace::new();
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_excise"));

    // A bundled scenario, and the bundled fixture it runs on.
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("crates/excise-harness/scenarios")
        .join("navigate-and-quit.toml");
    let scenario = Scenario::from_path(&path).expect("the bundled scenario loads");
    scenario.validate().expect("a valid scenario");
    let id = scenario.fixture.clone();
    let name = scenario.name.clone();
    let bundled = FixtureSpec::load_bundled(&id).expect("a bundled fixture");

    // Another tree under the same id, in a directory of specs of one's own.
    let spec = format!(
        r#"
schema_version = 1
id = "{id}"
description = "another tree, under the id of a bundled fixture"
seed = 1

[[parts]]
kind = "tree"
root = "another-tree"
depth = 0
files_per_dir = 1
"#
    );
    let other = FixtureSpec::from_toml_str(&spec).expect("a valid spec");
    assert_ne!(
        other.canonical_json(),
        bundled.canonical_json(),
        "the two specs name two trees"
    );
    let specs_dir = work.0.join("specs");
    fs::create_dir_all(&specs_dir).expect("a directory of specs");
    fs::write(specs_dir.join(format!("{id}.toml")), spec).expect("the spec is written");

    let cache = FixtureCache::at(work.0.join("cache"));
    let options = BenchOptions {
        baseline_binary: binary.clone(),
        baseline_ref: "test-baseline".to_owned(),
        candidate_binary: binary,
        candidate_ref: "test-candidate".to_owned(),
        fixtures: Fixtures::new(&specs_dir, cache.clone()),
        scenario_fixtures: Fixtures::new(FixtureSpec::bundled_dir(), cache),
        cases: vec![
            Case::Fixture { id: id.clone() },
            Case::Scenario {
                scenario: Box::new(scenario),
                profile: Profile::Deterministic,
            },
        ],
        pairs: 1,
        seed: 1,
        timing_threshold: f64::INFINITY,
        memory_tolerance: 0.5,
        strict: false,
        timeout: Duration::from_secs(120),
        work_dir: work.0.join("scratch"),
        out_root: work.0.join("out"),
    };

    let error = run_bench_e2e(&options).expect_err("an id that names two trees is refused");

    let message = error.to_string();
    assert!(
        message.contains(&id) && message.contains(&name),
        "the refusal names the id and the scenario: {message}"
    );
    assert!(
        !options.work_dir.exists(),
        "the refusal came after a scratch area was made"
    );
    assert!(
        !options.out_root.exists(),
        "the refusal came after an output directory was made"
    );
}
