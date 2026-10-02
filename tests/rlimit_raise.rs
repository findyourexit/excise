//! Proves the soft `RLIMIT_NOFILE` raise at startup, on Linux, where the effective limit can be
//! read from `/proc/<pid>/limits` while the process is still alive. On macOS the unit tests on
//! the pure clamp (`src/os/unix.rs::raised_soft_limit`) suffice: there is no equivalent live-limit
//! inspection in this workspace without `unsafe`, which is denied outside `os/windows.rs`.
//!
//! The wrapper lowers only the soft limit (`ulimit -Sn 64`) so the hard limit stays whatever this
//! host inherited, and the raised value is checked against that same hard limit rather than a
//! hardcoded assumption about the test environment.
#![cfg(target_os = "linux")]

use std::fs;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use excise_harness::{
    fixture::Fixtures,
    runner::work_base,
    safety::{FixtureRoot, Scratch, isolated_env},
    scenario::Profile,
};

/// The target computation's own ceiling (`src/os/unix.rs::OPEN_MAX`): macOS's `setrlimit(2)`
/// rejects `RLIM_INFINITY` for `RLIMIT_NOFILE`, so an unbounded hard limit is capped here
/// instead of requested outright. Duplicated rather than imported: this test exercises the
/// real syscalls through the compiled binary, not the pure function in-process, so it checks
/// the contract against the observed `/proc` value independently of that function's own unit
/// tests.
const OPEN_MAX: u64 = 10_240;

/// The soft limit the wrapper lowers the child to before `exec`, below any hard limit a test
/// host has, so the raise has something to grant.
const LOWERED_SOFT_LIMIT: u64 = 64;

/// Parses one `/proc/<pid>/limits` "Max open files" line into (soft, hard), mapping
/// `unlimited` to `u64::MAX`.
fn parse_nofile_line(line: &str) -> Option<(u64, u64)> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    let parse = |value: &str| -> Option<u64> {
        if value == "unlimited" {
            Some(u64::MAX)
        } else {
            value.parse().ok()
        }
    };
    Some((parse(fields.get(3)?)?, parse(fields.get(4)?)?))
}

fn read_nofile_limits(path: &str) -> Option<(u64, u64)> {
    let text = fs::read_to_string(path).ok()?;
    text.lines()
        .find(|line| line.starts_with("Max open files"))
        .and_then(parse_nofile_line)
}

/// Reads the child's limits once it is the binary itself and has raised its soft limit.
///
/// The child starts as `/bin/sh`: until the shell has lowered its soft limit and `exec`ed the
/// binary, `/proc` shows the limits this test inherited, and right after the `exec` it shows the
/// lowered limit for the moment before the raise. So a reading counts only once the process's
/// name is the binary's and its soft limit has left the lowered value. If that never happens, the
/// last reading taken after the `exec` is returned, so the assertion shows what the binary kept.
fn read_raised_nofile_limits(pid: u32, name: &str, deadline: Instant) -> Option<(u64, u64)> {
    let mut last = None;
    while Instant::now() < deadline {
        let comm = fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
        if comm.trim_end() == name
            && let Some(limits) = read_nofile_limits(&format!("/proc/{pid}/limits"))
        {
            if limits.0 != LOWERED_SOFT_LIMIT {
                return Some(limits);
            }
            last = Some(limits);
        }
        thread::sleep(Duration::from_millis(1));
    }
    last
}

#[test]
fn the_soft_limit_is_raised_toward_the_hard_limit() {
    let (_, test_process_hard) = read_nofile_limits("/proc/self/limits")
        .expect("this test process's own limits should be readable");

    let binary = Path::new(env!("CARGO_BIN_EXE_excise"));
    let master = Fixtures::bundled()
        .master("wide-1k")
        .expect("the bundled fixture materializes");
    let fixture = FixtureRoot::open(&master.root)
        .expect("the fixture carries the ownership marker")
        .path()
        .to_path_buf();

    let work = work_base().join(format!("xh-test-rlimit-raise-{}", std::process::id()));
    fs::create_dir_all(&work).expect("the work area is writable");
    let scratch = Scratch::create(&work).expect("a scratch area can be created");

    let mut command = Command::new("/bin/sh");
    command
        .arg("-c")
        .arg(format!(
            "ulimit -Sn {LOWERED_SOFT_LIMIT} && exec \"{}\" --format json --output \"{}\" \"{}\"",
            binary.display(),
            scratch.report().display(),
            fixture.display()
        ))
        .env_clear()
        .envs(isolated_env(&scratch, Profile::Deterministic, false, None))
        .current_dir(scratch.cwd())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child: Child = command.spawn().expect("the wrapped process should spawn");
    let pid = child.id();
    let name = binary
        .file_name()
        .and_then(|name| name.to_str())
        .expect("the binary has a UTF-8 file name");

    let observed = read_raised_nofile_limits(pid, name, Instant::now() + Duration::from_secs(10));
    let status = child.wait().expect("the process should be waited for");
    let _ = fs::remove_dir_all(&work);

    assert!(
        status.success(),
        "a scan under a lowered-then-raised soft limit should still succeed: {status:?}"
    );
    let (soft, hard) = observed.expect("the child's limits should be readable while it ran");
    assert_eq!(
        hard, test_process_hard,
        "the wrapper lowers only the soft limit, so the hard limit is inherited unchanged"
    );
    let expected = LOWERED_SOFT_LIMIT.max(hard.min(OPEN_MAX));
    assert_eq!(
        soft, expected,
        "the raised soft limit should be max(inherited = {LOWERED_SOFT_LIMIT}, min(hard, OPEN_MAX))"
    );
    assert!(
        soft > LOWERED_SOFT_LIMIT,
        "a hard limit at or below the inherited soft limit would make this assertion vacuous \
         on this host (hard = {hard})"
    );
}
