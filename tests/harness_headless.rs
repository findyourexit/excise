//! Runs the harness's headless oracle diff against this crate's own `excise` binary, on the cheap
//! bundled fixtures that need no privileges.
//!
//! Each fixture is scanned once with `--format json`, and the report the scan writes is held to
//! the oracle of the tree under the accounting contract. The timings against `du -sk` and the
//! large fixtures are `cargo xtask headless`.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Mutex, MutexGuard, PoisonError,
        atomic::{AtomicU32, Ordering},
    },
    time::Duration,
};

use excise_harness::{
    fixture::{FixtureCache, FixtureSpec, Fixtures},
    headless::{Expectations, SuiteOptions, SuiteReport, run_suite},
    report::{Tier, Verdict},
    runner::work_base,
    scenario::Profile,
};

/// One suite at a time, so that a script written here is never executed while another thread of
/// this test process may hold a descriptor that is open for writing (`ETXTBSY` on Linux).
fn serial() -> MutexGuard<'static, ()> {
    static SUITE: Mutex<()> = Mutex::new(());
    SUITE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A unique directory under the harness work area, removed when dropped.
struct Workspace(PathBuf);

impl Workspace {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = work_base().join(format!(
            "xh-test-{}-{}",
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

/// Scans each of `fixtures` once with `binary`: the oracle diff, and no timings. The fixture cache
/// is `work`'s `cache` directory, so a test can tell whether the suite built anything in it.
fn suite(binary: &Path, fixtures: &[&str], work: &Workspace) -> SuiteReport {
    run_suite(
        &SuiteOptions {
            binary: binary.to_path_buf(),
            fixtures: Fixtures::new(
                FixtureSpec::bundled_dir(),
                FixtureCache::at(work.0.join("cache")),
            ),
            tier: Tier::Quick,
            fixture_ids: fixtures.iter().map(|id| (*id).to_owned()).collect(),
            classes: Vec::new(),
            profile: Profile::Deterministic,
            repeat: 0,
            timeout: Duration::from_secs(120),
            keep: false,
            out_root: work.0.join("out"),
            work_dir: Some(work.0.join("scratch")),
            git_sha: "0".repeat(40),
            privileged: None,
            expectations: Expectations::bundled().expect("the shipped expectations are valid"),
        },
        |_| {},
    )
    .expect("the suite runs")
}

#[test]
fn the_report_of_each_cheap_fixture_matches_its_oracle_under_the_accounting_contract() {
    let _suite = serial();
    let work = Workspace::new();
    let fixtures = ["identity-small", "wide-1k", "mount-boundary"];

    let report = suite(Path::new(env!("CARGO_BIN_EXE_excise")), &fixtures, &work);

    assert_eq!(report.fixtures.len(), fixtures.len());
    for fixture in &report.fixtures {
        assert_eq!(
            fixture.verdict,
            Verdict::Pass,
            "{}: {:?} (error: {:?})",
            fixture.fixture,
            fixture.first_failure(),
            fixture.error
        );
        assert!(
            !fixture.diffs.is_empty(),
            "{} was not diffed",
            fixture.fixture
        );
        assert!(fixture.entries > 1, "{} has a tree", fixture.fixture);
        assert!(
            fixture
                .diffs
                .iter()
                .all(|(_, diff)| diff.compared.reported == diff.compared.entries),
            "{}: the report lists every entry the scan covers",
            fixture.fixture
        );
    }
    assert!(report.is_success());
}

/// The suite reads the cached master of a fixture, generated once and then reused, except for a
/// fixture that `cargo clean` could not remove, here one with directories that cannot be listed:
/// that one is scanned in a disposable copy and never cached, because the cache lives below the
/// target directory.
#[test]
fn the_suite_caches_only_the_fixtures_that_cargo_clean_can_remove() {
    let _suite = serial();
    let work = Workspace::new();
    let binary = Path::new(env!("CARGO_BIN_EXE_excise"));

    let hostile = suite(binary, &["hostile-small"], &work);
    let [fixture] = hostile.fixtures.as_slice() else {
        panic!("one fixture ran, not {}", hostile.fixtures.len());
    };
    assert!(
        fixture.error.is_none(),
        "hostile-small ran: {:?}",
        fixture.error
    );
    assert!(!fixture.diffs.is_empty(), "hostile-small was scanned");
    assert!(
        !work.0.join("cache").exists(),
        "hostile-small was cached below the target directory"
    );

    let first = suite(binary, &["identity-small"], &work);
    let second = suite(binary, &["identity-small"], &work);
    assert!(
        first.fixtures[0].generation.is_some(),
        "the first run generates the master"
    );
    assert!(
        second.fixtures[0].generation.is_none(),
        "the second run reuses it"
    );
}

/// A binary that writes a report that is not a scan report, as the product's `--output` does.
#[cfg(unix)]
#[test]
fn a_binary_whose_report_breaks_the_published_schema_fails_the_run() {
    use std::os::unix::fs::PermissionsExt as _;

    use excise_harness::headless::DiscrepancyKind;

    let _suite = serial();
    let work = Workspace::new();
    let fake = work.0.join("excise-that-lies");
    // `--format json --output <report> <root>`: the report goes where the fourth argument says.
    fs::write(
        &fake,
        "#!/bin/sh\nprintf '{\"version\": 3}' > \"$4\"\nexit 0\n",
    )
    .expect("a script is written");
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).expect("it is made executable");

    let report = suite(&fake, &["identity-small"], &work);

    assert!(!report.is_success());
    let fixture = &report.fixtures[0];
    assert_eq!(fixture.verdict, Verdict::Fail, "{:?}", fixture.error);
    assert!(
        fixture.kinds().contains(&DiscrepancyKind::InvalidReport),
        "{:?}",
        fixture.kinds()
    );
}
