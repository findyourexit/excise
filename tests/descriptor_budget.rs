//! Proves the scan store's own descriptor bound independently of the startup
//! `RLIMIT_NOFILE` raise: with both the soft and hard limits lowered to 256 before
//! `excise` starts (so the raise has nothing left to grant), a headless scan of
//! `tiny-files-50k` still completes and matches the oracle. On the base this fails with exit 70
//! and no report ("Too many open files", os error 24); `Command::pre_exec` needs `unsafe`,
//! which is denied, so the limit is lowered with a `/bin/sh` wrapper script instead
//! (`ulimit -Hn 256` and `ulimit -Sn 256`, then `exec` of the real binary with the same
//! arguments).
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use excise_harness::{
    fixture::Fixtures,
    headless::{Expectations, SuiteOptions, run_suite},
    report::Tier,
    runner::work_base,
    scenario::Profile,
};

/// A unique directory under the harness work area, removed when dropped.
struct Workspace(PathBuf);

impl Workspace {
    fn new(name: &str) -> Self {
        let path = work_base().join(format!("xh-test-{name}-{}", std::process::id()));
        fs::create_dir_all(&path).expect("the work area is writable");
        Self(path)
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Writes a `/bin/sh` wrapper that lowers both the soft and hard `RLIMIT_NOFILE` before
/// `exec`-ing the real binary with the same arguments: the one way to constrain a child's
/// descriptor limit without `Command::pre_exec`'s `unsafe`.
fn ulimit_wrapper(work: &Path, limit: u32, binary: &Path) -> PathBuf {
    let path = work.join("ulimit-wrapper.sh");
    fs::write(
        &path,
        format!(
            "#!/bin/sh\nulimit -Hn {limit}\nulimit -Sn {limit}\nexec \"{}\" \"$@\"\n",
            binary.display()
        ),
    )
    .expect("the wrapper script should write");
    let mut permissions = fs::metadata(&path)
        .expect("the wrapper script should exist")
        .permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&path, permissions).expect("the wrapper script should be executable");
    path
}

#[test]
fn a_256_descriptor_limit_still_completes_a_large_scan() {
    let work = Workspace::new("descriptor-budget");
    let binary = Path::new(env!("CARGO_BIN_EXE_excise"));
    let wrapper = ulimit_wrapper(&work.0, 256, binary);

    let report = run_suite(
        &SuiteOptions {
            binary: wrapper,
            fixtures: Fixtures::bundled(),
            tier: Tier::Full,
            fixture_ids: vec!["tiny-files-50k".to_owned()],
            classes: Vec::new(),
            profile: Profile::Deterministic,
            repeat: 0,
            timeout: Duration::from_secs(90),
            keep: false,
            out_root: work.0.join("out"),
            work_dir: Some(work.0.join("scratch")),
            git_sha: "0".repeat(40),
            privileged: None,
            expectations: Expectations::bundled().expect("the shipped expectations are valid"),
            timing_informational: false,
        },
        |_| {},
    )
    .expect("the suite runs");

    // With no measured rounds (`repeat: 0`) the fixture's scan-time budget is never checked,
    // so this asserts the oracle diff itself rather than the overall verdict.
    assert_eq!(report.fixtures.len(), 1, "exactly the named fixture ran");
    let fixture = &report.fixtures[0];
    assert_eq!(
        fixture.error, None,
        "the harness itself should run the fixture without error"
    );
    assert!(
        !fixture.diffs.is_empty(),
        "at least one round should have been scanned and diffed against the oracle"
    );
    assert!(
        fixture.first_failure().is_none(),
        "a 256-descriptor hard limit should not stop the scan store's own bound from matching \
         the oracle (on the base this is exit 70, \"Too many open files\", no report): {:?}",
        fixture.first_failure()
    );
    for (round, diff) in &fixture.diffs {
        assert_eq!(
            diff.compared.reported, diff.compared.entries,
            "round {round}: the report should list every entry the scan covers, not stop \
             early with \"too many open files\""
        );
    }
    assert!(
        fixture.entries > 40_000,
        "this should be the large fixture, not a smaller stand-in: {} entries",
        fixture.entries
    );
}
