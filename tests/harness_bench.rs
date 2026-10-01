//! Runs the harness's `bench-e2e` comparison with this crate's own `excise` binary as both sides,
//! on a small cacheable fixture with few pairs: fast enough for `cargo test`, and proof that the
//! comparison produces a `harness-ab` document that validates against its schema with nothing
//! blocking when there is, by construction, no regression (baseline and candidate are the same
//! binary). The real baseline-vs-candidate comparison is `cargo xtask bench-e2e`.

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

    let options = BenchOptions {
        baseline_binary: binary.clone(),
        baseline_ref: "test-baseline".to_owned(),
        candidate_binary: binary,
        candidate_ref: "test-candidate".to_owned(),
        fixtures: Fixtures::new(
            FixtureSpec::bundled_dir(),
            FixtureCache::at(work.0.join("cache")),
        ),
        cases: vec![Case::Fixture {
            id: "wide-1k".to_owned(),
        }],
        pairs: 5,
        seed: 1,
        timing_threshold: 0.20,
        memory_tolerance: 0.05,
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
