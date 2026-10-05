//! Counts cheap fixtures with this crate's own `excise` binary: the proof that the counts suite
//! writes a `harness-counts` document that validates against its schema, reads back as it was
//! written, and is the same on every run. The real suite is `cargo xtask counts`.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU32, Ordering},
    time::Duration,
};

#[cfg(unix)]
use excise_harness::counts::CountsError;
use excise_harness::{
    counts::{Commit, CountsOptions, artifact::read_untrusted, metric, run_counts},
    fixture::{FixtureCache, FixtureSpec, Fixtures},
    report::{Document, HarnessCounts},
    runner::work_base,
};

/// A unique directory under the harness work area, removed when dropped.
struct Workspace(PathBuf);

impl Workspace {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = work_base().join(format!(
            "xh-counts-test-{}-{}",
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

fn options(work: &Workspace, binary: &Path, out: &str, repeat: u32) -> CountsOptions {
    CountsOptions {
        binary: binary.to_path_buf(),
        fixtures: Fixtures::new(
            FixtureSpec::bundled_dir(),
            FixtureCache::at(work.0.join("cache")),
        ),
        fixture_ids: vec!["wide-1k".to_owned(), "identity-small".to_owned()],
        repeat,
        timeout: Duration::from_secs(120),
        work_dir: work.0.join("scratch"),
        out: work.0.join(out),
        commit: Commit {
            sha: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            committed_at: "2026-10-05T10:35:18+11:00".to_owned(),
        },
        pull_request: None,
    }
}

/// The messages that `document` breaks its schema with: none for a valid one.
fn violations(text: &str) -> Vec<String> {
    let instance: serde_json::Value = serde_json::from_str(text).expect("the document is JSON");
    let schema: serde_json::Value =
        serde_json::from_str(HarnessCounts::SCHEMA_JSON).expect("the schema is JSON");
    jsonschema::draft202012::options()
        .should_validate_formats(true)
        .build(&schema)
        .expect("the schema compiles")
        .iter_errors(&instance)
        .map(|error| error.to_string())
        .collect()
}

#[test]
fn the_counts_of_two_runs_validate_read_back_and_are_identical() {
    let work = Workspace::new();
    let binary = Path::new(env!("CARGO_BIN_EXE_excise"));

    // The first run counts every fixture twice and fails if any count differs between the two.
    let first = run_counts(&options(&work, binary, "first.json", 2), |_| {})
        .expect("the counts are taken, and agree across both runs");
    // The second run is a separate invocation: its document must be the first one's, byte for
    // byte, because nothing in it depends on the run.
    let second = run_counts(&options(&work, binary, "second.json", 1), |_| {})
        .expect("the counts are taken again");

    let written = fs::read_to_string(&first.document_path).expect("the document was written");
    assert_eq!(
        violations(&written),
        Vec::<String>::new(),
        "the file that the writer produced must validate against {}",
        HarnessCounts::SCHEMA_ID
    );
    assert_eq!(
        read_untrusted(&first.document_path).expect("the strict reader accepts the writer's file"),
        first.document,
        "the document reads back as it was written"
    );
    assert_eq!(
        written,
        fs::read_to_string(&second.document_path).expect("the second document was written"),
        "two runs write the same document"
    );
    assert_eq!(
        HarnessCounts::from_json_str(&written).expect("the canonical form parses"),
        first.document
    );

    let ids: Vec<&str> = first
        .document
        .cases
        .iter()
        .map(|case| case.fixture.id.as_str())
        .collect();
    assert_eq!(
        ids,
        ["wide-1k", "identity-small"],
        "in the order they were named"
    );
    let wide = &first.document.cases[0].metrics;
    assert!(wide[metric::ENTRIES] >= 1_000, "{wide:?}");
    assert!(wide[metric::SCAN_STORE_BYTES] > 0, "{wide:?}");
    assert_eq!(wide[metric::RESIDUE_FILES], 0, "{wide:?}");
    // Only counts that do not depend on timing, or on when the program was looked at, are taken.
    assert_eq!(
        wide.keys().map(String::as_str).collect::<Vec<_>>(),
        [
            metric::ENTRIES,
            metric::RESIDUE_FILES,
            metric::SCAN_STORE_BYTES
        ],
        "{wide:?}"
    );
    assert_eq!(
        first.document.context.git_sha,
        "0123456789abcdef0123456789abcdef01234567"
    );
    assert!(first.document.context.pull_request.is_none());
}

/// A program that does not scan is not counted: its exit status is checked, so a binary that fails
/// at once cannot leave a document of zeros behind.
#[test]
#[cfg(unix)]
fn a_program_that_fails_is_an_error_not_a_count_of_nothing() {
    let work = Workspace::new();
    let Some(program) = ["/usr/bin/false", "/bin/false"]
        .into_iter()
        .map(Path::new)
        .find(|path| path.is_file())
    else {
        // A system without `false` at either path has nothing to run.
        return;
    };

    let result = run_counts(&options(&work, program, "never.json", 1), |_| {});

    match result {
        Err(CountsError::Failed { fixture, what }) => {
            assert_eq!(fixture, "wide-1k");
            assert!(what.contains("exited with code 1"), "{what}");
        }
        other => panic!("expected the scan to be reported as failed, got {other:?}"),
    }
    assert!(
        !work.0.join("never.json").exists(),
        "no document is written for a run that failed"
    );
}
