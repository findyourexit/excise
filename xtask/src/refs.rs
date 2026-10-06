//! Builds a release binary of `excise` for any git ref, in a temporary detached worktree.
//!
//! `bench-e2e` builds its baseline here with the toolchain of the environment it runs under
//! ([`ToolchainPolicy::Inherited`]), so that the baseline and the candidate are compiled by the
//! same compiler. `sweep` builds every published release with the toolchain that release pins
//! ([`ToolchainPolicy::RefPinned`]): it judges a release by the compiler the release was built and
//! tested with, not by whichever one the sweep happens to run under.
//!
//! A ref is built in a detached worktree directly below [`RefLayout::worktrees`], with its own
//! `CARGO_TARGET_DIR`, `<builds>/<sha>/`. The worktree is removed whatever happens, even when the
//! build panics, and nothing but that worktree is ever removed: when git will not remove it, the
//! directory and the worktree's own entry in git's administrative area go, never anything else,
//! and `git worktree prune` is never run, because it forgets every worktree of the repository
//! whose directory is missing. The target directory stays and caches the binary by commit SHA, so
//! asking again for the same commit builds nothing.
//!
//! No wait here is unbounded. Every `git`, `rustup`, and `cargo` command runs under a deadline
//! (`BUILD_DEADLINE` for the build, `GIT_DEADLINE` and `RUSTUP_DEADLINE` for the others), in a
//! process group of its own on Unix, and one that is still running when its deadline passes is
//! killed, with everything it started where there is a group to signal (Windows has none: there
//! the command alone). A build that ran out of time fails as one that failed does: its worktree
//! is removed, and the error says that it timed out. A build that exits by itself, succeeding or
//! failing, has what it left running in its group killed too, so that nothing it started outlives
//! it; the `git` and `rustup` commands get no such kill (see the `bounded` module for why).

use std::{
    env,
    error::Error,
    ffi::OsString,
    fmt,
    fs::{self, File},
    io,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

/// The file, next to a build's `release` directory, that records the toolchain that built it:
/// the channel on the first line, `rustc --version` on the second.
const RECORD_FILE: &str = "toolchain.txt";
/// The channel the record and [`ToolchainUsed`] give for a ref that pins no toolchain.
const DEFAULT_CHANNEL: &str = "default";
/// The name rustup reads first, and the only one in which a bare channel is allowed.
const LEGACY_TOOLCHAIN_FILE: &str = "rust-toolchain";
const TOOLCHAIN_FILE: &str = "rust-toolchain.toml";
/// The variables that name a toolchain, or a tool of one, and so would override a pin.
const TOOLCHAIN_OVERRIDES: [&str; 4] = ["RUSTUP_TOOLCHAIN", "CARGO", "RUSTC", "RUSTDOC"];
/// How many lines of a failed build's log its error carries.
const LOG_TAIL_LINES: usize = 30;
/// How long one `cargo build` of a ref may run before it is killed with everything it started: 60
/// minutes. A cold release build of `excise` takes a few minutes, even under the oldest toolchain
/// a release pins, so this is far above anything a healthy build needs. It is there to end a build
/// that is stuck (a lock that is never released, a download that never ends, a linker that never
/// returns), so that a sweep goes on to its next ref instead of waiting for ever.
const BUILD_DEADLINE: Duration = Duration::from_secs(60 * 60);
/// How long one `git` command may run before it is killed with everything it started: 5 minutes.
/// The longest, checking a commit out into a worktree, takes seconds; the bound ends a command
/// that waits for something that never comes (a lock held by a process that is gone, a prompt that
/// nobody sees).
const GIT_DEADLINE: Duration = Duration::from_secs(5 * 60);
/// How long one `rustup` command, or the compiler it starts to say which version it is, may run
/// before it is killed with everything it started: 5 minutes. They answer in a fraction of a
/// second; the bound ends one that is stuck behind a lock, or on a download that rustup was told
/// not to make.
const RUSTUP_DEADLINE: Duration = Duration::from_secs(5 * 60);

/// Where the builds of refs and their temporary worktrees live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RefLayout {
    /// `<builds>/<sha>/` is the `CARGO_TARGET_DIR` of one ref, so its binary is
    /// `<builds>/<sha>/release/excise`.
    pub builds: PathBuf,
    /// The temporary detached worktrees are made directly below this directory.
    pub worktrees: PathBuf,
}

impl RefLayout {
    /// The layout whose two directories, called `builds` and `worktrees`, lie directly below
    /// `target`.
    pub(crate) fn below(target: &Path, builds: &str, worktrees: &str) -> Self {
        Self {
            builds: target.join(builds),
            worktrees: target.join(worktrees),
        }
    }
}

/// Which toolchain compiles a ref.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolchainPolicy {
    /// The toolchain of the environment the caller runs under: `$CARGO` (or `cargo`), with the
    /// environment left as it is, so that `RUSTUP_TOOLCHAIN` still decides and the ref is compiled
    /// by the same compiler as the caller. A toolchain file in the ref has no say.
    Inherited,
    /// The toolchain the ref pins in its own `rust-toolchain.toml` (or the legacy
    /// `rust-toolchain`): `rustup run <channel> cargo build ...`, with `RUSTUP_TOOLCHAIN`,
    /// `CARGO`, `RUSTC`, and `RUSTDOC` removed from the environment, so that nothing the caller
    /// runs under can override the pin, and with `RUSTUP_AUTO_INSTALL=0`, because a build never
    /// installs a toolchain: a pin that is not installed is an error that names the command that
    /// installs it.
    ///
    /// A ref that pins nothing is built by plain `cargo`, found on `PATH`, in the same
    /// environment, and its channel is the word `default`. Rustup then picks the toolchain for
    /// the worktree as it would for any other directory. When the worktrees lie below the
    /// checkout that runs the build, that is the toolchain that checkout pins, because rustup
    /// finds its `rust-toolchain.toml` on the way up: the toolchain the build itself runs under,
    /// unless it was started with an explicit `+toolchain`.
    RefPinned,
}

/// What `plan_ref` and `build_ref` need to know.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BuildSpec<'a> {
    /// The repository whose refs are built: the root of a checkout.
    pub root: &'a Path,
    /// Where the builds and the worktrees go.
    pub layout: &'a RefLayout,
    /// Which toolchain compiles the ref.
    pub policy: ToolchainPolicy,
    /// Where the output of `cargo build` goes: a file, created along with its parent directories
    /// and truncated for every build, or, with `None`, wherever the caller's own output goes. A
    /// build runs in a process group of its own, which is not the terminal's foreground group,
    /// so that where the output is a terminal that sets `tostop` the build is stopped at its
    /// first write: outside the tests, every caller that builds names a file.
    pub log: Option<&'a Path>,
}

/// The toolchain that compiled a build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolchainUsed {
    /// The channel the ref pins, or `default` for a ref that pins none.
    pub channel: String,
    /// The first line `rustc --version` printed under that toolchain, as the toolchain printed
    /// it: `rustc 1.88.0 (6b00bc388 2025-06-23)`.
    pub rustc: String,
}

/// What `plan_ref` found out about a ref, before anything is built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlannedRef {
    /// The reference as it was given: a branch, a tag, or a commit.
    pub reference: String,
    /// The 40-character SHA of its commit.
    pub sha: String,
    /// The channel the ref pins under `ToolchainPolicy::RefPinned`, `None` when it pins nothing.
    /// Always `None` under `ToolchainPolicy::Inherited`, which does not look.
    pub channel: Option<String>,
}

/// A binary that was built, or found already built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BuiltRef {
    /// The 40-character SHA of the commit the binary was built from.
    pub sha: String,
    /// `<builds>/<sha>/release/excise`, with the platform's executable suffix.
    pub binary: PathBuf,
    /// The toolchain that compiled it under `ToolchainPolicy::RefPinned`, read from the record
    /// when the binary was cached. `None` under `ToolchainPolicy::Inherited`, which does not ask.
    pub toolchain: Option<ToolchainUsed>,
    /// Whether the binary was there already, so that nothing was built.
    pub cached: bool,
}

/// The end of a failed build's log.
#[derive(Debug)]
pub(crate) struct LogTail {
    path: PathBuf,
    tail: String,
}

/// Why a ref could not be planned or built. The message says what to do about it and carries the
/// cause, so the error has no separate `source`.
#[derive(Debug)]
pub(crate) enum BuildError {
    /// A `git` command failed, or could not be run; the message says which and why.
    Git(String),
    /// `reference` is not a branch, a tag, or a commit of the repository.
    NoSuchCommit { reference: String, detail: String },
    /// The repository has no `v1.N.N` tag.
    NoReleaseTags { root: PathBuf },
    /// The toolchain file `file` of the commit `sha` cannot be used.
    ToolchainFile {
        sha: String,
        file: &'static str,
        problem: String,
    },
    /// `rustup` could not be run at all.
    RustupUnavailable(io::Error),
    /// `rustup` has no toolchain called `channel`.
    ToolchainMissing { channel: String, detail: String },
    /// The compiler of `toolchain` would not say which version it is.
    Probe { toolchain: String, detail: String },
    /// A file system operation failed.
    Io { action: String, source: io::Error },
    /// `cargo build` failed, could not be started, or did not finish within its deadline and was
    /// killed: `what` is what was built, `outcome` how it ended, and `log` the end of its output
    /// when that went to a file.
    Build {
        what: String,
        outcome: String,
        log: Option<Box<LogTail>>,
    },
    /// The build succeeded, but there is no binary where it belongs.
    NoBinary { binary: PathBuf },
    /// The temporary worktree `dir` could not be removed, or git still has an entry for it:
    /// `detail` says what is left and what went wrong.
    WorktreeLeft { dir: PathBuf, detail: String },
    /// `error`, and besides it `cleanup`: the worktree of a failed build could not be removed
    /// either.
    Cleanup {
        error: Box<BuildError>,
        cleanup: Box<BuildError>,
    },
}

impl BuildError {
    fn io(action: String, source: io::Error) -> Self {
        Self::Io { action, source }
    }

    /// `self`, together with the failure to clean up after it.
    fn with_cleanup(self, cleanup: Self) -> Self {
        Self::Cleanup {
            error: Box::new(self),
            cleanup: Box::new(cleanup),
        }
    }
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Git(message) => f.write_str(message),
            Self::NoSuchCommit { reference, detail } => {
                write!(f, "cannot resolve `{reference}` to a commit: {detail}")
            }
            Self::NoReleaseTags { root } => write!(
                f,
                "`{}` has no `v1.N.N` release tag; fetch them with `git fetch --tags`",
                root.display()
            ),
            Self::ToolchainFile { sha, file, problem } => {
                write!(f, "`{file}` at commit {sha} cannot be used: {problem}")
            }
            Self::RustupUnavailable(error) => write!(
                f,
                "cannot run `rustup`: {error}; building each ref with its own toolchain needs \
                 rustup"
            ),
            Self::ToolchainMissing { channel, detail } => write!(
                f,
                "the pinned toolchain `{channel}` is not available ({detail}); install it with \
                 `rustup toolchain install {channel}`"
            ),
            Self::Probe { toolchain, detail } => write!(
                f,
                "cannot tell which compiler the `{toolchain}` toolchain runs: {detail}"
            ),
            Self::Io { action, source } => write!(f, "cannot {action}: {source}"),
            Self::Build { what, outcome, log } => {
                write!(f, "building {what} failed ({outcome})")?;
                let Some(log) = log else {
                    return Ok(());
                };
                if log.tail.is_empty() {
                    return write!(f, "; its output file `{}` is empty", log.path.display());
                }
                write!(f, "; the end of its output, from `{}`:", log.path.display())?;
                for line in log.tail.lines() {
                    write!(f, "\n  {line}")?;
                }
                Ok(())
            }
            Self::NoBinary { binary } => write!(
                f,
                "the build succeeded but produced no `{}`: the package `excise` has no binary of \
                 that name",
                binary.display()
            ),
            Self::WorktreeLeft { dir, detail } => write!(
                f,
                "the temporary worktree `{0}` could not be removed ({detail}); remove it with \
                 `git worktree remove --force {0}` (`git worktree prune` would forget its entry \
                 too, and also the entry of every other worktree whose directory is missing, so \
                 read `git worktree list` before you run it)",
                dir.display()
            ),
            Self::Cleanup { error, cleanup } => write!(f, "{error}\nin addition, {cleanup}"),
        }
    }
}

impl Error for BuildError {}

/// Resolves `reference` (a branch, a tag, or a commit) to the full SHA of its commit, peeling an
/// annotated tag to the commit it points at.
///
/// # Errors
///
/// Fails when `git` cannot be run, or when `reference` does not name a commit of the repository
/// at `root`.
pub(crate) fn resolve_commit(root: &Path, reference: &str) -> Result<String, BuildError> {
    // Git would take a reference that starts with `-` for an option.
    if reference.starts_with('-') {
        return Err(unresolved(
            reference,
            "a reference cannot start with `-`".to_owned(),
        ));
    }
    let mut command = git(root);
    command
        .args(["rev-parse", "--verify"])
        .arg(format!("{reference}^{{commit}}"));
    let output =
        run_captured(&mut command, GIT_DEADLINE).map_err(|detail| unresolved(reference, detail))?;
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if is_object_name(&sha) {
        Ok(sha)
    } else {
        Err(unresolved(
            reference,
            format!("`git rev-parse` printed `{sha}`, which is not the SHA of a commit"),
        ))
    }
}

/// Every published release tag, `v1.N.N` exactly (a pre-release such as `v1.4.0-rc.1`, a `v0.*`
/// tag, and anything else is ignored), oldest first by version.
///
/// # Errors
///
/// Fails when `git` cannot be run, or when the repository at `root` has no such tag, which usually
/// means that the tags were not fetched.
pub(crate) fn release_tags(root: &Path) -> Result<Vec<String>, BuildError> {
    let listing = git_stdout(root, &["tag", "--list", "v1.*", "--sort=v:refname"])?;
    let tags: Vec<String> = String::from_utf8_lossy(&listing)
        .lines()
        .map(str::trim)
        .filter(|tag| is_release_tag(tag))
        .map(str::to_owned)
        .collect();
    if tags.is_empty() {
        return Err(BuildError::NoReleaseTags {
            root: root.to_path_buf(),
        });
    }
    Ok(tags)
}

/// The toolchain channel the commit `sha` pins, read from the commit and not from the working
/// tree: the `channel` of `rust-toolchain.toml`'s `[toolchain]` table, or the channel a legacy
/// `rust-toolchain` names, whose content is either that channel alone on one line or the same
/// TOML. `None` when the commit has neither file.
///
/// When both files exist, `rust-toolchain` wins, as it does for rustup.
///
/// # Errors
///
/// Fails when `git` cannot be run, or when the toolchain file cannot be used: it names a `path`,
/// has no `channel`, or has one that is not a toolchain name.
pub(crate) fn pinned_channel(root: &Path, sha: &str) -> Result<Option<String>, BuildError> {
    let listing = git_stdout(
        root,
        &[
            "ls-tree",
            "--name-only",
            sha,
            "--",
            LEGACY_TOOLCHAIN_FILE,
            TOOLCHAIN_FILE,
        ],
    )?;
    let listing = String::from_utf8_lossy(&listing);
    let present: Vec<&str> = listing.lines().collect();
    let Some(file) = [LEGACY_TOOLCHAIN_FILE, TOOLCHAIN_FILE]
        .into_iter()
        .find(|file| present.contains(file))
    else {
        return Ok(None);
    };
    let blob = git_stdout(root, &["cat-file", "blob", &format!("{sha}:{file}")])?;
    let unusable = |problem: String| BuildError::ToolchainFile {
        sha: sha.to_owned(),
        file,
        problem,
    };
    let text = String::from_utf8(blob).map_err(|_| unusable("it is not UTF-8".to_owned()))?;
    parse_toolchain_file(file, &text)
        .map(Some)
        .map_err(unusable)
}

/// Checks that rustup has the toolchain `channel` installed, without installing anything.
///
/// # Errors
///
/// Fails when `rustup` cannot be run, or does not answer within `RUSTUP_DEADLINE`, or when it has
/// no such toolchain; the message then names the command that installs it,
/// `rustup toolchain install <channel>`.
pub(crate) fn require_toolchain(channel: &str) -> Result<(), BuildError> {
    let mut command = Command::new("rustup");
    command
        .args(["which", "--toolchain", channel, "rustc"])
        // Without this, a rustup that auto-installs would download a missing toolchain right here.
        .env("RUSTUP_AUTO_INSTALL", "0");
    let shown = command_line(&command);
    let output = bounded::capture(&mut command, RUSTUP_DEADLINE)
        .and_then(|ended| ended.ok_or_else(|| timed_out(&shown, RUSTUP_DEADLINE)))
        .map_err(BuildError::RustupUnavailable)?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = stderr_text(&output);
    Err(BuildError::ToolchainMissing {
        channel: channel.to_owned(),
        detail: stderr
            .lines()
            .next()
            .map_or_else(|| output.status.to_string(), str::to_owned),
    })
}

/// Resolves `reference` and, under `ToolchainPolicy::RefPinned`, reads the toolchain the commit
/// pins and checks that it is installed. Builds nothing and creates nothing, so a caller can plan
/// every ref before it builds the first.
///
/// # Errors
///
/// Fails as `resolve_commit`, `pinned_channel`, and `require_toolchain` do.
pub(crate) fn plan_ref(spec: &BuildSpec<'_>, reference: &str) -> Result<PlannedRef, BuildError> {
    let sha = resolve_commit(spec.root, reference)?;
    let channel = match spec.policy {
        ToolchainPolicy::Inherited => None,
        ToolchainPolicy::RefPinned => {
            let channel = pinned_channel(spec.root, &sha)?;
            if let Some(channel) = &channel {
                require_toolchain(channel)?;
            }
            channel
        }
    };
    Ok(PlannedRef {
        reference: reference.to_owned(),
        sha,
        channel,
    })
}

/// Builds `reference` and returns its binary, or the binary an earlier build of the same commit
/// left behind.
///
/// `plan_ref` runs first, so a ref whose pinned toolchain is missing fails before anything is
/// created. The binary is `<builds>/<sha>/release/excise`. Under `Inherited` it is a hit when it
/// exists. Under `RefPinned` it is a hit when `<builds>/<sha>/toolchain.txt`, written after a
/// successful build, also names the channel the ref pins: a binary without that record is built
/// again, so a stale binary of a failed run is never trusted. A hit builds nothing, creates no
/// worktree, and leaves the log alone.
///
/// Otherwise the commit is checked out in a detached worktree below `layout.worktrees` and built
/// there with `cargo build --release --locked -p excise` and `CARGO_TARGET_DIR` set to
/// `<builds>/<sha>` (`ToolchainPolicy` says which `cargo`, in which environment). Under `RefPinned`
/// the toolchain is asked for its `rustc --version` first, and the build writes it to
/// `toolchain.txt` once it succeeded. The worktree is removed on every path out, a panic included;
/// a build that succeeded but left its worktree behind is an error.
///
/// Nothing is waited for without a bound. On Unix `cargo` runs in a process group of its own, and
/// it is killed, with every process it started (on Windows, the build alone), if it has not
/// finished within `BUILD_DEADLINE` (60 minutes): the build then fails as a failed one does, its
/// worktree is removed, and the error says that it timed out. A build that exits by itself,
/// succeeding or failing, has what it left running in its group killed too, so that nothing it
/// started outlives it. The `git` and `rustup` commands run in the same way under `GIT_DEADLINE`
/// and `RUSTUP_DEADLINE` (5 minutes each), but what they leave in their groups is left alone (the
/// `bounded` module says why). The input of the build is the null device: a build in a group of
/// its own is not the terminal's foreground group, and a read from the terminal would stop it for
/// good.
///
/// # Errors
///
/// Fails as `plan_ref` does, and when a directory, the log, or the worktree cannot be made, when
/// the toolchain will not say which compiler it is, when `cargo` fails or does not finish in time
/// (the error then carries the exit status, or says that it timed out, and, with a log, the
/// last 30 lines of it), when it produces no binary, and when the worktree cannot be removed
/// afterwards.
pub(crate) fn build_ref(spec: &BuildSpec<'_>, reference: &str) -> Result<BuiltRef, BuildError> {
    build_ref_with(spec, reference, &Build::STANDARD)
}

/// `build_ref` for a ref that `plan_ref` has looked at already: it builds the commit `plan` names
/// and resolves nothing again, so a branch that has moved since the planning does not change what
/// is built. A caller that decided something from the plan (that a build will run, which
/// toolchain it needs) is told the truth about the build that follows.
///
/// # Errors
///
/// Fails as `build_ref` does, except for what `plan_ref` has checked already.
pub(crate) fn build_planned(
    spec: &BuildSpec<'_>,
    plan: &PlannedRef,
) -> Result<BuiltRef, BuildError> {
    build_planned_with(spec, plan, &Build::STANDARD)
}

/// How `cargo build` is run: under which deadline, and by which command. `Build::STANDARD` is what
/// every caller outside the tests uses. A test shortens the deadline, and puts a command that
/// never ends, or that ends at once, in place of `cargo`.
struct Build {
    /// How long the command may run before it is killed with everything it started.
    deadline: Duration,
    /// Makes the command that builds a worktree.
    command: BuildCommand,
}

/// What makes the command that builds a worktree: it takes what `build_command` takes, the spec,
/// the channel the ref pins, the worktree, and the target directory.
type BuildCommand = fn(&BuildSpec<'_>, Option<&str>, &Path, &Path) -> Command;

impl Build {
    /// `cargo build`, under `BUILD_DEADLINE`.
    const STANDARD: Self = Self {
        deadline: BUILD_DEADLINE,
        command: build_command,
    };
}

/// `build_ref` with `build` in place of the standard `cargo build` and its deadline.
fn build_ref_with(
    spec: &BuildSpec<'_>,
    reference: &str,
    build: &Build,
) -> Result<BuiltRef, BuildError> {
    let plan = plan_ref(spec, reference)?;
    build_planned_with(spec, &plan, build)
}

/// `build_planned` with `build` in place of the standard `cargo build` and its deadline.
fn build_planned_with(
    spec: &BuildSpec<'_>,
    plan: &PlannedRef,
    build: &Build,
) -> Result<BuiltRef, BuildError> {
    // A relative `CARGO_TARGET_DIR` is resolved by cargo against its working directory, which is
    // the worktree, and the worktree is removed afterwards: the directories are made absolute.
    let target_dir = absolute(&spec.layout.builds)?.join(&plan.sha);
    if let Some(cached) = cached_build(spec, plan, &target_dir) {
        return Ok(cached);
    }

    let log = spec.log.map(create_log).transpose()?;
    let worktrees = absolute(&spec.layout.worktrees)?;
    let guard = WorktreeGuard::add(spec.root, &worktrees, &plan.sha)?;
    let outcome = build_in(spec, plan, &guard.dir, &target_dir, log, build);
    let toolchain = match (outcome, guard.finish()) {
        (Ok(toolchain), Ok(())) => toolchain,
        (Err(error), Ok(())) | (Ok(_), Err(error)) => return Err(error),
        (Err(error), Err(left)) => return Err(error.with_cleanup(left)),
    };
    Ok(BuiltRef {
        sha: plan.sha.clone(),
        binary: binary_path(&target_dir),
        toolchain,
        cached: false,
    })
}

/// Whether `build_planned` would return an earlier build of `plan`'s commit and build nothing:
/// the binary of the commit is there and can be trusted (see `build_ref`). A caller that announces
/// a build asks this first and then builds the same plan, so that it announces one only when one
/// runs.
///
/// # Errors
///
/// Fails when the builds directory cannot be made absolute.
pub(crate) fn is_cached(spec: &BuildSpec<'_>, plan: &PlannedRef) -> Result<bool, BuildError> {
    let target_dir = absolute(&spec.layout.builds)?.join(&plan.sha);
    Ok(cached_build(spec, plan, &target_dir).is_some())
}

/// The binary of an earlier build of the planned commit, if there is one that can be trusted.
fn cached_build(spec: &BuildSpec<'_>, plan: &PlannedRef, target_dir: &Path) -> Option<BuiltRef> {
    let binary = binary_path(target_dir);
    if !binary.is_file() {
        return None;
    }
    let toolchain = match spec.policy {
        ToolchainPolicy::Inherited => None,
        ToolchainPolicy::RefPinned => {
            let used = read_record(&target_dir.join(RECORD_FILE))?;
            if used.channel != plan.channel.as_deref().unwrap_or(DEFAULT_CHANNEL) {
                return None;
            }
            Some(used)
        }
    };
    Some(BuiltRef {
        sha: plan.sha.clone(),
        binary,
        toolchain,
        cached: true,
    })
}

/// The build proper, run inside the worktree `worktree`: the toolchain probe for `RefPinned`,
/// `cargo build` (or what `build` puts in its place), the check that the binary exists, and the
/// record of the toolchain.
fn build_in(
    spec: &BuildSpec<'_>,
    plan: &PlannedRef,
    worktree: &Path,
    target_dir: &Path,
    log: Option<File>,
    build: &Build,
) -> Result<Option<ToolchainUsed>, BuildError> {
    let toolchain = match spec.policy {
        ToolchainPolicy::Inherited => None,
        ToolchainPolicy::RefPinned => Some(probe_toolchain(plan.channel.as_deref(), worktree)?),
    };
    run_build(spec, plan, worktree, target_dir, log, build)?;
    let binary = binary_path(target_dir);
    if !binary.is_file() {
        return Err(BuildError::NoBinary { binary });
    }
    if let Some(used) = &toolchain {
        write_record(target_dir, used)?;
    }
    Ok(toolchain)
}

/// The command that builds the worktree `worktree` into `target_dir`; `channel` is the channel
/// the ref pins, which only `RefPinned` looks at. Nothing is run and no output is redirected, so a
/// test can look at the command.
fn build_command(
    spec: &BuildSpec<'_>,
    channel: Option<&str>,
    worktree: &Path,
    target_dir: &Path,
) -> Command {
    let mut command = match spec.policy {
        ToolchainPolicy::Inherited => {
            Command::new(env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo")))
        }
        ToolchainPolicy::RefPinned => pinned_tool(channel, "cargo"),
    };
    command
        .args(["build", "--release", "--locked", "-p", "excise"])
        .current_dir(worktree)
        .env("CARGO_TARGET_DIR", target_dir);
    command
}

/// `tool` as the toolchain `channel` runs it: `rustup run <channel> <tool>`, or `tool` alone,
/// found on `PATH`, when the ref pins no channel. Either way nothing that names a toolchain
/// stays in the environment, and rustup is told never to install one.
fn pinned_tool(channel: Option<&str>, tool: &str) -> Command {
    let mut command = match channel {
        Some(channel) => {
            let mut rustup = Command::new("rustup");
            rustup.args(["run", channel, tool]);
            rustup
        }
        None => Command::new(tool),
    };
    for name in TOOLCHAIN_OVERRIDES {
        command.env_remove(name);
    }
    command.env("RUSTUP_AUTO_INSTALL", "0");
    command
}

/// Runs the build command in the worktree, with its output in `log` when there is one, and kills
/// it, with everything it started, when it has not finished within `build.deadline`.
fn run_build(
    spec: &BuildSpec<'_>,
    plan: &PlannedRef,
    worktree: &Path,
    target_dir: &Path,
    log: Option<File>,
    build: &Build,
) -> Result<(), BuildError> {
    let mut command = (build.command)(spec, plan.channel.as_deref(), worktree, target_dir);
    if let Some(stdout) = log {
        let stderr = stdout.try_clone().map_err(|source| {
            BuildError::io(
                "share the build log between stdout and stderr".to_owned(),
                source,
            )
        })?;
        command.stdout(stdout).stderr(stderr);
    }
    let outcome = match bounded::run(&mut command, build.deadline) {
        Ok(Some(status)) if status.success() => return Ok(()),
        Ok(Some(status)) => status.to_string(),
        Ok(None) => timed_out_after(build.deadline),
        Err(error) => format!(
            "could not run `{}`: {error}",
            command.get_program().to_string_lossy()
        ),
    };
    Err(build_failure(spec, plan, outcome))
}

/// The error for a build that failed, with the end of its log if its output went to one.
fn build_failure(spec: &BuildSpec<'_>, plan: &PlannedRef, outcome: String) -> BuildError {
    let log = spec.log.map(|path| {
        let tail = match fs::read(path) {
            Ok(bytes) => tail_lines(&String::from_utf8_lossy(&bytes), LOG_TAIL_LINES),
            Err(error) => format!("(cannot read it: {error})"),
        };
        Box::new(LogTail {
            path: path.to_path_buf(),
            tail,
        })
    });
    let toolchain = match (spec.policy, plan.channel.as_deref()) {
        (ToolchainPolicy::Inherited, _) => String::new(),
        (ToolchainPolicy::RefPinned, Some(channel)) => format!(" with toolchain `{channel}`"),
        (ToolchainPolicy::RefPinned, None) => " with the default toolchain".to_owned(),
    };
    BuildError::Build {
        what: format!("`{}` ({}){toolchain}", plan.reference, plan.sha),
        outcome,
        log,
    }
}

/// Asks the toolchain the ref pins which compiler it is, from inside the worktree.
fn probe_toolchain(channel: Option<&str>, worktree: &Path) -> Result<ToolchainUsed, BuildError> {
    let toolchain = channel.unwrap_or(DEFAULT_CHANNEL);
    let probe = |detail: String| BuildError::Probe {
        toolchain: toolchain.to_owned(),
        detail,
    };
    let mut command = pinned_tool(channel, "rustc");
    command.arg("--version").current_dir(worktree);
    let output = run_captured(&mut command, RUSTUP_DEADLINE).map_err(probe)?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let Some(rustc) = stdout.lines().next().filter(|line| !line.trim().is_empty()) else {
        return Err(probe("`rustc --version` printed nothing".to_owned()));
    };
    Ok(ToolchainUsed {
        channel: toolchain.to_owned(),
        rustc: rustc.to_owned(),
    })
}

/// The binary a build into `target_dir` produces.
fn binary_path(target_dir: &Path) -> PathBuf {
    target_dir
        .join("release")
        .join(format!("excise{}", env::consts::EXE_SUFFIX))
}

/// The record of the toolchain that built the binary in `target_dir`, if it can be read.
fn read_record(path: &Path) -> Option<ToolchainUsed> {
    let text = fs::read_to_string(path).ok()?;
    let mut lines = text.lines().filter(|line| !line.is_empty());
    let channel = lines.next()?;
    let rustc = lines.next()?;
    Some(ToolchainUsed {
        channel: channel.to_owned(),
        rustc: rustc.to_owned(),
    })
}

fn write_record(target_dir: &Path, used: &ToolchainUsed) -> Result<(), BuildError> {
    let path = target_dir.join(RECORD_FILE);
    fs::write(&path, format!("{}\n{}\n", used.channel, used.rustc))
        .map_err(|source| BuildError::io(format!("write `{}`", path.display()), source))
}

/// Creates the log file, and the directories above it.
fn create_log(path: &Path) -> Result<File, BuildError> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .map_err(|source| BuildError::io(format!("create `{}`", parent.display()), source))?;
    }
    File::create(path).map_err(|source| {
        BuildError::io(format!("create the build log `{}`", path.display()), source)
    })
}

fn absolute(path: &Path) -> Result<PathBuf, BuildError> {
    std::path::absolute(path)
        .map_err(|source| BuildError::io(format!("make `{}` absolute", path.display()), source))
}

/// The last `count` lines of `text`.
fn tail_lines(text: &str, count: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(count)..].join("\n")
}

/// A detached worktree of the repository at `root`, removed when the guard goes: by `finish`,
/// which reports a failure to remove it, or else by `drop`, which can only warn about one. Either
/// way the worktree goes on every path out of a build: success, failure, an early `?`, a panic.
///
/// Removing it is `git worktree remove --force`. When git will not (a worktree that is locked, a
/// directory it cannot empty), the guard deletes the directory itself and then the one
/// administrative directory git keeps for this worktree, `<git-common-dir>/worktrees/<name>`: the
/// one the `gitdir:` line of `<dir>/.git` named when git made the worktree, which the guard checks
/// again before it deletes anything. It never runs `git worktree prune`, which forgets every
/// worktree of the repository whose directory is missing (an unmounted disk's, a moved
/// directory's), together with its index, its detached `HEAD`, and its reflog.
struct WorktreeGuard {
    root: PathBuf,
    dir: PathBuf,
    /// `dir` with every link resolved, which is how git records and lists a worktree (on macOS
    /// `/var` is a link). Taken while the directory exists, and `dir` itself if that failed.
    real_dir: PathBuf,
    /// The administrative directory of this worktree, or why it could not be told.
    admin: Result<PathBuf, String>,
    /// Cleared by `finish`, so that `drop` has nothing left to do.
    armed: bool,
}

impl WorktreeGuard {
    /// Checks `sha` out, detached, into a new directory below `parent`.
    fn add(root: &Path, parent: &Path, sha: &str) -> Result<Self, BuildError> {
        Self::add_within(root, parent, sha, GIT_DEADLINE)
    }

    /// `add`, with `limit` as the bound of the `git worktree add` it runs, so that a test can
    /// shorten it.
    fn add_within(
        root: &Path,
        parent: &Path,
        sha: &str,
        limit: Duration,
    ) -> Result<Self, BuildError> {
        fs::create_dir_all(parent)
            .map_err(|source| BuildError::io(format!("create `{}`", parent.display()), source))?;
        let dir = unique_worktree_dir(parent, sha);
        let mut command = git(root);
        command
            .args(["worktree", "add", "--detach"])
            .arg(&dir)
            .arg(sha);
        let added = run_captured(&mut command, limit);
        let mut guard = Self {
            root: root.to_path_buf(),
            real_dir: fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone()),
            admin: admin_of(&dir),
            dir,
            armed: true,
        };
        let Err(message) = added else {
            return Ok(guard);
        };
        let error = BuildError::Git(message);
        if !present(&guard.dir) {
            // Git removes what it made when the checkout fails, and it made nothing here.
            guard.armed = false;
            return Err(error);
        }
        // A command that is killed at its deadline cannot clean up after itself: what it left is
        // removed here.
        match guard.finish() {
            Ok(()) => Err(error),
            Err(left) => Err(error.with_cleanup(left)),
        }
    }

    /// Removes the worktree now, and says so when it could not.
    fn finish(mut self) -> Result<(), BuildError> {
        self.armed = false;
        self.remove()
    }

    /// `git worktree remove --force` and, if that failed or left anything, `remove_by_hand`. It
    /// worked when the directory, git's entry for it, and its line in `git worktree list` are all
    /// gone.
    fn remove(&self) -> Result<(), BuildError> {
        let mut problems = Vec::new();
        let mut remove = git(&self.root);
        remove
            .args(["worktree", "remove", "--force"])
            .arg(&self.dir);
        let git_removed = noted(run_captured(&mut remove, GIT_DEADLINE), &mut problems);
        if (!git_removed || self.still_there())
            && let Err(problem) = self.remove_by_hand()
        {
            problems.push(problem);
        }
        let left = self.leftovers();
        if left.is_empty() {
            return Ok(());
        }
        problems.extend(left);
        Err(BuildError::WorktreeLeft {
            dir: self.dir.clone(),
            detail: problems.join("; "),
        })
    }

    /// Whether the directory, or git's entry for it, is still there.
    fn still_there(&self) -> bool {
        present(&self.dir) || self.admin.as_ref().is_ok_and(|admin| present(admin))
    }

    /// What of the worktree is still there, one sentence each: its directory, git's entry for it,
    /// and its line in `git worktree list`. Empty when it is all gone.
    fn leftovers(&self) -> Vec<String> {
        let mut left = Vec::new();
        if present(&self.dir) {
            left.push(format!(
                "its directory `{}` is still there",
                self.dir.display()
            ));
        }
        if let Ok(admin) = &self.admin
            && present(admin)
        {
            left.push(format!(
                "git's entry for it, `{}`, is still there",
                admin.display()
            ));
        }
        match self.listed() {
            Ok(false) => {}
            Ok(true) => left.push("`git worktree list` still lists it".to_owned()),
            Err(problem) => left.push(format!("cannot ask git what it lists: {problem}")),
        }
        left
    }

    /// Whether `git worktree list` lists this worktree.
    fn listed(&self) -> Result<bool, String> {
        let mut list = git(&self.root);
        list.args(["worktree", "list", "--porcelain"]);
        let output = run_captured(&mut list, GIT_DEADLINE)?;
        let listing = String::from_utf8_lossy(&output.stdout);
        let listed = listing
            .lines()
            .filter_map(|line| line.strip_prefix("worktree "))
            .map(Path::new)
            .any(|path| path == self.dir || path == self.real_dir);
        Ok(listed)
    }

    /// Deletes by hand what `git worktree remove --force` left of this worktree: its directory
    /// and its own administrative directory, and nothing else. Everything is checked before
    /// anything is deleted, so a refusal leaves both for a person to see.
    fn remove_by_hand(&self) -> Result<(), String> {
        let known = self.admin.as_ref();
        let admin = known.map_err(|reason| format!("nothing is deleted by hand: {reason}"))?;
        // What is deleted is the path that was checked, with every link resolved.
        let entry = if present(admin) {
            let checked = self.check_entry(admin).map_err(|why| {
                format!("refusing to delete `{}` by hand: {why}", admin.display())
            })?;
            Some(checked)
        } else {
            None
        };
        if present(&self.dir)
            && let Err(error) = fs::remove_dir_all(&self.dir)
        {
            return Err(format!("removing `{}` failed: {error}", self.dir.display()));
        }
        if let Some(entry) = entry
            && let Err(error) = fs::remove_dir_all(&entry)
        {
            return Err(format!("removing `{}` failed: {error}", entry.display()));
        }
        Ok(())
    }

    /// Checks that `admin` is this worktree's own administrative directory, so that deleting it
    /// cannot touch anything else: a real directory, not a link, directly inside
    /// `<git-common-dir>/worktrees/`, whose `gitdir` file names this worktree's `.git`. Gives
    /// the path of the directory with every link resolved.
    fn check_entry(&self, admin: &Path) -> Result<PathBuf, String> {
        let metadata = fs::symlink_metadata(admin)
            .map_err(|error| format!("it cannot be looked at: {error}"))?;
        if !metadata.is_dir() {
            return Err("it is not a real directory".to_owned());
        }
        let real =
            fs::canonicalize(admin).map_err(|error| format!("it cannot be resolved: {error}"))?;
        let worktrees = self.common_dir()?.join("worktrees");
        let expected = fs::canonicalize(&worktrees)
            .map_err(|error| format!("cannot resolve `{}`: {error}", worktrees.display()))?;
        if real.parent() != Some(expected.as_path()) {
            let place = expected.display();
            return Err(format!("it is not directly inside `{place}`"));
        }
        let file = real.join("gitdir");
        let text = fs::read_to_string(&file)
            .map_err(|error| format!("cannot read `{}`: {error}", file.display()))?;
        // A relative path in it is relative to the administrative directory.
        let named = real.join(text.trim_end_matches(['\n', '\r']));
        if named == self.real_dir.join(".git") || same_place(&named, &self.dir.join(".git")) {
            return Ok(real);
        }
        Err(format!(
            "its `gitdir` file names `{}`, not this worktree's `.git`",
            named.display()
        ))
    }

    /// The git directory that all the worktrees of the repository share, as an absolute path with
    /// every link resolved.
    fn common_dir(&self) -> Result<PathBuf, String> {
        let mut command = git(&self.root);
        command.args(["rev-parse", "--git-common-dir"]);
        let output = run_captured(&mut command, GIT_DEADLINE)?;
        let printed = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if printed.is_empty() {
            return Err("`git rev-parse --git-common-dir` printed nothing".to_owned());
        }
        // Git prints a path relative to the directory it ran in, which is `root`, unless the path
        // is absolute, which `join` keeps.
        let common = self.root.join(printed);
        fs::canonicalize(&common)
            .map_err(|error| format!("cannot resolve `{}`: {error}", common.display()))
    }
}

impl Drop for WorktreeGuard {
    fn drop(&mut self) {
        if self.armed
            && let Err(error) = self.remove()
        {
            eprintln!("warning: {error}");
        }
    }
}

/// The administrative directory that the `gitdir:` line of `<dir>/.git` names, or why that cannot
/// be told.
fn admin_of(dir: &Path) -> Result<PathBuf, String> {
    let file = dir.join(".git");
    let text = fs::read_to_string(&file)
        .map_err(|error| format!("cannot read `{}`: {error}", file.display()))?;
    let named = text
        .lines()
        .find_map(|line| line.strip_prefix("gitdir:"))
        .map(str::trim)
        .filter(|named| !named.is_empty())
        .ok_or_else(|| format!("`{}` has no `gitdir:` line", file.display()))?;
    // A relative path is relative to the directory of the worktree.
    Ok(dir.join(named))
}

/// Whether `a` and `b` are the same place: the same path, or, when both exist, the same file once
/// every link is resolved.
fn same_place(a: &Path, b: &Path) -> bool {
    a == b
        || matches!(
            (fs::canonicalize(a), fs::canonicalize(b)),
            (Ok(real_a), Ok(real_b)) if real_a == real_b
        )
}

/// Whether the command that gave `result` worked; if it did not, why is added to `problems`.
fn noted(result: Result<Output, String>, problems: &mut Vec<String>) -> bool {
    match result {
        Ok(_) => true,
        Err(problem) => {
            problems.push(problem);
            false
        }
    }
}

/// Whether anything, even a dangling link, is at `path`.
fn present(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

/// A unique path below `parent` for one worktree, with nothing at it yet, so that whatever is
/// found there after `git worktree add` is what git put there.
fn unique_worktree_dir(parent: &Path, sha: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    loop {
        let name = format!(
            "{sha}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let dir = parent.join(name);
        if !present(&dir) {
            return dir;
        }
    }
}

/// `git -C <root>`: every git command runs against the repository the caller named.
fn git(root: &Path) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(root);
    command
}

/// What `git -C <root> <args>` printed on stdout.
fn git_stdout(root: &Path, args: &[&str]) -> Result<Vec<u8>, BuildError> {
    let mut command = git(root);
    command.args(args);
    run_captured(&mut command, GIT_DEADLINE)
        .map(|output| output.stdout)
        .map_err(BuildError::Git)
}

/// Runs `command` with its output captured, in a process group of its own, and kills it, with
/// everything it started, when it has not finished within `limit`. A failure is described by the
/// command line and by what the command said or how it ended.
fn run_captured(command: &mut Command, limit: Duration) -> Result<Output, String> {
    let shown = command_line(command);
    match bounded::capture(command, limit) {
        Ok(Some(output)) if output.status.success() => Ok(output),
        Ok(Some(output)) => Err(format!("`{shown}` failed: {}", failure_detail(&output))),
        Ok(None) => Err(format!("`{shown}` {}", timed_out_after(limit))),
        Err(error) => Err(format!("cannot run `{shown}`: {error}")),
    }
}

/// What is said of a command that was killed at its deadline: `timed out after 5 minutes and was
/// killed`.
fn timed_out_after(limit: Duration) -> String {
    format!("timed out after {} and was killed", describe_limit(limit))
}

/// The error of the command `shown`, which was killed at its deadline.
fn timed_out(shown: &str, limit: Duration) -> io::Error {
    let message = format!("`{shown}` {}", timed_out_after(limit));
    io::Error::new(io::ErrorKind::TimedOut, message)
}

/// A deadline as a person says it: `60 minutes`, `90 seconds`. One that is not a whole number of
/// seconds is written the way `Duration` writes it, `250ms`.
fn describe_limit(limit: Duration) -> String {
    let seconds = limit.as_secs();
    if limit.subsec_nanos() > 0 || seconds == 0 {
        return format!("{limit:?}");
    }
    let minutes = seconds / 60;
    let (count, unit) = if minutes > 0 && minutes * 60 == seconds {
        (minutes, "minute")
    } else {
        (seconds, "second")
    };
    let plural = if count == 1 { "" } else { "s" };
    format!("{count} {unit}{plural}")
}

fn command_line(command: &Command) -> String {
    std::iter::once(command.get_program())
        .chain(command.get_args())
        .map(|part| part.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}

/// What a failed command said on stderr, or how it ended if it said nothing.
fn failure_detail(output: &Output) -> String {
    let stderr = stderr_text(output);
    if stderr.is_empty() {
        output.status.to_string()
    } else {
        format!("{stderr} ({})", output.status)
    }
}

fn stderr_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).trim().to_owned()
}

fn unresolved(reference: &str, detail: String) -> BuildError {
    BuildError::NoSuchCommit {
        reference: reference.to_owned(),
        detail,
    }
}

/// Whether `text` is the full name of a git object: 40 hexadecimal digits, or 64 in a repository
/// that uses SHA-256.
fn is_object_name(text: &str) -> bool {
    matches!(text.len(), 40 | 64) && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Whether `tag` is exactly `v1.N.N`: a release, not a pre-release and not another major version.
fn is_release_tag(tag: &str) -> bool {
    let Some(rest) = tag.strip_prefix("v1.") else {
        return false;
    };
    let mut numbers = rest.split('.');
    match (numbers.next(), numbers.next(), numbers.next()) {
        (Some(minor), Some(patch), None) => is_number(minor) && is_number(patch),
        _ => false,
    }
}

fn is_number(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())
}

/// Whether `channel` is a toolchain name that can be handed to rustup as one argument: a letter
/// or a digit first, so that it can never read as an option, then letters, digits, `.`, `-`, and
/// `_`. That covers `stable`, `1.88.0`, `nightly-2026-08-18`, and a name with a host triple.
fn is_channel_name(channel: &str) -> bool {
    let mut chars = channel.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

/// The channel the toolchain file `file` pins, or what is wrong with it. Rustup reads a bare
/// channel only from the legacy `rust-toolchain`, and only when the file has just one line;
/// everything else is TOML.
fn parse_toolchain_file(file: &str, text: &str) -> Result<String, String> {
    if text.trim().is_empty() {
        return Err("it is empty".to_owned());
    }
    let channel = if file == LEGACY_TOOLCHAIN_FILE && text.lines().count() == 1 {
        text.trim().to_owned()
    } else {
        toml_channel(text)?
    };
    if is_channel_name(&channel) {
        Ok(channel)
    } else {
        Err(format!(
            "`{channel}` is not a toolchain name: it is letters, digits, `.`, `-`, and `_`, and \
             starts with a letter or a digit"
        ))
    }
}

/// The `channel` of the `[toolchain]` table of a toolchain file in TOML.
///
/// This reads what such a file holds: tables, `key = "string"` lines, comments, and arrays that
/// run over several lines. It is not a TOML parser. What it does not understand is an error and
/// never a guess, because a wrong pin would build a release with the wrong compiler without a
/// word.
fn toml_channel(text: &str) -> Result<String, String> {
    let mut in_toolchain = false;
    let mut has_toolchain = false;
    let mut has_path = false;
    let mut channel: Option<String> = None;
    // How many brackets and braces a value that goes on over the next lines has left open.
    let mut open: isize = 0;
    for (index, raw) in text.lines().enumerate() {
        let number = index + 1;
        let scanned = scan_line(raw).map_err(|problem| format!("line {number}: {problem}"))?;
        let continued = open > 0;
        open += scanned.depth;
        if open < 0 {
            return Err(format!(
                "line {number}: a bracket is closed that was never opened"
            ));
        }
        let line = scanned.code.trim();
        if continued || line.is_empty() {
            continue;
        }
        if let Some(table) = table_name(line) {
            in_toolchain = table == "toolchain";
            has_toolchain |= in_toolchain;
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!(
                "line {number}: expected `key = value`, found `{line}`"
            ));
        };
        if !in_toolchain {
            continue;
        }
        match key.trim() {
            "channel" => {
                if channel.is_some() {
                    return Err(format!("line {number}: `channel` is set twice"));
                }
                let name = string_value(value.trim())
                    .map_err(|problem| format!("line {number}: {problem}"))?;
                channel = Some(name.to_owned());
            }
            "path" => has_path = true,
            _ => {}
        }
    }
    if open != 0 {
        return Err("a bracket is never closed".to_owned());
    }
    if !has_toolchain {
        return Err("it has no `[toolchain]` table".to_owned());
    }
    if has_path {
        return Err(
            "its `[toolchain]` table names a `path`, a toolchain that exists on one machine, \
             not a `channel` that can be installed"
                .to_owned(),
        );
    }
    channel.ok_or_else(|| "its `[toolchain]` table has no `channel`".to_owned())
}

/// One line of a toolchain file: what comes before a comment, and how many more brackets and
/// braces it opens than it closes outside of strings.
struct Scanned<'a> {
    code: &'a str,
    depth: isize,
}

fn scan_line(line: &str) -> Result<Scanned<'_>, String> {
    let mut depth: isize = 0;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (index, c) in line.char_indices() {
        if let Some(open) = quote {
            if escaped {
                escaped = false;
            } else if open == '"' && c == '\\' {
                escaped = true;
            } else if c == open {
                quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' => quote = Some(c),
            '#' => {
                return Ok(Scanned {
                    code: &line[..index],
                    depth,
                });
            }
            '[' | '{' => depth += 1,
            ']' | '}' => depth -= 1,
            _ => {}
        }
    }
    if quote.is_some() {
        return Err("a string is not closed on its line".to_owned());
    }
    Ok(Scanned { code: line, depth })
}

/// The name in a `[table]` header line. A `[[table]]` header gives `[table]`, which is no table
/// this code looks for.
fn table_name(line: &str) -> Option<&str> {
    line.strip_prefix('[')?.strip_suffix(']').map(str::trim)
}

/// The text of a basic or literal string that is the whole of `value`.
fn string_value(value: &str) -> Result<&str, String> {
    let inner = value
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .or_else(|| {
            value
                .strip_prefix('\'')
                .and_then(|rest| rest.strip_suffix('\''))
        })
        .ok_or_else(|| format!("`channel` is not a plain quoted string: `{value}`"))?;
    if inner.contains(['"', '\'', '\\']) {
        return Err(format!(
            "`channel` has a quote or an escape in it: `{value}`"
        ));
    }
    Ok(inner)
}

/// Running a command under a deadline, in a process group of its own, as the harness runners do
/// (`excise_harness::headless::process`), but without taking the command's output over: a build
/// writes to a log file or to its caller's terminal, and the output of a `git` command is read
/// whole, however much of it there is.
///
/// On Unix the group is made with `process_group(0)`, so that its id is the command's pid and a
/// signal for the group reaches everything the command started. A command that is still running
/// at its deadline is sent `SIGKILL` together with its group, before it is reaped, so that its id
/// is still the id of the group, and is then waited for. Windows has no process groups to signal;
/// there the command alone is killed, as the harness runners do.
///
/// The group is only ever signalled while its leader, the command, has not been reaped. A zombie
/// is still a process: its id is not given to another process, and neither is the id of a group
/// that has the same number, until it is waited for. Once it is reaped and the group is empty,
/// the id is free, and a signal for it would reach whichever process took it, so what a command
/// leaves in its group is dealt with before the reaping and never after it.
///
/// What a command that ends by itself leaves behind in its group depends on what it is:
///
/// * `run`, which runs a build, kills what is left of the group once the build has exited,
///   whether it succeeded or failed with a status: a `rustc` that cargo did not wait for, a build
///   script's helper. Nothing a build started outlives it in its group, so nothing is left to
///   hold the worktree it ran in when the worktree is removed (on Windows there is no group, and
///   nothing is killed). The exit is seen without reaping the build (on Unix `waitid` with
///   `WNOWAIT`, through `excise_harness::safety::has_exited_unreaped`), the group is signalled
///   while the build is still a zombie, and only then is the build reaped. The signal reaches the
///   members of the group that are still running, or none: `ESRCH`, or `EPERM` on macOS, which
///   will not signal a group that holds nothing but a zombie. The answer is ignored.
/// * `capture`, which runs the `git` and `rustup` commands, leaves whatever they started alone.
///   `git` can start a detached `git gc --auto`, which puts itself in a session of its own
///   (`setsid`) and so is outside the group anyway, and it runs the user's hooks, whose
///   background work is the user's own configuration of their repository: killing it would break
///   their maintenance. The short `rustup` probes start nothing that outlives them. A process
///   left behind that still holds the output of the command open is an error, not a wait.
///
/// The commands run with the null device as their input: a command in a group of its own is not
/// the terminal's foreground group, and a read from the terminal would stop it for good. For the
/// same reason an interrupt (Ctrl-C) reaches the process that waits and not the command, which
/// is then left to finish by itself, with no deadline, if the waiting process dies of it.
mod bounded {
    use std::{
        io::{self, Read},
        process::{Child, Command, ExitStatus, Output, Stdio},
        sync::mpsc::{self, Receiver, RecvTimeoutError},
        thread,
        time::{Duration, Instant},
    };

    /// How long a command that has exited may take to close its output: a process it started and
    /// left behind can hold the output open, and then it cannot be read to its end.
    const OUTPUT_GRACE: Duration = Duration::from_secs(2);
    /// How long a command that was killed is waited for, so that a process that cannot be killed
    /// (one that is stuck in the kernel) does not hold up the caller for ever.
    const KILL_GRACE: Duration = Duration::from_secs(10);
    /// The first pause between two looks at a running command. It doubles up to `POLL_LAST`, so
    /// that a command that ends at once is noticed at once, and a build that runs for an hour is
    /// not looked at all the time.
    const POLL_FIRST: Duration = Duration::from_millis(1);
    /// The longest pause between two looks at a running command.
    const POLL_LAST: Duration = Duration::from_millis(50);

    /// Runs `command` to its end, with its output where the caller put it, or kills it, and on
    /// Unix everything it started, once `limit` has passed. `Ok(None)` says that it was killed.
    /// When it exits by itself, succeeding or not, what it left running in its group is killed
    /// too, before it is reaped (see the documentation of the module).
    pub(super) fn run(command: &mut Command, limit: Duration) -> io::Result<Option<ExitStatus>> {
        run_with(command, limit, kill_group)
    }

    /// [`run`], with `signal` in place of the kill of the group of a command that exited by
    /// itself. It is called once, with the command's `Child`, once the exit has been seen and
    /// before the command is reaped: the order a test looks at.
    pub(super) fn run_with(
        command: &mut Command,
        limit: Duration,
        signal: impl FnOnce(&Child),
    ) -> io::Result<Option<ExitStatus>> {
        command.stdin(Stdio::null());
        own_group(command);
        let mut child = command.spawn()?;
        match poll_until(Instant::now() + limit, || has_exited(&mut child)) {
            Ok(true) => {
                // The command exited by itself, whatever its status, and nothing has waited for
                // it: it is a zombie, which keeps its id, and the id of its group, from being
                // given to another process. What it left running in its group goes now, and the
                // signal can reach nothing else. Reaping it comes after, and still has its
                // status to give.
                signal(&child);
                child.wait().map(Some)
            }
            Ok(false) => {
                kill_and_reap(&mut child);
                Ok(None)
            }
            Err(error) => {
                kill_and_reap(&mut child);
                Err(error)
            }
        }
    }

    /// Runs `command` with its output captured, whole, or kills it, and on Unix everything it
    /// started, once `limit` has passed: `Ok(None)` says that it was killed. What it leaves
    /// running in its group when it exits by itself is left alone (see the documentation of the
    /// module). It is an error when the command ended but something it started still holds its
    /// output open.
    pub(super) fn capture(command: &mut Command, limit: Duration) -> io::Result<Option<Output>> {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        own_group(command);
        let mut child = command.spawn()?;
        let stdout = read_in_background(child.stdout.take());
        let stderr = read_in_background(child.stderr.take());
        let Some(status) = wait_or_kill(&mut child, limit)? else {
            return Ok(None);
        };
        Ok(Some(Output {
            status,
            stdout: received(&stdout)?,
            stderr: received(&stderr)?,
        }))
    }

    /// Waits for `child` to exit. When it is still running once `limit` has passed, or cannot be
    /// waited for, it is killed.
    fn wait_or_kill(child: &mut Child, limit: Duration) -> io::Result<Option<ExitStatus>> {
        let waited = wait_until(child, Instant::now() + limit);
        if !matches!(waited, Ok(Some(_))) {
            kill_and_reap(child);
        }
        waited
    }

    /// Waits until `deadline` for `child` to exit, and reaps it: `Ok(None)` when it is still
    /// running then.
    fn wait_until(child: &mut Child, deadline: Instant) -> io::Result<Option<ExitStatus>> {
        let mut status = None;
        poll_until(deadline, || {
            status = child.try_wait()?;
            Ok(status.is_some())
        })?;
        Ok(status)
    }

    /// Calls `exited` until it says that the command has, pausing between the calls for a time
    /// that doubles from `POLL_FIRST` up to `POLL_LAST`: `Ok(false)` when `deadline` came first.
    fn poll_until(
        deadline: Instant,
        mut exited: impl FnMut() -> io::Result<bool>,
    ) -> io::Result<bool> {
        let mut pause = POLL_FIRST;
        loop {
            if exited()? {
                return Ok(true);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(false);
            }
            thread::sleep(pause.min(left));
            pause = (pause * 2).min(POLL_LAST);
        }
    }

    /// Whether `child` has exited. On Unix this looks and does not reap: the exit is seen while
    /// the child is a zombie, which `run_with` needs (see the documentation of the module).
    #[cfg(unix)]
    fn has_exited(child: &mut Child) -> io::Result<bool> {
        excise_harness::safety::has_exited_unreaped(child.id())
    }

    /// Whether `child` has exited. There are no process groups to signal here, so there is
    /// nothing to do before it is reaped.
    #[cfg(not(unix))]
    fn has_exited(child: &mut Child) -> io::Result<bool> {
        child.try_wait().map(|status| status.is_some())
    }

    /// Kills `child`, and on Unix its process group, and waits for it to be gone, for at most
    /// `KILL_GRACE`.
    fn kill_and_reap(child: &mut Child) {
        // The child has not been reaped, so its id is still the id of its process group and
        // cannot belong to another one.
        kill_group(child);
        let _ = child.kill();
        let _ = wait_until(child, Instant::now() + KILL_GRACE);
    }

    /// Sends `SIGKILL` to the process group that `own_group` made for `child`, whose id is the pid
    /// of `child`. It is only called while `child` has not been reaped, so that the id cannot have
    /// been given to another process (see the documentation of the module). Whether anything is
    /// left in the group does not matter, and an error is ignored.
    #[cfg(unix)]
    fn kill_group(child: &Child) {
        let _ = excise_harness::safety::kill_process_group(child.id());
    }

    /// Windows has no process groups to signal; see the documentation of the module.
    #[cfg(not(unix))]
    fn kill_group(_child: &Child) {}

    /// Puts `command` in a process group of its own, so that a signal for the group reaches every
    /// process the command starts.
    #[cfg(unix)]
    fn own_group(command: &mut Command) {
        use std::os::unix::process::CommandExt as _;

        command.process_group(0);
    }

    /// Windows has no process groups to signal; see the documentation of the module.
    #[cfg(not(unix))]
    fn own_group(_command: &mut Command) {}

    /// Reads `pipe` to its end on a thread of its own, so that a command that writes more than a
    /// pipe holds is never blocked, and sends what it read once the pipe is closed.
    fn read_in_background(pipe: Option<impl Read + Send + 'static>) -> Receiver<Vec<u8>> {
        let (sender, receiver) = mpsc::channel();
        let _ = thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut bytes);
            }
            let _ = sender.send(bytes);
        });
        receiver
    }

    /// What `reader` read, which it sends when the output of the command is closed. It waits for
    /// that for at most `OUTPUT_GRACE`.
    fn received(reader: &Receiver<Vec<u8>>) -> io::Result<Vec<u8>> {
        reader
            .recv_timeout(OUTPUT_GRACE)
            .map_err(|error| match error {
                RecvTimeoutError::Timeout => io::Error::new(
                    io::ErrorKind::TimedOut,
                    "it ended, but something it started still holds its output open",
                ),
                RecvTimeoutError::Disconnected => io::Error::other("its output could not be read"),
            })
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, ffi::OsStr, panic};

    use tempfile::TempDir;

    use super::*;

    const CARGO_TOML: &str =
        "[package]\nname = \"excise\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[workspace]\n";
    const CARGO_LOCK: &str = "# This file is automatically @generated by Cargo.\n\
                              # It is not intended for manual editing.\n\
                              version = 4\n\n\
                              [[package]]\n\
                              name = \"excise\"\n\
                              version = \"0.0.0\"\n";
    const MAIN_RS: &str = "fn main() {\n    println!(\"refs test marker\");\n}\n";
    const BROKEN_MAIN_RS: &str = "fn main() {\n    let _: u32 = \"not a number\";\n}\n";

    /// Runs `git -C <dir> <args>` for a test, with no `GIT_*` variable of the caller's (a git hook
    /// sets some that would point git at the caller's own repository) and no configuration file of
    /// the caller's or the machine's, and returns what it printed.
    fn git_in(dir: &Path, args: &[&str]) -> String {
        let mut command = Command::new("git");
        for (name, _) in env::vars_os() {
            if name.to_string_lossy().starts_with("GIT_") {
                command.env_remove(name);
            }
        }
        command
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", dir.join("no-global-gitconfig"));
        let output = bounded::capture(&mut command, GIT_DEADLINE)
            .expect("git runs")
            .expect("git ends in time");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn rustup_available() -> bool {
        let mut command = Command::new("rustup");
        command.arg("--version");
        bounded::capture(&mut command, RUSTUP_DEADLINE)
            .ok()
            .flatten()
            .is_some_and(|output| output.status.success())
    }

    /// A throwaway repository holding a minimal crate called `excise`, and a layout, in a
    /// temporary directory of its own, to build it in.
    struct Repo {
        _dir: TempDir,
        root: PathBuf,
        layout: RefLayout,
        logs: PathBuf,
    }

    impl Repo {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("a temporary directory");
            let root = dir.path().join("repo");
            fs::create_dir(&root).expect("the repository directory");
            let layout = RefLayout::below(&dir.path().join("target"), "builds", "worktrees");
            let logs = dir.path().join("logs");
            let repo = Self {
                _dir: dir,
                root,
                layout,
                logs,
            };
            repo.git(&["init", "--quiet"]);
            repo.git(&["symbolic-ref", "HEAD", "refs/heads/main"]);
            repo.git(&["config", "user.name", "Refs Test"]);
            repo.git(&["config", "user.email", "refs-test@example.invalid"]);
            repo.git(&["config", "commit.gpgsign", "false"]);
            repo.git(&["config", "tag.gpgsign", "false"]);
            repo.write("Cargo.toml", CARGO_TOML);
            repo.write("Cargo.lock", CARGO_LOCK);
            repo.write("src/main.rs", MAIN_RS);
            repo.commit("a minimal crate");
            repo
        }

        fn git(&self, args: &[&str]) -> String {
            git_in(&self.root, args)
        }

        fn write(&self, relative: &str, contents: &str) {
            let path = self.root.join(relative);
            fs::create_dir_all(path.parent().expect("a parent")).expect("a directory");
            fs::write(path, contents).expect("a file");
        }

        /// Commits everything as it is, even when nothing changed, and returns the commit.
        fn commit(&self, message: &str) -> String {
            self.git(&["add", "-A"]);
            self.git(&["commit", "--quiet", "--allow-empty", "-m", message]);
            self.git(&["rev-parse", "HEAD"])
        }

        /// Commits the given toolchain files and no others.
        fn commit_toolchain_files(&self, files: &[(&str, &str)]) -> String {
            for name in [TOOLCHAIN_FILE, LEGACY_TOOLCHAIN_FILE] {
                let _ = fs::remove_file(self.root.join(name));
            }
            for (name, text) in files {
                self.write(name, text);
            }
            self.commit("toolchain files")
        }

        fn spec<'a>(&'a self, policy: ToolchainPolicy, log: Option<&'a Path>) -> BuildSpec<'a> {
            BuildSpec {
                root: &self.root,
                layout: &self.layout,
                policy,
                log,
            }
        }

        fn binary_of(&self, sha: &str) -> PathBuf {
            binary_path(&self.layout.builds.join(sha))
        }

        /// The worktrees directory holds nothing, and git lists only the main checkout.
        fn assert_only_the_main_worktree(&self) {
            let left = fs::read_dir(&self.layout.worktrees).map_or(0, Iterator::count);
            assert_eq!(
                left, 0,
                "{left} entries are left in the worktrees directory"
            );
            let listing = self.git(&["worktree", "list", "--porcelain"]);
            let listed = listing
                .lines()
                .filter(|line| line.starts_with("worktree "))
                .count();
            assert_eq!(listed, 1, "git still lists a worktree:\n{listing}");
        }

        /// The directory of the builds' worktrees holds nothing, and git lists none of them.
        fn assert_no_build_worktree_is_left(&self, when: &str) {
            let left = fs::read_dir(&self.layout.worktrees).map_or(0, Iterator::count);
            assert_eq!(
                left, 0,
                "{when}: {left} entries are left in the worktrees directory"
            );
            let listing = self.git(&["worktree", "list", "--porcelain"]);
            assert!(
                !listing.contains("/target/worktrees/"),
                "{when}: git still lists a worktree of the build:\n{listing}"
            );
        }

        /// Adds a worktree of the repository called `name`, in a directory of its own next to
        /// the repository, and returns that directory.
        fn add_worktree(&self, name: &str) -> PathBuf {
            let parent = self.root.parent().expect("a parent");
            let dir = parent.join("elsewhere").join(name);
            let path = dir.to_str().expect("a UTF-8 path");
            self.git(&["worktree", "add", "--detach", path, "HEAD"]);
            dir
        }

        /// Adds a worktree called `name` and deletes its directory by hand, as an unmounted disk
        /// or a moved directory would: git keeps its entry, and lists it as prunable.
        fn add_missing_worktree(&self, name: &str) {
            let dir = self.add_worktree(name);
            fs::remove_dir_all(dir).expect("delete the directory by hand");
        }

        /// The entry git keeps for the worktree called `name`: `<git-dir>/worktrees/<name>`.
        fn entry_of(&self, name: &str) -> PathBuf {
            self.root.join(".git").join("worktrees").join(name)
        }

        /// The lines `git worktree list --porcelain` has for the worktree called `name`, if it
        /// lists one.
        fn listing_of(&self, name: &str) -> Option<String> {
            let listing = self.git(&["worktree", "list", "--porcelain"]);
            let suffix = format!("/{name}");
            listing
                .split("\n\n")
                .find(|block| {
                    let first = block.lines().next().unwrap_or_default();
                    first.ends_with(&suffix)
                })
                .map(str::to_owned)
        }

        /// The worktree called `name`, whose directory is gone, is still known to git: its entry
        /// is there, and it is listed as prunable.
        fn assert_missing_worktree_survives(&self, name: &str, when: &str) {
            let entry = self.entry_of(name);
            assert!(
                entry.is_dir(),
                "{when}: git forgot the worktree `{name}`: `{}` is gone",
                entry.display()
            );
            let listed = self.listing_of(name);
            let block = listed.unwrap_or_else(|| panic!("{when}: git no longer lists `{name}`"));
            assert!(
                block.contains("prunable"),
                "{when}: `{name}` is not listed as prunable:\n{block}"
            );
        }
    }

    #[test]
    fn resolve_commit_gives_the_same_sha_for_a_branch_and_for_both_kinds_of_tag() {
        let repo = Repo::new();
        let head = repo.git(&["rev-parse", "HEAD"]);
        repo.git(&["branch", "feature"]);
        repo.git(&["tag", "light"]);
        repo.git(&["tag", "-a", "annotated", "-m", "an annotated tag"]);
        // The annotated tag is an object of its own: only peeling it gives the commit.
        assert_ne!(repo.git(&["rev-parse", "annotated"]), head);
        for reference in ["feature", "light", "annotated", "main", "HEAD", &head] {
            assert_eq!(
                resolve_commit(&repo.root, reference).expect("a commit"),
                head,
                "{reference}"
            );
        }
    }

    #[test]
    fn resolve_commit_names_a_ref_it_cannot_resolve() {
        let repo = Repo::new();
        let error = resolve_commit(&repo.root, "no-such-ref").expect_err("an unknown ref");
        assert!(matches!(error, BuildError::NoSuchCommit { .. }), "{error}");
        assert!(error.to_string().contains("`no-such-ref`"), "{error}");
        // A ref that begins with a dash is refused before git can read it as an option.
        assert!(resolve_commit(&repo.root, "--all").is_err());
    }

    #[test]
    fn release_tags_keep_the_v1_releases_in_version_order() {
        let repo = Repo::new();
        let tags = [
            "v1.10.0",
            "v1.0.0",
            "v1.2.10",
            "v0.9.0",
            "v1.2.9",
            "v1.4.0-rc.1",
            "v2.0.0",
            "release-1",
            "v1.2.0",
            "v1.3",
            "v1.3.0.1",
        ];
        for tag in tags {
            repo.git(&["tag", tag]);
        }
        assert_eq!(
            release_tags(&repo.root).expect("release tags"),
            ["v1.0.0", "v1.2.0", "v1.2.9", "v1.2.10", "v1.10.0"]
        );
    }

    #[test]
    fn release_tags_fail_when_none_is_left_and_say_to_fetch_the_tags() {
        let repo = Repo::new();
        for tag in ["v0.9.0", "v1.4.0-rc.1", "v2.0.0", "release-1"] {
            repo.git(&["tag", tag]);
        }
        let error = release_tags(&repo.root).expect_err("no release tag");
        assert!(matches!(error, BuildError::NoReleaseTags { .. }), "{error}");
        assert!(error.to_string().contains("git fetch --tags"), "{error}");
    }

    #[test]
    fn a_release_tag_is_exactly_v1_and_two_numbers() {
        for tag in ["v1.0.0", "v1.2.10", "v1.10.0", "v1.0.99"] {
            assert!(is_release_tag(tag), "{tag}");
        }
        for tag in [
            "v1.0",
            "v1.0.",
            "v1..0",
            "v1.0.0.0",
            "v1.0.0-rc.1",
            "v1.x.0",
            "v2.0.0",
            "v0.1.0",
            "1.0.0",
            "V1.0.0",
            "v1.0.0 ",
            "",
        ] {
            assert!(!is_release_tag(tag), "{tag}");
        }
    }

    #[test]
    fn a_toolchain_file_is_read_through_comments_extra_keys_and_either_quote() {
        let cases = [
            (
                "a bare table",
                "[toolchain]\nchannel = \"1.88.0\"\n",
                "1.88.0",
            ),
            (
                "comments, blank lines, and extra keys",
                "# pinned for the release build\n\n[toolchain]   # the only table\n\
                 channel = \"1.88.0\"  # exact\n\ncomponents = [\"clippy\", \"rustfmt\"]\n\
                 profile = 'minimal'\n",
                "1.88.0",
            ),
            (
                "single quotes",
                "[toolchain]\nchannel = '1.88.0'\n",
                "1.88.0",
            ),
            (
                "no spaces around the equals sign",
                "[toolchain]\nchannel=\"stable\"\n",
                "stable",
            ),
            (
                "Windows line endings",
                "[toolchain]\r\nchannel = \"1.88.0\"\r\ncomponents = [\"clippy\"]\r\n",
                "1.88.0",
            ),
            (
                "a commented-out channel before the real one",
                "[toolchain]\n# channel = \"1.70.0\"\nchannel = \"1.88.0\"\n",
                "1.88.0",
            ),
            (
                "an array over several lines, with comments",
                "[toolchain]\ncomponents = [\n    \"clippy\",  # lint\n    \"rustfmt\",\n]\n\
                 channel = \"nightly-2026-08-18\"\n",
                "nightly-2026-08-18",
            ),
            (
                "a channel after another table that has one too",
                "[other]\nchannel = \"wrong\"\n\n[toolchain]\nchannel = \"1.88.0\"\n\n\
                 [more]\nchannel = \"also wrong\"\n",
                "1.88.0",
            ),
            (
                "a host triple",
                "[toolchain]\nchannel = \"stable-aarch64-apple-darwin\"\n",
                "stable-aarch64-apple-darwin",
            ),
        ];
        for (label, text, channel) in cases {
            assert_eq!(
                parse_toolchain_file(TOOLCHAIN_FILE, text).as_deref(),
                Ok(channel),
                "{label}"
            );
        }
    }

    #[test]
    fn a_toolchain_file_with_a_path_or_no_usable_channel_is_an_error() {
        let cases = [
            ("a path", "[toolchain]\npath = \"/opt/rust\"\n", "path"),
            (
                "a path next to a channel",
                "[toolchain]\nchannel = \"1.88.0\"\npath = \"/opt/rust\"\n",
                "path",
            ),
            (
                "no channel",
                "[toolchain]\ncomponents = [\"clippy\"]\n",
                "no `channel`",
            ),
            (
                "no table",
                "channel = \"1.88.0\"\n",
                "no `[toolchain]` table",
            ),
            (
                "a channel in another table",
                "[other]\nchannel = \"1.88.0\"\n",
                "no `[toolchain]` table",
            ),
            (
                "a channel that is not a string",
                "[toolchain]\nchannel = 1.88\n",
                "quoted string",
            ),
            (
                "a channel that reads as an option",
                "[toolchain]\nchannel = \"--help\"\n",
                "not a toolchain name",
            ),
            (
                "a channel set twice",
                "[toolchain]\nchannel = \"1.88.0\"\nchannel = \"1.90.0\"\n",
                "twice",
            ),
            (
                "a string that is not closed",
                "[toolchain]\nchannel = \"1.88.0\n",
                "not closed",
            ),
            (
                "an array that is not closed",
                "[toolchain]\ncomponents = [\"clippy\"\n",
                "never closed",
            ),
            ("an empty file", "\n  \n", "empty"),
            (
                "a bare channel in a TOML file",
                "1.88.0\n",
                "expected `key = value`",
            ),
        ];
        for (label, text, expected) in cases {
            let problem = parse_toolchain_file(TOOLCHAIN_FILE, text)
                .expect_err(&format!("{label} must be refused"));
            assert!(problem.contains(expected), "{label}: {problem}");
        }
    }

    #[test]
    fn a_legacy_toolchain_file_may_hold_a_bare_channel_or_toml() {
        for (text, channel) in [
            ("1.88.0\n", "1.88.0"),
            ("1.88.0", "1.88.0"),
            ("  nightly-2026-08-18  \r\n", "nightly-2026-08-18"),
            ("[toolchain]\nchannel = \"1.88.0\"\n", "1.88.0"),
        ] {
            assert_eq!(
                parse_toolchain_file(LEGACY_TOOLCHAIN_FILE, text).as_deref(),
                Ok(channel),
                "{text:?}"
            );
        }
        // Rustup reads two lines as TOML, whatever the first one says.
        assert!(parse_toolchain_file(LEGACY_TOOLCHAIN_FILE, "1.88.0\n\n").is_err());
        assert!(parse_toolchain_file(LEGACY_TOOLCHAIN_FILE, "--help\n").is_err());
        assert!(parse_toolchain_file(LEGACY_TOOLCHAIN_FILE, "\n").is_err());
    }

    #[test]
    fn pinned_channel_reads_the_toolchain_file_of_the_commit_not_of_the_working_tree() {
        let repo = Repo::new();
        let text = "# pinned\n\n[toolchain]\nchannel = \"1.88.0\"\ncomponents = [\"clippy\"]\n";
        let pinned = repo.commit_toolchain_files(&[(TOOLCHAIN_FILE, text)]);
        let later =
            repo.commit_toolchain_files(&[(TOOLCHAIN_FILE, "[toolchain]\nchannel = \"1.98.0\"\n")]);
        assert_eq!(
            pinned_channel(&repo.root, &pinned).expect("a pin"),
            Some("1.88.0".to_owned())
        );
        assert_eq!(
            pinned_channel(&repo.root, &later).expect("a pin"),
            Some("1.98.0".to_owned())
        );
        // The working tree now holds something else, which no commit pins.
        repo.write(TOOLCHAIN_FILE, "[toolchain]\nchannel = \"1.0.0\"\n");
        assert_eq!(
            pinned_channel(&repo.root, &pinned).expect("a pin"),
            Some("1.88.0".to_owned())
        );
    }

    #[test]
    fn pinned_channel_reads_a_legacy_plain_rust_toolchain() {
        let repo = Repo::new();
        let sha = repo.commit_toolchain_files(&[(LEGACY_TOOLCHAIN_FILE, "1.88.0\n")]);
        assert_eq!(
            pinned_channel(&repo.root, &sha).expect("a pin"),
            Some("1.88.0".to_owned())
        );
    }

    #[test]
    fn pinned_channel_is_none_for_a_commit_without_a_toolchain_file() {
        let repo = Repo::new();
        let sha = repo.commit_toolchain_files(&[]);
        assert_eq!(pinned_channel(&repo.root, &sha).expect("no error"), None);
    }

    #[test]
    fn pinned_channel_refuses_a_path_and_a_missing_channel() {
        let repo = Repo::new();
        let with_path =
            repo.commit_toolchain_files(&[(TOOLCHAIN_FILE, "[toolchain]\npath = \"/opt/rust\"\n")]);
        let without_channel = repo.commit_toolchain_files(&[(
            TOOLCHAIN_FILE,
            "[toolchain]\ncomponents = [\"clippy\"]\n",
        )]);
        for (sha, expected) in [(with_path, "`path`"), (without_channel, "no `channel`")] {
            let error = pinned_channel(&repo.root, &sha).expect_err("an unusable file");
            assert!(matches!(error, BuildError::ToolchainFile { .. }), "{error}");
            let text = error.to_string();
            assert!(text.contains(expected), "{text}");
            assert!(
                text.contains(TOOLCHAIN_FILE) && text.contains(&sha),
                "{text}"
            );
        }
    }

    #[test]
    fn rust_toolchain_wins_over_rust_toolchain_toml_as_it_does_for_rustup() {
        let repo = Repo::new();
        let sha = repo.commit_toolchain_files(&[
            (TOOLCHAIN_FILE, "[toolchain]\nchannel = \"1.98.0\"\n"),
            (LEGACY_TOOLCHAIN_FILE, "1.88.0\n"),
        ]);
        assert_eq!(
            pinned_channel(&repo.root, &sha).expect("a pin"),
            Some("1.88.0".to_owned())
        );
    }

    #[test]
    fn a_pinned_build_runs_under_rustup_with_nothing_that_could_override_the_pin() {
        let layout = RefLayout::below(Path::new("target"), "builds", "worktrees");
        let spec = BuildSpec {
            root: Path::new("repo"),
            layout: &layout,
            policy: ToolchainPolicy::RefPinned,
            log: None,
        };
        let worktree = Path::new("worktrees/abc-1-0");
        let target_dir = Path::new("builds/abc");

        let command = build_command(&spec, Some("1.88.0"), worktree, target_dir);

        assert_eq!(command.get_program(), "rustup");
        let args: Vec<&OsStr> = command.get_args().collect();
        assert_eq!(
            args,
            [
                "run",
                "1.88.0",
                "cargo",
                "build",
                "--release",
                "--locked",
                "-p",
                "excise"
            ]
        );
        assert_eq!(command.get_current_dir(), Some(worktree));
        let envs: BTreeMap<&OsStr, Option<&OsStr>> = command.get_envs().collect();
        assert_eq!(
            envs.get(OsStr::new("CARGO_TARGET_DIR")),
            Some(&Some(target_dir.as_os_str()))
        );
        for name in ["RUSTUP_TOOLCHAIN", "CARGO", "RUSTC", "RUSTDOC"] {
            assert_eq!(envs.get(OsStr::new(name)), Some(&None), "{name} is kept");
        }
        assert_eq!(
            envs.get(OsStr::new("RUSTUP_AUTO_INSTALL")),
            Some(&Some(OsStr::new("0")))
        );
    }

    #[test]
    fn a_pinned_build_of_a_ref_that_pins_nothing_runs_plain_cargo_in_the_same_environment() {
        let layout = RefLayout::below(Path::new("target"), "builds", "worktrees");
        let spec = BuildSpec {
            root: Path::new("repo"),
            layout: &layout,
            policy: ToolchainPolicy::RefPinned,
            log: None,
        };
        let command = build_command(&spec, None, Path::new("w"), Path::new("t"));

        assert_eq!(command.get_program(), "cargo");
        let args: Vec<&OsStr> = command.get_args().collect();
        assert_eq!(args, ["build", "--release", "--locked", "-p", "excise"]);
        let envs: BTreeMap<&OsStr, Option<&OsStr>> = command.get_envs().collect();
        for name in ["RUSTUP_TOOLCHAIN", "CARGO", "RUSTC", "RUSTDOC"] {
            assert_eq!(envs.get(OsStr::new(name)), Some(&None), "{name} is kept");
        }
        assert_eq!(
            envs.get(OsStr::new("RUSTUP_AUTO_INSTALL")),
            Some(&Some(OsStr::new("0")))
        );
    }

    #[test]
    fn an_inherited_build_changes_nothing_in_the_environment_but_the_target_directory() {
        let layout = RefLayout::below(Path::new("target"), "builds", "worktrees");
        let spec = BuildSpec {
            root: Path::new("repo"),
            layout: &layout,
            policy: ToolchainPolicy::Inherited,
            log: None,
        };
        let worktree = Path::new("worktrees/abc-1-0");
        let target_dir = Path::new("builds/abc");

        let command = build_command(&spec, None, worktree, target_dir);

        let cargo = env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
        assert_eq!(command.get_program(), cargo.as_os_str());
        let args: Vec<&OsStr> = command.get_args().collect();
        assert_eq!(args, ["build", "--release", "--locked", "-p", "excise"]);
        assert_eq!(command.get_current_dir(), Some(worktree));
        let envs: Vec<(&OsStr, Option<&OsStr>)> = command.get_envs().collect();
        assert_eq!(
            envs,
            [(OsStr::new("CARGO_TARGET_DIR"), Some(target_dir.as_os_str()))]
        );
    }

    #[test]
    fn the_tail_of_a_log_is_its_last_lines() {
        let log = (1..=40)
            .map(|number| format!("line {number}"))
            .collect::<Vec<_>>()
            .join("\n");
        let tail = tail_lines(&log, 30);
        assert_eq!(tail.lines().count(), 30);
        assert!(
            tail.starts_with("line 11\n") && tail.ends_with("line 40"),
            "{tail}"
        );
        assert_eq!(tail_lines("one\ntwo\n", 30), "one\ntwo");
        assert_eq!(tail_lines("", 30), "");
    }

    #[test]
    fn worktree_directories_are_unique_and_named_for_the_commit() {
        let parent = Path::new("worktrees");
        let first = unique_worktree_dir(parent, "abc123");
        let second = unique_worktree_dir(parent, "abc123");
        assert_ne!(first, second);
        for dir in [first, second] {
            assert_eq!(dir.parent(), Some(parent));
            let name = dir.file_name().expect("a name").to_string_lossy();
            assert!(name.starts_with("abc123-"), "{name}");
        }
    }

    #[test]
    fn a_build_error_with_a_leftover_worktree_reports_both() {
        let failed = BuildError::Build {
            what: "`v1.0.0`".to_owned(),
            outcome: "exit status: 101".to_owned(),
            log: None,
        };
        let left = BuildError::WorktreeLeft {
            dir: PathBuf::from("worktrees/abc-1-0"),
            detail: "permission denied".to_owned(),
        };
        let text = failed.with_cleanup(left).to_string();
        assert!(text.contains("exit status: 101"), "{text}");
        assert!(text.contains("worktrees/abc-1-0"), "{text}");
        assert!(text.contains("git worktree remove --force"), "{text}");
        // `git worktree prune` forgets every worktree whose directory is missing: it is no plain
        // next step, and the message says what it reaches.
        assert!(!text.contains("and then `git worktree prune`"), "{text}");
        assert!(
            text.contains("every other worktree whose directory is missing"),
            "{text}"
        );
    }

    #[test]
    fn require_toolchain_names_the_command_that_installs_a_missing_toolchain() {
        if !rustup_available() {
            eprintln!("skipped: rustup cannot be run here");
            return;
        }
        let error = require_toolchain("0.0.1").expect_err("no toolchain has that name");
        assert!(
            matches!(error, BuildError::ToolchainMissing { .. }),
            "{error}"
        );
        assert!(
            error.to_string().contains("rustup toolchain install 0.0.1"),
            "{error}"
        );
    }

    #[test]
    fn a_missing_pinned_toolchain_stops_the_build_before_anything_is_created() {
        if !rustup_available() {
            eprintln!("skipped: rustup cannot be run here");
            return;
        }
        let repo = Repo::new();
        repo.commit_toolchain_files(&[(TOOLCHAIN_FILE, "[toolchain]\nchannel = \"0.0.1\"\n")]);
        let spec = repo.spec(ToolchainPolicy::RefPinned, None);

        let planned = plan_ref(&spec, "HEAD").expect_err("the toolchain is not installed");
        let built = build_ref(&spec, "HEAD").expect_err("the toolchain is not installed");

        for error in [planned, built] {
            let text = error.to_string();
            assert!(text.contains("rustup toolchain install 0.0.1"), "{text}");
        }
        assert!(!repo.layout.worktrees.exists(), "a worktree was made");
        assert!(!repo.layout.builds.exists(), "a build directory was made");
    }

    #[test]
    fn plan_ref_reads_the_pin_only_for_a_pinned_build_and_requires_it_to_exist_only_then() {
        let repo = Repo::new();
        let sha =
            repo.commit_toolchain_files(&[(TOOLCHAIN_FILE, "[toolchain]\nchannel = \"0.0.1\"\n")]);
        // `Inherited` does not look at the toolchain file, so a pin nobody has installed is fine.
        let inherited = plan_ref(&repo.spec(ToolchainPolicy::Inherited, None), "HEAD")
            .expect("an inherited plan");
        assert_eq!(
            inherited,
            PlannedRef {
                reference: "HEAD".to_owned(),
                sha,
                channel: None,
            }
        );
        // A ref that pins nothing needs no toolchain either way.
        let sha = repo.commit_toolchain_files(&[]);
        let unpinned = plan_ref(&repo.spec(ToolchainPolicy::RefPinned, None), "HEAD")
            .expect("a plan for a ref that pins nothing");
        assert_eq!((unpinned.sha, unpinned.channel), (sha, None));
        assert!(!repo.layout.worktrees.exists() && !repo.layout.builds.exists());
    }

    #[test]
    fn an_unknown_ref_fails_before_anything_is_created() {
        let repo = Repo::new();
        let error = build_ref(&repo.spec(ToolchainPolicy::Inherited, None), "no-such-ref")
            .expect_err("an unknown ref");
        assert!(error.to_string().contains("`no-such-ref`"), "{error}");
        assert!(!repo.layout.worktrees.exists() && !repo.layout.builds.exists());
    }

    #[test]
    fn a_build_is_cached_by_commit_and_its_worktree_is_removed() {
        let repo = Repo::new();
        let head = repo.git(&["rev-parse", "HEAD"]);
        let spec = repo.spec(ToolchainPolicy::Inherited, None);

        let first = build_ref(&spec, "main").expect("the first build");
        assert!(!first.cached);
        assert_eq!(first.sha, head);
        assert_eq!(first.toolchain, None);
        assert!(first.binary.is_file(), "{}", first.binary.display());
        let relative = Path::new(&head)
            .join("release")
            .join(format!("excise{}", env::consts::EXE_SUFFIX));
        assert!(
            first.binary.ends_with(relative),
            "{}",
            first.binary.display()
        );
        repo.assert_only_the_main_worktree();

        // A hit makes no worktree: with the parent directory gone, a worktree would bring it back.
        fs::remove_dir(&repo.layout.worktrees).expect("remove the empty worktrees directory");
        let second = build_ref(&spec, "HEAD").expect("the cached build");
        assert!(second.cached);
        assert_eq!(second.sha, head);
        assert_eq!(second.binary, first.binary);
        assert!(
            !repo.layout.worktrees.exists(),
            "a cache hit made a worktree"
        );
        repo.assert_only_the_main_worktree();
    }

    #[test]
    fn a_failed_build_reports_its_status_and_log_and_leaves_nothing_behind() {
        let repo = Repo::new();
        repo.write("src/main.rs", BROKEN_MAIN_RS);
        let broken = repo.commit("a crate that does not compile");
        repo.git(&["tag", "v1.0.0-broken"]);
        let log = repo.logs.join("broken.log");
        let spec = repo.spec(ToolchainPolicy::Inherited, Some(&log));

        let error = build_ref(&spec, "v1.0.0-broken").expect_err("the build must fail");

        let text = error.to_string();
        assert!(matches!(error, BuildError::Build { .. }), "{text}");
        // Cargo exits with 101 when the compiler fails, and `E0308` is the type error.
        assert!(text.contains("101"), "the exit status is not in: {text}");
        assert!(
            text.contains("E0308"),
            "the compiler's output is not in: {text}"
        );
        assert!(
            text.contains("broken.log"),
            "the log is not named in: {text}"
        );
        let logged = fs::read_to_string(&log).expect("the log");
        assert!(logged.contains("E0308"), "{logged}");
        assert!(
            !repo.binary_of(&broken).exists(),
            "a failed build left a binary"
        );
        repo.assert_only_the_main_worktree();
    }

    #[test]
    fn a_ref_is_built_with_the_toolchain_it_pins_and_the_toolchain_is_recorded() {
        let workspace_pin = fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../rust-toolchain.toml"),
        )
        .expect("the workspace's toolchain file");
        let channel = parse_toolchain_file(TOOLCHAIN_FILE, &workspace_pin)
            .expect("the workspace pins a channel");
        if !rustup_available() || require_toolchain(&channel).is_err() {
            eprintln!("skipped: rustup cannot be run here, or `{channel}` is not installed");
            return;
        }
        let repo = Repo::new();
        let pin = format!("[toolchain]\nchannel = \"{channel}\"\n");
        let sha = repo.commit_toolchain_files(&[(TOOLCHAIN_FILE, pin.as_str())]);
        let log = repo.logs.join("pinned.log");
        let spec = repo.spec(ToolchainPolicy::RefPinned, Some(&log));

        let planned = plan_ref(&spec, "HEAD").expect("a plan");
        assert_eq!(planned.channel.as_deref(), Some(channel.as_str()));

        let built = build_ref(&spec, "HEAD").expect("the pinned build");
        assert!(!built.cached);
        assert_eq!(built.sha, sha);
        assert!(built.binary.is_file(), "{}", built.binary.display());
        let used = built
            .toolchain
            .clone()
            .expect("the toolchain that built it");
        assert_eq!(used.channel, channel);
        assert!(used.rustc.starts_with("rustc "), "{}", used.rustc);
        assert!(repo.layout.builds.join(&sha).join(RECORD_FILE).is_file());
        let logged = fs::metadata(&log).expect("the log").len();
        assert!(logged > 0, "the log is empty");
        repo.assert_only_the_main_worktree();

        fs::remove_dir(&repo.layout.worktrees).expect("remove the empty worktrees directory");
        let again = build_ref(&spec, "HEAD").expect("the cached build");
        assert!(again.cached);
        assert_eq!(again.binary, built.binary);
        assert_eq!(again.toolchain, Some(used));
        assert!(
            !repo.layout.worktrees.exists(),
            "a cache hit made a worktree"
        );
    }

    #[test]
    fn a_binary_without_a_matching_record_is_not_a_hit_for_a_pinned_build() {
        let repo = Repo::new();
        let sha = repo.git(&["rev-parse", "HEAD"]);
        let target_dir = absolute(&repo.layout.builds).expect("absolute").join(&sha);
        let binary = binary_path(&target_dir);
        fs::create_dir_all(binary.parent().expect("a parent")).expect("the release directory");
        fs::write(&binary, "left by a failed run").expect("a stale binary");
        let plan = PlannedRef {
            reference: "HEAD".to_owned(),
            sha,
            channel: Some("1.88.0".to_owned()),
        };
        let pinned = repo.spec(ToolchainPolicy::RefPinned, None);
        let inherited = repo.spec(ToolchainPolicy::Inherited, None);

        assert_eq!(cached_build(&pinned, &plan, &target_dir), None, "no record");
        // The inherited policy keeps no record: the binary alone is a hit, as it always was.
        assert!(cached_build(&inherited, &plan, &target_dir).is_some_and(|built| built.cached));

        fs::write(
            target_dir.join(RECORD_FILE),
            "1.90.0\nrustc 1.90.0 (abc 2026-01-01)\n",
        )
        .expect("a record");
        assert_eq!(
            cached_build(&pinned, &plan, &target_dir),
            None,
            "another channel"
        );

        fs::write(target_dir.join(RECORD_FILE), "1.88.0\n").expect("a half record");
        assert_eq!(
            cached_build(&pinned, &plan, &target_dir),
            None,
            "no rustc line"
        );

        fs::write(
            target_dir.join(RECORD_FILE),
            "1.88.0\nrustc 1.88.0 (abc 2025-06-23)\n",
        )
        .expect("a record");
        let hit = cached_build(&pinned, &plan, &target_dir).expect("a hit");
        assert!(hit.cached);
        assert_eq!(
            hit.toolchain,
            Some(ToolchainUsed {
                channel: "1.88.0".to_owned(),
                rustc: "rustc 1.88.0 (abc 2025-06-23)".to_owned(),
            })
        );
    }

    #[test]
    fn the_worktree_goes_when_its_guard_is_dropped_and_when_a_panic_unwinds_through_it() {
        let repo = Repo::new();
        let head = repo.git(&["rev-parse", "HEAD"]);
        let parent = repo.layout.worktrees.clone();

        let dir = {
            let guard = WorktreeGuard::add(&repo.root, &parent, &head).expect("a worktree");
            assert!(guard.dir.join("Cargo.toml").is_file());
            guard.dir.clone()
        };
        assert!(!dir.exists(), "{} is still there", dir.display());
        repo.assert_only_the_main_worktree();

        let unwound: Result<(), _> = panic::catch_unwind(|| {
            let guard = WorktreeGuard::add(&repo.root, &parent, &head).expect("a worktree");
            assert!(guard.dir.is_dir());
            panic::resume_unwind(Box::new("a panic while the worktree exists"))
        });
        assert!(unwound.is_err());
        repo.assert_only_the_main_worktree();
    }

    #[test]
    fn finish_removes_the_worktree_and_says_so_when_it_cannot() {
        let repo = Repo::new();
        let head = repo.git(&["rev-parse", "HEAD"]);
        let guard =
            WorktreeGuard::add(&repo.root, &repo.layout.worktrees, &head).expect("a worktree");
        guard.finish().expect("the worktree is removed");
        repo.assert_only_the_main_worktree();

        // Neither git nor deleting a directory can remove a file, and a file is left alone.
        let stuck = repo.layout.worktrees.join("not-a-directory");
        fs::write(&stuck, "left alone").expect("a file");
        let guard = WorktreeGuard {
            root: repo.root.clone(),
            dir: stuck.clone(),
            real_dir: stuck.clone(),
            admin: Err("it has none".to_owned()),
            armed: true,
        };
        let error = guard.finish().expect_err("a file is not a worktree");
        assert!(matches!(error, BuildError::WorktreeLeft { .. }), "{error}");
        assert!(
            error.to_string().contains(&stuck.display().to_string()),
            "{error}"
        );
        assert!(
            error.to_string().contains("nothing is deleted by hand"),
            "{error}"
        );
        assert!(stuck.is_file(), "the file was deleted");
    }

    #[test]
    fn a_deadline_is_described_in_whole_minutes_when_it_is_that_and_in_seconds_otherwise() {
        for (limit, text) in [
            (Duration::from_secs(3600), "60 minutes"),
            (Duration::from_secs(300), "5 minutes"),
            (Duration::from_secs(60), "1 minute"),
            (Duration::from_secs(90), "90 seconds"),
            (Duration::from_secs(2), "2 seconds"),
            (Duration::from_secs(1), "1 second"),
            (Duration::from_millis(250), "250ms"),
        ] {
            assert_eq!(describe_limit(limit), text);
        }
        assert_eq!(
            timed_out_after(Duration::from_secs(300)),
            "timed out after 5 minutes and was killed"
        );
    }

    #[test]
    fn a_build_is_bounded_at_sixty_minutes_and_a_git_or_rustup_command_at_five() {
        assert_eq!(Build::STANDARD.deadline, Duration::from_secs(60 * 60));
        assert_eq!(GIT_DEADLINE, Duration::from_secs(5 * 60));
        assert_eq!(RUSTUP_DEADLINE, Duration::from_secs(5 * 60));
    }

    #[test]
    fn a_worktree_directory_is_never_one_that_exists() {
        let parent = tempfile::tempdir().expect("a temporary directory");
        let first = unique_worktree_dir(parent.path(), "abc123");
        let counter: u64 = first
            .file_name()
            .and_then(OsStr::to_str)
            .and_then(|name| name.rsplit('-').next())
            .and_then(|digits| digits.parse().ok())
            .expect("the name ends in the counter");
        // Take the names the counter gives next, however many tests draw from it meanwhile.
        let prefix = format!("abc123-{}-", std::process::id());
        for ahead in 1..=500 {
            let name = format!("{prefix}{}", counter + ahead);
            fs::create_dir(parent.path().join(name)).expect("take a name");
        }

        let next = unique_worktree_dir(parent.path(), "abc123");

        assert!(!present(&next), "{} exists", next.display());
    }

    #[test]
    fn a_worktree_whose_directory_is_missing_survives_every_way_a_build_cleans_up() {
        let repo = Repo::new();
        repo.add_missing_worktree("unmounted");
        repo.assert_missing_worktree_survives("unmounted", "before any build");
        let log = repo.logs.join("survives.log");
        let spec = repo.spec(ToolchainPolicy::Inherited, Some(&log));

        build_ref(&spec, "main").expect("a build that succeeds");
        repo.assert_missing_worktree_survives("unmounted", "after a build that succeeded");
        repo.assert_no_build_worktree_is_left("after a build that succeeded");

        repo.write("src/main.rs", BROKEN_MAIN_RS);
        repo.commit("a crate that does not compile");
        let error = build_ref(&spec, "HEAD").expect_err("a build that fails");
        assert!(matches!(error, BuildError::Build { .. }), "{error}");
        repo.assert_missing_worktree_survives("unmounted", "after a build that failed");
        repo.assert_no_build_worktree_is_left("after a build that failed");

        // A guard that goes without `finish`, as in a panic or an early return.
        let head = repo.git(&["rev-parse", "HEAD"]);
        let guard =
            WorktreeGuard::add(&repo.root, &repo.layout.worktrees, &head).expect("a worktree");
        drop(guard);
        repo.assert_missing_worktree_survives("unmounted", "after a guard that was dropped");
        repo.assert_no_build_worktree_is_left("after a guard that was dropped");
    }

    #[test]
    fn a_locked_worktree_is_deleted_by_hand_together_with_only_its_own_entry() {
        let repo = Repo::new();
        let head = repo.git(&["rev-parse", "HEAD"]);
        repo.add_missing_worktree("unmounted");
        let neighbour = repo.add_worktree("neighbour");
        let guard =
            WorktreeGuard::add(&repo.root, &repo.layout.worktrees, &head).expect("a worktree");
        let dir = guard.dir.clone();
        let name = dir
            .file_name()
            .expect("a name")
            .to_string_lossy()
            .into_owned();
        let entry = repo.entry_of(&name);
        assert!(entry.is_dir(), "git made no entry at {}", entry.display());
        // `--force` once does not remove a locked worktree: git refuses, and the fallback runs.
        repo.git(&["worktree", "lock", dir.to_str().expect("a UTF-8 path")]);

        guard.finish().expect("the fallback removes the worktree");

        assert!(!dir.exists(), "{} is still there", dir.display());
        assert!(!entry.exists(), "{} is still there", entry.display());
        let listing = repo.git(&["worktree", "list", "--porcelain"]);
        assert!(!listing.contains(&name), "git still lists it:\n{listing}");
        repo.assert_missing_worktree_survives("unmounted", "after the fallback");
        assert!(
            repo.entry_of("neighbour").is_dir(),
            "the neighbour's entry is gone"
        );
        assert!(
            neighbour.join("Cargo.toml").is_file(),
            "the neighbour's files are gone"
        );
        let block = repo
            .listing_of("neighbour")
            .expect("the neighbour is still listed");
        assert!(!block.contains("prunable"), "{block}");
    }

    #[test]
    fn the_fallback_refuses_an_entry_that_is_not_the_worktrees_own() {
        let repo = Repo::new();
        let head = repo.git(&["rev-parse", "HEAD"]);
        let neighbour = repo.add_worktree("neighbour");
        let mut guard =
            WorktreeGuard::add(&repo.root, &repo.layout.worktrees, &head).expect("a worktree");
        let own = guard.admin.clone().expect("the guard knows its entry");
        // A directory elsewhere that holds something that matters. Its `gitdir` file even names
        // this worktree, so that only where it lies can refuse it.
        let decoy = repo.root.with_file_name("decoy");
        fs::create_dir(&decoy).expect("a decoy directory");
        fs::write(decoy.join("precious.txt"), "keep").expect("a file");
        let named = format!("{}\n", guard.real_dir.join(".git").display());
        fs::write(decoy.join("gitdir"), named).expect("a gitdir file");
        let git_dir = repo.root.join(".git");
        let cases = [
            (
                "a directory elsewhere",
                decoy.clone(),
                "not directly inside",
            ),
            ("the git directory", git_dir.clone(), "not directly inside"),
            (
                "the directory of the entries",
                git_dir.join("worktrees"),
                "not directly inside",
            ),
            (
                "another worktree's entry",
                repo.entry_of("neighbour"),
                "not this worktree's",
            ),
            (
                "an entry that is not there",
                repo.entry_of("nothing"),
                "cannot be looked at",
            ),
        ];
        for (label, admin, expected) in cases {
            let problem = guard.check_entry(&admin).expect_err(label);
            assert!(problem.contains(expected), "{label}: {problem}");
        }
        let checked = guard.check_entry(&own).expect("its own entry is accepted");
        assert_eq!(checked.file_name(), own.file_name());
        #[cfg(unix)]
        {
            let link = repo.entry_of("link-to-the-entry");
            std::os::unix::fs::symlink(&own, &link).expect("a link");
            let problem = guard.check_entry(&link).expect_err("a link is refused");
            assert!(problem.contains("not a real directory"), "{problem}");
            fs::remove_file(&link).expect("remove the link");
        }

        // Through `finish`: git will not remove a locked worktree, so the fallback runs, finds an
        // entry that is not the worktree's, and refuses before it deletes anything.
        let dir = guard.dir.clone();
        repo.git(&["worktree", "lock", dir.to_str().expect("a UTF-8 path")]);
        guard.admin = Ok(decoy.clone());
        let error = guard.finish().expect_err("the fallback refuses");
        let text = error.to_string();
        assert!(matches!(error, BuildError::WorktreeLeft { .. }), "{text}");
        assert!(text.contains("refusing to delete"), "{text}");
        assert!(text.contains("not directly inside"), "{text}");
        assert!(
            decoy.join("precious.txt").is_file(),
            "the decoy was deleted: {text}"
        );
        assert!(dir.is_dir(), "the directory was deleted: {text}");
        assert!(own.is_dir(), "its own entry was deleted: {text}");
        assert!(
            neighbour.join("Cargo.toml").is_file(),
            "the neighbour was touched"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_git_command_that_runs_out_of_time_is_killed_and_what_it_made_is_removed() {
        use std::os::unix::fs::PermissionsExt as _;

        let repo = Repo::new();
        let head = repo.git(&["rev-parse", "HEAD"]);
        // A hook that never ends keeps `git worktree add` running after it made the worktree.
        let hooks = repo.root.with_file_name("hooks");
        fs::create_dir(&hooks).expect("a hooks directory");
        let hook = hooks.join("post-checkout");
        fs::write(&hook, "#!/bin/sh\nsleep 30\n").expect("a hook");
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).expect("an executable hook");
        let path = hooks.to_str().expect("a UTF-8 path");
        repo.git(&["config", "core.hooksPath", path]);
        let started = std::time::Instant::now();

        let outcome = WorktreeGuard::add_within(
            &repo.root,
            &repo.layout.worktrees,
            &head,
            Duration::from_secs(2),
        );

        let Err(error) = outcome else {
            panic!("`git worktree add` was not killed at its deadline");
        };
        let text = error.to_string();
        assert!(matches!(error, BuildError::Git(_)), "{text}");
        assert!(
            text.contains("timed out after 2 seconds and was killed"),
            "{text}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(25),
            "git was waited for: {text}"
        );
        repo.assert_only_the_main_worktree();
    }

    /// The pid that a command wrote into `path`.
    #[cfg(unix)]
    fn read_pid(path: &Path) -> u32 {
        fs::read_to_string(path)
            .expect("the command wrote its pid")
            .trim()
            .parse()
            .expect("a pid")
    }

    #[cfg(unix)]
    fn eventually(mut condition: impl FnMut() -> bool) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if condition() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        condition()
    }

    /// Whether every process of the group `group` is gone, which takes a moment after the kill.
    #[cfg(unix)]
    fn group_is_gone(group: u32) -> bool {
        eventually(|| !excise_harness::safety::process_group_exists(group))
    }

    /// Asserts that every process of the group `group` is gone. A group that is not is killed
    /// first, so that a test that fails leaves nothing running.
    #[cfg(unix)]
    fn assert_group_gone(group: u32, what: &str) {
        let gone = group_is_gone(group);
        if !gone {
            let _ = excise_harness::safety::kill_process_group(group);
        }
        assert!(gone, "{what}: the process group {group} is still alive");
    }

    #[cfg(unix)]
    #[test]
    fn a_bounded_run_that_outlives_its_deadline_kills_its_whole_process_group() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let pid_file = dir.path().join("pid");
        let mut command = Command::new("sh");
        // The shell leads the group and starts a `sleep` that is a member of it.
        command
            .args(["-c", "echo $$ > \"$1\"; sleep 60 & wait", "sh"])
            .arg(&pid_file);
        let started = std::time::Instant::now();

        let ended = bounded::run(&mut command, Duration::from_secs(2)).expect("it starts");

        assert!(ended.is_none(), "it was not killed: {ended:?}");
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the run waited for the sleep"
        );
        let group = read_pid(&pid_file);
        assert!(group_is_gone(group), "the process group {group} is alive");
    }

    #[cfg(unix)]
    #[test]
    fn a_captured_command_gives_all_its_output_and_how_it_ended() {
        let mut command = Command::new("sh");
        // More than a pipe holds: the command would stop on a full pipe if nothing read it.
        command.args([
            "-c",
            "printf out; printf err >&2; head -c 300000 /dev/zero; exit 3",
        ]);

        let output = bounded::capture(&mut command, Duration::from_secs(60))
            .expect("it starts")
            .expect("it ends in time");

        assert_eq!(output.status.code(), Some(3));
        assert_eq!(output.stderr, b"err");
        assert_eq!(output.stdout.len(), 3 + 300_000);
        assert!(output.stdout.starts_with(b"out"));
    }

    #[cfg(unix)]
    #[test]
    fn a_captured_command_that_outlives_its_deadline_is_killed_with_what_it_started() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let pid_file = dir.path().join("pid");
        let mut command = Command::new("sh");
        // The `sleep` holds the output of the shell open: without the kill of the group, reading
        // that output would go on for a minute after the shell was killed.
        command
            .args(["-c", "echo $$ > \"$1\"; sleep 60 & wait", "sh"])
            .arg(&pid_file);
        let started = std::time::Instant::now();

        let output = bounded::capture(&mut command, Duration::from_secs(2)).expect("it starts");

        assert!(output.is_none(), "it was not killed: {output:?}");
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the output was waited for"
        );
        let group = read_pid(&pid_file);
        assert!(group_is_gone(group), "the process group {group} is alive");

        // `run_captured` says the same in words.
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 60"]);
        let message = run_captured(&mut command, Duration::from_millis(300))
            .expect_err("a command that does not finish in time is an error");
        assert!(
            message.contains("timed out after 300ms and was killed"),
            "{message}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_captured_command_whose_output_stays_open_after_it_ended_is_an_error_not_a_wait() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let pid_file = dir.path().join("pid");
        let mut command = Command::new("sh");
        // The shell ends at once and leaves a `sleep` that holds its output open.
        command
            .args(["-c", "sleep 30 & echo $! > \"$1\"; echo done", "sh"])
            .arg(&pid_file);
        let started = std::time::Instant::now();

        let outcome = bounded::capture(&mut command, Duration::from_secs(60));

        // Whatever the verdict, the `sleep` goes first: it is a member of the shell's group.
        let sleeper = read_pid(&pid_file);
        let group = excise_harness::safety::process_group_of(sleeper).expect("it is alive");
        excise_harness::safety::kill_process_group(group).expect("kill the group");
        let error = outcome.expect_err("the output stayed open");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut, "{error}");
        assert!(
            error.to_string().contains("still holds its output open"),
            "{error}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(25),
            "the output was waited for"
        );
        assert!(group_is_gone(group), "the process group {group} is alive");
    }

    /// The command of a test build: `script`, run by the shell in the worktree, with the target
    /// directory as `$1`.
    #[cfg(unix)]
    fn shell_build(script: &str, worktree: &Path, target_dir: &Path) -> Command {
        let mut command = Command::new("sh");
        command
            .args(["-c", script, "sh"])
            .arg(target_dir)
            .current_dir(worktree);
        command
    }

    /// A build that never ends: it says that it started, notes its pid, and starts a `sleep`.
    #[cfg(unix)]
    fn endless_build(
        _spec: &BuildSpec<'_>,
        _channel: Option<&str>,
        worktree: &Path,
        target_dir: &Path,
    ) -> Command {
        let script = "echo starting; mkdir -p \"$1\"; echo $$ > \"$1/pid\"; sleep 30 & wait";
        shell_build(script, worktree, target_dir)
    }

    /// A build that succeeds, leaving the binary a build leaves, and exits at once with a `sleep`
    /// of a minute still running in its process group. It notes its pid, the id of that group.
    #[cfg(unix)]
    fn succeeding_build_with_a_sleeper(
        _spec: &BuildSpec<'_>,
        _channel: Option<&str>,
        worktree: &Path,
        target_dir: &Path,
    ) -> Command {
        let script = "mkdir -p \"$1/release\"; : > \"$1/release/excise\"; \
                      echo $$ > \"$1/pid\"; sleep 60 & exit 0";
        shell_build(script, worktree, target_dir)
    }

    /// A build that fails with status 3 and leaves a `sleep` of a minute running in its process
    /// group. It notes its pid, the id of that group.
    #[cfg(unix)]
    fn failing_build_with_a_sleeper(
        _spec: &BuildSpec<'_>,
        _channel: Option<&str>,
        worktree: &Path,
        target_dir: &Path,
    ) -> Command {
        let script = "mkdir -p \"$1\"; echo $$ > \"$1/pid\"; echo broken; sleep 60 & exit 3";
        shell_build(script, worktree, target_dir)
    }

    #[cfg(unix)]
    #[test]
    fn a_build_that_outlives_its_deadline_fails_with_its_log_and_leaves_no_worktree() {
        let repo = Repo::new();
        let head = repo.git(&["rev-parse", "HEAD"]);
        let log = repo.logs.join("endless.log");
        let spec = repo.spec(ToolchainPolicy::Inherited, Some(&log));
        let build = Build {
            deadline: Duration::from_secs(2),
            command: endless_build,
        };
        let started = std::time::Instant::now();

        let error = build_ref_with(&spec, "main", &build).expect_err("the build never ends");

        let text = error.to_string();
        assert!(matches!(error, BuildError::Build { .. }), "{text}");
        assert!(
            text.contains("timed out after 2 seconds and was killed"),
            "{text}"
        );
        assert!(
            text.contains("starting"),
            "the end of the log is not in: {text}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(25),
            "the build was waited for: {text}"
        );
        let group = read_pid(&repo.layout.builds.join(&head).join("pid"));
        assert!(
            group_is_gone(group),
            "the build's process group {group} is alive"
        );
        assert!(
            !repo.binary_of(&head).exists(),
            "a build that did not finish left a binary"
        );
        repo.assert_only_the_main_worktree();
    }

    #[cfg(unix)]
    #[test]
    fn what_a_build_leaves_in_its_process_group_is_killed_when_the_build_exits() {
        let builds: [(&str, BuildCommand, bool); 2] = [
            (
                "a build that succeeds",
                succeeding_build_with_a_sleeper,
                true,
            ),
            ("a build that fails", failing_build_with_a_sleeper, false),
        ];
        for (label, command, succeeds) in builds {
            let repo = Repo::new();
            let head = repo.git(&["rev-parse", "HEAD"]);
            let log = repo.logs.join("leaves.log");
            let spec = repo.spec(ToolchainPolicy::Inherited, Some(&log));
            let build = Build {
                deadline: Duration::from_secs(120),
                command,
            };

            let outcome = build_ref_with(&spec, "main", &build);

            assert_eq!(outcome.is_ok(), succeeds, "{label}: {outcome:?}");
            if let Err(error) = &outcome {
                // The failure is the status the build exited with, nothing else.
                assert!(
                    error.to_string().contains("exit status: 3"),
                    "{label}: {error}"
                );
            }
            let group = read_pid(&repo.layout.builds.join(&head).join("pid"));
            assert_group_gone(group, label);
            repo.assert_only_the_main_worktree();
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_group_of_a_command_that_exited_is_signalled_while_its_leader_is_still_unreaped() {
        // The shell leads a group that holds nothing but itself, so that when the group is
        // signalled it is the leader that keeps the id. It must still be waiting to be reaped
        // then: a leader that had been reaped would leave the id free to be given to another
        // process, and the signal would reach that one.
        for (script, code) in [("exit 0", 0), ("exit 3", 3)] {
            let mut command = Command::new("sh");
            command.args(["-c", script]);
            let mut when_signalled = None;

            let status = bounded::run_with(&mut command, Duration::from_secs(60), |child| {
                when_signalled = Some(excise_harness::safety::has_exited_unreaped(child.id()));
                let _ = excise_harness::safety::kill_process_group(child.id());
            })
            .expect("it runs")
            .expect("it ends in time");

            assert_eq!(
                status.code(),
                Some(code),
                "{script}: the status is still there to be had"
            );
            let leader = when_signalled.expect("the group was signalled");
            assert!(
                matches!(leader, Ok(true)),
                "{script}: when the group was signalled its leader was not waiting to be reaped \
                 ({leader:?}), so its id was free to be given to another process"
            );
        }
    }

    /// A build that makes the binary and keeps the `marker.txt` of the worktree it ran in.
    #[cfg(unix)]
    fn marker_build(
        _spec: &BuildSpec<'_>,
        _channel: Option<&str>,
        worktree: &Path,
        target_dir: &Path,
    ) -> Command {
        let script =
            "mkdir -p \"$1/release\"; : > \"$1/release/excise\"; cp marker.txt \"$1/marker\"";
        shell_build(script, worktree, target_dir)
    }

    #[cfg(unix)]
    #[test]
    fn a_planned_ref_is_built_at_the_commit_it_was_planned_at_when_its_branch_has_moved_on() {
        let repo = Repo::new();
        repo.write("marker.txt", "planned\n");
        let planned_at = repo.commit("what is planned");
        let spec = repo.spec(ToolchainPolicy::Inherited, None);
        let plan = plan_ref(&spec, "main").expect("a plan");
        assert_eq!(plan.sha, planned_at);
        assert!(
            !is_cached(&spec, &plan).expect("a verdict"),
            "a build will run"
        );
        // The branch moves between the planning and the build.
        repo.write("marker.txt", "moved on\n");
        let moved_to = repo.commit("what the branch moves to");
        assert_ne!(moved_to, planned_at);
        let build = Build {
            deadline: Duration::from_secs(120),
            command: marker_build,
        };

        let made = build_planned_with(&spec, &plan, &build).expect("the build");

        assert_eq!(made.sha, planned_at);
        assert!(!made.cached);
        assert_eq!(
            fs::read_to_string(repo.layout.builds.join(&planned_at).join("marker"))
                .expect("the marker of the worktree the build ran in"),
            "planned\n",
            "the build ran at the planned commit"
        );
        assert!(
            !repo.binary_of(&moved_to).exists(),
            "something was built of the commit the branch moved to"
        );
        repo.assert_only_the_main_worktree();
    }

    #[cfg(unix)]
    #[test]
    fn a_captured_command_leaves_what_it_started_in_its_process_group_alone() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let pid_file = dir.path().join("pid");
        let alive_file = dir.path().join("alive");
        // The shell ends at once and leaves a subshell in its group. A second later the subshell
        // says that it is still there, and then it sleeps. Its output is closed, so that nothing
        // waits for it.
        let script = "echo $$ > \"$1\"; \
                      (sleep 1; echo alive > \"$2\"; sleep 60) >/dev/null 2>&1 &";
        let mut command = Command::new("sh");
        command
            .args(["-c", script, "sh"])
            .arg(&pid_file)
            .arg(&alive_file);

        let output = bounded::capture(&mut command, Duration::from_secs(60))
            .expect("it starts")
            .expect("it ends in time");

        assert!(output.status.success(), "{output:?}");
        // The subshell must have said that it is still there. It is killed before the verdict, so
        // that a failure leaves nothing running.
        let group = read_pid(&pid_file);
        let survived = eventually(|| alive_file.exists());
        let _ = excise_harness::safety::kill_process_group(group);
        assert!(
            survived,
            "the group of the command was killed when it exited"
        );
        assert_group_gone(group, "after the test killed it");
    }

    #[test]
    fn a_ref_is_cached_when_the_binary_of_its_commit_is_there() {
        let repo = Repo::new();
        let spec = repo.spec(ToolchainPolicy::Inherited, None);
        let plan = plan_ref(&spec, "main").expect("a plan");
        assert!(
            !is_cached(&spec, &plan).expect("a verdict"),
            "nothing is built yet"
        );

        let binary = repo.binary_of(&plan.sha);
        fs::create_dir_all(binary.parent().expect("a parent")).expect("the release directory");
        fs::write(&binary, "a binary").expect("a binary");

        assert!(
            is_cached(&spec, &plan).expect("a verdict"),
            "the binary is there"
        );
        // A pinned build also wants the record of its toolchain, which is not there.
        let pinned = repo.spec(ToolchainPolicy::RefPinned, None);
        let pinned_plan = plan_ref(&pinned, "main").expect("a plan");
        assert!(
            !is_cached(&pinned, &pinned_plan).expect("a verdict"),
            "a binary without a record is not a hit"
        );
        // `build_ref` agrees with the first verdict: it builds nothing and makes no worktree.
        let built = build_ref(&spec, "main").expect("the cached build");
        assert!(built.cached);
        assert!(!repo.layout.worktrees.exists(), "a worktree was made");
    }

    #[test]
    fn a_build_log_is_made_with_every_directory_above_it_and_truncated_for_every_build() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let log = dir.path().join("target/baselines/build.log");

        // Neither `target` nor `baselines` is there: a build makes them, before it runs.
        drop(create_log(&log).expect("a log"));
        assert!(log.is_file(), "{} was not made", log.display());

        // The next build starts from an empty file, whatever an earlier one printed.
        fs::write(&log, "what an earlier build printed").expect("an earlier log");
        drop(create_log(&log).expect("the log again"));
        assert_eq!(fs::read_to_string(&log).expect("the log"), "");
    }
}
