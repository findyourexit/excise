//! Linux cgroup v2 memory cap for a spawned `excise`, via `systemd-run --scope`.
//!
//! Off by default (`EXCISE_HARNESS_CGROUP` unset): nothing here changes how a process is spawned
//! or measured. [`CgroupOptIn::from_env`] is the only way to ask for it, mirroring
//! [`crate::fixture::PrivilegedOptIn`]. [`detect`] then checks whether this host can actually do
//! it (Linux, `systemd-run` on `PATH`, cgroup v2 mounted); a caller that cannot wrap must skip
//! cleanly, with the reason `detect` gives, rather than silently running unwrapped.
//!
//! # How the wrap works
//!
//! `systemd-run --scope -p MemoryMax=<limit> -p MemorySwapMax=0 --collect --unit=<name>
//! [--user] -- <program> <args...>` runs `program` as a transient scope unit: cgroup v2's kernel
//! OOM killer enforces the cap, not this crate. `systemd-run(1)`'s own wording ("a scope command is
//! executed by systemd-run itself as parent process") reads like a fork, but is not one on the
//! systemd this was verified against: backgrounding `systemd-run --scope` directly and comparing
//! its own pid with the wrapped program's self-reported pid showed they are the same. The wrapped
//! program *becomes* the `systemd-run` process (`execve`, not a fork), keeping its pid, its
//! process group (so the existing process-group kill is unaffected), and its cgroup throughout.
//! [`wrap`] rewrites the spawn in place: the program this crate ends up running is `systemd-run`,
//! and the pid this crate's own `Command` reports is already the wrapped program's own pid, with
//! nothing left to discover.
//!
//! Reading the cgroup's `memory.peak` works from that same pid, at the same "zombie, not yet
//! reaped" moment the existing peak-memory sampling already uses (see `metrics::sample`): the
//! scope's cgroup is not garbage-collected while the process, zombie included, remains a member.
//! The scope ends, and with it the cgroup directory, once the process this crate spawned has been
//! reaped: no residue.
//!
//! No command here is run with elevated privileges by this crate; whether an unprivileged
//! `--user` scope or a privileged system scope is available is a property of the host (see
//! [`user_manager_reachable`]), not a choice this crate makes. `--user` needs `XDG_RUNTIME_DIR` in
//! `systemd-run`'s own environment before it can even reach the bus to register the scope; an
//! isolated spawn environment does not carry it by default, so [`wrap`]'s third return value
//! names it for the caller to add.

use std::{
    fmt, fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

/// Set to exactly `1` to ask both runners to spawn `excise` under the Linux cgroup memory cap.
/// Unset, or any other value, leaves every run exactly as it is without this module: no process is
/// wrapped, and neither runner records the `cgroup_memory_peak_bytes` metric.
pub const OPT_IN_ENV: &str = "EXCISE_HARNESS_CGROUP";

/// Proof that the operator explicitly asked for the Linux cgroup memory cap.
///
/// The only way to build one is [`CgroupOptIn::from_env`]: reading [`OPT_IN_ENV`]. Its single
/// field is private, so it cannot be constructed with a struct literal outside this module:
/// holding one is itself the proof.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CgroupOptIn {
    accepted: (),
}

impl CgroupOptIn {
    /// Reads [`OPT_IN_ENV`] from the process environment. `Some` only when it is set to exactly
    /// `1`.
    #[must_use]
    pub fn from_env() -> Option<Self> {
        opt_in_from_value(std::env::var(OPT_IN_ENV).ok().as_deref())
    }
}

/// The pure parser behind [`CgroupOptIn::from_env`]. Separated out because tests cannot call
/// `std::env::set_var` (unsafe in edition 2024) and so cannot exercise `from_env` directly.
fn opt_in_from_value(value: Option<&str>) -> Option<CgroupOptIn> {
    (value == Some("1")).then_some(CgroupOptIn { accepted: () })
}

/// Why this host cannot run a process under the cgroup cap, even with the opt-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unavailable {
    /// Cgroup v2 memory accounting through `systemd-run --scope` is Linux-only.
    NotLinux,
    /// `systemd-run` is not on `PATH`.
    SystemdRunMissing,
    /// `/sys/fs/cgroup/cgroup.controllers` does not exist: no unified cgroup v2 hierarchy.
    CgroupV2Missing,
}

impl fmt::Display for Unavailable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::NotLinux => "the Linux cgroup memory cap only applies on Linux",
            Self::SystemdRunMissing => "`systemd-run` is not on `PATH`",
            Self::CgroupV2Missing => {
                "`/sys/fs/cgroup/cgroup.controllers` does not exist: no unified cgroup v2 hierarchy"
            }
        })
    }
}

/// Whether this host can run a process under the cgroup cap at all: Linux, `systemd-run` on
/// `PATH`, and cgroup v2 mounted. Does not confirm that a scope can actually be registered (that
/// needs a reachable bus and permissions this function does not probe): that can only be known by
/// trying, which [`wrap`] leaves to `systemd-run` itself.
///
/// # Errors
///
/// Returns the first reason this host cannot, so a caller can skip cleanly and say why.
pub fn detect() -> Result<(), Unavailable> {
    if std::env::consts::OS != "linux" {
        return Err(Unavailable::NotLinux);
    }
    if find_on_path("systemd-run").is_none() {
        return Err(Unavailable::SystemdRunMissing);
    }
    if !Path::new("/sys/fs/cgroup/cgroup.controllers").is_file() {
        return Err(Unavailable::CgroupV2Missing);
    }
    Ok(())
}

/// Whether a reachable user service manager and bus exist for `systemd-run --user`:
/// `$XDG_RUNTIME_DIR` is set and `<dir>/bus` exists. Without one, `--user` would just fail to
/// connect, so [`wrap`] omits it and relies on the system manager instead (root, or whatever the
/// CI tier allows).
#[must_use]
pub fn user_manager_reachable() -> bool {
    std::env::var_os("XDG_RUNTIME_DIR").is_some_and(|dir| Path::new(&dir).join("bus").exists())
}

/// A valid-enough systemd unit name derived from `seed`: ASCII letters, digits, `.`, `_`, and `-`
/// only (other characters become `-`), non-empty, and unique within this process (an atomic
/// counter and this process's id are appended), so that concurrent or repeated runs never collide
/// on the system or user manager.
#[must_use]
pub fn unit_name(seed: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let mut cleaned: String = seed
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
                character
            } else {
                '-'
            }
        })
        .collect();
    cleaned.truncate(120);
    if !cleaned.chars().any(char::is_alphanumeric) {
        cleaned.clear();
        cleaned.push_str("excise-harness");
    }
    let ordinal = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{cleaned}-{}-{ordinal}", std::process::id())
}

/// The `systemd-run` argument list that runs `program` (with `args`) as a transient, aggressively
/// collected (`--collect`) scope unit named `unit`, capped at `limit_bytes` of memory with swap
/// disabled. `--user` is added only when `user_manager` is true.
#[must_use]
pub fn scope_args(
    unit: &str,
    user_manager: bool,
    limit_bytes: u64,
    program: &Path,
    args: &[std::ffi::OsString],
) -> Vec<std::ffi::OsString> {
    let mut out: Vec<std::ffi::OsString> = Vec::with_capacity(10 + args.len());
    out.push("--scope".into());
    out.push("--collect".into());
    out.push(format!("--unit={unit}").into());
    if user_manager {
        out.push("--user".into());
    }
    out.push("-p".into());
    out.push(format!("MemoryMax={limit_bytes}").into());
    out.push("-p".into());
    out.push("MemorySwapMax=0".into());
    out.push("--".into());
    out.push(program.as_os_str().to_owned());
    out.extend(args.iter().cloned());
    out
}

/// Rewrites a spawn to run `program` (with `args`) under the cgroup cap, if this host supports it:
/// the program becomes `systemd-run`, with [`scope_args`] in front of the original command. The
/// third element is environment variables the caller must set in addition to its own (isolated)
/// environment, for `systemd-run` itself to work: with `--user`, that is `XDG_RUNTIME_DIR` (the
/// same variable [`user_manager_reachable`] read to choose `--user`), which `systemd-run` needs
/// before it can even reach the bus to register the scope, and which an isolated environment does
/// not otherwise carry. Empty without `--user`.
///
/// # Errors
///
/// Returns the reason this host cannot, exactly as [`detect`] would. This function only ever
/// builds the wrapped command; a caller is responsible for the "skip cleanly" half of the
/// contract (deciding, before anything is spawned, that a scenario or a fixture needed this and
/// could not get it).
#[allow(
    clippy::type_complexity,
    reason = "the plain tuple is the whole contract; a named struct would not read any clearer for three fields that are always used together and nowhere else"
)]
pub fn wrap(
    unit: &str,
    limit_bytes: u64,
    program: &Path,
    args: &[std::ffi::OsString],
) -> Result<
    (
        PathBuf,
        Vec<std::ffi::OsString>,
        Vec<(std::ffi::OsString, std::ffi::OsString)>,
    ),
    Unavailable,
> {
    detect()?;
    let systemd_run = find_on_path("systemd-run").ok_or(Unavailable::SystemdRunMissing)?;
    let user_manager = user_manager_reachable();
    let wrapped_args = scope_args(unit, user_manager, limit_bytes, program, args);
    let extra_env = if user_manager {
        std::env::var_os("XDG_RUNTIME_DIR")
            .into_iter()
            .map(|dir| ("XDG_RUNTIME_DIR".into(), dir))
            .collect()
    } else {
        Vec::new()
    };
    Ok((systemd_run, wrapped_args, extra_env))
}

/// The first executable file called `name` in a directory of `PATH`.
///
/// The same small search as `headless::du::find_on_path`, kept separate so this module does not
/// depend on `headless` (see the crate's module documentation on layering).
fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(name))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// The cgroup v2 `memory.peak` of the scope that `pid` (still alive, or a zombie not yet reaped)
/// belongs to: the all-time peak memory of the whole scope. `pid` is the pid this crate itself
/// spawned, which `systemd-run --scope` execs into (see the module documentation), so it is also
/// the wrapped program's own pid throughout. `None` where any step cannot be read: `pid`'s own
/// cgroup, the cgroup v2 mount point, or the counter file.
#[must_use]
pub fn read_memory_peak(pid: u32) -> Option<u64> {
    let cgroup_text = fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    let relative = parse_cgroup_path(&cgroup_text)?;
    let mounts = fs::read_to_string("/proc/mounts").ok()?;
    let mount = parse_cgroup2_mount(&mounts)?;
    let relative = relative.strip_prefix('/').unwrap_or(&relative);
    let counter = fs::read_to_string(mount.join(relative).join("memory.peak")).ok()?;
    parse_memory_peak(&counter)
}

/// The cgroup v2 (hierarchy id `0`) path from the text of `/proc/<pid>/cgroup`: the part after
/// `0::`, trimmed. A process can be listed on more than one hierarchy only under cgroup v1, which
/// this crate does not support; `0::` is always the unified (v2) one.
fn parse_cgroup_path(text: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.strip_prefix("0::"))
        .map(|path| path.trim().to_owned())
}

/// The cgroup v2 mount point from the text of `/proc/mounts`: the second field of the line whose
/// third field is `cgroup2`. Cgroup v2's unified hierarchy mounts in exactly one place.
fn parse_cgroup2_mount(text: &str) -> Option<PathBuf> {
    text.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let _device = fields.next()?;
        let mount_point = fields.next()?;
        let filesystem = fields.next()?;
        (filesystem == "cgroup2").then(|| PathBuf::from(mount_point))
    })
}

/// Parses the content of a cgroup v2 `memory.peak` file: a plain decimal byte count. (The kernel
/// also defines `max` for a few other `memory.*` counters when a value is unbounded; `memory.peak`
/// itself is only ever a number once the cgroup exists, so anything else is treated as unknown.)
fn parse_memory_peak(text: &str) -> Option<u64> {
    text.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::{
        opt_in_from_value, parse_cgroup_path, parse_cgroup2_mount, parse_memory_peak, scope_args,
        unit_name,
    };

    #[test]
    fn the_opt_in_needs_exactly_the_string_1() {
        assert!(opt_in_from_value(Some("1")).is_some());
        assert!(opt_in_from_value(Some("true")).is_none());
        assert!(opt_in_from_value(Some("01")).is_none());
        assert!(opt_in_from_value(Some(" 1")).is_none());
        assert!(opt_in_from_value(Some("")).is_none());
        assert!(opt_in_from_value(None).is_none());
    }

    #[test]
    fn unit_names_are_sanitized_and_unique() {
        let first = unit_name("memory-budget-interactive-1m (default)");
        let second = unit_name("memory-budget-interactive-1m (default)");
        assert_ne!(first, second, "repeated calls must not collide");
        assert!(
            first
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')),
            "{first}"
        );
        assert!(!first.contains(' '), "{first}");
        assert!(!first.contains('('), "{first}");

        let empty = unit_name("///");
        assert!(empty.starts_with("excise-harness"), "{empty}");
    }

    #[test]
    fn scope_args_build_the_documented_invocation() {
        let program = std::path::Path::new("/usr/local/bin/excise");
        let args: Vec<std::ffi::OsString> =
            vec!["--format".into(), "json".into(), "/fixtures/root".into()];

        let with_user = scope_args("excise-harness-7-0", true, 536_870_912, program, &args);
        assert_eq!(
            with_user,
            [
                "--scope",
                "--collect",
                "--unit=excise-harness-7-0",
                "--user",
                "-p",
                "MemoryMax=536870912",
                "-p",
                "MemorySwapMax=0",
                "--",
                "/usr/local/bin/excise",
                "--format",
                "json",
                "/fixtures/root",
            ]
        );

        let without_user = scope_args("excise-harness-7-1", false, 536_870_912, program, &args);
        assert!(!without_user.contains(&std::ffi::OsString::from("--user")));
        assert_eq!(without_user.len(), with_user.len() - 1);
    }

    #[test]
    fn the_unified_cgroup_line_is_the_one_with_hierarchy_id_zero() {
        let unified =
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/excise-harness-7-0.scope\n";
        assert_eq!(
            parse_cgroup_path(unified).as_deref(),
            Some(
                "/user.slice/user-1000.slice/user@1000.service/app.slice/excise-harness-7-0.scope"
            )
        );

        // A cgroup v1 host lists several numbered hierarchies; only `0::` is the v2 one.
        let mixed = "12:pids:/user.slice\n11:memory:/user.slice/foo\n0::/user.slice/bar.scope\n";
        assert_eq!(
            parse_cgroup_path(mixed).as_deref(),
            Some("/user.slice/bar.scope")
        );

        assert_eq!(parse_cgroup_path("1:name=systemd:/\n"), None);
    }

    #[test]
    fn the_cgroup2_mount_is_found_among_other_mounts() {
        let mounts = "proc /proc proc rw 0 0\n\
                      tmpfs /run tmpfs rw 0 0\n\
                      cgroup2 /sys/fs/cgroup cgroup2 rw,nosuid,nodev,noexec 0 0\n\
                      ext4 / ext4 rw,relatime 0 0\n";
        assert_eq!(
            parse_cgroup2_mount(mounts),
            Some(std::path::PathBuf::from("/sys/fs/cgroup"))
        );
        assert_eq!(parse_cgroup2_mount("proc /proc proc rw 0 0\n"), None);
    }

    #[test]
    fn memory_peak_is_a_plain_byte_count() {
        assert_eq!(parse_memory_peak("536870912\n"), Some(536_870_912));
        assert_eq!(parse_memory_peak("0\n"), Some(0));
        assert_eq!(parse_memory_peak("max\n"), None);
        assert_eq!(parse_memory_peak(""), None);
        assert_eq!(parse_memory_peak("not a number\n"), None);
    }
}
