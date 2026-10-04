//! The free-space reserve behind the `scan-store-quota` scenario, which the scenario's steps cannot
//! assert: run it against a real attached volume and, once its scan has stopped at the scan-store
//! quota (75% of the free space at the start), check that the volume still has about a quarter of
//! that free space.
//!
//! Attaching a volume needs `EXCISE_HARNESS_PRIVILEGED=1` (see `excise_harness::fixture::volume`);
//! without it the test reports itself skipped. The test reads the variable and never sets it:
//!
//! ```console
//! EXCISE_HARNESS_PRIVILEGED=1 cargo test --locked --test harness_scan_store_quota
//! ```
#![cfg(unix)]

use std::path::Path;

use excise_harness::{
    fixture::{Fixtures, PRIVILEGED_ENV, PrivilegedOptIn},
    report::Verdict,
    runner::{LatencyScale, RunRequest, run_scenario, work_base},
    scenario::Scenario,
};

const SCENARIO: &str = "crates/excise-harness/scenarios/scan-store-quota.toml";

/// The bytes an unprivileged process may still use on the volume at `path`.
fn free_bytes(path: &Path) -> Result<u64, Box<dyn std::error::Error>> {
    let statistics = rustix::fs::statvfs(path)?;
    Ok(statistics.f_bavail.saturating_mul(statistics.f_frsize))
}

#[test]
fn a_scan_stopped_at_the_quota_leaves_a_quarter_of_the_free_space()
-> Result<(), Box<dyn std::error::Error>> {
    let Some(opt_in) = PrivilegedOptIn::from_env() else {
        eprintln!("skipped: {PRIVILEGED_ENV}=1 is not set");
        return Ok(());
    };
    let scenario = Scenario::from_path(Path::new(env!("CARGO_MANIFEST_DIR")).join(SCENARIO))?;
    scenario.validate()?;
    let work = tempfile::Builder::new()
        .prefix("xh-quota-")
        .tempdir_in(work_base())?;
    // The volume stays attached until `fixture` drops, also when an assertion below fails.
    let mut fixture = Fixtures::bundled().run_copy(&scenario.fixture, work.path())?;
    fixture.attach_volumes(opt_in)?;
    let mount = match fixture.attached_mount_points()[..] {
        [mount] => mount.to_path_buf(),
        ref mounts => panic!(
            "the fixture should declare one volume part, not {}",
            mounts.len()
        ),
    };

    let free_before = free_bytes(&mount)?;
    let report = run_scenario(&RunRequest {
        scenario: &scenario,
        profile: scenario.profiles[0],
        binary: Path::new(env!("CARGO_BIN_EXE_excise")),
        fixture_root: fixture.root(),
        scan_store_dir: Some(&mount.join("store")),
        work_dir: work.path(),
        bundle_dir: Some(&work.path().join("bundle")),
        repro_command: "EXCISE_HARNESS_PRIVILEGED=1 cargo test --locked --test harness_scan_store_quota",
        fixture_seed: fixture.plan().spec().seed,
        keep_scratch: false,
        latency_scale: LatencyScale::STRICT,
        timing_informational: false,
    });
    let free_after = free_bytes(&mount)?;

    assert!(
        report.verdict == Verdict::Pass,
        "verdict {:?}, failure {:?}, error {:?}",
        report.verdict,
        report.failure,
        report.error
    );
    // A fifth, not a quarter: the file system's own metadata takes some of the reserve.
    assert!(
        free_after.saturating_mul(5) >= free_before,
        "the reserve is gone: {free_after} bytes free after the scan, {free_before} before"
    );
    fixture.remove()?;
    Ok(())
}
